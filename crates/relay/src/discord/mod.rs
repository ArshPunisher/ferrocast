//! Ferrocast Pro F4, the Ferrocast bot for Discord (docs/plan/PRO.md F4, milestone M8;
//! API.md §9.6). REST only, no gateway.
//!
//! - The streamer adds the bot to their server (`POST /v1/pro/discord/connect` → Discord's
//!   install page → `GET /discord/callback`, which learns the server from Discord's own token
//!   answer, never from the URL).
//! - Cloud go-live posts (`live`): an embed in the chosen channel, edited at the end.
//! - Role perks (`perks`): rules, perk events from the desktop, grants with an end, the hourly
//!   sweep. Viewers link themselves at `/d/<code>/link` (`pages`): Discord sign-in with
//!   `identify connections`, the Twitch / YouTube ids are kept, the token is revoked at once.
//! - `POST /discord/interactions` (`interactions`): Ed25519-checked, answers PING.
//!
//! Env: `DISCORD_APP_ID`, `DISCORD_BOT_TOKEN`, `DISCORD_PUBLIC_KEY`, `DISCORD_CLIENT_SECRET`;
//! `features.discord` is true only with all four. Limits per subscription: 1 server, 10 rules,
//! 5,000 linked members.

mod api;
mod interactions;
mod live;
mod pages;
mod perks;
mod store;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ed25519_dalek::VerifyingKey;
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde::Deserialize;
use serde_json::{Value, json};

pub use api::{Channel, Connection, DiscordApi, DiscordError, Fut, Guild, HttpDiscord, Role, Token};
pub use perks::sweep;
pub(crate) use store::SCHEMA;

use crate::billing::gate::{ProCtx, require_pro};
use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, authed, now_ms};

pub(crate) const DAY_MS: i64 = 86_400_000;
/// Discord users with a linked account, per subscription (its one server).
pub const MAX_LINKS: i64 = 5_000;
/// Servers per subscription.
pub const MAX_GUILDS: i64 = 1;
/// A signed `state` (bot install, viewer link) lasts this long.
pub const STATE_TTL_MS: i64 = 10 * 60_000;
/// Channels and roles are read from Discord at most this often per server.
const SNAPSHOT_TTL_MS: i64 = 60_000;
/// Requests per subscription per minute, every route together: a rate, not a quota.
pub const RATE: Limit = Limit { name: "discord_requests", period: Period::Minute, limit: 60 };
/// Viewer link pages (sign-ins) per address per 10 minutes.
const LINKS_PER_IP: u32 = 20;
/// Data is kept this long after Pro ends (the hourly sweep), then the bot leaves.
pub const KEEP_AFTER_LAPSE_MS: i64 = 30 * DAY_MS;
/// Manage Roles, Send Messages, Embed Links, View Channels.
pub const PERMISSIONS: u64 = (1 << 28) | (1 << 11) | (1 << 14) | (1 << 10);

/// The Discord app, from the environment. `Debug` never shows the secrets.
#[derive(Clone)]
pub struct DiscordConfig {
    pub app_id: String,
    pub bot_token: String,
    pub client_secret: String,
    pub public_key: VerifyingKey,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("app_id", &self.app_id)
            .field("bot_token", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl DiscordConfig {
    /// `Ok(None)` when none of the four is set; an error names what's missing or wrong.
    pub fn from_env() -> Result<Option<DiscordConfig>, String> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Self::from_values(
            env("DISCORD_APP_ID"),
            env("DISCORD_BOT_TOKEN"),
            env("DISCORD_CLIENT_SECRET"),
            env("DISCORD_PUBLIC_KEY"),
        )
    }

    pub fn from_values(
        app_id: Option<String>,
        bot_token: Option<String>,
        client_secret: Option<String>,
        public_key: Option<String>,
    ) -> Result<Option<DiscordConfig>, String> {
        match (app_id, bot_token, client_secret, public_key) {
            (None, None, None, None) => Ok(None),
            (Some(app_id), Some(bot_token), Some(client_secret), Some(pk)) => {
                if !perks::is_snowflake(&app_id) {
                    return Err("DISCORD_APP_ID isn't an application id".into());
                }
                let public_key = interactions::parse_public_key(&pk)
                    .ok_or_else(|| "DISCORD_PUBLIC_KEY isn't the app's 64-character hex public key".to_string())?;
                Ok(Some(DiscordConfig { app_id, bot_token, client_secret, public_key }))
            }
            _ => {
                Err("set all four of DISCORD_APP_ID, DISCORD_BOT_TOKEN, DISCORD_CLIENT_SECRET, DISCORD_PUBLIC_KEY"
                    .into())
            }
        }
    }
}

/// A server's channels and roles as the dashboard's pickers show them.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    channels: Vec<Value>,
    channel_ids: Vec<String>,
    roles: Vec<Value>,
    /// role id → assignable by the bot
    assignable: HashMap<String, bool>,
}

