//! Auto-replies (API.md §4 "Auto-replies"): the bot answers chat that mentions something.
//! 400 names a bad field, 404 an unknown id, 409 a reorder of a list that changed, 413 a
//! reply over 500 characters, 422 a pattern that can't be used or the 100-rule limit.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use ferrocast_core::autoreply::AutoReply;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/autoreplies", get(list).post(create))
        .route("/autoreplies/order", put(reorder))
        .route("/autoreplies/try", post(try_it))
        .route("/autoreplies/starters", get(starters))
        .route("/autoreplies/{id}", put(update).delete(remove))
}

async fn list(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.autoreplies()))
}

async fn create(State(app): State<AppState>, Body(r): Body<AutoReply>) -> ApiResult<impl IntoResponse> {
    Ok((StatusCode::CREATED, Json(json!(app.engine.create_autoreply(r).await?))))
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(r): Body<AutoReply>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.update_autoreply(&id, r).await?)))
}

async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_autoreply(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Order {
    ids: Vec<String>,
}

async fn reorder(State(app): State<AppState>, Body(o): Body<Order>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.reorder_autoreplies(o.ids).await?)))
}

#[derive(Deserialize)]
struct TryBody {
    text: String,
    /// An unsaved rule from the add/edit dialog; without it, the saved list is tried.
    #[serde(default)]
    rule: Option<AutoReply>,
    #[serde(default)]
    platform: Option<String>,
}

async fn try_it(State(app): State<AppState>, Body(b): Body<TryBody>) -> ApiResult<Json<Value>> {
    let platform = b.platform.as_deref().map(super::platform).transpose()?;
    Ok(Json(json!(app.engine.try_autoreply(&b.text, b.rule, platform).await?)))
}

/// The examples the empty list offers ("discord" → the link, "what game" → the category).
async fn starters() -> Json<Value> {
    Json(json!(ferrocast_core::autoreply::starters()))
}

#[cfg(test)]
mod tests;
