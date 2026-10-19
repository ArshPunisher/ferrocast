//! YouTube for the cloud bot (Pro F1, milestone M12, a beta): a separate Google OAuth client
//! "Ferrocast Cloud" (type Web, `YOUTUBE_CLOUD_CLIENT_ID` / `YOUTUBE_CLOUD_CLIENT_SECRET`).
//!
//! - **Sign-in**: one step through a relay broker, the streamer's own channel. `connect`
//!   carries the PC's desktop YouTube token (`owner_token`); the relay asks Google whose it
//!   is (`tokeninfo`: issued to the desktop app's client id; `channels?mine=true`: which
//!   channel), signs that channel id into a single-use state, and the callback refuses (and
//!   revokes) a sign-in for any other channel. The cloud speaks as the channel itself, like
//!   the desktop bot does on YouTube. Scope: `youtube.force-ssl` only (reading, posting and,
//!   with the opt-in, moderating live chat all need it; `youtube.readonly` adds nothing).
//! - **Quota**: every call spends the Google project's shared 10,000 units a day. The relay
//!   counts each call's units per subscription (`YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB`, 1,500)
//!   and relay-wide (`YOUTUBE_CLOUD_DAILY_UNITS`, 9,000) per *Pacific* day (YouTube's quota
//!   resets at midnight Pacific time), after the sign-in is at hand and before the call.
//!   Live detection is `liveBroadcasts.list` (1 unit), slowing down while offline: every 2
//!   minutes for the first half hour after the takeover or the last live, every 5 until 2 h,
//!   every 15 until 6 h, then hourly; free signals wake it at once (the PC's `youtube_live`,
//!   the same subscription going live on Twitch or Kick, "Check now"). Checks have their own
//!   cap (150 units a subscription, 30% of the relay's budget). Chat is one gRPC `streamList`
//!   (5 units per opened stream, the desktop's own reader); a reply or moderation action is
//!   50, and those stop [`YoutubeConfig::read_reserve`] units early (relay-wide too) so reading and
//!   live checks always have room.
//! - **Only while it matters**: nothing is asked of YouTube unless the cloud bot is on, Pro
//!   is active and the cloud holds YouTube's lease (the PC quiet for 90 s); chat is read only
//!   while the channel is live. The PC's `bot_seen` says whether it saw the stream live, so a
//!   takeover mid-stream looks at once and one after a stream waits.
//!
//! Every Google call sits behind [`YoutubeCloud`] (a fake in tests). Tokens and the client
//! secret never appear in `Debug`, an error or a log.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use ferrocast_core::event::Platform;
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::time::Instant;

pub use ferrocast_youtube::feed::Batch;

use super::{Fut, SendErr, store};
use crate::db::Db;
use crate::quota::civil;
use crate::{ApiError, AppState, now_ms};

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
pub const GRPC_BASE: &str = "https://youtube.googleapis.com";
const TIMEOUT: Duration = Duration::from_secs(10);

/// The one scope: reading, posting and moderating live chat.
pub const FORCE_SSL: &str = "https://www.googleapis.com/auth/youtube.force-ssl";
pub const SCOPES: &[&str] = &[FORCE_SSL];
/// Moderation needs no extra scope on YouTube: the opt-in is the switch.
pub const MOD_SCOPES: &[&str] = &[FORCE_SSL];
/// `pending_oauth` keys of the YouTube step (single use).
const NONCE_PREFIX: &str = "ytbot.";

/// The official desktop app's public Google client id (the releases' FERROCAST_YOUTUBE_CLIENT_ID).
/// A relay for another build sets YOUTUBE_CLOUD_DESKTOP_CLIENT_ID.
pub const DESKTOP_YOUTUBE_CLIENT_ID: &str = "701866487620-p0npjp4t20ivffdcn89s5jhro9vik5cl.apps.googleusercontent.com";
/// Units a subscription may spend a (Pacific) day, unless set.
pub const DEFAULT_UNITS_PER_SUB: i64 = 1_500;
/// Units the whole relay may spend a day, unless set (Google gives a project 10,000).
pub const DEFAULT_UNITS: i64 = 9_000;

/// Live detection while the cloud has just taken YouTube, was just live (or the PC saw it live).
pub const CHECK_FAST: Duration = Duration::from_secs(120);
/// Live detection after the first half hour offline (and after a failed look).
pub const CHECK_SLOW: Duration = Duration::from_secs(300);
/// Live detection after 2 hours offline.
pub const CHECK_SLOWER: Duration = Duration::from_secs(15 * 60);
/// Live detection after 6 hours without a stream.
pub const CHECK_IDLE: Duration = Duration::from_secs(60 * 60);
/// How long the fast cadence lasts after a takeover or the last live.
pub const FAST_FOR: Duration = Duration::from_secs(30 * 60);
/// Then every 5 minutes until this long offline.
pub const SLOW_FOR: Duration = Duration::from_secs(2 * 60 * 60);
/// Then every 15 until this long; hourly after.
pub const IDLE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// A wake ("Check now", a free signal) never looks sooner than this after the last look.
pub const WAKE_GAP: Duration = Duration::from_secs(30);
/// Live checks a subscription may spend a day (at most a tenth of a smaller budget).
pub const DETECT_UNITS_PER_SUB: i64 = 150;
/// The share of the relay's daily budget live checks may use, in percent.
pub const DETECT_SHARE_PCT: i64 = 30;
/// Units of a subscription's day that replies, timers and moderation leave for reading and
/// live checks (at most a fifth of a smaller budget).
pub const READ_RESERVE: i64 = 300;
/// The relay-wide reserve for reading, in percent of the relay's budget.
pub const RELAY_RESERVE_PCT: i64 = 10;
/// At most one timer post on YouTube this often (`YOUTUBE_CLOUD_TIMER_GAP_MIN`, 0 = off).
pub const TIMER_GAP: Duration = Duration::from_secs(15 * 60);
/// A chat stream that says nothing this long is opened again from the page token.
pub const FEED_IDLE: Duration = Duration::from_secs(90);
/// Failed opens wait 30 s doubling up to this long.
pub const FAIL_WAIT_MAX: Duration = Duration::from_secs(60 * 60);
/// After this many failed opens in a row reading stops (`youtube_unavailable`) until "Try
/// again", or a live check at least [`GIVE_UP_FOR`] later.
pub const GIVE_UP_AFTER: u32 = 6;
pub const GIVE_UP_FOR: Duration = Duration::from_secs(60 * 60);
/// `bot_kv` key of Google's own "quota used up" stop (its reset, ms), kept across restarts.
pub(crate) const GOOGLE_OUT_KEY: &str = "yt_google_out";
/// A `bot_seen` hint about YouTube counts for this long.
pub const HINT_FRESH: Duration = Duration::from_secs(10 * 60);
/// A closed chat stream is opened again after at least this long (each open is 5 units);
/// streams that keep closing quickly wait twice as long each time, up to [`REOPEN_MAX`].
pub const REOPEN_MIN: Duration = Duration::from_secs(30);
pub const REOPEN_MAX: Duration = Duration::from_secs(300);
/// A stream that stayed open this long was a good one: the wait starts over.
const GOOD_STREAM: Duration = Duration::from_secs(300);
/// After YouTube (or the budget) said no for the day, look again this often at most.
const QUOTA_RECHECK: Duration = Duration::from_secs(15 * 60);

/// What a Google call costs in quota units, and its name in the per-call counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `liveBroadcasts.list`.
    Detect,
    /// `liveChatMessages.streamList` (one opened stream).
    Read,
    /// `liveChatMessages.insert`.
    Send,
    /// `liveChatMessages.delete` / `liveChatBans.insert`.
    Moderate,
    /// `channels.list?mine=true` with the cloud's own sign-in.
    Channel,
}

impl Call {
    pub fn units(self) -> i64 {
        match self {
            Call::Detect | Call::Channel => 1,
            Call::Read => 5,
            Call::Send | Call::Moderate => 50,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Call::Detect => "detect",
            Call::Read => "read",
            Call::Send => "send",
            Call::Moderate => "moderate",
            Call::Channel => "channel",
        }
    }

    pub const ALL: [Call; 5] = [Call::Detect, Call::Read, Call::Send, Call::Moderate, Call::Channel];
}

/// The YouTube "Ferrocast Cloud" client and the budget. `Debug` never shows the secret.
#[derive(Clone)]
pub struct YoutubeConfig {
    pub client_id: String,
    pub client_secret: String,
    /// The desktop app's public client id: the token a PC sends with `connect` must be one
    /// of its (`None`: any app's token for the channel proves it, logged at start).
    pub desktop_client_id: Option<String>,
    pub units_per_sub: i64,
    pub units: i64,
    /// A subscription's units replies, timers and moderation leave for reading and checks.
    pub read_reserve: i64,
    /// The relay's units replies, timers and moderation leave for reading and checks.
    pub relay_reserve: i64,
    /// Live-check units a subscription may spend a day.
    pub detect_per_sub: i64,
    /// Live-check units the relay may spend a day.
    pub detect_units: i64,
    /// At most one timer post this often on YouTube (zero: only the timers' own intervals).
    pub timer_gap: Duration,
}

impl std::fmt::Debug for YoutubeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YoutubeConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("desktop_client_id", &self.desktop_client_id)
            .field("units_per_sub", &self.units_per_sub)
            .field("units", &self.units)
            .field("read_reserve", &self.read_reserve)
            .field("relay_reserve", &self.relay_reserve)
            .field("detect_per_sub", &self.detect_per_sub)
            .field("detect_units", &self.detect_units)
            .field("timer_gap", &self.timer_gap)
            .finish()
    }
}