/// The bot: config, the REST client, small caches. Shared by every request.
#[derive(Debug)]
pub struct Runtime {
    cfg: Option<DiscordConfig>,
    api: Option<Arc<dyn DiscordApi>>,
    snapshots: Mutex<HashMap<String, (i64, Arc<Snapshot>)>>,
    /// (10-minute window, sign-ins) per address, for the viewer link pages.
    per_ip: Mutex<HashMap<std::net::IpAddr, (i64, u32)>>,
    /// One go-live report at a time per subscription and platform (`<sub>/<platform>`), so
    /// two quick reports can't both post.
    live_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Between removals in the sweep (Discord's per-route limits); zero in tests.
    pub(crate) sweep_pause: Duration,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(None, None)
    }
}

impl Runtime {
    /// Both or neither: without the app's config the client is useless and vice versa.
    pub fn new(cfg: Option<DiscordConfig>, api: Option<Arc<dyn DiscordApi>>) -> Runtime {
        let (cfg, api) = match (cfg, api) {
            (Some(c), Some(a)) => (Some(c), Some(a)),
            _ => (None, None),
        };
        Runtime {
            cfg,
            api,
            snapshots: Mutex::default(),
            per_ip: Mutex::default(),
            live_locks: Mutex::default(),
            sweep_pause: Duration::from_millis(250),
        }
    }

    /// The production client for a config.
    pub fn from_config(cfg: Option<DiscordConfig>) -> Runtime {
        let api = cfg.as_ref().map(|c| {
            Arc::new(HttpDiscord::new(
                api::DEFAULT_BASE.into(),
                c.app_id.clone(),
                c.bot_token.clone(),
                c.client_secret.clone(),
            )) as Arc<dyn DiscordApi>
        });
        Runtime::new(cfg, api)
    }

    #[cfg(test)]
    pub(crate) fn without_pauses(mut self) -> Runtime {
        self.sweep_pause = Duration::ZERO;
        self
    }

    pub(crate) fn api(&self) -> Result<&dyn DiscordApi, ApiError> {
        self.api.as_deref().ok_or_else(not_configured)
    }

    fn cfg(&self) -> Result<&DiscordConfig, ApiError> {
        self.cfg.as_ref().ok_or_else(not_configured)
    }

    pub(crate) fn public_key(&self) -> Option<VerifyingKey> {
        self.cfg.as_ref().map(|c| c.public_key)
    }

    fn forget_snapshot(&self, guild: &str) {
        self.snapshots.lock().unwrap_or_else(|p| p.into_inner()).remove(guild);
    }

    /// The lock for one subscription's go-live reports on one platform. Unused locks are
    /// dropped as new ones are made, so the map stays small.
    pub(crate) fn live_lock(&self, sub: &str, platform: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.live_locks.lock().unwrap_or_else(|p| p.into_inner());
        m.retain(|_, l| Arc::strong_count(l) > 1);
        m.entry(format!("{sub}/{platform}")).or_default().clone()
    }

