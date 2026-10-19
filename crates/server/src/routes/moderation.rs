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

pub async fn chat(State(app): State<AppState>, Query(q): Query<ChatQuery>) -> ApiResult<Json<Value>> {
    let mut list = app.engine.events(None, q.limit.unwrap_or(200).clamp(1, 500), vec!["chat_message".into()]).await?;
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

pub async fn delete_message(State(app): State<AppState>, Body(b): Body<DeleteBody>) -> ApiResult<StatusCode> {
    app.engine.delete_message(platform(&b.platform)?, &b.message_id).await?;
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

pub async fn timeout(State(app): State<AppState>, Body(b): Body<TimeoutBody>) -> ApiResult<StatusCode> {
    app.engine.timeout(platform(&b.platform)?, &b.user_id, b.seconds, b.reason.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BanUser {
    platform: String,
    user_id: String,
    login: String,
    #[serde(default)]
    display_name: Option<String>,
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
            b.user.display_name.as_deref(),
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

/// Unbans on the platform, and takes them off the shared list when they're on it. Bans
/// the filter made on its own aren't on the list, so the platform is always asked.
pub async fn unban(State(app): State<AppState>, Body(b): Body<UnbanBody>) -> ApiResult<StatusCode> {
    app.engine.unban(platform(&b.platform)?, &b.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The shared ban list, newest first (API-REQUESTS R1).
pub async fn bans(State(app): State<AppState>) -> Json<Value> {
    let mut entries = app.engine.bans().entries;
    entries.sort_by_key(|e| std::cmp::Reverse(e.at.0));
    let list: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "platform": e.platform,
                "user_id": e.user_id.unwrap_or_default(),
                "display_name": if e.display_name.is_empty() { e.login.clone() } else { e.display_name },
                "login": e.login,
                "reason": e.reason,
                "banned_at": e.at.0,
                "everywhere": e.everywhere,
                "by": e.by,
                // Done from a phone through the cloud bot while this PC was off (M10).
                "offline": e.offline,
            })
        })
        .collect();
    Json(json!(list))
}

pub async fn panic_state(State(app): State<AppState>) -> Json<Value> {
    let (on, config) = app.engine.panic_state();
    Json(json!({ "on": on, "plan": config }))
}

pub async fn panic_config(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.panic_state().1))
}

#[derive(Deserialize)]
pub struct PanicBody {
    on: bool,
}

pub async fn panic(State(app): State<AppState>, Body(b): Body<PanicBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.panic(b.on).await?)))
}

pub async fn save_panic(State(app): State<AppState>, Body(cfg): Body<PanicConfig>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.save_panic(cfg).await?)))
}

pub async fn filters(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.filter()))
}

pub async fn save_filters(State(app): State<AppState>, Body(cfg): Body<FilterConfig>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.save_filter(cfg).await?)))
}

/// Everything the dashboard shows about one person (API-REQUESTS R6).
pub async fn viewer(State(app): State<AppState>, Path((p, user)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let uid = user.clone();
    let found = app
        .engine
        .db(move |s| {
            let card = repo::viewers::card(s, p, &uid)?;
            let chat = repo::events::latest_by_actor(s, p, &uid, "chat_message")?;
            let follow = repo::events::first_follow(s, p, &uid)?;
            let points = repo::loyalty::balance(s, p, &uid)?.unwrap_or(0);
            Ok((card, chat, follow, points))
        })
        .await;
    let (card, chat, follows_since, points) = match found {
        Ok(v) => v,
        Err(ferrocast_engine::EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "viewer_not_found",
                "Ferrocast hasn't seen this person yet.",
            )
            .hint("Viewer cards fill in once someone chats or follows while Ferrocast is running."));
        }
        Err(e) => return Err(e.into()),
    };
    let key = ferrocast_engine::platform_key(p);
    let me = card.identities.iter().find(|i| i.platform == key && i.user_id == user);
    let actor = chat.as_ref().and_then(|e| e.kind.actor().cloned());
    let banned = app.engine.bans().entries.iter().any(|e| e.platform == p && e.user_id.as_deref() == Some(&user));
    let others: Vec<Value> = card
        .identities
        .iter()
        .filter(|i| !(i.platform == key && i.user_id == user))
        .map(|i| json!({ "platform": i.platform, "login": i.login }))
        .collect();
    Ok(Json(json!({
        "platform": p,
        "user_id": user,
        "login": me.map(|i| i.login.clone()).unwrap_or_default(),
        "display_name": me.map(|i| i.display_name.clone()).unwrap_or_default(),
        "avatar_url": actor.as_ref().and_then(|a| a.avatar_url.clone()),
        "roles": actor.as_ref().map(|a| a.roles.0).unwrap_or(0),
        "first_seen": me.map(|i| i.first_seen),
        "messages": me.map(|i| i.messages).unwrap_or(0),
        "follows_since": follows_since,
        "note": card.note,
        "banned": banned,
        "timeouts": me.map(|i| i.timeouts).unwrap_or(0),
        "other_identities": others,
        "points": points,
    })))
}

pub async fn note(State(app): State<AppState>, Path((p, user)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let p = platform(&p)?;
    let card = app.engine.db(move |s| repo::viewers::card(s, p, &user)).await;
    match card {
        Ok(c) => Ok(Json(json!({ "text": c.note }))),
        Err(ferrocast_engine::EngineError::Store(ferrocast_store::StoreError::NotFound(_))) => {
            Ok(Json(json!({ "text": "" })))
        }
        Err(e) => Err(e.into()),
    }
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
    if b.text.chars().count() > 2_000 {
        return Err(ApiError::invalid("Notes can be up to 2,000 characters."));
    }
    let p = platform(&p)?;
    let text = b.text.clone();
    app.engine.db(move |s| repo::viewers::set_note(s, p, &user, &text)).await?;
    Ok(Json(json!({ "text": b.text })))
}

/// Messages waiting for a mod (R-48).
pub async fn held(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.held().await?)))
}

pub async fn release_held(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.release_held(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn drop_held(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.drop_held(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
