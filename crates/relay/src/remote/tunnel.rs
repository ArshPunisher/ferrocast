//! `/v1/remote/tunnel`: the PC's outbound socket for the dashboard from anywhere.
//!
//! Frames are JSON text, one per message:
//! - relay → PC: `{t:"hello", devices, codes}` on connect and `{t:"devices", devices, codes}`
//!   when they change (the PC closes the tunnel at none); `{t:"req", id, method, path,
//!   headers, body_b64, device_id, role}`; `{t:"live_open", sock, device_id, role, since?}`
//!   and `{t:"live_close", sock}` for each phone's live socket.
//! - PC → relay: `{t:"res", id, status, headers, body_b64}` and `{t:"live", to?, role?, data}`
//!   (a live frame as text: for one socket with `to`, for owners only with `role:"owner"`,
//!   else for every phone the role allows).
//!
//! One tunnel per install: a new one replaces the old. The relay pings every 25 s and drops a
//! tunnel that's silent for 75 s; Pro is checked again every 5 minutes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{AppState, MAX_IN_FLIGHT, MAX_RESPONSE, available, proxy, unavailable};
use crate::billing::gate::require_pro;
use crate::{ApiError, now_ms};

/// The largest frame the PC may send: an 8 MB answer in base64 and its envelope.
const MAX_FRAME: usize = MAX_RESPONSE / 3 * 4 + 64 * 1024;
const PING_EVERY: Duration = Duration::from_secs(25);
const SILENT_MAX: Duration = Duration::from_secs(75);
const PRO_RECHECK: Duration = Duration::from_secs(300);

/// The PC's answer to one request.
#[derive(Debug)]
pub(crate) struct Res {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why a request didn't get an answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failed {
    /// The tunnel closed.
    Gone,
    /// Too many requests are waiting on this PC, or its queue is full.
    Busy,
    Timeout,
}

#[derive(Debug)]
pub(crate) struct Tunnel {
    pub id: u64,
    out: mpsc::Sender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Res>>>,
}

impl Tunnel {
    /// Queues a frame; `false` when the tunnel is gone or can't keep up.
    pub fn send(&self, v: &Value) -> bool {
        self.out.try_send(Message::Text(Utf8Bytes::from(v.to_string()))).is_ok()
    }

    /// Sends `frame` (a `req` with this `id`) and waits up to `wait` (15 s) for its answer;
    /// the whole call never takes longer than `wait`.
    pub async fn request(&self, id: u64, frame: Value, wait: Duration) -> Result<Res, Failed> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if p.len() >= MAX_IN_FLIGHT {
                return Err(Failed::Busy);
            }
            p.insert(id, tx);
        }
        // Never wait for room in the queue: a PC that can't take more is busy, now, and the
        // phone hears so at once instead of after the timeout.
        match self.out.try_send(Message::Text(Utf8Bytes::from(frame.to_string()))) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.forget(id);
                return Err(Failed::Busy);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.forget(id);
                return Err(Failed::Gone);
            }
        }
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(_)) => Err(Failed::Gone),
            Err(_) => {
                self.forget(id);
                Err(Failed::Timeout)
            }
        }
    }

    fn forget(&self, id: u64) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    fn answer(&self, id: u64, res: Res) {
        if let Some(tx) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            let _ = tx.send(res);
        }
    }
}

/// `GET /v1/remote/tunnel` (WebSocket): install `Authorization` and Pro, checked before the
/// upgrade (401 / 402 / 503 `remote_unavailable`).
pub(crate) async fn connect(
    State(st): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let pro = require_pro(&st, &headers).await?;
    if !available(&st) {
        return Err(unavailable());
    }
    Ok(ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| run(st, pro.install, socket)))
}

