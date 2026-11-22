//! Clips and highlights (API.md "Clips"): the recordings folder as a library to browse, trim,
//! rename and delete (to the Trash), put several together, caption and send to YouTube. The work
//! runs as background jobs with progress. PC only: a paired phone (the relay's tunnel) never
//! reaches these (`remote_scope::OWNER_DENIED`). Every failure has our error shape.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ferrocast_engine::clips::edit::{CompileReq, Cue, TrimReq};
use ferrocast_engine::clips::upload::UploadReq;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::{Body, OptionalBody};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/clips", get(list))
        .route("/clips/compile", post(compile))
        .route("/clips/jobs", get(jobs))
        .route("/clips/jobs/{id}", get(job).delete(dismiss_job))
        .route("/clips/jobs/{id}/cancel", post(cancel_job))
        .route("/clips/{id}", get(probe).put(rename).delete(remove))
        .route("/clips/{id}/thumbnail", get(thumbnail))
        .route("/clips/{id}/file", get(file))
        .route("/clips/{id}/trim", post(trim))
        .route("/clips/{id}/captions", get(captions).put(save_captions).post(make_captions))
        .route("/clips/{id}/captions.srt", get(captions_srt))
        .route("/clips/{id}/captions/burn", post(burn))
        .route("/clips/{id}/upload", post(upload))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    q: Option<String>,
}

/// `GET /clips?q=` → `{ folder, ffmpeg_ready, youtube_connected, clips: [...] }`, newest first.
async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.clips_overview(q.q.as_deref()).await?)))
}

/// `GET /clips/{id}` → `{ duration_ms, width, height, has_audio, has_video }` (needs FFmpeg).
async fn probe(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.clip_probe(&id).await?)))
}

/// `GET /clips/{id}/thumbnail` → `{ image }` (a JPEG data URL; needs FFmpeg).
async fn thumbnail(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "image": app.engine.clip_thumbnail_image(&id).await? })))
}

/// `GET /clips/{id}/file` (the session may ride as `?session=` so a `<video>` can load it): the
/// clip itself, with `Range` so it seeks. The name is checked like every other call's.
async fn file(State(app): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let (path, mime) = match app.engine.clip_file(&id).await {
        Ok(x) => x,
        Err(e) => return crate::error::ApiError::from(e).into_response(),
    };
    match tokio::fs::File::open(&path).await {
        Ok(f) => crate::assets::range_response(f, mime, &headers).await,
        Err(_) => crate::error::ApiError::from(ferrocast_engine::clips::no_such_clip()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

/// `PUT /clips/{id}` `{ name }` → the clip under its new name (its captions go with it).
async fn rename(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<RenameBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.rename_clip(&id, &b.name).await?)))
}

/// `DELETE /clips/{id}`: to the Trash (with its captions), never gone for good.
async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_clip(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn accepted(job: impl serde::Serialize) -> Response {
    (StatusCode::ACCEPTED, Json(json!(job))).into_response()
}

/// `POST /clips/{id}/trim` `{ start_ms, end_ms, name?, precise? }` → 202 and the job.
async fn trim(State(app): State<AppState>, Path(id): Path<String>, Body(b): Body<TrimReq>) -> ApiResult<Response> {
    Ok(accepted(app.engine.trim_clip(&id, b).await?))
}

/// `POST /clips/compile` `{ clips: [id…], crossfade_s?, normalize?, name? }` → 202 and the job.
async fn compile(State(app): State<AppState>, Body(b): Body<CompileReq>) -> ApiResult<Response> {
    Ok(accepted(app.engine.compile_clips(b).await?))
}

/// `GET /clips/{id}/captions` → `{ cues: [{ start_ms, end_ms, text }] }` (none: empty).
async fn captions(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "cues": app.engine.clip_captions(&id).await? })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CuesBody {
    cues: Vec<Cue>,
}

/// `PUT /clips/{id}/captions` `{ cues }` → `{ cues }`: what the streamer edited, saved beside the clip.
async fn save_captions(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<CuesBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "cues": app.engine.set_clip_captions(&id, b.cues).await? })))
}

/// `POST /clips/{id}/captions`: Whisper writes them → 202 and the job.
async fn make_captions(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(accepted(app.engine.make_clip_captions(&id).await?))
}

/// `GET /clips/{id}/captions.srt`: the captions as a .srt download.
async fn captions_srt(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let text = app.engine.clip_captions_srt(&id).await?;
    let stem = id.rsplit_once('.').map_or(id.as_str(), |(s, _)| s);
    // The file name is the clip's (a plain name, checked), with anything a header can't hold cut out.
    let safe: String = stem.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_')).collect();
    let name = if safe.trim().is_empty() { "captions".to_string() } else { safe };
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-subrip; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.srt\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        text,
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BurnBody {
    #[serde(default)]
    name: Option<String>,
}

/// `POST /clips/{id}/captions/burn` `{ name? }` → 202 and the job (a new copy with them in the picture).
async fn burn(
    State(app): State<AppState>,
    Path(id): Path<String>,
    OptionalBody(b): OptionalBody<BurnBody>,
) -> ApiResult<Response> {
    Ok(accepted(app.engine.burn_clip_captions(&id, b.name.as_deref()).await?))
}

/// `POST /clips/{id}/upload` `{ title, description?, tags?, privacy?, category_id?, made_for_kids?,
/// shorts? }` → 202 and the job. Privacy defaults to private.
async fn upload(State(app): State<AppState>, Path(id): Path<String>, Body(b): Body<UploadReq>) -> ApiResult<Response> {
    Ok(accepted(app.engine.upload_clip(&id, b).await?))
}

/// `GET /clips/jobs` → `{ jobs: [...] }`, newest first.
async fn jobs(State(app): State<AppState>) -> Json<Value> {
    Json(json!({ "jobs": app.engine.clip_jobs() }))
}

/// `GET /clips/jobs/{id}` → the job.
async fn job(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.clip_job(&id)?)))
}

/// `POST /clips/jobs/{id}/cancel` → the job (waiting or running: stopped, nothing half-made stays).
async fn cancel_job(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.cancel_clip_job(&id)?)))
}

/// `DELETE /clips/jobs/{id}`: takes a finished job off the list (409 while it runs).
async fn dismiss_job(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.dismiss_clip_job(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests;
