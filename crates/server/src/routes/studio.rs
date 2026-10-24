//! Studio routes (API.md §4 "Studio"): counters, quotes, chat modules, stream info, the
//! go-live checklist, OBS helpers and moments. Every failure has our error shape and the
//! status that fits (400 bad field, 404 unknown id, 409 clash, 422 can't run now).

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use ferrocast_core::chatbot::CounterOp;
use ferrocast_core::event::Platform;
use ferrocast_core::stream_info::{Preset, StreamInfo};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // chatbot
        .route("/counters", get(counters).post(create_counter))
        .route("/counters/{name}", put(update_counter).delete(delete_counter))
        .route("/counters/{name}/change", post(change_counter))
        .route("/quotes", get(quotes).post(add_quote))
        .route("/quotes/{id}", put(edit_quote).delete(delete_quote))
        .route("/modules", get(modules).put(save_modules))
        // stream info
        .route("/stream-info", get(stream_info))
        .route("/stream-info/apply", post(apply_stream_info))
        .route("/stream-info/categories", get(categories))
        .route("/stream-info/presets", post(create_preset))
        .route("/stream-info/presets/{id}", put(update_preset).delete(delete_preset))
        // show
        .route("/show", get(show))
        .route("/show/config", get(show_config).put(save_show_config))
        .route("/show/preflight", get(preflight))
        .route("/show/preflight/fix", post(preflight_fix))
        .route("/show/start", post(start_show))
        .route("/show/countdown/skip", post(skip_countdown))
        .route("/show/countdown/cancel", post(cancel_countdown))
        .route("/show/live-scene", post(live_scene))
        .route("/show/end", post(end_show))
        // OBS helpers
        .route("/obs/scenes", get(obs_scenes))
        .route("/obs/inputs", get(obs_inputs))
        .route("/obs/scene", post(switch_scene))
        .route("/obs/source", post(source_visibility))
        .route("/obs/input/mute", post(mute_input))
        .route("/obs/replay/save", post(clip))
        .route("/obs/brb", post(brb))
        .route("/obs/stream", get(obs_stream).post(set_obs_stream))
        .route("/obs/health", get(health).post(check_health))
        .route("/obs/switcher", get(switcher).put(save_switcher))
        // moments
        .route("/moments/config", get(moments_config).put(save_moments_config))
        .route("/moments/lane", get(lane))
        .route("/moments/lane/dismiss-all", post(dismiss_all))
        .route("/moments/lane/{id}/dismiss", post(dismiss))
        .route("/moments/markers", get(markers).post(add_marker))
        .route("/moments/markers/export", get(export_markers))
        .route("/moments/markers/{id}", delete(delete_marker))
}

fn int_id(raw: &str, what: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| ApiError::not_found(what))
}

fn since(q: &HashMap<String, String>) -> Result<Option<i64>, ApiError> {
    match q.get("since") {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|v| (0..=ferrocast_engine::now().0).contains(v))
            .map(Some)
            .ok_or_else(|| ApiError::invalid("since: a time in Unix milliseconds, not in the future.")),
    }
}

// ------------------------------------------------------------------ counters

async fn counters(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.counters()))
}

async fn create_counter(State(app): State<AppState>, Body(body): Body<Map<String, Value>>) -> ApiResult<Response> {
    let (def, value) = ferrocast_engine::studio::chatbot::counter_from_body(&body)?;
    let c = app.engine.create_counter(def, value).await?;
    Ok((StatusCode::CREATED, Json(json!(c))).into_response())
}

async fn update_counter(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(body): Body<Map<String, Value>>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.update_counter(&name, &body).await?)))
}

async fn delete_counter(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_counter(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ChangeBody {
    op: String,
    #[serde(default)]
    amount: Option<i64>,
}

async fn change_counter(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<ChangeBody>,
) -> ApiResult<Json<Value>> {
    let max = ferrocast_core::chatbot::MAX_COUNTER_VALUE;
    let amount = |default: i64| -> Result<i64, ApiError> {
        let n = b.amount.unwrap_or(default);
        if n.abs() > max {
            return Err(ApiError::invalid("amount: up to ±1,000,000,000."));
        }
        Ok(n)
    };
    let op = match b.op.as_str() {
        "add" => CounterOp::Add(amount(1)?),
        "set" => CounterOp::Set(
            b.amount.ok_or_else(|| ApiError::invalid("amount: say what to set it to."))?.clamp(-max, max),
        ),
        "reset" => CounterOp::Reset,
        _ => return Err(ApiError::invalid("op: add, set or reset.")),
    };
    Ok(Json(json!(app.engine.change_counter(&name, op).await?)))
}

// ------------------------------------------------------------------ quotes

async fn quotes(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.quotes().await?)))
}

