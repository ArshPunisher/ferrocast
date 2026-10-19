//! Twitch for the cloud bot: a second, confidential app "Ferrocast Cloud" (R-23). The
//! streamer's bot account grants `user:bot user:read:chat user:write:chat`, their own
//! account grants `channel:bot`; chat arrives through an EventSub Conduit whose shard is a
//! webhook at `/v1/bot/twitch/eventsub`; replies go out with the app token (Send Chat
//! Message), which is what shows the Chat Bot badge.
//!
//! Both sign-in steps are tied to the Twitch account the PC itself is signed in to: the PC
//! sends its desktop-app token with `connect`, the relay asks Twitch whose it is, and the
//! callback refuses a channel step signed in by anyone else.
//!
//! Every Twitch call sits behind [`TwitchCloud`] (a fake in tests). Tokens and the client
//! secret never appear in `Debug`, an error or a log.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrocast_core::event::{Millis, Platform};
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::{Fut, SendErr, TwitchConfig};
use crate::{ApiError, AppState, now_ms};

pub const ID_BASE: &str = "https://id.twitch.tv";
pub const HELIX_BASE: &str = "https://api.twitch.tv/helix";
const TIMEOUT: Duration = Duration::from_secs(8);

/// The speaking account's scopes; the moderator ones only with the opt-in.
pub const BOT_SCOPES: &[&str] = &["user:bot", "user:read:chat", "user:write:chat"];
pub const MOD_SCOPES: &[&str] = &["moderator:manage:banned_users", "moderator:manage:chat_messages"];
pub const CHANNEL_SCOPES: &[&str] = &["channel:bot"];
/// EventSub messages older (or newer) than this are refused (Twitch's own advice).
pub const MAX_AGE_MS: i64 = 10 * 60_000;
/// `pending_oauth` keys of the Twitch steps (single use).
const NONCE_PREFIX: &str = "twbot.";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TwitchError {
    #[error("Twitch refused the sign-in")]
    Unauthorized,
    #[error("Twitch refused: {0}")]
    Forbidden(String),
    /// Send Chat Message said `is_sent: false` (AutoMod, slow mode, followers-only …).
    #[error("Twitch didn't show the message: {0}")]
    Dropped(String),
    #[error("Twitch asked to slow down for {retry_after_s} s")]
    RateLimited { retry_after_s: u64 },
    #[error("it already exists")]
    Conflict,
    #[error("not found")]
    NotFound,
    #[error("Twitch said the request was wrong: {0}")]
    BadRequest(String),
    #[error("Twitch is unreachable: {0}")]
    Unavailable(String),
}

/// A user's sign-in, from a code or a refresh.
#[derive(Clone, PartialEq, Eq)]
pub struct UserTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for UserTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserTokens").field("scopes", &self.scopes).finish_non_exhaustive()
    }
}

/// What `oauth2/validate` says about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub user_id: String,
    pub login: String,
    pub scopes: Vec<String>,
    /// The app the token was issued to.
    pub client_id: String,
}

/// Twitch's REST for the cloud bot.
pub trait TwitchCloud: Send + Sync + std::fmt::Debug {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, UserTokens, TwitchError>;
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, UserTokens, TwitchError>;
    /// Any app's user token (the cloud's own, or the desktop app's the PC sends).
    fn validate<'a>(&'a self, access_token: &'a str) -> Fut<'a, TokenInfo, TwitchError>;
    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, (), TwitchError>;
    /// An app access token (client credentials) and its lifetime in seconds.
    fn app_token(&self) -> Fut<'_, (String, u64), TwitchError>;
    /// The app's conduit (made if there is none) with shard 0 pointed at `callback`
    /// (left alone when it's already enabled there).
    fn conduit<'a>(&'a self, app: &'a str, callback: &'a str, secret: &'a str) -> Fut<'a, String, TwitchError>;
    /// A subscription on the conduit; its id. `Conflict` when it already exists.
    fn subscribe<'a>(
        &'a self,
        app: &'a str,
        conduit: &'a str,
        kind: &'a str,
        condition: Value,
    ) -> Fut<'a, String, TwitchError>;
    /// The id of an existing subscription of `kind` with exactly `condition` (after a `Conflict`).
    fn find_subscription<'a>(
        &'a self,
        app: &'a str,
        kind: &'a str,
        condition: &'a Value,
    ) -> Fut<'a, Option<String>, TwitchError>;
    fn unsubscribe<'a>(&'a self, app: &'a str, id: &'a str) -> Fut<'a, (), TwitchError>;
    /// Send Chat Message with the app token (`sender` is the bot account); the message id.
    fn send_chat<'a>(
        &'a self,
        app: &'a str,
        broadcaster: &'a str,
        sender: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, TwitchError>;
    /// When the channel's stream started (ms), or `None` while it's offline (Get Streams).
    fn stream_started<'a>(&'a self, app: &'a str, broadcaster: &'a str) -> Fut<'a, Option<i64>, TwitchError>;
}

/// The production client.
pub struct HttpTwitch {
    http: reqwest::Client,
    id_base: String,
    helix: String,
    client_id: String,
    client_secret: String,
}

impl std::fmt::Debug for HttpTwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTwitch").field("client_id", &self.client_id).field("client_secret", &"<redacted>").finish()
    }
}

