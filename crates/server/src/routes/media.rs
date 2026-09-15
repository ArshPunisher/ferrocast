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

pub async fn upload(State(app): State<AppState>, mut form: Multipart) -> ApiResult<impl IntoResponse> {
    let field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => {
                return Err(ApiError::invalid("Attach the file in a form field named \"file\"."));
            }
            Err(e) => return Err(multipart_error(e)),
        }
    };
    let name = field.file_name().unwrap_or("file").to_string();
    let data = field.bytes().await.map_err(multipart_error)?;
    let saved = app.engine.db(move |s| Ok(media::save(s, &name, &data))).await?;
    match saved {
        Ok(m) => {
            let url = format!("/api/v1/media/{}", m.id);
            Ok((
                StatusCode::CREATED,
                Json(json!({ "id": m.id, "kind": m.kind, "name": m.name, "bytes": m.bytes, "url": url })),
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
    let used_by: Vec<String> = used_by
        .into_iter()
        .chain(app.engine.designs_using_media(&id).await?.into_iter().map(|d| format!("the design {d}")))
        .collect();
    if !used_by.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "in_use",
            format!("This file is used by: {}.", used_by.join(", ")),
        )
        .hint("Change those alerts or designs to another file first."));
    }
    app.engine.db(move |s| media::delete(s, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes export files older than an hour (ones Windows couldn't delete while sending).
async fn clear_old_exports(dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name();
        let old = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(3600));
        if old && name.to_string_lossy().starts_with("export-") {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

/// `POST /api/v1/export`: the whole setup as a `.ferropack` download.
pub async fn export(State(app): State<AppState>) -> ApiResult<axum::response::Response> {
    let dir = app.engine.store().dir().join("tmp");
    clear_old_exports(&dir).await;
    let path = dir.join(format!("export-{}.ferropack", ferrocast_store::new_id()));
    let p = path.clone();
    app.engine
        .db(move |s| {
            std::fs::create_dir_all(p.parent().unwrap_or(std::path::Path::new(".")))?;
            ferrocast_store::pack::export(s, &p, env!("CARGO_PKG_VERSION"))
        })
        .await?;
    // Streamed: a pack holds the whole media library and can be gigabytes.
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ApiError::from(ferrocast_store::StoreError::Io(e)));
        }
    };
    let len = file.metadata().await.map(|m| m.len()).ok();
    // The open handle keeps the data readable on Unix; Windows can't delete an open file,
    // so leftovers there are cleared by the next export.
    #[cfg(unix)]
    let _ = tokio::fs::remove_file(&path).await;
    let name = format!("ferrocast-{}.ferropack", ferrocast_engine::now().0 / 1000);
    let mut headers = vec![
        (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
    ];
    if let Some(len) = len {
        headers.push((axum::http::header::CONTENT_LENGTH, len.to_string()));
    }
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut res = body.into_response();
    for (k, v) in headers {
        if let Ok(v) = axum::http::HeaderValue::from_str(&v) {
            res.headers_mut().insert(k, v);
        }
    }
    Ok(res)
}

/// `POST /api/v1/import`: multipart `file` (a `.ferropack`). Takes a backup first. The
/// upload streams to disk, so a pack full of videos never sits in memory.
pub async fn import(State(app): State<AppState>, mut form: Multipart) -> ApiResult<Json<Value>> {
    use tokio::io::AsyncWriteExt;
    let mut field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(ApiError::invalid("Attach the .ferropack in a form field named \"file\".")),
            Err(e) => return Err(pack_upload_error(e)),
        }
    };
    let io = |e: std::io::Error| ApiError::from(ferrocast_store::StoreError::Io(e));
    let dir = app.engine.store().dir().join("tmp");
    let path = dir.join(format!("import-{}.ferropack", ferrocast_store::new_id()));
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let mut out = tokio::fs::File::create(&path).await.map_err(io)?;
    let mut size: u64 = 0;
    let copied: ApiResult<()> = async {
        while let Some(chunk) = field.chunk().await.map_err(pack_upload_error)? {
            size += chunk.len() as u64;
            if size > ferrocast_store::pack::MAX_PACK {
                return Err(pack_too_large());
            }
            out.write_all(&chunk).await.map_err(io)?;
        }
        out.flush().await.map_err(io)?;
        Ok(())
    }
    .await;
    drop(out);
    if let Err(e) = copied {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(ApiError::invalid("The file is empty.").hint("Pick a .ferropack you exported from Ferrocast."));
    }
    let p = path.clone();
    let result = app.engine.db(move |s| ferrocast_store::pack::import(s, &p)).await;
    let _ = tokio::fs::remove_file(&path).await;
    let report = result.map_err(|e| match e {
        ferrocast_engine::EngineError::Store(ferrocast_store::StoreError::Pack(p)) => pack_error(p),
        other => ApiError::from(other),
    })?;
    app.engine.reload().await?;
    Ok(Json(json!(report)))
}

fn pack_error(e: ferrocast_store::PackError) -> ApiError {
    use ferrocast_store::PackError as P;
    let message = e.to_string();
    match e {
        P::NotAPack | P::Unsafe(_) => ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_pack", message)
            .hint("Pick a .ferropack file you exported from Ferrocast."),
        P::Corrupt(_) => ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "damaged_pack", message)
            .hint("The file may not have finished downloading or copying. Export it again."),
        P::TooNew(_) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "pack_too_new", message)
            .hint("Update Ferrocast, then import the pack again."),
        P::TooLarge(_) => ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message),
    }
}

fn pack_too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("Packs can be up to {} GB.", ferrocast_store::pack::MAX_PACK / 1024 / 1024 / 1024),
    )
    .hint("Remove large videos from the media library on the other computer, then export again.")
}

fn pack_upload_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        pack_too_large()
    } else {
        ApiError::invalid(format!("The upload didn't arrive whole: {}", e.body_text()))
            .hint("Try again; if it keeps failing, export the pack again.")
    }
}
