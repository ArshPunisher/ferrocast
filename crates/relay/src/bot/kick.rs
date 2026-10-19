//! Kick for the cloud bot. Kick's webhooks already reach the relay (`crate::kick_webhook`),
//! routed to the install that connected the channel; replies need an opt-in sign-in with
//! `chat:write`, made through the relay's existing Kick broker (`/v1/kick/oauth`'s flow and
//! its `/v1/kick/callback`, with a `bot.` state so the tokens stay here, sealed, instead of
//! going to the PC).
//!
//! The channel is always the one this install owns in `kick_owners` (it signed in to Kick as
//! that channel in Accounts), checked again when Kick sends the browser back.
//!
//! Every Kick call sits behind [`KickBot`] (a fake in tests).

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine as _;
use ferrocast_core::event::{Millis, Platform};
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};

use super::{Fut, SendErr};
use crate::billing::gate::ProCtx;
use crate::{ApiError, AppState, now_ms};

/// The cloud bot's Kick scopes; `moderation:ban` only with the opt-in.
pub const SCOPES: &[&str] = &["user:read", "chat:write"];
pub const MOD_SCOPES: &[&str] = &["moderation:ban"];
/// States from the cloud bot's Kick sign-in start with this (base64url never has a `.`).
pub const STATE_PREFIX: &str = "bot.";
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KickError {
    #[error("Kick refused the sign-in")]
    Unauthorized,
    #[error("Kick refused: {0}")]
    Forbidden(String),
    /// Kick took the request but won't show the message (400: too long, a blocked word …).
    #[error("Kick didn't show the message: {0}")]
    Dropped(String),
    #[error("Kick asked to slow down")]
    RateLimited { retry_after_s: u64 },
    #[error("Kick is unreachable: {0}")]
    Unavailable(String),
}

/// A fresh Kick sign-in from a refresh.
#[derive(Clone, PartialEq, Eq)]
pub struct KickTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for KickTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KickTokens").field("expires_in", &self.expires_in).finish_non_exhaustive()
    }
}

pub trait KickBot: Send + Sync + std::fmt::Debug {
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, KickTokens, KickError>;
    /// Posts a chat message; Kick's message id.
    fn send_chat<'a>(
        &'a self,
        access: &'a str,
        broadcaster: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, KickError>;
    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), KickError>;
}

/// The production client (the relay's Kick app).
pub struct HttpKick {
    http: reqwest::Client,
    id_url: String,
    api_url: String,
    client_id: String,
    client_secret: String,
}

impl std::fmt::Debug for HttpKick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpKick").field("client_id", &self.client_id).field("client_secret", &"<redacted>").finish()
    }
}

impl HttpKick {
    pub fn new(cfg: &crate::Config) -> Option<HttpKick> {
        let (Some(id), Some(secret)) = (&cfg.kick_client_id, &cfg.kick_client_secret) else { return None };
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("FerrocastCloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Some(HttpKick {
            http,
            id_url: cfg.kick_id_url.clone(),
            api_url: cfg.kick_api_url.clone(),
            client_id: id.clone(),
            client_secret: secret.clone(),
        })
    }

    async fn status(res: Result<reqwest::Response, reqwest::Error>) -> Result<reqwest::Response, KickError> {
        let res = res.map_err(|e| KickError::Unavailable(e.without_url().to_string()))?;
        let code = res.status().as_u16();
        match code {
            200..=299 => Ok(res),
            401 => Err(KickError::Unauthorized),
            400 | 403 => {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                let m: String = v["message"].as_str().unwrap_or("").chars().take(200).collect();
                Err(if code == 400 { KickError::Dropped(m) } else { KickError::Forbidden(m) })
            }
            429 => Err(KickError::RateLimited { retry_after_s: 30 }),
            s => Err(KickError::Unavailable(format!("status {s}"))),
        }
    }
}

impl KickBot for HttpKick {
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, KickTokens, KickError> {
        Box::pin(async move {
            let form = [
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
            ];
            let res = Self::status(self.http.post(format!("{}/oauth/token", self.id_url)).form(&form).send().await)
                .await
                .map_err(|e| match e {
                    KickError::Forbidden(_) | KickError::Dropped(_) => KickError::Unauthorized,
                    other => other,
                })?;
            let t: crate::kick::TokenResponse =
                res.json().await.map_err(|_| KickError::Unavailable("a bad token answer".into()))?;
            Ok(KickTokens { access_token: t.access_token, refresh_token: t.refresh_token, expires_in: t.expires_in })
        })
    }