impl HttpTwitch {
    pub fn new(cfg: &TwitchConfig) -> HttpTwitch {
        HttpTwitch::with_bases(cfg, ID_BASE, HELIX_BASE)
    }

    /// Against other addresses (a loopback fake in tests).
    pub fn with_bases(cfg: &TwitchConfig, id_base: &str, helix: &str) -> HttpTwitch {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("FerrocastCloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        HttpTwitch {
            http,
            id_base: id_base.trim_end_matches('/').into(),
            helix: helix.trim_end_matches('/').into(),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
        }
    }

    async fn check(res: Result<reqwest::Response, reqwest::Error>) -> Result<Value, TwitchError> {
        let res = res.map_err(|e| TwitchError::Unavailable(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        let reset =
            res.headers().get("ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        if (200..300).contains(&status) {
            if status == 204 {
                return Ok(Value::Null);
            }
            return Ok(res.json().await.unwrap_or(Value::Null));
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let msg = body["message"].as_str().unwrap_or("").chars().take(200).collect::<String>();
        Err(match status {
            401 => TwitchError::Unauthorized,
            403 => TwitchError::Forbidden(msg),
            404 => TwitchError::NotFound,
            409 => TwitchError::Conflict,
            429 => TwitchError::RateLimited {
                retry_after_s: reset.map_or(30, |r| u64::try_from(r - now_ms() / 1000).unwrap_or(1).clamp(1, 300)),
            },
            400 => TwitchError::BadRequest(msg),
            _ => TwitchError::Unavailable(format!("status {status}")),
        })
    }

    fn tokens(v: &Value) -> Result<UserTokens, TwitchError> {
        let access_token = v["access_token"].as_str().ok_or(TwitchError::Unauthorized)?.to_string();
        Ok(UserTokens {
            access_token,
            refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
            scopes: scopes(&v["scope"]),
        })
    }

    fn helix(&self, method: reqwest::Method, path: &str, app: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.helix)).bearer_auth(app).header("Client-Id", &self.client_id)
    }
}

fn scopes(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

impl TwitchCloud for HttpTwitch {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ];
            let v =
                Self::check(self.http.post(format!("{}/oauth2/token", self.id_base)).form(&form).send().await).await?;
            Self::tokens(&v)
        })
    }

    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ];
            let res = self.http.post(format!("{}/oauth2/token", self.id_base)).form(&form).send().await;
            // A refresh token Twitch no longer takes answers 400: the sign-in is gone.
            match Self::check(res).await {
                Err(TwitchError::BadRequest(_)) => Err(TwitchError::Unauthorized),
                other => Self::tokens(&other?),
            }
        })
    }

    fn validate<'a>(&'a self, access_token: &'a str) -> Fut<'a, TokenInfo, TwitchError> {
        Box::pin(async move {
            let res = self
                .http
                .get(format!("{}/oauth2/validate", self.id_base))
                .header(header::AUTHORIZATION, format!("OAuth {access_token}"))
                .send()
                .await;
            let v = Self::check(res).await?;
            Ok(TokenInfo {
                user_id: v["user_id"].as_str().ok_or(TwitchError::Unauthorized)?.to_string(),
                login: v["login"].as_str().unwrap_or("").to_string(),
                scopes: scopes(&v["scopes"]),
                client_id: v["client_id"].as_str().unwrap_or("").to_string(),
            })
        })
    }

    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, (), TwitchError> {
        Box::pin(async move {
            let form = [("client_id", self.client_id.as_str()), ("token", access_token)];
            Self::check(self.http.post(format!("{}/oauth2/revoke", self.id_base)).form(&form).send().await).await?;
            Ok(())
        })
    }

    fn app_token(&self) -> Fut<'_, (String, u64), TwitchError> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "client_credentials"),
            ];
            let v =
                Self::check(self.http.post(format!("{}/oauth2/token", self.id_base)).form(&form).send().await).await?;
            let t = v["access_token"].as_str().ok_or(TwitchError::Unauthorized)?.to_string();
            Ok((t, v["expires_in"].as_u64().unwrap_or(3_600)))
        })
    }

    fn conduit<'a>(&'a self, app: &'a str, callback: &'a str, secret: &'a str) -> Fut<'a, String, TwitchError> {
        Box::pin(async move {
            let list = Self::check(self.helix(reqwest::Method::GET, "/eventsub/conduits", app).send().await).await?;
            let id = match list.pointer("/data/0/id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => {
                    let made = Self::check(
                        self.helix(reqwest::Method::POST, "/eventsub/conduits", app)
                            .json(&json!({ "shard_count": 1 }))
                            .send()
                            .await,
                    )
                    .await?;
                    made.pointer("/data/0/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| TwitchError::Unavailable("no conduit id".into()))?
                        .to_string()
                }
            };
            // Shard 0 already enabled at this callback: nothing to change (a PATCH would make
            // Twitch verify the callback again).
            let shards = Self::check(
                self.helix(reqwest::Method::GET, "/eventsub/conduits/shards", app)
                    .query(&[("conduit_id", id.as_str())])
                    .send()
                    .await,
            )
            .await?;
            let shard0 = shards["data"].as_array().and_then(|a| a.iter().find(|s| s["id"] == "0"));
            if shard0
                .is_some_and(|s| s["status"] == "enabled" && s.pointer("/transport/callback") == Some(&json!(callback)))
            {
                return Ok(id);
            }
            let body = json!({ "conduit_id": id, "shards": [{ "id": "0", "transport": {
                "method": "webhook", "callback": callback, "secret": secret } }] });
            let res = Self::check(
                self.helix(reqwest::Method::PATCH, "/eventsub/conduits/shards", app).json(&body).send().await,
            )
            .await?;
            if let Some(e) = res.pointer("/errors/0/message").and_then(Value::as_str) {
                return Err(TwitchError::BadRequest(e.chars().take(200).collect()));
            }
            Ok(id)
        })
    }

    fn subscribe<'a>(
        &'a self,
        app: &'a str,
        conduit: &'a str,
        kind: &'a str,
        condition: Value,
    ) -> Fut<'a, String, TwitchError> {
        Box::pin(async move {
            let body = json!({ "type": kind, "version": "1", "condition": condition,
                               "transport": { "method": "conduit", "conduit_id": conduit } });
            let v =
                Self::check(self.helix(reqwest::Method::POST, "/eventsub/subscriptions", app).json(&body).send().await)
                    .await?;
            Ok(v.pointer("/data/0/id").and_then(Value::as_str).unwrap_or("").to_string())
        })
    }

    fn find_subscription<'a>(
        &'a self,
        app: &'a str,
        kind: &'a str,
        condition: &'a Value,
    ) -> Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move {
            let Some(user) = condition["broadcaster_user_id"].as_str() else { return Ok(None) };
            let mut after: Option<String> = None;
            // A channel has a handful; a few pages at most.
            for _ in 0..5 {
                let mut q = vec![("user_id", user.to_string())];
                if let Some(a) = &after {
                    q.push(("after", a.clone()));
                }
                let v = Self::check(
                    self.helix(reqwest::Method::GET, "/eventsub/subscriptions", app).query(&q).send().await,
                )
                .await?;
                let hit = v["data"].as_array().and_then(|a| {
                    a.iter().find(|s| s["type"] == kind && &s["condition"] == condition).and_then(|s| s["id"].as_str())
                });
                if let Some(id) = hit {
                    return Ok(Some(id.to_string()));
                }
                match v.pointer("/pagination/cursor").and_then(Value::as_str) {
                    Some(c) if !c.is_empty() => after = Some(c.to_string()),
                    _ => break,
                }
            }
            Ok(None)
        })
    }

    fn unsubscribe<'a>(&'a self, app: &'a str, id: &'a str) -> Fut<'a, (), TwitchError> {
        Box::pin(async move {
            Self::check(
                self.helix(reqwest::Method::DELETE, "/eventsub/subscriptions", app).query(&[("id", id)]).send().await,
            )
            .await?;
            Ok(())
        })
    }

    fn send_chat<'a>(
        &'a self,
        app: &'a str,
        broadcaster: &'a str,
        sender: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move {
            let mut body = json!({ "broadcaster_id": broadcaster, "sender_id": sender, "message": text });
            if let Some(r) = reply_to {
                body["reply_parent_message_id"] = json!(r);
            }
            let v =
                Self::check(self.helix(reqwest::Method::POST, "/chat/messages", app).json(&body).send().await).await?;
            if v.pointer("/data/0/is_sent").and_then(Value::as_bool) == Some(false) {
                let why = v.pointer("/data/0/drop_reason/message").and_then(Value::as_str).unwrap_or("dropped");
                return Err(TwitchError::Dropped(why.chars().take(200).collect()));
            }
            Ok(v.pointer("/data/0/message_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
        })
    }

    fn stream_started<'a>(&'a self, app: &'a str, broadcaster: &'a str) -> Fut<'a, Option<i64>, TwitchError> {
        Box::pin(async move {
            let v = Self::check(
                self.helix(reqwest::Method::GET, "/streams", app).query(&[("user_id", broadcaster)]).send().await,
            )
            .await?;
            let Some(s) = v.pointer("/data/0") else { return Ok(None) };
            if s["type"] != "live" {
                return Ok(None);
            }
            Ok(Some(s["started_at"].as_str().and_then(parse_time).unwrap_or_else(now_ms)))
        })
    }
}

