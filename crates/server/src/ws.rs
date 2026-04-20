//! The dashboard's live socket and the overlay sockets (API.md §5, §6).
//!
//! Overlays render only: the server accepts `hello`, `ack` and `visibility` from them and
//! ignores everything else, so a page can never create an event (R-36). A socket that falls
//! behind gets a fresh snapshot instead of being dropped.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
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

#[derive(Deserialize)]
pub struct LiveQuery {
    session: Option<String>,
}

/// `GET /api/v1/live?session=…`
pub async fn live(
    State(app): State<AppState>,
    Query(q): Query<LiveQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ok = q.session.as_deref().is_some_and(|t| app.sessions.valid(t));
    if !ok {
        return ApiError::unauthorized().into_response();
    }
    ws.max_message_size(MAX_INBOUND)
        .on_upgrade(move |socket| live_loop(app, socket))
}

async fn live_loop(app: AppState, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut frames = app.engine.subscribe_live();
    let hello = [
        LiveFrame::new("hello", &json!({ "seq": app.engine.current_seq() })),
        LiveFrame::new("status", &app.engine.status()),
        LiveFrame::new("queue", &app.engine.queue().await),
    ];
    for f in hello {
        if tx
            .send(Message::Text(Utf8Bytes::from(f.text.as_ref())))
            .await
            .is_err()
        {
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

async fn overlay_ws(
    State(app): State<AppState>,
    Path(token): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    match app.engine.overlay_by_token(&token).await {
        Ok(Some(o)) => ws
            .max_message_size(MAX_INBOUND)
            .on_upgrade(move |socket| overlay_loop(app, o, socket)),
        Ok(None) => ApiError::not_found("overlay").into_response(),
        Err(e) => ApiError::from(e).into_response(),
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
        Audience::Alerts => o
            .widgets
            .iter()
            .any(|w| matches!(w.kind, WidgetKind::Alerts | WidgetKind::Events)),
        Audience::Overlay(id) => *id == o.id,
    }
}

async fn send_snapshot(
    app: &AppState,
    o: &Overlay,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> bool {
    let snap = match app.engine.overlay_snapshot(o).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't build an overlay snapshot");
            json!({ "overlay": Value::Null, "state": {} })
        }
    };
    let text = envelope(Some(app.engine.current_seq()), "snapshot", &snap);
    tx.send(Message::Text(Utf8Bytes::from(text.as_ref())))
        .await
        .is_ok()
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
                        let v: Value = serde_json::from_str(&f.text).unwrap_or(Value::Null);
                        if v["type"] == "revoked" {
                            let _ = tx.send(Message::Close(None)).await;
                            return;
                        }
                        if let Ok(Some(fresh)) = app.engine.overlay_by_token(&overlay.token).await {
                            overlay = fresh;
                        }
                    }
                    if tx.send(Message::Text(Utf8Bytes::from(Arc::clone(&f.text).as_ref()))).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if !send_snapshot(&app, &overlay, &mut tx).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}
