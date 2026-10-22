//! Ferrocast accounts (docs/ACCOUNTS.md): Google or email-link sign-in, sessions for the app
//! (`fca_…` bearer) and the web dashboard (`__Host-fc_session` cookie + CSRF), Pro that
//! belongs to an account, and the Control Center's account tabs (`admin`).
//!
//! The app works without an account, as before; nothing here changes an install that never
//! signs in. Accounts are served only when the web host (`RELAY_REMOTE_URL`) is set.

pub(crate) mod admin;
pub mod google;
mod login_api;
pub(crate) mod pages;
pub(crate) mod store;
#[cfg(test)]
mod tests;
mod web;

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

pub use google::GoogleConfig;
pub(crate) use store::SCHEMA;
use store::{Account, Session};

use crate::billing::access;
use crate::{ApiError, AppState, authed, now_ms};

const MINUTE: i64 = 60_000;
const DAY: i64 = 86_400_000;
/// Web sessions: 30 days from the last use, 90 days at most.
pub const WEB_TTL_MS: i64 = 30 * DAY;
pub const WEB_MAX_MS: i64 = 90 * DAY;
/// App sessions: 180 days from the last use.
pub const APP_TTL_MS: i64 = 180 * DAY;
const APP_MAX_MS: i64 = 3650 * DAY;
/// Expiry slides at most this often (one write, not one per request).
const TOUCH_EVERY_MS: i64 = MINUTE;
/// An app sign-in, from start to the browser coming back.
pub const FLOW_TTL_MS: i64 = 10 * MINUTE;
/// The one-time code the app trades for its session.
pub const APP_CODE_TTL_MS: i64 = 2 * MINUTE;
/// Email links and codes.
pub const MAGIC_TTL_MS: i64 = 15 * MINUTE;
pub const MAGIC_TRIES: i64 = 5;
/// A Google round trip.
pub const OAUTH_TTL_MS: i64 = 10 * MINUTE;
/// The login browser cookie.
pub const LOGIN_COOKIE_S: i64 = 30 * 60;

pub const SESSION_COOKIE: &str = "__Host-fc_session";
pub const LOGIN_COOKIE: &str = "__Host-fc_login";

/// Whether the web dashboard needs a signed-in account (`RELAY_REMOTE_LOGIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteLogin {
    /// Production default: pairing and every device call need the account cookie.
    Required,
    /// Rollout escape hatch and tests: phones work without an account (as in 1.2.0).
    #[default]
    Optional,
}

/// Accounts' settings.
#[derive(Debug, Clone, Default)]
pub struct AccountsConfig {
    pub google: GoogleConfig,
    pub remote_login: RemoteLogin,
}

impl AccountsConfig {
    pub fn from_env() -> AccountsConfig {
        let login = std::env::var("RELAY_REMOTE_LOGIN").ok().map(|v| v.trim().to_ascii_lowercase());
        AccountsConfig {
            google: GoogleConfig::from_env(),
            remote_login: if login.as_deref() == Some("optional") {
                RemoteLogin::Optional
            } else {
                RemoteLogin::Required
            },
        }
    }
}

type Limits = HashMap<(&'static str, String), (i64, u32)>;

/// Accounts' in-memory parts: settings, Google's keys, rate limits.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    pub(crate) cfg: AccountsConfig,
    keys: google::KeyCache,
    limits: Mutex<Limits>,
}

impl Runtime {
    pub(crate) fn new(cfg: AccountsConfig) -> Runtime {
        Runtime { cfg, ..Default::default() }
    }