impl YoutubeConfig {
    /// `Ok(None)` when neither id nor secret is set; an error names what's missing or wrong.
    pub fn from_env() -> Result<Option<YoutubeConfig>, String> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let desktop = env("YOUTUBE_CLOUD_DESKTOP_CLIENT_ID")
            .or_else(|| option_env!("FERROCAST_YOUTUBE_CLIENT_ID").map(String::from).filter(|v| !v.is_empty()))
            .or_else(|| Some(DESKTOP_YOUTUBE_CLIENT_ID.to_string()));
        let Some(cfg) = Self::from_values(env("YOUTUBE_CLOUD_CLIENT_ID"), env("YOUTUBE_CLOUD_CLIENT_SECRET"))? else {
            return Ok(None);
        };
        let num = |k: &str, default: i64| -> Result<i64, String> {
            match env(k) {
                None => Ok(default),
                Some(v) => v.parse::<i64>().map_err(|_| format!("{k} must be a whole number")),
            }
        };
        let per_sub = num("YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB", DEFAULT_UNITS_PER_SUB)?;
        let total = num("YOUTUBE_CLOUD_DAILY_UNITS", DEFAULT_UNITS)?;
        let gap = num("YOUTUBE_CLOUD_TIMER_GAP_MIN", (TIMER_GAP.as_secs() / 60) as i64)?;
        cfg.with_desktop_client_id(desktop).with_units(per_sub, total)?.with_timer_gap_min(gap).map(Some)
    }

    pub fn from_values(
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> Result<Option<YoutubeConfig>, String> {
        match (client_id, client_secret) {
            (None, None) => Ok(None),
            (Some(client_id), Some(client_secret)) => {
                if !client_id.ends_with(".apps.googleusercontent.com") {
                    return Err(
                        "YOUTUBE_CLOUD_CLIENT_ID must be a Google OAuth client id (…apps.googleusercontent.com)".into(),
                    );
                }
                Ok(Some(
                    YoutubeConfig {
                        client_id,
                        client_secret,
                        desktop_client_id: None,
                        units_per_sub: 0,
                        units: 0,
                        read_reserve: 0,
                        relay_reserve: 0,
                        detect_per_sub: 0,
                        detect_units: 0,
                        timer_gap: TIMER_GAP,
                    }
                    .with_units(DEFAULT_UNITS_PER_SUB, DEFAULT_UNITS)?,
                ))
            }
            _ => Err("set both YOUTUBE_CLOUD_CLIENT_ID and YOUTUBE_CLOUD_CLIENT_SECRET".into()),
        }
    }

    pub fn with_desktop_client_id(mut self, id: Option<String>) -> YoutubeConfig {
        self.desktop_client_id = id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self
    }

    /// The daily budgets: a subscription's at least one reply (50) and at most the relay's;
    /// the relay's at most 1,000,000 (an audited project's quota).
    pub fn with_units(mut self, per_sub: i64, total: i64) -> Result<YoutubeConfig, String> {
        if !(50..=1_000_000).contains(&total) {
            return Err("YOUTUBE_CLOUD_DAILY_UNITS must be from 50 to 1,000,000".into());
        }
        if !(50..=total).contains(&per_sub) {
            return Err("YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB must be from 50 to YOUTUBE_CLOUD_DAILY_UNITS".into());
        }
        self.units_per_sub = per_sub;
        self.units = total;
        // The reserves and the live-check caps follow the budgets.
        self.read_reserve = READ_RESERVE.min(per_sub / 5);
        self.relay_reserve = total * RELAY_RESERVE_PCT / 100;
        self.detect_per_sub = DETECT_UNITS_PER_SUB.min(per_sub / 10).max(1);
        self.detect_units = (total * DETECT_SHARE_PCT / 100).max(1);
        Ok(self)
    }

    /// The reserves replies, timers and moderation leave (per subscription, relay-wide).
    pub fn with_reserves(mut self, per_sub: i64, relay: i64) -> YoutubeConfig {
        self.read_reserve = per_sub.clamp(0, self.units_per_sub);
        self.relay_reserve = relay.clamp(0, self.units);
        self
    }

    /// `YOUTUBE_CLOUD_TIMER_GAP_MIN`: at most one timer post on YouTube every this many
    /// minutes (0 to 1,440; 0 leaves only the timers' own intervals).
    pub fn with_timer_gap_min(mut self, minutes: i64) -> Result<YoutubeConfig, String> {
        let m = u64::try_from(minutes)
            .ok()
            .filter(|m| *m <= 1_440)
            .ok_or_else(|| "YOUTUBE_CLOUD_TIMER_GAP_MIN must be from 0 to 1,440".to_string())?;
        self.timer_gap = Duration::from_secs(m * 60);
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum YoutubeError {
    #[error("Google refused the sign-in")]
    Unauthorized,
    #[error("YouTube refused: {0}")]
    Forbidden(String),
    #[error("YouTube's daily limit is used up")]
    Quota,
    #[error("YouTube asked to slow down for {retry_after_s} s")]
    RateLimited { retry_after_s: u64 },
    #[error("not found")]
    NotFound,
    #[error("the live chat ended")]
    ChatEnded,
    #[error("YouTube said the request was wrong: {0}")]
    BadRequest(String),
    #[error("YouTube is unreachable: {0}")]
    Unavailable(String),
}

/// A Google sign-in, from a code or a refresh.
#[derive(Clone, PartialEq, Eq)]
pub struct GoogleTokens {
    pub access_token: String,
    /// Empty on a refresh that kept the same refresh token.
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub expires_in: i64,
}

impl std::fmt::Debug for GoogleTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleTokens")
            .field("scopes", &self.scopes)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// What `tokeninfo` says about an access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleTokenInfo {
    /// The client the token was issued to (`azp`, else `aud`).
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// A YouTube channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    pub title: String,
}

/// The channel's active broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broadcast {
    pub live_chat_id: String,
    /// When it started (ms), if YouTube says.
    pub started_at: Option<i64>,
}

/// An open chat stream.
pub trait ChatFeed: Send + std::fmt::Debug {
    /// The next batch, `Ok(None)` when YouTube closed the stream (open again from the page token).
    fn next(&mut self) -> Fut<'_, Option<Batch>, YoutubeError>;
}

/// Google's sign-in and YouTube's live chat for the cloud bot.
pub trait YoutubeCloud: Send + Sync + std::fmt::Debug {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, GoogleTokens, YoutubeError>;
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, GoogleTokens, YoutubeError>;
    /// Any app's access token (the cloud's own, or the desktop app's the PC sends). No quota.
    fn token_info<'a>(&'a self, access_token: &'a str) -> Fut<'a, GoogleTokenInfo, YoutubeError>;
    /// `channels.list?mine=true` (1 unit, billed to the token's project).
    fn my_channel<'a>(&'a self, access_token: &'a str) -> Fut<'a, ChannelInfo, YoutubeError>;
    /// Revokes a refresh or access token (the whole grant).
    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), YoutubeError>;
    /// `liveBroadcasts.list?broadcastStatus=active` (1 unit).
    fn active_broadcast<'a>(&'a self, access_token: &'a str) -> Fut<'a, Option<Broadcast>, YoutubeError>;
    /// Opens `streamList` (5 units).
    fn open_chat<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        page_token: Option<String>,
    ) -> Fut<'a, Box<dyn ChatFeed>, YoutubeError>;
    /// `liveChatMessages.insert` (50 units); the new message's id.
    fn send<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        text: &'a str,
    ) -> Fut<'a, Option<String>, YoutubeError>;
    /// `liveChatMessages.delete` (50 units).
    fn delete_message<'a>(&'a self, access_token: &'a str, message_id: &'a str) -> Fut<'a, (), YoutubeError>;
    /// `liveChatBans.insert` (50 units): a ban, or a timeout of `seconds`.
    fn ban<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        channel_id: &'a str,
        seconds: Option<u32>,
    ) -> Fut<'a, (), YoutubeError>;
}

// ── The production client ────────────────────────────────────────────────────────────

/// Google's REST (and the gRPC chat stream). `Debug` never shows the secret.
pub struct HttpYoutube {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token_url: String,
    tokeninfo_url: String,
    revoke_url: String,
    api: String,
    grpc: String,
}

impl std::fmt::Debug for HttpYoutube {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpYoutube").field("client_id", &self.client_id).field("client_secret", &"<redacted>").finish()
    }
}

#[derive(Debug)]
struct GrpcFeed(ferrocast_youtube::feed::Feed);

impl ChatFeed for GrpcFeed {
    fn next(&mut self) -> Fut<'_, Option<Batch>, YoutubeError> {
        Box::pin(async move { self.0.next().await.map_err(from_feed) })
    }
}

fn from_feed(e: ferrocast_youtube::feed::FeedError) -> YoutubeError {
    use ferrocast_youtube::feed::FeedError as F;
    match e {
        F::SignInNeeded => YoutubeError::Unauthorized,
        F::Quota => YoutubeError::Quota,
        F::ChatEnded => YoutubeError::ChatEnded,
        F::Refused(m) => YoutubeError::Forbidden(m),
        F::Lost(m) => YoutubeError::Unavailable(m),
    }
}

impl HttpYoutube {
    pub fn new(cfg: &YoutubeConfig) -> HttpYoutube {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("FerrocastCloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        HttpYoutube {
            http,
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            token_url: TOKEN_URL.into(),
            tokeninfo_url: TOKENINFO_URL.into(),
            revoke_url: REVOKE_URL.into(),
            api: API_BASE.into(),
            grpc: GRPC_BASE.into(),
        }
    }

    async fn check(res: Result<reqwest::Response, reqwest::Error>) -> Result<Value, YoutubeError> {
        let res = res.map_err(|e| YoutubeError::Unavailable(e.without_url().to_string()))?;
        let status = res.status().as_u16();
        if (200..300).contains(&status) {
            if status == 204 {
                return Ok(Value::Null);
            }
            return Ok(res.json().await.unwrap_or(Value::Null));
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        Err(classify(status, &body))
    }

    fn tokens(v: &Value) -> Result<GoogleTokens, YoutubeError> {
        Ok(GoogleTokens {
            access_token: v["access_token"].as_str().ok_or(YoutubeError::Unauthorized)?.to_string(),
            refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
            scopes: v["scope"].as_str().unwrap_or("").split_whitespace().map(String::from).collect(),
            expires_in: v["expires_in"].as_i64().unwrap_or(3_600),
        })
    }

    fn get(&self, path: &str, access: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.api)).bearer_auth(access)
    }
}

