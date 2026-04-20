//! Chat and moderation routes.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use ferrocast_core::filter::FilterConfig;
use ferrocast_core::moderation::PanicConfig;
use ferrocast_store::repo;
use serde::Deserialize;
use serde_json::{Value, json};

use super::platform;
use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

#[derive(Deserialize)]
pub struct ChatQuery {
    limit: Option<u32>,
}

pub async fn chat(
    State(app): State<AppState>,
    Query(q): Query<ChatQuery>,
) -> ApiResult<Json<Value>> {
    let mut list = app
        .engine
        .events(
            None,
            q.limit.unwrap_or(200).clamp(1, 500),
            vec!["chat_message".into()],
        )
        .await?;
    list.reverse();
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Targets {
    All(String),
    Some(Vec<String>),
}

#[derive(Deserialize)]
pub struct ReplyTo {
    platform: String,
    message_id: String,
}

#[derive(Deserialize)]
pub struct SendBody {
    text: String,
    platforms: Targets,
    #[serde(default)]
    reply_to: Option<ReplyTo>,
}

pub async fn send(State(app): State<AppState>, Body(b): Body<SendBody>) -> ApiResult<Json<Value>> {
    if b.text.chars().count() > 2_000 {
        return Err(ApiError::invalid("Messages can be up to 2,000 characters."));
    }
    let platforms = match b.platforms {
        Targets::All(s) if s == "all" => vec![],
        Targets::All(s) => vec![platform(&s)?],
        Targets::Some(list) => list.iter().map(|s| platform(s)).collect::<Result<_, _>>()?,
    };
    let reply = match b.reply_to {
        Some(r) => Some((platform(&r.platform)?, r.message_id)),
        None => None,
    };
    let out = app.engine.send_chat(&b.text, &platforms, reply).await?;
    Ok(Json(json!({ "sent": out.ok, "failed": out.failed })))
}

#[derive(Deserialize)]
pub struct DeleteBody {
    platform: String,
    message_id: String,
}

pub async fn delete_message(
    State(app): State<AppState>,
    Body(b): Body<DeleteBody>,
) -> ApiResult<StatusCode> {
    app.engine
        .delete_message(platform(&b.platform)?, &b.message_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TimeoutBody {
    platform: String,
    user_id: String,
    seconds: u32,
    #[serde(default)]
    reason: Option<String>,
}

pub async fn timeout(
    State(app): State<AppState>,
    Body(b): Body<TimeoutBody>,
) -> ApiResult<StatusCode> {
    app.engine
        .timeout(
            platform(&b.platform)?,
            &b.user_id,
            b.seconds,
            b.reason.as_deref(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BanUser {
    platform: String,
    user_id: String,
    login: String,
}

#[derive(Deserialize)]
pub struct BanBody {
    user: BanUser,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    everywhere: bool,
}

pub async fn ban(State(app): State<AppState>, Body(b): Body<BanBody>) -> ApiResult<Json<Value>> {
    if b.user.user_id.trim().is_empty() {
        return Err(ApiError::invalid("Who to ban is missing (user_id)."));
    }
    let reason = b.reason.map(|r| r.chars().take(500).collect::<String>());
    let out = app
        .engine
        .ban(
            platform(&b.user.platform)?,
            &b.user.user_id,
            &b.user.login,
            reason,
            b.everywhere,
        )
        .await?;
    Ok(Json(json!({ "banned": out.ok, "failed": out.failed })))
}

#[derive(Deserialize)]
pub struct UnbanBody {
    platform: String,
    user_id: String,
}

pub async fn unban(State(app): State<AppState>, Body(b): Body<UnbanBody>) -> ApiResult<StatusCode> {
    app.engine.unban(platform(&b.platform)?, &b.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn bans(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.bans()))
}

pub async fn panic_state(State(app): State<AppState>) -> Json<Value> {
    let (on, config) = app.engine.panic_state();
    Json(json!({ "on": on, "config": config }))
}

#[derive(Deserialize)]
pub struct PanicBody {
    on: bool,
}

pub async fn panic(
    State(app): State<AppState>,
    Body(b): Body<PanicBody>,
) -> ApiResult<Json<Value>> {
    let plan = app.engine.panic(b.on).await?;
    Ok(Json(json!({ "on": b.on, "plan": plan })))
}

pub async fn save_panic(
    State(app): State<AppState>,
    Body(cfg): Body<PanicConfig>,
) -> ApiResult<Json<Value>> {
    if cfg.slow_seconds.is_some_and(|s| s == 0 || s > 120) {
        return Err(ApiError::invalid("Slow mode is 1 to 120 seconds."));
    }
    if cfg.followers_only_minutes.is_some_and(|m| m > 129_600) {
        return Err(ApiError::invalid("Followers-only can be up to 90 days."));
    }
    Ok(Json(json!(app.engine.save_panic(cfg).await?)))
}

pub async fn filters(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.filter()))
}

pub async fn save_filters(
    State(app): State<AppState>,
    Body(cfg): Body<FilterConfig>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.save_filter(cfg).await?)))
}

pub async fn viewer(
    State(app): State<AppState>,
    Path((p, user)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let card = app
        .engine
        .db(move |s| repo::viewers::card(s, p, &user))
        .await?;
    Ok(Json(json!(card)))
}

#[derive(Deserialize)]
pub struct NoteBody {
    text: String,
}

pub async fn save_note(
    State(app): State<AppState>,
    Path((p, user)): Path<(String, String)>,
    Body(b): Body<NoteBody>,
) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let card = app
        .engine
        .db(move |s| {
            repo::viewers::set_note(s, p, &user, &b.text)?;
            repo::viewers::card(s, p, &user)
        })
        .await?;
    Ok(Json(json!(card)))
}