    /// Counts one try against `max` per `window_ms` (fixed windows); `Err(seconds to wait)`.
    pub(crate) fn take(&self, kind: &'static str, key: &str, max: u32, window_ms: i64) -> Result<(), i64> {
        let now = now_ms();
        let window = now.div_euclid(window_ms.max(1));
        let mut m = self.limits.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 20_000 {
            m.retain(|_, (w, _)| *w >= window - 1);
        }
        let e = m.entry((kind, key.to_string())).or_insert((window, 0));
        if e.0 != window {
            *e = (window, 0);
        }
        if e.1 >= max {
            return Err(((window + 1) * window_ms - now).div_euclid(1000).max(1));
        }
        e.1 += 1;
        Ok(())
    }
}

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route("/v1/account", get(me))
        .route("/v1/account/logout", post(logout))
        .route("/v1/account/sessions", get(sessions))
        .route("/v1/account/sessions/{id}", delete(revoke_session))
        .route("/v1/account/app/start", post(app_start))
        .route("/v1/account/app/token", post(app_token))
        .merge(pages::routes())
        .merge(login_api::routes())
        .merge(web::routes())
        .merge(admin::routes())
}

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn e(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn off() -> ApiError {
    e(StatusCode::NOT_FOUND, "not_found", "Accounts aren't set up on this relay.")
}

pub(crate) fn signed_out() -> ApiError {
    e(StatusCode::UNAUTHORIZED, "signed_out", "You're signed out.").hint("Sign in again.")
}

pub(crate) fn disabled() -> ApiError {
    e(StatusCode::FORBIDDEN, "account_disabled", "This Ferrocast account is turned off.")
        .hint("Write to support@clearcraftlabs.com if you think that's a mistake. The app still works without it.")
}

fn csrf_refused() -> ApiError {
    e(StatusCode::FORBIDDEN, "csrf", "This request didn't come from the Ferrocast dashboard.")
        .hint("Reload the page and try again.")
}

pub(crate) fn login_required() -> ApiError {
    e(StatusCode::FORBIDDEN, "login_required", "Sign in to use the web dashboard.")
        .hint("Accounts are free. Open /login on this site.")
}

pub(crate) fn wrong_account() -> ApiError {
    e(StatusCode::FORBIDDEN, "wrong_account", "This device was paired by another Ferrocast account.")
        .hint("Sign in with that account, or pair this browser again from the streaming PC.")
}

fn rate_limited(wait: i64, message: &str) -> ApiError {
    e(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
        .hint(format!("Try again in {} minute{}.", (wait + 59) / 60, if wait <= 60 { "" } else { "s" }))
        .header(header::RETRY_AFTER, wait.to_string())
}

// ── Small helpers ────────────────────────────────────────────────────────────────────

pub(crate) fn available(st: &AppState) -> bool {
    st.remote.cfg.origin.is_some()
}

/// The web host's origin, where sign-in cookies and callbacks live: the API host
/// (`https://api.clearcraftlabs.com`), not the website (`page_origin`).
pub(crate) fn web_origin(st: &AppState) -> String {
    st.remote.cfg.origin.clone().unwrap_or_default()
}

/// Where people are sent (the dashboard, sign-in, email links): the website when it's set
/// (`RELAY_WEB_URL`), else the web host. Callbacks from Google and the platforms stay on
/// [`web_origin`], the host that holds the sign-in cookie.
pub(crate) fn page_origin(st: &AppState) -> String {
    st.remote.cfg.page_origin().unwrap_or_default().to_string()
}

pub(crate) fn sha(s: &str) -> String {
    crate::hash(s)
}

fn ct_eq(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// A cookie's value from the request's `Cookie` headers.
pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        (k == name && !v.is_empty() && v.len() <= 256).then(|| v.to_string())
    })
}

/// `Set-Cookie` for one of our `__Host-` cookies (`max_age` 0 clears it).
pub(crate) fn set_cookie(name: &str, value: &str, max_age_s: i64) -> HeaderValue {
    let v = format!("{name}={value}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax", max_age_s.max(0));
    HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static("__Host-x=; Path=/; Max-Age=0"))
}

/// A cross-site request is refused: `Origin`, when sent, must be the web host's, and
/// `Sec-Fetch-Site`, when sent, same-origin (or `none`: typed in the address bar).
/// Our pages send `Referrer-Policy: no-referrer`, so browsers post `Origin: null` from them:
/// then `Sec-Fetch-Site: same-origin` must vouch for the request instead.
pub(crate) fn same_origin(st: &AppState, headers: &HeaderMap) -> bool {
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    let origin_ok = match origin {
        None => true,
        Some("null") => site == Some("same-origin"),
        Some(o) => st.remote.cfg.trusted_origin(o),
    };
    // The website (another address on the same site) calls the web host across origins.
    let from_website = origin.is_some_and(|o| st.remote.cfg.web.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(o)));
    let site_ok = match site {
        None => true,
        Some("same-site") => from_website,
        Some(s) => matches!(s, "same-origin" | "none"),
    };
    origin_ok && site_ok
}

