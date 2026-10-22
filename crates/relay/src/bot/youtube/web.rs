//! YouTube from the cloud without a PC (Pro, 1.4; API.md §9.7 "From the web"): a signed-in
//! Pro account connects its YouTube channel on the web dashboard, and the cloud bot reads
//! and answers that channel's live chat from the relay with no Ferrocast PC at all.
//!
//! The channel is bound to the **account's Pro subscription** (the same key the account's
//! PCs use), so it's one cloud bot with one config and one YouTube budget per account
//! (the per-subscription units of `youtube`, cut to the day's fair share).
//!
//! 1. `POST /r/v1/pro/youtube/connect {moderation?}` (web host, cookie + CSRF, Pro) → `{url}`:
//!    Google's consent page for the "Ferrocast Cloud" client, with a signed single-use state
//!    naming the account (the PC flow's state with the owner `web`).
//! 2. Google sends the browser to the existing `/v1/bot/youtube/callback` on the API host.
//!    The code is traded, YouTube says whose channel it is, and the sign-in is sealed for that
//!    account under a one-time code (ten minutes); the browser goes on to the web host.
//! 3. `GET /r/v1/pro/youtube/finish?code=` binds it only for the **same signed-in account**
//!    (the web cookie, which the API host can't see) that still has Pro. Anyone else binds
//!    nothing, and the fresh grant is given back to Google.
//!
//! `GET /r/v1/pro/youtube` shows it (with today's quota), `PUT /r/v1/pro/youtube/bot
//! {enabled}` switches the cloud bot, `DELETE /r/v1/pro/youtube` disconnects (revoked at
//! Google). The PC flow (`connect` with the PC's token) is unchanged.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use ferrocast_core::event::Platform;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::{
    AUTHORIZE_URL, Call, ChannelInfo, GoogleTokens, NONCE_PREFIX, RELAY_SUB, SCOPES, StateInfo, YoutubeConfig,
    YoutubeError, quota_view, redirect_uri, revoke_stored, sign_state,
};
use crate::accounts::{web_session, web_session_write};
use crate::billing::gate::ProCtx;
use crate::bot::{disconnect_platform, forget_platform, set_enabled, store, take_rate, token_purpose};
use crate::vault::Sealed;
use crate::{ApiError, AppState, now_ms};

/// The state's owner for a web connect (a channel id never looks like this).
pub(crate) const WEB_OWNER: &str = "web";
/// What a connect from the Cloud page keeps with its nonce: the finish goes back there.
const CLOUD_PAGE: &str = "web_cloud";
/// `pending_oauth` keys of a sign-in waiting for its finish on the web host.
const PENDING_PREFIX: &str = "ytweb.";
const PENDING_PURPOSE: &str = "youtube_web_pending";
const CONNECTED_BY: &str = "web";

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post, put};
    axum::Router::new()
        .route("/r/v1/pro/youtube", get(view).delete(disconnect))
        .route("/r/v1/pro/youtube/connect", post(connect))
        .route("/r/v1/pro/youtube/finish", get(finish))
        .route("/r/v1/pro/youtube/bot", put(set_bot))
}

fn no_store(v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn see_other(to: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

pub(crate) fn not_set_up() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "platform_unavailable",
        "YouTube from the cloud isn't set up on Ferrocast's servers yet.",
    )
    .hint("YouTube keeps working in the app on your PC. Try again later.")
}

pub(crate) fn pro_needed() -> ApiError {
    ApiError::new(StatusCode::PAYMENT_REQUIRED, "pro_required", "YouTube from the cloud is part of Ferrocast Pro.")
        .hint("See Plans on the Account page. Twitch and Kick work in the cloud for free, and YouTube keeps working in the app.")
}

/// The account's Pro subscription, if it grants Pro now.
pub(crate) fn account_pro(st: &AppState, account: &str) -> Result<Option<String>, ApiError> {
    let now = now_ms();
    Ok(st
        .db
        .account_sub(account, now)
        .map_err(ApiError::db)?
        .filter(|s| crate::billing::access::is_pro(&s.access(), now))
        .map(|s| s.id))
}

fn channel_json(ch: &store::Channel) -> Value {
    json!({
        "id": ch.channel_id, "title": ch.channel_login, "moderation": ch.moderation,
        "error": ch.error, "live": ch.live_since.is_some(),
        "from_web": ch.connected_by == CONNECTED_BY, "connected_at": ch.connected_at,
    })
}

