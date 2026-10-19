//! Ferrocast Pro F5, cloud backup and sync (docs/plan/PRO.md F5, milestone M3; API.md §9.3).
//!
//! The desktop exports its `.ferropack`, encrypts it on the PC with `age` and uploads the
//! ciphertext here in parts of [`PART_SIZE`]: `POST /v1/pro/backups` says how big it is and
//! its SHA-256, each part goes up as `application/octet-stream`, and `complete` joins them
//! and checks the size and hash (a mismatch deletes the upload). The relay never sees the
//! plaintext or a key: it keeps opaque files in the `BlobStore` (`backups/<sub>/<id>`) and a
//! row of sizes and dates. Backups belong to the subscription (its PCs share them), at most
//! [`MAX_BACKUPS`] and [`MAX_TOTAL`] bytes; when a new one doesn't fit, the oldest automatic
//! ones go first. Unfinished uploads go after a day; everything goes 30 days after Pro
//! ends, and until then the list, download and delete stay open (R-04).

pub(crate) mod store;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures_util::{StreamExt, TryStreamExt};
use rsa::sha2::{Digest, Sha256};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) use store::SCHEMA;
use store::{COMPLETE, Row, UPLOADING};

use crate::billing::gate::{ProCtx, pro_required, require_pro};
use crate::blob::{BlobError, human_bytes};
use crate::quota::{Limit, Period};
use crate::{ApiError, AppState, authed, now_ms};

/// Bytes per part (8 MiB); the last part is the rest.
pub const PART_SIZE: i64 = 8 * 1024 * 1024;
/// Everything a subscription keeps, together.
pub const MAX_TOTAL: i64 = 250_000_000;
/// Backups a subscription keeps.
pub const MAX_BACKUPS: usize = 7;
/// An upload with no part for this long is gone at the next sweep.
pub const STALE_UPLOAD_MS: i64 = 24 * 3_600_000;
/// An upload another PC is still sending blocks a new one for this long after its last part;
/// after that (or from the same PC) a new upload replaces it.
pub const BUSY_MS: i64 = 3_600_000;
/// Backups stay this long after Pro ends.
pub const KEEP_AFTER_LAPSE_MS: i64 = 30 * 86_400_000;

/// Uploads per subscription per UTC day (listed in `/v1/pro/status` `quotas`).
pub const UPLOADS: Limit = Limit { name: "backup_uploads", period: Period::Day, limit: 6 };

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/backups", get(list).post(start))
        .route("/v1/pro/backups/{id}", get(download).delete(remove))
        .route("/v1/pro/backups/{id}/parts/{n}", put(put_part))
        .route("/v1/pro/backups/{id}/complete", post(complete))
}

/// `features.backups`: built, and the relay has somewhere to keep files.
pub(crate) fn available(st: &AppState) -> bool {
    st.blobs().is_ok()
}

/// Its per-subscription counters, listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[UPLOADS];

// ── Keys and checks ──────────────────────────────────────────────────────────────────

/// The finished file.
pub(crate) fn blob_key(sub: &str, id: &str) -> String {
    format!("backups/{sub}/{id}")
}

/// Part `n` while it waits to be joined.
fn part_key(sub: &str, id: &str, n: i64) -> String {
    format!("backups/{sub}/{id}.part{n}")
}

fn new_id() -> String {
    let mut b = [0u8; 12];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    format!("bk_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

fn id_ok(id: &str) -> bool {
    id.len() == 27 && id.starts_with("bk_") && id[3..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's no cloud backup with that id.")
        .hint("Reload the list: it may have been deleted from another PC.")
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_backup", message)
}

fn quota_full() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "backup_quota",
        format!(
            "Cloud backups can use {} and {MAX_BACKUPS} backups in all, and this one doesn't fit.",
            human_bytes(MAX_TOTAL as u64)
        ),
    )
    .hint("Delete an older backup you made yourself in Settings › Your data, then try again.")
}

/// The gate, plus a storage check, for routes that write.
async fn gate_write(st: &AppState, headers: &HeaderMap) -> Result<ProCtx, ApiError> {
    let ctx = require_pro(st, headers).await?;
    st.blobs()?;
    Ok(ctx)
}

/// Reading and deleting stay open after Pro ends while backups are kept (R-04): 402 only
/// without a subscription, or a lapsed one with nothing kept.
async fn gate_read(st: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let install = authed(st, headers)?;
    let sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?.ok_or_else(pro_required)?;
    let pro = crate::billing::access::is_pro(&sub.access(), now_ms());
    if !pro && !st.db.has_backups(&sub.id).map_err(ApiError::db)? {
        return Err(pro_required());
    }
    Ok(sub.id)
}

/// A JSON body, read as bytes so the gate answers before the body is judged.
#[derive(Debug)]
struct RawJson(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawJson {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawJson).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "That request is too large.")
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid", "The request body couldn't be read.")
            }
        })
    }
}

