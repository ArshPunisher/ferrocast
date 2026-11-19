//! Guest cameras (API.md §9.11): the relay only introduces a guest's browser to the streamer's PC.
//! The camera and mic go straight between them over WebRTC; the relay never sees them.
//!
//! - The PC makes an invite (`POST /v1/guests/invites`, install `Authorization`): a random token,
//!   kept here only as its SHA-256, for 30 days or until the PC removes it. Its link is the
//!   remote host's `/app/guest.html#<token>`: a page of the dashboard's build, so the token,
//!   after `#`, never reaches a server log.
//! - The guest's page posts its WebRTC offer (`POST /g/v1/offer`, on the remote host). The relay
//!   passes it down that install's link socket (`/v1/link`) as a `guest_offer` frame and waits
//!   for the PC's `guest_answer` (the same `req`) for up to [`ANSWER_WAIT`]. Nothing is queued: a
//!   PC that isn't connected can't take a guest, and the guest is told so.
//!
//! Offers are size and rate limited per address and per invite; bodies are never logged.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::remote::limits::{Limits, ip_key};
use crate::remote::proxy::on_remote_host;
use crate::{ApiError, AppState, authed, client_ip, hash, now_ms, random};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS guest_invites (hash TEXT PRIMARY KEY, install_id TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS guest_invites_install ON guest_invites(install_id);
";

/// An invite lasts this long.
pub const INVITE_TTL_MS: i64 = 30 * 86_400_000;
/// Invites one install may hold at once.
pub const MAX_INVITES: i64 = 20;
/// How long a guest waits for the PC's answer.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);
/// The largest offer taken (a browser's is a few KB).
pub const MAX_SDP: usize = 32 * 1024;
/// A guest's name, at most.
pub const MAX_NAME: usize = 40;

/// The PCs' link sockets (frames to send down them) and the offers waiting for an answer.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    /// install → (this link's number, its frames).
    links: Mutex<HashMap<String, (u64, mpsc::UnboundedSender<String>)>>,
    /// req → (the install that must answer, the guest's request).
    waiting: Mutex<HashMap<String, (String, oneshot::Sender<Value>)>>,
    limits: Mutex<Limits>,
    next: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// An install's link, registered while its socket is open (dropped: unregistered, unless a newer
/// link took its place).
#[derive(Debug)]
pub(crate) struct Attached {
    st: AppState,
    install: String,
    n: u64,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mut links = lock(&self.st.guests.links);
        if links.get(&self.install).is_some_and(|(n, _)| *n == self.n) {
            links.remove(&self.install);
        }
    }
}

impl Runtime {
    /// Registers an install's link: frames for it arrive on the receiver.
    pub(crate) fn attach(st: &AppState, install: &str) -> (Attached, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let n = st.guests.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lock(&st.guests.links).insert(install.to_string(), (n, tx));
        (Attached { st: st.clone(), install: install.to_string(), n }, rx)
    }

