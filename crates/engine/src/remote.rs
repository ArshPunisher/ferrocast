//! The PC's side of the dashboard from anywhere (Pro F2, API.md §9.5): an outbound
//! WebSocket to the relay's `/v1/remote/tunnel`, separate from the relay link.
//!
//! It's open only while Pro is active with the feature (the remembered status, no network
//! check) and at least one phone is paired or a pairing link is waiting (`remote_tunnel`
//! in the settings table, kept in step with the relay's `hello` / `devices` frames). It
//! reconnects with backoff (1 s doubling to 60 s, plus jitter; back to the start once a
//! connection held for 30 s) and treats 75 s of silence as a dead socket (the relay pings
//! every 25 s).
//!
//! The tunnel's loop never waits on a request: each runs in a task of the session's own
//! `JoinSet` (32 at once, else an instant 503 `pc_busy`; 14 s each, else 504 `pc_timeout`),
//! and so does each new phone's first frames (4 built at once). When the session ends the
//! set is dropped, so nothing it started keeps running.
//!
//! Each `req` frame is answered by the [`RemoteHandler`] the server registers: the same
//! router as the local dashboard, run in-process with the device's role instead of a
//! session token (the server checks the role again, `server/src/routes/remote_scope.rs`).
//! Live frames go to phones that opened a live socket: a `hello`, `status` and (owners)
//! `queue` for each new one, then everything from the live channel, marked owner-only when
//! a mod may not see it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ferrocast_store::repo;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, broadcast::error::RecvError, mpsc};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::frames::LiveFrame;
use crate::pro::{relay_error, unreachable};
use crate::{Engine, EngineError, now};

const SAVE_KEY: &str = "remote_tunnel";
/// Requests answered at once; more get 503 `pc_busy` at once. The relay lets no more than
/// this wait on one PC (`relay/src/remote/mod.rs` `MAX_IN_FLIGHT`, checked by a test).
pub const IN_FLIGHT: usize = 32;
/// One request's longest run here: under the relay's 15 s, so the phone hears why.
const HANDLE_TIMEOUT: Duration = Duration::from_secs(14);
/// New phones' first frames (`hello`, `status`, the queue, missed events) built at once.
const HELLO_AT_ONCE: usize = 4;
/// Phone sockets this tunnel keeps track of (5 own + 10 mods, 3 tabs each, fit).
const MAX_LIVE_SOCKS: usize = 64;
/// A connection that held this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// The largest answer sent back (the relay refuses more).
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const SILENT_MAX: Duration = Duration::from_secs(75);
/// When nothing says otherwise, look again this often.
const IDLE_CHECK: Duration = Duration::from_secs(60);

/// One request from a phone, as the relay sent it.
#[derive(Debug, Clone)]
pub struct RemoteRequest {
    pub method: String,
    /// `/api/v1/...`, with the query.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub device_id: String,
    /// `owner` or `mod`.
    pub role: String,
}

/// The answer that goes back.
#[derive(Debug, Clone)]
pub struct RemoteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Runs phone requests: the server's router, in-process (`server/src/remote.rs`).
#[async_trait::async_trait]
pub trait RemoteHandler: Send + Sync + std::fmt::Debug {
    async fn handle(&self, req: RemoteRequest) -> RemoteResponse;
    /// Whether a live frame (as text) may go to a phone with this role.
    fn live_allowed(&self, role: &str, frame: &str) -> bool;
}

/// Saved: whether the tunnel is wanted (a phone paired or a link waiting).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Saved {
    pub wanted: bool,
}

/// What Settings › Dashboard anywhere shows about the tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TunnelView {
    /// `off` (not wanted, or no Pro), `connecting`, `connected`, `retrying`.
    pub state: &'static str,
    pub detail: Option<String>,
    pub since: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct Remote {
    handler: std::sync::RwLock<Option<Arc<dyn RemoteHandler>>>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    wake: Notify,
    view: std::sync::Mutex<TunnelView>,
}

