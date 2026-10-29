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

use ferrocast_engine::studio_host::{
    AudioChange, FilterEdit, ItemEdit, MediaAct, NewSource, SceneTransition, StudioAct, StudioModeOptions, WebAct,
};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        // the Studio's engine (2.1): libobs in the desktop app
        .route("/studio/engine", get(engine_status))
        .route("/studio/engine/start", post(engine_start))
        .route("/studio/engine/stop", post(engine_stop))
        .route("/studio/engine/kinds", get(engine_kinds))
        .route("/studio/engine/kinds/{id}/options", get(engine_options))
        .route("/studio/engine/preview", put(engine_preview))
        .route("/studio/engine/kinds/{id}/defaults", get(engine_defaults))
        .route("/studio/engine/stream", get(engine_stream_stats).post(engine_stream))
        .route("/studio/engine/scenes", get(engine_scenes).post(engine_add_scene))
        .route("/studio/engine/vertical", post(engine_start_vertical).delete(engine_stop_vertical))
        .route("/studio/engine/scenes/{scene}", delete(engine_remove_scene))
        .route("/studio/engine/scenes/{scene}/duplicate", post(engine_duplicate_scene))
        .route("/studio/engine/scenes/{scene}/move", post(engine_move_scene))
        .route("/studio/engine/scenes/{scene}/current", post(engine_set_current))
        .route("/studio/engine/scenes/{scene}/items", get(engine_items).post(engine_add_item))
        .route("/studio/engine/scenes/{scene}/items/{id}/edit", post(engine_edit_item))
        .route("/studio/engine/scenes/{scene}/items/{id}/box", put(engine_item_box))
        .route("/studio/engine/scenes/{scene}/items/{id}/crop", put(engine_item_crop))
        .route("/studio/engine/scenes/{scene}/thumbnail", get(engine_scene_thumbnail))
        .route(
            "/studio/engine/scenes/{scene}/items/{id}/transform",
            get(engine_item_transform).put(engine_set_item_transform),
        )
        .route("/studio/engine/preview/selection", put(engine_select))
        .route("/studio/engine/studio-mode", put(engine_studio_mode))
        .route("/studio/engine/studio-mode/preview", put(engine_set_preview))
        .route("/studio/engine/studio-mode/transition", post(engine_transition_now))
        .route("/studio/engine/studio-mode/options", put(engine_studio_options))
        .route("/studio/engine/studio-mode/swap", post(engine_swap_scenes))
        .route("/studio/engine/studio-mode/tbar", put(engine_tbar))
        .route("/studio/engine/black", post(engine_fade_to_black))
        .route("/studio/engine/scenes/{scene}/items/{id}/transition", put(engine_item_transition))
        .route("/studio/engine/scenes/{scene}/items/{id}/group", put(engine_item_group))
        .route("/studio/engine/scenes/{scene}/items/{id}/ungroup", post(engine_ungroup))
        .route("/studio/engine/scenes/{scene}/items/{id}/order", put(engine_item_order))
        .route("/studio/engine/sources/{name}/deinterlace", put(engine_deinterlace))
        .route("/studio/engine/quick-transitions", put(engine_quick_transitions))
        .route("/studio/engine/undo", post(engine_undo))
        .route("/studio/engine/redo", post(engine_redo))
        .route("/studio/engine/history", get(engine_history))
        .route("/studio/engine/pick-file", post(engine_pick_file))
        .route("/studio/engine/web-window", post(engine_web_window))
        .route("/studio/engine/log", get(engine_log))
        .route("/studio/engine/log/show", post(engine_show_log))
        .route("/studio/engine/screen-access", get(engine_screen_access))
        .route("/studio/engine/screen-access/fix", post(engine_fix_screen_access))
        .route("/studio/engine/captions", get(engine_captions).put(engine_set_captions))
        .route("/studio/engine/restart", post(engine_restart))
        .route("/studio/engine/projectors", post(engine_projector).delete(engine_close_projectors))
        .route("/studio/engine/screens", get(engine_screens))
        .route("/studio/engine/library/{kind}", get(engine_shelf_list).post(engine_shelf_new))
        .route("/studio/engine/library/{kind}/current", put(engine_shelf_switch))
        .route("/studio/engine/library/{kind}/import", post(engine_shelf_import))
        .route("/studio/engine/library/{kind}/{id}", delete(engine_shelf_remove))
        .route("/studio/engine/library/{kind}/{id}/rename", post(engine_shelf_rename))
        .route("/studio/engine/library/{kind}/{id}/export", get(engine_shelf_export))
        .route("/studio/engine/scenes/{scene}/audio", get(engine_audio))
        .route("/studio/engine/transition", put(engine_transition))
        .route(
            "/studio/engine/transitions/{kind}/settings",
            get(engine_transition_settings).put(engine_set_transition_settings),
        )
        .route("/studio/engine/scenes/{scene}/transition", put(engine_scene_transition))
        .route("/studio/engine/sources", get(engine_sources))
        .route("/studio/engine/sources/{name}/rename", post(engine_rename))
        .route("/studio/engine/sources/{name}/settings", get(engine_source_settings).put(engine_update_source))
        .route("/studio/engine/sources/{name}/options", get(engine_source_options))
        .route("/studio/engine/sources/{name}/audio", put(engine_set_audio))
        .route("/studio/engine/sources/{name}/filters", get(engine_filters).post(engine_add_filter))
        .route("/studio/engine/sources/{name}/filters/{filter}", delete(engine_remove_filter))
        .route("/studio/engine/sources/{name}/paste-filters", post(engine_paste_filters))
        .route("/studio/engine/sources/{name}/web", post(engine_web))
        .route("/studio/engine/sources/{name}/screenshot", post(engine_screenshot))
        .route("/studio/engine/obs-collections", get(engine_obs_collections))
        .route("/studio/engine/obs-collections/import", post(engine_import_obs))
        .route("/studio/engine/streamlabs-collections", get(engine_streamlabs_collections))
        .route("/studio/engine/streamlabs-collections/import", post(engine_import_streamlabs))
        .route("/studio/engine/sources/{name}/filters/{filter}/edit", post(engine_edit_filter))
        .route(
            "/studio/engine/sources/{name}/filters/{filter}/settings",
            get(engine_filter_settings).put(engine_update_filter),
        )
        .route("/studio/engine/sources/{name}/filters/{filter}/options", get(engine_filter_options))
        .route("/studio/engine/channels/{channel}", put(engine_set_channel).delete(engine_clear_channel))
        .route("/studio/engine/levels", post(engine_levels))
        .route("/studio/engine/settings", get(engine_settings).put(engine_set_settings))
        .route("/studio/engine/stats", get(engine_stats))
        .route("/studio/engine/record", post(engine_record))
        .route("/studio/engine/record/pause", post(engine_record_pause))
        .route("/studio/engine/record/split", post(engine_record_split))
        .route("/studio/engine/replay", post(engine_replay))
        .route("/studio/engine/missing-files", get(engine_missing_files).put(engine_replace_file))
        .route("/studio/engine/sources/{name}/media", get(engine_media_state).post(engine_media))
        .route("/studio/engine/missing-files/search", post(engine_find_missing))
        .route("/studio/engine/replay/save", post(engine_replay_save))
        .route("/studio/engine/virtualcam", post(engine_virtual_camera))
        .route("/studio/engine/record/chapter", post(engine_record_chapter))
        .route("/studio/engine/remux", post(engine_remux))
        .route("/studio/engine/encoders/{id}/options", post(engine_encoder_options))
        .route("/studio/engine/monitoring", get(engine_monitoring))
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

