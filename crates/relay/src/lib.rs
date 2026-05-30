//! The Ferrocast relay (ARCHITECTURE.md §1): the only server Ferrocast runs.
//!
//! It exists because some things need a public URL or a client secret: Kick's webhooks,
//! Kick's token exchange, and (later) payment webhooks for the tip page. Everything it
//! receives is forwarded to the streamer's desktop over an outbound WebSocket (no inbound
//! ports on their PC), queued while the desktop is offline (R-01, R-13 in VISION).

pub mod db;
pub mod kick;
pub mod tips;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rsa::sha2::{Digest, Sha256};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

pub use db::Db;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Read from the environment at start (see `deploy/relay.env.example`).
#[derive(Debug, Clone)]
pub struct Config {
    /// The relay's public base URL, e.g. `https://ferrocast-relay.clearcraftlabs.com`.
    pub public_url: String,
    pub kick_client_id: Option<String>,
    pub kick_client_secret: Option<String>,
    pub kick_id_url: String,
    pub kick_api_url: String,
}

impl Config {
    pub fn from_env() -> Config {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            public_url: env("RELAY_PUBLIC_URL")
                .unwrap_or_else(|| "http://127.0.0.1:8787".into())
                .trim_end_matches('/')
                .to_string(),
            kick_client_id: env("KICK_CLIENT_ID"),
            kick_client_secret: env("KICK_CLIENT_SECRET"),
            kick_id_url: env("KICK_ID_URL").unwrap_or_else(|| "https://id.kick.com".into()),
            kick_api_url: env("KICK_API_URL").unwrap_or_else(|| "https://api.kick.com".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Db>,
    pub http: reqwest::Client,
    pub keys: Arc<kick::KeyCache>,
    /// Wakes an install's link socket when something is queued for it.
    wakers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// Install registrations per IP per minute.
    signups: Arc<Mutex<HashMap<std::net::IpAddr, (i64, u32)>>>,
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> AppState {
        AppState {
            cfg: Arc::new(cfg),
            db: Arc::new(db),
            http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default(),
            keys: Arc::default(),
            wakers: Arc::default(),
            signups: Arc::default(),
        }
    }

    fn waker(&self, install: &str) -> Arc<Notify> {
        self.wakers.lock().unwrap_or_else(|p| p.into_inner()).entry(install.to_string()).or_default().clone()
    }

    pub(crate) fn enqueue(&self, install: &str, kind: &str, payload: &Value) -> Result<(), ApiError> {
        self.db.enqueue(install, kind, &payload.to_string()).map_err(ApiError::db)?;
        self.waker(install).notify_one();
        Ok(())
    }
}

/// `{error:{code,message,hint}}` with the right status, never a bare 500.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    hint: Option<String>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into(), hint: None }
    }
    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub(crate) fn db(e: rusqlite::Error) -> Self {
        tracing::error!(error = %e, "relay database error");
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The relay couldn't save that. Try again shortly.")
    }
    fn unauthorized() -> Self {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "This Ferrocast install isn't recognised.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(json!({ "error": { "code": self.code, "message": self.message, "hint": self.hint } })))
            .into_response()
    }
}

/// The largest request body the relay reads.
pub const BODY_LIMIT: usize = 256 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/installs", post(new_install))
        .route("/v1/link", get(link))
        .route("/v1/kick/oauth", post(kick_oauth))
        .route("/v1/kick/callback", get(kick_callback))
        .route("/v1/kick/refresh", post(kick_refresh))
        .route("/v1/kick/webhook", post(kick_webhook))
        .route("/v1/tips/config", axum::routing::put(tips::save_config).delete(tips::delete_config))
        .route("/v1/tips/razorpay/{install}", post(tips::razorpay))
        .route("/v1/tips/kofi/{install}", post(tips::kofi))
        .route("/v1/tips/stripe/{install}", post(tips::stripe))
        .route("/t/{handle}", get(tips::page))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

fn random(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The caller's address. Behind the local reverse proxy (the peer is loopback) that's the
/// last `X-Forwarded-For` hop the proxy added; a direct caller can't spoof it.
pub fn client_ip(peer: std::net::SocketAddr, headers: &HeaderMap) -> std::net::IpAddr {
    if peer.ip().is_loopback()
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks `Authorization: Bearer <install>.<secret>` and returns the install id.
pub(crate) fn authed(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let (id, secret) = raw.trim().split_once('.').ok_or_else(ApiError::unauthorized)?;
    let stored = state.db.install_hash(id).map_err(ApiError::db)?.ok_or_else(ApiError::unauthorized)?;
    if !bool::from(stored.as_bytes().ct_eq(hash(secret).as_bytes())) {
        return Err(ApiError::unauthorized());
    }
    let _ = state.db.touch_install(id);
    Ok(id.to_string())
}

async fn new_install(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ip = client_ip(addr, &headers);
    {
        let mut s = st.signups.lock().unwrap_or_else(|p| p.into_inner());
        let minute = now_ms() / 60_000;
        // Keep the map to the current minute so it can't grow without bound.
        s.retain(|_, (m, _)| *m == minute);
        let e = s.entry(ip).or_insert((minute, 0));
        e.1 += 1;
        if e.1 > 10 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many new installs from this address. Wait a minute.",
            ));
        }
    }
    let id = random(12);
    let secret = random(32);
    st.db.add_install(&id, &hash(&secret)).map_err(ApiError::db)?;
    Ok((StatusCode::CREATED, axum::Json(json!({ "install_id": id, "install_secret": secret })).into_response())
        .into_response())
}