    fn send_chat<'a>(
        &'a self,
        access: &'a str,
        broadcaster: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, KickError> {
        Box::pin(async move {
            let id: u64 = broadcaster.parse().map_err(|_| KickError::Forbidden("bad channel id".into()))?;
            let mut body = json!({ "type": "user", "content": text, "broadcaster_user_id": id });
            if let Some(r) = reply_to {
                body["reply_to_message_id"] = json!(r);
            }
            let res = Self::status(
                self.http.post(format!("{}/public/v1/chat", self.api_url)).bearer_auth(access).json(&body).send().await,
            )
            .await?;
            let v: Value = res.json().await.unwrap_or(Value::Null);
            Ok(v.pointer("/data/message_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
        })
    }

    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), KickError> {
        Box::pin(async move {
            Self::status(
                self.http
                    .post(format!("{}/oauth/revoke", self.id_url))
                    .query(&[("token", token), ("token_hint_type", "refresh_token")])
                    .send()
                    .await,
            )
            .await?;
            Ok(())
        })
    }
}

/// The Kick sign-in page for the cloud bot (`POST /v1/pro/bot/kick/connect`).
pub(crate) fn auth_url(st: &AppState, ctx: &ProCtx, moderation: bool) -> Result<String, ApiError> {
    let (client_id, _) = crate::kick_app(st).map_err(|_| super::platform_unavailable("kick"))?;
    let verifier = crate::random(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    // `bot.m.<random>` asked for the moderator scope too.
    let state = format!("{STATE_PREFIX}{}{}", if moderation { "m." } else { "" }, crate::random(24));
    st.db.put_oauth(&state, &ctx.install, &verifier).map_err(ApiError::db)?;
    let mut scopes = SCOPES.to_vec();
    if moderation {
        scopes.extend_from_slice(MOD_SCOPES);
    }
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", st.cfg.kick_id_url)).map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "platform_unavailable", "The relay's Kick address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &format!("{}/v1/kick/callback", st.cfg.public_url))
        .append_pair("scope", &scopes.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Ok(url.to_string())
}

const TRY_SHORTLY: &str = "The relay couldn't save that. Try again shortly.";

/// The relay's Kick callback with a `bot.` state: keep the sign-in here, sealed, for the
/// install's subscription. The account that signed in speaks; the channel is the one this
/// install owns in `kick_owners`. A binding another subscription still has for that channel
/// is stale (the owner is here): it's removed and its sign-in revoked.
pub(crate) async fn finish(
    st: &AppState,
    install: &str,
    tokens: &crate::kick::TokenResponse,
    user_id: &str,
    name: &str,
    state: &str,
) -> Response {
    let page = crate::page;
    // Every way out but success gives the fresh sign-in back to Kick.
    let revoke = || async {
        if let Ok(api) = st.bot.kick()
            && !tokens.refresh_token.is_empty()
            && let Err(e) = api.revoke(&tokens.refresh_token).await
        {
            tracing::debug!(error = %e, "couldn't revoke an unused Kick sign-in");
        }
    };
    let ctx = match super::pro_for_install(st, install) {
        Ok(Some(c)) => c,
        Ok(None) => {
            revoke().await;
            return page(StatusCode::PAYMENT_REQUIRED, "The cloud bot is part of Pro, and this PC's Pro isn't active.");
        }
        Err(_) => {
            revoke().await;
            return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
        }
    };
    let channel = match st.db.bot_kick_channel_of(install) {
        Ok(Some(c)) => c,
        Ok(None) => {
            revoke().await;
            return page(
                StatusCode::CONFLICT,
                "Connect Kick in Ferrocast's Accounts first, then connect the cloud bot.",
            );
        }
        Err(_) => {
            revoke().await;
            return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
        }
    };
    // Still this install's channel (it could have moved since Connect was pressed).
    if !matches!(st.db.kick_owner(&channel), Ok(Some(owner)) if owner == install) {
        revoke().await;
        return page(
            StatusCode::CONFLICT,
            "This PC isn't connected to that Kick channel any more. Connect Kick in Accounts, then try again.",
        );
    }
    if tokens.refresh_token.is_empty() {
        return page(StatusCode::BAD_GATEWAY, "Kick didn't give a lasting sign-in. Press Connect again in Ferrocast.");
    }
    let Ok(vault) = st.vault() else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, "The cloud bot isn't switched on on Ferrocast's servers yet.");
    };
    let Ok(sealed) = vault.seal(&ctx.sub_id, &super::token_purpose("kick", "bot"), tokens.refresh_token.as_bytes())
    else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    // The owner is here: a binding another subscription kept for this channel goes.
    match st.db.bot_channel_by_channel("kick", &channel) {
        Ok(Some(other)) if other.sub_id != ctx.sub_id => {
            super::disconnect_platform(st, &other.sub_id, Platform::Kick).await;
            tracing::info!("a Kick channel's owner took the cloud bot back from another subscription");
        }
        Ok(_) => {}
        Err(_) => {
            revoke().await;
            return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
        }
    }
    // A sign-in this replaces is revoked at Kick first.
    if st.db.bot_token(&ctx.sub_id, "kick", "bot").ok().flatten().is_some() {
        revoke_stored(st, &ctx.sub_id).await;
    }
    // Kick's token answer doesn't list scopes: what was asked for (Kick shows all or nothing).
    let moderation = state.starts_with(&format!("{STATE_PREFIX}m."));
    let mut asked = SCOPES.to_vec();
    if moderation {
        asked.extend_from_slice(MOD_SCOPES);
    }
    let scopes = asked.join(" ");
    // The channel's name: the signed-in account's when it's the same one.
    let channel_login = if channel == user_id { name.to_string() } else { String::new() };
    if let Err(e) = st.db.bot_set_account(
        &ctx.sub_id,
        "kick",
        (user_id, name),
        Some((&channel, &channel_login)),
        &scopes,
        moderation,
        install,
        now_ms(),
    ) {
        tracing::warn!(error = %e, "couldn't save a cloud bot Kick sign-in");
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    if st.db.bot_put_token(&ctx.sub_id, "kick", "bot", &sealed).is_err() {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    st.bot.cache_kick_access(&ctx.sub_id, &tokens.access_token, now_ms() + tokens.expires_in.clamp(0, 86_400) * 1000);
    let code = super::pc_code(st, install);
    let note = if channel == user_id {
        String::new()
    } else {
        format!(" {name} will post in this PC's Kick channel as a bot.")
    };
    page(
        StatusCode::OK,
        &format!(
            "The cloud bot can now answer in Kick chat as {name}.{note} This is for the Ferrocast PC with the code {code}; the Bot page there shows the same code. You can close this tab."
        ),
    )
}

/// A Kick webhook the relay verified and routed to `install` (`crate::kick_webhook`): queued
/// for the cloud bot (bounded; never holds up Kick).
pub(crate) fn queue_webhook(st: &AppState, install: String, broadcaster: String, ty: String, id: String, v: Value) {
    if !matches!(ty.as_str(), "chat.message.sent" | "livestream.status.updated") {
        return;
    }
    let st2 = st.clone();
    st.bot.work_kick(Box::pin(async move { on_webhook(&st2, &install, &broadcaster, &ty, &id, &v).await }));
}

/// One Kick webhook for the cloud bot.
pub(crate) async fn on_webhook(
    st: &AppState,
    install: &str,
    broadcaster: &str,
    ty: &str,
    message_id: &str,
    body: &Value,
) {
    if !matches!(ty, "chat.message.sent" | "livestream.status.updated") {
        return;
    }
    let (install2, b2) = (install.to_string(), broadcaster.to_string());
    let ch = super::db(st, move |db| -> rusqlite::Result<Option<super::store::Channel>> {
        let Some(sub) = db.pro_sub_for_install(&install2)? else { return Ok(None) };
        Ok(db.bot_channel(&sub.id, "kick")?.filter(|ch| ch.channel_id.as_deref() == Some(b2.as_str())))
    })
    .await;
    let Ok(Some(ch)) = ch else { return };
    match ty {
        "livestream.status.updated" => {
            let live = body.get("is_live").and_then(Value::as_bool).unwrap_or(false);
            let at = body.get("started_at").and_then(Value::as_str).and_then(super::twitch::parse_time);
            super::set_live(st, &ch.sub_id, Platform::Kick, live.then(|| at.unwrap_or_else(now_ms)));
        }
        _ => {
            let e = ferrocast_kick::map::map(ty, message_id, body, Millis(now_ms()));
            super::on_chat(st, &ch, Platform::Kick, e).await;
        }
    }
}

/// A usable access token for a subscription's Kick bot. One refresh at a time per
/// subscription (Kick's refresh tokens are single use): the cache is checked again under the
/// lock, the new refresh token sealed before the lock goes, and a failed refresh is retried
/// once when the sealed token changed meanwhile (another refresh won the race).
async fn access(st: &AppState, sub: &str) -> Result<String, SendErr> {
    if let Some(t) = st.bot.kick_access(sub, now_ms()) {
        return Ok(t);
    }
    let api = st.bot.kick().map_err(|_| SendErr::NotReady)?;
    let lock = st.bot.kick_lock(sub);
    let _one = lock.lock().await;
    if let Some(t) = st.bot.kick_access(sub, now_ms()) {
        return Ok(t);
    }
    let mut refresh = super::open_token(st, sub, "kick", "bot").ok_or(SendErr::SignedOut)?;
    let mut tries = 0;
    let t = loop {
        match api.refresh(&refresh).await {
            Ok(t) => break t,
            Err(KickError::Unauthorized) if tries == 0 => {
                tries += 1;
                match super::open_token(st, sub, "kick", "bot") {
                    Some(now) if now != refresh => refresh = now,
                    _ => return Err(SendErr::SignedOut),
                }
            }
            Err(e) => return Err(SendErr::from(e)),
        }
    };
    if !t.refresh_token.is_empty() && t.refresh_token != refresh {
        super::seal_token(st, sub, "kick", "bot", &t.refresh_token);
    }
    st.bot.cache_kick_access(sub, &t.access_token, now_ms() + t.expires_in.clamp(0, 86_400) * 1000);
    Ok(t.access_token)
}

/// Sends one line in the Kick channel; Kick's message id.
pub(crate) async fn send(
    st: &AppState,
    ch: &super::store::Channel,
    text: &str,
    reply_to: Option<&str>,
) -> Result<Option<String>, SendErr> {
    let Some(channel) = &ch.channel_id else { return Err(SendErr::NotReady) };
    let api = st.bot.kick().map_err(|_| SendErr::NotReady)?;
    let token = access(st, &ch.sub_id).await?;
    match api.send_chat(&token, channel, text, reply_to).await {
        Ok(id) => Ok(id),
        Err(KickError::Unauthorized) => {
            // Only this token: another sender may have cached a fresh one already.
            st.bot.forget_kick_access_if(&ch.sub_id, &token);
            let token = access(st, &ch.sub_id).await?;
            api.send_chat(&token, channel, text, reply_to).await.map_err(SendErr::from)
        }
        Err(e) => Err(SendErr::from(e)),
    }
}

impl From<KickError> for SendErr {
    fn from(e: KickError) -> Self {
        match e {
            KickError::Unauthorized => SendErr::SignedOut,
            KickError::RateLimited { retry_after_s } => SendErr::RateLimited(retry_after_s),
            KickError::Forbidden(m) => SendErr::Refused(m),
            KickError::Dropped(m) => SendErr::Dropped(m),
            KickError::Unavailable(m) => SendErr::Unavailable(m),
        }
    }
}

/// Revokes the stored Kick sign-in at Kick.
pub(crate) async fn revoke_stored(st: &AppState, sub: &str) {
    let Ok(api) = st.bot.kick() else { return };
    if let Some(refresh) = super::open_token(st, sub, "kick", "bot")
        && let Err(e) = api.revoke(&refresh).await
    {
        tracing::debug!(error = %e, "couldn't revoke a Kick sign-in");
    }
    st.bot.forget_kick_access(sub);
}