async fn engine_status(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.studio_status().await))
}

async fn engine_start(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.studio_start().await?)))
}

async fn engine_stop(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.studio_stop().await?)))
}

async fn engine_kinds(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_kinds().await?))
}

async fn engine_options(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_options(&id).await?))
}

#[derive(Deserialize)]
struct PreviewBody {
    rect: Option<ferrocast_engine::studio_host::PreviewRect>,
    /// Studio Mode's second box, for what's on air.
    #[serde(default)]
    program: Option<ferrocast_engine::studio_host::PreviewRect>,
    /// The vertical picture's box.
    #[serde(default)]
    vertical: Option<ferrocast_engine::studio_host::PreviewRect>,
}

/// `PUT /studio/engine/preview` `{ rect: {x, y, w, h} | null }`: the preview over the page's box.
async fn engine_preview(State(app): State<AppState>, Body(b): Body<PreviewBody>) -> ApiResult<StatusCode> {
    app.engine.studio_preview(b.rect, b.program, b.vertical).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StreamBody {
    on: bool,
}

async fn engine_stream(State(app): State<AppState>, Body(b): Body<StreamBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_stream(b.on).await?))
}

async fn engine_stream_stats(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_stream_stats().await?))
}

async fn engine_defaults(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_defaults(&id).await?))
}

