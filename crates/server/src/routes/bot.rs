//! Bot commands, timers and TTS routes.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::command::{Command, Timer};
use ferrocast_core::tts::TtsPolicy;
use ferrocast_store::repo;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn commands(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.commands()))
}

pub async fn create_command(
    State(app): State<AppState>,
    Body(mut c): Body<Command>,
) -> ApiResult<impl IntoResponse> {
    if c.id.trim().is_empty() {
        c.id = ferrocast_store::new_id();
    }
    if app.engine.commands().iter().any(|x| x.id == c.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "A command with that id already exists.",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!(app.engine.save_command(c).await?)),
    ))
}

pub async fn update_command(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(mut c): Body<Command>,
) -> ApiResult<Json<Value>> {
    if !app.engine.commands().iter().any(|x| x.id == id) {
        return Err(ApiError::not_found("command"));
    }
    c.id = id;
    Ok(Json(json!(app.engine.save_command(c).await?)))
}

pub async fn delete_command(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    app.engine.delete_command(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn timers(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.timers()))
}

pub async fn create_timer(
    State(app): State<AppState>,
    Body(mut t): Body<Timer>,
) -> ApiResult<impl IntoResponse> {
    if t.id.trim().is_empty() {
        t.id = ferrocast_store::new_id();
    }
    if app.engine.timers().iter().any(|x| x.id == t.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "A timer with that id already exists.",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!(app.engine.save_timer(t).await?)),
    ))
}

pub async fn update_timer(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(mut t): Body<Timer>,
) -> ApiResult<Json<Value>> {
    if !app.engine.timers().iter().any(|x| x.id == id) {
        return Err(ApiError::not_found("timer"));
    }
    t.id = id;
    Ok(Json(json!(app.engine.save_timer(t).await?)))
}

pub async fn delete_timer(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    app.engine.delete_timer(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn tts(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.tts_policy()))
}

pub async fn save_tts(
    State(app): State<AppState>,
    Body(p): Body<TtsPolicy>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.save_tts(p).await?)))
}

pub async fn tts_pending(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.db(repo::events::pending_tts).await?)))
}

pub async fn tts_approve(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    app.engine.decide_tts(&id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn tts_reject(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    app.engine.decide_tts(&id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}
