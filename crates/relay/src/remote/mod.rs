//! Ferrocast Pro F2, the dashboard from anywhere (docs/plan/PRO.md F2, milestone M6;
//! API.md §9.5). Through the PC's tunnel while it's on; while it's off the relay answers chat,
//! moderation (by the cloud bot), history and the cloud bot's switch itself (`offline`, M10).
//!
//! - The PC keeps an outbound WebSocket here, `/v1/remote/tunnel` (install `Authorization`,
//!   Pro), separate from `/v1/link`, while Pro is on and a phone is paired (`tunnel`).
//! - A phone pairs without accounts: the desktop asks for a one-time code
//!   (`POST /v1/pro/remote/codes`, 10 minutes, single use, kept hashed), shows it as a QR
//!   code and link, and the phone trades it for a device token (`POST /r/v1/pair`; 30 days
//!   from the last use, kept hashed, revocable).
//! - The phone's dashboard is served on `RELAY_REMOTE_URL`'s host at `/app/` (`app`), and its
//!   API calls go to the same origin's `/r/api/v1/*` (`proxy`): checked against the role
//!   (`scope`), size and rate limited, then sent through the tunnel as one frame and run
//!   in-process by the PC's own router, which checks the role again.
//!
//! Bodies are never stored or logged. Changes and refusals are kept in `remote_audit` (the
//! method and route template, the status, the device's name and the time) for 30 days.

mod app;
pub(crate) mod limits;
mod offline;
#[cfg(test)]
mod offline_tests;
mod proxy;
pub mod scope;
mod store;
#[cfg(test)]
mod tests;
mod tunnel;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use base64::Engine as _;
use serde_json::{Value, json};

pub(crate) use offline::{cloud_changed, cloud_chat};
pub(crate) use store::{SCHEMA, SCHEMA_SOURCE};

use crate::billing::gate::require_pro;
use crate::quota::Limit;
use crate::{ApiError, AppState, authed, now_ms};
use scope::Role;

const DAY_MS: i64 = 86_400_000;
/// A pairing code works once, for this long.
pub const CODE_TTL_MS: i64 = 10 * 60_000;
/// A device token lasts this long after its last use.
pub const TOKEN_TTL_MS: i64 = 30 * DAY_MS;
/// The expiry slides at most once a minute (one write, not one per request).
const TOUCH_EVERY_MS: i64 = 60_000;
/// Owner devices (the streamer's own phones and browsers) per PC, codes not yet used included.
pub const MAX_OWNER_DEVICES: i64 = 5;
/// Mods per PC, codes not yet used included.
pub const MAX_MOD_DEVICES: i64 = 10;
/// Pairing tries per address (IPv6: per /64) in [`PAIR_WINDOW_MS`], as a token bucket: five
/// at once, then one every two minutes.
pub const PAIR_TRIES: u32 = 5;
pub const PAIR_WINDOW_MS: i64 = 10 * 60_000;
/// Codes an install may make per hour.
pub const CODES_PER_HOUR: u32 = 30;
/// The largest request body a phone may send.
pub const MAX_BODY: usize = 2 * 1024 * 1024;
/// The largest answer the PC sends back through the tunnel.
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// Requests (and live socket opens) per device per second.
pub const DEVICE_RATE: u32 = 20;
/// Live sockets one device may have open (tabs); a newer one closes the oldest.
pub const MAX_LIVE_PER_DEVICE: usize = 3;
/// A live socket's ticket (`POST /r/v1/ticket {for:"live"}`): single use, this long.
pub const LIVE_TICKET_TTL_MS: i64 = 60_000;
/// A media ticket (`{for:"media"}`): `GET /media/{id}` only, reusable (video seeks), this long.
pub const MEDIA_TICKET_TTL_MS: i64 = 5 * 60_000;
/// Live tickets waiting to be used, relay-wide.
const MAX_TICKETS: usize = 50_000;
/// How long a request waits for the PC.
pub const PC_TIMEOUT_MS: u64 = 15_000;
/// Requests waiting on one PC at once: the PC answers 32 at once (`engine/src/remote.rs`
/// `IN_FLIGHT`), so more would only wait there.
pub const MAX_IN_FLIGHT: usize = 32;
/// Audit lines are kept this long.
pub const AUDIT_KEEP_MS: i64 = 30 * DAY_MS;
/// Device names, in characters.
pub const MAX_NAME: usize = 40;
/// Alphabet without look-alikes (no 0/O, 1/I/L), so a code can be typed from a screen.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LEN: usize = 10;

