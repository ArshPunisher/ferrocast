//! An account's media library in the cloud (docs/CLOUD.md §5, §9): images, sounds and short
//! videos for designs and alerts. Checked by content (magic bytes, never the name), 20 MB a
//! file, 200 MB an account, kept in the blob store under `cloud/<account>/<sha256>` (the same
//! file twice is kept once), served only as an image, sound or video.
//!
//! Uploads are read into memory (at most 20 MB), so only a few run at once relay-wide (the
//! relay container has 1 GB, §8); one more waits for none and gets 503 `cloud_busy`.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};

use super::store::Media;
use super::{FILE_LIMIT, STORAGE_LIMIT, app_account, created, e, json, no_store, web_account};
use crate::{ApiError, AppState, now_ms};

/// Uploads read at once relay-wide (each up to 20 MB in memory).
pub const UPLOAD_SLOTS: usize = 4;

pub(super) fn routes() -> Router<AppState> {
    let limit = axum::extract::DefaultBodyLimit::max(usize::try_from(FILE_LIMIT).unwrap_or(20_000_000) + 64 * 1024);
    Router::new()
        .route("/c/api/v1/media", get(list).post(upload_web).layer(limit))
        .route("/c/api/v1/media/{id}", get(get_web).delete(delete_web))
        .route("/v1/cloud/media", axum::routing::post(upload_app).layer(limit))
        .route("/v1/cloud/media/{id}", get(get_app))
}

pub(crate) fn blob_key(account: &str, sha256: &str) -> String {
    format!("cloud/{account}/{sha256}")
}

fn kind_of(mime: &str) -> &'static str {
    match mime.split('/').next() {
        Some("image") => "image",
        Some("audio") => "sound",
        _ => "video",
    }
}

