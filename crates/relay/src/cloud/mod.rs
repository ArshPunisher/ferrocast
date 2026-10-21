//! Cloud mode (docs/CLOUD.md): Ferrocast running from the relay for a signed-in account,
//! with no desktop app needed. Free with an account.
//!
//! - `store`: the tables (documents, overlay URLs, channels, media, connect finishes).
//! - `docs`: overlays, alert rules, designs and settings per account, checked with the same
//!   core validators the engine uses.
//! - `web`: the web dashboard's `/c/…` routes (cookie + CSRF, the engine's paths).
//! - `sync`: the desktop app's `/v1/cloud/…` routes (its `fca_` session).
//!
//! Nothing here runs for an account that never turned cloud mode on, and the desktop app
//! never needs any of it (R-01).

// The parts the event pipeline, media and the sleep sweep use land with them (CLOUD.md §12).
#![allow(dead_code)]

pub(crate) mod connect;
#[cfg(test)]
mod connect_tests;
pub(crate) mod docs;
pub(crate) mod store;
mod sync;
#[cfg(test)]
mod tests;
mod web;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrocast_core::design::Design;
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::Value;

pub(crate) use store::SCHEMA;

use crate::accounts::{self, Authed};
use crate::{ApiError, AppState, now_ms};

/// Uploads an account may keep (docs/CLOUD.md §5).
pub const STORAGE_LIMIT: i64 = 200 * 1_000_000;
/// One uploaded file.
pub const FILE_LIMIT: i64 = 20 * 1_000_000;
/// Documents per account.
pub const MAX_OVERLAYS: i64 = 50;
pub const MAX_RULES: i64 = 200;
pub const MAX_DESIGNS: i64 = 100;
/// One stored document.
pub const MAX_DOC_BYTES: usize = 256 * 1024;
/// Dashboard requests per account per minute, and writes among them.
pub const REQUESTS_PER_MINUTE: u32 = 120;
pub const WRITES_PER_MINUTE: u32 = 60;
/// An account not used for this long sleeps (ingest stops until the next sign-in).
pub const SLEEP_AFTER_MS: i64 = 30 * 86_400_000;
/// Event history kept (counts and labels; chat is never stored).
pub const HISTORY_MS: i64 = 30 * 86_400_000;

/// Cloud mode's settings (`CloudConfig::from_env()` in production).
#[derive(Clone, Default)]
pub struct CloudConfig {
    /// `RELAY_OVERLAY_URL`'s origin; `None`: overlays are on the API host (`RELAY_PUBLIC_URL`).
    pub overlay_origin: Option<String>,
    /// `RELAY_OVERLAYS_DIR`: the overlay build (`ferrocast-web/dist/overlays`).
    pub overlays_dir: Option<PathBuf>,
    /// Keys overlay URLs; derived from `RELAY_VAULT_KEY`. Cloud mode is off without it.
    key: Option<[u8; 32]>,
    /// Accounts whose event bus may be in memory at once (`CLOUD_MAX_ACTIVE`).
    pub max_active: usize,
    /// Overlay sockets open relay-wide (`CLOUD_MAX_SOCKETS`).
    pub max_sockets: usize,
}

impl std::fmt::Debug for CloudConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudConfig")
            .field("overlay_origin", &self.overlay_origin)
            .field("overlays_dir", &self.overlays_dir)
            .field("key", &self.key.map(|_| "<redacted>"))
            .field("max_active", &self.max_active)
            .field("max_sockets", &self.max_sockets)
            .finish()
    }
}

pub const DEFAULT_MAX_ACTIVE: usize = 400;
pub const DEFAULT_MAX_SOCKETS: usize = 4_000;

/// HKDF-style key for a label from a secret (HMAC-SHA256 extract + one expand step).
fn derive(secret: &[u8], label: &str) -> [u8; 32] {
    crate::bot::derive_key(secret, label)
}

impl CloudConfig {
    /// `secret`: any secret bytes (the decoded vault key); `None` keeps cloud mode off.
    pub fn new(overlay_origin: Option<&str>, overlays_dir: Option<PathBuf>, secret: Option<&[u8]>) -> CloudConfig {
        CloudConfig {
            overlay_origin: overlay_origin
                .and_then(|u| url::Url::parse(u).ok())
                .map(|u| u.origin().ascii_serialization())
                .filter(|o| o != "null"),
            overlays_dir,
            key: secret.filter(|s| s.len() >= 16).map(|s| derive(s, "ferrocast cloud overlay v1")),
            max_active: DEFAULT_MAX_ACTIVE,
            max_sockets: DEFAULT_MAX_SOCKETS,
        }
    }