/// Settings for this feature, from the environment.
#[derive(Clone, Default)]
pub struct RemoteConfig {
    /// `RELAY_REMOTE_URL`'s origin (`https://remote.clearcraftlabs.com`): the phone's page.
    pub origin: Option<String>,
    /// Keys the stored token and code hashes: from `RELAY_REMOTE_PEPPER`, else derived from
    /// `RELAY_VAULT_KEY`. Never logged.
    pepper: Option<[u8; 32]>,
    /// `RELAY_DASHBOARD_DIR`: the dashboard build (`ferrocast-web/dist/dashboard`).
    pub dashboard_dir: Option<PathBuf>,
}

impl std::fmt::Debug for RemoteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteConfig")
            .field("origin", &self.origin)
            .field("pepper", &self.pepper.map(|_| "<redacted>"))
            .field("dashboard_dir", &self.dashboard_dir)
            .finish()
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let Ok(mut mac) = Hmac::<rsa::sha2::Sha256>::new_from_slice(key) else { return [0; 32] };
    mac.update(data);
    mac.finalize().into_bytes().into()
}

impl RemoteConfig {
    /// `origin`: `RELAY_REMOTE_URL` (only its origin counts); `pepper`: any secret bytes.
    pub fn new(origin: Option<&str>, pepper: Option<&[u8]>, dashboard_dir: Option<PathBuf>) -> RemoteConfig {
        RemoteConfig {
            origin: origin
                .and_then(|u| reqwest::Url::parse(u).ok())
                .map(|u| u.origin().ascii_serialization())
                .filter(|o| o != "null"),
            pepper: pepper.filter(|p| p.len() >= 16).map(|p| hmac(p, b"ferrocast remote pepper v1")),
            dashboard_dir,
        }
    }

    pub fn from_env() -> RemoteConfig {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let pepper: Option<Vec<u8>> = env("RELAY_REMOTE_PEPPER")
            .map(String::into_bytes)
            .or_else(|| env("RELAY_VAULT_KEY").and_then(|k| base64::engine::general_purpose::STANDARD.decode(k).ok()));
        RemoteConfig::new(
            env("RELAY_REMOTE_URL").as_deref(),
            pepper.as_deref(),
            env("RELAY_DASHBOARD_DIR").map(PathBuf::from),
        )
    }

    /// The phone page's host (`remote.clearcraftlabs.com`, with a port if it has one).
    fn host(&self) -> Option<&str> {
        self.origin.as_deref().and_then(|o| o.split_once("://")).map(|(_, h)| h)
    }