// ── Signed state ─────────────────────────────────────────────────────────────────────

/// Which step a sign-in is: the bot account (with or without moderator scopes) or the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Bot { moderation: bool },
    Channel,
}

impl Step {
    fn tag(self) -> &'static str {
        match self {
            Step::Bot { moderation: false } => "b",
            Step::Bot { moderation: true } => "bm",
            Step::Channel => "c",
        }
    }

    pub fn scopes(self) -> Vec<&'static str> {
        match self {
            Step::Bot { moderation } => {
                let mut s = BOT_SCOPES.to_vec();
                if moderation {
                    s.extend_from_slice(MOD_SCOPES);
                }
                s
            }
            Step::Channel => CHANNEL_SCOPES.to_vec(),
        }
    }
}

/// What a checked state carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateInfo {
    pub step: Step,
    pub install: String,
    /// The Twitch account the PC is signed in to (from Twitch's check of its token).
    pub owner: String,
    pub nonce: String,
}

fn mac(cfg: &TwitchConfig, payload: &str) -> String {
    // Its own key, derived from the client secret (never the secret itself).
    let key = super::derive_key(cfg.client_secret.as_bytes(), "cloud bot twitch state v2");
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut m = Hmac::<Sha256>::new_from_slice(&key).expect("hmac takes any key");
    m.update(payload.as_bytes());
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// `<step>.<install>.<expires ms>.<nonce>.<owner>.<hmac>`, 10 minutes. The nonce is kept in
/// `pending_oauth` by [`auth_url`] and taken by the callback (one use).
pub(crate) fn sign_state(cfg: &TwitchConfig, step: Step, install: &str, owner: &str, nonce: &str, now: i64) -> String {
    let payload = format!("{}.{install}.{}.{nonce}.{owner}", step.tag(), now + super::STATE_TTL_MS);
    let m = mac(cfg, &payload);
    format!("{payload}.{m}")
}

pub(crate) fn check_state(cfg: &TwitchConfig, raw: &str, now: i64) -> Option<StateInfo> {
    let (payload, m) = raw.rsplit_once('.')?;
    if !bool::from(mac(cfg, payload).as_bytes().ct_eq(m.as_bytes())) {
        return None;
    }
    let mut parts = payload.split('.');
    let step = match parts.next()? {
        "b" => Step::Bot { moderation: false },
        "bm" => Step::Bot { moderation: true },
        "c" => Step::Channel,
        _ => return None,
    };
    let install = parts.next()?.to_string();
    let exp: i64 = parts.next()?.parse().ok()?;
    let nonce = parts.next()?.to_string();
    let owner = parts.next()?.to_string();
    if parts.next().is_some() || now > exp || install.is_empty() || owner.is_empty() || nonce.is_empty() {
        return None;
    }
    Some(StateInfo { step, install, owner, nonce })
}

pub(crate) fn redirect_uri(st: &AppState) -> String {
    format!("{}/v1/bot/twitch/callback", st.cfg.public_url)
}

pub(crate) fn callback_url(st: &AppState) -> String {
    format!("{}/v1/bot/twitch/eventsub", st.cfg.public_url)
}

/// Twitch's sign-in page for one step. `force_verify` so the streamer can pick the bot
/// account rather than the one their browser is signed in with. `owner` is the PC's own
/// Twitch account (id, login), checked by Twitch.
pub(crate) fn auth_url(
    st: &AppState,
    cfg: &TwitchConfig,
    step: Step,
    install: &str,
    owner: (&str, &str),
    now: i64,
) -> Result<String, ApiError> {
    let nonce = crate::random(12);
    st.db.put_oauth(&format!("{NONCE_PREFIX}{nonce}"), install, owner.1).map_err(ApiError::db)?;
    let mut url = url::Url::parse(&format!("{ID_BASE}/oauth2/authorize")).map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "platform_unavailable", "Twitch's sign-in address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &redirect_uri(st))
        .append_pair("response_type", "code")
        .append_pair("scope", &step.scopes().join(" "))
        .append_pair("force_verify", "true")
        .append_pair("state", &sign_state(cfg, step, install, owner.0, &nonce, now));
    Ok(url.to_string())
}

/// Checks the desktop-app token a PC sent with `connect`: whose it is (Twitch says) and that
/// it was issued to Ferrocast's desktop app. `(user id, login)`.
pub(crate) async fn owner_of(
    st: &AppState,
    cfg: &TwitchConfig,
    token: Option<&str>,
) -> Result<(String, String), ApiError> {
    let not_linked = |m: &str| {
        ApiError::new(StatusCode::CONFLICT, "twitch_not_linked", m)
            .hint("Connect Twitch in Accounts on this PC, then try again: the cloud bot speaks in that channel.")
    };
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(not_linked("Connect Twitch in Accounts on this PC first."));
    };
    let api = st.bot.twitch()?;
    match api.validate(token).await {
        Ok(info) => {
            if let Some(want) = cfg.desktop_client_id.as_deref()
                && !bool::from(want.as_bytes().ct_eq(info.client_id.as_bytes()))
            {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "wrong_twitch_app",
                    "That Twitch sign-in isn't from Ferrocast on this PC.",
                )
                .hint("Connect Twitch in Accounts on this PC, then try again."));
            }
            Ok((info.user_id, info.login))
        }
        Err(TwitchError::Unauthorized) => Err(not_linked("Twitch on this PC needs signing in again.")),
        Err(TwitchError::RateLimited { retry_after_s }) => {
            Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "twitch_rate", "Twitch asked Ferrocast to slow down.")
                .hint("Try again in a minute.")
                .header(header::RETRY_AFTER, retry_after_s.to_string()))
        }
        Err(e) => {
            tracing::debug!(error = %e, "couldn't check a PC's Twitch sign-in");
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "twitch_unreachable",
                "Couldn't reach Twitch to check this PC's account.",
            )
            .hint("Try again in a minute."))
        }
    }
}

