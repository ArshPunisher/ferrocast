//! Local API key management routes and authenticated API key principal extractor.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use ferrocast_core::api_keys::{ApiKeyCreate, ApiKeyScope};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// Extension attached to requests authenticated via a scoped local API key (`Bearer fc_key_...`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ApiKeyPrincipal {
    pub id: String,
    pub name: String,
    pub scopes: Vec<ApiKeyScope>,
}

/// Key management is this PC's own: a direct loopback connection, never a phone or a LAN peer
/// that holds a session token, never the relay's tunnel.
fn pc_only(
    connect: &Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: &Option<axum::Extension<crate::remote::RemotePrincipal>>,
) -> ApiResult<()> {
    if remote.is_some() {
        return Err(ApiError::forbidden("API keys are managed on this computer, not over remote access."));
    }
    // No peer address: an in-process call (tests, the embedded app), which is this PC.
    if let Some(axum::Extension(axum::extract::ConnectInfo(addr))) = connect
        && !crate::is_loopback(addr.ip())
    {
        return Err(ApiError::forbidden("API keys are managed on this computer, not from another device."));
    }
    Ok(())
}

/// `GET /api/v1/api-keys`: lists key metadata (never secrets/hashes) and recent redacted usage.
pub async fn list(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
) -> ApiResult<Json<Value>> {
    pc_only(&connect, &remote)?;
    let view = app.engine.api_keys_view().await?;
    Ok(Json(json!(view)))
}

/// `POST /api/v1/api-keys`: creates a new scoped key and returns the plain token once.
pub async fn create(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Body(req): Body<ApiKeyCreate>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    pc_only(&connect, &remote)?;
    let created = app.engine.create_api_key(req).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// `DELETE /api/v1/api-keys/{id}`: revokes an API key immediately.
pub async fn revoke(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    pc_only(&connect, &remote)?;
    app.engine.revoke_api_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