    /// Hex HMAC-SHA256 of a token or code with the pepper.
    fn hash(&self, secret: &str) -> Option<String> {
        self.pepper.map(|p| hmac(&p, secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The feature's in-memory parts: open tunnels, open phone sockets, rate limits.
#[derive(Debug, Default)]
pub struct Runtime {
    pub(crate) cfg: RemoteConfig,
    tunnels: Mutex<HashMap<String, Arc<tunnel::Tunnel>>>,
    phones: Mutex<HashMap<String, Vec<proxy::Phone>>>,
    /// The installs with open phone sockets per subscription (for the cloud's offline frames,
    /// so a chat line doesn't scan every phone on the relay). Added on open, cleaned lazily
    /// when a look finds the install's phones gone; when both are held, `phones` is locked first.
    phone_subs: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    limits: Mutex<limits::Limits>,
    /// Unused live tickets: hash → (device id, expiry).
    tickets: Mutex<HashMap<String, (String, i64)>>,
    next: AtomicU64,
    /// How long a request waits for the PC, in ms (0: [`PC_TIMEOUT_MS`]); tests shorten it.
    timeout_ms: AtomicU64,
}

impl Runtime {
    pub fn new(cfg: RemoteConfig) -> Runtime {
        Runtime { cfg, ..Default::default() }
    }

    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Counts one try; `false` once `key` used up its `max` per `window_ms` (token bucket).
    fn allow(&self, kind: &'static str, key: &str, max: u32, window_ms: i64) -> bool {
        self.take(kind, key, max, window_ms).is_ok()
    }

    /// Like [`Runtime::allow`], with the seconds until the next try on a refusal.
    fn take(&self, kind: &'static str, key: &str, max: u32, window_ms: i64) -> Result<(), u64> {
        self.limits.lock().unwrap_or_else(|p| p.into_inner()).take(kind, key, max, window_ms, now_ms())
    }

    /// A single-use live ticket for `device_id` (kept hashed, 60 s); `None` when too many wait.
    fn new_live_ticket(&self, device_id: &str) -> Option<String> {
        let ticket = format!("lt_{}", crate::random(24));
        let hash = self.cfg.hash(&ticket)?;
        let now = now_ms();
        let mut m = self.tickets.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= MAX_TICKETS {
            m.retain(|_, (_, exp)| *exp > now);
            if m.len() >= MAX_TICKETS {
                return None;
            }
        }
        m.insert(hash, (device_id.to_string(), now + LIVE_TICKET_TTL_MS));
        Some(ticket)
    }

    /// Uses a live ticket up: the device it was for, if it's known and not expired.
    fn take_live_ticket(&self, ticket: &str) -> Option<String> {
        let hash = self.cfg.hash(ticket)?;
        let (device, exp) = self.tickets.lock().unwrap_or_else(|p| p.into_inner()).remove(&hash)?;
        (exp > now_ms()).then_some(device)
    }

    /// A media ticket: `mt1.<device id>.<expiry ms>.<signature>`, signed with the pepper, so
    /// nothing is kept for it. Reusable until it expires (a video asks for many ranges).
    fn new_media_ticket(&self, device_id: &str) -> Option<String> {
        let exp = now_ms() + MEDIA_TICKET_TTL_MS;
        Some(format!("mt1.{device_id}.{exp}.{}", self.media_sig(device_id, exp)?))
    }

    fn media_sig(&self, device_id: &str, exp: i64) -> Option<String> {
        let h = self.cfg.hash(&format!("media ticket v1|{device_id}|{exp}"))?;
        Some(h[..32].to_string())
    }

    /// The device a media ticket is for, if its signature holds and it hasn't expired.
    fn check_media_ticket(&self, ticket: &str) -> Option<String> {
        use subtle::ConstantTimeEq;
        let mut parts = ticket.strip_prefix("mt1.")?.splitn(3, '.');
        let (device, exp, sig) = (parts.next()?, parts.next()?, parts.next()?);
        let exp: i64 = exp.parse().ok()?;
        let want = self.media_sig(device, exp)?;
        let ok = bool::from(want.as_bytes().ct_eq(sig.as_bytes()));
        (ok && exp > now_ms() && device.starts_with("dv_")).then(|| device.to_string())
    }

    fn pc_timeout(&self) -> std::time::Duration {
        let ms = self.timeout_ms.load(Ordering::Relaxed);
        std::time::Duration::from_millis(if ms == 0 { PC_TIMEOUT_MS } else { ms })
    }

    #[cfg(test)]
    pub(crate) fn set_pc_timeout_ms(&self, ms: u64) {
        self.timeout_ms.store(ms, Ordering::Relaxed);
    }

    fn tunnel(&self, install: &str) -> Option<Arc<tunnel::Tunnel>> {
        self.tunnels.lock().unwrap_or_else(|p| p.into_inner()).get(install).cloned()
    }

    /// Whether a PC's tunnel is open (the phone strip's "PC online").
    pub fn pc_online(&self, install: &str) -> bool {
        self.tunnel(install).is_some()
    }
}

/// 503 `remote_unavailable`: this relay has no remote host or no pepper set.
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "remote_unavailable",
        "The dashboard from anywhere isn't set up on this relay yet.",
    )
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // The desktop's side.
        .route("/v1/remote/tunnel", get(tunnel::connect))
        .route("/v1/pro/remote/codes", post(create_code))
        .route("/v1/pro/remote/devices", get(list_devices))
        .route("/v1/pro/remote/devices/{id}", delete(delete_device))
        // The phone's side (on the remote host only).
        .route("/r/v1/pair", post(proxy::pair))
        .route("/r/v1/me", get(proxy::me))
        .route("/r/v1/forget", post(proxy::forget))
        .route("/r/v1/ticket", post(proxy::ticket))
        .route("/r/api/v1/live", get(proxy::live))
        .route("/r/api/v1/{*path}", any(proxy::api))
        .route("/app", get(app::app_root))
        .route("/app/", get(app::index))
        .route("/app/{*path}", get(app::file))
}

/// `features.remote`: a remote host and a pepper are set.
pub(crate) fn available(st: &AppState) -> bool {
    st.remote.cfg.origin.is_some() && st.remote.cfg.pepper.is_some()
}

/// Request rates, not quotas a streamer uses up: nothing to list in `/v1/pro/status`.
pub(crate) const QUOTAS: &[Limit] = &[];

/// A device name as stored: control and direction characters out, trimmed, at most 40.
fn clean_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_NAME).collect()
}