// ── EventSub ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    BadSignature,
    Stale,
}

/// RFC 3339 in UTC (`2026-10-19T14:56:51.634234626Z`, or `+00:00`) → Unix ms.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        ms = format!("{:0<3}", &digits[..digits.len().min(3)]).parse::<i64>().ok()?;
        rest = &frac[digits.len()..];
    }
    if !matches!(rest, "Z" | "z" | "+00:00") {
        return None;
    }
    // Days since 1970-01-01 (the civil-from-days inverse).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3_600 + mi * 60 + sec) * 1000) + ms)
}

/// Twitch's signature: `sha256=` + hex HMAC-SHA256(secret, id + timestamp + body), compared
/// in constant time; then the timestamp within 10 minutes of now.
pub(crate) fn verify(secret: &str, id: &str, ts: &str, body: &[u8], sig: &str, now: i64) -> Result<(), Refusal> {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac takes any key");
    m.update(id.as_bytes());
    m.update(ts.as_bytes());
    m.update(body);
    let want: String = m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let got = sig.trim().strip_prefix("sha256=").unwrap_or("");
    if !bool::from(want.as_bytes().ct_eq(got.to_ascii_lowercase().as_bytes())) {
        return Err(Refusal::BadSignature);
    }
    match parse_time(ts) {
        Some(at) if (now - at).abs() <= MAX_AGE_MS => Ok(()),
        _ => Err(Refusal::Stale),
    }
}

