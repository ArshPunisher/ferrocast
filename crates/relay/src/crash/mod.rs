//! Opt-in crash and error reports (3.8.1): `POST /v1/crash-reports` from the app, and the
//! owner's read side for the Control Center (`/admin/v1/crash-reports…`, contract in
//! docs/ADMIN-API.md § Crash reports).
//!
//! What crosses the trust boundary here is a public-internet upload from any install (free
//! to mint), so every limit is enforced before anything is kept:
//!
//! - **Auth**: the install's `Authorization: Bearer <id>.<secret>`, like the other app routes.
//! - **Size**: a 1 MB body, read as a stream and cut off at the cap (413 in the relay's error
//!   shape); the bundle inside is one base64 field of a JSON envelope.
//! - **Rate**: 5 an hour and 20 a day per install, 30 an hour per address (counted per
//!   attempt, never stored), and a relay-wide daily cap (429 with `Retry-After`).
//! - **Fields**: length caps and strict character sets; nothing the client sends is trusted,
//!   used as a path or a key, or logged as it came.
//! - **The bundle** is an opaque gzip blob in the [`BlobStore`] under a key the relay makes
//!   (`crash-reports/cr_<random>`). It is never unpacked on upload (only its first bytes are
//!   checked for the gzip magic). Only the admin text view decompresses it, streaming, and
//!   stops at [`TEXT_CAP`] bytes, so a decompression bomb costs a few MB of memory at most.
//! - **Storage**: the newest [`KEEP_BUNDLES`] bundles per signature; a total cap with
//!   oldest-first eviction; bundles go after 90 days and the metadata after a year (`sweep`).
//! - **Privacy**: the install id is stored only as an HMAC with a key kept in the database
//!   (`install_hash`), never as the id; client addresses are never stored.

pub(crate) mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine as _;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::{Value, json};
use store::{ListFilter, NewReport};

pub(crate) use store::SCHEMA;

use crate::admin_pro::{self, Peer};
use crate::blob::BlobStore;
use crate::{ApiError, AppState, authed, client_ip, now_ms};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// The largest request body, and so the largest bundle (it travels as base64 inside).
pub const MAX_BODY: usize = 1024 * 1024;
/// The admin text view stops reading a bundle after this many bytes of text.
pub const TEXT_CAP: usize = 2 * 1024 * 1024;
/// Bundles kept per signature (newest first).
pub const KEEP_BUNDLES: i64 = 3;
/// Kinds of report.
pub const KINDS: [&str; 4] = ["crash", "error", "unclean_exit", "hang"];

/// The caps, all in one place so the tests can use small ones.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_body: usize,
    pub per_install_hour: i64,
    pub per_install_day: i64,
    pub per_ip_hour: usize,
    /// Reports the whole relay takes in a day.
    pub global_day: i64,
    /// Bytes of bundles kept in all; the oldest go first.
    pub storage_cap: i64,
    pub keep_bundles: i64,
    /// Report rows kept per signature (counts keep growing past it).
    pub max_rows_per_signature: i64,
    /// Signatures kept; past it the one seen longest ago goes.
    pub max_signatures: i64,
    pub bundle_days: i64,
    pub metadata_days: i64,
    pub text_cap: usize,
    /// The longest a body may take to arrive.
    pub body_timeout: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_body: MAX_BODY,
            per_install_hour: 5,
            per_install_day: 20,
            per_ip_hour: 30,
            global_day: 5_000,
            storage_cap: 2_000_000_000,
            keep_bundles: KEEP_BUNDLES,
            max_rows_per_signature: 200,
            max_signatures: 20_000,
            bundle_days: 90,
            metadata_days: 365,
            text_cap: TEXT_CAP,
            body_timeout: std::time::Duration::from_secs(30),
        }
    }
}