/// The CSRF token of a web session: derived from its secret, so nothing more is stored.
pub(crate) fn csrf_token(secret: &str) -> String {
    sha(&format!("ferrocast csrf v1|{secret}"))[..32].to_string()
}

/// The hidden form token of the login pages: derived from the login cookie.
pub(crate) fn form_token(login_cookie: &str) -> String {
    sha(&format!("ferrocast login form v1|{login_cookie}"))[..32].to_string()
}

/// `<session id>.<secret>`, with or without the app's `fca_` prefix.
fn parse_session(raw: &str) -> Option<(&str, &str)> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("fca_").unwrap_or(raw);
    let (id, secret) = raw.split_once('.')?;
    (id.starts_with("se_") && id.len() <= 64 && secret.len() >= 20 && secret.len() <= 100).then_some((id, secret))
}

/// "Chrome on macOS" from a User-Agent; the raw string is never kept.
pub(crate) fn ua_label(headers: &HeaderMap) -> String {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "A browser"
    };
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() { browser.to_string() } else { format!("{browser} on {os}") }
}

/// A label as stored: no control or direction characters, trimmed, at most 40.
pub(crate) fn clean_label(s: &str) -> String {
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
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(40).collect()
}

fn json_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    if let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        && !ct.trim_start().to_ascii_lowercase().starts_with("application/json")
    {
        return Err(e(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the request as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| e(StatusCode::BAD_REQUEST, "bad_json", "The request body isn't a JSON object."))
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

// ── Sessions ─────────────────────────────────────────────────────────────────────────

/// A signed-in request.
#[derive(Debug, Clone)]
pub(crate) struct Authed {
    pub session: Session,
    pub account: Account,
    /// The session's secret (the web CSRF token is derived from it).
    secret: String,
    pub web: bool,
}

/// Makes a session; its token (`<id>.<secret>`) and the stored row.
pub(crate) fn new_session(
    st: &AppState,
    account: &Account,
    kind: &str,
    label: &str,
    install: Option<&str>,
) -> Result<(String, Session), ApiError> {
    let id = store::new_id("se_");
    let secret = crate::random(32);
    let now = now_ms();
    let (ttl, max) = if kind == "web" { (WEB_TTL_MS, WEB_MAX_MS) } else { (APP_TTL_MS, APP_MAX_MS) };
    st.db
        .account_add_session(&id, &account.id, &sha(&secret), kind, label, install, now, ttl, max)
        .map_err(ApiError::db)?;
    let s = st.db.account_session(&id).map_err(ApiError::db)?.ok_or_else(signed_out)?;
    Ok((format!("{id}.{secret}"), s))
}

/// Checks a session token: known, hash equal, not revoked or expired, account not disabled.
fn check_session(st: &AppState, raw: &str, kind: &str) -> Result<(Session, Account, String), ApiError> {
    let (id, secret) = parse_session(raw).ok_or_else(signed_out)?;
    let s = st.db.account_session(id).map_err(ApiError::db)?.ok_or_else(signed_out)?;
    let now = now_ms();
    if !ct_eq(&s.token_hash, &sha(secret))
        || s.kind != kind
        || s.revoked_at.is_some()
        || s.expires_at <= now
        || s.max_expires_at <= now
    {
        return Err(signed_out());
    }
    let a = st.db.account(&s.account_id).map_err(ApiError::db)?.ok_or_else(signed_out)?;
    if a.disabled_at.is_some() {
        return Err(disabled());
    }
    if now - s.last_used_at >= TOUCH_EVERY_MS {
        let ttl = if kind == "web" { WEB_TTL_MS } else { APP_TTL_MS };
        let _ = st.db.account_touch(&s, now, ttl);
    }
    Ok((s, a, secret.to_string()))
}

/// The web session behind the cookie, on the web host only (no CSRF check: reads).
pub(crate) fn web_session(st: &AppState, headers: &HeaderMap) -> Result<Authed, ApiError> {
    if !available(st) || !crate::remote::on_web_host(st, headers) {
        return Err(signed_out());
    }
    let raw = cookie(headers, SESSION_COOKIE).ok_or_else(signed_out)?;
    let (session, account, secret) = check_session(st, &raw, "web")?;
    Ok(Authed { session, account, secret, web: true })
}

/// A web session for a state-changing request: also the CSRF token and the origin.
pub(crate) fn web_session_write(st: &AppState, headers: &HeaderMap) -> Result<Authed, ApiError> {
    let a = web_session(st, headers)?;
    check_csrf(st, headers, &a)?;
    Ok(a)
}

fn check_csrf(st: &AppState, headers: &HeaderMap, a: &Authed) -> Result<(), ApiError> {
    let given = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !same_origin(st, headers) || given.is_empty() || !ct_eq(given, &csrf_token(&a.secret)) {
        return Err(csrf_refused());
    }
    Ok(())
}

/// The app's bearer session, or (on the web host) the cookie session; writes with the
/// cookie need the CSRF token.
pub(crate) fn session_auth(st: &AppState, headers: &HeaderMap, method: &Method) -> Result<Authed, ApiError> {
    if !available(st) {
        return Err(off());
    }
    let bearer =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if let Some(raw) = bearer {
        if !raw.trim().starts_with("fca_") {
            return Err(signed_out());
        }
        let (session, account, secret) = check_session(st, raw, "app")?;
        return Ok(Authed { session, account, secret, web: false });
    }
    let a = web_session(st, headers)?;
    if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        check_csrf(st, headers, &a)?;
    }
    Ok(a)
}

// ── What the account sees ────────────────────────────────────────────────────────────

pub(crate) fn pro_json(st: &AppState, account: &str) -> Result<(bool, Value), ApiError> {
    let now = now_ms();
    let Some(s) = st.db.account_sub(account, now).map_err(ApiError::db)? else {
        return Ok((
            false,
            json!({ "active": false, "source": null, "plan": null, "status": null, "subscription_id": null,
                    "current_end": null, "access_until": null, "renews": false, "cancel_at_period_end": false }),
        ));
    };
    let a = s.access();
    let active = access::is_pro(&a, now);
    let grant = crate::billing::comp::is_comp(&s.id);
    Ok((
        active,
        json!({
            "active": active,
            "source": if grant { "grant" } else { "subscription" },
            "plan": s.plan, "status": s.status, "subscription_id": s.id,
            "current_end": s.current_end, "access_until": access::access_until(&a),
            "renews": !grant && access::PAID.contains(&s.status.as_str()) && !s.cancel_at_end && s.refunded_at.is_none(),
            "cancel_at_period_end": s.cancel_at_end,
        }),
    ))
}

fn me_json(st: &AppState, a: &Authed) -> Result<Value, ApiError> {
    let providers = st.db.account_providers(&a.account.id).map_err(ApiError::db)?;
    let (pro, pro_v) = pro_json(st, &a.account.id)?;
    let pcs = st.db.account_pcs(&a.account.id, now_ms()).map_err(ApiError::db)?.len();
    let mut v = json!({
        "account": {
            "id": a.account.id, "email": a.account.email, "name": a.account.name,
            "avatar_url": a.account.avatar_url, "created_at": a.account.created_at, "providers": providers,
        },
        "plan": if pro { "pro" } else { "free" },
        "pro": pro_v,
        "session": { "id": a.session.id, "kind": a.session.kind, "label": a.session.label, "expires_at": a.session.expires_at },
        "pcs": pcs,
    });
    if a.web {
        v["csrf_token"] = json!(csrf_token(&a.secret));
    }
    Ok(v)
}

/// `GET /v1/account`
async fn me(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = session_auth(&st, &headers, &method)?;
    Ok(no_store(axum::Json(me_json(&st, &a)?).into_response()))
}

/// `POST /v1/account/logout {everywhere?}` → 204.
async fn logout(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = session_auth(&st, &headers, &method)?;
    let b = json_body(&headers, &body)?;
    let now = now_ms();
    if b["everywhere"].as_bool() == Some(true) {
        st.db.account_revoke_all(&a.account.id, "user", now).map_err(ApiError::db)?;
    } else {
        st.db.account_revoke(&a.account.id, &a.session.id, "user", now).map_err(ApiError::db)?;
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    if a.web {
        r.headers_mut().insert(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0));
    }
    Ok(no_store(r))
}

/// `GET /v1/account/sessions`
async fn sessions(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = session_auth(&st, &headers, &method)?;
    let list = st.db.account_sessions(&a.account.id, now_ms()).map_err(ApiError::db)?;
    let v: Vec<Value> = list
        .iter()
        .map(|s| {
            json!({ "id": s.id, "kind": s.kind, "label": s.label, "created_at": s.created_at,
                    "last_used_at": s.last_used_at, "expires_at": s.expires_at, "current": s.id == a.session.id })
        })
        .collect();
    Ok(no_store(axum::Json(json!({ "sessions": v })).into_response()))
}

/// `DELETE /v1/account/sessions/{id}` → 204.
async fn revoke_session(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = session_auth(&st, &headers, &method)?;
    if id.len() > 64 || !st.db.account_revoke(&a.account.id, &id, "user", now_ms()).map_err(ApiError::db)? {
        return Err(e(StatusCode::NOT_FOUND, "not_found", "No session of yours with that id."));
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    if a.web && id == a.session.id {
        r.headers_mut().insert(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0));
    }
    Ok(no_store(r))
}

// ── The app's sign-in (§3) ───────────────────────────────────────────────────────────

/// `http://127.0.0.1:<port>/…` or `http://localhost:<port>/…`, nothing else.
pub(crate) fn loopback_ok(raw: &str) -> bool {
    if raw.len() > 512 {
        return false;
    }
    let Ok(u) = url::Url::parse(raw) else { return false };
    u.scheme() == "http"
        && matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
        && u.port().is_some()
        && u.username().is_empty()
        && u.password().is_none()
        && u.fragment().is_none()
}

fn url_safe(s: &str, len: std::ops::RangeInclusive<usize>) -> bool {
    len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// `POST /v1/account/app/start` (install auth) → 201 `{flow_id, url, expires_at}`.
async fn app_start(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if !available(&st) {
        return Err(off());
    }
    let install = authed(&st, &headers)?;
    let b = json_body(&headers, &body)?;
    let redirect = b["redirect_uri"].as_str().unwrap_or("").trim().to_string();
    if !loopback_ok(&redirect) {
        return Err(e(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_redirect",
            "redirect_uri must be http://127.0.0.1:<port>/… or http://localhost:<port>/….",
        ));
    }
    let challenge = b["code_challenge"].as_str().unwrap_or("");
    if b["code_challenge_method"].as_str() != Some("S256") || challenge.len() != 43 || !url_safe(challenge, 43..=43) {
        return Err(e(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_challenge",
            "code_challenge must be BASE64URL(SHA256(verifier)) with code_challenge_method S256.",
        ));
    }
    let state = b["state"].as_str().unwrap_or("");
    if !url_safe(state, 8..=128) {
        return Err(e(StatusCode::UNPROCESSABLE_ENTITY, "invalid", "state: 8 to 128 letters, digits, - . _ or ~."));
    }
    let label = match &b["label"] {
        Value::String(s) => clean_label(s),
        _ => String::new(),
    };
    let label = if label.is_empty() { "Ferrocast app".to_string() } else { label };
    if let Err(wait) = st.accounts.take("app_start", &install, 10, 3_600_000) {
        return Err(rate_limited(wait, "Too many sign-ins started from this PC."));
    }
    let id = format!("af_{}", crate::random(18));
    let now = now_ms();
    let expires_at = now + FLOW_TTL_MS;
    st.db
        .account_put_flow(&id, &install, &redirect, challenge, state, &label, now, expires_at)
        .map_err(ApiError::db)?;
    // The website's sign-in page when there is one, else this host's own.
    let url = if st.remote.cfg.web.is_some() {
        format!("{}/login/?flow={id}", page_origin(&st))
    } else {
        format!("{}/login?flow={id}", web_origin(&st))
    };
    Ok(no_store(
        (StatusCode::CREATED, axum::Json(json!({ "flow_id": id, "url": url, "expires_at": expires_at })))
            .into_response(),
    ))
}

/// `POST /v1/account/app/token {code, code_verifier}` (install auth) → 201 with the session.
async fn app_token(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if !available(&st) {
        return Err(off());
    }
    let install = authed(&st, &headers)?;
    let b = json_body(&headers, &body)?;
    let ip = crate::client_ip(peer, &headers).to_string();
    if let Err(wait) = st
        .accounts
        .take("app_token", &install, 20, 3_600_000)
        .and_then(|_| st.accounts.take("app_token_ip", &ip, 60, 3_600_000))
    {
        return Err(rate_limited(wait, "Too many sign-in codes tried."));
    }
    let expired = || {
        e(StatusCode::GONE, "flow_expired", "This sign-in expired or was already used.")
            .hint("Press Sign in again in Ferrocast.")
    };
    let code = b["code"].as_str().unwrap_or("").trim();
    let verifier = b["code_verifier"].as_str().unwrap_or("");
    if code.is_empty() || code.len() > 128 {
        return Err(expired());
    }
    if !url_safe(verifier, 43..=128) {
        return Err(e(StatusCode::UNPROCESSABLE_ENTITY, "invalid_challenge", "code_verifier doesn't match."));
    }
    let now = now_ms();
    let f = st.db.account_flow_by_code(&sha(code)).map_err(ApiError::db)?.ok_or_else(expired)?;
    if f.install_id != install || f.used_at.is_some() || f.code_expires_at.is_none_or(|t| t <= now) {
        return Err(expired());
    }
    let want = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, Sha256::digest(verifier));
    if !ct_eq(&want, &f.challenge) {
        return Err(e(StatusCode::UNPROCESSABLE_ENTITY, "invalid_challenge", "code_verifier doesn't match."));
    }
    let account_id = f.account_id.clone().ok_or_else(expired)?;
    if !st.db.account_flow_use(&f.id, now).map_err(ApiError::db)? {
        return Err(expired());
    }
    let account = st.db.account(&account_id).map_err(ApiError::db)?.ok_or_else(expired)?;
    if account.disabled_at.is_some() {
        return Err(disabled());
    }
    let (token, session) = new_session(&st, &account, "app", &f.label, Some(&install))?;
    tracing::info!(session = %session.id, "app signed in to an account");
    let a = Authed { session: session.clone(), account, secret: String::new(), web: false };
    Ok(no_store(
        (
            StatusCode::CREATED,
            axum::Json(json!({
                "session_token": format!("fca_{token}"),
                "session": { "id": session.id, "expires_at": session.expires_at },
                "account": me_json(&st, &a)?,
            })),
        )
            .into_response(),
    ))
}

/// After a sign-in: attach the Razorpay subscriptions bought with this verified email.
pub(crate) fn attach_by_email(st: &AppState, account: &str, email: &str) {
    let Some(key) = st.billing_cfg.email_key.as_deref() else { return };
    let h = crate::billing::email_hash(key, email);
    match st.db.account_attach_email_subs(account, &h, now_ms()) {
        Ok(n) if n > 0 => tracing::info!(attached = n, "subscriptions attached to an account by email"),
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "couldn't attach subscriptions by email"),
    }
}