/// A Google error answer → [`YoutubeError`] (quota, auth, an ended chat, the rest).
pub(crate) fn classify(status: u16, body: &Value) -> YoutubeError {
    let reason = body.pointer("/error/errors/0/reason").and_then(Value::as_str).unwrap_or("");
    let oauth = body["error"].as_str().unwrap_or("");
    let msg: String = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body["error_description"].as_str())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    match (status, reason) {
        (401, _) => YoutubeError::Unauthorized,
        (400, _) if oauth == "invalid_grant" || oauth == "invalid_token" => YoutubeError::Unauthorized,
        (403, "quotaExceeded" | "dailyLimitExceeded") => YoutubeError::Quota,
        (403 | 429, "rateLimitExceeded" | "userRateLimitExceeded") | (429, _) => {
            YoutubeError::RateLimited { retry_after_s: 30 }
        }
        (_, "liveChatEnded" | "liveChatNotFound" | "liveChatDisabled") => YoutubeError::ChatEnded,
        (403, _) => YoutubeError::Forbidden(msg),
        (404, _) => YoutubeError::NotFound,
        (400, _) => YoutubeError::BadRequest(msg),
        _ => YoutubeError::Unavailable(format!("status {status}")),
    }
}

impl YoutubeCloud for HttpYoutube {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, GoogleTokens, YoutubeError> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ];
            let v = Self::check(self.http.post(&self.token_url).form(&form).send().await).await?;
            Self::tokens(&v)
        })
    }

    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, GoogleTokens, YoutubeError> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ];
            match Self::check(self.http.post(&self.token_url).form(&form).send().await).await {
                // A refresh token Google no longer takes: the sign-in is gone.
                Err(YoutubeError::BadRequest(_) | YoutubeError::Forbidden(_)) => Err(YoutubeError::Unauthorized),
                other => Self::tokens(&other?),
            }
        })
    }

    fn token_info<'a>(&'a self, access_token: &'a str) -> Fut<'a, GoogleTokenInfo, YoutubeError> {
        Box::pin(async move {
            // POSTed, so the token never sits in a URL.
            let res = self.http.post(&self.tokeninfo_url).form(&[("access_token", access_token)]).send().await;
            let v = match Self::check(res).await {
                Err(YoutubeError::BadRequest(_)) => return Err(YoutubeError::Unauthorized),
                other => other?,
            };
            let client = v["azp"].as_str().filter(|s| !s.is_empty()).or_else(|| v["aud"].as_str()).unwrap_or("");
            Ok(GoogleTokenInfo {
                client_id: client.to_string(),
                scopes: v["scope"].as_str().unwrap_or("").split_whitespace().map(String::from).collect(),
            })
        })
    }

    fn my_channel<'a>(&'a self, access_token: &'a str) -> Fut<'a, ChannelInfo, YoutubeError> {
        Box::pin(async move {
            let res = self.get("/channels", access_token).query(&[("part", "snippet"), ("mine", "true")]).send().await;
            let v = Self::check(res).await?;
            let item = v.pointer("/items/0").ok_or(YoutubeError::NotFound)?;
            Ok(ChannelInfo {
                id: item["id"].as_str().ok_or(YoutubeError::NotFound)?.to_string(),
                title: item.pointer("/snippet/title").and_then(Value::as_str).unwrap_or("YouTube").to_string(),
            })
        })
    }

    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            Self::check(self.http.post(&self.revoke_url).form(&[("token", token)]).send().await).await?;
            Ok(())
        })
    }

    fn active_broadcast<'a>(&'a self, access_token: &'a str) -> Fut<'a, Option<Broadcast>, YoutubeError> {
        Box::pin(async move {
            let q = [("part", "snippet"), ("broadcastStatus", "active"), ("broadcastType", "all"), ("maxResults", "5")];
            let v = Self::check(self.get("/liveBroadcasts", access_token).query(&q).send().await).await?;
            Ok(v["items"].as_array().into_iter().flatten().find_map(|i| {
                let chat = i.pointer("/snippet/liveChatId").and_then(Value::as_str).filter(|s| !s.is_empty())?;
                let started =
                    i.pointer("/snippet/actualStartTime").and_then(Value::as_str).and_then(super::twitch::parse_time);
                Some(Broadcast { live_chat_id: chat.to_string(), started_at: started })
            }))
        })
    }

    fn open_chat<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        page_token: Option<String>,
    ) -> Fut<'a, Box<dyn ChatFeed>, YoutubeError> {
        Box::pin(async move {
            let feed = ferrocast_youtube::feed::open(&self.grpc, access_token, live_chat_id, page_token)
                .await
                .map_err(from_feed)?;
            Ok(Box::new(GrpcFeed(feed)) as Box<dyn ChatFeed>)
        })
    }

    fn send<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        text: &'a str,
    ) -> Fut<'a, Option<String>, YoutubeError> {
        Box::pin(async move {
            let body = json!({ "snippet": { "liveChatId": live_chat_id, "type": "textMessageEvent",
                                            "textMessageDetails": { "messageText": text } } });
            let res = self
                .http
                .post(format!("{}/liveChat/messages", self.api))
                .bearer_auth(access_token)
                .query(&[("part", "snippet")])
                .json(&body)
                .send()
                .await;
            let v = Self::check(res).await?;
            Ok(v["id"].as_str().filter(|s| !s.is_empty()).map(String::from))
        })
    }

    fn delete_message<'a>(&'a self, access_token: &'a str, message_id: &'a str) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            let res = self
                .http
                .delete(format!("{}/liveChat/messages", self.api))
                .bearer_auth(access_token)
                .query(&[("id", message_id)])
                .send()
                .await;
            Self::check(res).await?;
            Ok(())
        })
    }

    fn ban<'a>(
        &'a self,
        access_token: &'a str,
        live_chat_id: &'a str,
        channel_id: &'a str,
        seconds: Option<u32>,
    ) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            let mut snippet = json!({ "liveChatId": live_chat_id, "type": "permanent",
                                      "bannedUserDetails": { "channelId": channel_id } });
            if let Some(s) = seconds {
                snippet["type"] = json!("temporary");
                snippet["banDurationSeconds"] = json!(s.clamp(1, super::moderate::MAX_TIMEOUT_S));
            }
            let res = self
                .http
                .post(format!("{}/liveChat/bans", self.api))
                .bearer_auth(access_token)
                .query(&[("part", "snippet")])
                .json(&json!({ "snippet": snippet }))
                .send()
                .await;
            Self::check(res).await?;
            Ok(())
        })
    }
}

// ── The Pacific day and the unit budget ──────────────────────────────────────────────

const HOUR_MS: i64 = 3_600_000;

/// The day number of a date (days since 1970-01-01).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `n`th Sunday (1-based) of a month, as a day number.
fn nth_sunday(y: i64, m: i64, n: i64) -> i64 {
    let first = days_from_civil(y, m, 1);
    // 1970-01-01 was a Thursday: (days + 4) % 7 is 0 on Sundays.
    let to_sunday = (7 - (first + 4).rem_euclid(7)).rem_euclid(7);
    first + to_sunday + 7 * (n - 1)
}

/// US Pacific time's offset from UTC at `utc` ms: −7 h from the second Sunday of March
/// (2:00 local = 10:00 UTC) to the first Sunday of November (2:00 local = 9:00 UTC), else −8 h.
pub fn pacific_offset_ms(utc: i64) -> i64 {
    let (y, _, _) = civil((utc - 8 * HOUR_MS).div_euclid(super::DAY_MS));
    let start = nth_sunday(y, 3, 2) * super::DAY_MS + 10 * HOUR_MS;
    let end = nth_sunday(y, 11, 1) * super::DAY_MS + 9 * HOUR_MS;
    if (start..end).contains(&utc) { -7 * HOUR_MS } else { -8 * HOUR_MS }
}

/// The Pacific day `now` falls in (`p2026-10-19`) and when it ends (the next midnight
/// Pacific, in UTC ms): YouTube's quota day.
pub fn pacific_day(now: i64) -> (String, i64) {
    let local = now + pacific_offset_ms(now);
    let day = local.div_euclid(super::DAY_MS);
    let (y, m, d) = civil(day);
    let next_local = (day + 1) * super::DAY_MS;
    let guess = next_local - pacific_offset_ms(now);
    (format!("p{y:04}-{m:02}-{d:02}"), next_local - pacific_offset_ms(guess))
}

/// `HH:MM UTC` of a time, for messages (the dashboard shows it in the viewer's own time).
pub fn utc_hm(ms: i64) -> String {
    let mins = ms.rem_euclid(super::DAY_MS) / 60_000;
    format!("{:02}:{:02} UTC", mins / 60, mins % 60)
}

/// The relay-wide counter's owner in `quota_usage`.
pub(crate) const RELAY_SUB: &str = "*relay";
/// The total counter's name; per-call counters are `yt_units_<call>`.
pub(crate) const UNITS: &str = "yt_units";

/// A budget that's spent: whose (the subscription's or the relay's), when it's back, and
/// whether it's only the live checks' own cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spent {
    pub relay_wide: bool,
    pub reset_at: i64,
    pub checks: bool,
}

#[derive(Debug)]
pub(crate) enum UnitsErr {
    Spent(Spent),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for UnitsErr {
    fn from(e: rusqlite::Error) -> Self {
        UnitsErr::Db(e)
    }
}

/// How much room a call has today: the subscription's and the relay's limit, and the call
/// type's own caps (live checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Room {
    pub sub: i64,
    pub relay: i64,
    pub call_sub: Option<i64>,
    pub call_relay: Option<i64>,
}

impl Room {
    /// Just the two budgets.
    pub(crate) fn plain(sub: i64, relay: i64) -> Room {
        Room { sub, relay, call_sub: None, call_relay: None }
    }

