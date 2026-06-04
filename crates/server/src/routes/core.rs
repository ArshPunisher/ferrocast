//! Status, pairing, accounts, overlays, alerts, queue, events, goals and settings.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::alert::AlertRule;
use ferrocast_core::goal::Goal;
use ferrocast_core::overlay::{Overlay, Preset, Widget};
use ferrocast_engine::state::EngineSettings;
use ferrocast_store::{backup, repo};
use serde::Deserialize;
use serde_json::{Value, json};

use super::platform;
use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn status(State(app): State<AppState>) -> Json<Value> {
    let mut v = app.engine.status();
    v["version"] = json!(env!("CARGO_PKG_VERSION"));
    v["port"] = json!(app.guard.port);
    let overlays = app.engine.overlays().await.map(|o| !o.is_empty()).unwrap_or(false);
    v["setup_done"] = json!(overlays);
    Json(v)
}

#[derive(Deserialize)]
pub struct PairBody {
    code: String,
}

pub async fn pair(State(app): State<AppState>, Body(b): Body<PairBody>) -> ApiResult<Json<Value>> {
    let token = app.sessions.pair(&b.code)?;
    Ok(Json(json!({ "token": token })))
}

pub async fn pair_code(State(app): State<AppState>) -> Json<Value> {
    let code = app.sessions.new_code();
    let expires_at = now_ms() + crate::auth::PAIR_TTL.as_millis() as i64;
    Json(
        json!({ "code": code, "url": format!("{}/app/#pair={code}", app.engine.base_url()), "expires_at": expires_at }),
    )
}

fn now_ms() -> i64 {
    ferrocast_engine::now().0
}

pub async fn accounts(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let list = app.engine.db(repo::settings::accounts).await?;
    let out: Vec<Value> = list
        .into_iter()
        .map(|a| {
            json!({ "platform": a.platform, "name": a.display_name, "avatar_url": a.avatar_url,
                         "connected_at": a.connected_at, "scopes_ok": true })
        })
        .collect();
    Ok(Json(json!(out)))
}

pub async fn connect(State(app): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    Ok(Json(json!(app.engine.connect_account(p).await?)))
}

pub async fn disconnect(State(app): State<AppState>, Path(p): Path<String>) -> ApiResult<StatusCode> {
    app.engine.disconnect_account(platform(&p)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn overlays(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.overlays().await?)))
}

#[derive(Deserialize)]
pub struct NewOverlay {
    name: String,
    #[serde(default)]
    preset: Option<Preset>,
}

pub async fn create_overlay(State(app): State<AppState>, Body(b): Body<NewOverlay>) -> ApiResult<impl IntoResponse> {
    let o = app.engine.create_overlay(&b.name, b.preset.unwrap_or(Preset::Starter)).await?;
    Ok((StatusCode::CREATED, Json(json!(o))))
}

pub async fn overlay(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let o = app.engine.overlay(&id).await?;
    Ok(Json(json!(app.engine.overlay_view(&o))))
}

#[derive(Deserialize)]
pub struct OverlayEdit {
    name: String,
    width: i32,
    height: i32,
    widgets: Vec<Widget>,
}

pub async fn update_overlay(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<OverlayEdit>,
) -> ApiResult<Json<Value>> {
    let current = app.engine.overlay(&id).await?;
    let edited = Overlay { name: b.name, width: b.width, height: b.height, widgets: b.widgets, ..current };
    Ok(Json(json!(app.engine.update_overlay(edited).await?)))
}

pub async fn delete_overlay(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_overlay(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn regenerate_url(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.regenerate_overlay_url(&id).await?)))
}

pub async fn add_to_obs(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let added = app.engine.add_to_obs(&id).await.map_err(|e| {
        let api = ApiError::from(e);
        if api.code == "unavailable" {
            api.hint("Or copy the overlay URL and add a Browser Source in OBS yourself.")
        } else {
            api
        }
    })?;
    Ok(Json(json!(added)))
}

pub async fn obs(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.obs().await?)))
}

#[derive(Deserialize)]
pub struct ObsBody {
    host: String,
    port: u16,
    /// Absent keeps the saved password; `null` clears it; a string replaces it.
    #[serde(default, deserialize_with = "deserialize_some")]
    password: Option<Option<String>>,
}

fn deserialize_some<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

pub async fn save_obs(State(app): State<AppState>, Body(b): Body<ObsBody>) -> ApiResult<Json<Value>> {
    use ferrocast_engine::PasswordChange;
    let change = match b.password {
        None => PasswordChange::Keep,
        Some(None) => PasswordChange::Clear,
        Some(Some(p)) if p.chars().count() > 256 => {
            return Err(ApiError::invalid("That password is too long."));
        }
        Some(Some(p)) => PasswordChange::Set(p),
    };
    let cfg = ferrocast_obs::ObsConfig { host: b.host, port: b.port };
    Ok(Json(json!(app.engine.save_obs(cfg, change).await?)))
}

pub async fn rules(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.rules()))
}

pub async fn create_rule(State(app): State<AppState>, Body(mut r): Body<AlertRule>) -> ApiResult<impl IntoResponse> {
    if r.id.trim().is_empty() {
        r.id = ferrocast_store::new_id();
    }
    if app.engine.rules().iter().any(|x| x.id == r.id) {
        return Err(ApiError::new(StatusCode::CONFLICT, "conflict", "A rule with that id already exists."));
    }
    Ok((StatusCode::CREATED, Json(json!(app.engine.save_rule(r).await?))))
}

pub async fn update_rule(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(mut r): Body<AlertRule>,
) -> ApiResult<Json<Value>> {
    if !app.engine.rules().iter().any(|x| x.id == id) {
        return Err(ApiError::not_found("alert rule"));
    }
    r.id = id;
    Ok(Json(json!(app.engine.save_rule(r).await?)))
}