async fn act(app: &AppState, a: StudioAct) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_act(a).await?))
}

async fn act_done(app: &AppState, a: StudioAct) -> ApiResult<StatusCode> {
    app.engine.studio_act(a).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
    /// A scene for the vertical picture (adding scenes only).
    #[serde(default)]
    vertical: bool,
}

#[derive(Deserialize)]
struct MoveBody {
    to: usize,
}

#[derive(Deserialize)]
struct DoBody<T> {
    #[serde(rename = "do")]
    what: T,
}

#[derive(Deserialize)]
struct SettingsBody {
    settings: Value,
}

/// `POST …/items`: a new source, or (`existing`) one from another scene.
#[derive(Deserialize)]
struct AddItemBody {
    existing: Option<String>,
    /// With `existing`: a new copy of it instead of the same source (OBS's Paste (Duplicate)).
    #[serde(default)]
    copy: bool,
    /// A new group of this name: empty, or holding these sources of the scene (`items`, ids),
    /// each where it is on screen.
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    items: Option<Vec<i64>>,
    #[serde(flatten)]
    source: Option<NewSource>,
    #[serde(default)]
    fit: bool,
}

#[derive(Deserialize)]
struct LevelsBody {
    names: Vec<String>,
}

/// `GET /studio/engine/scenes` → `{ scenes, current, transition }`.
async fn engine_scenes(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Scenes).await
}

async fn engine_add_scene(
    State(app): State<AppState>,
    Body(b): Body<NameBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(v) = act(&app, StudioAct::AddScene { name: b.name, vertical: b.vertical }).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize)]
struct VerticalBody {
    /// The first vertical scene's name.
    scene: String,
}

/// `POST /studio/engine/vertical` `{ scene }`: turns the vertical picture on.
async fn engine_start_vertical(State(app): State<AppState>, Body(b): Body<VerticalBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::StartVertical { scene: b.scene }).await
}

/// `DELETE /studio/engine/vertical`: turns it off (its scenes go).
async fn engine_stop_vertical(State(app): State<AppState>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::StopVertical).await
}

async fn engine_remove_scene(State(app): State<AppState>, Path(scene): Path<String>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::RemoveScene { scene }).await
}

async fn engine_duplicate_scene(
    State(app): State<AppState>,
    Path(scene): Path<String>,
    Body(b): Body<NameBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(v) = act(&app, StudioAct::DuplicateScene { scene, name: b.name }).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn engine_move_scene(
    State(app): State<AppState>,
    Path(scene): Path<String>,
    Body(b): Body<MoveBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::MoveScene { scene, to: b.to }).await
}

/// Puts a scene on air through the transition.
async fn engine_set_current(State(app): State<AppState>, Path(scene): Path<String>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetCurrent { scene }).await
}