/// The limits, the per-address attempt counters (memory only) and the lock that makes the
/// storage checks and the write one step.
pub(crate) struct Runtime {
    limits: Limits,
    ips: Mutex<HashMap<IpAddr, VecDeque<i64>>>,
    key: OnceLock<Vec<u8>>,
    ingest: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Runtime").field("limits", &self.limits).finish_non_exhaustive()
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(Limits::default())
    }
}

impl Runtime {
    pub(crate) fn new(limits: Limits) -> Runtime {
        Runtime { limits, ips: Mutex::default(), key: OnceLock::new(), ingest: tokio::sync::Mutex::new(()) }
    }

    /// Counts an attempt from `ip`; `Err(seconds to wait)` when the hour's allowance is used up.
    fn take_ip(&self, ip: IpAddr, now: i64) -> Result<(), i64> {
        let mut map = self.ips.lock().unwrap_or_else(|p| p.into_inner());
        // The map can't grow without bound: old entries go whenever it is big.
        if map.len() > 10_000 {
            map.retain(|_, v| v.back().is_some_and(|t| now - *t < HOUR_MS));
        }
        let q = map.entry(ip).or_default();
        while q.front().is_some_and(|t| now - *t >= HOUR_MS) {
            q.pop_front();
        }
        if q.len() >= self.limits.per_ip_hour {
            let first = q.front().copied().unwrap_or(now);
            return Err(((first + HOUR_MS - now) / 1000).max(1));
        }
        q.push_back(now);
        Ok(())
    }

    /// `install_hash`: the hex HMAC-SHA256 of the install id under the relay's own key,
    /// domain-separated. Stable across restarts; never the id.
    fn install_hash(&self, st: &AppState, install: &str) -> Result<String, ApiError> {
        let key = match self.key.get() {
            Some(k) => k,
            None => {
                let k = st.db.crash_key().map_err(ApiError::db)?;
                self.key.get_or_init(|| k)
            }
        };
        #[allow(clippy::expect_used)] // HMAC takes a key of any length
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key");
        mac.update(b"ferrocast-crash-install\0");
        mac.update(install.as_bytes());
        Ok(mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect())
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/crash-reports", post(upload))
        .route("/admin/v1/crash-reports", get(list))
        .route("/admin/v1/crash-reports/reports/{id}/text", get(text))
        .route("/admin/v1/crash-reports/reports/{id}/bundle", get(bundle))
        .route("/admin/v1/crash-reports/{signature}", get(detail))
        .route("/admin/v1/crash-reports/{signature}", delete(remove))
}

// ── Checks on what the app sends ─────────────────────────────────────────────────────

fn invalid(field: &str, why: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_field", format!("{field} {why}."))
        .hint("Crash reports are sent by Ferrocast itself; update the app if this keeps happening.")
}

fn too_large(max: usize) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("A crash report can be at most {} KB.", max / 1024),
    )
    .hint("Ferrocast keeps the report on this PC.")
}

fn rate_limited(wait: i64, what: &str) -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("Too many crash reports {what}."))
        .hint(format!("Try again in {wait} seconds."))
        .header(header::RETRY_AFTER, wait.to_string())
}

