//! Connecting Twitch and Kick from the web dashboard (docs/CLOUD.md §4).
//!
//! 1. `POST /c/api/v1/accounts/{platform}/connect` (cookie + CSRF) gives the platform's
//!    sign-in page, with a single-use state (`cl.<random>`, kept in `pending_oauth` for ten
//!    minutes, naming the account).
//! 2. The platform sends the browser back to the relay's existing callback on the API host
//!    (`/v1/bot/twitch/callback`, `/v1/kick/callback`), which sees the `cl.` state and hands
//!    over here: the code is traded, Twitch or Kick says whose account it is, and the sign-in
//!    is sealed into `cloud_pending` under a one-time code (five minutes). The browser goes on
//!    to the web host.
//! 3. `GET /c/v1/connect/finish?code=` binds it only for the **same signed-in account** (the
//!    web cookie, which the API host can't see). Anyone else, or a signed-out browser, binds
//!    nothing and the fresh sign-in is revoked.
//! 4. Bound: the channel row, the sealed sign-in (where the cloud bot reads it, §6), the
//!    cloud bot's channel row and the platform's event subscriptions.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine as _;
use ferrocast_core::event::Platform;
use rsa::sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::store::Channel;
use super::{e, json, no_store, web_account};
use crate::bot::twitch::TwitchError;
use crate::{ApiError, AppState, now_ms};

/// States of a cloud connect start with this (base64url never has a `.`).
pub(crate) const STATE_PREFIX: &str = "cl.";
/// How long the hand-over from the API host to the web host may take.
pub const PENDING_TTL_MS: i64 = 5 * 60_000;

/// Twitch: everything cloud mode needs, asked once (R-22). The bot speaks as the streamer's
/// own account (R-28), with the bot badge through `channel:bot`.
pub const TWITCH_SCOPES: &[&str] = &[
    "user:read:chat",
    "user:write:chat",
    "user:bot",
    "channel:bot",
    "moderator:read:followers",
    "channel:read:subscriptions",
    "bits:read",
    "channel:read:redemptions",
];

/// The EventSub subscriptions of a cloud channel (all from `ferrocast_twitch::map`'s list, so
/// the same mapper reads them). The streamer is broadcaster, moderator and chat user, so
/// each costs 0 against Twitch's limit.
pub const TWITCH_KINDS: &[&str] = &[
    "channel.chat.message",
    "channel.chat.message_delete",
    "channel.chat.clear_user_messages",
    "channel.follow",
    "channel.subscribe",
    "channel.subscription.message",
    "channel.subscription.gift",
    "channel.cheer",
    "channel.raid",
    "channel.channel_points_custom_reward_redemption.add",
    "stream.online",
    "stream.offline",
];

/// Kick: the webhooks of a cloud channel, made with the streamer's own sign-in.
pub const KICK_EVENTS: &[&str] = &[
    "chat.message.sent",
    "channel.followed",
    "channel.subscription.new",
    "channel.subscription.renewal",
    "channel.subscription.gifts",
    "kicks.gifted",
    "livestream.status.updated",
    "moderation.banned",
];

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/api/v1/accounts", get(accounts))
        .route("/c/api/v1/accounts/{platform}/connect", post(connect))
        .route("/c/api/v1/accounts/{platform}", delete(disconnect))
        .route("/c/v1/connect/finish", get(finish))
}

fn label(platform: &str) -> &'static str {
    match platform {
        "twitch" => "Twitch",
        "kick" => "Kick",
        "youtube" => "YouTube",
        _ => "That platform",
    }
}

/// `twitch` or `kick`; YouTube in the cloud is Pro (§11, built separately).
fn platform_of(raw: &str) -> Result<&'static str, ApiError> {
    match raw {
        "twitch" => Ok("twitch"),
        "kick" => Ok("kick"),
        "youtube" => {
            Err(e(StatusCode::PAYMENT_REQUIRED, "pro_required", "YouTube in cloud mode is part of Ferrocast Pro.")
                .hint("Twitch and Kick are free. YouTube keeps working in the app on your PC."))
        }
        _ => {
            Err(e(StatusCode::NOT_FOUND, "not_found", "Cloud mode connects Twitch and Kick.")
                .hint("Pick Twitch or Kick."))
        }
    }
}

