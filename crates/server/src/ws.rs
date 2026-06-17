//! The dashboard's live socket and the overlay sockets (API.md §5, §6).
//!
//! Overlays render only: the server accepts `hello`, `ack` and `visibility` from them and
//! ignores everything else, so a page can never create an event (R-36). A socket that falls
//! behind gets a fresh snapshot instead of being dropped.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrocast_core::overlay::{Overlay, WidgetKind};
use ferrocast_core::queue::AckPhase;
use ferrocast_engine::frames::{Audience, LiveFrame, envelope};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;
use crate::error::ApiError;

/// Overlays and the dashboard send tiny frames; anything bigger is a mistake or an attack.
const MAX_INBOUND: usize = 16 * 1024;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/o/{token}/ws", get(overlay_ws))
}

/// Reads `key` from a query string, leniently: a malformed query never turns into a
/// plain-text error page a socket client can't read.
fn query_value(raw: Option<&str>, key: &str) -> Option<String> {
    raw?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| v.to_string())
    })
}

/// `GET /api/v1/live?session=…&since=…` (`since`: the last `event` seq the dashboard saw;
/// missed events are replayed first, R11).
pub async fn live(State(app): State<AppState>, RawQuery(raw): RawQuery, ws: WebSocketUpgrade) -> Response {
    let ok = query_value(raw.as_deref(), "session").is_some_and(|t| app.sessions.valid(&t));
    if !ok {
        return ApiError::unauthorized().into_response();
    }
    let since = query_value(raw.as_deref(), "since").and_then(|s| s.parse::<u64>().ok());
    ws.max_message_size(MAX_INBOUND).on_upgrade(move |socket| live_loop(app, socket, since))
}

