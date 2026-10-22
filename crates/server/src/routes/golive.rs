//! Go live everywhere routes (API.md "Go live"): the destinations, OBS setup, the upload check
//! and the local ingest for setting OBS up by hand. Every failure has our error shape: 400 bad
//! field, 404 unknown destination, 409 too many, 422 can't run now (with what to do), 429 busy.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/golive", get(view))
        .route("/golive/ingest", get(ingest))
        .route("/golive/destinations", put(order).post(add))
        .route("/golive/destinations/{id}", delete(remove))
        .route("/golive/obs/setup", post(point_obs))
        .route("/golive/obs/restore", post(restore_obs))
        .route("/golive/upload-check", post(upload_check))
}

async fn view(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.golive_view().await))
}

/// The local server and key, to type into OBS by hand. Never sent to a paired phone
/// (not in the remote allow-list).
async fn ingest(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let (server, key) = app.engine.golive_ingest().await?;
    Ok(Json(json!({ "server": server, "key": key })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderBody {
    destinations: Vec<OrderItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderItem {
    id: String,
    enabled: bool,
}

async fn order(State(app): State<AppState>, Body(b): Body<OrderBody>) -> ApiResult<Json<Value>> {
    let order = b.destinations.into_iter().map(|d| (d.id, d.enabled)).collect();
    Ok(Json(json!(app.engine.set_golive_destinations(order).await?)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    name: String,
    url: String,
    key: String,
}

async fn add(State(app): State<AppState>, Body(b): Body<AddBody>) -> ApiResult<Response> {
    let d = app.engine.add_golive_destination(&b.name, &b.url, &b.key).await?;
    Ok((StatusCode::CREATED, Json(json!(d))).into_response())
}

async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.remove_golive_destination(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn point_obs(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.golive_point_obs().await?)))
}

async fn restore_obs(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.golive_restore_obs().await?)))
}

async fn upload_check(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.golive_upload_check().await?)))
}