pub(crate) fn view_json(st: &AppState, account: &str) -> Result<Value, ApiError> {
    let available = st.bot.youtube_cfg().is_some() && st.vault().is_ok();
    let Some(sub) = account_pro(st, account)? else {
        return Ok(json!({ "available": available, "pro": false, "bot_on": false, "channel": null, "quota": null }));
    };
    let ch = st.db.bot_channel(&sub, "youtube").map_err(ApiError::db)?;
    let bot_on = st.db.bot_config(&sub).map_err(ApiError::db)?.is_some_and(|r| r.enabled);
    Ok(json!({
        "available": available, "pro": true, "bot_on": bot_on,
        "channel": ch.as_ref().map(channel_json),
        "quota": if available { quota_view(st, &sub) } else { Value::Null },
    }))
}

/// `GET /r/v1/pro/youtube`.
async fn view(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_session(&st, &headers)?;
    Ok(no_store(view_json(&st, &a.account.id)?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectIn {
    #[serde(default)]
    moderation: bool,
}

fn body<T: serde::de::DeserializeOwned + Default>(headers: &HeaderMap, raw: &[u8]) -> Result<T, ApiError> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if !json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send it as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice(raw)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("The request isn't right: {e}")))
}

/// Google's consent page; `cloud`: started on the Cloud page, so the finish goes back there.
fn auth_url(
    st: &AppState,
    cfg: &YoutubeConfig,
    account: &str,
    moderation: bool,
    cloud: bool,
) -> Result<String, ApiError> {
    let nonce = crate::random(12);
    let kept = if cloud { CLOUD_PAGE } else { WEB_OWNER };
    st.db.put_oauth(&format!("{NONCE_PREFIX}{nonce}"), account, kept).map_err(ApiError::db)?;
    let mut url = url::Url::parse(AUTHORIZE_URL).map_err(|_| not_set_up())?;
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &redirect_uri(st))
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent select_account")
        .append_pair("include_granted_scopes", "false")
        .append_pair("state", &sign_state(cfg, moderation, account, WEB_OWNER, &nonce, now_ms()));
    Ok(url.to_string())
}

/// `POST /r/v1/pro/youtube/connect {moderation?}` → `{url}`.
async fn connect(State(st): State<AppState>, headers: HeaderMap, raw: axum::body::Bytes) -> Result<Response, ApiError> {
    let a = web_session_write(&st, &headers)?;
    let b: ConnectIn = body(&headers, &raw)?;
    let url = connect_url(&st, &a.account.id, b.moderation, false)?;
    Ok(no_store(json!({ "flow": "browser", "url": url })))
}

/// Checks and makes a web connect's consent link (the Account page's, or the Cloud page's
/// with `cloud`): 503 without the "Ferrocast Cloud (YouTube)" client or the vault, 402
/// without Pro, 429 after 20 in an hour.
pub(crate) fn connect_url(st: &AppState, account: &str, moderation: bool, cloud: bool) -> Result<String, ApiError> {
    let cfg = st.bot.youtube_cfg().ok_or_else(not_set_up)?.clone();
    st.vault().map_err(|_| not_set_up())?;
    if account_pro(st, account)?.is_none() {
        return Err(pro_needed());
    }
    if let Err(wait) = st.accounts.take("youtube_connect", account, 20, 3_600_000) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "That's a lot of YouTube sign-ins in an hour.",
        )
        .hint("Wait a little and press Connect again.")
        .header(header::RETRY_AFTER, wait.to_string()));
    }
    auth_url(st, &cfg, account, moderation, cloud)
}

