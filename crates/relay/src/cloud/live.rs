//! The web dashboard's live side of cloud mode (docs/CLOUD.md §9.1): test alerts through the
//! cloud pipeline, the queue, the recent events and chat from the bus (memory only), and the
//! live socket (`hello`, `status`, `queue`, then `event`, `queue` and `notice` frames).

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ferrocast_core::event::{Event, Platform};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::bus::{Audience, Bus, envelope, queue_view};
use super::{body, e, json, typed, web_account};
use crate::{ApiError, AppState};

/// Test alerts per account per minute (docs/CLOUD.md §5).
pub const TESTS_PER_MINUTE: u32 = 10;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/api/v1/test", post(test))
        .route("/c/api/v1/queue", get(queue))
        .route("/c/api/v1/queue/{op}", post(queue_op))
        .route("/c/api/v1/queue/{alert_id}/remove", post(queue_remove))
        .route("/c/api/v1/themes", get(themes))
        .route("/c/api/v1/goals", get(goals))
        .route("/c/api/v1/events", get(events))
        .route("/c/api/v1/chat", get(chat))
        .route("/c/api/v1/live", get(live))
}

fn busy() -> ApiError {
    e(StatusCode::SERVICE_UNAVAILABLE, "cloud_busy", "Ferrocast's servers are busy right now.")
        .hint("Your overlays come back on their own. Try again in a minute.")
}

fn bus_for(st: &AppState, account: &str) -> Result<Arc<Bus>, ApiError> {
    st.cloud.buses.open(account, st.cloud.cfg.max_active).map_err(|_| busy())
}

#[derive(Deserialize)]
struct TestBody {
    kind: String,
    #[serde(default)]
    platform: Option<Platform>,
    #[serde(default)]
    design_id: Option<String>,
    #[serde(default)]
    amount: Option<f64>,
}

/// `POST /c/api/v1/test` → 202 `{ ok: true }`.
async fn test(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    st.accounts.take("cloud_test", account, TESTS_PER_MINUTE, 60_000).map_err(|wait| {
        e(StatusCode::TOO_MANY_REQUESTS, "cloud_rate", "That's a lot of test alerts in a minute.")
            .hint(format!("Wait {wait} seconds and send another."))
            .header(header::RETRY_AFTER, wait.to_string())
    })?;
    let t: TestBody = typed(body(&headers, &b)?)?;
    let design = match t.design_id.as_deref() {
        Some(id) => {
            let d = super::docs::design(&st, account, id)?;
            if d.kind() != ferrocast_core::design::DesignKind::Alert {
                return Err(super::invalid("design_id: test alerts need an alert design."));
            }
            Some(d)
        }
        None => None,
    };
    let bus = bus_for(&st, account)?;
    let event = super::ingest::test_event(&bus, &t.kind, t.platform, t.amount, design).map_err(super::invalid)?;
    super::ingest::publish(&st, account, event);
    Ok(super::no_store((StatusCode::ACCEPTED, axum::Json(json!({ "ok": true }))).into_response()))
}

async fn queue(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let view = match st.cloud.buses.get(&a.account.id) {
        Some(b) => queue_view(&b.state().queue),
        None => json!({ "paused": false, "playing": null, "waiting": [] }),
    };
    Ok(json(view))
}

/// `POST /c/api/v1/queue/{pause|resume|skip|clear}` → the queue.
async fn queue_op(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(op): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    if !matches!(op.as_str(), "pause" | "resume" | "skip" | "clear") {
        return Err(super::not_found("queue action").hint("Use pause, resume, skip or clear."));
    }
    let bus = bus_for(&st, &a.account.id)?;
    let view = match op.as_str() {
        "pause" => bus.queue_op(|q, _| {
            q.pause();
            vec![]
        }),
        "resume" => bus.queue_op(|q, t| q.resume(t)),
        "skip" => bus.queue_op(|q, t| q.skip(t)),
        _ => bus.queue_op(|q, _| {
            q.clear_waiting();
            vec![]
        }),
    };
    Ok(json(view))
}