fn new_code() -> String {
    let mut b = [0u8; CODE_LEN];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    // 31 symbols: 256 % 31 = 8, so drop the top values to keep every symbol equally likely.
    let mut out = String::with_capacity(CODE_LEN);
    let mut i = 0;
    while out.len() < CODE_LEN {
        let v = if i < b.len() {
            b[i]
        } else {
            let mut x = [0u8; 1];
            #[allow(clippy::expect_used)]
            getrandom::fill(&mut x).expect("the OS random number generator is unavailable");
            x[0]
        };
        i += 1;
        if usize::from(v) < 248 {
            out.push(char::from(CODE_ALPHABET[usize::from(v) % CODE_ALPHABET.len()]));
        }
    }
    out
}

/// Normalises a typed or scanned code: upper case, no spaces or dashes.
fn normal_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn json_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    let ct = headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.starts_with("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
            "Send JSON (Content-Type: application/json).",
        ));
    }
    serde_json::from_slice(body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid", "The body isn't valid JSON."))
}

fn limit_for(role: Role) -> i64 {
    match role {
        Role::Owner => MAX_OWNER_DEVICES,
        Role::Mod => MAX_MOD_DEVICES,
    }
}

/// Tells the PC how many devices and codes it has (it closes the tunnel at none).
fn tell_counts(st: &AppState, install: &str) {
    if let Some(t) = st.remote.tunnel(install)
        && let Ok((devices, codes)) = st.db.remote_totals(install, now_ms())
    {
        t.send(&json!({ "t": "devices", "devices": devices, "codes": codes }));
    }
}

