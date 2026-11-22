//! Local API key management routes and authenticated API key principal extractor.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use ferrocast_core::api_keys::{ApiKeyCreate, ApiKeyScope};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

/// Extension attached to requests authenticated via a scoped local API key (`Bearer fc_key_...`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ApiKeyPrincipal {
    pub id: String,
    pub name: String,
    pub scopes: Vec<ApiKeyScope>,
}

/// `GET /api/v1/api-keys`: lists key metadata (never secrets/hashes) and recent redacted usage.
pub async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let view = app.engine.api_keys_view().await?;
    Ok(Json(json!(view)))
}

/// `POST /api/v1/api-keys`: creates a new scoped key and returns the plain token once.
pub async fn create(
    State(app): State<AppState>,
    Body(req): Body<ApiKeyCreate>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let created = app.engine.create_api_key(req).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// `DELETE /api/v1/api-keys/{id}`: revokes an API key immediately.
pub async fn revoke(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.revoke_api_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
