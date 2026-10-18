//! The dashboard from anywhere on this PC (Pro F2, API.md §9.5).
//!
//! [`Handler`] answers the requests that come through the relay's tunnel
//! (`ferrocast_engine::RemoteHandler`): each one runs through **this same router** in-process
//! (`tower::ServiceExt::oneshot`), carrying a [`RemotePrincipal`] extension instead of a
//! session token. A request from the network can't set an extension, so only the tunnel can
//! act as a device; `api_guard` (lib.rs) then checks the role against `routes::remote_scope`
//! (the relay checked the same rules first). Only `Content-Type`, `Accept` and (for media)
//! `Range` reach the router, with this server's own `Host`; only a few headers go back. A
//! media range is cut to 4 MB, so a video preview streams in parts under the relay's 8 MB
//! per answer.
//!
//! The desktop's own routes for Settings › Dashboard anywhere are here too:
//! `GET /pro/remote`, `POST /pro/remote/codes`, `DELETE /pro/remote/devices/{id}`. A phone
//! can never reach them (`/pro/**` is closed to every role).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use ferrocast_engine::{Engine, REMOTE_MAX_RESPONSE, RemoteHandler, RemoteRequest, RemoteResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body as JsonBody;
use crate::routes::remote_scope::{self, Role};

/// Who a tunnelled request is for. Present only on requests from [`Handler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePrincipal {
    pub device_id: String,
    pub role: Role,
}

/// 403 `not_allowed_for_role`, the same answer the relay gives.
pub(crate) fn not_allowed(role: Role) -> ApiError {
    let message =
        if role == Role::Mod { "Mods can't do that from here." } else { "That only works on the streaming PC." };
    ApiError::new(StatusCode::FORBIDDEN, "not_allowed_for_role", message)
        .hint("Accounts, secrets, Pro, export and import stay on the PC itself.")
}

/// Runs phone requests through the router.
pub(crate) struct Handler {
    router: Router,
    engine: Engine,
    host: HeaderValue,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler").field("host", &self.host).finish_non_exhaustive()
    }
}

impl Handler {
    pub(crate) fn new(router: Router, engine: Engine, port: u16) -> Handler {
        let host = HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap_or(HeaderValue::from_static("127.0.0.1"));
        Handler { router, engine, host }
    }

    #[cfg(test)]
    pub(crate) fn engine_for_tests(&self) -> &Engine {
        &self.engine
    }
}

fn error_response(e: ApiError) -> RemoteResponse {
    let body = json!({ "error": { "code": e.code, "message": e.message, "hint": e.hint } }).to_string();
    RemoteResponse {
        status: e.status.as_u16(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.into_bytes(),
    }
}

/// Answers kept from the router's response.
const PASSED_BACK: [HeaderName; 6] = [
    header::CONTENT_TYPE,
    header::CONTENT_DISPOSITION,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::RETRY_AFTER,
    HeaderName::from_static("x-quota-reset"),
];

/// The largest media range one phone request gets (well under the relay's 8 MB answers).
pub(crate) const RANGE_CHUNK: u64 = 4 * 1024 * 1024;

/// A `Range` value cut to [`RANGE_CHUNK`] bytes: `bytes=0-` (a video's first ask) becomes
/// `bytes=0-4194303`, and the player asks for the next part itself. Anything else (several
/// ranges, nonsense) is left for the media route to refuse.
pub(crate) fn clamp_range(value: &str) -> String {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return value.to_string() };
    if spec.contains(',') {
        return value.to_string();
    }
    let Some((a, b)) = spec.split_once('-') else { return value.to_string() };
    let last = |start: u64| start.saturating_add(RANGE_CHUNK - 1);
    match (a.trim().parse::<u64>().ok(), b.trim()) {
        (Some(start), "") => format!("bytes={start}-{}", last(start)),
        (Some(start), end) => match end.parse::<u64>() {
            Ok(end) if end >= start && end - start >= RANGE_CHUNK => format!("bytes={start}-{}", last(start)),
            _ => value.to_string(),
        },
        (None, n) if a.trim().is_empty() => match n.parse::<u64>() {
            Ok(n) if n > RANGE_CHUNK => format!("bytes=-{RANGE_CHUNK}"),
            _ => value.to_string(),
        },
        _ => value.to_string(),
    }
}