impl Engine {
    /// The server hands over its router once it's listening; the tunnel can start then.
    pub fn set_remote_handler(&self, handler: Arc<dyn RemoteHandler>) {
        *self.inner.remote.handler.write().unwrap_or_else(|p| p.into_inner()) = Some(handler);
        let mut slot = self.inner.remote.task.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            self.inner.remote.wake.notify_one();
            return;
        }
        let engine = self.clone();
        *slot = Some(tokio::spawn(async move { engine.remote_loop().await }));
    }

    fn remote_handler(&self) -> Option<Arc<dyn RemoteHandler>> {
        self.inner.remote.handler.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn remote_tunnel_view(&self) -> TunnelView {
        self.inner.remote.view.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn set_tunnel(&self, state: &'static str, detail: Option<String>) {
        let mut v = self.inner.remote.view.lock().unwrap_or_else(|p| p.into_inner());
        if v.state != state {
            v.since = Some(now().0);
        }
        v.state = state;
        v.detail = detail;
    }

    pub(crate) async fn remote_wanted(&self) -> bool {
        let saved = self.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.ok().flatten().unwrap_or_default();
        saved.wanted && self.remote_allowed().await
    }

    /// Whether this PC may open the tunnel: Pro with the feature, or signed in to any
    /// Ferrocast account (free accounts get the web dashboard, ACCOUNTS.md §7). The relay
    /// checks again (402 otherwise).
    pub(crate) async fn remote_allowed(&self) -> bool {
        self.account_signed_in() || self.pro_has(|f| f.remote).await
    }

    /// Look again at whether the tunnel should be open (signed in or out, Pro changed).
    pub(crate) fn remote_wake(&self) {
        self.inner.remote.wake.notify_one();
    }

    pub(crate) async fn set_remote_wanted(&self, wanted: bool) {
        let saved = Saved { wanted };
        if let Err(e) = self.db(move |s| repo::settings::set(s, SAVE_KEY, &saved)).await {
            tracing::warn!(error = %e, "couldn't save the remote tunnel setting");
        }
        if wanted {
            self.inner.remote.wake.notify_one();
        }
    }

    async fn remote_loop(&self) {
        let mut attempt = 0u32;
        loop {
            if !self.remote_wanted().await || self.relay_url().is_err() {
                self.set_tunnel("off", None);
                let _ = tokio::time::timeout(IDLE_CHECK, self.inner.remote.wake.notified()).await;
                continue;
            }
            self.set_tunnel("connecting", None);
            let mut up_since = None;
            match self.remote_session(&mut up_since).await {
                Ok(()) => attempt = 0,
                Err(e) => {
                    attempt = next_attempt(attempt, up_since.map(|t: tokio::time::Instant| t.elapsed()));
                    tracing::debug!(error = %e, attempt, "remote tunnel lost");
                    self.set_tunnel("retrying", Some(e));
                }
            }
            let wait = Duration::from_secs(1u64 << attempt.min(6)).min(Duration::from_secs(60));
            let jitter = Duration::from_millis(u64::try_from(now().0.rem_euclid(500)).unwrap_or(0));
            let _ = tokio::time::timeout(wait + jitter, self.inner.remote.wake.notified()).await;
        }
    }

    /// One tunnel connection, until it closes or isn't wanted any more. `up_since` is set
    /// once it's connected.
    async fn remote_session(&self, up_since: &mut Option<tokio::time::Instant>) -> Result<(), String> {
        let Some(handler) = self.remote_handler() else { return Err("the local server isn't ready".into()) };
        let url = self.relay_url().map_err(|e| e.to_string())?;
        let auth = self.relay_auth().await.map_err(|e| e.to_string())?;
        let ws_url = format!("{}/v1/remote/tunnel", url.replacen("http", "ws", 1));
        let mut req = ws_url.into_client_request().map_err(|e| e.to_string())?;
        req.headers_mut()
            .insert("authorization", format!("Bearer {auth}").parse().map_err(|_| "bad install id".to_string())?);
        let cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let connected = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async_with_config(req, Some(cfg), false),
        )
        .await
        .map_err(|_| "timed out connecting to the relay".to_string())?;
        let ws = match connected {
            Ok((ws, _)) => ws,
            Err(tokio_tungstenite::tungstenite::Error::Http(res)) => {
                return Err(match res.status().as_u16() {
                    402 => "Pro isn't active for this PC".into(),
                    401 => "the relay didn't recognise this install".into(),
                    503 => "the relay doesn't have this feature set up".into(),
                    s => format!("the relay refused (status {s})"),
                });
            }
            Err(e) => return Err(e.to_string()),
        };
        self.set_tunnel("connected", None);
        *up_since = Some(tokio::time::Instant::now());
        let (mut tx, mut rx) = ws.split();
        let (out, mut queued) = mpsc::channel::<String>(512);
        let writer = tokio::spawn(async move {
            while let Some(text) = queued.recv().await {
                if tx.send(Message::text(text)).await.is_err() {
                    return;
                }
            }
            let _ = tx.close().await;
        });
        let mut live = LiveSockets::default();
        let mut frames = self.subscribe_live();
        let permits = Arc::new(Semaphore::new(IN_FLIGHT));
        let hellos = Arc::new(Semaphore::new(HELLO_AT_ONCE));
        // Everything this session starts; dropped (so aborted) when it ends.
        let mut tasks: JoinSet<()> = JoinSet::new();
        let result = loop {
            while tasks.try_join_next().is_some() {}
            tokio::select! {
                msg = tokio::time::timeout(SILENT_MAX, rx.next()) => {
                    let msg = match msg {
                        Err(_) => break Err("the relay went quiet".to_string()),
                        Ok(None) => break Err("the relay closed the tunnel".to_string()),
                        Ok(Some(Err(e))) => break Err(e.to_string()),
                        Ok(Some(Ok(m))) => m,
                    };
                    let Message::Text(text) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    match v["t"].as_str() {
                        Some("hello" | "devices") => {
                            let none = v["devices"].as_i64() == Some(0) && v["codes"].as_i64() == Some(0);
                            if none {
                                // Nothing paired and no link waiting: close until a link is made.
                                self.set_remote_wanted(false).await;
                                break Ok(());
                            }
                        }
                        Some("req") => {
                            // Never wait for a free place here: this loop also carries live
                            // frames and the relay's pings.
                            let Ok(permit) = permits.clone().try_acquire_owned() else {
                                let _ = out.try_send(busy(&v["id"]).to_string());
                                continue;
                            };
                            let handler = handler.clone();
                            let out = out.clone();
                            tasks.spawn(async move {
                                let res = answer(handler.as_ref(), &v, HANDLE_TIMEOUT).await;
                                let _ = out.send(res.to_string()).await;
                                drop(permit);
                            });
                        }
                        Some("live_open") => {
                            let (Some(sock), Some(role)) = (v["sock"].as_u64(), v["role"].as_str()) else { continue };
                            live.open(sock, role == "owner");
                            if live.socks.len() == 1 {
                                // Frames sent while no phone listened were never wanted; start fresh.
                                frames = self.subscribe_live();
                            }
                            // Built off this loop: the queue and missed events can take a while.
                            let (engine, handler, out, hellos) = (self.clone(), handler.clone(), out.clone(), hellos.clone());
                            let (role, since) = (role.to_string(), v["since"].as_u64());
                            tasks.spawn(async move {
                                let Ok(_turn) = hellos.acquire_owned().await else { return };
                                for f in engine.remote_hello(handler.as_ref(), &role, since).await {
                                    let frame = json!({ "t": "live", "to": sock, "data": f.text.as_ref() });
                                    if out.send(frame.to_string()).await.is_err() {
                                        return;
                                    }
                                }
                            });
                        }
                        Some("live_close") => {
                            if let Some(sock) = v["sock"].as_u64() {
                                live.close(sock);
                            }
                        }
                        _ => {}
                    }
                }
                frame = frames.recv(), if !live.socks.is_empty() => match frame {
                    Ok(f) => {
                        let for_mods = handler.live_allowed("mod", &f.text);
                        if !for_mods && !live.any_owner() {
                            continue;
                        }
                        let msg = if for_mods {
                            json!({ "t": "live", "data": f.text.as_ref() })
                        } else {
                            json!({ "t": "live", "role": "owner", "data": f.text.as_ref() })
                        };
                        if out.try_send(msg.to_string()).is_err() {
                            break Err("the tunnel fell behind".to_string());
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        let resync = LiveFrame::new("resync", &json!({ "missed": n }));
                        if out.try_send(json!({ "t": "live", "data": resync.text.as_ref() }).to_string()).is_err() {
                            break Err("the tunnel fell behind".to_string());
                        }
                    }
                    Err(RecvError::Closed) => break Err("the engine is shutting down".to_string()),
                },
                _ = self.inner.remote.wake.notified() => {
                    if !self.remote_wanted().await {
                        break Ok(());
                    }
                }
            }
        };
        // Requests and first frames still running belong to this connection only.
        tasks.abort_all();
        drop(tasks);
        drop(out);
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        result
    }

    /// A new phone socket's first frames, like the local live socket's: `hello`, `status`,
    /// the queue (owners), and with `since` the events it missed (or `resync`).
    async fn remote_hello(&self, handler: &dyn RemoteHandler, role: &str, since: Option<u64>) -> Vec<LiveFrame> {
        let replay = since.filter(|s| *s > 0).map(|s| (s, self.events_since(s)));
        let seq = replay.as_ref().map_or_else(|| self.current_seq(), |(s, _)| *s);
        let mut out = vec![LiveFrame::new("hello", &json!({ "seq": seq })), LiveFrame::new("status", &self.status())];
        if role == "owner" {
            out.push(LiveFrame::new("queue", &self.queue().await));
        }
        match (since, replay) {
            (Some(_), Some((s, (missed, gap)))) => {
                if gap || s > self.current_seq() {
                    out.push(LiveFrame::new("resync", &json!({ "missed": 0 })));
                }
                out.extend(missed);
            }
            // `since: 0` (the relay doesn't know the phone's seq after a reconnect): refetch.
            (Some(_), None) => out.push(LiveFrame::new("resync", &json!({ "missed": 0 }))),
            _ => {}
        }
        out.retain(|f| handler.live_allowed(role, &f.text));
        out
    }

    // ── Settings › Dashboard anywhere ────────────────────────────────────────────────

    /// `GET /api/v1/pro/remote`: the relay's devices and audit, plus the tunnel's state.
    pub async fn remote_view(&self) -> Result<Value, EngineError> {
        let mut v = self.pro_call(reqwest::Method::GET, "/v1/pro/remote/devices", None).await?;
        let paired = v["devices"].as_array().is_some_and(|d| !d.is_empty());
        if paired {
            self.set_remote_wanted(true).await;
        }
        v["tunnel"] = json!(self.remote_tunnel_view());
        v["available"] = json!(self.remote_allowed().await);
        Ok(v)
    }

    /// `POST /api/v1/pro/remote/codes {role, name}`: a one-time pairing link. The tunnel
    /// opens so the phone finds the PC online.
    pub async fn remote_code(&self, role: &str, name: Option<&str>) -> Result<Value, EngineError> {
        if role != "owner" && role != "mod" {
            return Err(EngineError::Invalid("role: \"owner\" (your own phone) or \"mod\".".into()));
        }
        let body = json!({ "role": role, "name": name });
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/remote/codes", Some(body)).await?;
        self.set_remote_wanted(true).await;
        Ok(v)
    }

    /// `DELETE /api/v1/pro/remote/devices/{id}`.
    pub async fn remote_revoke(&self, id: &str) -> Result<(), EngineError> {
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(EngineError::NotFound("device"));
        }
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let res = self
            .relay_http()
            .delete(format!("{url}/v1/pro/remote/devices/{id}"))
            .bearer_auth(auth)
            .send()
            .await
            .map_err(|e| unreachable(e.without_url()))?;
        if !res.status().is_success() {
            return Err(relay_error(res).await);
        }
        Ok(())
    }
}