    /// A call's room under `cfg`: replies, timers and moderation stop at the reserves; live
    /// checks have their own caps; reading may use everything.
    pub(crate) fn of(cfg: &YoutubeConfig, call: Call) -> Room {
        match call {
            Call::Send | Call::Moderate => {
                Room::plain(cfg.units_per_sub - cfg.read_reserve, cfg.units - cfg.relay_reserve)
            }
            Call::Detect => Room {
                call_sub: Some(cfg.detect_per_sub),
                call_relay: Some(cfg.detect_units),
                ..Room::plain(cfg.units_per_sub, cfg.units)
            },
            Call::Read | Call::Channel => Room::plain(cfg.units_per_sub, cfg.units),
        }
    }
}

impl Db {
    /// Counts a call's units against the subscription's and the relay's budget for today
    /// (Pacific) if there's room (`Room`); nothing is counted otherwise. `room: None` counts
    /// without a limit (a call already made). One transaction. Rows: the totals and the
    /// call type's, for the subscription and for [`RELAY_SUB`].
    pub(crate) fn yt_take_units(&self, sub: &str, call: Call, room: Option<Room>, now: i64) -> Result<(), UnitsErr> {
        let (period, reset_at) = pacific_day(now);
        let amount = call.units();
        let kind = format!("{UNITS}_{}", call.name());
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(room) = room {
            let used = |owner: &str, name: &str| -> rusqlite::Result<i64> {
                Ok(tx
                    .query_row(
                        "SELECT used FROM quota_usage WHERE sub_id = ?1 AND name = ?2 AND period = ?3",
                        params![owner, name, period],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(0))
            };
            let spent = |relay_wide, checks| UnitsErr::Spent(Spent { relay_wide, reset_at, checks });
            if used(RELAY_SUB, UNITS)? + amount > room.relay {
                return Err(spent(true, false));
            }
            if used(sub, UNITS)? + amount > room.sub {
                return Err(spent(false, false));
            }
            if let Some(cap) = room.call_relay
                && used(RELAY_SUB, &kind)? + amount > cap
            {
                return Err(spent(true, true));
            }
            if let Some(cap) = room.call_sub
                && used(sub, &kind)? + amount > cap
            {
                return Err(spent(false, true));
            }
        }
        for (owner, name) in [(RELAY_SUB, UNITS), (RELAY_SUB, kind.as_str()), (sub, UNITS), (sub, kind.as_str())] {
            tx.execute(
                "INSERT INTO quota_usage(sub_id, name, period, used, reset_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sub_id, name, period) DO UPDATE SET used = used + excluded.used",
                params![owner, name, period, amount, reset_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Gives back `amount` of a call's units counted today (a call that cost less, e.g. one
    /// Google refused with 401 before doing anything: it keeps one unit).
    pub(crate) fn yt_refund_units(&self, sub: &str, call: Call, amount: i64, now: i64) -> rusqlite::Result<()> {
        let (period, _) = pacific_day(now);
        let kind = format!("{UNITS}_{}", call.name());
        let mut c = self.c();
        let tx = c.transaction()?;
        for (owner, name) in [(RELAY_SUB, UNITS), (RELAY_SUB, kind.as_str()), (sub, UNITS), (sub, kind.as_str())] {
            tx.execute(
                "UPDATE quota_usage SET used = MAX(0, used - ?4) WHERE sub_id = ?1 AND name = ?2 AND period = ?3",
                params![owner, name, period, amount],
            )?;
        }
        tx.commit()
    }

    /// Units a subscription (or [`RELAY_SUB`]) spent today (Pacific), in all or for one call.
    pub(crate) fn yt_units_used(&self, sub: &str, call: Option<Call>, now: i64) -> rusqlite::Result<i64> {
        let (period, _) = pacific_day(now);
        let name = match call {
            Some(c) => format!("{UNITS}_{}", c.name()),
            None => UNITS.to_string(),
        };
        Ok(self
            .c()
            .query_row(
                "SELECT used FROM quota_usage WHERE sub_id = ?1 AND name = ?2 AND period = ?3",
                params![sub, name, period],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }
}

/// Takes a call's units for `sub` now: `Err(Some(spent))` over budget (or YouTube said so
/// for today), `Err(None)` when the count couldn't be kept (no call then either). Replies,
/// timers and moderation stop at the reserves; live checks at their caps.
pub(crate) fn take(st: &AppState, sub: &str, call: Call) -> Result<(), Option<Spent>> {
    let now = now_ms();
    let out = st.bot.yt_google_out.load(Ordering::Relaxed);
    if now < out {
        return Err(Some(Spent { relay_wide: true, reset_at: out, checks: false }));
    }
    let Some(cfg) = st.bot.youtube_cfg() else { return Err(None) };
    match st.db.yt_take_units(sub, call, Some(Room::of(cfg, call)), now) {
        Ok(()) => Ok(()),
        Err(UnitsErr::Spent(s)) => Err(Some(s)),
        Err(UnitsErr::Db(e)) => {
            tracing::warn!(error = %e, "couldn't count YouTube units");
            Err(None)
        }
    }
}

/// A call Google refused with 401 before doing anything: it keeps one unit of what was
/// counted (the retry is counted again).
pub(crate) fn refund_401(st: &AppState, sub: &str, call: Call) {
    if let Err(e) = st.db.yt_refund_units(sub, call, call.units() - 1, now_ms()) {
        tracing::debug!(error = %e, "couldn't give back YouTube units");
    }
}

/// Google said the project's quota is used up: no YouTube call until midnight Pacific, kept
/// in `bot_kv` so a restart doesn't ask again.
fn google_out(st: &AppState) {
    let (_, reset) = pacific_day(now_ms());
    if st.bot.yt_google_out.swap(reset, Ordering::Relaxed) != reset {
        tracing::warn!("YouTube says the cloud bot's Google project quota is used up until midnight Pacific");
        if let Err(e) = st.db.bot_put_kv(GOOGLE_OUT_KEY, &reset.to_string()) {
            tracing::warn!(error = %e, "couldn't keep YouTube's quota stop");
        }
    }
}

/// At start: Google's stop from before a restart, if it still holds.
pub(crate) fn load_google_out(st: &AppState) {
    let kept = st.db.bot_kv(GOOGLE_OUT_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok());
    if let Some(reset) = kept.filter(|r| now_ms() < *r) {
        st.bot.yt_google_out.fetch_max(reset, Ordering::Relaxed);
    }
}

/// The words for a spent budget (kept on the channel, shown with the time in the dashboard).
pub(crate) fn spent_error(s: Spent) -> Value {
    let mut e = if s.checks {
        super::error_json(
            "youtube_quota",
            &format!(
                "The cloud bot has used today's YouTube live checks, so it won't notice you going live until {}.",
                utc_hm(s.reset_at)
            ),
            "The bot on your PC isn't affected. Turn your PC on to have chat answered before then.",
        )
    } else {
        super::error_json(
            "youtube_quota",
            &format!("YouTube's daily limit for the cloud bot is used up; it's back at {}.", utc_hm(s.reset_at)),
            "The bot on your PC isn't affected. The cloud bot answers again after that.",
        )
    };
    e["reset_at"] = json!(s.reset_at);
    e["relay_wide"] = json!(s.relay_wide);
    e
}

/// A `youtube_quota` error whose reset has passed (cleared by the next pass).
pub(crate) fn quota_error_stale(ch: &store::Channel, now: i64) -> bool {
    ch.error_code() == Some("youtube_quota")
        && ch.error.as_ref().and_then(|e| e["reset_at"].as_i64()).is_none_or(|r| r <= now)
}

/// `quota` in a YouTube channel's view: units used today, the limit, when it resets.
pub(crate) fn quota_view(st: &AppState, sub: &str) -> Value {
    let now = now_ms();
    let (_, reset_at) = pacific_day(now);
    let Some(cfg) = st.bot.youtube_cfg() else { return Value::Null };
    let used = st.db.yt_units_used(sub, None, now).unwrap_or(0);
    let relay = st.db.yt_units_used(RELAY_SUB, None, now).unwrap_or(0);
    let google = now < st.bot.yt_google_out.load(Ordering::Relaxed);
    let mut by_call = serde_json::Map::new();
    for c in Call::ALL {
        by_call.insert(c.name().into(), json!(st.db.yt_units_used(sub, Some(c), now).unwrap_or(0)));
    }
    let room = Room::of(cfg, Call::Send);
    json!({
        "used": used, "limit": cfg.units_per_sub, "reset_at": reset_at, "by_call": by_call,
        // Room for one more reply (above the reserve kept for reading)?
        "exhausted": used + Call::Send.units() > room.sub || relay + Call::Send.units() > room.relay || google,
    })
}

// ── What the relay remembers while it runs ───────────────────────────────────────────

/// Per subscription, in memory only: the access token, the live chat, the reader, the
/// cadence of live checks, the PC's last hint, the chat stream's failure streak and the
/// last timer post.
#[derive(Debug, Default)]
pub(crate) struct YtMem {
    access: Option<(String, i64)>,
    /// The active broadcast's chat (while live).
    pub(crate) chat: Option<String>,
    page_token: Option<String>,
    reader: Option<(u64, tokio::task::AbortHandle)>,
    next_check: Option<Instant>,
    /// The last live check.
    last_check: Option<Instant>,
    /// When the cloud last took YouTube (the fast cadence's start).
    duty_from: Option<Instant>,
    /// When the channel was last seen live (the fast cadence starts again from there).
    last_live: Option<Instant>,
    /// The PC's last word on whether YouTube was live, and when.
    hint: Option<(bool, Instant)>,
    /// Failed stream opens in a row (kept across reader restarts).
    pub(crate) fail_streak: u32,
    /// Reading stopped after [`GIVE_UP_AFTER`] failures, then (until "Try again" or a live
    /// check [`GIVE_UP_FOR`] later).
    gave_up: Option<Instant>,
    /// The last timer posted on YouTube (at most one per [`YoutubeConfig::timer_gap`]).
    last_timer: Option<Instant>,
}

impl YtMem {
    /// "Try again" / "Check now": look for a live broadcast on the next pass (at most once
    /// per [`WAKE_GAP`]), and read again after a give-up.
    pub(crate) fn retry_now(&mut self) {
        self.fail_streak = 0;
        self.gave_up = None;
        self.wake();
    }

    /// A free signal says the channel may be live: look on the next pass (at most once per
    /// [`WAKE_GAP`]), unless a reader is already in the chat.
    pub(crate) fn wake(&mut self) {
        if self.reading() {
            return;
        }
        let now = Instant::now();
        let soonest = self.last_check.map_or(now, |t| (t + WAKE_GAP).max(now));
        self.next_check = Some(self.next_check.map_or(soonest, |t| t.min(soonest)));
    }

    fn reading(&self) -> bool {
        self.reader.as_ref().is_some_and(|(_, h)| !h.is_finished())
    }

    /// Forgets the cached access token if it's still `token` (YouTube refused it).
    pub(crate) fn forget_access(&mut self, token: &str) {
        if self.access.as_ref().is_some_and(|(t, _)| t == token) {
            self.access = None;
        }
    }

    /// Forgets the cached access token, whatever it is (tests: the next call refreshes).
    #[cfg(test)]
    pub(crate) fn forget_access_all(&mut self) {
        self.access = None;
    }

    pub(crate) fn stop(&mut self) {
        if let Some((_, h)) = self.reader.take() {
            h.abort();
        }
    }
}

impl Drop for YtMem {
    fn drop(&mut self) {
        self.stop();
    }
}

impl super::Runtime {
    pub(crate) fn yt<T>(&self, sub: &str, f: impl FnOnce(&mut YtMem) -> T) -> T {
        let mut m = self.yt.lock().unwrap_or_else(|p| p.into_inner());
        f(m.entry(sub.to_string()).or_default())
    }

    /// Like [`Self::yt`], only for a subscription the cloud already looked after on YouTube.
    pub(crate) fn yt_if(&self, sub: &str, f: impl FnOnce(&mut YtMem)) {
        if let Some(m) = self.yt.lock().unwrap_or_else(|p| p.into_inner()).get_mut(sub) {
            f(m);
        }
    }

    /// Stops the reader and forgets everything about `sub`'s YouTube (disconnect, lapse).
    pub(crate) fn forget_yt(&self, sub: &str) {
        self.yt.lock().unwrap_or_else(|p| p.into_inner()).remove(sub);
        self.yt_locks.lock().unwrap_or_else(|p| p.into_inner()).remove(sub);
    }

    fn yt_lock(&self, sub: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.yt_locks.lock().unwrap_or_else(|p| p.into_inner()).entry(sub.to_string()).or_default().clone()
    }

    /// Drops refresh locks of subscriptions without YouTube (none held right now).
    pub(crate) fn prune_yt_locks(&self, keep: &HashSet<String>) {
        self.yt_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|sub, l| keep.contains(sub) || Arc::strong_count(l) > 1);
    }

    #[cfg(test)]
    pub(crate) fn yt_lock_count(&self) -> usize {
        self.yt_locks.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// The subscriptions with a chat reader running.
    pub(crate) fn yt_reading(&self) -> Vec<String> {
        let m = self.yt.lock().unwrap_or_else(|p| p.into_inner());
        m.iter().filter(|(_, y)| y.reading()).map(|(k, _)| k.clone()).collect()
    }
}

/// The PC's `bot_seen` said whether its YouTube was live (`youtube_live`). Turning live
/// (a free signal) wakes the next live check.
pub(crate) fn hint(st: &AppState, sub: &str, live: bool) {
    st.bot.yt(sub, |m| {
        let was_live = m.hint.is_some_and(|(l, at)| l && at.elapsed() < HINT_FRESH);
        m.hint = Some((live, Instant::now()));
        if live && !was_live {
            m.wake();
        }
    });
}

/// A free signal that `sub` may be live on YouTube now (it went live on Twitch or Kick).
pub(crate) fn wake(st: &AppState, sub: &str) {
    st.bot.yt_if(sub, YtMem::wake);
}

// ── Signed state and the broker ──────────────────────────────────────────────────────

fn mac(cfg: &YoutubeConfig, payload: &str) -> String {
    // Its own key, derived from the client secret (never the secret itself).
    let key = super::derive_key(cfg.client_secret.as_bytes(), "cloud bot youtube state v1");
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut m = Hmac::<Sha256>::new_from_slice(&key).expect("hmac takes any key");
    m.update(payload.as_bytes());
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// What a checked state carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateInfo {
    pub moderation: bool,
    pub install: String,
    /// The channel the PC is signed in to (from Google's answer about its token).
    pub owner: String,
    pub nonce: String,
}

/// `<y|ym>.<install>.<expires ms>.<nonce>.<owner channel>.<hmac>`, 10 minutes, single use.
pub(crate) fn sign_state(
    cfg: &YoutubeConfig,
    moderation: bool,
    install: &str,
    owner: &str,
    nonce: &str,
    now: i64,
) -> String {
    let tag = if moderation { "ym" } else { "y" };
    let payload = format!("{tag}.{install}.{}.{nonce}.{owner}", now + super::STATE_TTL_MS);
    let m = mac(cfg, &payload);
    format!("{payload}.{m}")
}

pub(crate) fn check_state(cfg: &YoutubeConfig, raw: &str, now: i64) -> Option<StateInfo> {
    let (payload, m) = raw.rsplit_once('.')?;
    if !bool::from(mac(cfg, payload).as_bytes().ct_eq(m.as_bytes())) {
        return None;
    }
    let mut parts = payload.split('.');
    let moderation = match parts.next()? {
        "y" => false,
        "ym" => true,
        _ => return None,
    };
    let install = parts.next()?.to_string();
    let exp: i64 = parts.next()?.parse().ok()?;
    let nonce = parts.next()?.to_string();
    let owner = parts.next()?.to_string();
    if parts.next().is_some() || now > exp || install.is_empty() || owner.is_empty() || nonce.is_empty() {
        return None;
    }
    Some(StateInfo { moderation, install, owner, nonce })
}

pub(crate) fn redirect_uri(st: &AppState) -> String {
    format!("{}/v1/bot/youtube/callback", st.cfg.public_url)
}

/// Google's consent page. `prompt=consent` + `access_type=offline` so a refresh token always
/// comes back; `login_hint`-free, so the streamer picks the right Google account.
pub(crate) fn auth_url(
    st: &AppState,
    cfg: &YoutubeConfig,
    moderation: bool,
    install: &str,
    owner: &ChannelInfo,
    now: i64,
) -> Result<String, ApiError> {
    let nonce = crate::random(12);
    st.db.put_oauth(&format!("{NONCE_PREFIX}{nonce}"), install, &owner.title).map_err(ApiError::db)?;
    let mut url = url::Url::parse(AUTHORIZE_URL).map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "platform_unavailable", "Google's sign-in address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &redirect_uri(st))
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent select_account")
        .append_pair("include_granted_scopes", "false")
        .append_pair("state", &sign_state(cfg, moderation, install, &owner.id, &nonce, now));
    Ok(url.to_string())
}

/// Checks the desktop-app token a PC sent with `connect`: issued to Ferrocast's desktop app
/// (Google's `tokeninfo`) and whose channel it is (`channels?mine=true`, billed to the
/// desktop app's project). The token is never kept or logged.
pub(crate) async fn owner_of(st: &AppState, cfg: &YoutubeConfig, token: Option<&str>) -> Result<ChannelInfo, ApiError> {
    let not_linked = |m: &str| {
        ApiError::new(StatusCode::CONFLICT, "youtube_not_linked", m)
            .hint("Connect YouTube in Accounts on this PC, then try again: the cloud bot speaks in that channel.")
    };
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(not_linked("Connect YouTube in Accounts on this PC first."));
    };
    let api = st.bot.youtube()?;
    let unreachable = |e: YoutubeError| {
        tracing::debug!(error = %e, "couldn't check a PC's YouTube sign-in");
        match e {
            YoutubeError::RateLimited { retry_after_s } => {
                ApiError::new(StatusCode::TOO_MANY_REQUESTS, "youtube_rate", "YouTube asked Ferrocast to slow down.")
                    .hint("Try again in a minute.")
                    .header(axum::http::header::RETRY_AFTER, retry_after_s.to_string())
            }
            YoutubeError::Quota => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "youtube_quota",
                "YouTube's daily limit is used up, so the channel can't be checked now.",
            )
            .hint("Try again after midnight Pacific time."),
            _ => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "youtube_unreachable",
                "Couldn't reach YouTube to check this PC's channel.",
            )
            .hint("Try again in a minute."),
        }
    };
    let info = match api.token_info(token).await {
        Ok(i) => i,
        Err(YoutubeError::Unauthorized) => return Err(not_linked("YouTube on this PC needs signing in again.")),
        Err(e) => return Err(unreachable(e)),
    };
    if let Some(want) = cfg.desktop_client_id.as_deref()
        && !bool::from(want.as_bytes().ct_eq(info.client_id.as_bytes()))
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_youtube_app",
            "That YouTube sign-in isn't from Ferrocast on this PC.",
        )
        .hint("Connect YouTube in Accounts on this PC, then try again."));
    }
    match api.my_channel(token).await {
        Ok(c) => Ok(c),
        Err(YoutubeError::Unauthorized) => Err(not_linked("YouTube on this PC needs signing in again.")),
        Err(YoutubeError::NotFound) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "youtube_no_channel",
            "The Google account on this PC has no YouTube channel.",
        )
        .hint("Connect the account that owns your channel in Accounts.")),
        Err(e) => Err(unreachable(e)),
    }
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

