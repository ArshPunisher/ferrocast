//! Chat polls: start, watch, end.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::poll::PollSpec;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// The running poll, or the last one that finished; `null` when there's none.
pub async fn current(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.poll()))
}

/// `{ question, options: string[2..5], duration_s: 10..1800 }`. Read by hand so every
/// wrong type gets a message that names its field.
pub async fn start(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<impl IntoResponse> {
    if !b.is_object() {
        return Err(ApiError::invalid("Send the poll as an object: { question, options, duration_s }."));
    }
    let question = match &b["question"] {
        Value::String(s) => s.clone(),
        _ => return Err(ApiError::invalid("question: must be text.")),
    };
    let options = match &b["options"] {
        Value::Array(list) => list
            .iter()
            .map(|o| o.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| ApiError::invalid("options: every answer must be text."))?,
        _ => return Err(ApiError::invalid("options: must be a list of 2 to 5 answers.")),
    };
    let duration_s = match &b["duration_s"] {
        Value::Null => 60,
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| ApiError::invalid("duration_s: polls run for 10 seconds to 30 minutes."))?,
        _ => return Err(ApiError::invalid("duration_s: must be a whole number of seconds.")),
    };
    let view = app.engine.start_poll(PollSpec { question, options, duration_s }).await?;
    Ok((StatusCode::CREATED, Json(json!(view))))
}

pub async fn end(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.end_poll().await?)))
}

pub async fn clear(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.clear_poll()?;
    Ok(StatusCode::NO_CONTENT)
}