    /// The PC's `guest_answer` on its link: handed to the guest waiting for it (only if this
    /// install was the one asked).
    pub(crate) fn answered(&self, install: &str, frame: &Value) {
        let Some(req) = frame["req"].as_str() else { return };
        let mut waiting = lock(&self.waiting);
        if waiting.get(req).is_some_and(|(i, _)| i == install)
            && let Some((_, tx)) = waiting.remove(req)
        {
            let _ = tx.send(frame.clone());
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/guests/invites", post(create_invite))
        .route("/v1/guests/invites/{token}", delete(delete_invite))
        .route("/g/v1/offer", post(offer))
        .route("/g/v1/ice", post(ice))
}

/// The STUN server every call gets (the guest's browser learns its own public address).
const STUN: &str = "stun:stun.cloudflare.com:3478";
/// How long a guest's TURN credentials last (a long show is covered; they're for this call).
const TURN_TTL_S: u64 = 4 * 3600;

/// Cloudflare TURN's key, as the relay's `RELAY_TURN_KEY_ID` and `RELAY_TURN_KEY_TOKEN` give it
/// (Cloudflare dashboard › Realtime › TURN; free tier). None: guests get STUN only, which is
/// enough unless both sides are behind strict networks.
fn turn_key() -> Option<(String, String)> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((env("RELAY_TURN_KEY_ID")?, env("RELAY_TURN_KEY_TOKEN")?))
}

/// Cloudflare's answer as a list of ICE servers: `generate-ice-servers` gives an array,
/// the older `generate` one object.
pub(crate) fn ice_servers_of(v: &Value) -> Vec<Value> {
    match v.get("iceServers") {
        Some(Value::Array(a)) => a.iter().filter(|s| s.get("urls").is_some()).cloned().collect(),
        Some(o @ Value::Object(_)) if o.get("urls").is_some() => vec![o.clone()],
        _ => Vec::new(),
    }
}

/// Short-lived TURN servers for one call from Cloudflare (5 s at most).
async fn turn_servers(http: &reqwest::Client, id: &str, token: &str) -> Result<Vec<Value>, String> {
    let base = std::env::var("RELAY_TURN_API_URL").unwrap_or_else(|_| "https://rtc.live.cloudflare.com".into());
    let url = format!("{}/v1/turn/keys/{id}/credentials/generate-ice-servers", base.trim_end_matches('/'));
    let res = http
        .post(url)
        .bearer_auth(token)
        .json(&json!({ "ttl": TURN_TTL_S }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let v: Value = res.json().await.map_err(|e| e.without_url().to_string())?;
    let servers = ice_servers_of(&v);
    if servers.is_empty() { Err("no servers in the answer".into()) } else { Ok(servers) }
}

/// `POST /g/v1/ice` `{token}` → `{iceServers}` for a guest's browser: STUN, and TURN for this
/// call when the relay has a TURN key (so two strict networks still connect).
async fn ice(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if !on_remote_host(&st, &headers) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's nothing here."));
    }
    let now = now_ms();
    let ip = ip_key(client_ip(peer, &headers));
    if let Err(wait) = lock(&st.guests.limits).take("guest_ice_ip", &ip, 20, 60_000, now) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many tries from here.")
            .hint(format!("Try again in {wait} s."))
            .header(axum::http::header::RETRY_AFTER, wait.to_string()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| bad("That wasn't a guest's request."))?;
    let token =
        v["token"].as_str().filter(|t| !t.is_empty() && t.len() <= 64).ok_or_else(|| bad("The link is incomplete."))?;
    let live: bool = st
        .db
        .c()
        .query_row("SELECT 1 FROM guest_invites WHERE hash = ?1 AND expires_at > ?2", params![hash(token), now], |_| {
            Ok(true)
        })
        .optional()
        .map_err(ApiError::db)?
        .unwrap_or(false);
    if !live {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "invite_ended", "This guest link has ended.")
            .hint("Ask the streamer for a new link."));
    }
    // Per link too: a leaked one can't mint TURN credentials from many addresses (CodeRabbit).
    if let Err(wait) = lock(&st.guests.limits).take("guest_ice_invite", token, 30, 600_000, now) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "This link has been tried a lot.")
            .hint(format!("Try again in {wait} s."))
            .header(axum::http::header::RETRY_AFTER, wait.to_string()));
    }
    let mut servers = vec![json!({ "urls": STUN })];
    if let Some((id, key)) = turn_key() {
        match turn_servers(&st.http, &id, &key).await {
            Ok(mut t) => servers.append(&mut t),
            // STUN still works for most networks; the key never shows in the log.
            Err(e) => tracing::warn!(error = %e, "no TURN servers for a guest"),
        }
    }
    Ok(axum::Json(json!({ "iceServers": servers })).into_response())
}

/// `POST /v1/guests/invites` → `{token, url, expires_at}`.
async fn create_invite(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let Some(origin) = st.remote.cfg.origin.clone() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "guests_unavailable",
            "Guest links aren't set up on this relay yet.",
        ));
    };
    let now = now_ms();
    let token = random(18);
    let added = {
        let c = st.db.c();
        c.execute("DELETE FROM guest_invites WHERE expires_at <= ?1", [now]).map_err(ApiError::db)?;
        let held: i64 = c
            .query_row("SELECT COUNT(*) FROM guest_invites WHERE install_id = ?1", [&install], |r| r.get(0))
            .map_err(ApiError::db)?;
        if held >= MAX_INVITES {
            false
        } else {
            c.execute(
                "INSERT INTO guest_invites(hash, install_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![hash(&token), install, now, now + INVITE_TTL_MS],
            )
            .map_err(ApiError::db)?;
            true
        }
    };
    if !added {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "too_many_invites",
            format!("You have {MAX_INVITES} guest links already."),
        )
        .hint("Remove one you no longer use, then make a new one."));
    }
    let url = format!("{origin}/app/guest.html#{token}");
    Ok((StatusCode::CREATED, axum::Json(json!({ "token": token, "url": url, "expires_at": now + INVITE_TTL_MS })))
        .into_response())
}

