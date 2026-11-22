//! Chat Highlight: pin one chat message on stream until dismissed or timed out.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// The currently pinned chat message, or `null`.
pub async fn current(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.chat_highlight()))
}

/// Pins one chat message: `{ event_id, duration_s?: 5..=3600 | null }`.
pub async fn pin(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<(StatusCode, Json<Value>)> {
    if !b.is_object() {
        return Err(ApiError::invalid("Send the highlight as an object: { event_id, duration_s? }."));
    }
    let event_id = match &b["event_id"] {
        Value::String(s) => s.clone(),
        _ => return Err(ApiError::invalid("event_id: must be a chat message id.")),
    };
    let duration_s = match &b["duration_s"] {
        Value::Null => None,
        Value::Number(n) => {
            let v = n.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| {
                ApiError::invalid("duration_s: leave empty to stay until dismissed, or pick 5 to 3600 seconds.")
            })?;
            Some(v)
        }
        _ => {
            return Err(ApiError::invalid(
                "duration_s: leave empty to stay until dismissed, or pick 5 to 3600 seconds.",
            ));
        }
    };
    let h = app.engine.pin_chat_highlight(&event_id, duration_s).await?;
    Ok((StatusCode::OK, Json(json!(h))))
}

/// Dismisses any highlighted chat message.
pub async fn clear(State(app): State<AppState>) -> StatusCode {
    app.engine.clear_chat_highlight();
    StatusCode::NO_CONTENT
}