#[derive(Deserialize)]
struct TransitionBody {
    kind: String,
    ms: u32,
}

async fn engine_transition(State(app): State<AppState>, Body(b): Body<TransitionBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetTransition { kind: b.kind, ms: b.ms }).await
}

#[derive(Deserialize)]
struct SceneTransitionBody {
    transition: Option<SceneTransition>,
}

async fn engine_scene_transition(
    State(app): State<AppState>,
    Path(scene): Path<String>,
    Body(b): Body<SceneTransitionBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetSceneTransition { scene, transition: b.transition }).await
}

async fn engine_items(State(app): State<AppState>, Path(scene): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Items { scene }).await
}

async fn engine_add_item(
    State(app): State<AppState>,
    Path(scene): Path<String>,
    Body(b): Body<AddItemBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let a = match (b.existing, b.source) {
        _ if b.group.is_some() => match b.items {
            Some(ids) => StudioAct::GroupItems { scene, ids, name: b.group.unwrap_or_default() },
            None => StudioAct::AddGroup { scene, name: b.group.unwrap_or_default() },
        },
        (Some(name), _) if b.copy => StudioAct::PasteCopy { scene, name },
        (Some(name), _) => StudioAct::AddExisting { scene, name },
        (None, Some(source)) => StudioAct::AddSource { scene, source, fit: b.fit },
        (None, None) => {
            return Err(ApiError::invalid("kind and name for a new source, or existing for one already made."));
        }
    };
    let Json(v) = act(&app, a).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn engine_edit_item(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<DoBody<ItemEdit>>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::EditItem { scene, id, edit: b.what }).await
}

#[derive(Deserialize)]
struct BoxBody {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    #[serde(default)]
    save: bool,
}

/// `PUT …/items/{id}/box` `{ x, y, w, h, save? }`: moved or resized in the preview → the item.
async fn engine_item_box(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<BoxBody>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::SetItemBox { scene, id, x: b.x, y: b.y, w: b.w, h: b.h, save: b.save }).await
}

#[derive(Deserialize)]
struct ThumbQuery {
    #[serde(default = "thumb_width")]
    w: u32,
}

fn thumb_width() -> u32 {
    256
}

/// `GET …/scenes/{scene}/thumbnail?w=256` → `{ image }`: a small PNG of the scene now (the
/// Scenes grid's tiles).
async fn engine_scene_thumbnail(
    State(app): State<AppState>,
    Path(scene): Path<String>,
    Query(q): Query<ThumbQuery>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Thumbnail { scene, width: q.w }).await
}

#[derive(Deserialize)]
struct CropBody {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    #[serde(default)]
    save: bool,
}

/// `PUT …/items/{id}/crop` `{ left, top, right, bottom, save? }`: cropped in the preview → the item.
async fn engine_item_crop(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<CropBody>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::SetItemCrop { scene, id, crop: [b.left, b.top, b.right, b.bottom], save: b.save }).await
}

async fn engine_item_transform(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Transform { scene, id }).await
}

/// `PUT …/items/{id}/transform` with the whole transform → the item.
async fn engine_set_item_transform(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(transform): Body<ferrocast_engine::studio_host::ItemTransform>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::SetTransform { scene, id, transform }).await
}

async fn engine_transition_settings(State(app): State<AppState>, Path(kind): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::TransitionSettings { kind }).await
}

/// `PUT /studio/engine/transitions/{kind}/settings` `{ settings }`: changes only the keys sent.
async fn engine_set_transition_settings(
    State(app): State<AppState>,
    Path(kind): Path<String>,
    Body(b): Body<SettingsBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetTransitionSettings { kind, settings: b.settings }).await
}

/// `PUT /studio/engine/studio-mode` `{ on }`.
async fn engine_studio_mode(State(app): State<AppState>, Body(b): Body<OnBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::StudioMode { on: b.on }).await
}

async fn engine_set_preview(State(app): State<AppState>, Body(b): Body<SceneBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetPreview { scene: b.scene }).await
}