fn content_type(headers: &HeaderMap) -> String {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn parse<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, ApiError> {
    if content_type(headers) != "application/json" {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send this as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<T>(body).map_err(|e| {
        let what = if e.is_syntax() || e.is_eof() { "isn't JSON" } else { "has a missing or wrong field" };
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("The request {what}: {e}"))
    })
}

// ── Quota and pruning ────────────────────────────────────────────────────────────────

/// Which complete backups must go so one more of `bytes` fits: the oldest automatic ones,
/// oldest first. `None` when it can't fit even then (only backups made by hand are left).
pub(crate) fn to_prune(kept: &[Row], bytes: i64) -> Option<Vec<String>> {
    let mut total: i64 = kept.iter().map(|b| b.bytes).sum::<i64>() + bytes;
    let mut count = kept.len() + 1;
    let mut gone = Vec::new();
    for b in kept.iter().filter(|b| b.reason == "automatic") {
        if total <= MAX_TOTAL && count <= MAX_BACKUPS {
            break;
        }
        total -= b.bytes;
        count -= 1;
        gone.push(b.id.clone());
    }
    (total <= MAX_TOTAL && count <= MAX_BACKUPS).then_some(gone)
}

/// Removes a backup's files (its parts too) and its row. Missing files are fine.
async fn forget(st: &AppState, b: &Row) {
    if let Ok(blobs) = st.blobs() {
        for n in 0..b.parts() {
            if let Err(e) = blobs.delete(&part_key(&b.sub_id, &b.id, n)).await {
                tracing::warn!(error = %e, "couldn't delete a backup part");
            }
        }
        if let Err(e) = blobs.delete(&blob_key(&b.sub_id, &b.id)).await {
            tracing::warn!(error = %e, "couldn't delete a cloud backup file");
        }
    }
    if let Err(e) = st.db.delete_backup(&b.id) {
        tracing::warn!(error = %e, "couldn't delete a cloud backup row");
    }
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StartIn {
    bytes: i64,
    sha256: String,
    app_version: String,
    reason: String,
}

/// `POST /v1/pro/backups {bytes, sha256, app_version, reason}` → 201 `{id, part_size, parts}`.
async fn start(State(st): State<AppState>, headers: HeaderMap, RawJson(body): RawJson) -> Result<Response, ApiError> {
    let ctx = gate_write(&st, &headers).await?;
    let b: StartIn = parse(&headers, &body)?;
    if b.bytes <= 0 {
        return Err(invalid("bytes: the encrypted backup's size, more than 0."));
    }
    if b.bytes > MAX_TOTAL {
        return Err(quota_full());
    }
    let sha = b.sha256.trim().to_ascii_lowercase();
    if sha.len() != 64 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid("sha256: 64 hex characters, the SHA-256 of the encrypted backup."));
    }
    let version = b.app_version.trim();
    if version.is_empty()
        || version.len() > 40
        || !version.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
    {
        return Err(invalid("app_version: the Ferrocast version, like 0.1.0-beta.9."));
    }
    if !matches!(b.reason.as_str(), "automatic" | "manual") {
        return Err(invalid("reason: automatic or manual."));
    }
    let now = now_ms();
    // One upload at a time per subscription. Another PC's that is still moving waits;
    // this PC's own (it restarted) or a stalled one is replaced.
    for old in st.db.backups(&ctx.sub_id, UPLOADING).map_err(ApiError::db)? {
        if old.install_id != ctx.install && now - old.updated_at < BUSY_MS {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "upload_in_progress",
                "Another PC on your Pro is uploading a backup right now.",
            )
            .hint("Try again in a few minutes."));
        }
        forget(&st, &old).await;
    }
    let kept = st.db.backups(&ctx.sub_id, COMPLETE).map_err(ApiError::db)?;
    if to_prune(&kept, b.bytes).is_none() {
        return Err(quota_full());
    }
    st.db
        .try_take(&ctx.sub_id, UPLOADS.name, UPLOADS.period, 1, UPLOADS.limit)
        .map_err(|e| e.into_api("backup_upload_quota", format!("That's {} cloud backups today.", UPLOADS.limit)))?;
    let row = Row {
        id: new_id(),
        sub_id: ctx.sub_id,
        install_id: ctx.install,
        bytes: b.bytes,
        sha256: sha,
        app_version: version.to_string(),
        reason: b.reason,
        part_size: PART_SIZE,
        state: UPLOADING.into(),
        created_at: now,
        updated_at: now,
    };
    st.db.add_backup(&row).map_err(ApiError::db)?;
    Ok((StatusCode::CREATED, axum::Json(json!({ "id": row.id, "part_size": PART_SIZE, "parts": row.parts() })))
        .into_response())
}

