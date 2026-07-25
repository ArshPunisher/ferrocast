//! First-time chatter settings: the welcome message and the returning badge.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn config(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.welcome()))
}

/// Saves the fields that were sent; 400 names a bad field.
pub async fn save(State(app): State<AppState>, Body(patch): Body<Value>) -> ApiResult<Json<Value>> {
    let Some(patch) = patch.as_object() else {
        return Err(ApiError::invalid("The welcome settings must be an object."));
    };
    Ok(Json(json!(app.engine.patch_welcome(patch).await?)))
}