#[derive(Deserialize)]
struct QuickBody {
    #[serde(default)]
    quick: Option<SceneTransition>,
}

/// `POST /studio/engine/studio-mode/transition` `{ quick? }`: the preview goes on air.
async fn engine_transition_now(State(app): State<AppState>, Body(b): Body<QuickBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::TransitionNow { quick: b.quick }).await
}

/// `PUT /studio/engine/studio-mode/options` `{ swap, duplicate_scene, duplicate_sources, double_click }`.
async fn engine_studio_options(
    State(app): State<AppState>,
    Body(options): Body<StudioModeOptions>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetStudioOptions { options }).await
}

/// `POST /studio/engine/studio-mode/swap`: the preview and what's on air swap, with a cut.
async fn engine_swap_scenes(State(app): State<AppState>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SwapScenes).await
}

#[derive(Deserialize)]
struct TBarBody {
    at: f32,
    #[serde(default)]
    release: bool,
}

/// `PUT /studio/engine/studio-mode/tbar` `{ at, release? }` → `{ active, at }`.
async fn engine_tbar(State(app): State<AppState>, Body(b): Body<TBarBody>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::TBar { at: b.at, release: b.release }).await
}

#[derive(Deserialize)]
struct GroupBody {
    group: Option<i64>,
}

/// `PUT …/items/{id}/group` `{ group }`: into that group of the scene, or out (null).
async fn engine_item_group(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<GroupBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetGroup { scene, id, group: b.group }).await
}

#[derive(Deserialize)]
struct OrderBody {
    to: usize,
}

/// `PUT …/items/{id}/order` `{ to }`: moves it to that place in the list (0: on top).
async fn engine_item_order(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<OrderBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::ItemOrder { scene, id, to: b.to }).await
}

/// `POST …/items/{id}/ungroup`: the group goes; its sources stay in the scene.
async fn engine_ungroup(State(app): State<AppState>, Path((scene, id)): Path<(String, i64)>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::Ungroup { scene, id }).await
}

#[derive(Deserialize)]
struct ItemTransitionBody {
    show: bool,
    transition: Option<SceneTransition>,
}

/// `PUT /studio/engine/scenes/{scene}/items/{id}/transition` `{ show, transition }` (null: none).
async fn engine_item_transition(
    State(app): State<AppState>,
    Path((scene, id)): Path<(String, i64)>,
    Body(b): Body<ItemTransitionBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetItemTransition { scene, id, show: b.show, transition: b.transition }).await
}

#[derive(Deserialize)]
struct DeinterlaceBody {
    mode: String,
    #[serde(default = "yes")]
    top_field_first: bool,
}

fn yes() -> bool {
    true
}

/// `PUT /studio/engine/sources/{name}/deinterlace` `{ mode, top_field_first? }`.
async fn engine_deinterlace(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<DeinterlaceBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetDeinterlace { name, mode: b.mode, top_field_first: b.top_field_first }).await
}

/// `POST /studio/engine/black` → `{ black }`: fades to black, or back.
async fn engine_fade_to_black(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::FadeToBlack).await
}

#[derive(Deserialize)]
struct QuickListBody {
    list: Vec<SceneTransition>,
}

async fn engine_quick_transitions(State(app): State<AppState>, Body(b): Body<QuickListBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetQuickTransitions { list: b.list }).await
}

/// `POST /studio/engine/undo` → `{ done, undo, redo }`.
async fn engine_undo(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Undo).await
}

async fn engine_redo(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Redo).await
}

/// `GET /studio/engine/history` → `{ undo: [labels], redo: [labels] }`, newest first.
async fn engine_history(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::History).await
}

#[derive(Deserialize)]
struct SelectBody {
    scene: Option<String>,
    id: Option<i64>,
    #[serde(default)]
    frames: bool,
    #[serde(default)]
    also: Vec<i64>,
}

