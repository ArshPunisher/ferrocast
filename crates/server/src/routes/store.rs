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
use crate::error::{ApiError, ApiResult};
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
    /// Left out: the latest action. Anything that is there (null, a number, an empty text) is a
    /// token and must be a good one: it is never read as "the latest".
    #[serde(deserialize_with = "present")]
    token: Option<serde_json::Value>,
}

/// A field that is there is `Some`, even when it is null (serde would read null as left out).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
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
    // Served from this origin but never run: no script, no frame, no network if it is opened.
    res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    res.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    Ok(res)
}

/// Installing, updating, removing and undoing change what runs on this PC (plugins included):
/// the owner at this computer only. A phone or browser paired over the network holds a session
/// token too, and doesn't get this.
fn at_this_pc(
    connect: &Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: &Option<axum::Extension<crate::remote::RemotePrincipal>>,
) -> ApiResult<()> {
    let loopback = |ip: std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    };
    // No peer address: an in-process call (the embedded app, tests), which is this PC.
    let from_elsewhere = remote.is_some()
        || connect.as_ref().is_some_and(|axum::Extension(axum::extract::ConnectInfo(a))| !loopback(a.ip()));
    if from_elsewhere {
        return Err(ApiError::forbidden("Installing from the Store is done at this computer.")
            .hint("Open Ferrocast on the PC to install, update or remove Store items."));
    }
    Ok(())
}

async fn install(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Body(b): Body<IdBody>,
) -> ApiResult<impl IntoResponse> {
    at_this_pc(&connect, &remote)?;
    let out = app.engine.install_store_item(&b.id).await?;
    Ok((StatusCode::CREATED, Json(json!(out))))
}

async fn update(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Body(b): Body<IdBody>,
) -> ApiResult<Json<Value>> {
    at_this_pc(&connect, &remote)?;
    let out = app.engine.update_store_item(&b.id).await?;
    Ok(Json(json!(out)))
}

async fn uninstall(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Body(b): Body<IdBody>,
) -> ApiResult<Json<Value>> {
    at_this_pc(&connect, &remote)?;
    let out = app.engine.uninstall_store_item(&b.id).await?;
    Ok(Json(json!(out)))
}

async fn undo(
    connect: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    remote: Option<axum::Extension<crate::remote::RemotePrincipal>>,
    State(app): State<AppState>,
    Body(b): Body<UndoBody>,
) -> ApiResult<Json<Value>> {
    at_this_pc(&connect, &remote)?;
    let token = match &b.token {
        None => None,
        Some(serde_json::Value::String(t)) => Some(t.as_str()),
        Some(_) => {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "store_undo_bad_token",
                "The undo token must be the text the action gave.",
            )
            .hint("Leave the token out to undo the latest action."));
        }
    };
    let out = app.engine.undo_store_action(token).await?;
    Ok(Json(json!(out)))
}

async fn resolve_link(State(app): State<AppState>, Body(b): Body<LinkBody>) -> ApiResult<Json<Value>> {
    let out = app.engine.resolve_store_link(&b.uri).await?;
    Ok(Json(json!(out)))
}