    /// One more sign-in from `ip` in this 10-minute window, if it's allowed.
    fn take_ip(&self, ip: std::net::IpAddr, now: i64) -> bool {
        let window = now.div_euclid(600_000);
        let mut m = self.per_ip.lock().unwrap_or_else(|p| p.into_inner());
        m.retain(|_, (w, _)| *w == window);
        let e = m.entry(ip).or_insert((window, 0));
        e.1 += 1;
        e.1 <= LINKS_PER_IP
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/discord", get(status).delete(disconnect))
        .route("/v1/pro/discord/connect", post(connect))
        .route("/v1/pro/discord/channel", put(set_channel))
        .route("/v1/pro/discord/go-live", put(set_go_live))
        .route("/v1/pro/discord/rules", put(add_rule))
        .route("/v1/pro/discord/rules/{id}", put(update_rule).delete(delete_rule))
        .route("/v1/pro/discord/perk-events", post(perk_events))
        .route("/v1/pro/discord/live", post(live_report))
        .route("/discord/callback", get(pages::callback))
        .route("/discord/interactions", post(interactions::interactions))
        .route("/d/{code}/link", get(pages::link_page))
        .route("/d/{code}/link/start", get(pages::link_start))
}

/// `features.discord`: true only with the Discord app configured.
pub(crate) fn available(st: &AppState) -> bool {
    st.discord.api.is_some()
}

/// Its counters in `/v1/pro/status` `quotas`: none are per day or month (1 server, 10 rules,
/// 5,000 links are totals; `GET /v1/pro/discord` lists them as `limits`).
pub(crate) const QUOTAS: &[Limit] = &[];

// ── Errors ───────────────────────────────────────────────────────────────────────────

pub(crate) fn not_configured() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "discord_unavailable",
        "The Ferrocast bot isn't set up on this relay yet.",
    )
    .hint("Your free webhook go-live posts keep working.")
}

pub(crate) fn not_connected() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "not_connected", "No Discord server is connected yet.")
        .hint("Press Connect server in Settings › Discord.")
}

pub(crate) const ROLE_HINT: &str = "Move Ferrocast's role above the perk roles in Server Settings › Roles.";

/// What Discord said, as the relay's answer.
pub(crate) fn discord_error(e: DiscordError) -> ApiError {
    match e {
        DiscordError::MissingPermission => ApiError::new(
            StatusCode::FORBIDDEN,
            "missing_permission",
            "Discord says Ferrocast is missing a permission.",
        )
        .hint(ROLE_HINT),
        DiscordError::MissingAccess => ApiError::new(
            StatusCode::FORBIDDEN,
            "missing_permission",
            "Discord says Ferrocast can't see that channel or server.",
        )
        .hint("Let Ferrocast's role view the channel, or connect the server again if the bot was removed."),
        DiscordError::RateLimited { retry_after_s } => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "discord_rate",
            format!("Discord asked Ferrocast to slow down. Try again in {retry_after_s} seconds."),
        )
        .header(header::RETRY_AFTER, retry_after_s.to_string()),
        DiscordError::NotFound(api::codes::UNKNOWN_GUILD) => guild_not_found(),
        DiscordError::NotFound(_) => {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "Discord doesn't know that any more.")
                .hint("Reload Settings › Discord.")
        }
        DiscordError::Unauthorized => {
            tracing::error!("Discord refused the bot token: check DISCORD_BOT_TOKEN");
            not_configured()
        }
        DiscordError::BadRequest(m) => {
            tracing::warn!(discord = %m, "Discord refused a request");
            ApiError::new(StatusCode::BAD_GATEWAY, "discord_unavailable", "Discord refused that request.")
        }
        DiscordError::Unavailable(m) => {
            tracing::warn!(why = %m, "Discord unavailable");
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "discord_unavailable",
                "Couldn't reach Discord. Try again in a minute.",
            )
        }
    }
}

fn guild_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "guild_not_found", "Ferrocast isn't in that Discord server any more.")
        .hint("Connect the server again in Settings › Discord.")
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

/// Strips control and direction characters, trims, caps the length.
pub(crate) fn clean(s: &str, max: usize) -> String {
    crate::tips::clean_text(s, max)
}

// ── Request plumbing ─────────────────────────────────────────────────────────────────

/// A body read as bytes, so the Pro gate answers before the body is judged.
#[derive(Debug)]
pub(crate) struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Requests can be up to 256 KB.")
            } else {
                invalid("The request body couldn't be read.")
            }
        })
    }
}

fn json_body<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if !json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<T>(body).map_err(|e| {
        if e.is_syntax() || e.is_eof() {
            invalid("The request isn't JSON.")
        } else {
            invalid(format!("The request has a missing or wrong field: {e}"))
        }
    })
}

/// The Pro gate, then the bot being set up here, then the per-subscription rate.
async fn pro(st: &AppState, headers: &HeaderMap) -> Result<ProCtx, ApiError> {
    let ctx = require_pro(st, headers).await?;
    st.discord.api()?;
    match st.db.try_take(&ctx.sub_id, RATE.name, RATE.period, 1, RATE.limit) {
        Ok(()) => Ok(ctx),
        Err(TakeErr::Over(q)) => {
            let wait = u64::try_from(q.reset_at - now_ms()).unwrap_or(0).div_ceil(1000).max(1);
            Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "discord_rate",
                "Too many Discord requests. Wait a minute.",
            )
            .header(HeaderName::from_static("x-quota-reset"), q.reset_at.to_string())
            .header(header::RETRY_AFTER, wait.to_string()))
        }
        Err(TakeErr::Db(e)) => Err(ApiError::db(e)),
    }
}