#[async_trait::async_trait]
impl RemoteHandler for Handler {
    async fn handle(&self, r: RemoteRequest) -> RemoteResponse {
        let Some(role) = Role::parse(&r.role) else {
            return error_response(ApiError::new(
                StatusCode::FORBIDDEN,
                "not_allowed_for_role",
                "Unknown device role.",
            ));
        };
        let Ok(method) = Method::from_bytes(r.method.as_bytes()) else {
            return error_response(not_allowed(role));
        };
        let (path, _query) = r.path.split_once('?').unwrap_or((r.path.as_str(), ""));
        let Some(rel) = path.strip_prefix("/api/v1") else {
            return error_response(ApiError::new(StatusCode::NOT_FOUND, "no_route", "There's no such API route."));
        };
        // Checked here and again in `api_guard` (the router's own gate).
        if !remote_scope::allowed(role, method.as_str(), rel) {
            return error_response(not_allowed(role));
        }
        // Owners may change settings from a phone, but not open this PC to the network.
        if method == Method::PUT && rel == "/settings" && lan_change(&self.engine, &r.body) {
            return error_response(
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    "not_allowed_for_role",
                    "LAN access can only change on the PC itself.",
                )
                .hint("Everything else in Settings works from here."),
            );
        }
        let mut builder =
            Request::builder().method(method.clone()).uri(r.path.as_str()).header(header::HOST, self.host.clone());
        let media = method == Method::GET && rel.starts_with("/media/");
        for (k, v) in &r.headers {
            let k = k.to_ascii_lowercase();
            let v = if k == "range" && media { clamp_range(v) } else { v.clone() };
            if (k == "content-type" || k == "accept" || (k == "range" && media))
                && let Ok(v) = HeaderValue::from_str(&v)
            {
                builder = builder.header(k.as_str(), v);
            }
        }
        let Ok(mut req) = builder.body(Body::from(r.body)) else {
            return error_response(ApiError::invalid("That request couldn't be read."));
        };
        req.extensions_mut().insert(RemotePrincipal { device_id: r.device_id, role });
        let res = match self.router.clone().oneshot(req).await {
            Ok(res) => res,
            Err(never) => match never {},
        };
        let status = res.status().as_u16();
        let headers: Vec<(String, String)> = PASSED_BACK
            .iter()
            .filter_map(|n| Some((n.as_str().to_string(), res.headers().get(n)?.to_str().ok()?.to_string())))
            .collect();
        match axum::body::to_bytes(res.into_body(), REMOTE_MAX_RESPONSE).await {
            Ok(body) => RemoteResponse { status, headers, body: body.to_vec() },
            Err(_) => error_response(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "That's too big to open through Ferrocast's servers (8 MB at most).",
            )),
        }
    }

    fn live_allowed(&self, role: &str, frame: &str) -> bool {
        Role::parse(role).is_some_and(|r| remote_scope::live_frame_allowed(r, frame))
    }
}

/// Whether a settings body would switch LAN access (sent and different from now).
fn lan_change(engine: &Engine, body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    match v.get("lan_access") {
        None => false,
        Some(want) => json!(engine.settings()).get("lan_access") != Some(want),
    }
}

/// The handler for `Engine::set_remote_handler`.
pub(crate) fn handler(router: Router, engine: Engine, port: u16) -> Arc<dyn RemoteHandler> {
    Arc::new(Handler::new(router, engine, port))
}

// ── Settings › Dashboard anywhere ────────────────────────────────────────────────────

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/pro/remote", get(view))
        .route("/pro/remote/codes", post(code))
        .route("/pro/remote/devices/{id}", delete(revoke))
}

/// `GET /pro/remote` → `{devices, audit, limits, pc_online, tunnel, available}`.
async fn view(State(app): State<AppState>) -> ApiResult<impl IntoResponse> {
    let v = app.engine.remote_view().await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], axum::Json(v)))
}

#[derive(Debug, Deserialize)]
struct CodeIn {
    role: String,
    #[serde(default)]
    name: Option<String>,
}

/// `POST /pro/remote/codes {role, name?}` → 201 `{code, url, expires_at, role, name}`.
async fn code(State(app): State<AppState>, JsonBody(b): JsonBody<CodeIn>) -> ApiResult<impl IntoResponse> {
    let v = app.engine.remote_code(&b.role, b.name.as_deref()).await?;
    Ok((StatusCode::CREATED, [(header::CACHE_CONTROL, "no-store")], axum::Json(v)))
}

/// `DELETE /pro/remote/devices/{id}` → 204.
async fn revoke(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.remote_revoke(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