const TRY_SHORTLY: &str = "The relay couldn't save that. Try again shortly.";

/// `GET /v1/bot/youtube/callback`.
pub(crate) async fn callback(State(st): State<AppState>, Query(q): Query<Callback>) -> Response {
    let page = crate::page;
    let Some(cfg) = st.bot.youtube_cfg() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The YouTube cloud bot isn't set up on this relay yet.");
    };
    if q.error.is_some() {
        return page(StatusCode::OK, "Google sign-in was cancelled. You can close this tab.");
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(StatusCode::BAD_REQUEST, "This link is incomplete. Go back to Ferrocast and press Connect again.");
    };
    let Some(s) = check_state(cfg, &state, now_ms()) else {
        return page(StatusCode::BAD_REQUEST, "This sign-in link expired. Press Connect again in Ferrocast.");
    };
    // One use: the nonce is taken here, so a state can never be replayed.
    let owner_title = match st.db.take_oauth(&format!("{NONCE_PREFIX}{}", s.nonce)) {
        Ok(Some((install, title))) if install == s.install => title,
        Ok(_) => {
            return page(
                StatusCode::BAD_REQUEST,
                "This sign-in link expired or was already used. Press Connect again in Ferrocast.",
            );
        }
        Err(_) => return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY),
    };
    let Ok(api) = st.bot.youtube() else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The YouTube cloud bot isn't set up on this relay yet.");
    };
    let tokens = match api.exchange_code(&code, &redirect_uri(&st)).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "Google didn't take a cloud bot sign-in code");
            return page(
                StatusCode::BAD_GATEWAY,
                "Google didn't accept the sign-in. Press Connect again in Ferrocast.",
            );
        }
    };
    // Whose channel: from YouTube's own answer, never the URL (1 unit, counted). Asked
    // first, so a refusal knows whose grant it would give back.
    let counted = st.db.pro_sub_for_install(&s.install).ok().flatten().map(|r| r.id);
    let _ = st.db.yt_take_units(counted.as_deref().unwrap_or(RELAY_SUB), Call::Channel, None, now_ms());
    let channel = api.my_channel(&tokens.access_token).await;
    // Every way out but success gives the fresh grant back to Google, unless that would
    // end a sign-in still in use: Google's revoke ends the whole grant of that Google
    // account for Ferrocast Cloud, so a channel any subscription has bound keeps it, and so
    // does a channel YouTube didn't name (it may be one of those).
    let revoke = || async {
        let bound = match &channel {
            Ok(c) => st.db.bot_channel_by_channel("youtube", &c.id).map(|b| b.is_some()).unwrap_or(true),
            Err(_) => true,
        };
        if bound {
            tracing::debug!("an unused Google sign-in is left to expire: its grant may be in use");
            return;
        }
        let t = if tokens.refresh_token.is_empty() { &tokens.access_token } else { &tokens.refresh_token };
        if let Err(e) = api.revoke(t).await {
            tracing::debug!(error = %e, "couldn't revoke an unused Google sign-in");
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
    if !SCOPES.iter().all(|x| tokens.scopes.iter().any(|g| g == x)) {
        revoke().await;
        return page(
            StatusCode::BAD_REQUEST,
            "Google didn't allow the cloud bot to manage your YouTube chat. Press Connect again and tick every box.",
        );
    }
    if tokens.refresh_token.is_empty() {
        revoke().await;
        return page(
            StatusCode::BAD_GATEWAY,
            "Google didn't give a lasting sign-in. Press Connect again in Ferrocast.",
        );
    }
    let channel = match &channel {
        Ok(c) => c.clone(),
        Err(YoutubeError::NotFound) => {
            revoke().await;
            return page(
                StatusCode::CONFLICT,
                "That Google account has no YouTube channel. Press Connect again and pick the account that owns your channel.",
            );
        }
        Err(_) => {
            revoke().await;
            return page(StatusCode::BAD_GATEWAY, "YouTube didn't say whose channel this is. Try again.");
        }
    };
    // Proof of the channel: the one the PC itself is signed in to.
    if !bool::from(channel.id.as_bytes().ct_eq(s.owner.as_bytes())) {
        revoke().await;
        return page(
            StatusCode::FORBIDDEN,
            &format!(
                "You signed in to the channel {}, but Ferrocast on this PC is connected to YouTube as {owner_title}. Press Connect again and pick the Google account of {owner_title}.",
                channel.title
            ),
        );
    }
    let Ok(vault) = st.vault() else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, "The cloud bot isn't switched on on Ferrocast's servers yet.");
    };
    let Ok(sealed) = vault.seal(&ctx.sub_id, &super::token_purpose("youtube", "bot"), tokens.refresh_token.as_bytes())
    else {
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    };
    // The owner is here: a binding another subscription kept for this channel goes. Its
    // sign-in is the same Google grant as this new one, so it isn't revoked (that would end
    // this one too); dropping the row is enough.
    match st.db.bot_channel_by_channel("youtube", &channel.id) {
        Ok(Some(other)) if other.sub_id != ctx.sub_id => {
            super::forget_platform(&st, &other.sub_id, Platform::Youtube);
            tracing::info!("a YouTube channel's owner took the cloud bot back from another subscription");
        }
        Ok(_) => {}
        Err(_) => {
            revoke().await;
            return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
        }
    }
    // A sign-in this replaces: the same channel (the moderation opt-in, connecting again) is
    // the same grant, so it's only overwritten; another channel's is revoked first.
    let replaced = st.db.bot_channel(&ctx.sub_id, "youtube").ok().flatten().and_then(|c| c.channel_id);
    if replaced.as_deref().is_some_and(|old| old != channel.id)
        && st.db.bot_token(&ctx.sub_id, "youtube", "bot").ok().flatten().is_some()
    {
        revoke_stored(&st, &ctx.sub_id).await;
    }
    let saved = st.db.bot_set_account(
        &ctx.sub_id,
        "youtube",
        (&channel.id, &channel.title),
        Some((&channel.id, &channel.title)),
        &tokens.scopes.join(" "),
        s.moderation,
        &ctx.install,
        now_ms(),
    );
    if saved.is_err() || st.db.bot_put_token(&ctx.sub_id, "youtube", "bot", &sealed).is_err() {
        tracing::warn!("couldn't save a cloud bot YouTube sign-in");
        revoke().await;
        return page(StatusCode::SERVICE_UNAVAILABLE, TRY_SHORTLY);
    }
    let until = now_ms() + tokens.expires_in.clamp(0, 86_400) * 1000;
    st.bot.yt(&ctx.sub_id, |m| m.access = Some((tokens.access_token.clone(), until)));
    let code = super::pc_code(&st, &ctx.install);
    page(
        StatusCode::OK,
        &format!(
            "The cloud bot (beta) can now answer in {}'s YouTube chat. This is for the Ferrocast PC with the code {code}; the Bot page there shows the same code. If it doesn't, remove Ferrocast Cloud at myaccount.google.com/permissions. You can close this tab.",
            channel.title
        ),
    )
}