// ── Signed state ─────────────────────────────────────────────────────────────────────

/// What a `state` is for: `i` adds the bot for an install, `l` links a viewer for a server
/// (by its link code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Install,
    Link,
}

impl Purpose {
    fn tag(self) -> &'static str {
        match self {
            Purpose::Install => "i",
            Purpose::Link => "l",
        }
    }
}

fn state_mac(cfg: &DiscordConfig, payload: &str) -> String {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut mac = Hmac::<Sha256>::new_from_slice(cfg.client_secret.as_bytes()).expect("hmac takes any key");
    mac.update(b"ferrocast discord state v1|");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// `<purpose>.<subject>.<expires ms>.<nonce>.<hmac>`: tied to the install (or server) and
/// short-lived, so a state can't be reused for someone else or later.
pub(crate) fn sign_state(cfg: &DiscordConfig, purpose: Purpose, subject: &str, now: i64) -> String {
    let payload = format!("{}.{subject}.{}.{}", purpose.tag(), now + STATE_TTL_MS, crate::random(9));
    let mac = state_mac(cfg, &payload);
    format!("{payload}.{mac}")
}

/// The subject of a good, unexpired state for `purpose`.
pub(crate) fn check_state(cfg: &DiscordConfig, raw: &str, now: i64) -> Option<(Purpose, String)> {
    let (payload, mac) = raw.rsplit_once('.')?;
    let want = state_mac(cfg, payload);
    use subtle::ConstantTimeEq;
    if !bool::from(want.as_bytes().ct_eq(mac.as_bytes())) {
        return None;
    }
    let mut parts = payload.split('.');
    let purpose = match parts.next()? {
        "i" => Purpose::Install,
        "l" => Purpose::Link,
        _ => return None,
    };
    let subject = parts.next()?.to_string();
    let exp: i64 = parts.next()?.parse().ok()?;
    parts.next()?;
    if parts.next().is_some() || now > exp || subject.is_empty() {
        return None;
    }
    Some((purpose, subject))
}

/// A short code for an install ("7F3A-90C2"), shown on the "Ferrocast is in your server" page
/// and in Settings › Discord, so a streamer can see that the server went to *their* PC and not
/// to someone who sent them an install link. Keyed with the app's secret: it gives nothing
/// about the install id away.
pub(crate) fn pc_code(cfg: &DiscordConfig, install: &str) -> String {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut mac = Hmac::<Sha256>::new_from_slice(cfg.client_secret.as_bytes()).expect("hmac takes any key");
    mac.update(b"ferrocast discord pc code v1|");
    mac.update(install.as_bytes());
    let b = mac.finalize().into_bytes();
    format!("{:02X}{:02X}-{:02X}{:02X}", b[0], b[1], b[2], b[3])
}

pub(crate) fn redirect_uri(st: &AppState) -> String {
    format!("{}/discord/callback", st.cfg.public_url)
}

// ── The view ─────────────────────────────────────────────────────────────────────────

/// Channels (text and announcement, by position) and roles (highest first; `@everyone` and
/// integration roles left out), with whether the bot can hand each role out: only roles
/// below its own highest one.
async fn snapshot(st: &AppState, guild: &str) -> Result<Arc<Snapshot>, DiscordError> {
    let now = now_ms();
    if let Some((at, s)) = st.discord.snapshots.lock().unwrap_or_else(|p| p.into_inner()).get(guild)
        && now - at < SNAPSHOT_TTL_MS
    {
        return Ok(s.clone());
    }
    let Ok(api) = st.discord.api() else { return Err(DiscordError::Unavailable("not configured".into())) };
    let mut channels = api.channels(guild).await?;
    let mut roles = api.roles(guild).await?;
    let mine = api.bot_roles(guild).await?;
    channels.retain(|c| c.kind == 0 || c.kind == 5);
    channels.sort_by_key(|c| (c.position, c.id.clone()));
    let top = roles.iter().filter(|r| mine.contains(&r.id)).map(|r| r.position).max().unwrap_or(0);
    roles.retain(|r| r.id != guild && !r.managed);
    roles.sort_by(|a, b| b.position.cmp(&a.position).then(a.id.cmp(&b.id)));
    let s = Snapshot {
        channel_ids: channels.iter().map(|c| c.id.clone()).collect(),
        channels: channels.iter().map(|c| json!({ "id": c.id, "name": c.name })).collect(),
        assignable: roles.iter().map(|r| (r.id.clone(), r.position < top)).collect(),
        roles: roles
            .iter()
            .map(|r| json!({ "id": r.id, "name": r.name, "color": r.color, "assignable": r.position < top }))
            .collect(),
    };
    let s = Arc::new(s);
    st.discord.snapshots.lock().unwrap_or_else(|p| p.into_inner()).insert(guild.to_string(), (now, s.clone()));
    Ok(s)
}

fn limits() -> Value {
    json!({ "guilds": MAX_GUILDS, "rules": perks::MAX_RULES, "links": MAX_LINKS })
}

async fn view(st: &AppState, ctx: &ProCtx) -> Result<Value, ApiError> {
    let sub = ctx.sub_id.as_str();
    let cfg = st.discord.cfg()?;
    let this_pc = pc_code(cfg, &ctx.install);
    let Some(g) = st.db.discord_guild(sub).map_err(ApiError::db)? else {
        return Ok(json!({
            "connected": false, "guild": null, "channel_id": null, "channels": [], "roles": [],
            "go_live": { "enabled": false, "message": null }, "rules": [], "linked": 0, "link_url": null,
            "limits": limits(), "warning": null, "perk_issue": null, "connected_by": null, "pc_code": this_pc,
        }));
    };
    let (channels, roles, warning) = match snapshot(st, &g.guild_id).await {
        Ok(s) => (s.channels.clone(), s.roles.clone(), None),
        Err(
            DiscordError::NotFound(api::codes::UNKNOWN_GUILD)
            | DiscordError::MissingPermission
            | DiscordError::MissingAccess,
        ) => {
            (vec![], vec![], Some(format!("Ferrocast isn't in {} any more. Disconnect and connect it again.", g.name)))
        }
        Err(e) => {
            tracing::debug!(error = %e, "Discord channels and roles unavailable");
            (vec![], vec![], Some("Discord couldn't be reached, so channels and roles may be missing.".to_string()))
        }
    };
    let rules = st.db.discord_rules(sub).map_err(ApiError::db)?;
    let linked = st.db.discord_linked(&g.guild_id).map_err(ApiError::db)?;
    let icon_url = g.icon.as_ref().map(|h| format!("https://cdn.discordapp.com/icons/{}/{h}.png?size=64", g.guild_id));
    Ok(json!({
        "connected": true,
        "guild": { "id": g.guild_id, "name": g.name, "icon_url": icon_url },
        "channel_id": g.channel_id,
        "channels": channels,
        "roles": roles,
        "go_live": { "enabled": g.go_live, "message": g.go_live_message },
        "rules": rules,
        "linked": linked,
        "link_url": format!("{}/d/{}/link", st.cfg.public_url, g.link_code),
        "limits": limits(),
        "warning": warning,
        "perk_issue": g.perk_issue.as_deref().and_then(perks::issue_view),
        "connected_by": {
            "pc_code": pc_code(cfg, &g.connected_by),
            "this_pc": g.connected_by == ctx.install,
            "at": g.connected_at,
        },
        "pc_code": this_pc,
    }))
}

fn no_store(v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    r
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// `GET /v1/pro/discord`.
async fn status(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    Ok(no_store(view(&st, &ctx).await?))
}

/// `POST /v1/pro/discord/connect` → 201 `{invite_url, expires_at}`.
async fn connect(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    if st.db.discord_guild(&ctx.sub_id).map_err(ApiError::db)?.is_some() {
        return Err(ApiError::new(StatusCode::CONFLICT, "already_connected", "A Discord server is already connected.")
            .hint("Pro has one server. Disconnect it first to use another."));
    }
    let cfg = st.discord.cfg()?;
    let now = now_ms();
    let mut url = url::Url::parse("https://discord.com/oauth2/authorize")
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "config", "The Discord address is wrong."))?;
    url.query_pairs_mut()
        .append_pair("client_id", &cfg.app_id)
        .append_pair("scope", "bot applications.commands")
        .append_pair("permissions", &PERMISSIONS.to_string())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri(&st))
        .append_pair("integration_type", "0")
        .append_pair("state", &sign_state(cfg, Purpose::Install, &ctx.install, now));
    let body = json!({ "invite_url": url.to_string(), "expires_at": now + STATE_TTL_MS });
    Ok((StatusCode::CREATED, no_store(body)).into_response())
}