/// The next backoff step after a lost connection: back to the first step once it held for
/// [`STABLE_AFTER`] (one drop after hours of streaming shouldn't wait a minute).
fn next_attempt(attempt: u32, up_for: Option<Duration>) -> u32 {
    if up_for.is_some_and(|d| d >= STABLE_AFTER) { 1 } else { attempt.saturating_add(1) }
}

/// The phone sockets open on this tunnel (at most [`MAX_LIVE_SOCKS`]), and whether each is
/// an owner's.
#[derive(Debug, Default)]
struct LiveSockets {
    socks: HashMap<u64, bool>,
}

impl LiveSockets {
    fn any_owner(&self) -> bool {
        self.socks.values().any(|o| *o)
    }

    /// Adds a socket; past the cap the oldest (lowest id: the relay counts up) is forgotten.
    fn open(&mut self, sock: u64, owner: bool) {
        self.socks.insert(sock, owner);
        while self.socks.len() > MAX_LIVE_SOCKS {
            let Some(oldest) = self.socks.keys().min().copied() else { break };
            self.socks.remove(&oldest);
        }
    }

    fn close(&mut self, sock: u64) {
        self.socks.remove(&sock);
    }
}

/// A `res` frame with an error body.
fn error_res(id: &Value, status: u16, code: &str, message: &str, hint: &str, retry: Option<&str>) -> Value {
    let body = json!({ "error": { "code": code, "message": message, "hint": hint } }).to_string();
    let mut headers = vec![json!(["content-type", "application/json"])];
    if let Some(r) = retry {
        headers.push(json!(["retry-after", r]));
    }
    json!({ "t": "res", "id": id, "status": status, "headers": headers,
            "body_b64": base64::engine::general_purpose::STANDARD.encode(body) })
}