/// A new checkout from a signed-in install belongs to its account.
pub(crate) fn attach_checkout(st: &AppState, install: &str, sub: &str) {
    let now = now_ms();
    if let Ok(Some(acct)) = st.db.account_for_install(install, now)
        && let Err(err) = st.db.account_attach_sub(&acct, sub, "checkout", now)
    {
        tracing::warn!(error = %err, "couldn't attach a checkout to its account");
    }
}

// ── The web dashboard's login rule (remote) ──────────────────────────────────────────

pub(crate) fn remote_login_required(st: &AppState) -> bool {
    st.accounts.cfg.remote_login == RemoteLogin::Required
}

/// For pairing: the signed-in account (cookie + CSRF), required unless the login mode is
/// `optional` (then `None` without a cookie).
pub(crate) fn pairing_account(st: &AppState, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let has_cookie = cookie(headers, SESSION_COOKIE).is_some();
    if !remote_login_required(st) && !has_cookie {
        return Ok(None);
    }
    match web_session_write(st, headers) {
        Ok(a) => Ok(Some(a.account.id)),
        Err(err) if err.status() == StatusCode::UNAUTHORIZED => {
            if remote_login_required(st) {
                Err(login_required())
            } else {
                Ok(None)
            }
        }
        Err(err) => Err(err),
    }
}

/// For a device call: the cookie's account must be the device's (a device from before
/// accounts is bound to the first account that uses it). Only in `required` mode.
pub(crate) fn device_account_ok(st: &AppState, headers: &HeaderMap, device: &str) -> Result<(), ApiError> {
    if !remote_login_required(st) {
        return Ok(());
    }
    let a = match web_session(st, headers) {
        Ok(a) => a,
        Err(err) if err.status() == StatusCode::UNAUTHORIZED => return Err(login_required()),
        Err(err) => return Err(err),
    };
    let owner = st.db.account_bind_device(device, &a.account.id, now_ms()).map_err(ApiError::db)?;
    if owner != a.account.id {
        return Err(wrong_account());
    }
    Ok(())
}