#[derive(Deserialize)]
struct ChannelIn {
    channel_id: String,
}

/// `PUT /v1/pro/discord/channel {channel_id}`.
async fn set_channel(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: ChannelIn = json_body(&headers, &body)?;
    let g = st.db.discord_guild(&ctx.sub_id).map_err(ApiError::db)?.ok_or_else(not_connected)?;
    st.discord.forget_snapshot(&g.guild_id);
    let s = snapshot(&st, &g.guild_id).await.map_err(discord_error)?;
    let id = b.channel_id.trim();
    if !s.channel_ids.iter().any(|c| c == id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "channel_not_found", "That channel isn't in your server.")
            .hint("Pick a text channel Ferrocast can see."));
    }
    st.db.discord_set_channel(&ctx.sub_id, id, now_ms()).map_err(ApiError::db)?;
    Ok(no_store(view(&st, &ctx).await?))
}

#[derive(Deserialize)]
struct GoLiveIn {
    enabled: bool,
    #[serde(default)]
    message: Option<String>,
}

pub const MAX_MESSAGE: usize = 300;

/// `PUT /v1/pro/discord/go-live {enabled, message?}`.
async fn set_go_live(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: GoLiveIn = json_body(&headers, &body)?;
    let g = st.db.discord_guild(&ctx.sub_id).map_err(ApiError::db)?.ok_or_else(not_connected)?;
    let message = b.message.as_deref().map(|m| clean(m, usize::MAX)).filter(|m| !m.is_empty());
    if message.as_ref().is_some_and(|m| m.chars().count() > MAX_MESSAGE) {
        return Err(invalid(format!("message: up to {MAX_MESSAGE} characters.")));
    }
    if b.enabled && g.channel_id.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_channel",
            "Pick a channel for go-live posts first.",
        ));
    }
    st.db.discord_set_go_live(&ctx.sub_id, b.enabled, message.as_deref(), now_ms()).map_err(ApiError::db)?;
    Ok(no_store(view(&st, &ctx).await?))
}