#[derive(Deserialize)]
struct QuoteBody {
    text: String,
}

async fn add_quote(State(app): State<AppState>, Body(b): Body<QuoteBody>) -> ApiResult<Response> {
    let q = app.engine.add_quote(&b.text, "Dashboard").await?;
    Ok((StatusCode::CREATED, Json(json!(q))).into_response())
}

async fn edit_quote(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<QuoteBody>,
) -> ApiResult<Json<Value>> {
    let id = int_id(&id, "quote")?;
    Ok(Json(json!(app.engine.edit_quote(id, &b.text).await?)))
}

async fn delete_quote(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_quote(int_id(&id, "quote")?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn modules(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.modules()))
}

async fn save_modules(State(app): State<AppState>, Body(b): Body<Map<String, Value>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.patch_modules(&b).await?)))
}

// ------------------------------------------------------------------ stream info

async fn stream_info(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.stream_info().await?)))
}

#[derive(Deserialize)]
struct ApplyBody {
    info: StreamInfo,
    #[serde(default)]
    platforms: Option<Vec<String>>,
}

/// 200 when every platform took it, 207 when some did, 422 when none did: each with the
/// per-platform results so the dashboard can say what to do for each.
async fn apply_stream_info(State(app): State<AppState>, Body(b): Body<ApplyBody>) -> ApiResult<Response> {
    let platforms = match b.platforms {
        None => None,
        Some(list) => Some(list.iter().map(|p| super::platform(p)).collect::<Result<Vec<Platform>, _>>()?),
    };
    let r = app.engine.apply_stream_info(b.info, platforms, None).await?;
    if r.ok == 0 {
        let body = json!({
            "error": { "code": "stream_info_failed", "message": "No platform took the change.",
                       "hint": "See what each platform said below." },
            "results": r.results, "ok": 0, "failed": r.failed,
        });
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response());
    }
    let status = if r.failed > 0 { StatusCode::MULTI_STATUS } else { StatusCode::OK };
    Ok((status, Json(json!(r))).into_response())
}

async fn categories(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    let p = super::platform(q.get("platform").map(String::as_str).unwrap_or(""))?;
    let query = q.get("q").cloned().unwrap_or_default();
    Ok(Json(json!({ "platform": p, "items": app.engine.search_categories(p, &query).await? })))
}

async fn create_preset(State(app): State<AppState>, Body(p): Body<Preset>) -> ApiResult<Response> {
    let saved = app.engine.save_preset(p, true).await?;
    Ok((StatusCode::CREATED, Json(json!(saved))).into_response())
}

async fn update_preset(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(mut p): Body<Preset>,
) -> ApiResult<Json<Value>> {
    p.id = id;
    Ok(Json(json!(app.engine.save_preset(p, false).await?)))
}

async fn delete_preset(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_preset(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ show

async fn show(State(app): State<AppState>) -> Json<Value> {
    Json(json!({ "show": app.engine.show_run() }))
}

async fn show_config(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.show_config()))
}

async fn save_show_config(State(app): State<AppState>, Body(b): Body<Map<String, Value>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.patch_show_config(&b).await?)))
}

/// `GET /music`: Settings › Music, what's playing and what can't be read.
pub async fn music(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.music_view()))
}

/// `PUT /music`: changes Settings › Music (only the fields sent).
pub async fn save_music(State(app): State<AppState>, Body(b): Body<Map<String, Value>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.patch_music_config(&b).await?)))
}

async fn preflight(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.preflight().await))
}

#[derive(Deserialize)]
struct FixBody {
    action: String,
}

async fn preflight_fix(State(app): State<AppState>, Body(b): Body<FixBody>) -> ApiResult<Json<Value>> {
    let action = b.action.strip_prefix("action:").unwrap_or(&b.action);
    Ok(Json(json!(app.engine.preflight_fix(action).await?)))
}

#[derive(Deserialize, Default)]
struct StartBody {
    #[serde(default)]
    force: bool,
}

