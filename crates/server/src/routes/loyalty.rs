//! Loyalty points: settings, the leaderboard, and one person's balance.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::{Value, json};

use super::platform;
use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn config(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.loyalty()))
}

/// Saves the fields that were sent and keeps the rest; 400 names a bad field.
pub async fn save_config(State(app): State<AppState>, Body(patch): Body<Value>) -> ApiResult<Json<Value>> {
    let Some(patch) = patch.as_object() else {
        return Err(ApiError::invalid("The points settings must be an object."));
    };
    Ok(Json(json!(app.engine.patch_loyalty(patch).await?)))
}

/// `?limit=` from 1 to 100 (default 20).
pub async fn leaderboard(
    State(app): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let limit = match q.get("limit") {
        None => 20,
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| ApiError::invalid("limit: must be a whole number from 1 to 100."))?,
    };
    let rows: Vec<Value> = app
        .engine
        .leaderboard(limit)
        .await?
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            json!({ "rank": i + 1, "platform": l.platform, "user_id": l.user_id, "login": l.login,
                    "display_name": l.display_name, "points": l.points })
        })
        .collect();
    Ok(Json(json!(rows)))
}

pub async fn balance(State(app): State<AppState>, Path((p, user)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let points = app.engine.points(p, &user).await?;
    Ok(Json(json!({ "points": points, "name": app.engine.loyalty().name })))
}

/// `{ delta }`: a whole number, not 0, at most a billion either way. Checked here so a
/// fraction or a string gets a message about `delta`, not about JSON.
pub async fn adjust(
    State(app): State<AppState>,
    Path((p, user)): Path<(String, String)>,
    Body(b): Body<Value>,
) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let delta = match b.get("delta") {
        None => return Err(ApiError::invalid("delta: say how many points to add (or take away, negative).")),
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| ApiError::invalid("delta: must be a whole number."))?,
        Some(_) => return Err(ApiError::invalid("delta: must be a whole number.")),
    };
    let points = app.engine.adjust_points(p, &user, delta).await?;
    Ok(Json(json!({ "points": points, "name": app.engine.loyalty().name })))
}