fn not_set_up(platform: &str) -> ApiError {
    e(
        StatusCode::SERVICE_UNAVAILABLE,
        "platform_unavailable",
        format!("{} isn't set up for cloud mode on Ferrocast's servers yet.", label(platform)),
    )
    .hint("Try again later. The app on your PC keeps working.")
}

fn required_scopes(platform: &str) -> Vec<&'static str> {
    if platform == "twitch" { TWITCH_SCOPES.to_vec() } else { crate::KICK_SCOPES.to_vec() }
}

/// `GET /c/api/v1/accounts`: the engine's `Account[]` for the connected channels.
async fn accounts(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let list: Vec<Value> = st
        .db
        .cloud_channels(&a.account.id)
        .map_err(ApiError::db)?
        .into_iter()
        .filter(|c| c.platform == "twitch" || c.platform == "kick")
        .map(|c| {
            let ok = required_scopes(&c.platform).iter().all(|s| c.scopes.iter().any(|g| g == s));
            json!({ "platform": c.platform, "name": c.name.clone().unwrap_or_else(|| c.login.clone()),
                    "avatar_url": c.avatar_url, "connected_at": c.connected_at, "scopes_ok": ok })
        })
        .collect();
    Ok(json(json!(list)))
}

/// `POST /c/api/v1/accounts/{platform}/connect` → `{ flow: "browser", url }`.
async fn connect(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let p = platform_of(&platform)?;
    // The sign-in is kept sealed: no vault, no connecting.
    st.vault()?;
    let state = format!("{STATE_PREFIX}{}", crate::random(24));
    let url = match p {
        "twitch" => {
            let cfg = st.bot.twitch_cfg().ok_or_else(|| not_set_up(p))?;
            st.db.put_oauth(&state, &a.account.id, "twitch").map_err(ApiError::db)?;
            let mut url = url::Url::parse("https://id.twitch.tv/oauth2/authorize").map_err(|_| not_set_up(p))?;
            url.query_pairs_mut()
                .append_pair("client_id", &cfg.client_id)
                .append_pair("redirect_uri", &crate::bot::twitch::redirect_uri(&st))
                .append_pair("response_type", "code")
                .append_pair("scope", &TWITCH_SCOPES.join(" "))
                .append_pair("force_verify", "true")
                .append_pair("state", &state);
            url.to_string()
        }
        _ => {
            let (client_id, _) = crate::kick_app(&st).map_err(|_| not_set_up(p))?;
            let verifier = crate::random(48);
            let challenge =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            st.db.put_oauth(&state, &a.account.id, &verifier).map_err(ApiError::db)?;
            let mut url =
                url::Url::parse(&format!("{}/oauth/authorize", st.cfg.kick_id_url)).map_err(|_| not_set_up(p))?;
            url.query_pairs_mut()
                .append_pair("client_id", &client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &format!("{}/v1/kick/callback", st.cfg.public_url))
                .append_pair("scope", &crate::KICK_SCOPES.join(" "))
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state);
            url.to_string()
        }
    };
    Ok(json(json!({ "flow": "browser", "url": url })))
}

// ── The hand-over (API host → web host) ──────────────────────────────────────────────

/// A sign-in waiting for its finish: sealed in `cloud_pending`, never logged.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub platform: String,
    pub channel_id: String,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub scopes: Vec<String>,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").field("platform", &self.platform).field("login", &self.login).finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct SealedJson {
    k: i64,
    n: String,
    c: String,
}

fn pending_purpose(platform: &str) -> String {
    format!("cloud_pending_{platform}")
}