/// Revokes the stored YouTube sign-in at Google (the whole grant of that Google account for
/// Ferrocast Cloud: only on a Disconnect, the lapse sweep, or a sign-in for another channel).
pub(crate) async fn revoke_stored(st: &AppState, sub: &str) {
    if let Ok(api) = st.bot.youtube()
        && let Some(refresh) = super::open_token(st, sub, "youtube", "bot")
        && let Err(e) = api.revoke(&refresh).await
    {
        tracing::debug!(error = %e, "couldn't revoke a Google sign-in");
    }
    st.bot.yt(sub, |m| m.access = None);
}

// ── Tokens ───────────────────────────────────────────────────────────────────────────

fn signed_out_error() -> Value {
    super::error_json(
        "youtube_signed_out",
        "Google signed the cloud bot out of YouTube.",
        "Disconnect YouTube here and connect it again.",
    )
}

/// A usable access token for `sub`'s YouTube: cached, else refreshed from the sealed sign-in
/// (one refresh at a time per subscription; a new refresh token is sealed). Google refusing
/// the refresh marks the channel signed out.
pub(crate) async fn access(st: &AppState, sub: &str) -> Result<String, SendErr> {
    let cached = |st: &AppState| {
        st.bot.yt(sub, |m| m.access.clone()).filter(|(_, until)| now_ms() < until - 60_000).map(|(t, _)| t)
    };
    if let Some(t) = cached(st) {
        return Ok(t);
    }
    let api = st.bot.youtube().map_err(|_| SendErr::NotReady)?;
    let lock = st.bot.yt_lock(sub);
    let _one = lock.lock().await;
    if let Some(t) = cached(st) {
        return Ok(t);
    }
    let refresh = super::open_token(st, sub, "youtube", "bot").ok_or(SendErr::SignedOut)?;
    match api.refresh(&refresh).await {
        Ok(t) => {
            if !t.refresh_token.is_empty() && t.refresh_token != refresh {
                super::seal_token(st, sub, "youtube", "bot", &t.refresh_token);
            }
            let until = now_ms() + t.expires_in.clamp(0, 86_400) * 1000;
            st.bot.yt(sub, |m| m.access = Some((t.access_token.clone(), until)));
            Ok(t.access_token)
        }
        Err(YoutubeError::Unauthorized) => {
            let _ = st.db.bot_set_error(sub, "youtube", Some(&signed_out_error()));
            Err(SendErr::SignedOut)
        }
        Err(e) => Err(SendErr::from(e)),
    }
}

fn forget_access_if(st: &AppState, sub: &str, token: &str) {
    st.bot.yt(sub, |m| m.forget_access(token));
}

impl From<YoutubeError> for SendErr {
    fn from(e: YoutubeError) -> Self {
        match e {
            YoutubeError::Unauthorized => SendErr::SignedOut,
            YoutubeError::RateLimited { retry_after_s } => SendErr::RateLimited(retry_after_s),
            YoutubeError::Forbidden(m) => SendErr::Refused(m),
            YoutubeError::BadRequest(m) => SendErr::Dropped(m),
            YoutubeError::Quota => {
                SendErr::Quota(Spent { relay_wide: true, reset_at: pacific_day(now_ms()).1, checks: false })
            }
            YoutubeError::ChatEnded | YoutubeError::NotFound => SendErr::NotReady,
            YoutubeError::Unavailable(m) => SendErr::Unavailable(m),
        }
    }
}