async fn start_show(State(app): State<AppState>, Body(b): Body<StartBody>) -> ApiResult<Response> {
    let run = app.engine.start_show(b.force).await?;
    Ok((StatusCode::CREATED, Json(json!({ "show": run }))).into_response())
}

async fn skip_countdown(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "show": app.engine.skip_countdown().await? })))
}

async fn cancel_countdown(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "show": app.engine.cancel_countdown()? })))
}

async fn live_scene(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "show": app.engine.go_live_scene().await? })))
}

async fn end_show(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "show": app.engine.end_show().await? })))
}

// ------------------------------------------------------------------ OBS

async fn obs_scenes(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.obs_scenes().await?)))
}

async fn obs_inputs(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let list = app.engine.obs_inputs().await?;
    Ok(Json(json!(list.into_iter().map(|(name, kind)| json!({ "name": name, "kind": kind })).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct SceneBody {
    scene: String,
}

async fn switch_scene(State(app): State<AppState>, Body(b): Body<SceneBody>) -> ApiResult<StatusCode> {
    app.engine.switch_scene(&b.scene).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SourceBody {
    scene: String,
    item_id: i64,
    visible: bool,
}

async fn source_visibility(State(app): State<AppState>, Body(b): Body<SourceBody>) -> ApiResult<StatusCode> {
    app.engine.set_source_visible(&b.scene, b.item_id, b.visible).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MuteBody {
    input: String,
    muted: bool,
}

async fn mute_input(State(app): State<AppState>, Body(b): Body<MuteBody>) -> ApiResult<StatusCode> {
    app.engine.set_input_muted(&b.input, b.muted).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct ClipBody {
    #[serde(default)]
    label: Option<String>,
}

async fn clip(State(app): State<AppState>, Body(b): Body<ClipBody>) -> ApiResult<Response> {
    if b.label.as_ref().is_some_and(|l| l.chars().count() > 200) {
        return Err(ApiError::invalid("label: 200 characters at most."));
    }
    let r = app.engine.clip_that(b.label.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(json!(r))).into_response())
}

#[derive(Deserialize)]
struct BrbBody {
    on: bool,
}

async fn brb(State(app): State<AppState>, Body(b): Body<BrbBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.set_brb(b.on).await?))
}

async fn obs_stream(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "streaming": app.engine.obs_streaming().await? })))
}

async fn set_obs_stream(State(app): State<AppState>, Body(b): Body<BrbBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "streaming": app.engine.obs_set_streaming(b.on).await? })))
}

async fn health(State(app): State<AppState>) -> Json<Value> {
    Json(json!({ "health": app.engine.obs_health() }))
}

async fn check_health(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "health": app.engine.check_obs_health().await? })))
}

async fn switcher(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.scenes_config()))
}

async fn save_switcher(State(app): State<AppState>, Body(b): Body<Map<String, Value>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.patch_scenes_config(&b).await?)))
}

// ------------------------------------------------------------------ moments

async fn moments_config(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.moments_config()))
}

async fn save_moments_config(State(app): State<AppState>, Body(b): Body<Map<String, Value>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.patch_moments_config(&b).await?)))
}

async fn lane(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.lane()))
}

async fn dismiss(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.dismiss_lane(&id)?)))
}

async fn dismiss_all(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.dismiss_all_lane()))
}

async fn markers(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.markers(since(&q)?).await?)))
}

#[derive(Deserialize, Default)]
struct MarkerBody {
    #[serde(default)]
    label: Option<String>,
}

async fn add_marker(State(app): State<AppState>, Body(b): Body<MarkerBody>) -> ApiResult<Response> {
    let label = b.label.unwrap_or_default();
    if label.chars().count() > 200 {
        return Err(ApiError::invalid("label: 200 characters at most."));
    }
    let label = if label.trim().is_empty() { "Marker".to_string() } else { label };
    let m = app.engine.add_marker_now(ferrocast_core::moments::MomentKind::Manual, &label, None, false).await?;
    Ok((StatusCode::CREATED, Json(json!(m))).into_response())
}

async fn delete_marker(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_marker(int_id(&id, "marker")?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A download: `?format=csv|txt&since=<ms>`.
async fn export_markers(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Response> {
    let format = q.get("format").map(String::as_str).unwrap_or("csv");
    let (ct, name, body) = app.engine.export_markers(format, since(&q)?).await?;
    Ok((
        [
            (header::CONTENT_TYPE, ct.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests;
