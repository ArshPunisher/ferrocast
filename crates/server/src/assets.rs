//! The embedded dashboard and overlay bundles, and media files with byte ranges.

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use rust_embed::Embed;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::AppState;
use crate::error::ApiError;

#[derive(Embed)]
#[folder = "$FERROCAST_WEB_DIR/dashboard"]
struct Dashboard;

#[derive(Embed)]
#[folder = "$FERROCAST_WEB_DIR/overlays"]
struct Overlays;

/// The dashboard may load avatars and emotes from platform CDNs; scripts only from itself.
const DASHBOARD_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob: https:; media-src 'self' blob: data:; font-src 'self' data:; \
    connect-src 'self' ws://127.0.0.1:* ws://localhost:*; frame-src 'self'; frame-ancestors 'none'; \
    base-uri 'none'; form-action 'none'";

/// Overlays: same, but they're embedded in OBS, never framed by websites.
const OVERLAY_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https:; media-src 'self' data: blob:; font-src 'self' data: https:; \
    connect-src 'self' ws://127.0.0.1:* ws://localhost:*; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/app/") }))
        .route("/app", get(|| async { Redirect::permanent("/app/") }))
        .route("/app/", get(dashboard_index))
        .route("/app/{*path}", get(dashboard_file))
        .route("/o/{token}", get(overlay_redirect))
        .route("/o/{token}/", get(overlay_index))
        .route("/o/{token}/{*path}", get(overlay_file))
}

fn file_response<E: Embed>(path: &str, csp: &str) -> Option<Response> {
    let file = E::get(path)?;
    let mime = file.metadata.mimetype().to_string();
    let mut res = Response::new(Body::from(file.data.into_owned()));
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // Vite fingerprints files under assets/, so they can be cached for good; HTML can't.
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if path.ends_with(".html")
        && let Ok(v) = HeaderValue::from_str(csp)
    {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    Some(res)
}

async fn dashboard_index() -> Response {
    file_response::<Dashboard>("index.html", DASHBOARD_CSP).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// Real files are served; any other path without an extension is a client-side route.
async fn dashboard_file(Path(path): Path<String>) -> Response {
    if let Some(r) = file_response::<Dashboard>(&path, DASHBOARD_CSP) {
        return r;
    }
    let looks_like_file = path.rsplit('/').next().is_some_and(|last| last.contains('.'));
    if looks_like_file {
        return StatusCode::NOT_FOUND.into_response();
    }
    dashboard_index().await
}

async fn overlay_redirect(Path(token): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/o/{}/", token.chars().filter(char::is_ascii_hexdigit).collect::<String>()))
}

async fn overlay_index(State(app): State<AppState>, Path(token): Path<String>) -> Response {
    match app.engine.overlay_by_token(&token).await {
        Ok(Some(_)) => file_response::<Overlays>("index.html", OVERLAY_CSP)
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
        _ => (StatusCode::NOT_FOUND, "This overlay URL doesn't exist anymore. Copy it again from Ferrocast.")
            .into_response(),
    }
}

async fn overlay_file(
    State(app): State<AppState>,
    Path((token, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !matches!(app.engine.overlay_by_token(&token).await, Ok(Some(_))) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(id) = path.strip_prefix("media/") {
        return media_response(&app, id, &headers).await;
    }
    file_response::<Overlays>(&path, OVERLAY_CSP).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// `GET /api/v1/media/{id}` for dashboard previews (auth checked by the API guard).
pub async fn dashboard_media(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    media_response(&app, &id, &headers).await
}

/// Serves a media file with `Range` support so OBS can seek in videos (Tauri's asset
/// protocol couldn't, R-41).
async fn media_response(app: &AppState, id: &str, headers: &HeaderMap) -> Response {
    let mid = id.to_string();
    let found = app.engine.db(move |s| ferrocast_store::repo::media::get(s, &mid)).await;
    let (media, path): (_, PathBuf) = match found {
        Ok(Some(x)) => x,
        Ok(None) => return ApiError::not_found("file").into_response(),
        Err(e) => return ApiError::from(e).into_response(),
    };
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return ApiError::not_found("file").into_response();
    };
    let len = media.bytes;
    let range = headers.get(header::RANGE).and_then(|r| r.to_str().ok()).and_then(|r| parse_range(r, len));
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, media.mime.as_str())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    match range {
        Some((start, end)) => {
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            }
            let count = end - start + 1;
            builder = builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, count);
            builder
                .body(Body::from_stream(ReaderStream::new(file.take(count))))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None if headers.contains_key(header::RANGE) => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => builder
            .header(header::CONTENT_LENGTH, len)
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

/// Parses a single `bytes=a-b`, `bytes=a-` or `bytes=-n` range. Multi-range requests and
/// anything malformed or out of bounds return `None`.
pub(crate) fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            (len.saturating_sub(n), len - 1)
        }
        (s, "") => (s.parse().ok()?, len - 1),
        (s, e) => (s.parse().ok()?, e.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}
