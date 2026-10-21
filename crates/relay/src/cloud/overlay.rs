//! Overlay pages and sockets on the overlay origin (docs/CLOUD.md §3, §9.2): the overlay
//! build served unchanged under `/o/<token>/`, its media, and the socket speaking the
//! engine's protocol (API.md §6).
//!
//! Never on the web host (it carries the session cookie). An unknown token: the page is a
//! plain 404 (a scanner learns nothing), the socket closes with 4404. The socket takes only
//! `hello`, `ack` (for the alert this account's queue is playing) and `visibility`; anything
//! else is ignored and a frame over 4 KB closes it (R-36). Close codes the page knows: 4404
//! (gone: clear, stop), and 4410 (asleep), 4429 (too many open), 4503 (busy), after which it
//! tries again later.

use std::net::SocketAddr;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use ferrocast_core::event::Event;
use ferrocast_core::overlay::{Overlay, WidgetKind};
use ferrocast_core::queue::AckPhase;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::bus::{Audience, Bus, Frame, SOCKETS_PER_ACCOUNT, SOCKETS_PER_OVERLAY, envelope};
use super::docs;
use crate::{AppState, now_ms};

/// Overlays send tiny frames; anything bigger is a mistake or an attack (§10).
pub const MAX_INBOUND: usize = 4 * 1024;
/// Per IP per minute on the overlay paths (§10).
pub const PAGE_LOADS_PER_MINUTE: u32 = 60;
pub const SOCKET_OPENS_PER_MINUTE: u32 = 30;
/// Test mode: a sample every few seconds, as on the PC.
const TEST_EVERY: std::time::Duration = std::time::Duration::from_secs(6);

pub const CLOSE_GONE: u16 = 4404;
pub const CLOSE_ASLEEP: u16 = 4410;
pub const CLOSE_TOO_MANY: u16 = 4429;
pub const CLOSE_BUSY: u16 = 4503;

/// Scripts and connections only to this origin, images and fonts from https (§10).
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https:; media-src 'self' data: blob:; font-src 'self' data: https:; \
    connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/o/{token}", get(redirect))
        .route("/o/{token}/", get(index))
        .route("/o/{token}/ws", get(socket))
        .route("/o/{token}/{*path}", get(file))
}