/// The role must be in the server and below Ferrocast's own role.
async fn check_role(st: &AppState, guild: &str, role: &str) -> Result<(), ApiError> {
    st.discord.forget_snapshot(guild);
    let s = snapshot(st, guild).await.map_err(discord_error)?;
    match s.assignable.get(role) {
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "role_not_found", "That role isn't in your server.")
            .hint("Pick a role from the list.")),
        Some(false) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "missing_permission",
            "Ferrocast can't give that role: it's above Ferrocast's own role.",
        )
        .hint(ROLE_HINT)),
        Some(true) => Ok(()),
    }
}

fn rule_id() -> String {
    let mut b = [0u8; 6];
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    format!("dr_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// `PUT /v1/pro/discord/rules` → 201: a new rule.
async fn add_rule(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: perks::RuleIn = json_body(&headers, &body)?;
    let g = st.db.discord_guild(&ctx.sub_id).map_err(ApiError::db)?.ok_or_else(not_connected)?;
    let rule = perks::check_rule(b, rule_id(), now_ms())?;
    if st.db.discord_rules(&ctx.sub_id).map_err(ApiError::db)?.len() as i64 >= perks::MAX_RULES {
        return Err(too_many_rules());
    }
    check_role(&st, &g.guild_id, &rule.role_id).await?;
    if !st.db.discord_add_rule(&ctx.sub_id, &rule, perks::MAX_RULES).map_err(ApiError::db)? {
        return Err(too_many_rules());
    }
    Ok((StatusCode::CREATED, no_store(view(&st, &ctx).await?)).into_response())
}

fn too_many_rules() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "too_many_rules", format!("Up to {} perk rules.", perks::MAX_RULES))
        .hint("Delete one to add another.")
}

fn rule_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "rule_not_found", "That perk rule doesn't exist any more.")
        .hint("Reload Settings › Discord.")
}