fn seal_pending(st: &AppState, account: &str, p: &Pending) -> Option<String> {
    let vault = st.vault().ok()?;
    let plain = serde_json::to_vec(p).ok()?;
    let s = vault.seal(account, &pending_purpose(&p.platform), &plain).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD;
    serde_json::to_string(&SealedJson { k: s.key_v, n: b64.encode(&s.nonce), c: b64.encode(&s.ct) }).ok()
}

fn open_pending(st: &AppState, account: &str, platform: &str, raw: &str) -> Option<Pending> {
    let vault = st.vault().ok()?;
    let j: SealedJson = serde_json::from_str(raw).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let sealed = crate::vault::Sealed { key_v: j.k, nonce: b64.decode(j.n).ok()?, ct: b64.decode(j.c).ok()? };
    match vault.open(account, &pending_purpose(platform), &sealed) {
        Ok(plain) => serde_json::from_slice(&plain).ok(),
        Err(err) => {
            crate::vault::damaged(&err);
            None
        }
    }
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

const TRY_SHORTLY: &str = "Ferrocast couldn't save that. Press Connect again on the Cloud page in a minute.";

/// Seals the sign-in for its finish and sends the browser to the web host.
async fn hand_over(st: &AppState, account: &str, p: Pending) -> Response {
    let code = crate::random(32);
    let saved = seal_pending(st, account, &p).is_some_and(|payload| {
        st.db
            .cloud_put_pending(&crate::accounts::sha(&code), account, &p.platform, &payload, now_ms() + PENDING_TTL_MS)
            .is_ok()
    });
    if !saved {
        revoke(st, &p).await;
        return crate::page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    see_other(&format!("{}/c/v1/connect/finish?code={code}", crate::accounts::web_origin(st)))
}

/// Gives a sign-in that won't be used back to the platform.
async fn revoke(st: &AppState, p: &Pending) {
    match p.platform.as_str() {
        "twitch" => {
            if let Ok(api) = st.bot.twitch()
                && let Err(err) = api.revoke(&p.access_token).await
            {
                tracing::debug!(error = %err, "couldn't revoke an unused Twitch sign-in");
            }
        }
        _ => {
            if let Ok(api) = st.bot.kick()
                && !p.refresh_token.is_empty()
                && let Err(err) = api.revoke(&p.refresh_token).await
            {
                tracing::debug!(error = %err, "couldn't revoke an unused Kick sign-in");
            }
        }
    }
}

/// The Twitch callback with a `cl.` state (from `bot::twitch::callback`).
pub(crate) async fn twitch_callback(st: &AppState, code: Option<String>, state: &str) -> Response {
    let page = crate::page;
    let account = match st.db.take_oauth(state) {
        Ok(Some((account, platform))) if platform == "twitch" => account,
        Ok(_) => {
            return page(
                StatusCode::BAD_REQUEST,
                "This sign-in link expired or was already used. Press Connect again on the Cloud page.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Some(code) = code else {
        return page(StatusCode::BAD_REQUEST, "This link is incomplete. Press Connect again on the Cloud page.");
    };
    let Ok(api) = st.bot.twitch() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "Twitch isn't set up for cloud mode on Ferrocast's servers yet.");
    };
    let tokens = match api.exchange_code(&code, &crate::bot::twitch::redirect_uri(st)).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "Twitch didn't take a cloud mode sign-in code");
            return page(StatusCode::BAD_GATEWAY, "Twitch didn't accept the sign-in. Press Connect again.");
        }
    };
    let p = |info: &crate::bot::twitch::TokenInfo| Pending {
        platform: "twitch".into(),
        channel_id: info.user_id.clone(),
        login: info.login.clone(),
        name: Some(info.login.clone()),
        avatar_url: None,
        scopes: info.scopes.clone(),
        refresh_token: tokens.refresh_token.clone(),
        access_token: tokens.access_token.clone(),
        expires_in: 3_600,
    };
    // Whose account: from Twitch's own token check, never from the URL.
    let info = match api.validate(&tokens.access_token).await {
        Ok(i) => i,
        Err(_) => {
            let _ = api.revoke(&tokens.access_token).await;
            return page(StatusCode::BAD_GATEWAY, "Twitch didn't say whose account this is. Press Connect again.");
        }
    };
    let pending = p(&info);
    let missing: Vec<&str> = TWITCH_SCOPES.iter().copied().filter(|x| !info.scopes.iter().any(|g| g == x)).collect();
    if !missing.is_empty() {
        revoke(st, &pending).await;
        return page(
            StatusCode::BAD_REQUEST,
            &format!("Twitch didn't allow {}. Press Connect again and allow everything listed.", missing.join(", ")),
        );
    }
    if tokens.refresh_token.is_empty() {
        revoke(st, &pending).await;
        return page(StatusCode::BAD_GATEWAY, "Twitch didn't give a lasting sign-in. Press Connect again.");
    }
    hand_over(st, &account, pending).await
}