/// `PUT /studio/engine/preview/selection` `{ scene, id }` (both null for none).
async fn engine_select(State(app): State<AppState>, Body(b): Body<SelectBody>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::Select { scene: b.scene, id: b.id, frames: b.frames, also: b.also }).await
}

/// `POST /studio/engine/pick-file` `{ kind, filter? }` → `{ path }` (null when cancelled).
async fn engine_pick_file(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::studio_host::FilePick>,
) -> ApiResult<Json<Value>> {
    let path = app.engine.studio_pick_file(b).await?;
    Ok(Json(serde_json::json!({ "path": path })))
}

#[derive(serde::Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

/// `GET /studio/engine/log?lines=` (default 500, at most 5000) → `{ file, lines }`, secrets
/// taken out.
async fn engine_log(State(app): State<AppState>, Query(q): Query<LogQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_log(q.lines.unwrap_or(500)).await?))
}

/// `POST /studio/engine/log/show` → 204: the log file in Finder or Explorer.
async fn engine_show_log(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.studio_show_log().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct WebWindowBody {
    name: String,
    url: String,
}

/// `POST /studio/engine/web-window` `{ name, url }` → `{ ok: true }`: a web panel's page in a
/// window of its own.
async fn engine_web_window(State(app): State<AppState>, Body(b): Body<WebWindowBody>) -> ApiResult<Json<Value>> {
    app.engine.studio_web_window(b.name, b.url).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn engine_screen_access(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_screen_access().await?))
}

async fn engine_captions(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_captions(None).await?))
}

async fn engine_set_captions(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::studio_host::CaptionsSet>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_captions(Some(b)).await?))
}

async fn engine_fix_screen_access(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_fix_screen_access().await?))
}

/// `POST /studio/engine/projectors` `{ what, name?, screen?, on_top? }` → 201 `{ id }`.
async fn engine_projector(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::studio_host::ProjectorReq>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(app.engine.studio_projector(b).await?)))
}

async fn engine_close_projectors(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.studio_close_projectors().await?;
    Ok(StatusCode::NO_CONTENT)
}

use ferrocast_engine::studio_host::ShelfAct;

async fn engine_shelf_list(State(app): State<AppState>, Path(kind): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_shelf(&kind, ShelfAct::List).await?))
}

#[derive(Deserialize)]
struct ShelfNewBody {
    name: String,
    #[serde(default)]
    copy: bool,
}

/// `POST /studio/engine/library/{kind}` `{ name, copy? }` → 201 `{ id }`, switched to.
async fn engine_shelf_new(
    State(app): State<AppState>,
    Path(kind): Path<String>,
    Body(b): Body<ShelfNewBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let v = app.engine.studio_shelf(&kind, ShelfAct::New { name: b.name, copy: b.copy }).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn engine_shelf_switch(
    State(app): State<AppState>,
    Path(kind): Path<String>,
    Body(b): Body<IdBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_shelf(&kind, ShelfAct::Switch { id: b.id }).await?))
}

async fn engine_shelf_rename(
    State(app): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
    Body(b): Body<NameBody>,
) -> ApiResult<StatusCode> {
    app.engine.studio_shelf(&kind, ShelfAct::Rename { id, name: b.name }).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn engine_shelf_remove(
    State(app): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    app.engine.studio_shelf(&kind, ShelfAct::Remove { id }).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn engine_shelf_export(
    State(app): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_shelf(&kind, ShelfAct::Export { id }).await?))
}

#[derive(Deserialize)]
struct ImportBody {
    name: String,
    doc: Value,
}

/// `POST /studio/engine/library/collections/import` `{ name, doc }` → 201, what came in.
async fn engine_shelf_import(
    State(app): State<AppState>,
    Path(kind): Path<String>,
    Body(b): Body<ImportBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let v = app.engine.studio_shelf(&kind, ShelfAct::Import { name: b.name, doc: b.doc }).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn engine_screens(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_screens().await?))
}