/// `DELETE /v1/guests/invites/{token}`: the link stops working (204 even if it had already).
async fn delete_invite(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    st.db
        .c()
        .execute("DELETE FROM guest_invites WHERE hash = ?1 AND install_id = ?2", params![hash(&token), install])
        .map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn bad(message: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message)
}

/// `POST /g/v1/offer` `{token, name, offer: {type: "offer", sdp}}` → `{answer}`.
async fn offer(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if !on_remote_host(&st, &headers) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's nothing here."));
    }
    let now = now_ms();
    let ip = ip_key(client_ip(peer, &headers));
    if let Err(wait) = lock(&st.guests.limits).take("guest_offer_ip", &ip, 10, 60_000, now) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many tries from here.")
            .hint(format!("Try again in {wait} s."))
            .header(axum::http::header::RETRY_AFTER, wait.to_string()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| bad("That wasn't a guest's request."))?;
    let token =
        v["token"].as_str().filter(|t| !t.is_empty() && t.len() <= 64).ok_or_else(|| bad("The link is incomplete."))?;
    let name: String =
        v["name"].as_str().unwrap_or("").trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    if name.is_empty() {
        return Err(bad("Enter your name."));
    }
    let sdp = v["offer"]["sdp"].as_str().filter(|s| !s.is_empty() && s.len() <= MAX_SDP);
    let (Some(sdp), true) = (sdp, v["offer"]["type"] == "offer") else {
        return Err(bad("Your browser's connection details were missing or too long."));
    };
    let install: Option<String> = st
        .db
        .c()
        .query_row(
            "SELECT install_id FROM guest_invites WHERE hash = ?1 AND expires_at > ?2",
            params![hash(token), now],
            |r| r.get(0),
        )
        .optional()
        .map_err(ApiError::db)?;
    let Some(install) = install else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "invite_ended", "This guest link has ended.")
            .hint("Ask the streamer for a new link."));
    };
    if let Err(wait) = lock(&st.guests.limits).take("guest_offer_invite", token, 20, 600_000, now) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "This link has been tried a lot.")
            .hint(format!("Try again in {wait} s."))
            .header(axum::http::header::RETRY_AFTER, wait.to_string()));
    }
    let link = lock(&st.guests.links).get(&install).map(|(_, tx)| tx.clone());
    let offline = || {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "streamer_offline", "The streamer's Ferrocast isn't open.")
            .hint("Ask them to open Ferrocast, then try again.")
    };
    let Some(link) = link else { return Err(offline()) };
    let req = random(12);
    let (tx, rx) = oneshot::channel();
    lock(&st.guests.waiting).insert(req.clone(), (install, tx));
    // Gone however this ends, the guest leaving early included (the future dropped).
    struct Waiting<'a>(&'a Runtime, String);
    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            lock(&self.0.waiting).remove(&self.1);
        }
    }
    let _waiting = Waiting(&st.guests, req.clone());
    let frame = json!({
        "type": "guest_offer",
        "req": req,
        "token": token,
        "name": name,
        "offer": { "type": "offer", "sdp": sdp },
    });
    if link.send(frame.to_string()).is_err() {
        return Err(offline());
    }
    let reply = tokio::time::timeout(ANSWER_WAIT, rx).await;
    let reply = match reply {
        Ok(Ok(r)) => r,
        _ => {
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "no_answer",
                "The streamer's Ferrocast didn't answer.",
            )
            .hint("Try again in a moment."));
        }
    };
    if let (Some(sdp), true) = (reply["answer"]["sdp"].as_str(), reply["answer"]["type"] == "answer") {
        return Ok(axum::Json(json!({ "answer": { "type": "answer", "sdp": sdp } })).into_response());
    }
    // The PC said no (guests off, the Studio closed…): its words, short and plain.
    let said: String =
        reply["error"].as_str().unwrap_or("The streamer can't take guests right now.").chars().take(200).collect();
    Err(ApiError::new(StatusCode::CONFLICT, "guest_refused", said))
}

#[cfg(test)]
#[path = "guests_tests.rs"]
mod tests;
