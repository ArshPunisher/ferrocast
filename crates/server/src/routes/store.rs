//! Community Theme Store routes (`/api/v1/store/*`, R37).
//!
//! PC-only (`remote_scope` denies phones). Every catalog read verifies minisign + bounds,
//! every package/asset download checks SHA-256, and every `ferrocast://` resolve returns a
//! confirmation preview without side effects.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/store/catalog", get(catalog))
        .route("/store/installed", get(installed))
        .route("/store/assets/{item_id}/{name}", get(asset))
        .route("/store/install", post(install))
        .route("/store/update", post(update))
        .route("/store/uninstall", post(uninstall))
        .route("/store/undo", post(undo))
        .route("/store/resolve-link", post(resolve_link))
}

#[derive(Deserialize)]
struct FreshQuery {
    #[serde(default)]
    fresh: bool,
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UndoBody {
    token: Option<String>,
}

#[derive(Deserialize)]
struct LinkBody {
    uri: String,
}

async fn catalog(State(app): State<AppState>, Query(q): Query<FreshQuery>) -> ApiResult<Json<Value>> {
    let view = app.engine.store_catalog(q.fresh).await?;
    Ok(Json(json!(view)))
}

async fn installed(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let view = app.engine.store_installed().await?;
    Ok(Json(json!(view)))
}

async fn asset(
    State(app): State<AppState>,
    Path((item_id, name)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let (mime, bytes) = app.engine.store_asset(&item_id, &name).await?;
    let mut res = (StatusCode::OK, bytes).into_response();
    if let Ok(ct) = HeaderValue::from_str(&mime) {
        res.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    res.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    Ok(res)
}

async fn install(State(app): State<AppState>, Body(b): Body<IdBody>) -> ApiResult<impl IntoResponse> {
    let out = app.engine.install_store_item(&b.id).await?;
    Ok((StatusCode::CREATED, Json(json!(out))))
}

async fn update(State(app): State<AppState>, Body(b): Body<IdBody>) -> ApiResult<Json<Value>> {
    let out = app.engine.update_store_item(&b.id).await?;
    Ok(Json(json!(out)))
}

async fn uninstall(State(app): State<AppState>, Body(b): Body<IdBody>) -> ApiResult<Json<Value>> {
    let out = app.engine.uninstall_store_item(&b.id).await?;
    Ok(Json(json!(out)))
}

async fn undo(State(app): State<AppState>, Body(b): Body<UndoBody>) -> ApiResult<Json<Value>> {
    let out = app.engine.undo_store_action(b.token.as_deref()).await?;
    Ok(Json(json!(out)))
}

async fn resolve_link(State(app): State<AppState>, Body(b): Body<LinkBody>) -> ApiResult<Json<Value>> {
    let out = app.engine.resolve_store_link(&b.uri).await?;
    Ok(Json(json!(out)))
}