async fn engine_restart(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.studio_restart().await?;
    Ok(StatusCode::ACCEPTED)
}

async fn engine_audio(State(app): State<AppState>, Path(scene): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Audio { scene }).await
}

/// `GET /studio/engine/sources`: every source, with the scenes it's in.
async fn engine_sources(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Sources).await
}

async fn engine_rename(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<NameBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::Rename { name, new_name: b.name }).await
}

async fn engine_source_settings(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Settings { name }).await
}

async fn engine_source_options(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Options { name }).await
}

async fn engine_update_source(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<SettingsBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::Update { name, settings: b.settings }).await
}

async fn engine_set_audio(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(change): Body<AudioChange>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetAudio { name, change }).await
}

async fn engine_filters(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Filters { name }).await
}

async fn engine_add_filter(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(filter): Body<NewSource>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Json(v) = act(&app, StudioAct::AddFilter { name, filter }).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn engine_remove_filter(
    State(app): State<AppState>,
    Path((name, filter)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::RemoveFilter { name, filter }).await
}

#[derive(Deserialize)]
struct FromBody {
    from: String,
}

async fn engine_obs_collections(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_obs_collections().await?))
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

async fn engine_import_obs(State(app): State<AppState>, Body(b): Body<IdBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_import_obs(&b.id).await?))
}

async fn engine_streamlabs_collections(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_streamlabs_collections().await?))
}

async fn engine_import_streamlabs(State(app): State<AppState>, Body(b): Body<IdBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_import_streamlabs(&b.id).await?))
}

async fn engine_screenshot(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Screenshot { name }).await
}

async fn engine_web(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<DoBody<WebAct>>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::Web { name, what: b.what }).await
}

async fn engine_paste_filters(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<FromBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::PasteFilters { name, from: b.from }).await
}

async fn engine_edit_filter(
    State(app): State<AppState>,
    Path((name, filter)): Path<(String, String)>,
    Body(b): Body<DoBody<FilterEdit>>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::EditFilter { name, filter, edit: b.what }).await
}

async fn engine_filter_settings(
    State(app): State<AppState>,
    Path((name, filter)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::FilterSettings { name, filter }).await
}

async fn engine_filter_options(
    State(app): State<AppState>,
    Path((name, filter)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::FilterOptions { name, filter }).await
}

async fn engine_update_filter(
    State(app): State<AppState>,
    Path((name, filter)): Path<(String, String)>,
    Body(b): Body<SettingsBody>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::UpdateFilter { name, filter, settings: b.settings }).await
}

/// A global sound device (every scene) on channel 1 to 6.
async fn engine_set_channel(
    State(app): State<AppState>,
    Path(channel): Path<u32>,
    Body(device): Body<NewSource>,
) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetChannel { channel, device: Some(device) }).await
}

async fn engine_clear_channel(State(app): State<AppState>, Path(channel): Path<u32>) -> ApiResult<StatusCode> {
    act_done(&app, StudioAct::SetChannel { channel, device: None }).await
}

async fn engine_settings(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.studio_settings().await?)))
}

/// `PUT /studio/engine/settings` (the whole object; missing fields take their defaults) →
/// `{ settings, restart }`.
async fn engine_set_settings(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::studio_host::StudioSettings>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_set_settings(b).await?))
}

async fn engine_stats(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_stats().await?))
}

#[derive(Deserialize)]
struct OnBody {
    on: bool,
}

async fn engine_record(State(app): State<AppState>, Body(b): Body<OnBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_record(b.on).await?))
}

/// `GET /studio/engine/sources/{name}/media` → `{ state, time_ms, duration_ms }`.
async fn engine_media_state(State(app): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Media { name, what: None, seek_ms: None }).await
}

#[derive(Deserialize)]
struct MediaBody {
    #[serde(rename = "do", default)]
    what: Option<MediaAct>,
    #[serde(default)]
    seek_ms: Option<i64>,
}