/// `POST /v1/pro/remote/codes {role, name?}` → 201 `{code, url, expires_at, expires_in, role, name}`.
async fn create_code(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let pro = require_pro(&st, &headers).await?;
    if !available(&st) {
        return Err(unavailable());
    }
    let v = json_body(&headers, &body)?;
    let role = v["role"].as_str().and_then(Role::parse).ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", "role: \"owner\" (your own phone) or \"mod\".")
    })?;
    let name = match &v["name"] {
        Value::Null => String::new(),
        Value::String(s) => clean_name(s),
        _ => return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid", "name: text, up to 40 characters.")),
    };
    let name = if name.is_empty() {
        match role {
            Role::Owner => "My phone".to_string(),
            Role::Mod => "Mod".to_string(),
        }
    } else {
        name
    };
    let now = now_ms();
    let max = limit_for(role);
    if let Err(wait) = st.remote.take("codes", &pro.install, CODES_PER_HOUR, 3_600_000) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "That's a lot of links in an hour.")
            .hint("Wait a few minutes, then make a new one.")
            .header(axum::http::header::RETRY_AFTER, wait.to_string()));
    }
    let code = new_code();
    let hash = st.remote.cfg.hash(&code).ok_or_else(unavailable)?;
    let expires_at = now + CODE_TTL_MS;
    // Counted and added in one statement: two clicks at once can't both pass the limit.
    let added = st
        .db
        .remote_add_code_capped(&hash, &pro.install, &pro.sub_id, role.as_str(), &name, now, expires_at, max)
        .map_err(ApiError::db)?;
    if !added {
        let what = match role {
            Role::Owner => format!("{max} of your own devices"),
            Role::Mod => format!("{max} mods"),
        };
        return Err(ApiError::new(StatusCode::CONFLICT, "too_many_devices", format!("This PC already has {what}."))
            .hint(
                "Remove one in Settings › Dashboard anywhere first. Unused links count until they expire (10 minutes).",
            ));
    }
    tell_counts(&st, &pro.install);
    let origin = st.remote.cfg.origin.clone().unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "code": code,
            // In the fragment: never sent to a server, so it can't end up in a log.
            "url": format!("{origin}/app/#pair={code}"),
            "expires_at": expires_at,
            // Seconds left from now: the desktop counts down from when it got this, so its
            // clock being off doesn't matter.
            "expires_in": CODE_TTL_MS / 1000,
            "role": role.as_str(),
            "name": name,
        })),
    )
        .into_response())
}

fn device_json(d: &store::Device) -> Value {
    json!({
        "id": d.id, "name": d.name, "role": d.role,
        "created_at": d.created_at, "last_used_at": d.last_used_at, "expires_at": d.expires_at,
    })
}

/// `GET /v1/pro/remote/devices` → `{devices, audit, limits, pc_online}`. Install auth only:
/// a device can always be seen and removed, Pro or not.
async fn list_devices(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let now = now_ms();
    let devices = st.db.remote_devices(&install, now).map_err(ApiError::db)?;
    let audit = st.db.remote_audit_list(&install, 50).map_err(ApiError::db)?;
    let mut res = axum::Json(json!({
        "devices": devices.iter().map(device_json).collect::<Vec<_>>(),
        "audit": audit,
        "limits": { "owner": MAX_OWNER_DEVICES, "mod": MAX_MOD_DEVICES },
        "pc_online": st.remote.pc_online(&install),
    }))
    .into_response();
    res.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    Ok(res)
}

/// `DELETE /v1/pro/remote/devices/{id}` → 204. Its token stops working at once and its
/// open sockets close.
async fn delete_device(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let found = st.db.remote_device(&id).map_err(ApiError::db)?.filter(|d| d.install_id == install);
    let Some(d) = found else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "That device isn't paired with this PC."));
    };
    st.db.remote_delete_device(&d.id, Some(&install)).map_err(ApiError::db)?;
    let _ = st.db.remote_audit(&d.sub_id, &install, &d.id, &d.name, "REVOKE device", 204, now_ms());
    proxy::close_device(&st, &install, &d.id);
    tell_counts(&st, &install);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The hourly housekeeping: expired devices and codes, audit lines past 30 days.
pub async fn sweep(st: &AppState) {
    let now = now_ms();
    match st.db.remote_prune(now, now - AUDIT_KEEP_MS) {
        Ok(installs) => {
            for i in installs {
                tell_counts(st, &i);
            }
        }
        Err(e) => tracing::warn!(error = %e, "remote housekeeping failed"),
    }
}
