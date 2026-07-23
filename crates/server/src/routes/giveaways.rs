//! Giveaways: start, close entries, draw, redraw, end, cancel.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::giveaway::GiveawaySpec;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// The running giveaway, or the last one that ended; `null` when there's none.
pub async fn current(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.giveaway().await))
}

/// `{ prize, keyword?: "!join", cost?: 0, subscriber_bonus?: false }`, read by hand so each
/// wrong type gets a message naming its field.
pub async fn start(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<impl IntoResponse> {
    if !b.is_object() {
        return Err(ApiError::invalid("Send the giveaway as an object: { prize, keyword, cost, subscriber_bonus }."));
    }
    let prize = match &b["prize"] {
        Value::String(s) => s.clone(),
        _ => return Err(ApiError::invalid("prize: must be text.")),
    };
    let keyword = match &b["keyword"] {
        Value::Null => "!join".to_string(),
        Value::String(s) => s.clone(),
        _ => return Err(ApiError::invalid("keyword: must be text, like !join.")),
    };
    let cost = match &b["cost"] {
        Value::Null => 0,
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| ApiError::invalid("cost: must be a whole number from 0 to 1,000,000,000 points."))?,
        _ => return Err(ApiError::invalid("cost: must be a whole number of points.")),
    };
    let subscriber_bonus = match &b["subscriber_bonus"] {
        Value::Null => false,
        Value::Bool(v) => *v,
        _ => return Err(ApiError::invalid("subscriber_bonus: must be true or false.")),
    };
    let view = app.engine.start_giveaway(GiveawaySpec { prize, keyword, cost, subscriber_bonus }).await?;
    Ok((StatusCode::CREATED, Json(json!(view))))
}

pub async fn close(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.close_giveaway().await?)))
}

pub async fn draw(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.draw_giveaway(false).await?)))
}

pub async fn redraw(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.draw_giveaway(true).await?)))
}

pub async fn end(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.end_giveaway().await?)))
}

/// Cancels it and refunds every entry fee: `{ refunded }` people.
pub async fn cancel(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "refunded": app.engine.cancel_giveaway().await? })))
}
