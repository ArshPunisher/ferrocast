//! StreamElements and Streamlabs importers (API.md §4, "Importing from StreamElements and
//! Streamlabs"): preview first, nothing written; then apply.

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use ferrocast_engine::import::streamlabs::{self, Files, Upload};
use ferrocast_store::import::OnConflict;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// Body limit for the Streamlabs upload route: both files at their limit, plus the form.
pub const MAX_STREAMLABS_BODY: usize = 2 * streamlabs::MAX_FILE + 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct StreamElementsBody {
    token: String,
}

/// `POST /api/v1/import/streamelements/preview { token }`. The token is used for this
/// request only and never saved.
pub async fn streamelements_preview(
    State(app): State<AppState>,
    Body(b): Body<StreamElementsBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.preview_streamelements(&b.token).await?)))
}

fn upload_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large("upload")
    } else {
        ApiError::invalid(format!("The upload didn't arrive whole: {}", e.body_text()))
            .hint("Pick the files again and retry.")
    }
}

fn too_large(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("The {what} is too big: each file can be up to {} MB.", streamlabs::MAX_FILE / 1024 / 1024),
    )
    .hint("Export only the columns you need, or split the file, then try again.")
}

/// `POST /api/v1/import/streamlabs/preview`: multipart `platform` (twitch, youtube or kick;
/// needed with `points`), `points` and/or `tips` files (CSV or .xlsx, up to 10 MB each).
pub async fn streamlabs_preview(State(app): State<AppState>, mut form: Multipart) -> ApiResult<Json<Value>> {
    let mut files = Files::default();
    while let Some(mut field) = form.next_field().await.map_err(upload_error)? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "platform" => {
                let text = field.text().await.map_err(upload_error)?;
                files.platform = Some(super::platform(text.trim()).map_err(|e| {
                    ApiError::invalid(format!("platform: {}", e.message)).hint("Pick where these viewers watch you.")
                })?);
            }
            "points" | "tips" => {
                let file_name = field.file_name().unwrap_or("file").to_string();
                let label = if name == "points" { "points file" } else { "tips file" };
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(upload_error)? {
                    if bytes.len() + chunk.len() > streamlabs::MAX_FILE {
                        return Err(too_large(label));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let up = Some(Upload { file_name, bytes });
                if name == "points" { files.points = up } else { files.tips = up }
            }
            _ => {}
        }
    }
    Ok(Json(json!(app.engine.preview_streamlabs(files).await?)))
}

#[derive(Debug, Deserialize)]
pub struct ApplyBody {
    preview_id: String,
    on_conflict: OnConflict,
}

/// `POST /api/v1/import/apply { preview_id, on_conflict: "keep" | "replace" }`.
pub async fn apply(State(app): State<AppState>, Body(b): Body<ApplyBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.apply_import(&b.preview_id, b.on_conflict).await?)))
}
