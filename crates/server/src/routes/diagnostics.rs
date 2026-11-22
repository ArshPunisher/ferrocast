//! Support diagnostics endpoints: preview likely causes + export redacted zip archive (`#7`).

use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use ferrocast_engine::studio_host::FilePick;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;
use crate::remote::RemotePrincipal;

fn ensure_local_loopback(
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    remote: Option<Extension<RemotePrincipal>>,
) -> ApiResult<()> {
    if remote.is_some() {
        return Err(ApiError::forbidden(
            "Support diagnostics are only available directly on this computer, not over remote access.",
        ));
    }
    let Some(Extension(ConnectInfo(addr))) = connect else {
        return Err(ApiError::forbidden("Support diagnostics require a direct local connection."));
    };
    let is_loopback = match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    };
    if !is_loopback {
        return Err(ApiError::forbidden("Support diagnostics are only available from this computer, not over LAN."));
    }
    Ok(())
}

/// `GET /api/v1/diagnostics/preview` -> redacted [`ferrocast_core::diagnostics::SupportReport`].
pub async fn preview(
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    remote: Option<Extension<RemotePrincipal>>,
    State(app): State<AppState>,
) -> ApiResult<Json<Value>> {
    ensure_local_loopback(connect, remote)?;
    let report = app.engine.diagnostics_preview().await?;
    Ok(Json(json!(report)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ReportRequest {
    /// Destination directory where `ferrocast-support-*.zip` is written. When omitted, opens
    /// the desktop folder picker (returning `{ cancelled: true }` if dismissed).
    pub folder: Option<String>,
}

/// `POST /api/v1/diagnostics/report` `{ folder? }` -> writes redacted zip into picked folder.
pub async fn write_report(
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    remote: Option<Extension<RemotePrincipal>>,
    State(app): State<AppState>,
    Body(b): Body<ReportRequest>,
) -> ApiResult<Json<Value>> {
    ensure_local_loopback(connect, remote)?;
    let folder = match b.folder.map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
        Some(f) => f,
        None => {
            let pick = FilePick { kind: "folder".into(), filter: String::new() };
            let picked = app.engine.studio_pick_file(pick).await?;
            match picked.as_str() {
                Some(path) if !path.trim().is_empty() => path.to_string(),
                _ => {
                    return Ok(Json(json!({
                        "cancelled": true,
                        "path": null,
                        "filename": null,
                        "size_bytes": 0,
                        "report": null,
                    })));
                }
            }
        }
    };

    let written = app.engine.write_diagnostics_report(std::path::Path::new(&folder)).await?;
    Ok(Json(json!({
        "cancelled": false,
        "path": written.path,
        "filename": written.filename,
        "size_bytes": written.size_bytes,
        "report": written.report,
    })))
}

/// `POST /api/v1/diagnostics/report.zip` -> streams the redacted support zip directly for browsers.
pub async fn download_zip(
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    remote: Option<Extension<RemotePrincipal>>,
    State(app): State<AppState>,
) -> ApiResult<impl IntoResponse> {
    ensure_local_loopback(connect, remote)?;
    let (filename, bytes, _) = app.engine.diagnostics_zip_bytes().await?;
    let disp = format!("attachment; filename=\"{filename}\"");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, disp),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    ))
}