fn gone() -> Response {
    let mut r = (StatusCode::NOT_FOUND, "This overlay URL doesn't exist anymore. Copy it again from Ferrocast.")
        .into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The account and overlay behind a token, on the overlay origin only (the overlay still
/// there; its layout isn't read, so a page's many files cost one lookup each).
fn resolve(st: &AppState, headers: &HeaderMap, token: &str) -> Option<(String, String)> {
    if !super::available(st) || crate::remote::on_web_host(st, headers) {
        return None;
    }
    let (account, overlay) = super::overlay_for_token(st, token)?;
    st.db.cloud_doc(&account, docs::OVERLAY, &overlay).ok()??.deleted_at.is_none().then_some((account, overlay))
}

fn ip_ok(st: &AppState, kind: &'static str, peer: SocketAddr, headers: &HeaderMap, max: u32) -> bool {
    let ip = crate::client_ip(peer, headers).to_string();
    st.accounts.take(kind, &ip, max, 60_000).is_ok()
}

async fn redirect(Path(token): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/o/{}/", token.chars().filter(char::is_ascii_hexdigit).collect::<String>()))
}

async fn index(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !ip_ok(&st, "overlay_page", peer, &headers, PAGE_LOADS_PER_MINUTE) {
        let mut r =
            (StatusCode::TOO_MANY_REQUESTS, "Too many overlay loads from here. Try again in a minute.").into_response();
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        return r;
    }
    if resolve(&st, &headers, &token).is_none() {
        return gone();
    }
    static_file(&st, "index.html").await
}

async fn file(State(st): State<AppState>, headers: HeaderMap, Path((token, path)): Path<(String, String)>) -> Response {
    let Some((account, _)) = resolve(&st, &headers, &token) else { return gone() };
    if let Some(id) = path.strip_prefix("media/") {
        return media(&st, &account, id, &headers).await;
    }
    static_file(&st, &path).await
}

/// A file of the overlay build: plain relative paths only, never out of the folder.
async fn static_file(st: &AppState, path: &str) -> Response {
    let Some(dir) = st.cloud.cfg.overlays_dir.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Cloud overlays aren't set up on this server yet. Try again later.")
            .into_response();
    };
    let rel = PathBuf::from(path);
    let safe = !path.is_empty()
        && path.len() <= 200
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_'))
        && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !safe {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(bytes) = tokio::fs::read(dir.join(&rel)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ext = path.rsplit('.').next().unwrap_or("");
    let mut r = Response::new(Body::from(bytes));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime_of(ext)));
    // Vite fingerprints files under assets/, so they can be cached for good; HTML can't.
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if ext == "html" {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    }
    r
}

fn mime_of(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// An uploaded file, with `Range` (OBS seeks in videos, R-41). Only ever as an image, sound
/// or video, never inline as a page (§10).
pub(crate) async fn media(st: &AppState, account: &str, id: &str, headers: &HeaderMap) -> Response {
    let Ok(Some(m)) = st.db.cloud_media_one(account, id) else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(blobs) = st.blobs() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let key = super::media::blob_key(account, &m.sha256);
    let len = u64::try_from(m.bytes).unwrap_or(0);
    let range = headers.get(header::RANGE).and_then(|r| r.to_str().ok()).map(|r| parse_range(r, len));
    let (start, count, status) = match range {
        None => (0, len, StatusCode::OK),
        Some(Some((a, b))) => (a, b - a + 1, StatusCode::PARTIAL_CONTENT),
        Some(None) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return r;
        }
    };
    let Ok(body) = blobs.get(&key).await else { return StatusCode::NOT_FOUND.into_response() };
    // Skip to `start`, then `count` bytes, chunk by chunk (never the whole file in memory).
    let stream = futures_util::stream::unfold((body, start, count), |(mut body, mut skip, mut left)| async move {
        while left > 0 {
            let chunk = match body.next().await? {
                Ok(c) => c,
                Err(e) => return Some((Err(e), (body, 0, 0))),
            };
            let n = chunk.len() as u64;
            if skip >= n {
                skip -= n;
                continue;
            }
            let from = usize::try_from(skip).unwrap_or(0);
            let take = (n - skip).min(left);
            let to = from + usize::try_from(take).unwrap_or(0);
            skip = 0;
            left -= take;
            return Some((Ok(chunk.slice(from..to)), (body, skip, left)));
        }
        None
    });
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = status;
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&m.mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{len}", start + count - 1))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

/// A single `bytes=a-b`, `bytes=a-` or `bytes=-n`; anything else `None`.
pub(crate) fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            (len.saturating_sub(n), len - 1)
        }
        (s, "") => (s.parse().ok()?, len - 1),
        (s, e) => (s.parse().ok()?, e.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

// ── The socket ───────────────────────────────────────────────────────────────────────

fn query_has(raw: Option<&str>, key: &str) -> bool {
    raw.is_some_and(|q| q.split('&').any(|p| p.split_once('=').map_or(p, |(k, _)| k) == key))
}

async fn close_with(socket: WebSocket, code: u16, reason: &'static str) {
    let mut socket = socket;
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
}

async fn socket(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(token): Path<String>,
    RawQuery(raw): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let ws = ws.max_message_size(MAX_INBOUND).max_frame_size(MAX_INBOUND);
    if !ip_ok(&st, "overlay_socket", peer, &headers, SOCKET_OPENS_PER_MINUTE) {
        return ws.on_upgrade(|s| close_with(s, CLOSE_TOO_MANY, "too many opens"));
    }
    let Some((account, overlay_id)) = resolve(&st, &headers, &token) else {
        return ws.on_upgrade(|s| close_with(s, CLOSE_GONE, "overlay not found"));
    };
    // A disabled account's overlays stop; a sleeping one's show nothing until it wakes.
    if !matches!(st.db.account(&account), Ok(Some(a)) if a.disabled_at.is_none()) {
        return ws.on_upgrade(|s| close_with(s, CLOSE_GONE, "overlay not found"));
    }
    match st.db.cloud_account(&account) {
        Ok(Some(c)) if c.sleeping_at.is_none() => {
            if now_ms() - c.last_active_at > 60_000 {
                // An overlay open in OBS counts as use (§5).
                let _ = st.db.cloud_touch(&account, now_ms());
            }
        }
        Ok(Some(_)) => return ws.on_upgrade(|s| close_with(s, CLOSE_ASLEEP, "asleep")),
        _ => return ws.on_upgrade(|s| close_with(s, CLOSE_GONE, "overlay not found")),
    }
    if st.cloud.buses.sockets.load(Ordering::Relaxed) >= st.cloud.cfg.max_sockets {
        return ws.on_upgrade(|s| close_with(s, CLOSE_BUSY, "busy"));
    }
    let Ok(bus) = st.cloud.buses.open(&account, st.cloud.cfg.max_active) else {
        return ws.on_upgrade(|s| close_with(s, CLOSE_BUSY, "busy"));
    };
    let Ok(overlay) = docs::overlay(&st, &account, &overlay_id) else {
        return ws.on_upgrade(|s| close_with(s, CLOSE_GONE, "overlay not found"));
    };
    let test = query_has(raw.as_deref(), "test");
    // Count it before the upgrade, so a burst can't get past the caps.
    let Some(guard) = SocketGuard::take(&st, &bus, &overlay) else {
        return ws.on_upgrade(|s| close_with(s, CLOSE_TOO_MANY, "too many sockets"));
    };
    ws.on_upgrade(move |socket| async move {
        let mut guard = guard;
        if test {
            test_loop(st, bus, overlay, token, socket).await;
        } else {
            overlay_loop(st, bus, overlay, token, socket, &mut guard).await;
        }
    })
}

/// One open overlay socket, counted on its bus and relay-wide while it lives.
struct SocketGuard {
    st: AppState,
    bus: Arc<Bus>,
    overlay: String,
    chat: bool,
}

impl SocketGuard {
    fn take(st: &AppState, bus: &Arc<Bus>, o: &Overlay) -> Option<SocketGuard> {
        let mut s = bus.state();
        if s.socket_count() >= SOCKETS_PER_ACCOUNT || s.sockets.get(&o.id).copied().unwrap_or(0) >= SOCKETS_PER_OVERLAY
        {
            return None;
        }
        *s.sockets.entry(o.id.clone()).or_default() += 1;
        let chat = o.wants_chat();
        if chat {
            s.chat_sockets += 1;
        }
        drop(s);
        st.cloud.buses.sockets.fetch_add(1, Ordering::Relaxed);
        bus.touch();
        Some(SocketGuard { st: st.clone(), bus: bus.clone(), overlay: o.id.clone(), chat })
    }

    /// The overlay's layout changed: whether it shows chat may have too.
    fn set_chat(&mut self, chat: bool) {
        if chat != self.chat {
            let mut s = self.bus.state();
            if chat {
                s.chat_sockets += 1;
            } else {
                s.chat_sockets = s.chat_sockets.saturating_sub(1);
            }
            self.chat = chat;
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let mut s = self.bus.state();
        if let Some(n) = s.sockets.get_mut(&self.overlay) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.sockets.remove(&self.overlay);
            }
        }
        if self.chat {
            s.chat_sockets = s.chat_sockets.saturating_sub(1);
        }
        drop(s);
        self.bus.touch();
        self.st.cloud.buses.sockets.fetch_sub(1, Ordering::Relaxed);
    }
}

type Tx = futures_util::stream::SplitSink<WebSocket, Message>;

async fn send_text(tx: &mut Tx, text: &str) -> bool {
    tx.send(Message::Text(Utf8Bytes::from(text))).await.is_ok()
}

/// The page's own copy of the overlay: no URL (it holds the token).
fn public(o: &Overlay) -> Value {
    json!({ "id": o.id, "name": o.name, "width": o.width, "height": o.height, "widgets": o.widgets,
            "updated_at": o.updated_at, "design": o.design })
}

fn shows_alerts(o: &Overlay) -> bool {
    o.widgets.iter().any(|w| matches!(w.kind, WidgetKind::Alerts | WidgetKind::Events))
}

fn wants(o: &Overlay, a: &Audience) -> bool {
    match a {
        Audience::All => true,
        Audience::Chat => o.wants_chat(),
        Audience::Alerts => shows_alerts(o),
        Audience::Overlay(id) => *id == o.id,
        Audience::Labels => o.widgets.iter().any(|w| w.kind == WidgetKind::Label),
        Audience::Dashboard => false,
    }
}

/// The first frame: layout and current state, so nothing is blank (R-38). Polls and
/// giveaways need the PC: always `null` here (§1.2).
fn snapshot(bus: &Bus, o: &Overlay, preview: bool) -> Arc<str> {
    let st = bus.state();
    let events: Vec<&Event> =
        if shows_alerts(o) && !preview { st.events.iter().map(|e| &**e).collect() } else { Vec::new() };
    let chat: Vec<&Event> =
        if o.wants_chat() && !preview { st.chat.iter().map(|e| &**e).collect() } else { Vec::new() };
    let labels = if preview { serde_json::Map::new() } else { st.session.labels(o) };
    let data = json!({
        "overlay": public(o),
        "state": { "goals": [], "labels": labels, "viewers": st.viewers, "events": events, "chat": chat,
                   "poll": null, "giveaway": null },
    });
    envelope(Some(st.seq), "snapshot", &data)
}

async fn revoke(tx: &mut Tx) {
    let _ = send_text(tx, &envelope(Some(0), "revoked", &json!({}))).await;
    let _ = tx.send(Message::Close(Some(CloseFrame { code: CLOSE_GONE, reason: "revoked".into() }))).await;
}

/// A frame for this overlay alone: a new layout, or the URL is gone. `false` when the
/// socket is done.
async fn own_frame(st: &AppState, bus: &Bus, overlay: &mut Overlay, f: &Frame, tx: &mut Tx) -> bool {
    let v: Value = serde_json::from_str(&f.text).unwrap_or(Value::Null);
    if v["type"] == "revoked" {
        revoke(tx).await;
        return false;
    }
    match docs::overlay(st, &bus.account, &overlay.id) {
        Ok(fresh) => *overlay = fresh,
        Err(_) => {
            revoke(tx).await;
            return false;
        }
    }
    send_text(tx, &f.text).await
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum Inbound {
    Hello {
        #[allow(dead_code)]
        since_seq: Option<u64>,
    },
    Ack {
        alert_id: String,
        phase: AckPhase,
    },
    Visibility {
        #[allow(dead_code)]
        visible: bool,
    },
}

/// Still this overlay's URL (after falling behind, a `revoked` may have been missed).
fn token_still(st: &AppState, bus: &Bus, o: &Overlay, token: &str) -> bool {
    super::overlay_for_token(st, token).is_some_and(|(a, id)| a == bus.account && id == o.id)
}

async fn overlay_loop(
    st: AppState,
    bus: Arc<Bus>,
    mut overlay: Overlay,
    token: String,
    socket: WebSocket,
    guard: &mut SocketGuard,
) {
    let (mut tx, mut rx) = socket.split();
    let mut frames = bus.subscribe();
    if !send_text(&mut tx, &snapshot(&bus, &overlay, false)).await {
        return;
    }
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Inbound>(&t) {
                    // Only the alert this account's queue is playing counts (core::queue).
                    Ok(Inbound::Ack { alert_id, phase }) => {
                        bus.queue_op(move |q, now| q.ack(&alert_id, phase, now));
                    }
                    Ok(Inbound::Hello { .. } | Inbound::Visibility { .. }) => bus.touch(),
                    Err(_) => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    if !wants(&overlay, &f.audience) {
                        continue;
                    }
                    let ok = match &f.audience {
                        Audience::Overlay(_) => {
                            let ok = own_frame(&st, &bus, &mut overlay, &f, &mut tx).await;
                            guard.set_chat(overlay.wants_chat());
                            ok
                        }
                        Audience::Labels => {
                            let (seq, labels) = {
                                let mut s = bus.state();
                                (s.next_seq(), s.session.labels(&overlay))
                            };
                            send_text(&mut tx, &envelope(Some(seq), "state", &json!({ "labels": labels }))).await
                        }
                        _ => send_text(&mut tx, &f.text).await,
                    };
                    if !ok {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let fresh = token_still(&st, &bus, &overlay, &token)
                        .then(|| docs::overlay(&st, &bus.account, &overlay.id).ok())
                        .flatten();
                    let Some(fresh) = fresh else {
                        revoke(&mut tx).await;
                        return;
                    };
                    overlay = fresh;
                    guard.set_chat(overlay.wants_chat());
                    if !send_text(&mut tx, &snapshot(&bus, &overlay, false)).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// A preview socket: samples every few seconds, layout changes, nothing from the real queue
/// (and its acks are ignored).
async fn test_loop(st: AppState, bus: Arc<Bus>, mut overlay: Overlay, token: String, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut frames = bus.subscribe();
    if !send_text(&mut tx, &snapshot(&bus, &overlay, true)).await {
        return;
    }
    let mut tick = tokio::time::interval(TEST_EVERY);
    let mut n: u64 = 0;
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            _ = tick.tick() => {
                let Some(cached) = super::ingest::cached(&st, &bus) else { return };
                let (seq1, seq2) = { let mut s = bus.state(); (s.next_seq(), s.next_seq()) };
                let (event, alert) = super::ingest::preview(&cached, n, seq1);
                n += 1;
                if !send_text(&mut tx, &envelope(Some(seq1), "event", &event)).await {
                    return;
                }
                if let Some(a) = alert && !send_text(&mut tx, &envelope(Some(seq2), "alert", &a)).await {
                    return;
                }
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    if f.audience != Audience::Overlay(overlay.id.clone()) {
                        continue;
                    }
                    if !own_frame(&st, &bus, &mut overlay, &f, &mut tx).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if !token_still(&st, &bus, &overlay, &token) {
                        revoke(&mut tx).await;
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}