/// Part of the hourly sweep: refreshes the sign-in (no quota); one Google no longer takes
/// marks the channel signed out.
pub(crate) async fn check_signin(st: &AppState, ch: &store::Channel) {
    let Ok(api) = st.bot.youtube() else { return };
    let Some(refresh) = super::open_token(st, &ch.sub_id, "youtube", "bot") else { return };
    match api.refresh(&refresh).await {
        Ok(t) => {
            if !t.refresh_token.is_empty() && t.refresh_token != refresh {
                super::seal_token(st, &ch.sub_id, "youtube", "bot", &t.refresh_token);
            }
            let until = now_ms() + t.expires_in.clamp(0, 86_400) * 1000;
            st.bot.yt(&ch.sub_id, |m| m.access = Some((t.access_token, until)));
        }
        Err(YoutubeError::Unauthorized) => {
            let _ = st.db.bot_set_error(&ch.sub_id, "youtube", Some(&signed_out_error()));
            st.bot.yt(&ch.sub_id, YtMem::stop);
        }
        Err(e) => tracing::debug!(error = %e, "couldn't check a Google sign-in this hour"),
    }
}

// ── Sending ──────────────────────────────────────────────────────────────────────────

/// Takes a send's or moderation's units; `Err` with what to tell.
fn take_send(st: &AppState, sub: &str, call: Call) -> Result<(), SendErr> {
    match take(st, sub, call) {
        Ok(()) => Ok(()),
        Err(Some(s)) => Err(SendErr::Quota(s)),
        Err(None) => Err(SendErr::NotReady),
    }
}

/// Google's own quota answer to a call.
fn google_spent(st: &AppState) -> Spent {
    google_out(st);
    Spent { relay_wide: true, reset_at: pacific_day(now_ms()).1, checks: false }
}

/// Sends one line in the live chat (50 units, counted once the sign-in is at hand and above
/// the reserve for reading); YouTube's message id. Not live: nothing to send to. A 401 keeps
/// one unit and the retry is counted again.
pub(crate) async fn send(st: &AppState, ch: &store::Channel, text: &str) -> Result<Option<String>, SendErr> {
    let api = st.bot.youtube().map_err(|_| SendErr::NotReady)?;
    let Some(chat) = st.bot.yt(&ch.sub_id, |m| m.chat.clone()) else { return Err(SendErr::NotReady) };
    // YouTube takes 200 characters a message.
    let text: String = text.chars().take(200).collect();
    let token = access(st, &ch.sub_id).await?;
    take_send(st, &ch.sub_id, Call::Send)?;
    let res = match api.send(&token, &chat, &text).await {
        Err(YoutubeError::Unauthorized) => {
            refund_401(st, &ch.sub_id, Call::Send);
            forget_access_if(st, &ch.sub_id, &token);
            let token = access(st, &ch.sub_id).await?;
            take_send(st, &ch.sub_id, Call::Send)?;
            api.send(&token, &chat, &text).await
        }
        r => r,
    };
    match res {
        Ok(id) => Ok(id),
        Err(YoutubeError::Quota) => Err(SendErr::Quota(google_spent(st))),
        Err(YoutubeError::ChatEnded) => {
            went_offline(st, &ch.sub_id);
            Err(SendErr::NotReady)
        }
        Err(e) => Err(SendErr::from(e)),
    }
}

/// The live chat a moderation action works in (bans need one), if the channel is live.
pub(crate) fn live_chat(st: &AppState, sub: &str) -> Option<String> {
    st.bot.yt(sub, |m| m.chat.clone())
}

/// A moderation call's units (above the reserve for reading); `Err` with the 429 when the
/// budget is spent.
pub(crate) fn take_moderation(st: &AppState, sub: &str) -> Result<(), ApiError> {
    match take(st, sub, Call::Moderate) {
        Ok(()) => Ok(()),
        Err(Some(s)) => Err(spent_api(s)),
        Err(None) => Err(super::storage_busy().hint("Try again in a moment.")),
    }
}

/// 429 `youtube_quota` with the reset time.
pub(crate) fn spent_api(s: Spent) -> ApiError {
    let wait = u64::try_from(s.reset_at - now_ms()).unwrap_or(0).div_ceil(1000).max(1);
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "youtube_quota",
        format!("YouTube's daily limit for the cloud bot is used up; it's back at {}.", utc_hm(s.reset_at)),
    )
    .hint("Do this once your PC is on, or after the limit resets.")
    .header(axum::http::HeaderName::from_static("x-quota-reset"), s.reset_at.to_string())
    .header(axum::http::header::RETRY_AFTER, wait.to_string())
}

/// Google said the project's quota is gone during a moderation call.
pub(crate) fn note_google_quota(st: &AppState) -> ApiError {
    spent_api(google_spent(st))
}

/// Whether a timer may go out on YouTube now (at most one per `timer_gap`); remembers it.
pub(crate) fn timer_slot(st: &AppState, sub: &str) -> bool {
    let gap = st.bot.youtube_cfg().map_or(TIMER_GAP, |c| c.timer_gap);
    st.bot.yt(sub, |m| {
        let now = Instant::now();
        if m.last_timer.is_some_and(|t| now.duration_since(t) < gap) {
            return false;
        }
        m.last_timer = Some(now);
        true
    })
}

// ── Live detection and reading ───────────────────────────────────────────────────────

/// The channel went offline (YouTube said the chat ended): reader stopped, next look later.
fn went_offline(st: &AppState, sub: &str) {
    super::set_live(st, sub, Platform::Youtube, None);
    st.bot.yt(sub, |m| {
        m.chat = None;
        m.page_token = None;
        m.last_live = Some(Instant::now());
        m.next_check = Some(Instant::now() + CHECK_FAST);
    });
}

/// How long until the next live check while offline: every 2 minutes for the first half
/// hour after the takeover or the last live (or while the PC last saw it live), every 5
/// until 2 h, every 15 until 6 h, then hourly.
pub(crate) fn cadence(m: &YtMem, now: Instant) -> Duration {
    let fresh_live = m.hint.is_some_and(|(live, at)| live && now.duration_since(at) < HINT_FRESH);
    if fresh_live {
        return CHECK_FAST;
    }
    let from = match (m.duty_from, m.last_live) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let Some(from) = from else { return CHECK_FAST };
    match now.duration_since(from) {
        d if d < FAST_FOR => CHECK_FAST,
        d if d < SLOW_FOR => CHECK_SLOW,
        d if d < IDLE_AFTER => CHECK_SLOWER,
        _ => CHECK_IDLE,
    }
}

/// How long to wait before opening the stream again after `streak` failed opens in a row:
/// 30 s doubling, at most an hour.
pub fn fail_wait(streak: u32) -> Duration {
    if streak == 0 {
        return Duration::ZERO;
    }
    (REOPEN_MIN * 2u32.pow((streak - 1).min(10))).min(FAIL_WAIT_MAX)
}

/// The channels YouTube checks look at: switched-on Pro subscriptions with a ready channel.
async fn channels(st: &AppState) -> Option<Vec<store::Channel>> {
    let work = super::db(st, |db| {
        let mut out = Vec::new();
        for sub in db.bot_enabled_subs()? {
            if !super::sub_is_pro(db, &sub) {
                continue;
            }
            if let Some(ch) = db.bot_channel(&sub, "youtube")?.filter(store::Channel::ready) {
                out.push(ch);
            }
        }
        Ok(out)
    })
    .await;
    match work {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list cloud bot YouTube channels");
            None
        }
    }
}

/// How many channels' checks run at once.
pub const CHECK_CONCURRENCY: usize = 8;