pub async fn delete_rule(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_rule(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TestBody {
    kind: String,
    #[serde(default)]
    platform: Option<String>,
}

pub async fn test(State(app): State<AppState>, Body(b): Body<TestBody>) -> ApiResult<impl IntoResponse> {
    let p = b.platform.as_deref().map(platform).transpose()?;
    app.engine.test(&b.kind, p).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))))
}

pub async fn queue(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.queue().await))
}

pub async fn queue_pause(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.queue_pause().await))
}

pub async fn queue_resume(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.queue_resume().await))
}

pub async fn queue_skip(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.queue_skip().await))
}

pub async fn queue_clear(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.queue_clear().await))
}

pub async fn queue_remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.queue_remove(&id).await?)))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    before: Option<i64>,
    limit: Option<u32>,
    kinds: Option<String>,
}

pub async fn events(State(app): State<AppState>, Query(q): Query<EventsQuery>) -> ApiResult<Json<Value>> {
    let kinds: Vec<String> = q
        .kinds
        .map(|k| k.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(20).collect())
        .unwrap_or_default();
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let events = app.engine.events(q.before, limit, kinds).await?;
    let next = if events.len() as u32 == limit { events.last().map(|e| e.seq) } else { None };
    Ok(Json(json!({ "events": events, "next_before": next })))
}

pub async fn replay(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    app.engine.replay(&id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))))
}

pub async fn why(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let w = app.engine.why(&id).ok_or_else(|| {
        ApiError::not_found("event").hint("Ferrocast remembers why for the last 500 events of this session.")
    })?;
    Ok(Json(json!(w)))
}

pub async fn goals(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.goals()))
}

pub async fn create_goal(State(app): State<AppState>, Body(mut g): Body<Goal>) -> ApiResult<impl IntoResponse> {
    if g.id.trim().is_empty() {
        g.id = ferrocast_store::new_id();
    }
    if app.engine.goals().iter().any(|x| x.id == g.id) {
        return Err(ApiError::new(StatusCode::CONFLICT, "conflict", "A goal with that id already exists."));
    }
    Ok((StatusCode::CREATED, Json(json!(app.engine.save_goal(g).await?))))
}

pub async fn update_goal(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(mut g): Body<Goal>,
) -> ApiResult<Json<Value>> {
    let Some(current) = app.engine.goals().into_iter().find(|x| x.id == id) else {
        return Err(ApiError::not_found("goal"));
    };
    g.id = id;
    // Progress only moves through events and "adjust", never by editing the goal.
    g.current = current.current;
    Ok(Json(json!(app.engine.save_goal(g).await?)))
}

pub async fn delete_goal(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_goal(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Adjust {
    delta: i64,
}

pub async fn adjust_goal(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<Adjust>,
) -> ApiResult<Json<Value>> {
    if b.delta.unsigned_abs() > 1_000_000_000 {
        return Err(ApiError::invalid("That change is too large."));
    }
    Ok(Json(json!(app.engine.adjust_goal(&id, b.delta).await?)))
}

pub async fn settings(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.settings()))
}

pub async fn save_settings(State(app): State<AppState>, Body(s): Body<EngineSettings>) -> ApiResult<Json<Value>> {
    if !(1..=3650).contains(&s.keep_chat_days) {
        return Err(ApiError::invalid("Keep chat for 1 to 3650 days."));
    }
    Ok(Json(json!(app.engine.save_settings(s).await?)))
}

pub async fn backups(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let dir = app.engine.store().dir().to_path_buf();
    let list = app.engine.db(move |_| backup::list(&dir)).await?;
    Ok(Json(json!(list)))
}

pub async fn backup_now(State(app): State<AppState>) -> ApiResult<impl IntoResponse> {
    app.engine.db(|s| s.snapshot("manual")).await?;
    let dir = app.engine.store().dir().to_path_buf();
    let list = app.engine.db(move |_| backup::list(&dir)).await?;
    Ok((StatusCode::CREATED, Json(json!(list))))
}

pub async fn restore_backup(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let dir = app.engine.store().dir().to_path_buf();
    app.engine.db(move |_| backup::schedule_restore(&dir, &id)).await?;
    Ok(Json(json!({ "restart_needed": true, "message": "Restart Ferrocast to finish restoring." })))
}

pub async fn save_tips(State(app): State<AppState>, Body(cfg): Body<Value>) -> ApiResult<Json<Value>> {
    if !cfg.is_object() {
        return Err(ApiError::invalid("The tip page settings must be an object.")
            .hint("Send { handle, display_name, message, razorpay?, kofi?, stripe? }."));
    }
    let saved = app.engine.save_tips(&cfg).await?;
    // Keep the relay's cleaned-up copy: normalised names, and secrets only as "has_secret".
    let mut public = match saved.get("config") {
        Some(c) if c.is_object() => c.clone(),
        _ => return Err(ApiError::unavailable("The relay saved the tip page but didn't send it back. Reload Tips.")),
    };
    if let Some(obj) = public.as_object_mut() {
        obj.insert("page_url".into(), saved["page_url"].clone());
        obj.insert("webhooks".into(), saved["webhooks"].clone());
    }
    let stored = public.clone();
    app.engine.db(move |s| repo::settings::set(s, "tips", &stored)).await?;
    Ok(Json(public))
}

pub async fn tips(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Option<Value> = app.engine.db(|s| repo::settings::get(s, "tips")).await?;
    Ok(Json(v.unwrap_or_else(|| {
        json!({ "handle": "", "display_name": "", "message": "", "page_url": null, "webhooks": null,
                "razorpay": null, "kofi": null, "stripe": null })
    })))
}