/// What waits for the finish (sealed for the account).
#[derive(Debug, serde::Serialize, Deserialize)]
struct Pending {
    channel_id: String,
    title: String,
    scopes: String,
    moderation: bool,
    key_v: i64,
    nonce: String,
    ct: String,
    /// Started on the Cloud page: the finish goes back there.
    #[serde(default)]
    cloud: bool,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Gives a fresh grant nobody will use back to Google, unless a binding uses that channel
/// (Google's revoke ends the whole grant of that Google account for Ferrocast Cloud).
async fn give_back(st: &AppState, channel: Option<&str>, token: &str) {
    let bound = match channel {
        Some(c) => st.db.bot_channel_by_channel("youtube", c).map(|b| b.is_some()).unwrap_or(true),
        None => true,
    };
    if bound || token.is_empty() {
        return;
    }
    if let Ok(api) = st.bot.youtube()
        && let Err(e) = api.revoke(token).await
    {
        tracing::debug!(error = %e, "couldn't revoke an unused Google sign-in");
    }
}

const TRY_SHORTLY: &str = "Ferrocast couldn't save that. Press Connect YouTube again on the Account page in a minute.";

/// The API host's callback for a web state (from `youtube::callback`, the nonce not yet used).
pub(crate) async fn callback(st: &AppState, s: StateInfo, code: &str) -> Response {
    let page = crate::page;
    let account = s.install.clone();
    let cloud = match st.db.take_oauth(&format!("{NONCE_PREFIX}{}", s.nonce)) {
        Ok(Some((a, owner))) if a == account && (owner == WEB_OWNER || owner == CLOUD_PAGE) => owner == CLOUD_PAGE,
        Ok(_) => {
            return page(
                StatusCode::BAD_REQUEST,
                "This sign-in link expired or was already used. Press Connect YouTube again on the Account page.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Ok(api) = st.bot.youtube() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "YouTube from the cloud isn't set up on this relay yet.");
    };
    let tokens: GoogleTokens = match api.exchange_code(code, &redirect_uri(st)).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "Google didn't take a web YouTube sign-in code");
            return page(StatusCode::BAD_GATEWAY, "Google didn't accept the sign-in. Press Connect YouTube again.");
        }
    };
    let counted = account_pro(st, &account).ok().flatten();
    let _ = st.db.yt_take_units(counted.as_deref().unwrap_or(RELAY_SUB), Call::Channel, None, now_ms());
    let channel: ChannelInfo = match api.my_channel(&tokens.access_token).await {
        Ok(c) => c,
        Err(YoutubeError::NotFound) => {
            give_back(st, None, &tokens.refresh_token).await;
            return page(
                StatusCode::CONFLICT,
                "That Google account has no YouTube channel. Press Connect YouTube again and pick the account that owns your channel.",
            );
        }
        Err(_) => return page(StatusCode::BAD_GATEWAY, "YouTube didn't say whose channel this is. Try again."),
    };
    let grant =
        if tokens.refresh_token.is_empty() { tokens.access_token.clone() } else { tokens.refresh_token.clone() };
    if !SCOPES.iter().all(|x| tokens.scopes.iter().any(|g| g == x)) {
        give_back(st, Some(&channel.id), &grant).await;
        return page(
            StatusCode::BAD_REQUEST,
            "Google didn't allow Ferrocast to manage your YouTube chat. Press Connect YouTube again and tick every box.",
        );
    }
    if tokens.refresh_token.is_empty() {
        give_back(st, Some(&channel.id), &grant).await;
        return page(StatusCode::BAD_GATEWAY, "Google didn't give a lasting sign-in. Press Connect YouTube again.");
    }
    let Ok(vault) = st.vault() else {
        give_back(st, Some(&channel.id), &grant).await;
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "YouTube from the cloud isn't switched on on Ferrocast's servers yet.",
        );
    };
    let Ok(sealed) = vault.seal(&account, PENDING_PURPOSE, tokens.refresh_token.as_bytes()) else {
        give_back(st, Some(&channel.id), &grant).await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    let pending = Pending {
        channel_id: channel.id.clone(),
        title: channel.title.clone(),
        scopes: tokens.scopes.join(" "),
        moderation: s.moderation,
        key_v: sealed.key_v,
        nonce: b64().encode(&sealed.nonce),
        ct: b64().encode(&sealed.ct),
        cloud,
    };
    let finish = crate::random(32);
    let saved = serde_json::to_string(&pending).ok().is_some_and(|p| {
        st.db.put_oauth(&format!("{PENDING_PREFIX}{}", crate::accounts::sha(&finish)), &account, &p).is_ok()
    });
    if !saved {
        give_back(st, Some(&channel.id), &grant).await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    see_other(&format!("{}/r/v1/pro/youtube/finish?code={finish}", crate::accounts::web_origin(st)))
}

#[derive(Debug, Deserialize)]
struct FinishQuery {
    code: Option<String>,
}

fn open_pending(st: &AppState, account: &str, p: &Pending) -> Option<String> {
    let sealed = Sealed { key_v: p.key_v, nonce: b64().decode(&p.nonce).ok()?, ct: b64().decode(&p.ct).ok()? };
    let raw = st.vault().ok()?.open(account, PENDING_PURPOSE, &sealed).ok()?;
    String::from_utf8(raw).ok()
}

/// `GET /r/v1/pro/youtube/finish?code=` (web host): binds for the same signed-in account.
async fn finish(State(st): State<AppState>, headers: HeaderMap, Query(q): Query<FinishQuery>) -> Response {
    let page = crate::page;
    let Some(code) = q.code.filter(|c| !c.is_empty() && c.len() <= 128) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This link is incomplete. Press Connect YouTube again on the Account page.",
        );
    };
    let taken = st.db.take_oauth(&format!("{PENDING_PREFIX}{}", crate::accounts::sha(&code)));
    let (pending_account, payload) = match taken {
        Ok(Some(x)) => x,
        Ok(None) => {
            return page(
                StatusCode::GONE,
                "This sign-in expired or was already used. Press Connect YouTube again on the Account page.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Ok(p) = serde_json::from_str::<Pending>(&payload) else {
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    let refresh = open_pending(&st, &pending_account, &p);
    let signed_in = web_session(&st, &headers).ok().map(|a| a.account.id);
    let same = signed_in.as_deref().is_some_and(|a| bool::from(a.as_bytes().ct_eq(pending_account.as_bytes())));
    if !same {
        if let Some(t) = &refresh {
            give_back(&st, Some(&p.channel_id), t).await;
        }
        tracing::info!("a web YouTube sign-in reached a browser of another (or no) account: nothing bound");
        return page(
            StatusCode::FORBIDDEN,
            "This YouTube sign-in was started from another Ferrocast account, or this browser is signed out. Nothing was connected. Sign in and press Connect YouTube again.",
        );
    }
    let Some(refresh) = refresh else {
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    let sub = match account_pro(&st, &pending_account) {
        Ok(Some(s)) => s,
        Ok(None) => {
            give_back(&st, Some(&p.channel_id), &refresh).await;
            return page(
                StatusCode::PAYMENT_REQUIRED,
                "YouTube from the cloud is part of Pro, and this account's Pro isn't active.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    match bind(&st, &sub, &p, &refresh).await {
        Ok(()) if p.cloud => see_other(&format!("{}/app/#/cloud?connected=youtube", crate::accounts::page_origin(&st))),
        Ok(()) => see_other(&format!("{}/app/#/account?youtube=connected", crate::accounts::page_origin(&st))),
        Err(()) => {
            give_back(&st, Some(&p.channel_id), &refresh).await;
            page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY)
        }
    }
}

/// Binds the channel for `sub` with its sealed sign-in, as the PC flow's callback does.
async fn bind(st: &AppState, sub: &str, p: &Pending, refresh: &str) -> Result<(), ()> {
    let vault = st.vault().map_err(|_| ())?;
    let sealed = vault.seal(sub, &token_purpose("youtube", "bot"), refresh.as_bytes()).map_err(|_| ())?;
    // The owner is here: a binding another subscription kept for this channel goes (the
    // same Google grant, so it isn't revoked).
    match st.db.bot_channel_by_channel("youtube", &p.channel_id) {
        Ok(Some(other)) if other.sub_id != sub => {
            forget_platform(st, &other.sub_id, Platform::Youtube);
            tracing::info!("a YouTube channel's owner took the cloud bot back from another subscription");
        }
        Ok(_) => {}
        Err(_) => return Err(()),
    }
    let replaced = st.db.bot_channel(sub, "youtube").ok().flatten().and_then(|c| c.channel_id);
    if replaced.as_deref().is_some_and(|old| old != p.channel_id)
        && st.db.bot_token(sub, "youtube", "bot").ok().flatten().is_some()
    {
        revoke_stored(st, sub).await;
    }
    st.db
        .bot_set_account(
            sub,
            "youtube",
            (&p.channel_id, &p.title),
            Some((&p.channel_id, &p.title)),
            &p.scopes,
            p.moderation,
            CONNECTED_BY,
            now_ms(),
        )
        .map_err(|_| ())?;
    st.db.bot_put_token(sub, "youtube", "bot", &sealed).map_err(|_| ())?;
    st.bot.forget_yt(sub);
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BotIn {
    enabled: Option<bool>,
}

/// `PUT /r/v1/pro/youtube/bot {enabled}`: the cloud bot's switch for the account.
async fn set_bot(State(st): State<AppState>, headers: HeaderMap, raw: axum::body::Bytes) -> Result<Response, ApiError> {
    let a = web_session_write(&st, &headers)?;
    let b: BotIn = body(&headers, &raw)?;
    let enabled = b
        .enabled
        .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", "enabled: true or false."))?;
    let sub = account_pro(&st, &a.account.id)?.ok_or_else(pro_needed)?;
    take_rate(&st, &sub)?;
    let ctx = ProCtx { install: String::new(), sub_id: sub };
    set_enabled(&st, &ctx, enabled).await?;
    Ok(no_store(view_json(&st, &a.account.id)?))
}

/// `DELETE /r/v1/pro/youtube` → the view: the channel unbound and its sign-in revoked.
async fn disconnect(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_session_write(&st, &headers)?;
    // Open after Pro ends (R-04): the account's subscription, Pro or not.
    let sub = st.db.account_sub(&a.account.id, now_ms()).map_err(ApiError::db)?.map(|s| s.id);
    if let Some(sub) = sub
        && st.db.bot_channel(&sub, "youtube").map_err(ApiError::db)?.is_some()
    {
        disconnect_platform(&st, &sub, Platform::Youtube).await;
    }
    Ok(no_store(view_json(&st, &a.account.id)?))
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