/// The type from the first bytes (SVG and HTML never: they can carry scripts).
pub(crate) fn sniff(head: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| head.starts_with(sig);
    let at = |off: usize, sig: &[u8]| head.get(off..off + sig.len()) == Some(sig);
    Some(if starts(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if starts(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        "image/gif"
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if starts(b"RIFF") && at(8, b"WAVE") {
        "audio/wav"
    } else if starts(b"OggS") {
        "audio/ogg"
    } else if starts(b"ID3") || (head.len() > 1 && head[0] == 0xff && head[1] & 0xe0 == 0xe0) {
        "audio/mpeg"
    } else if starts(b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else if at(4, b"ftyp") {
        "video/mp4"
    } else {
        return None;
    })
}

fn too_large() -> ApiError {
    e(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("That file is over the {} limit for one file.", crate::blob::human_bytes(FILE_LIMIT as u64)),
    )
    .hint("Make it smaller (a shorter clip, a smaller picture) and upload it again.")
}

fn unsupported() -> ApiError {
    e(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media",
        "That file type isn't supported. Use PNG, JPEG, GIF or WebP for pictures, MP3, OGG or WAV for sounds, and WebM or MP4 for videos.",
    )
    .hint("Save it in one of those formats and upload it again.")
}

fn storage_full(used: i64, adding: i64) -> ApiError {
    let h = |n: i64| crate::blob::human_bytes(u64::try_from(n).unwrap_or(0));
    e(
        StatusCode::PAYLOAD_TOO_LARGE,
        "cloud_storage_full",
        format!(
            "Your cloud media is full: {} of {} used, and this file needs {}.",
            h(used),
            h(STORAGE_LIMIT),
            h(adding)
        ),
    )
    .hint("Delete files in the media library you don't use, then upload it again.")
}

fn busy() -> ApiError {
    e(StatusCode::SERVICE_UNAVAILABLE, "cloud_busy", "Ferrocast's servers are taking other uploads right now.")
        .hint("Try again in a minute.")
}

/// The file's name: what the browser or the app said, cleaned, 1 to 120 characters.
fn clean_name(raw: Option<&str>) -> String {
    let n: String = raw.unwrap_or("").chars().filter(|c| !c.is_control()).take(120).collect();
    let n = n.trim();
    if n.is_empty() { "Upload".to_string() } else { n.to_string() }
}

/// Reads a body of at most [`FILE_LIMIT`] bytes.
async fn read_capped(
    mut stream: impl futures_util::Stream<Item = Result<Bytes, ApiError>> + Unpin,
) -> Result<Vec<u8>, ApiError> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > usize::try_from(FILE_LIMIT).unwrap_or(usize::MAX) {
            return Err(too_large());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn bad_body() -> ApiError {
    e(StatusCode::BAD_REQUEST, "bad_upload", "The upload broke off before the whole file arrived.")
        .hint("Check your connection and upload it again.")
}

/// The bytes and name of an upload: multipart `file` (the dashboard) or the raw file with
/// `X-Media-Name` (the app).
async fn read_upload(st: &AppState, req: Request) -> Result<(Vec<u8>, String), ApiError> {
    let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, st).await.map_err(|_| bad_body())?;
        while let Some(field) = mp.next_field().await.map_err(|_| bad_body())? {
            if field.name() != Some("file") {
                continue;
            }
            let name = clean_name(field.file_name());
            let stream = Box::pin(field.map(|c| c.map_err(|_| bad_body())));
            let bytes = read_capped(stream).await?;
            return Ok((bytes, name));
        }
        return Err(e(StatusCode::BAD_REQUEST, "no_file", "There's no file in the upload.")
            .hint("Send the file in a field named \"file\"."));
    }
    let name = clean_name(req.headers().get("x-media-name").and_then(|v| v.to_str().ok()));
    let stream = Box::pin(req.into_body().into_data_stream().map(|c| c.map_err(|_| bad_body())));
    Ok((read_capped(stream).await?, name))
}

fn view(m: &Media, url_base: &str) -> Value {
    json!({ "id": m.id, "kind": m.kind, "name": m.name, "mime": m.mime, "bytes": m.bytes, "sha256": m.sha256,
            "created_at": m.created_at, "url": format!("{url_base}/{}", m.id) })
}

/// Stores an upload: checked by content, the per-account room, the same file kept once.
async fn store(st: &AppState, account: &str, bytes: Vec<u8>, name: String) -> Result<(Media, bool), ApiError> {
    if bytes.is_empty() {
        return Err(e(StatusCode::BAD_REQUEST, "empty", "The file is empty.").hint("Pick the file again."));
    }
    let mime = sniff(&bytes).ok_or_else(unsupported)?;
    let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if let Some(m) = st.db.cloud_media_by_sha(account, &sha).map_err(ApiError::db)? {
        return Ok((m, false));
    }
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let used = st.db.cloud_media_bytes(account).map_err(ApiError::db)?;
    if used + size > STORAGE_LIMIT {
        return Err(storage_full(used, size));
    }
    let blobs = st.blobs()?;
    let key = blob_key(account, &sha);
    blobs.put(&key, Bytes::from(bytes)).await.map_err(crate::blob::BlobError::into_api)?;
    let m = Media {
        id: super::docs::new_id(),
        sha256: sha,
        kind: kind_of(mime).to_string(),
        mime: mime.to_string(),
        bytes: size,
        name,
        created_at: now_ms(),
    };
    // The room is checked again in the same transaction, so two uploads at once can't pass it.
    if !st.db.cloud_add_media(account, &m, STORAGE_LIMIT).map_err(ApiError::db)? {
        if !matches!(st.db.cloud_media_by_sha(account, &m.sha256), Ok(Some(_))) {
            let _ = blobs.delete(&key).await;
        }
        let used = st.db.cloud_media_bytes(account).map_err(ApiError::db)?;
        return Err(storage_full(used, size));
    }
    Ok((m, true))
}

async fn upload(st: &AppState, account: &str, req: Request, url_base: &str) -> Result<Response, ApiError> {
    let Ok(_slot) = st.cloud.uploads.try_acquire() else { return Err(busy()) };
    let (bytes, name) = read_upload(st, req).await?;
    let (m, new) = store(st, account, bytes, name).await?;
    let v = view(&m, url_base);
    Ok(if new { created(v) } else { json(v) })
}

async fn upload_web(State(st): State<AppState>, req: Request) -> Result<Response, ApiError> {
    let a = web_account(&st, req.headers(), &Method::POST, true)?;
    upload(&st, &a.account.id, req, "/c/api/v1/media").await
}

async fn upload_app(State(st): State<AppState>, req: Request) -> Result<Response, ApiError> {
    let a = app_account(&st, req.headers(), &Method::POST)?;
    super::must_be_on(&st, &a.account.id)?;
    upload(&st, &a.account.id, req, "/v1/cloud/media").await
}

async fn list(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let list = st.db.cloud_media(&a.account.id).map_err(ApiError::db)?;
    Ok(json(json!(list.iter().map(|m| view(m, "/c/api/v1/media")).collect::<Vec<_>>())))
}

async fn get_web(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    file(&st, &a.account.id, &id, &headers).await
}

async fn get_app(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = app_account(&st, &headers, &method)?;
    super::must_be_on(&st, &a.account.id)?;
    file(&st, &a.account.id, &id, &headers).await
}

async fn file(st: &AppState, account: &str, id: &str, headers: &HeaderMap) -> Result<Response, ApiError> {
    if st.db.cloud_media_one(account, id).map_err(ApiError::db)?.is_none() {
        return Err(super::not_found("file"));
    }
    Ok(super::overlay::media(st, account, id, headers).await)
}

/// Which of the account's designs or alert rules use a file.
fn used_by(st: &AppState, account: &str, id: &str) -> Result<Option<String>, ApiError> {
    for d in super::docs::designs(st, account, None)?.into_iter().filter(|d| !d.built_in) {
        if d.media_ids().iter().any(|m| m == id) {
            return Ok(Some(format!("the design \"{}\"", d.name)));
        }
    }
    for r in super::docs::rules(st, account)? {
        if r.variations.iter().any(|v| v.image.as_deref() == Some(id) || v.sound.as_deref() == Some(id)) {
            return Ok(Some(format!("the alert rule \"{}\"", r.name)));
        }
    }
    Ok(None)
}

async fn delete_web(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    if st.db.cloud_media_one(account, &id).map_err(ApiError::db)?.is_none() {
        return Err(super::not_found("file"));
    }
    if let Some(what) = used_by(&st, account, &id)? {
        return Err(e(StatusCode::CONFLICT, "in_use", format!("This file is used by {what}."))
            .hint("Pick another file there first, then delete this one."));
    }
    if let Some((m, shared)) = st.db.cloud_delete_media(account, &id).map_err(ApiError::db)?
        && !shared
        && let Ok(blobs) = st.blobs()
        && let Err(err) = blobs.delete(&blob_key(account, &m.sha256)).await
    {
        tracing::warn!(error = %err, "couldn't delete a cloud media file");
    }
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

/// Deletes every file of an account (it was deleted). Best effort.
pub(crate) async fn delete_all(st: &AppState, account: &str) {
    let Ok(blobs) = st.blobs() else { return };
    for m in st.db.cloud_media(account).unwrap_or_default() {
        let _ = blobs.delete(&blob_key(account, &m.sha256)).await;
    }
}
