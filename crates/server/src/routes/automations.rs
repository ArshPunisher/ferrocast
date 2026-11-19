//! "When X, do Y" rules (crates/engine/src/automation.rs): the list, saving the whole list, and
//! a rule's Test button.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing::get, routing::post};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;
use ferrocast_engine::automation::Rule;

pub fn routes() -> Router<AppState> {
    Router::new().route("/automations", get(list).put(save)).route("/automations/{id}/run", post(run))
}

async fn list(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.automations()))
}

/// `PUT /automations` with the whole list → the list as saved.
async fn save(State(app): State<AppState>, Body(rules): Body<Vec<Rule>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_automations(rules)?)))
}

/// `POST /automations/{id}/run` → 202: its steps run now, cooldown or not.
async fn run(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.run_automation(&id)?;
    Ok(StatusCode::ACCEPTED)
}
