//! The loopback HTTP + WebSocket server (ARCHITECTURE.md §1, API.md).
//!
//! Serves the dashboard at `/app`, its API at `/api/v1`, overlays at `/o/<token>/`, all on
//! 127.0.0.1 by default. The desktop window and a paired browser use exactly the same API.

mod assets;
mod auth;
mod error;
mod extract;
mod routes;
mod ws;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use ferrocast_engine::Engine;
use tokio::net::TcpListener;

pub use auth::Sessions;
pub use error::ApiError;

/// Default port; the next free one is used when it's taken.
pub const DEFAULT_PORT: u16 = 47820;
/// Largest JSON body the API accepts.
pub const JSON_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub port: u16,
    /// Listen on every interface (phone on the same Wi-Fi). Off by default (R-43).
    pub lan: bool,
    /// Extra origins allowed to call the API, for frontend development.
    pub dev_origins: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        let dev = std::env::var("FERROCAST_DEV_ORIGIN").ok().into_iter().filter(|s| !s.is_empty()).collect();
        ServeOptions { port: DEFAULT_PORT, lan: false, dev_origins: dev }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub engine: Engine,
    pub sessions: Arc<Sessions>,
    pub guard: auth::Guard,
}

/// A running server.
#[derive(Debug)]
pub struct Server {
    pub addr: SocketAddr,
    pub sessions: Arc<Sessions>,
    pub task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

/// Binds and starts serving. Tries `opts.port`, then the next 20 ports.
pub async fn serve(engine: Engine, opts: ServeOptions) -> std::io::Result<Server> {
    let ip = if opts.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    let mut last_err = None;
    let mut listener = None;
    for port in opts.port..opts.port.saturating_add(20) {
        match TcpListener::bind((ip, port)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => TcpListener::bind((ip, 0)).await.map_err(|e| last_err.unwrap_or(e))?,
    };
    let addr = listener.local_addr()?;
    let sessions = Arc::new(Sessions::new());
    let state = AppState {
        engine: engine.clone(),
        sessions: sessions.clone(),
        guard: auth::Guard { port: addr.port(), lan: opts.lan, dev_origins: opts.dev_origins },
    };
    engine.set_base_url(&format!("http://127.0.0.1:{}", addr.port()));
    let app = router(state);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "the local server stopped");
        }
    });
    tracing::info!(%addr, "local server listening");
    Ok(Server { addr, sessions, task })
}

pub(crate) fn router(state: AppState) -> Router {
    let api = routes::api()
        .layer(DefaultBodyLimit::max(JSON_LIMIT))
        .merge(routes::uploads())
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no_route", "There's no such API route.") })
        .layer(middleware::from_fn_with_state(state.clone(), api_guard));
    Router::new()
        .nest("/api/v1", api)
        .merge(ws::routes())
        .merge(assets::routes())
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .with_state(state)
}

/// Every request: the Host must be ours (anti DNS-rebinding).
async fn host_guard(State(app): State<AppState>, req: Request, next: Next) -> Response {
    if !app.guard.host_ok(req.headers()) {
        return ApiError::forbidden("This address isn't allowed to reach Ferrocast.")
            .hint("Open it at http://127.0.0.1, or turn on LAN access in Settings.")
            .into_response();
    }
    next.run(req).await
}

/// API requests: Origin check, CORS for dev origins, and the session token.
async fn api_guard(State(app): State<AppState>, req: Request, next: Next) -> Response {
    if !app.guard.origin_ok(req.headers()) {
        return ApiError::forbidden("Requests from other websites aren't allowed.").into_response();
    }
    let dev_origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .filter(|o| app.guard.dev_origins.iter().any(|d| d == o))
        .map(str::to_string);
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        add_cors(&mut res, dev_origin.as_deref());
        return res;
    }
    let path = req.uri().path();
    let open = path.ends_with("/session/pair") || path.ends_with("/live");
    if !open {
        // GETs may carry the token as ?session= so <img>/<audio> previews can load media.
        let query_token =
            (req.method() == Method::GET).then(|| query_param(req.uri().query().unwrap_or(""), "session")).flatten();
        let token = auth::bearer(req.headers()).map(str::to_string).or(query_token);
        let ok = token.is_some_and(|t| app.sessions.valid(&t));
        if !ok {
            let mut res = ApiError::unauthorized().into_response();
            add_cors(&mut res, dev_origin.as_deref());
            return res;
        }
    }
    let mut res = next.run(req).await;
    add_cors(&mut res, dev_origin.as_deref());
    res
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}

fn add_cors(res: &mut Response, origin: Option<&str>) {
    let Some(origin) = origin.and_then(|o| HeaderValue::from_str(o).ok()) else {
        return;
    };
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, DELETE"));
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests;
