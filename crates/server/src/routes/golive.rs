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
        .route("/golive/protection", put(protection))
        .route("/golive/aitum", put(aitum_ok))
        .route("/golive/vertical/obs/setup", post(point_vertical))
        .route("/golive/vertical/source", put(vertical_source))
        .route("/golive/vertical/cut", put(vertical_cut))
        .route("/golive/picture", get(picture))
        .route("/golive/vertical/obs/restore", post(restore_vertical))
        .route("/golive/resume/{id}", post(resume))
}

async fn view(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.golive_view().await))
}

/// The local server and key, to type into OBS by hand. Never sent to a paired phone
/// (not in the remote allow-list).
async fn ingest(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let (server, key) = app.engine.golive_ingest().await?;
    let (_, vertical_key) = app.engine.golive_vertical_ingest().await?;
    Ok(Json(json!({ "server": server, "key": key, "vertical_key": vertical_key })))
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
    /// `vertical` for a server that takes OBS's vertical canvas.
    #[serde(default)]
    canvas: ferrocast_engine::golive::Canvas,
}

async fn add(State(app): State<AppState>, Body(b): Body<AddBody>) -> ApiResult<Response> {
    let d = app.engine.add_golive_destination(&b.name, &b.url, &b.key, b.canvas).await?;
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectBody {
    on: bool,
}

/// Upload protection on or off (A1.8). Off brings every paused destination back.
async fn protection(State(app): State<AppState>, Body(b): Body<ProtectBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_golive_protect(b.on).await?)))
}

/// "Resume now" on a destination upload protection paused.
async fn resume(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.golive_resume(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AitumBody {
    ok: bool,
}

/// "I use Aitum Multistream on purpose" (A2.2): hides the setup note.
async fn aitum_ok(State(app): State<AppState>, Body(b): Body<AitumBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_golive_aitum_ok(b.ok).await?)))
}

/// Points Aitum Vertical (OBS's vertical canvas) at Ferrocast (A2.3).
async fn point_vertical(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.golive_point_vertical().await?)))
}

/// Puts Aitum Vertical's own setting back.
async fn restore_vertical(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.golive_restore_vertical().await?)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceBody {
    source: ferrocast_engine::golive::VerticalSource,
}

/// Where the vertical picture comes from: OBS's vertical canvas, or cut from the stream (A2.4).
async fn vertical_source(State(app): State<AppState>, Body(b): Body<SourceBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_vertical_source(b.source).await?)))
}

/// The cut-from-stream layout.
async fn vertical_cut(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::golive::CutConfig>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_vertical_cut(b).await?)))
}

/// What's on air in OBS, for marking the camera and game (A2.4).
async fn picture(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.golive_picture().await?))
}