async fn run(st: AppState, install: String, socket: WebSocket) {
    let (out, mut queued) = mpsc::channel::<Message>(256);
    let tunnel = Arc::new(Tunnel { id: st.remote.next_id(), out, pending: Mutex::default() });
    let old = st.remote.tunnels.lock().unwrap_or_else(|p| p.into_inner()).insert(install.clone(), tunnel.clone());
    // The older socket ends when its sender is dropped with it.
    drop(old);
    let (mut tx, mut rx) = socket.split();
    let (devices, codes) = st.db.remote_totals(&install, now_ms()).unwrap_or((0, 0));
    tunnel.send(&json!({ "t": "hello", "devices": devices, "codes": codes }));
    proxy::tunnel_up(&st, &install, &tunnel);
    tracing::debug!(%install, "remote tunnel open");

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut pro_check = tokio::time::interval(PRO_RECHECK);
    pro_check.tick().await;
    let mut last_heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            m = queued.recv() => match m {
                Some(m) => if tx.send(m).await.is_err() { break },
                None => break,
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENT_MAX || tx.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            _ = pro_check.tick() => {
                if !still_pro(&st, &install) {
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
            }
            msg = rx.next() => {
                last_heard = tokio::time::Instant::now();
                match msg {
                    Some(Ok(Message::Text(t))) => handle(&st, &install, &tunnel, &t),
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
        // Replaced by a newer tunnel: stop.
        if !is_current(&st, &install, tunnel.id) {
            break;
        }
    }
    let removed = {
        let mut m = st.remote.tunnels.lock().unwrap_or_else(|p| p.into_inner());
        if m.get(&install).is_some_and(|t| t.id == tunnel.id) { m.remove(&install).is_some() } else { false }
    };
    // Waiting requests get `Gone` as their senders drop with the map.
    tunnel.pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    if removed {
        proxy::tunnel_down(&st, &install);
    }
    tracing::debug!(%install, "remote tunnel closed");
}

fn is_current(st: &AppState, install: &str, id: u64) -> bool {
    st.remote.tunnels.lock().unwrap_or_else(|p| p.into_inner()).get(install).is_some_and(|t| t.id == id)
}

fn still_pro(st: &AppState, install: &str) -> bool {
    match st.db.pro_sub_for_install(install) {
        Ok(Some(s)) => crate::billing::access::is_pro(&s.access(), now_ms()),
        Ok(None) => false,
        // A database hiccup isn't a reason to cut the streamer off.
        Err(_) => true,
    }
}

fn handle(st: &AppState, install: &str, tunnel: &Tunnel, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v["t"].as_str() {
        Some("res") => {
            let Some(id) = v["id"].as_u64() else { return };
            let status = v["status"].as_u64().and_then(|s| u16::try_from(s).ok()).filter(|s| (100..600).contains(s));
            let body = v["body_b64"].as_str().map_or(Some(Vec::new()), |b| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b).ok()
            });
            let (Some(status), Some(body)) = (status, body) else {
                tunnel.answer(id, Res { status: 502, headers: Vec::new(), body: Vec::new() });
                return;
            };
            let headers = v["headers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())))
                        .take(32)
                        .collect()
                })
                .unwrap_or_default();
            tunnel.answer(id, Res { status, headers, body });
        }
        Some("live") => {
            let Some(data) = v["data"].as_str() else { return };
            let to = v["to"].as_u64();
            let owners_only = v["role"].as_str() == Some("owner");
            proxy::fan_out(st, install, to, owners_only, data);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_queue_is_busy_at_once_never_a_hang() {
        let (out, rx) = mpsc::channel::<Message>(1);
        let t = Tunnel { id: 1, out, pending: Mutex::default() };
        assert!(t.send(&json!({ "t": "x" })), "the one free place");
        let started = tokio::time::Instant::now();
        let r = t.request(7, json!({ "t": "req", "id": 7 }), Duration::from_secs(15)).await;
        assert_eq!(r.err(), Some(Failed::Busy));
        assert!(started.elapsed() < Duration::from_secs(1), "answered at once");
        assert!(t.pending.lock().unwrap().is_empty(), "nothing left waiting");
        drop(rx);
        let r = t.request(8, json!({ "t": "req", "id": 8 }), Duration::from_secs(15)).await;
        assert_eq!(r.err(), Some(Failed::Gone));
    }

    #[tokio::test]
    async fn more_than_the_pc_takes_at_once_is_busy() {
        let (out, _rx) = mpsc::channel::<Message>(256);
        let t = Arc::new(Tunnel { id: 1, out, pending: Mutex::default() });
        let mut waiting = Vec::new();
        for id in 0..MAX_IN_FLIGHT as u64 {
            let t = t.clone();
            waiting.push(tokio::spawn(async move { t.request(id, json!({}), Duration::from_millis(300)).await }));
        }
        for _ in 0..100 {
            if t.pending.lock().unwrap().len() == MAX_IN_FLIGHT {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let r = t.request(999, json!({}), Duration::from_millis(300)).await;
        assert_eq!(r.err(), Some(Failed::Busy));
        for w in waiting {
            assert_eq!(w.await.unwrap().err(), Some(Failed::Timeout));
        }
    }
}