/// The Kick callback with a `cl.` state (from `crate::kick_callback`, which traded the code
/// with the verifier kept for `account` and asked Kick whose account it is).
pub(crate) async fn kick_callback(
    st: &AppState,
    account: &str,
    tokens: &crate::kick::TokenResponse,
    user_id: &str,
    user: &Value,
) -> Response {
    let pending = Pending {
        platform: "kick".into(),
        channel_id: user_id.to_string(),
        login: user["name"].as_str().unwrap_or("Kick").to_string(),
        name: user["name"].as_str().map(String::from),
        avatar_url: user["profile_picture"].as_str().filter(|u| u.starts_with("https://")).map(String::from),
        // Kick's token answer doesn't list scopes: what was asked for (Kick shows all or nothing).
        scopes: crate::KICK_SCOPES.iter().map(|s| s.to_string()).collect(),
        refresh_token: tokens.refresh_token.clone(),
        access_token: tokens.access_token.clone(),
        expires_in: tokens.expires_in.clamp(0, 86_400),
    };
    if tokens.refresh_token.is_empty() {
        return crate::page(StatusCode::BAD_GATEWAY, "Kick didn't give a lasting sign-in. Press Connect again.");
    }
    hand_over(st, account, pending).await
}

// ── The finish (web host) ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FinishQuery {
    code: Option<String>,
}