/// `POST /v1/bot/twitch/eventsub`: the conduit's webhook shard.
pub(crate) async fn eventsub(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let Some(cfg) = st.bot.twitch_cfg() else {
        return super::platform_unavailable("twitch").into_response();
    };
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (id, ts, sig, ty) = (
        h("twitch-eventsub-message-id"),
        h("twitch-eventsub-message-timestamp"),
        h("twitch-eventsub-message-signature"),
        h("twitch-eventsub-message-type"),
    );
    if id.is_empty() || ts.is_empty() || sig.is_empty() || ty.is_empty() {
        return ApiError::new(StatusCode::BAD_REQUEST, "missing_headers", "Twitch EventSub headers are missing.")
            .into_response();
    }
    match verify(&cfg.eventsub_secret, &id, &ts, &body, &sig, now_ms()) {
        Ok(()) => {}
        Err(Refusal::BadSignature) => {
            st.bot.note_bad_signature(now_ms());
            return ApiError::new(StatusCode::FORBIDDEN, "bad_signature", "The message isn't signed by Twitch.")
                .into_response();
        }
        Err(Refusal::Stale) => {
            return ApiError::new(StatusCode::FORBIDDEN, "stale_message", "The message is too old or from the future.")
                .into_response();
        }
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The message body isn't JSON.").into_response();
    };
    // One look in the `seen` table: `false` means a replay or a retry already handled.
    let key = format!("tw:{id}");
    let db = st.db.clone();
    match tokio::task::spawn_blocking(move || db.first_time(&key)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => return ApiError::db(e).into_response(),
        Err(_) => return super::storage_busy().into_response(),
    }
    match ty.as_str() {
        "webhook_callback_verification" => {
            let challenge = v["challenge"].as_str().unwrap_or("").to_string();
            let mut r = (StatusCode::OK, challenge).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/plain"));
            r
        }
        "revocation" => {
            let st2 = st.clone();
            st.bot.work_twitch(Box::pin(async move { revoked(&st2, &v).await }));
            StatusCode::NO_CONTENT.into_response()
        }
        "notification" => {
            // Answer at once (Twitch drops slow shards); the work runs after, bounded.
            let st2 = st.clone();
            st.bot.work_twitch(Box::pin(async move { notification(&st2, &id, &v).await }));
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

fn broadcaster_of(v: &Value) -> Option<String> {
    v.pointer("/subscription/condition/broadcaster_user_id").and_then(Value::as_str).map(String::from)
}

/// Twitch stopped a subscription. The grant was removed or an account is gone: the channel
/// is unbound (its subscriptions deleted, its sign-in dropped), so a later owner can connect
/// it again. Anything else shows an error the hourly sweep retries.
pub(crate) async fn revoked(st: &AppState, v: &Value) {
    let Some(b) = broadcaster_of(v) else { return };
    let Ok(Some(ch)) = super::db(st, move |db| db.bot_channel_by_channel("twitch", &b)).await else { return };
    let status = v.pointer("/subscription/status").and_then(Value::as_str).unwrap_or("revoked");
    match status {
        "authorization_revoked" | "user_removed" => {
            let message = if status == "user_removed" {
                "Twitch says the bot or channel account is gone."
            } else {
                "Twitch says the cloud bot's permission was removed."
            };
            let err = super::error_json(
                "twitch_revoked",
                message,
                "Press \"Allow it in your channel\" in Ferrocast to connect it again.",
            );
            unbind(st, &ch, &err).await;
        }
        _ => {
            let message = match status {
                "moderator_removed" => "Twitch says the bot is no longer a moderator there.",
                _ => "Twitch stopped sending chat to the cloud bot.",
            };
            let err = super::error_json(
                "twitch_unavailable",
                message,
                "Ferrocast tries again within the hour, or press Try again.",
            );
            if let Err(e) = st.db.bot_set_error(&ch.sub_id, "twitch", Some(&err)) {
                tracing::warn!(error = %e, "couldn't record a revoked EventSub subscription");
            }
        }
    }
}

/// Unbinds a Twitch channel from its subscription: EventSub subscriptions deleted, the
/// channel's sign-in revoked and dropped, the error kept for its dashboard.
pub(crate) async fn unbind(st: &AppState, ch: &super::store::Channel, err: &Value) {
    unsubscribe_all(st, ch).await;
    revoke_stored(st, &ch.sub_id, "channel").await;
    if let Err(e) = st.db.bot_unbind_channel(&ch.sub_id, "twitch", Some(err)) {
        tracing::warn!(error = %e, "couldn't unbind a cloud bot channel");
    }
    st.bot.forget_mem(&ch.sub_id, Some(Platform::Twitch));
}

async fn notification(st: &AppState, message_id: &str, v: &Value) {
    let kind = v.pointer("/subscription/type").and_then(Value::as_str).unwrap_or("");
    let Some(b) = broadcaster_of(v) else { return };
    let ch = match super::db(st, move |db| db.bot_channel_by_channel("twitch", &b)).await {
        Ok(Some(ch)) => ch,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't look up a cloud bot channel");
            return;
        }
    };
    let event = v.get("event").cloned().unwrap_or(Value::Null);
    match kind {
        "stream.online" => {
            let at = event["started_at"].as_str().and_then(parse_time).unwrap_or_else(now_ms);
            super::set_live(st, &ch.sub_id, Platform::Twitch, Some(at));
        }
        "stream.offline" => super::set_live(st, &ch.sub_id, Platform::Twitch, None),
        "channel.chat.message" => {
            let e = ferrocast_twitch::map::map(kind, message_id, &event, Millis(now_ms()));
            super::on_chat(st, &ch, Platform::Twitch, e).await;
        }
        _ => {}
    }
}

// ── The OAuth callback ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

const TRY_SHORTLY: &str = "The relay couldn't save that. Try again shortly.";

/// `GET /v1/bot/twitch/callback`.
pub(crate) async fn callback(State(st): State<AppState>, Query(q): Query<Callback>) -> Response {
    let page = crate::page;
    let Some(cfg) = st.bot.twitch_cfg() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The cloud bot isn't set up on this relay yet.");
    };
    if q.error.is_some() {
        return page(StatusCode::OK, "Twitch sign-in was cancelled. You can close this tab.");
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(StatusCode::BAD_REQUEST, "This link is incomplete. Go back to Ferrocast and press Connect again.");
    };
    let Some(s) = check_state(cfg, &state, now_ms()) else {
        return page(StatusCode::BAD_REQUEST, "This sign-in link expired. Press Connect again in Ferrocast.");
    };
    // One use: the nonce is taken here, so a state can never be replayed.
    let owner_login = match st.db.take_oauth(&format!("{NONCE_PREFIX}{}", s.nonce)) {
        Ok(Some((install, login))) if install == s.install => login,
        Ok(_) => {
            return page(
                StatusCode::BAD_REQUEST,
                "This sign-in link expired or was already used. Press Connect again in Ferrocast.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Ok(api) = st.bot.twitch() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The cloud bot isn't set up on this relay yet.");
    };
    let tokens = match api.exchange_code(&code, &redirect_uri(&st)).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "Twitch didn't take a cloud bot sign-in code");
            return page(
                StatusCode::BAD_GATEWAY,
                "Twitch didn't accept the sign-in. Press Connect again in Ferrocast.",
            );
        }
    };
    let revoke = || async {
        if let Err(e) = api.revoke(&tokens.access_token).await {
            tracing::debug!(error = %e, "couldn't revoke an unused Twitch sign-in");
        }
    };
    // Whose account: from Twitch's own token check, never from the URL.
    let info = match api.validate(&tokens.access_token).await {
        Ok(i) => i,
        Err(_) => {
            revoke().await;
            return page(StatusCode::BAD_GATEWAY, "Twitch didn't say whose account this is. Try again.");
        }
    };
    let ctx = match super::pro_for_install(&st, &s.install) {
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
    let missing: Vec<&str> = s.step.scopes().into_iter().filter(|x| !info.scopes.iter().any(|g| g == x)).collect();
    if !missing.is_empty() {
        revoke().await;
        return page(
            StatusCode::BAD_REQUEST,
            &format!("Twitch didn't allow {}. Press Connect again and allow everything listed.", missing.join(", ")),
        );
    }
    // The channel step proves the channel: it must be the account the PC is signed in to.
    if s.step == Step::Channel && info.user_id != s.owner {
        revoke().await;
        return page(
            StatusCode::FORBIDDEN,
            &format!(
                "You signed in as {}, but Ferrocast on this PC is connected to Twitch as {owner_login}. Press \"Allow it in your channel\" again and sign in as {owner_login}.",
                info.login
            ),
        );
    }
    let Ok(vault) = st.vault() else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, "The cloud bot isn't switched on on Ferrocast's servers yet.");
    };
    let purpose = match s.step {
        Step::Bot { .. } => "bot",
        Step::Channel => "channel",
    };
    let Ok(sealed) = vault.seal(&ctx.sub_id, &super::token_purpose("twitch", purpose), tokens.refresh_token.as_bytes())
    else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    let now = now_ms();
    // A sign-in this replaces is revoked at Twitch first (it would linger there otherwise).
    if st.db.bot_token(&ctx.sub_id, "twitch", purpose).ok().flatten().is_some() {
        revoke_stored(&st, &ctx.sub_id, purpose).await;
    }
    let saved = match s.step {
        Step::Bot { moderation } => {
            let has_mod = MOD_SCOPES.iter().all(|x| info.scopes.iter().any(|g| g == x));
            st.db
                .bot_set_account(
                    &ctx.sub_id,
                    "twitch",
                    (&info.user_id, &info.login),
                    None,
                    &info.scopes.join(" "),
                    moderation && has_mod,
                    &ctx.install,
                    now,
                )
                .map(|()| true)
        }
        Step::Channel => {
            match st.db.bot_channel_by_channel("twitch", &info.user_id) {
                // A proven owner takes the channel back from another subscription.
                Ok(Some(other)) if other.sub_id != ctx.sub_id => {
                    let err = super::error_json(
                        "twitch_channel_taken",
                        &format!("{} connected the cloud bot from another Ferrocast Pro.", info.login),
                        "Only the channel's owner can connect it. Disconnect Twitch here if you don't own it.",
                    );
                    unbind(&st, &other, &err).await;
                    tracing::info!("a proven channel owner took a cloud bot channel from another subscription");
                }
                Ok(_) => {}
                Err(_) => {
                    revoke().await;
                    return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
                }
            }
            st.db.bot_set_channel(&ctx.sub_id, "twitch", &info.user_id, &info.login)
        }
    };
    match saved {
        Ok(true) => {}
        Ok(false) => {
            revoke().await;
            return page(StatusCode::CONFLICT, "Connect the bot account first, then allow it in your channel.");
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't save a cloud bot sign-in");
            revoke().await;
            return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
        }
    }
    if let Err(e) = st.db.bot_put_token(&ctx.sub_id, "twitch", purpose, &sealed) {
        tracing::warn!(error = %e, "couldn't save a sealed Twitch token");
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    let code = super::pc_code(&st, &ctx.install);
    let ch = st.db.bot_channel(&ctx.sub_id, "twitch").ok().flatten();
    let check = format!(
        "This is for the Ferrocast PC with the code {code}; the Bot page there shows the same code. If it doesn't, or you didn't start this from your own Ferrocast, remove Ferrocast Cloud at twitch.tv/settings/connections."
    );
    match (s.step, ch) {
        (Step::Bot { .. }, Some(ch)) if ch.channel_id.is_none() => {
            let where_ = if info.user_id == s.owner {
                format!("{} is ready to speak for the cloud bot in its own chat.", info.login)
            } else {
                format!(
                    "{} is ready to speak for the cloud bot in {owner_login}'s chat. {} doesn't own that channel: it will post there as a bot.",
                    info.login, info.login
                )
            };
            page(
                StatusCode::OK,
                &format!(
                    "{where_} {check} Back in Ferrocast, press \"Allow it in your channel\" and sign in as {owner_login}."
                ),
            )
        }
        (_, Some(ch)) => {
            let enabled = st.db.bot_config(&ctx.sub_id).ok().flatten().is_some_and(|r| r.enabled);
            let problem = if enabled { subscribe_channel(&st, &ch).await.err() } else { None };
            match problem {
                None => page(
                    StatusCode::OK,
                    &format!(
                        "The cloud bot is set up in {}'s chat as {}. {check} You can close this tab.",
                        ch.channel_login.as_deref().unwrap_or(""),
                        ch.bot_login.as_deref().unwrap_or("")
                    ),
                ),
                Some(m) => page(StatusCode::BAD_GATEWAY, &m),
            }
        }
        (_, None) => page(StatusCode::OK, "Signed in. You can close this tab."),
    }
}