fn is_token(s: &str, extra: &[char], min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

/// A signature is lowercase hex (it is also a URL segment).
pub(crate) fn signature_ok(s: &str) -> bool {
    (8..=128).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn report_id_ok(s: &str) -> bool {
    s.strip_prefix("cr_").is_some_and(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    // No OS randomness means nothing secure is possible; refuse loudly.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
    format!("cr_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

fn blob_key(id: &str) -> String {
    format!("crash-reports/{id}")
}

/// A checked upload.
struct Upload {
    version: String,
    os: String,
    arch: String,
    kind: String,
    signature: String,
    cause_codes: Vec<String>,
    message: String,
    bundle: Vec<u8>,
}

fn string_field<'a>(v: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    v.get(name).and_then(Value::as_str).ok_or_else(|| invalid(name, "must be text"))
}

fn check(body: &[u8], max_bundle: usize) -> Result<Upload, ApiError> {
    let v: Value = serde_json::from_slice(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The report isn't a JSON object."))?;
    let version = string_field(&v, "version")?;
    if !is_token(version, &['.', '-', '_', '+'], 1, 32) {
        return Err(invalid("version", "must be 1 to 32 letters, digits or . - _ +"));
    }
    let os = string_field(&v, "os")?;
    if os.is_empty() || os.chars().count() > 64 || os.chars().any(char::is_control) {
        return Err(invalid("os", "must be 1 to 64 characters, no control characters"));
    }
    let arch = string_field(&v, "arch")?;
    if !is_token(arch, &['_', '-'], 1, 16) {
        return Err(invalid("arch", "must be 1 to 16 letters, digits, - or _"));
    }
    let kind = string_field(&v, "kind")?;
    if !KINDS.contains(&kind) {
        return Err(invalid("kind", "must be crash, error, unclean_exit or hang"));
    }
    let signature = string_field(&v, "signature")?;
    if !signature_ok(signature) {
        return Err(invalid("signature", "must be 8 to 128 lowercase hex characters"));
    }
    let mut cause_codes = Vec::new();
    match v.get("cause_codes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            if a.len() > 8 {
                return Err(invalid("cause_codes", "can have at most 8 entries"));
            }
            for c in a {
                let c = c.as_str().ok_or_else(|| invalid("cause_codes", "must be a list of text"))?;
                if !is_token(c, &['_', '-', '.', ':'], 1, 64) {
                    return Err(invalid("cause_codes", "entries must be 1 to 64 letters, digits or . : - _"));
                }
                cause_codes.push(c.to_string());
            }
        }
        Some(_) => return Err(invalid("cause_codes", "must be a list")),
    }
    let message = string_field(&v, "message")?;
    if message.len() > 2048 {
        return Err(invalid("message", "is longer than 2 KB"));
    }
    // Shown in the owner's lists: control characters (escape sequences, NUL) become spaces.
    let message: String =
        message.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { ' ' } else { c }).collect();
    let b64 = string_field(&v, "bundle")?;
    // The decoded size is at most three quarters of the text; refuse before decoding.
    if b64.len() / 4 * 3 > max_bundle + 3 {
        return Err(too_large(max_bundle));
    }
    let bundle =
        base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| invalid("bundle", "must be base64 text"))?;
    // Only the first bytes: it is never unpacked here.
    if bundle.len() < 18 || bundle[..3] != [0x1f, 0x8b, 0x08] {
        return Err(invalid("bundle", "must be gzip data"));
    }
    Ok(Upload {
        version: version.into(),
        os: os.into(),
        arch: arch.into(),
        kind: kind.into(),
        signature: signature.into(),
        cause_codes,
        message,
        bundle,
    })
}

/// The first line of the message, as the signature's short title.
fn title_of(message: &str) -> String {
    let line = message.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let t: String = line.chars().take(120).collect();
    if t.is_empty() { "(no message)".into() } else { t }
}

// ── Upload ───────────────────────────────────────────────────────────────────────────

/// What the checks before the body is read decide.
struct Pass {
    install_hash: String,
}

/// The per-install and relay-wide allowances (429 with `Retry-After` when one is used up).
fn check_rates(st: &AppState, install_hash: &str, now: i64) -> Result<(), ApiError> {
    let l = &st.crash.limits;
    let (n, oldest) = st.db.crash_rate(Some(install_hash), now - HOUR_MS).map_err(ApiError::db)?;
    if n >= l.per_install_hour {
        return Err(rate_limited(((oldest.unwrap_or(now) + HOUR_MS - now) / 1000).max(1), "from this install"));
    }
    let (n, oldest) = st.db.crash_rate(Some(install_hash), now - DAY_MS).map_err(ApiError::db)?;
    if n >= l.per_install_day {
        return Err(rate_limited(((oldest.unwrap_or(now) + DAY_MS - now) / 1000).max(1), "from this install today"));
    }
    let (n, oldest) = st.db.crash_rate(None, now - DAY_MS).map_err(ApiError::db)?;
    if n >= l.global_day {
        return Err(rate_limited(((oldest.unwrap_or(now) + DAY_MS - now) / 1000).max(1), "today"));
    }
    Ok(())
}

fn before_body(st: &AppState, peer: std::net::SocketAddr, headers: &HeaderMap, now: i64) -> Result<Pass, ApiError> {
    let install = authed(st, headers)?;
    // No file store, no crash reports (503), after the caller is known.
    st.blobs()?;
    let rt = &st.crash;
    if let Err(wait) = rt.take_ip(client_ip(peer, headers), now) {
        return Err(rate_limited(wait, "from this network"));
    }
    let install_hash = rt.install_hash(st, &install)?;
    check_rates(st, &install_hash, now)?;
    let l = &rt.limits;
    if let Some(declared) =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok())
        && declared > l.max_body
    {
        return Err(too_large(l.max_body));
    }
    Ok(Pass { install_hash })
}