/// `PUT /v1/pro/discord/rules/{id}`.
async fn update_rule(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: perks::RuleIn = json_body(&headers, &body)?;
    let g = st.db.discord_guild(&ctx.sub_id).map_err(ApiError::db)?.ok_or_else(not_connected)?;
    let old = st.db.discord_rules(&ctx.sub_id).map_err(ApiError::db)?.into_iter().find(|r| r.id == id);
    let Some(old) = old else { return Err(rule_not_found()) };
    let rule = perks::check_rule(b, old.id, old.created_at)?;
    check_role(&st, &g.guild_id, &rule.role_id).await?;
    if !st.db.discord_update_rule(&ctx.sub_id, &rule).map_err(ApiError::db)? {
        return Err(rule_not_found());
    }
    // A fresh role or a fixed order: the old problem may be gone; the next perk says.
    st.db.discord_clear_perk_issue_for(&ctx.sub_id, &rule.id).map_err(ApiError::db)?;
    Ok(no_store(view(&st, &ctx).await?))
}

/// `DELETE /v1/pro/discord/rules/{id}` → 204. Roles it gave keep their end date.
async fn delete_rule(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    if !st.db.discord_delete_rule(&ctx.sub_id, &id).map_err(ApiError::db)? {
        return Err(rule_not_found());
    }
    st.db.discord_clear_perk_issue_for(&ctx.sub_id, &id).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /v1/pro/discord` → 204: the bot leaves the server and everything about it is
/// deleted. Open after Pro ends too (R-04): only an install without any subscription gets 402.
async fn disconnect(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let sub =
        st.db.pro_sub_for_install(&install).map_err(ApiError::db)?.ok_or_else(crate::billing::gate::pro_required)?;
    if let Some(g) = st.db.discord_guild(&sub.id).map_err(ApiError::db)? {
        leave(&st, &g.guild_id).await;
        st.db.discord_forget(&sub.id).map_err(ApiError::db)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn leave(st: &AppState, guild: &str) {
    st.discord.forget_snapshot(guild);
    if let Ok(api) = st.discord.api() {
        match api.leave(guild).await {
            Ok(()) | Err(DiscordError::NotFound(_)) => {}
            Err(e) => tracing::warn!(error = %e, "couldn't leave a Discord server; it can be kicked by hand"),
        }
    }
}

/// `POST /v1/pro/discord/perk-events [..]` (0 to 100) → 200 with counts.
async fn perk_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let raw: Vec<Value> = json_body(&headers, &body)?;
    if raw.len() > perks::MAX_EVENTS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_many_events",
            format!("Up to {} perk events at a time.", perks::MAX_EVENTS),
        ));
    }
    // Each event is judged on its own: a bad one (or a clock running ahead) never costs the
    // rest their roles. Only a body that isn't a list of events is refused.
    let (events, invalid) = perks::parse_events(raw, now_ms());
    Ok(no_store(perks::apply(&st, &ctx.sub_id, &events, invalid).await?))
}

/// `POST /v1/pro/discord/live`.
async fn live_report(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: live::LiveIn = json_body(&headers, &body)?;
    Ok(no_store(live::report(&st, &ctx.sub_id, b).await?))
}

/// Part of the hourly sweep: a subscription whose Pro ended over 30 days ago loses its server
/// (the bot leaves); go-live posts older than 30 days are forgotten.
pub(crate) async fn sweep_lapsed(st: &AppState, now: i64) {
    let guilds = match st.db.discord_guilds() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list Discord servers");
            return;
        }
    };
    for g in guilds {
        let kept = match st.db.pro_sub(&g.sub_id) {
            Ok(None) => false,
            Ok(Some(s)) => {
                let access = s.access();
                crate::billing::access::is_pro(&access, now) || {
                    let stopped = s.stopped_at();
                    now < stopped.saturating_add(KEEP_AFTER_LAPSE_MS)
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't check a Discord server's subscription");
                continue;
            }
        };
        if !kept {
            leave(st, &g.guild_id).await;
            match st.db.discord_forget(&g.sub_id) {
                Ok(()) => tracing::info!("a Discord server was disconnected 30 days after Pro ended"),
                Err(e) => tracing::warn!(error = %e, "couldn't forget a lapsed Discord server"),
            }
        }
    }
    if let Err(e) = st.db.discord_prune_posts(now - KEEP_AFTER_LAPSE_MS) {
        tracing::warn!(error = %e, "couldn't prune old go-live posts");
    }
}