/// 503 `pc_busy`: every place is taken (the same words as the relay's).
fn busy(id: &Value) -> Value {
    error_res(
        id,
        503,
        "pc_busy",
        "Your PC is busy, try again in a moment.",
        "It's answering a lot of requests at once.",
        Some("2"),
    )
}

/// Runs one `req` frame (at most `wait`) and builds its `res` frame.
async fn answer(handler: &dyn RemoteHandler, v: &Value, wait: Duration) -> Value {
    let id = v["id"].clone();
    let fail = |status: u16, code: &str, message: &str| {
        error_res(&id, status, code, message, "Update Ferrocast on the PC, then try again.", None)
    };
    let (Some(method), Some(path), Some(role), Some(device)) =
        (v["method"].as_str(), v["path"].as_str(), v["role"].as_str(), v["device_id"].as_str())
    else {
        return fail(400, "invalid", "The relay sent an incomplete request.");
    };
    let Some(body) =
        v["body_b64"].as_str().map_or(Some(Vec::new()), |b| base64::engine::general_purpose::STANDARD.decode(b).ok())
    else {
        return fail(400, "invalid", "The relay sent a damaged request.");
    };
    let headers = v["headers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())))
                .take(16)
                .collect()
        })
        .unwrap_or_default();
    let req = RemoteRequest {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
        device_id: device.to_string(),
        role: role.to_string(),
    };
    let Ok(res) = tokio::time::timeout(wait, handler.handle(req)).await else {
        return error_res(
            &id,
            504,
            "pc_timeout",
            "Your streaming PC didn't answer in time.",
            "It may be very busy. Try again in a moment.",
            None,
        );
    };
    let headers: Vec<Value> = res.headers.iter().map(|(k, v)| json!([k, v])).collect();
    json!({ "t": "res", "id": id, "status": res.status, "headers": headers,
            "body_b64": base64::engine::general_purpose::STANDARD.encode(&res.body) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Slow(Duration);

    #[async_trait::async_trait]
    impl RemoteHandler for Slow {
        async fn handle(&self, _req: RemoteRequest) -> RemoteResponse {
            tokio::time::sleep(self.0).await;
            RemoteResponse { status: 200, headers: Vec::new(), body: b"{}".to_vec() }
        }
        fn live_allowed(&self, _role: &str, _frame: &str) -> bool {
            true
        }
    }

    fn req(id: u64) -> Value {
        json!({ "t": "req", "id": id, "method": "GET", "path": "/api/v1/status", "role": "owner", "device_id": "dv_1" })
    }

    fn error_of(res: &Value) -> Value {
        let body = base64::engine::general_purpose::STANDARD.decode(res["body_b64"].as_str().unwrap()).unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["error"].clone()
    }

    #[tokio::test]
    async fn a_request_that_runs_too_long_is_a_504_with_words() {
        let res = answer(&Slow(Duration::from_secs(5)), &req(7), Duration::from_millis(50)).await;
        assert_eq!((res["id"].as_u64(), res["status"].as_u64()), (Some(7), Some(504)));
        let e = error_of(&res);
        assert_eq!(e["code"], "pc_timeout");
        assert!(e["message"].as_str().is_some_and(|m| !m.is_empty()) && e["hint"].is_string(), "{e}");
        let ok = answer(&Slow(Duration::ZERO), &req(8), Duration::from_millis(500)).await;
        assert_eq!(ok["status"], 200);
    }

    #[test]
    fn a_full_pc_says_busy_at_once() {
        let res = busy(&json!(9));
        assert_eq!((res["id"].as_u64(), res["status"].as_u64()), (Some(9), Some(503)));
        let e = error_of(&res);
        assert_eq!(
            (e["code"].as_str(), e["message"].as_str()),
            (Some("pc_busy"), Some("Your PC is busy, try again in a moment."))
        );
        assert!(res["headers"].as_array().unwrap().contains(&json!(["retry-after", "2"])));
    }

    #[test]
    fn the_backoff_starts_over_after_a_connection_that_held() {
        assert_eq!(next_attempt(0, None), 1, "never connected");
        assert_eq!(next_attempt(5, Some(Duration::from_secs(3))), 6, "dropped right away: keep backing off");
        assert_eq!(next_attempt(6, Some(Duration::from_secs(3600))), 1, "up for an hour: start over");
        assert_eq!(next_attempt(u32::MAX, None), u32::MAX);
    }

    #[test]
    fn live_sockets_are_capped_oldest_first() {
        let mut l = LiveSockets::default();
        for s in 0..(MAX_LIVE_SOCKS as u64 + 5) {
            l.open(s, s == 0);
        }
        assert_eq!(l.socks.len(), MAX_LIVE_SOCKS);
        assert!(!l.socks.contains_key(&0) && l.socks.contains_key(&(MAX_LIVE_SOCKS as u64 + 4)));
        assert!(!l.any_owner(), "the oldest (the owner) went");
        l.close(10);
        assert_eq!(l.socks.len(), MAX_LIVE_SOCKS - 1);
    }

    #[tokio::test]
    async fn a_sessions_tasks_stop_with_it() {
        // What `remote_session` does with its `JoinSet` at the end: nothing it started lives on.
        let mut tasks: JoinSet<()> = JoinSet::new();
        let (alive_tx, mut alive_rx) = mpsc::channel::<()>(1);
        tasks.spawn(async move {
            let _keep = alive_tx;
            answer(&Slow(Duration::from_secs(3600)), &req(1), Duration::from_secs(3600)).await;
        });
        tokio::task::yield_now().await;
        tasks.abort_all();
        drop(tasks);
        let gone = tokio::time::timeout(Duration::from_secs(2), alive_rx.recv()).await;
        assert_eq!(gone, Ok(None), "the task (and what it held) was dropped");
    }
}