/// The desktop's link: queued items first, then live ones; each deleted only when acked.
async fn link(State(st): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    Ok(ws.max_message_size(64 * 1024).on_upgrade(move |socket| link_loop(st, install, socket)))
}

async fn link_loop(st: AppState, install: String, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let wake = st.waker(&install);
    let mut sent_up_to = 0i64;
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        // Send anything not yet sent on this connection.
        match st.db.pending(&install, sent_up_to, 200) {
            Ok(items) => {
                for it in items {
                    let frame = json!({ "type": "item", "id": it.id, "kind": it.kind, "payload": serde_json::from_str::<Value>(&it.payload).unwrap_or(Value::Null) });
                    if tx.send(Message::Text(Utf8Bytes::from(frame.to_string()))).await.is_err() {
                        return;
                    }
                    sent_up_to = it.id;
                }
            }
            Err(e) => tracing::error!(error = %e, "couldn't read the queue"),
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = ping.tick() => {
                if tx.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t)
                        && v["type"] == "ack"
                        && let Some(id) = v["id"].as_i64()
                    {
                        let _ = st.db.ack(&install, id);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

fn kick_app(st: &AppState) -> Result<(String, String), ApiError> {
    match (&st.cfg.kick_client_id, &st.cfg.kick_client_secret) {
        (Some(id), Some(secret)) => Ok((id.clone(), secret.clone())),
        _ => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "kick_unavailable",
            "Kick sign-in isn't set up on this relay yet.",
        )),
    }
}

/// Starts Kick sign-in for an install: returns the URL to open in the browser.
async fn kick_oauth(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let (client_id, _) = kick_app(&st)?;
    let verifier = random(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random(24);
    st.db.put_oauth(&state, &install, &verifier).map_err(ApiError::db)?;
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", st.cfg.kick_id_url)).map_err(|_| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "config", "The relay's Kick address is wrong.")
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &format!("{}/v1/kick/callback", st.cfg.public_url))
        .append_pair("scope", &KICK_SCOPES.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    Ok(axum::Json(json!({ "url": url.to_string() })).into_response())
}

/// Every Kick scope Ferrocast uses (mirrors `ferrocast_kick::SCOPES`).
pub const KICK_SCOPES: &[&str] = &[
    "user:read",
    "channel:read",
    "chat:write",
    "events:subscribe",
    "moderation:ban",
    "moderation:chat_message:manage",
    "channel:rewards:read",
    "kicks:read",
];

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn page(status: StatusCode, text: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>Ferrocast</title>\
         <body style=\"font:17px system-ui;background:#0c0e12;color:#e9e9ee;display:grid;place-items:center;min-height:100vh;margin:0;padding:24px;text-align:center\"><p>{}</p>",
        text.replace('&', "&amp;").replace('<', "&lt;")
    );
    (status, Html(body)).into_response()
}

/// Kick sends the streamer back here. Trades the code (with the secret), learns whose
/// channel it is, and hands the tokens to that desktop through its queue.
async fn kick_callback(State(st): State<AppState>, Query(q): Query<Callback>) -> Response {
    if let Some(e) = q.error {
        return page(StatusCode::OK, &format!("Kick sign-in was cancelled ({e}). You can close this tab."));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This link is incomplete. Go back to Ferrocast and press Connect Kick again.",
        );
    };
    let Ok(Some((install, verifier))) = st.db.take_oauth(&state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in link expired or was already used. Press Connect Kick again in Ferrocast.",
        );
    };
    let Ok((client_id, secret)) = kick_app(&st) else {
        return page(StatusCode::SERVICE_UNAVAILABLE, "Kick sign-in isn't set up on this relay yet.");
    };
    let redirect = format!("{}/v1/kick/callback", st.cfg.public_url);
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("client_secret", secret.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier.as_str()),
        ("code", code.as_str()),
    ];
    let tokens = match st.http.post(format!("{}/oauth/token", st.cfg.kick_id_url)).form(&form).send().await {
        Ok(r) if r.status().is_success() => r.json::<kick::TokenResponse>().await.ok(),
        _ => None,
    };
    let Some(tokens) = tokens else {
        return page(StatusCode::BAD_GATEWAY, "Kick didn't accept the sign-in. Press Connect Kick again in Ferrocast.");
    };
    let user: Option<Value> = match st
        .http
        .get(format!("{}/public/v1/users", st.cfg.kick_api_url))
        .bearer_auth(&tokens.access_token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    };
    let Some(user) = user.and_then(|u| u.pointer("/data/0").cloned()) else {
        return page(StatusCode::BAD_GATEWAY, "Kick didn't say whose account this is. Try again.");
    };
    let broadcaster = match &user["user_id"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return page(StatusCode::BAD_GATEWAY, "Kick sent an account without an id."),
    };
    if st.db.set_kick_owner(&broadcaster, &install).is_err() {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The relay couldn't save that. Try again shortly.");
    }
    let payload = json!({
        "tokens": { "access_token": tokens.access_token, "refresh_token": tokens.refresh_token,
                    "expires_at": now_ms() + tokens.expires_in.clamp(0, 86_400 * 30) * 1000 },
        "user": { "id": broadcaster, "name": user["name"], "avatar_url": user["profile_picture"] },
    });
    if st.enqueue(&install, "kick_connected", &payload).is_err() {
        return page(StatusCode::SERVICE_UNAVAILABLE, "The relay couldn't save that. Try again shortly.");
    }
    page(StatusCode::OK, "Kick is connected to Ferrocast. You can close this tab.")
}