/// Subscribes a bound channel's chat and live status on the conduit (again: old ones first).
/// Each id is saved as it's made, so a failure halfway leaves nothing unknown at Twitch; one
/// that already exists is looked up. The error is also kept on the channel, for the dashboard.
pub(crate) async fn subscribe_channel(st: &AppState, ch: &super::store::Channel) -> Result<(), String> {
    let (Some(bot), Some(channel)) = (&ch.bot_id, &ch.channel_id) else { return Ok(()) };
    let result = async {
        let api = st.bot.twitch().map_err(|_| TwitchError::Unavailable("not configured".into()))?;
        let app = st.bot.app_token().await?;
        let conduit = st.bot.conduit(st).await?;
        for old in known_ids(st, ch) {
            let _ = api.unsubscribe(&app, &old).await;
        }
        let _ = st.db.bot_set_subscriptions(&ch.sub_id, "twitch", &[]);
        let mut ids: Vec<String> = Vec::new();
        for (kind, cond) in [
            ("channel.chat.message", json!({ "broadcaster_user_id": channel, "user_id": bot })),
            ("stream.online", json!({ "broadcaster_user_id": channel })),
            ("stream.offline", json!({ "broadcaster_user_id": channel })),
        ] {
            let id = match api.subscribe(&app, &conduit, kind, cond.clone()).await {
                Ok(id) if !id.is_empty() => Some(id),
                Ok(_) => None,
                Err(TwitchError::Conflict) => api.find_subscription(&app, kind, &cond).await.ok().flatten(),
                Err(e) => return Err(e),
            };
            if let Some(id) = id
                && !ids.contains(&id)
            {
                ids.push(id);
                let _ = st.db.bot_set_subscriptions(&ch.sub_id, "twitch", &ids);
            }
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            let _ = st.db.bot_clear_transient_error(&ch.sub_id, "twitch");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't subscribe a cloud bot channel");
            let (code, message, hint) = match e {
                TwitchError::Forbidden(_) | TwitchError::Unauthorized => (
                    "twitch_not_allowed",
                    "Twitch didn't let the bot read this chat.",
                    "Press \"Allow it in your channel\" with your own account, or make the bot a moderator, then Try again.",
                ),
                _ => (
                    "twitch_unavailable",
                    "Couldn't reach Twitch to start the cloud bot.",
                    "Ferrocast tries again within the hour, or press Try again.",
                ),
            };
            let _ = st.db.bot_set_error(&ch.sub_id, "twitch", Some(&super::error_json(code, message, hint)));
            Err(format!("{message} {hint}"))
        }
    }
}