async fn upload(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let now = now_ms();
    // A refusal before the body is read drains it first, so the sender gets the answer
    // instead of a connection reset partway through sending.
    let pass = match before_body(&st, peer, &headers, now) {
        Ok(p) => p,
        Err(e) => {
            crate::blob::drain(body).await;
            return Err(e);
        }
    };
    let max = st.crash.limits.max_body;
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    // A sender that trickles bytes can't hold the connection for ever.
    let read = tokio::time::timeout(st.crash.limits.body_timeout, async {
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_body", "The report didn't arrive whole."));
            };
            if buf.len() + chunk.len() > max {
                return Err(too_large(max));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(())
    })
    .await;
    match read {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                crate::blob::drain_stream(stream).await;
            }
            return Err(e);
        }
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "The report took too long to arrive.",
            ));
        }
    }
    let up = check(&buf, max)?;
    drop(buf);
    let (id, signature) = store_report(&st, &pass, up, now).await?;
    let mut res = (StatusCode::CREATED, axum::Json(json!({ "id": id, "signature": signature, "received_ms": now })))
        .into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

async fn store_report(st: &AppState, pass: &Pass, up: Upload, now: i64) -> Result<(String, String), ApiError> {
    let blobs = st.blobs()?;
    let l = &st.crash.limits;
    // One upload at a time past this point, so the storage cap and the counts stay true.
    let _one = st.crash.ingest.lock().await;
    // Two uploads at once can both pass the check before the body: count again, alone.
    check_rates(st, &pass.install_hash, now_ms())?;
    let size = i64::try_from(up.bundle.len()).unwrap_or(i64::MAX);
    make_room(st, blobs, size).await?;
    let id = new_id();
    let key = blob_key(&id);
    blobs.put(&key, Bytes::from(up.bundle)).await.map_err(crate::blob::BlobError::into_api)?;
    let title = title_of(&up.message);
    let added = st.db.crash_add(&NewReport {
        id: &id,
        signature: &up.signature,
        kind: &up.kind,
        title: &title,
        version: &up.version,
        os: &up.os,
        arch: &up.arch,
        cause_codes: &up.cause_codes,
        message: &up.message,
        install_hash: &pass.install_hash,
        size_bytes: size,
        blob_key: &key,
        now,
    });
    if let Err(e) = added {
        // The row failed: the file must not stay behind without one.
        let _ = blobs.delete(&key).await;
        return Err(ApiError::db(e));
    }
    // Dedupe: only the newest few bundles of a signature stay; the rows are capped too.
    match st.db.crash_extra_bundles(&up.signature, l.keep_bundles) {
        Ok(extra) => {
            for (rid, rkey) in extra {
                remove_bundle(st, blobs, &rid, &rkey).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list extra crash bundles"),
    }
    if let Err(e) = st.db.crash_trim_rows(&up.signature, l.max_rows_per_signature) {
        tracing::warn!(error = %e, "couldn't trim crash report rows");
    }
    // Signatures can't be minted without bound: the one seen longest ago makes room.
    for _ in 0..10 {
        if st.db.crash_signature_count().unwrap_or(0) <= l.max_signatures {
            break;
        }
        match st.db.crash_stalest_signature(&up.signature) {
            Ok(Some(old)) => delete_signature(st, blobs, &old).await,
            _ => break,
        }
    }
    Ok((id, up.signature))
}

/// Removes the oldest bundles until `incoming` more bytes fit under the storage cap.
async fn make_room(st: &AppState, blobs: &dyn BlobStore, incoming: i64) -> Result<(), ApiError> {
    let cap = st.crash.limits.storage_cap;
    loop {
        let used = st.db.crash_bytes().map_err(ApiError::db)?;
        if used + incoming <= cap {
            return Ok(());
        }
        let oldest = st.db.crash_oldest_bundles(20).map_err(ApiError::db)?;
        if oldest.is_empty() {
            return Ok(());
        }
        let mut gone = 0;
        for (id, key) in oldest {
            if remove_bundle(st, blobs, &id, &key).await {
                gone += 1;
            }
        }
        // The disk or the database refuses: store anyway rather than loop for ever.
        if gone == 0 {
            tracing::error!("crash bundles can't be removed to make room");
            return Ok(());
        }
    }
}

/// Deletes one bundle's file, then forgets its key (the row, with its metadata, stays).
/// `true` when it is gone (and its key forgotten).
async fn remove_bundle(st: &AppState, blobs: &dyn BlobStore, id: &str, key: &str) -> bool {
    match blobs.delete(key).await {
        Ok(_) => match st.db.crash_clear_bundle(id) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't clear a crash bundle's key");
                false
            }
        },
        // Kept, so the next sweep tries again.
        Err(e) => {
            tracing::warn!(error = %e, "couldn't delete a crash bundle");
            false
        }
    }
}