/// One pass (its own loop, every 20 s): for each switched-on Pro subscription with a
/// ready YouTube channel, while the cloud holds YouTube's lease, look for a live broadcast
/// when one is due and keep a chat reader while live; at most [`CHECK_CONCURRENCY`] at once.
/// Readers of subscriptions that no longer qualify (the PC came back, switched off, lapsed,
/// disconnected) stop; refresh locks of subscriptions without YouTube go.
pub(crate) async fn tick(st: &AppState) {
    if st.bot.youtube.is_none() {
        return;
    }
    let Some(channels) = channels(st).await else { return };
    let keep: HashSet<String> = channels.iter().map(|c| c.sub_id.clone()).collect();
    for sub in st.bot.yt_reading() {
        if !keep.contains(&sub) {
            st.bot.yt(&sub, YtMem::stop);
        }
    }
    let with_youtube: HashSet<String> = st
        .db
        .bot_all_channels()
        .map(|all| all.into_iter().filter(|c| c.platform == "youtube").map(|c| c.sub_id).collect())
        .unwrap_or_else(|_| keep.clone());
    st.bot.prune_yt_locks(&with_youtube);
    let slots = Arc::new(tokio::sync::Semaphore::new(CHECK_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for ch in channels {
        let Ok(slot) = slots.clone().acquire_owned().await else { break };
        let st2 = st.clone();
        tasks.spawn(async move {
            step(&st2, &ch).await;
            drop(slot);
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// YouTube's own loop (spawned by [`super::run`]), so slow Google calls never hold up the
/// Twitch and Kick timers. Google's quota stop from before a restart is read first.
pub(crate) async fn run(st: AppState) {
    load_google_out(&st);
    loop {
        tokio::time::sleep(super::TIMER_TICK).await;
        tick(&st).await;
    }
}

/// One channel's turn in [`tick`].
pub(crate) async fn step(st: &AppState, ch: &store::Channel) {
    let sub = ch.sub_id.as_str();
    let now = Instant::now();
    // A spent day that's over: the notice goes.
    if quota_error_stale(ch, now_ms()) {
        let _ = st.db.bot_clear_transient_error(sub, "youtube");
    }
    if super::cloud_since(st, sub, Platform::Youtube).is_none() {
        // The PC answers YouTube: no calls at all from here.
        st.bot.yt(sub, |m| {
            m.stop();
            m.duty_from = None;
            m.next_check = None;
            m.chat = None;
            m.page_token = None;
        });
        return;
    }
    let due = st.bot.yt(sub, |m| {
        if m.reading() {
            return false;
        }
        if m.duty_from.is_none() {
            m.duty_from = Some(now);
            // The PC just saw it offline: no need to look at once.
            let quiet = m.hint.is_some_and(|(live, at)| !live && now.duration_since(at) < HINT_FRESH);
            let first = if quiet { now + CHECK_FAST } else { now };
            m.next_check = Some(m.next_check.map_or(first, |t| t.min(first)));
        }
        m.next_check.is_none_or(|t| now >= t)
    });
    if due {
        check_live(st, ch).await;
    }
}

/// `liveBroadcasts.list` (1 unit, counted once the sign-in is at hand): live → the reader
/// starts (unless reading gave up less than an hour ago); offline → the next look is set.
async fn check_live(st: &AppState, ch: &store::Channel) {
    let sub = ch.sub_id.clone();
    let later = |st: &AppState, d: Duration| st.bot.yt(&sub, |m| m.next_check = Some(Instant::now() + d));
    let Ok(api) = st.bot.youtube() else { return };
    let token = match access(st, &sub).await {
        Ok(t) => t,
        Err(_) => {
            later(st, CHECK_SLOW);
            return;
        }
    };
    match take(st, &sub, Call::Detect) {
        Ok(()) => {}
        Err(Some(s)) => {
            let _ = st.db.bot_set_error(&sub, "youtube", Some(&spent_error(s)));
            later(st, QUOTA_RECHECK);
            return;
        }
        Err(None) => {
            later(st, CHECK_FAST);
            return;
        }
    }
    st.bot.yt(&sub, |m| m.last_check = Some(Instant::now()));
    match api.active_broadcast(&token).await {
        Ok(Some(b)) => {
            if ch.live_since.is_none() {
                super::set_live(st, &sub, Platform::Youtube, Some(b.started_at.unwrap_or_else(now_ms)));
            }
            let now = Instant::now();
            let resting = st.bot.yt(&sub, |m| {
                m.last_live = Some(now);
                match m.gave_up {
                    Some(t) if now.duration_since(t) < GIVE_UP_FOR => {
                        m.chat = Some(b.live_chat_id.clone());
                        m.next_check = Some(t + GIVE_UP_FOR);
                        true
                    }
                    Some(_) => {
                        // An hour on and still live: one more try.
                        m.gave_up = None;
                        false
                    }
                    None => false,
                }
            });
            if !resting {
                start_reader(st, &sub, b.live_chat_id);
            }
        }
        Ok(None) => {
            if ch.live_since.is_some() {
                super::set_live(st, &sub, Platform::Youtube, None);
            }
            st.bot.yt(&sub, |m| {
                let now = Instant::now();
                if m.chat.is_some() || ch.live_since.is_some() {
                    m.last_live = Some(now);
                }
                m.chat = None;
                m.page_token = None;
                m.next_check = Some(now + cadence(m, now));
            });
        }
        Err(YoutubeError::Quota) => {
            google_out(st);
            later(st, QUOTA_RECHECK);
        }
        Err(YoutubeError::Unauthorized) => {
            refund_401(st, &sub, Call::Detect);
            forget_access_if(st, &sub, &token);
            later(st, CHECK_FAST);
        }
        Err(e) => {
            tracing::debug!(error = %e, "couldn't ask YouTube whether a channel is live");
            st.bot.yt(&sub, |m| m.next_check = Some(Instant::now() + cadence(m, Instant::now())));
        }
    }
}

fn start_reader(st: &AppState, sub: &str, chat: String) {
    static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = GEN.fetch_add(1, Ordering::Relaxed);
    let st2 = st.clone();
    let sub2 = sub.to_string();
    let chat2 = chat.clone();
    st.bot.yt(sub, move |m| {
        m.stop();
        if m.chat.as_deref() != Some(chat.as_str()) {
            m.page_token = None;
        }
        m.chat = Some(chat);
        let h = tokio::spawn(async move { read(&st2, &sub2, &chat2, generation).await });
        m.reader = Some((generation, h.abort_handle()));
    });
}

/// The reader ended by itself: forget it (only if it's still the current one).
fn reader_done(st: &AppState, sub: &str, generation: u64, next: Option<Duration>) {
    st.bot.yt(sub, |m| {
        if m.reader.as_ref().is_some_and(|(g, _)| *g == generation) {
            m.reader = None;
            if let Some(d) = next {
                m.next_check = Some(Instant::now() + d);
            }
        }
    });
}

/// How long to wait before opening the stream again after `quick` short streams in a row.
pub fn reopen_wait(quick: u32) -> Duration {
    (REOPEN_MIN * 2u32.pow(quick.min(4))).min(REOPEN_MAX)
}

/// How one opened stream ended.
enum Ended {
    /// YouTube closed it (normal now and then).
    Closed,
    /// Nothing came for [`FEED_IDLE`].
    Idle,
    Failed(YoutubeError),
}

/// Holds the chat stream while live: each open counts 5 units (once the sign-in is at
/// hand); lines go to [`super::on_chat`] (dedupe, ring, answers); a closed stream is opened
/// again from the last page token after [`reopen_wait`]; a stream silent for [`FEED_IDLE`]
/// is opened again (counted as a failure when it happens twice in a row); failed opens wait
/// [`fail_wait`] (the streak is kept across reader restarts) and after [`GIVE_UP_AFTER`]
/// reading stops with `youtube_unavailable`; the chat ending, the budget, or a sign-out stop it.
async fn read(st: &AppState, sub: &str, chat: &str, generation: u64) {
    let Ok(api) = st.bot.youtube() else { return };
    let mut quick = 0u32;
    let mut idle_in_a_row = 0u32;
    loop {
        let token = match access(st, sub).await {
            Ok(t) => t,
            Err(_) => return reader_done(st, sub, generation, Some(CHECK_SLOW)),
        };
        match take(st, sub, Call::Read) {
            Ok(()) => {}
            Err(Some(s)) => {
                let _ = st.db.bot_set_error(sub, "youtube", Some(&spent_error(s)));
                return reader_done(st, sub, generation, Some(QUOTA_RECHECK));
            }
            Err(None) => return reader_done(st, sub, generation, Some(CHECK_FAST)),
        }
        let page = st.bot.yt(sub, |m| m.page_token.clone());
        let opened = Instant::now();
        let ended = match api.open_chat(&token, chat, page).await {
            Ok(mut feed) => loop {
                match tokio::time::timeout(FEED_IDLE, feed.next()).await {
                    Err(_) => break Ended::Idle,
                    Ok(Ok(Some(batch))) => {
                        idle_in_a_row = 0;
                        let recovered = st.bot.yt(sub, |m| {
                            if let Some(t) = batch.next_page_token.clone() {
                                m.page_token = Some(t);
                            }
                            std::mem::take(&mut m.fail_streak) >= GIVE_UP_AFTER
                        });
                        if recovered {
                            let _ = st.db.bot_clear_transient_error(sub, "youtube");
                        }
                        if !batch.events.is_empty() {
                            let s2 = sub.to_string();
                            let ch = super::db(st, move |db| db.bot_channel(&s2, "youtube")).await;
                            if let Ok(Some(ch)) = ch {
                                for e in batch.events {
                                    super::on_chat(st, &ch, Platform::Youtube, e).await;
                                }
                            }
                        }
                        if batch.offline {
                            break Ended::Failed(YoutubeError::ChatEnded);
                        }
                    }
                    Ok(Ok(None)) => break Ended::Closed,
                    Ok(Err(e)) => break Ended::Failed(e),
                }
            },
            Err(e) => Ended::Failed(e),
        };
        let failed = match ended {
            Ended::Failed(YoutubeError::ChatEnded | YoutubeError::NotFound) => {
                went_offline(st, sub);
                return reader_done(st, sub, generation, None);
            }
            Ended::Failed(YoutubeError::Quota) => {
                let _ = st.db.bot_set_error(sub, "youtube", Some(&spent_error(google_spent(st))));
                return reader_done(st, sub, generation, Some(QUOTA_RECHECK));
            }
            Ended::Failed(YoutubeError::Unauthorized) => {
                refund_401(st, sub, Call::Read);
                forget_access_if(st, sub, &token);
                true
            }
            Ended::Failed(YoutubeError::Forbidden(m)) => {
                tracing::info!(why = %m, "YouTube refused the cloud bot's chat stream");
                true
            }
            Ended::Failed(e) => {
                tracing::debug!(error = %e, "the cloud bot's YouTube chat stream dropped");
                true
            }
            Ended::Idle => {
                tracing::debug!("the cloud bot's YouTube chat stream went silent; opening it again");
                idle_in_a_row += 1;
                idle_in_a_row > 1
            }
            Ended::Closed => false,
        };
        let streak = st.bot.yt(sub, |m| {
            if failed {
                m.fail_streak += 1;
            }
            m.fail_streak
        });
        if failed && streak >= GIVE_UP_AFTER {
            let err = super::error_json(
                "youtube_unavailable",
                "Couldn't keep reading your YouTube chat.",
                "Ferrocast tries again in an hour if you're still live, or press Try again.",
            );
            let _ = st.db.bot_set_error(sub, "youtube", Some(&err));
            st.bot.yt(sub, |m| m.gave_up = Some(Instant::now()));
            return reader_done(st, sub, generation, Some(GIVE_UP_FOR));
        }
        quick = if opened.elapsed() < GOOD_STREAM { quick + 1 } else { 0 };
        let wait = reopen_wait(quick.saturating_sub(1)).max(if failed { fail_wait(streak) } else { Duration::ZERO });
        tokio::time::sleep(wait).await;
    }
}