fn too_big(part_size: i64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("Each part can be up to {} ({} bytes).", human_bytes(part_size as u64), part_size),
    )
    .hint("Split the backup into parts of part_size bytes.")
}

/// `PUT /v1/pro/backups/{id}/parts/{n}`: one part, `application/octet-stream`, numbered
/// from 0. Sending a part again replaces it.
async fn put_part(
    State(st): State<AppState>,
    Path((id, n)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    // Every refusal before the body is read drains it first, so the sender gets the answer
    // instead of a connection reset partway through sending.
    let checked = async {
        let ctx = gate_write(&st, &headers).await?;
        if content_type(&headers) != "application/octet-stream" {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_type",
                "Backup parts are sent as application/octet-stream.",
            ));
        }
        if !id_ok(&id) {
            return Err(not_found());
        }
        let b = st.db.backup(&ctx.sub_id, &id).map_err(ApiError::db)?.ok_or_else(not_found)?;
        if b.state != UPLOADING {
            return Err(ApiError::new(StatusCode::CONFLICT, "already_complete", "That backup is already complete."));
        }
        let n: i64 = n.parse().ok().filter(|n| (0..b.parts()).contains(n)).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_part",
                format!("Parts are numbered 0 to {}.", b.parts() - 1),
            )
        })?;
        let declared =
            headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        if declared.is_some_and(|d| d > b.part_size) {
            return Err(too_big(b.part_size));
        }
        Ok((b, n))
    }
    .await;
    let (b, n) = match checked {
        Ok(v) => v,
        Err(e) => {
            crate::blob::drain(body).await;
            return Err(e);
        }
    };
    let blobs = st.blobs()?;
    let key = part_key(&b.sub_id, &b.id, n);
    // Tells a body that broke off (the PC lost its connection) from a disk problem here.
    let broke = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = broke.clone();
    let stream = body
        .into_data_stream()
        .map_err(move |e| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            io::Error::other(e)
        })
        .boxed();
    let got = match blobs.put_stream(&key, stream, b.part_size as u64).await {
        Ok(got) => i64::try_from(got).unwrap_or(i64::MAX),
        Err(BlobError::TooLarge(_)) => return Err(too_big(b.part_size)),
        Err(BlobError::Io(_)) if broke.load(std::sync::atomic::Ordering::Relaxed) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_part", "The part arrived cut off.")
                .hint("Send it again."));
        }
        Err(e) => return Err(e.into_api()),
    };
    let want = b.part_bytes(n);
    if got != want {
        let _ = blobs.delete(&key).await;
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_part",
            format!("Part {n} should be {want} bytes; {got} arrived."),
        ));
    }
    st.db.add_backup_part(&b.id, n, got, now_ms()).map_err(ApiError::db)?;
    Ok(axum::Json(json!({ "part": n, "bytes": got })).into_response())
}

/// `POST /v1/pro/backups/{id}/complete`: joins the parts and checks the size and hash.
/// 409 `parts_missing` keeps the upload (send the rest); 409 `hash_mismatch` deletes it.
async fn complete(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ctx = gate_write(&st, &headers).await?;
    if !id_ok(&id) {
        return Err(not_found());
    }
    let b = st.db.backup(&ctx.sub_id, &id).map_err(ApiError::db)?.ok_or_else(not_found)?;
    if b.state == COMPLETE {
        // A retry after a lost answer.
        return Ok(axum::Json(json!({ "backup": item(&b, &ctx.install), "pruned": [] })).into_response());
    }
    let parts = st.db.backup_parts(&b.id).map_err(ApiError::db)?;
    if let Some(missing) = (0..b.parts()).find(|n| !parts.iter().any(|(p, bytes)| p == n && *bytes == b.part_bytes(*n)))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "parts_missing",
            format!("Part {missing} of {} hasn't arrived yet.", b.parts()),
        )
        .hint("Send the missing parts, then complete again."));
    }
    let blobs = st.blobs()?;
    let hasher = Arc::new(Mutex::new(Sha256::new()));
    let h = hasher.clone();
    let keys: Vec<String> = (0..b.parts()).map(|n| part_key(&b.sub_id, &b.id, n)).collect();
    let joined = futures_util::stream::iter(keys)
        .then(move |k| async move { blobs.get(&k).await.map_err(|e| io::Error::other(e.to_string())) })
        .try_flatten()
        .inspect_ok(move |chunk| h.lock().unwrap_or_else(|p| p.into_inner()).update(chunk))
        .boxed();
    let key = blob_key(&b.sub_id, &b.id);
    let size = blobs.put_stream(&key, joined, b.bytes as u64).await;
    let sha = hex(&hasher.lock().unwrap_or_else(|p| p.into_inner()).clone().finalize());
    let size = match size {
        Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Err(BlobError::TooLarge(_)) => -1,
        Err(e) => return Err(e.into_api()),
    };
    if size != b.bytes || sha != b.sha256 {
        forget(&st, &b).await;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "hash_mismatch",
            "The uploaded backup doesn't match its size or SHA-256, so it was deleted.",
        )
        .hint("Ferrocast uploads it again from the start."));
    }
    // Make room: the oldest automatic backups go first.
    let kept = st.db.backups(&ctx.sub_id, COMPLETE).map_err(ApiError::db)?;
    let gone = to_prune(&kept, b.bytes).unwrap_or_default();
    for old in kept.iter().filter(|k| gone.contains(&k.id)) {
        forget(&st, old).await;
    }
    let now = now_ms();
    st.db.complete_backup(&b.id, now).map_err(ApiError::db)?;
    for n in 0..b.parts() {
        let _ = blobs.delete(&part_key(&b.sub_id, &b.id, n)).await;
    }
    let done = Row { state: COMPLETE.into(), created_at: now, updated_at: now, ..b };
    Ok((StatusCode::CREATED, axum::Json(json!({ "backup": item(&done, &ctx.install), "pruned": gone })))
        .into_response())
}