/// A signature with its bundles and rows.
async fn delete_signature(st: &AppState, blobs: &dyn BlobStore, signature: &str) {
    match st.db.crash_signature_bundles(signature) {
        Ok(keys) => {
            for key in keys {
                if let Err(e) = blobs.delete(&key).await {
                    tracing::warn!(error = %e, "couldn't delete a crash bundle");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list a signature's crash bundles"),
    }
    if let Err(e) = st.db.crash_delete_signature(signature) {
        tracing::warn!(error = %e, "couldn't delete a crash signature");
    }
}

/// Retention, hourly: bundles after `bundle_days`, everything after `metadata_days`.
pub async fn sweep(st: &AppState) {
    sweep_at(st, now_ms()).await;
}

pub(crate) async fn sweep_at(st: &AppState, now: i64) {
    let l = &st.crash.limits;
    if let Ok(blobs) = st.blobs() {
        match st.db.crash_bundles_before(now - l.bundle_days * DAY_MS) {
            Ok(old) => {
                for (id, key) in old {
                    remove_bundle(st, blobs, &id, &key).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't list old crash bundles"),
        }
        match st.db.crash_signatures_before(now - l.metadata_days * DAY_MS) {
            Ok(old) => {
                for sig in old {
                    delete_signature(st, blobs, &sig).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't list old crash signatures"),
        }
    }
    if let Err(e) = st.db.crash_prune_metadata(now - l.metadata_days * DAY_MS) {
        tracing::warn!(error = %e, "couldn't prune old crash reports");
    }
}

// ── The owner's read side ────────────────────────────────────────────────────────────

fn bad_query(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "bad_query", message)
}

fn not_found(what: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("No {what} with that id."))
}

fn sig_json(db: &crate::Db, s: &store::Signature) -> Result<Value, ApiError> {
    Ok(json!({
        "signature": s.signature,
        "kind": s.kind,
        "title": s.title,
        "count": s.count,
        "installs": s.installs,
        "first_seen_ms": s.first_seen,
        "last_seen_ms": s.last_seen,
    }))
    .and_then(|mut v| {
        let versions = db.crash_facet_values(&s.signature, "version").map_err(ApiError::db)?;
        let os = db.crash_facet_values(&s.signature, "os").map_err(ApiError::db)?;
        let latest = db.crash_latest_report_id(&s.signature).map_err(ApiError::db)?;
        v["versions"] = json!(versions);
        v["os"] = json!(os);
        v["latest_report_id"] = json!(latest);
        Ok(v)
    })
}

async fn list(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    admin_pro::reader(&st, peer, &headers)?;
    let mut f = ListFilter { limit: 50, ..ListFilter::default() };
    if let Some(k) = q.get("kind").filter(|k| !k.is_empty()) {
        if !KINDS.contains(&k.as_str()) {
            return Err(bad_query("kind must be crash, error, unclean_exit or hang."));
        }
        f.kind = Some(k.clone());
    }
    f.version = q.get("version").filter(|v| !v.is_empty()).cloned();
    f.q = q.get("q").filter(|v| !v.is_empty()).map(|v| v.chars().take(128).collect());
    if let Some(l) = q.get("limit").filter(|v| !v.is_empty()) {
        f.limit = l
            .parse::<i64>()
            .ok()
            .filter(|l| (1..=200).contains(l))
            .ok_or_else(|| bad_query("limit must be a whole number from 1 to 200."))?;
    }
    if let Some(c) = q.get("before").filter(|v| !v.is_empty()) {
        f.before = c
            .split_once(':')
            .and_then(|(ms, sig)| Some((ms.parse::<i64>().ok()?, sig.to_string())))
            .filter(|(_, sig)| signature_ok(sig));
        if f.before.is_none() {
            return Err(bad_query("before isn't a cursor this API gave."));
        }
    }
    let limit = f.limit as usize;
    let mut rows = st.db.crash_list(&f).map_err(ApiError::db)?;
    let next = (rows.len() > limit).then(|| {
        rows.truncate(limit);
        rows.last().map(|s| format!("{}:{}", s.last_seen, s.signature))
    });
    let items = rows.iter().map(|s| sig_json(&st.db, s)).collect::<Result<Vec<_>, _>>()?;
    let (reports, signatures, bytes) = st.db.crash_totals().map_err(ApiError::db)?;
    admin_pro::ok(json!({
        "items": items,
        "next": next.flatten(),
        "totals": { "reports": reports, "signatures": signatures, "bytes": bytes },
    }))
}

async fn detail(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(signature): Path<String>,
) -> Result<Response, ApiError> {
    admin_pro::reader(&st, peer, &headers)?;
    if !signature_ok(&signature) {
        return Err(not_found("crash signature"));
    }
    let s = st.db.crash_signature(&signature).map_err(ApiError::db)?.ok_or_else(|| not_found("crash signature"))?;
    let reports: Vec<Value> = st
        .db
        .crash_reports_of(&signature, 50)
        .map_err(ApiError::db)?
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "received_ms": r.received_ms,
                "version": r.version,
                "os": r.os,
                "arch": r.arch,
                "kind": r.kind,
                "cause_codes": r.cause_codes,
                "message": r.message,
                "size_bytes": r.size_bytes,
                "install_hash": r.install_hash,
            })
        })
        .collect();
    admin_pro::ok(json!({
        "signature": s.signature,
        "kind": s.kind,
        "title": s.title,
        "count": s.count,
        "installs": s.installs,
        "first_seen_ms": s.first_seen,
        "last_seen_ms": s.last_seen,
        "reports": reports,
    }))
}

async fn remove(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(signature): Path<String>,
) -> Result<Response, ApiError> {
    let now = admin_pro::reader(&st, peer, &headers)?;
    if !signature_ok(&signature) || st.db.crash_signature(&signature).map_err(ApiError::db)?.is_none() {
        return Err(not_found("crash signature"));
    }
    match st.blobs() {
        Ok(blobs) => delete_signature(&st, blobs, &signature).await,
        Err(_) => {
            st.db.crash_delete_signature(&signature).map_err(ApiError::db)?;
        }
    }
    // Who asked, if the Control Center says (not required: it isn't a Pro change).
    let actor = headers.get("x-admin-email").and_then(|v| v.to_str().ok()).map(|v| v.trim().to_ascii_lowercase());
    admin_pro::audit_log(
        &st,
        now,
        actor.as_deref().filter(|a| a.len() <= 320 && !a.chars().any(char::is_control)).unwrap_or("control-center"),
        "crash_reports.delete",
        &signature,
        json!({}),
    );
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// The report and its bundle's bytes (at most a body's worth), or the right 404.
async fn stored_bundle(st: &AppState, id: &str) -> Result<(store::Report, Vec<u8>), ApiError> {
    if !report_id_ok(id) {
        return Err(not_found("crash report"));
    }
    let r = st.db.crash_report(id).map_err(ApiError::db)?.ok_or_else(|| not_found("crash report"))?;
    let Some(key) = r.blob_key.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "bundle_removed",
            "This report's bundle was removed: only the newest 3 per problem are kept, for 90 days.",
        )
        .hint("The details of the report are still in the list."));
    };
    let blobs = st.blobs()?;
    let bytes = blobs
        .get_bytes(&key, st.crash.limits.max_body as u64 + 1024)
        .await
        .map_err(crate::blob::BlobError::into_api)?;
    Ok((r, bytes))
}

