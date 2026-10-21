//! Cloud sync and "Use cloud overlays" (docs/CLOUD.md §1.1, M6): off by default, only with a
//! signed-in Ferrocast account. Under /pro, like the account, so a phone never reaches it.

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

/// `GET /pro/cloud`: the switches, the last sync, what the cloud replaced here.
pub async fn status(State(app): State<AppState>) -> Json<Value> {
    Json(app.engine.cloud_status().await)
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudIn {
    #[serde(default)]
    sync: Option<bool>,
    #[serde(default)]
    use_cloud_overlays: Option<bool>,
}

/// `PUT /pro/cloud {sync?, use_cloud_overlays?}` → the status (409 `signed_out` without the account).
pub async fn set(State(app): State<AppState>, Body(b): Body<CloudIn>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_set(b.sync, b.use_cloud_overlays).await?))
}

/// `POST /pro/cloud/sync`: sync now → the status.
pub async fn sync(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.cloud_sync_now().await?))
}