/// The EventSub ids known for a channel: the stored ones (saved as they're made) and the
/// row's, each once.
fn known_ids(st: &AppState, ch: &super::store::Channel) -> Vec<String> {
    let stored = st.db.bot_channel(&ch.sub_id, "twitch").ok().flatten().map_or_else(Vec::new, |c| c.subscriptions);
    let mut out: Vec<String> = Vec::new();
    for id in stored.into_iter().chain(ch.subscriptions.iter().cloned()) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Deletes a channel's EventSub subscriptions (switched off, unbound, disconnected).
pub(crate) async fn unsubscribe_all(st: &AppState, ch: &super::store::Channel) {
    let ids = known_ids(st, ch);
    if ids.is_empty() {
        return;
    }
    if let (Ok(api), Ok(app)) = (st.bot.twitch(), st.bot.app_token().await) {
        for id in &ids {
            if let Err(e) = api.unsubscribe(&app, id).await {
                tracing::debug!(error = %e, "couldn't delete an EventSub subscription");
            }
        }
    }
    let _ = st.db.bot_set_subscriptions(&ch.sub_id, "twitch", &[]);
}

/// Sends one line in a Twitch channel; Twitch's message id.
pub(crate) async fn send(
    st: &AppState,
    ch: &super::store::Channel,
    text: &str,
    reply_to: Option<&str>,
) -> Result<Option<String>, SendErr> {
    let (Some(bot), Some(channel)) = (&ch.bot_id, &ch.channel_id) else { return Err(SendErr::NotReady) };
    let api = st.bot.twitch().map_err(|_| SendErr::NotReady)?;
    let app = st.bot.app_token().await.map_err(SendErr::from)?;
    match api.send_chat(&app, channel, bot, text, reply_to).await {
        Ok(id) => Ok(id),
        Err(TwitchError::Unauthorized) => {
            // The app token expired early: once more with a fresh one.
            st.bot.forget_app_token().await;
            let app = st.bot.app_token().await.map_err(SendErr::from)?;
            api.send_chat(&app, channel, bot, text, reply_to).await.map_err(SendErr::from)
        }
        Err(e) => Err(SendErr::from(e)),
    }
}

impl From<TwitchError> for SendErr {
    fn from(e: TwitchError) -> Self {
        match e {
            TwitchError::RateLimited { retry_after_s } => SendErr::RateLimited(retry_after_s),
            TwitchError::Unauthorized => SendErr::SignedOut,
            TwitchError::Forbidden(m) => SendErr::Refused(m),
            TwitchError::Dropped(m) => SendErr::Dropped(m),
            other => SendErr::Unavailable(other.to_string()),
        }
    }
}

/// Revokes a stored Twitch sign-in (refresh, then revoke what it gives).
pub(crate) async fn revoke_stored(st: &AppState, sub: &str, purpose: &str) {
    let Ok(api) = st.bot.twitch() else { return };
    let Some(refresh) = super::open_token(st, sub, "twitch", purpose) else { return };
    match api.refresh(&refresh).await {
        Ok(t) => {
            if let Err(e) = api.revoke(&t.access_token).await {
                tracing::debug!(error = %e, "couldn't revoke a Twitch sign-in");
            }
        }
        Err(e) => tracing::debug!(error = %e, "a Twitch sign-in was already gone"),
    }
}

/// Part of the hourly sweep (Twitch asks apps to validate sign-ins hourly): refreshes each
/// connected bot sign-in; one Twitch no longer takes marks the channel signed out.
pub(crate) async fn check_signins(st: &AppState, ch: &super::store::Channel) {
    let Ok(api) = st.bot.twitch() else { return };
    let Some(refresh) = super::open_token(st, &ch.sub_id, "twitch", "bot") else { return };
    match api.refresh(&refresh).await {
        Ok(t) => {
            if !t.refresh_token.is_empty() && t.refresh_token != refresh {
                super::seal_token(st, &ch.sub_id, "twitch", "bot", &t.refresh_token);
            }
        }
        Err(TwitchError::Unauthorized) => {
            let err = super::error_json(
                "twitch_signed_out",
                "Twitch signed the bot account out.",
                "Disconnect Twitch here and connect it again.",
            );
            let _ = st.db.bot_set_error(&ch.sub_id, "twitch", Some(&err));
        }
        Err(e) => tracing::debug!(error = %e, "couldn't check a Twitch sign-in this hour"),
    }
}

/// Asks Twitch whether the channel is live now (after a relay restart, or a switch back on,
/// when `stream.online/offline` may have been missed).
pub(crate) async fn reconcile_live(st: &AppState, ch: &super::store::Channel) {
    let Some(channel) = &ch.channel_id else { return };
    let (Ok(api), Ok(app)) = (st.bot.twitch(), st.bot.app_token().await) else { return };
    match api.stream_started(&app, channel).await {
        Ok(started) => {
            if started.is_some() != ch.live_since.is_some() {
                super::set_live(st, &ch.sub_id, Platform::Twitch, started);
            }
        }
        Err(e) => tracing::debug!(error = %e, "couldn't ask Twitch whether a channel is live"),
    }
}