/// `POST /studio/engine/sources/{name}/media` `{ do?: play | pause | play_pause | restart | stop
/// | next | previous, seek_ms? }` → where it is now.
async fn engine_media(
    State(app): State<AppState>,
    Path(name): Path<String>,
    Body(b): Body<MediaBody>,
) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Media { name, what: b.what, seek_ms: b.seek_ms }).await
}

/// `GET /studio/engine/missing-files` → the files sources use that aren't here.
async fn engine_missing_files(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::MissingFiles).await
}

#[derive(Deserialize)]
struct ReplaceFileBody {
    source: String,
    #[serde(default)]
    parent: Option<String>,
    kind: String,
    key: String,
    path: String,
    to: String,
}

/// `PUT /studio/engine/missing-files` `{ source, parent?, kind, key, path, to }`: `to` empty
/// clears it.
async fn engine_replace_file(State(app): State<AppState>, Body(b): Body<ReplaceFileBody>) -> ApiResult<StatusCode> {
    act_done(
        &app,
        StudioAct::ReplaceFile { source: b.source, parent: b.parent, kind: b.kind, key: b.key, path: b.path, to: b.to },
    )
    .await
}

#[derive(Deserialize)]
struct FolderBody {
    folder: String,
}

/// `POST /studio/engine/missing-files/search` `{ folder }` → `{ found: { path: new path } }`.
async fn engine_find_missing(State(app): State<AppState>, Body(b): Body<FolderBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_find_missing(b.folder).await?))
}

/// `POST /studio/engine/replay` `{ on }` → `{ active }`: OBS's replay buffer.
async fn engine_replay(State(app): State<AppState>, Body(b): Body<OnBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_replay(b.on).await?))
}

/// `POST /studio/engine/replay/save` → `{ file }`: the last seconds saved to a file.
async fn engine_replay_save(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_replay_save().await?))
}

#[derive(Deserialize)]
struct CameraBody {
    on: bool,
    /// One scene or source instead of what's on air.
    #[serde(default)]
    source: Option<String>,
}

/// `POST /studio/engine/virtualcam` `{ on, source? }` → `{ active }`.
async fn engine_virtual_camera(State(app): State<AppState>, Body(b): Body<CameraBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_virtual_camera(b.on, b.source).await?))
}

/// `POST /studio/engine/record/split`: the recording goes on in a new file.
async fn engine_record_split(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.studio_record_split().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ChapterBody {
    #[serde(default)]
    name: String,
}

/// `POST /studio/engine/record/chapter` `{ name? }`: a chapter marker here (MP4, MOV).
async fn engine_record_chapter(State(app): State<AppState>, Body(b): Body<ChapterBody>) -> ApiResult<StatusCode> {
    app.engine.studio_record_chapter(b.name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RemuxBody {
    file: String,
}

/// `POST /studio/engine/remux` `{ file }` → `{ to }`: an MP4 made beside it, in the background.
async fn engine_remux(State(app): State<AppState>, Body(b): Body<RemuxBody>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_remux(b.file).await?))
}

#[derive(Deserialize)]
struct EncoderSettingsBody {
    #[serde(default)]
    settings: serde_json::Map<String, Value>,
}

/// `POST /studio/engine/encoders/{id}/options` `{ settings }` → `{ options, values }`.
async fn engine_encoder_options(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<EncoderSettingsBody>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_encoder_options(id, b.settings).await?))
}

async fn engine_record_pause(State(app): State<AppState>, Body(b): Body<OnBody>) -> ApiResult<StatusCode> {
    app.engine.studio_record_pause(b.on).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn engine_monitoring(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.studio_monitoring().await?))
}

/// `POST /studio/engine/levels` `{ names }` → `{ name: { peak: [l, r], magnitude: [l, r] } }`.
async fn engine_levels(State(app): State<AppState>, Body(b): Body<LevelsBody>) -> ApiResult<Json<Value>> {
    act(&app, StudioAct::Levels { names: b.names }).await
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
