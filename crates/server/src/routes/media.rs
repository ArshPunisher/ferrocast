//! The media library: upload with clear 413/415 answers, list, delete.

use axum::Json;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_store::repo::media::{self, MediaKind, SaveError, UploadError};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

/// Largest file of any kind (videos).
pub const MAX_UPLOAD: usize = 50 * 1024 * 1024;

fn too_large(msg: &str) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", msg.to_string())
        .hint("Compress the file or pick a shorter one.")
}

pub async fn upload(
    State(app): State<AppState>,
    mut form: Multipart,
) -> ApiResult<impl IntoResponse> {
    let field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => {
                return Err(ApiError::invalid(
                    "Attach the file in a form field named \"file\".",
                ));
            }
            Err(e) => return Err(multipart_error(e)),
        }
    };
    let name = field.file_name().unwrap_or("file").to_string();
    let data = field.bytes().await.map_err(multipart_error)?;
    let saved = app
        .engine
        .db(move |s| Ok(media::save(s, &name, &data)))
        .await?;
    match saved {
        Ok(m) => {
            let url = format!("/api/v1/media/{}", m.id);
            Ok((
                StatusCode::CREATED,
                Json(
                    json!({ "id": m.id, "kind": m.kind, "name": m.name, "bytes": m.bytes, "url": url }),
                ),
            ))
        }
        Err(SaveError::Upload(UploadError::TooBig(msg))) => Err(too_large(msg)),
        Err(SaveError::Upload(UploadError::UnsupportedType)) => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
            UploadError::UnsupportedType.to_string(),
        )),
        Err(SaveError::Upload(UploadError::Empty)) => Err(ApiError::invalid("The file is empty.")),
        Err(SaveError::Store(e)) => Err(e.into()),
    }
}

fn multipart_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large(MediaKind::Video.limit_label())
    } else {
        ApiError::invalid(format!("The upload couldn't be read: {}", e.body_text()))
    }
}

pub async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let items = app.engine.db(media::list).await?;
    let out: Vec<Value> = items
        .into_iter()
        .map(|m| json!({ "id": m.id, "kind": m.kind, "name": m.name, "bytes": m.bytes, "url": format!("/api/v1/media/{}", m.id) }))
        .collect();
    Ok(Json(json!(out)))
}

pub async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let used_by: Vec<String> = app
        .engine
        .rules()
        .iter()
        .flat_map(|r| r.variations.iter().map(move |v| (r, v)))
        .filter(|(_, v)| v.image.as_deref() == Some(&id) || v.sound.as_deref() == Some(&id))
        .map(|(r, v)| format!("{} → {}", r.name, v.name))
        .collect();
    if !used_by.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "in_use",
            format!("This file is used by: {}.", used_by.join(", ")),
        )
        .hint("Change those alerts to another file first."));
    }
    app.engine.db(move |s| media::delete(s, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