#[derive(Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

/// Refreshes a Kick token for an install (the client secret never leaves the relay).
async fn kick_refresh(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(b): axum::Json<RefreshBody>,
) -> Result<Response, ApiError> {
    authed(&st, &headers)?;
    let (client_id, secret) = kick_app(&st)?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("client_secret", secret.as_str()),
        ("refresh_token", b.refresh_token.as_str()),
    ];
    let res =
        st.http.post(format!("{}/oauth/token", st.cfg.kick_id_url)).form(&form).send().await.map_err(|_| {
            ApiError::new(StatusCode::BAD_GATEWAY, "kick_unreachable", "Couldn't reach Kick. Try again.")
        })?;
    if !res.status().is_success() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "sign_in_needed", "Kick needs you to sign in again."));
    }
    let t: kick::TokenResponse = res
        .json()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "kick_unreachable", "Kick sent a bad answer."))?;
    Ok(axum::Json(json!({ "access_token": t.access_token, "refresh_token": t.refresh_token,
                          "expires_at": now_ms() + t.expires_in.clamp(0, 86_400 * 30) * 1000 }))
    .into_response())
}

/// Kick's webhook: verify, drop replays, route by channel, queue for the desktop. Always
/// quick, so Kick never times out and unsubscribes us.
async fn kick_webhook(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (id, ts, sig, ty, ver) = (
        h("kick-event-message-id"),
        h("kick-event-message-timestamp"),
        h("kick-event-signature"),
        h("kick-event-type"),
        h("kick-event-version"),
    );
    if id.is_empty() || ts.is_empty() || sig.is_empty() || ty.is_empty() {
        return ApiError::new(StatusCode::BAD_REQUEST, "missing_headers", "Kick event headers are missing.")
            .into_response();
    }
    let delivery = kick::Delivery { message_id: &id, timestamp: &ts, body: &body, signature: &sig };
    if let Err(e) = st.keys.verify(&st.http, &st.cfg.kick_api_url, &delivery, now_ms()).await {
        tracing::warn!(error = %e, "refused a Kick webhook");
        return ApiError::new(StatusCode::UNAUTHORIZED, "bad_signature", "The event isn't signed by Kick.")
            .into_response();
    }
    // Seen before? Answer OK so Kick stops retrying. The id is recorded only after the
    // event is safely queued, so a failed write lets Kick's retry through.
    match st.db.seen(&id) {
        Ok(true) => return StatusCode::OK.into_response(),
        Ok(false) => {}
        Err(e) => return ApiError::db(e).into_response(),
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The event body isn't JSON.").into_response();
    };
    let broadcaster = match v.pointer("/broadcaster/user_id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return StatusCode::OK.into_response(),
    };
    match st.db.kick_owner(&broadcaster) {
        Ok(Some(install)) => {
            let payload = json!({ "type": ty, "version": ver, "message_id": id, "body": v });
            if let Err(e) = st.enqueue(&install, "kick_event", &payload) {
                return e.into_response();
            }
        }
        Ok(None) => tracing::debug!(%broadcaster, "a Kick event for a channel no install owns"),
        Err(e) => return ApiError::db(e).into_response(),
    }
    if let Err(e) = st.db.first_time(&id) {
        tracing::warn!(error = %e, "couldn't record a webhook id; a retry may be queued twice (the desktop dedupes)");
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests;