/// One backup as the list shows it.
fn item(b: &Row, install: &str) -> Value {
    json!({
        "id": b.id,
        "bytes": b.bytes,
        "sha256": b.sha256,
        "created_at": b.created_at,
        "app_version": b.app_version,
        "from_install": b.install_id,
        "this_install": b.install_id == install,
        "reason": b.reason,
    })
}

/// `GET /v1/pro/backups` → the complete backups, newest first.
async fn list(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let sub = gate_read(&st, &headers).await?;
    let install = authed(&st, &headers)?;
    let mut rows = st.db.backups(&sub, COMPLETE).map_err(ApiError::db)?;
    rows.reverse();
    Ok(axum::Json(rows.iter().map(|b| item(b, &install)).collect::<Vec<_>>()).into_response())
}

/// `GET /v1/pro/backups/{id}` → the ciphertext, streamed.
async fn download(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let sub = gate_read(&st, &headers).await?;
    if !id_ok(&id) {
        return Err(not_found());
    }
    let b = st.db.backup(&sub, &id).map_err(ApiError::db)?.filter(|b| b.state == COMPLETE).ok_or_else(not_found)?;
    let stream = match st.blobs()?.get(&blob_key(&b.sub_id, &b.id)).await {
        Ok(s) => s,
        Err(BlobError::NotFound) => return Err(not_found()),
        Err(e) => return Err(e.into_api()),
    };
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&b.bytes.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"ferrocast-{}.ferrobackup\"", b.id)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&b.sha256) {
        h.insert("x-content-sha256", v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    Ok(res)
}

/// `DELETE /v1/pro/backups/{id}` → 204 (a finished backup or an upload in progress).
async fn remove(State(st): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Response, ApiError> {
    let sub = gate_read(&st, &headers).await?;
    if !id_ok(&id) {
        return Err(not_found());
    }
    let b = st.db.backup(&sub, &id).map_err(ApiError::db)?.ok_or_else(not_found)?;
    forget(&st, &b).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── Housekeeping ─────────────────────────────────────────────────────────────────────

/// Whether a subscription's backups are still kept at `now`: while Pro lasts and for
/// [`KEEP_AFTER_LAPSE_MS`] after it stopped (period over, refunded, or gone).
fn kept(st: &AppState, sub: &str, now: i64) -> rusqlite::Result<bool> {
    Ok(match st.db.pro_sub(sub)? {
        None => false,
        Some(s) => {
            let access = s.access();
            crate::billing::access::is_pro(&access, now) || {
                let stopped = s.stopped_at();
                now < stopped.saturating_add(KEEP_AFTER_LAPSE_MS)
            }
        }
    })
}

/// The hourly sweep (spawned in `main.rs`): uploads that stopped a day ago, and every
/// backup of a subscription whose Pro ended over 30 days ago.
pub async fn sweep(st: &AppState) {
    let now = now_ms();
    match st.db.stale_backup_uploads(now - STALE_UPLOAD_MS) {
        Ok(stale) => {
            for b in stale {
                forget(st, &b).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list unfinished cloud backups"),
    }
    let subs = match st.db.backup_subs() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list cloud backups");
            return;
        }
    };
    for sub in subs {
        match kept(st, &sub, now) {
            Ok(true) => {}
            Ok(false) => {
                let rows = st.db.all_backups(&sub).unwrap_or_default();
                let n = rows.len();
                for b in rows {
                    forget(st, &b).await;
                }
                tracing::info!(backups = n, "cloud backups removed 30 days after Pro ended");
            }
            Err(e) => tracing::warn!(error = %e, "couldn't check a subscription's cloud backups"),
        }
    }
}
