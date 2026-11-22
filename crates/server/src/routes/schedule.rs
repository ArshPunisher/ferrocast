//! The stream scheduler (API.md "Schedule"): plans with a time, scheduled broadcasts made on the
//! platforms that have them, a reminder before the start. Nothing here goes live.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/schedule", get(list).post(create)).route("/schedule/{id}", delete(remove))
}

async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "streams": app.engine.schedule_list().await? })))
}

async fn create(
    State(app): State<AppState>,
    Body(b): Body<ferrocast_engine::schedule::NewScheduled>,
) -> ApiResult<Response> {
    let item = app.engine.schedule_create(b).await?;
    Ok((StatusCode::CREATED, Json(json!(item))).into_response())
}

async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.schedule_delete(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