/// `GET /c/v1/connect/finish?code=`: binds the channel for the same signed-in account, then
/// back to the Cloud page.
async fn finish(State(st): State<AppState>, headers: HeaderMap, Query(q): Query<FinishQuery>) -> Response {
    let page = crate::page;
    if !super::available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return page(StatusCode::NOT_FOUND, "There's nothing here.");
    }
    let Some(code) = q.code.filter(|c| !c.is_empty() && c.len() <= 128) else {
        return page(StatusCode::BAD_REQUEST, "This link is incomplete. Press Connect again on the Cloud page.");
    };
    let taken = match st.db.cloud_take_pending(&crate::accounts::sha(&code), now_ms()) {
        Ok(t) => t,
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Some((account, platform, payload)) = taken else {
        return page(
            StatusCode::BAD_REQUEST,
            "This link expired or was already used. Press Connect again on the Cloud page.",
        );
    };
    let Some(p) = open_pending(&st, &account, &platform, &payload) else {
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    // The same signed-in account, in this browser: anything else binds nothing.
    let me = match crate::accounts::web_session(&st, &headers) {
        Ok(a) => a,
        Err(_) => {
            revoke(&st, &p).await;
            return page(
                StatusCode::UNAUTHORIZED,
                "Sign in to Ferrocast in this browser, then press Connect again on the Cloud page.",
            );
        }
    };
    if me.account.id != account {
        revoke(&st, &p).await;
        tracing::info!("a cloud connect finish reached another account's browser: nothing was bound");
        return page(
            StatusCode::FORBIDDEN,
            "This connect link was made by another Ferrocast account, so nothing was connected. Press Connect on your own Cloud page.",
        );
    }
    match bind(&st, &account, p).await {
        Ok(()) => see_other(&format!("/app/#/cloud?connected={platform}")),
        Err(m) => page(StatusCode::SERVICE_UNAVAILABLE, &m),
    }
}

// ── Binding and unbinding ────────────────────────────────────────────────────────────

fn platform_enum(p: &str) -> Platform {
    if p == "twitch" { Platform::Twitch } else { Platform::Kick }
}

pub(crate) fn error_json(code: &str, message: &str, hint: &str) -> String {
    json!({ "code": code, "message": message, "hint": hint, "at": now_ms() }).to_string()
}

/// Binds a proven channel to `account`. A binding this replaces is cleaned up first: this
/// account's earlier channel on the platform (its sign-in revoked), and another account's or
/// a Pro cloud bot's binding of the same channel, which moves to this newer, proven owner
/// (forgotten without a revoke: it's the same person's account at the platform, and a revoke
/// could end the new sign-in too).
pub(crate) async fn bind(st: &AppState, account: &str, p: Pending) -> Result<(), String> {
    let platform = p.platform.clone();
    let pe = platform_enum(&platform);
    let fail = |what: &str| {
        tracing::warn!(platform = %platform, what, "couldn't bind a cloud channel");
        TRY_SHORTLY.to_string()
    };
    let mine = st.db.cloud_account_channel(account, &platform).map_err(|_| fail("read"))?;
    let other = st.db.cloud_channel(&platform, &p.channel_id).map_err(|_| fail("read"))?;
    if let Some(old) = &mine {
        unsubscribe(st, old).await;
        if old.channel_id == p.channel_id {
            crate::bot::forget_platform(st, account, pe);
        } else {
            crate::bot::disconnect_platform(st, account, pe).await;
        }
    }
    if let Some(o) = other.as_ref().filter(|o| o.account_id != account) {
        unsubscribe(st, o).await;
        crate::bot::forget_platform(st, &o.account_id, pe);
        tracing::info!(platform = %platform, "a channel moved to its newer, proven cloud mode account");
    }
    // A Pro cloud bot (or another account's) row for this channel: the unique channel index
    // allows one, and the proven owner is here.
    if let Ok(Some(b)) = st.db.bot_channel_by_channel(&platform, &p.channel_id)
        && b.sub_id != account
    {
        if pe == Platform::Twitch {
            crate::bot::twitch::unsubscribe_all(st, &b).await;
        }
        crate::bot::forget_platform(st, &b.sub_id, pe);
    }
    let vault = st.vault().map_err(|_| fail("vault"))?;
    let sealed = vault
        .seal(account, &crate::bot::token_purpose(&platform, "bot"), p.refresh_token.as_bytes())
        .map_err(|_| fail("seal"))?;
    let now = now_ms();
    let scopes = p.scopes.join(" ");
    st.db
        .bot_set_account(
            account,
            &platform,
            (&p.channel_id, &p.login),
            Some((&p.channel_id, &p.login)),
            &scopes,
            false,
            "cloud",
            now,
        )
        .map_err(|_| fail("bot row"))?;
    st.db.bot_put_token(account, &platform, "bot", &sealed).map_err(|_| fail("token"))?;
    if pe == Platform::Kick {
        st.bot.cache_kick_access(account, &p.access_token, now + p.expires_in.clamp(0, 86_400) * 1000);
    }
    let ch = Channel {
        platform: platform.clone(),
        channel_id: p.channel_id.clone(),
        account_id: account.to_string(),
        login: p.login.clone(),
        name: p.name.clone(),
        avatar_url: p.avatar_url.clone(),
        scopes: p.scopes.clone(),
        event_subs: Vec::new(),
        connected_at: now,
        error: None,
    };
    st.db.cloud_bind_channel(&ch).map_err(|_| fail("channel row"))?;
    tracing::info!(platform = %platform, "a channel was connected for cloud mode");
    // The events: a failure is kept on the channel (the Cloud page shows it, the sweep retries).
    let _ = subscribe(st, &ch).await;
    Ok(())
}

/// Disconnects a platform: its subscriptions deleted, the sign-in revoked at the platform,
/// the rows deleted. `false` when it wasn't connected.
pub(crate) async fn unbind(st: &AppState, account: &str, platform: &str) -> Result<bool, ApiError> {
    let Some(ch) = st.db.cloud_account_channel(account, platform).map_err(ApiError::db)? else { return Ok(false) };
    unsubscribe(st, &ch).await;
    crate::bot::disconnect_platform(st, account, platform_enum(platform)).await;
    st.db.cloud_unbind_channel(account, platform).map_err(ApiError::db)?;
    tracing::info!(platform, "a cloud mode channel was disconnected");
    Ok(true)
}

/// `DELETE /c/api/v1/accounts/{platform}` → 204.
async fn disconnect(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let p = platform_of(&platform)?;
    if !unbind(&st, &a.account.id, p).await? {
        return Err(e(StatusCode::NOT_FOUND, "not_found", format!("{} isn't connected in cloud mode.", label(p)))
            .hint("Connect it on the Cloud page."));
    }
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

// ── Platform subscriptions ───────────────────────────────────────────────────────────

/// Makes a channel's event subscriptions (old ones first). Each id is saved as it's made, so
/// a failure halfway leaves nothing unknown at the platform. The outcome is kept on the row.
pub(crate) async fn subscribe(st: &AppState, ch: &Channel) -> Result<(), String> {
    unsubscribe(st, ch).await;
    let result = match ch.platform.as_str() {
        "twitch" => subscribe_twitch(st, ch).await,
        "kick" => subscribe_kick(st, ch).await,
        _ => Ok(()),
    };
    let err = result.as_ref().err().map(|(c, m, h)| error_json(c, m, h));
    if let Err(dbe) = st.db.cloud_set_channel_error(&ch.platform, &ch.channel_id, err.as_deref()) {
        tracing::warn!(error = %dbe, "couldn't save a cloud channel's state");
    }
    result.map_err(|(_, m, h)| format!("{m} {h}"))
}

type Problem = (&'static str, &'static str, &'static str);

async fn subscribe_twitch(st: &AppState, ch: &Channel) -> Result<(), Problem> {
    let unreachable = (
        "twitch_unavailable",
        "Couldn't reach Twitch to start cloud alerts and chat.",
        "Ferrocast tries again within the hour.",
    );
    let api = st.bot.twitch().map_err(|_| unreachable)?;
    let app = st.bot.app_token().await.map_err(|_| unreachable)?;
    let conduit = st.bot.conduit(st).await.map_err(|_| unreachable)?;
    let mut ids: Vec<String> = Vec::new();
    for sub in ferrocast_twitch::map::SUBSCRIPTIONS.iter().filter(|s| TWITCH_KINDS.contains(&s.kind)) {
        let cond = sub.condition_json(&ch.channel_id);
        let id = match api.subscribe_version(&app, &conduit, sub.kind, sub.version, cond.clone()).await {
            Ok(id) if !id.is_empty() => Some(id),
            Ok(_) => None,
            Err(TwitchError::Conflict) => api.find_subscription(&app, sub.kind, &cond).await.ok().flatten(),
            Err(TwitchError::Forbidden(_) | TwitchError::Unauthorized) => {
                return Err((
                    "twitch_not_allowed",
                    "Twitch didn't let Ferrocast read this channel's events.",
                    "Disconnect Twitch and connect it again, allowing everything listed.",
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, kind = sub.kind, "couldn't subscribe a cloud channel");
                return Err(unreachable);
            }
        };
        if let Some(id) = id
            && !ids.contains(&id)
        {
            ids.push(id);
            let _ = st.db.cloud_set_event_subs(&ch.platform, &ch.channel_id, &ids);
        }
    }
    Ok(())
}

async fn subscribe_kick(st: &AppState, ch: &Channel) -> Result<(), Problem> {
    let signed_out =
        ("kick_signed_out", "Kick signed Ferrocast out of this channel.", "Disconnect Kick and connect it again.");
    let unreachable = (
        "kick_unavailable",
        "Couldn't reach Kick to start cloud alerts and chat.",
        "Ferrocast tries again within the hour.",
    );
    let token = crate::bot::kick::access(st, &ch.account_id).await.map_err(|err| match err {
        crate::bot::SendErr::SignedOut => signed_out,
        _ => unreachable,
    })?;
    let broadcaster: Value = ch.channel_id.parse::<i64>().map_or_else(|_| json!(ch.channel_id), |n| json!(n));
    let events: Vec<Value> = KICK_EVENTS.iter().map(|n| json!({ "name": n, "version": 1 })).collect();
    let res = st
        .http
        .post(format!("{}/public/v1/events/subscriptions", st.cfg.kick_api_url))
        .bearer_auth(&token)
        .json(&json!({ "broadcaster_user_id": broadcaster, "events": events, "method": "webhook" }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| unreachable)?;
    match res.status().as_u16() {
        200..=299 => {}
        401 => {
            st.bot.forget_kick_access(&ch.account_id);
            return Err(signed_out);
        }
        403 => {
            return Err((
                "kick_not_allowed",
                "Kick didn't let Ferrocast read this channel's events.",
                "Disconnect Kick and connect it again, allowing everything listed.",
            ));
        }
        _ => return Err(unreachable),
    }
    let v: Value = res.json().await.unwrap_or(Value::Null);
    let ids: Vec<String> = v["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["subscription_id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    let _ = st.db.cloud_set_event_subs(&ch.platform, &ch.channel_id, &ids);
    Ok(())
}

/// Deletes a channel's event subscriptions at the platform (best effort; ids forgotten).
pub(crate) async fn unsubscribe(st: &AppState, ch: &Channel) {
    let ids = st.db.cloud_channel(&ch.platform, &ch.channel_id).ok().flatten().map_or_else(Vec::new, |c| c.event_subs);
    let ids: Vec<String> = ids.into_iter().chain(ch.event_subs.iter().cloned()).fold(Vec::new(), |mut v, id| {
        if !v.contains(&id) {
            v.push(id);
        }
        v
    });
    if ids.is_empty() {
        return;
    }
    match ch.platform.as_str() {
        "twitch" => {
            if let (Ok(api), Ok(app)) = (st.bot.twitch(), st.bot.app_token().await) {
                for id in &ids {
                    if let Err(err) = api.unsubscribe(&app, id).await {
                        tracing::debug!(error = %err, "couldn't delete a cloud EventSub subscription");
                    }
                }
            }
        }
        "kick" => {
            if let Ok(token) = crate::bot::kick::access(st, &ch.account_id).await {
                for id in &ids {
                    let r = st
                        .http
                        .delete(format!("{}/public/v1/events/subscriptions", st.cfg.kick_api_url))
                        .query(&[("id", id.as_str())])
                        .bearer_auth(&token)
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await;
                    if let Err(err) = r {
                        tracing::debug!(error = %err, "couldn't delete a cloud Kick subscription");
                    }
                }
            }
        }
        _ => {}
    }
    let _ = st.db.cloud_set_event_subs(&ch.platform, &ch.channel_id, &[]);
}

/// Expired hand-overs: their sign-ins given back to the platform (part of the hourly sweep).
pub(crate) async fn sweep_pending(st: &AppState) {
    let gone = st.db.cloud_prune_pending(now_ms()).unwrap_or_default();
    for (account, platform, payload) in gone {
        if let Some(p) = open_pending(st, &account, &platform, &payload) {
            revoke(st, &p).await;
        }
    }
}