    pub fn from_env() -> CloudConfig {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let secret = env("RELAY_VAULT_KEY")
            .and_then(|k| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, k).ok());
        let mut c = CloudConfig::new(
            env("RELAY_OVERLAY_URL").as_deref(),
            env("RELAY_OVERLAYS_DIR").map(PathBuf::from),
            secret.as_deref(),
        );
        if let Some(n) = env("CLOUD_MAX_ACTIVE").and_then(|v| v.parse::<usize>().ok()) {
            c.max_active = n.clamp(1, 100_000);
        }
        if let Some(n) = env("CLOUD_MAX_SOCKETS").and_then(|v| v.parse::<usize>().ok()) {
            c.max_sockets = n.clamp(1, 1_000_000);
        }
        c
    }

    /// Whether overlay URLs can be made (the key is set).
    pub fn keyed(&self) -> bool {
        self.key.is_some()
    }
}

/// Cloud mode's in-memory parts.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    pub(crate) cfg: CloudConfig,
    /// The built-in design catalogue (read from the overlay build once).
    builtins: RwLock<Option<Arc<Vec<Design>>>>,
}

impl Runtime {
    pub(crate) fn new(cfg: CloudConfig) -> Runtime {
        Runtime { cfg, ..Default::default() }
    }

    /// The built-in designs (`designs/catalog.json` in the overlay build); none without one.
    pub(crate) fn builtins(&self) -> Arc<Vec<Design>> {
        if let Some(b) = self.builtins.read().unwrap_or_else(|p| p.into_inner()).as_ref() {
            return b.clone();
        }
        let list = self
            .cfg
            .overlays_dir
            .as_ref()
            .map_or_else(Vec::new, |d| docs::load_catalog(&d.join("designs/catalog.json")));
        let list = Arc::new(list);
        *self.builtins.write().unwrap_or_else(|p| p.into_inner()) = Some(list.clone());
        list
    }

    /// Tests: a catalogue without a build folder.
    #[cfg(test)]
    pub(crate) fn set_builtins(&self, list: Vec<Design>) {
        *self.builtins.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(docs::checked_builtins(list)));
    }
}

pub(crate) fn routes() -> axum::Router<AppState> {
    web::routes().merge(sync::routes()).merge(connect::routes())
}

// ── Errors ───────────────────────────────────────────────────────────────────────────