fn attachment(name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).unwrap_or(HeaderValue::from_static("attachment"))
}

/// Inflates the first gzip member, stopping at `cap` bytes (a bomb costs `cap`, no more).
/// `(text, cut)`: `cut` when it stopped at the cap or the data broke off.
pub(crate) fn gunzip_capped(data: &[u8], cap: usize) -> Result<(Vec<u8>, bool), std::io::Error> {
    use std::io::Read;
    let mut out = Vec::new();
    let res = flate2::read::GzDecoder::new(data).take(cap as u64 + 1).read_to_end(&mut out);
    // Nothing readable at all is an error; text that broke off partway is shown, cut.
    if let Err(e) = &res
        && out.is_empty()
    {
        return Err(std::io::Error::new(e.kind(), e.to_string()));
    }
    let cut = out.len() > cap || res.is_err();
    out.truncate(cap);
    Ok((out, cut))
}

async fn text(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    admin_pro::reader(&st, peer, &headers)?;
    let (_, bytes) = stored_bundle(&st, &id).await?;
    let cap = st.crash.limits.text_cap;
    let (mut out, cut) = tokio::task::spawn_blocking(move || gunzip_capped(&bytes, cap))
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The relay couldn't read that."))?
        .map_err(|_| {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bundle_unreadable", "This bundle isn't readable text.")
                .hint("Download the raw bundle instead.")
        })?;
    // The cut can land inside a character; the marker says the rest is not shown.
    let mut text = String::from_utf8_lossy(&out).into_owned();
    out.clear();
    if cut {
        text.push_str("\n[... the rest isn't shown: the text view stops at 2 MB ...]\n");
    }
    let mut res = (StatusCode::OK, text).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    if q.get("download").is_some_and(|v| v == "1" || v == "true") {
        h.insert(header::CONTENT_DISPOSITION, attachment(&format!("{id}.txt")));
    }
    Ok(res)
}

/// The bundle as it was stored (gzip), always as a download.
async fn bundle(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    admin_pro::reader(&st, peer, &headers)?;
    let (_, bytes) = stored_bundle(&st, &id).await?;
    let mut res = (StatusCode::OK, bytes).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_DISPOSITION, attachment(&format!("{id}.txt.gz")));
    Ok(res)
}
