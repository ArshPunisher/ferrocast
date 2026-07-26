//! The end-of-stream recap.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

/// `?since=<ms>` (optional): from that moment instead of the stream's start.
pub async fn recap(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    let since = match q.get("since") {
        None => None,
        Some(raw) => {
            let now = ferrocast_engine::now().0;
            Some(
                raw.parse::<i64>()
                    .ok()
                    .filter(|v| (0..=now).contains(v))
                    .ok_or_else(|| ApiError::invalid("since: a time in Unix milliseconds, not in the future."))?,
            )
        }
    };
    Ok(Json(json!(app.engine.recap(since).await?)))
}