/// `POST /c/api/v1/queue/{alert_id}/remove` → the queue (404 when it isn't waiting).
async fn queue_remove(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(alert_id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let bus = bus_for(&st, &a.account.id)?;
    let mut found = false;
    let view = bus.queue_op(|q, _| {
        found = q.remove(&alert_id);
        vec![]
    });
    if !found {
        return Err(super::not_found("waiting alert").hint("It may have played already."));
    }
    Ok(json(view))
}

/// The overlay looks every build ships (the engine's list), for the overlay editor.
const THEMES: [(&str, &str, &str); 5] = [
    ("molten", "Molten", "Liquid-metal cards with a cobalt edge. The default."),
    ("daylight", "Daylight", "Clean white cards that read well on busy games."),
    ("arcade", "Arcade", "Chunky pixel type and hard shadows."),
    ("minimal", "Minimal", "Just the words, for streams that want quiet overlays."),
    ("sticker", "Sticker", "Bright cut-out shapes that pop on camera."),
];

async fn themes(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    web_account(&st, &headers, &method, true)?;
    let list: Vec<Value> = THEMES
        .iter()
        .map(|(id, name, description)| json!({ "id": id, "name": name, "description": description, "pro": false }))
        .collect();
    Ok(json(json!(list)))
}

/// Goals need the app on a PC in 1.4 (they're counted from its event log): none here, so the
/// overlay editor's goal picker is simply empty.
async fn goals(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    web_account(&st, &headers, &method, true)?;
    Ok(json(json!([])))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

/// `GET /c/api/v1/events`: the bus's recent alertable events, newest first (memory only:
/// a relay restart starts empty).
async fn events(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let limit = q.limit.as_deref().and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 200);
    let kinds: Vec<String> = q
        .kinds
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
        .collect();
    let list: Vec<Arc<Event>> = match st.cloud.buses.get(&a.account.id) {
        Some(b) => b.state().events.iter().rev().cloned().collect(),
        None => Vec::new(),
    };
    let list: Vec<&Event> = list
        .iter()
        .map(|e| &**e)
        .filter(|e| kinds.is_empty() || kinds.iter().any(|k| k == e.kind.name()))
        .take(limit)
        .collect();
    Ok(json(json!({ "events": list, "next_before": null })))
}

/// `GET /c/api/v1/chat`: the last chat lines, oldest first.
async fn chat(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let list: Vec<Arc<Event>> = match st.cloud.buses.get(&a.account.id) {
        Some(b) => b.state().chat.iter().cloned().collect(),
        None => Vec::new(),
    };
    let list: Vec<&Event> = list.iter().map(|e| &**e).collect();
    Ok(json(json!(list)))
}

/// `GET /c/api/v1/live` (WebSocket): the dashboard's live frames. The cookie session, on the
/// web host, with an `Origin` of the web host (no other site can open it).
async fn live(State(st): State<AppState>, method: Method, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let a = match web_account(&st, &headers, &method, true) {
        Ok(a) => a,
        Err(err) => return err.into_response(),
    };
    if !crate::accounts::same_origin(&st, &headers) {
        return e(StatusCode::FORBIDDEN, "csrf", "This live connection didn't come from the Ferrocast dashboard.")
            .hint("Open the dashboard on the Ferrocast web address.")
            .into_response();
    }
    let Ok(bus) = bus_for(&st, &a.account.id) else { return busy().into_response() };
    let status = super::web::status_json(&st, &a.account.id).unwrap_or(Value::Null);
    ws.max_message_size(4 * 1024).on_upgrade(move |socket| live_loop(bus, status, socket))
}

/// One open dashboard socket, counted on the bus (chat is kept while one is open).
struct DashGuard(Arc<Bus>);

impl Drop for DashGuard {
    fn drop(&mut self) {
        let mut s = self.0.state();
        s.dashboards = s.dashboards.saturating_sub(1);
        drop(s);
        self.0.touch();
    }
}

async fn live_loop(bus: Arc<Bus>, status: Value, socket: WebSocket) {
    bus.state().dashboards += 1;
    let _guard = DashGuard(bus.clone());
    let (mut tx, mut rx) = socket.split();
    let mut frames = bus.subscribe();
    let (seq, queue) = {
        let s = bus.state();
        (s.seq, queue_view(&s.queue))
    };
    for f in [
        envelope(None, "hello", &json!({ "seq": seq })),
        envelope(None, "status", &json!({ "connectors": status["connectors"] })),
        envelope(None, "queue", &queue),
    ] {
        if tx.send(Message::Text(Utf8Bytes::from(f.as_ref()))).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => bus.touch(),
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    if f.audience != Audience::Dashboard {
                        continue;
                    }
                    if tx.send(Message::Text(Utf8Bytes::from(f.text.as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    let resync = envelope(None, "resync", &json!({ "missed": n }));
                    if tx.send(Message::Text(Utf8Bytes::from(resync.as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => {
                    let _ = tx.send(Message::Close(Some(CloseFrame { code: 1012, reason: "restarting".into() }))).await;
                    return;
                }
            },
        }
    }
}