pub(crate) fn e(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn off() -> ApiError {
    e(StatusCode::NOT_FOUND, "not_found", "Cloud mode isn't set up on this server.")
        .hint("Use the Ferrocast app on your PC for now.")
}

pub(crate) fn not_found(what: &str) -> ApiError {
    e(StatusCode::NOT_FOUND, "not_found", format!("No {what} with that id."))
}

pub(crate) fn invalid(message: impl Into<String>) -> ApiError {
    e(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

fn cloud_off() -> ApiError {
    e(StatusCode::CONFLICT, "cloud_off", "Cloud mode is off for this account.")
        .hint("Turn on cloud mode on the Cloud page first. It's free.")
}

pub(crate) fn limit(message: impl Into<String>) -> ApiError {
    e(StatusCode::CONFLICT, "cloud_limit", message)
}

fn rate_limited(wait: i64) -> ApiError {
    e(StatusCode::TOO_MANY_REQUESTS, "cloud_rate", "That's a lot of changes in a minute.")
        .hint(format!("Wait {wait} seconds and try again."))
        .header(header::RETRY_AFTER, wait.to_string())
}

pub(crate) fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub(crate) fn json(v: Value) -> Response {
    no_store(axum::Json(v).into_response())
}

pub(crate) fn created(v: Value) -> Response {
    no_store((StatusCode::CREATED, axum::Json(v)).into_response())
}

/// A JSON object body (`Content-Type: application/json`, or none for an empty body).
pub(crate) fn body(headers: &HeaderMap, bytes: &[u8]) -> Result<Value, ApiError> {
    if let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        && !ct.trim_start().to_ascii_lowercase().starts_with("application/json")
    {
        return Err(e(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the request as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice::<Value>(bytes).ok().filter(Value::is_object).ok_or_else(|| {
        e(StatusCode::BAD_REQUEST, "bad_json", "The request body isn't a JSON object.")
            .hint("Send a JSON object, like {\"name\": \"Main\"}.")
    })
}

/// A typed body: 422 naming the wrong field.
pub(crate) fn typed<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, ApiError> {
    serde_json::from_value(v).map_err(|err| invalid(format!("A field is missing or wrong: {err}")))
}

// ── Who's asking ─────────────────────────────────────────────────────────────────────

/// Cloud mode can run here: accounts are on (the web host is set) and overlay URLs can be
/// keyed (the vault key).
pub(crate) fn available(st: &AppState) -> bool {
    accounts::available(st) && st.cloud.cfg.keyed()
}

/// Where overlay pages are served: `RELAY_OVERLAY_URL`, else the API host. Never the web
/// host (it carries the session cookie).
pub(crate) fn overlay_origin(st: &AppState) -> String {
    st.cloud.cfg.overlay_origin.clone().unwrap_or_else(|| st.cfg.public_url.clone())
}

/// A signed-in account on the web host: the cookie, the CSRF token on writes, the
/// per-account rates. `need_on`: cloud mode must be on (409 `cloud_off` otherwise).
pub(crate) fn web_account(
    st: &AppState,
    headers: &HeaderMap,
    method: &Method,
    need_on: bool,
) -> Result<Authed, ApiError> {
    if !available(st) {
        return Err(off());
    }
    // `/c/…` lives on the web host only (docs/CLOUD.md §10): elsewhere it doesn't exist.
    if !crate::remote::on_web_host(st, headers) {
        return Err(e(StatusCode::NOT_FOUND, "not_found", "There's nothing here.")
            .hint("Open the dashboard on the Ferrocast web address."));
    }
    let write = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let a = if write { accounts::web_session_write(st, headers)? } else { accounts::web_session(st, headers)? };
    rate(st, &a.account.id, write)?;
    if need_on {
        must_be_on(st, &a.account.id)?;
    }
    Ok(a)
}

/// The desktop app's session (`Authorization: Bearer fca_…`), never the cookie.
pub(crate) fn app_account(st: &AppState, headers: &HeaderMap, method: &Method) -> Result<Authed, ApiError> {
    if !available(st) {
        return Err(off());
    }
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !bearer.starts_with("Bearer fca_") {
        return Err(accounts::signed_out());
    }
    let a = accounts::session_auth(st, headers, method)?;
    let write = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    rate(st, &a.account.id, write)?;
    Ok(a)
}

fn rate(st: &AppState, account: &str, write: bool) -> Result<(), ApiError> {
    st.accounts.take("cloud", account, REQUESTS_PER_MINUTE, 60_000).map_err(rate_limited)?;
    if write {
        st.accounts.take("cloud_write", account, WRITES_PER_MINUTE, 60_000).map_err(rate_limited)?;
    }
    Ok(())
}

/// Cloud mode is on for the account (and this use wakes it if it slept).
pub(crate) fn must_be_on(st: &AppState, account: &str) -> Result<store::CloudAccount, ApiError> {
    let c = st.db.cloud_account(account).map_err(ApiError::db)?.ok_or_else(cloud_off)?;
    if c.sleeping_at.is_some() || now_ms() - c.last_active_at > 60_000 {
        let _ = st.db.cloud_touch(account, now_ms());
    }
    Ok(c)
}

// ── Overlay URLs ─────────────────────────────────────────────────────────────────────

/// An overlay's token for a generation: 40 hex characters, keyed (docs/CLOUD.md §3).
pub(crate) fn token_for(st: &AppState, account: &str, overlay: &str, generation: i64) -> Option<String> {
    let key = st.cloud.cfg.key?;
    let mut m = Hmac::<Sha256>::new_from_slice(&key).ok()?;
    m.update(format!("overlay|{account}|{overlay}|{generation}").as_bytes());
    Some(m.finalize().into_bytes().iter().take(20).map(|b| format!("{b:02x}")).collect())
}

/// The URL of an overlay, making its token on first use.
pub(crate) fn overlay_url(st: &AppState, account: &str, overlay: &str) -> Result<String, ApiError> {
    let generation = match st.db.cloud_token_generation(account, overlay).map_err(ApiError::db)? {
        Some(g) => g,
        None => {
            let token = token_for(st, account, overlay, 1).ok_or_else(off)?;
            st.db.cloud_set_token(account, overlay, &accounts::sha(&token), 1).map_err(ApiError::db)?;
            1
        }
    };
    let token = token_for(st, account, overlay, generation).ok_or_else(off)?;
    Ok(format!("{}/o/{token}/", overlay_origin(st)))
}

/// A new URL for an overlay: the old one stops at once.
pub(crate) fn rotate_url(st: &AppState, account: &str, overlay: &str) -> Result<String, ApiError> {
    let next = st.db.cloud_token_generation(account, overlay).map_err(ApiError::db)?.unwrap_or(0) + 1;
    let token = token_for(st, account, overlay, next).ok_or_else(off)?;
    st.db.cloud_set_token(account, overlay, &accounts::sha(&token), next).map_err(ApiError::db)?;
    Ok(format!("{}/o/{token}/", overlay_origin(st)))
}

/// Stops an overlay's URL (deleted): the generation moves on, so a later overlay with the
/// same id (a sync can bring one back) never revives an old URL. The new one is shown to
/// no one, and it opens nothing while the overlay stays deleted.
pub(crate) fn retire_url(st: &AppState, account: &str, overlay: &str) -> Result<(), ApiError> {
    if st.db.cloud_token_generation(account, overlay).map_err(ApiError::db)?.is_none() {
        return Ok(());
    }
    rotate_url(st, account, overlay).map(|_| ())
}

/// `(account, overlay)` for a token from an overlay URL.
pub(crate) fn overlay_for_token(st: &AppState, token: &str) -> Option<(String, String)> {
    if token.len() != 40 || !token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    st.db.cloud_token(&accounts::sha(token)).ok().flatten()
}

// ── The cloud bot's gate (docs/CLOUD.md §6) ──────────────────────────────────────────

/// Whether `key` is an account id (the cloud bot's key for a cloud account) rather than a
/// Pro subscription id.
pub(crate) fn is_account_key(key: &str) -> bool {
    key.starts_with("ac_")
}

/// The cloud bot may run for an account key: cloud mode on, awake, account not disabled.
pub(crate) fn bot_allowed(db: &crate::Db, account: &str) -> bool {
    let on = matches!(db.cloud_account(account), Ok(Some(c)) if c.sleeping_at.is_none());
    on && matches!(db.account(account), Ok(Some(a)) if a.disabled_at.is_none())
}

// ── Open overlay pages (filled by the bus) ───────────────────────────────────────────

/// An overlay's layout changed: its open pages re-render without reloading.
pub(crate) fn changed_overlay(st: &AppState, account: &str, o: &ferrocast_core::overlay::Overlay) {
    let _ = (st, account, o);
}

/// An overlay's URL was rotated or the overlay deleted: open pages on it stop at once.
pub(crate) fn revoked_overlay(st: &AppState, account: &str, overlay: &str) {
    let _ = (st, account, overlay);
}

// ── The hourly sweep ─────────────────────────────────────────────────────────────────

/// Once an hour: connect hand-overs that were never finished (their sign-ins revoked), old
/// tombstones, and channels whose subscriptions failed (tried again).
pub async fn sweep(st: &AppState) {
    if !available(st) {
        return;
    }
    connect::sweep_pending(st).await;
    let now = now_ms();
    if let Err(err) = st.db.cloud_prune_tombstones(now - store::TOMBSTONE_KEEP_MS) {
        tracing::warn!(error = %err, "couldn't prune cloud tombstones");
    }
    for ch in st.db.cloud_all_channels().unwrap_or_default() {
        // Only what may work now: a platform that was unreachable, or no subscriptions and no
        // error. A sign-in the platform refused waits for the streamer to connect again.
        let code = ch.error.as_deref().and_then(|e| serde_json::from_str::<Value>(e).ok()).map(|e| e["code"].clone());
        let retry = match code.as_ref().and_then(Value::as_str) {
            Some(c) => c.ends_with("_unavailable"),
            None => ch.event_subs.is_empty(),
        };
        if retry {
            let _ = connect::subscribe(st, &ch).await;
        }
    }
}