async fn live_loop(app: AppState, socket: WebSocket, since: Option<u64>) {
    let (mut tx, mut rx) = socket.split();
    // Subscribe before replaying, so nothing falls in the gap; the dashboard drops
    // duplicate event ids.
    let mut frames = app.engine.subscribe_live();
    let replay = since.map(|s| (s, app.engine.events_since(s)));
    // While replaying, `hello` carries `since`: the dashboard keeps the highest seq it has
    // seen, so a drop halfway through the replay must not skip what wasn't delivered.
    let hello_seq = replay.as_ref().map_or_else(|| app.engine.current_seq(), |(s, _)| *s);
    let mut hello = vec![
        LiveFrame::new("hello", &json!({ "seq": hello_seq })),
        LiveFrame::new("status", &app.engine.status()),
        LiveFrame::new("queue", &app.engine.queue().await),
    ];
    if let Some((since, (missed, gap))) = replay {
        // Too old to replay, or from before a restart: refetch instead. `missed` 0 means
        // "some, count unknown".
        if gap || since > app.engine.current_seq() {
            hello.push(LiveFrame::new("resync", &json!({ "missed": 0 })));
        }
        hello.extend(missed);
    }
    for f in hello {
        if tx.send(Message::Text(Utf8Bytes::from(f.text.as_ref()))).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {} // the dashboard sends nothing but pings
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    if tx.send(Message::Text(Utf8Bytes::from(f.text.as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    let resync = LiveFrame::new("resync", &json!({ "missed": n }));
                    if tx.send(Message::Text(Utf8Bytes::from(resync.text.as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// Close codes the overlay page treats like `revoked`: clear and stop reconnecting (R18).
const CLOSE_UNKNOWN: u16 = 4404;

async fn overlay_ws(
    State(app): State<AppState>,
    Path(token): Path<String>,
    RawQuery(raw): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    match app.engine.overlay_by_token(&token).await {
        Ok(Some(o)) => {
            // `?test` (any value) asks for a preview socket.
            let test = query_value(raw.as_deref(), "test").is_some();
            ws.max_message_size(MAX_INBOUND).on_upgrade(move |socket| async move {
                if test { test_loop(app, o, socket).await } else { overlay_loop(app, o, socket).await }
            })
        }
        // A browser can't read an HTTP error on a socket, so say it with a close code.
        Ok(None) => ws.on_upgrade(|mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(CloseFrame { code: CLOSE_UNKNOWN, reason: "overlay not found".into() })))
                .await;
        }),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// A preview socket: its own sample events and alerts every few seconds, and layout
/// changes, but nothing from the real queue (and its acks are ignored).
async fn test_loop(app: AppState, mut overlay: Overlay, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut frames = app.engine.subscribe_overlays();
    if !send_snapshot_as(&app, &overlay, &mut tx, true).await {
        return;
    }
    // The first sample goes out at once, then one every few seconds.
    let mut tick = tokio::time::interval(TEST_EVERY);
    let mut n: u64 = 0;
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            _ = tick.tick() => {
                let (event, alert) = app.engine.preview(n);
                n += 1;
                let mut out = vec![envelope(Some(event.seq), "event", &event)];
                if let Some(a) = alert {
                    out.push(envelope(Some(app.engine.next_seq()), "alert", &a));
                }
                for text in out {
                    if tx.send(Message::Text(Utf8Bytes::from(text.as_ref()))).await.is_err() {
                        return;
                    }
                }
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    let Audience::Overlay(id) = &f.audience else { continue };
                    if *id != overlay.id {
                        continue;
                    }
                    if !forward_overlay_frame(&app, &mut overlay, &f, &mut tx).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => match recheck(&app, &overlay, &mut tx).await {
                    Some(fresh) => overlay = fresh,
                    None => return,
                },
                Err(RecvError::Closed) => return,
            },
        }
    }
}

const TEST_EVERY: std::time::Duration = std::time::Duration::from_secs(6);

/// Sends a frame meant for this overlay (layout change or revoke). `false` when the socket
/// is done: it failed, or the URL was revoked and the socket is now closed.
async fn forward_overlay_frame(
    app: &AppState,
    overlay: &mut Overlay,
    f: &ferrocast_engine::frames::OverlayFrame,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> bool {
    let v: Value = serde_json::from_str(&f.text).unwrap_or(Value::Null);
    if v["type"] == "revoked" {
        revoke(tx).await;
        return false;
    }
    match app.engine.overlay_by_token(&overlay.token).await {
        Ok(Some(fresh)) => *overlay = fresh,
        // Gone without our seeing the frame: treat it as revoked, never keep streaming.
        Ok(None) => {
            revoke(tx).await;
            return false;
        }
        Err(e) => tracing::warn!(error = %e, "couldn't re-read an overlay"),
    }
    tx.send(Message::Text(Utf8Bytes::from(Arc::clone(&f.text).as_ref()))).await.is_ok()
}

/// Tells the page its URL is dead, then closes with 4404. The page clears itself on the
/// frame; the close code covers a page that misses it.
async fn revoke(tx: &mut futures_util::stream::SplitSink<WebSocket, Message>) {
    let text = envelope(Some(0), "revoked", &json!({}));
    let _ = tx.send(Message::Text(Utf8Bytes::from(text.as_ref()))).await;
    let _ = tx.send(Message::Close(Some(CloseFrame { code: CLOSE_UNKNOWN, reason: "revoked".into() }))).await;
}

/// After falling behind, a socket may have missed a `revoked` frame: check the token
/// before sending anything else. `None` = revoked (the socket is closed).
async fn recheck(
    app: &AppState,
    overlay: &Overlay,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> Option<Overlay> {
    match app.engine.overlay_by_token(&overlay.token).await {
        Ok(Some(fresh)) => Some(fresh),
        Ok(None) => {
            revoke(tx).await;
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't re-read an overlay");
            Some(overlay.clone())
        }
    }
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
        visible: bool,
    },
}

fn wants(o: &Overlay, a: &Audience) -> bool {
    match a {
        Audience::All => true,
        Audience::Chat => o.wants_chat(),
        Audience::Alerts => o.widgets.iter().any(|w| matches!(w.kind, WidgetKind::Alerts | WidgetKind::Events)),
        Audience::Overlay(id) => *id == o.id,
    }
}

async fn send_snapshot(
    app: &AppState,
    o: &Overlay,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> bool {
    send_snapshot_as(app, o, tx, false).await
}

/// `preview`: a test-mode socket gets the layout, goals and viewer counts, but no real
/// chat, events or names; its content comes from the samples.
async fn send_snapshot_as(
    app: &AppState,
    o: &Overlay,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    preview: bool,
) -> bool {
    let snap = match app.engine.overlay_snapshot(o).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't build an overlay snapshot");
            json!({ "overlay": Value::Null, "state": {} })
        }
    };
    let mut snap = snap;
    if preview {
        snap["state"]["events"] = json!([]);
        snap["state"]["chat"] = json!([]);
        snap["state"]["labels"] = json!({});
    }
    let text = envelope(Some(app.engine.current_seq()), "snapshot", &snap);
    tx.send(Message::Text(Utf8Bytes::from(text.as_ref()))).await.is_ok()
}

async fn overlay_loop(app: AppState, mut overlay: Overlay, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut frames = app.engine.subscribe_overlays();
    if !send_snapshot(&app, &overlay, &mut tx).await {
        return;
    }
    loop {
        tokio::select! {
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Inbound>(&t) {
                    Ok(Inbound::Ack { alert_id, phase }) => app.engine.ack(&alert_id, phase).await,
                    Ok(Inbound::Hello { .. }) => {}
                    Ok(Inbound::Visibility { visible }) => tracing::debug!(overlay = %overlay.id, visible, "overlay visibility"),
                    Err(_) => tracing::debug!(overlay = %overlay.id, "ignored an unknown overlay frame"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            frame = frames.recv() => match frame {
                Ok(f) => {
                    if !wants(&overlay, &f.audience) {
                        continue;
                    }
                    if let Audience::Overlay(_) = f.audience {
                        // Layout changed or the URL was revoked.
                        if !forward_overlay_frame(&app, &mut overlay, &f, &mut tx).await {
                            return;
                        }
                        continue;
                    }
                    if tx.send(Message::Text(Utf8Bytes::from(Arc::clone(&f.text).as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let Some(fresh) = recheck(&app, &overlay, &mut tx).await else { return };
                    overlay = fresh;
                    if !send_snapshot(&app, &overlay, &mut tx).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}
