//! App-level routes for the dashboard: looks, this stream's numbers, updates, first run.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use ferrocast_store::repo;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;

/// The overlay looks every build ships (API-REQUESTS R8). Ids match the overlay bundle;
/// an unknown id falls back to `molten`.
pub const THEMES: [(&str, &str, &str); 5] = [
    ("molten", "Molten", "Liquid-metal cards with a cobalt edge. The default."),
    ("daylight", "Daylight", "Clean white cards that read well on busy games."),
    ("arcade", "Arcade", "Chunky pixel type and hard shadows."),
    ("minimal", "Minimal", "Just the words, for streams that want quiet overlays."),
    ("sticker", "Sticker", "Bright cut-out shapes that pop on camera."),
];

pub async fn themes() -> Json<Value> {
    let list: Vec<Value> = THEMES
        .iter()
        .map(|(id, name, description)| json!({ "id": id, "name": name, "description": description, "pro": false }))
        .collect();
    Json(json!(list))
}

/// This stream so far (API-REQUESTS R7).
pub async fn session_stats(State(app): State<AppState>) -> Json<Value> {
    let (session, viewers) = app.engine.session_view();
    let messages: u32 = session.chat_lines.values().sum();
    Json(json!({
        "started_at": session.started_at,
        "follows": session.follows,
        "subs": session.subs,
        "tips": session.tips,
        "bits": session.bits,
        "messages": messages,
        "peak_viewers": session.peak_viewers,
        "viewers": viewers,
    }))
}

/// Update state (API-REQUESTS R9). There is no update feed yet, so `latest` is null; the
/// dashboard must never offer an update while `blocked_while_live` (R-60).
pub async fn updates(State(app): State<AppState>) -> Json<Value> {
    let settings = app.engine.settings();
    let live = app.engine.status()["live"].as_object().is_some_and(|m| m.values().any(|v| v == true));
    Json(json!({
        "current": env!("CARGO_PKG_VERSION"),
        "latest": Value::Null,
        "channel": settings.update_channel,
        "blocked_while_live": live && settings.never_update_while_live,
        "notes_url": Value::Null,
    }))
}

/// First run is finished: the dashboard stops showing onboarding (API-REQUESTS R10).
pub async fn setup_complete(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.db(|s| repo::settings::set(s, "setup_done", &true)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn setup_done(app: &AppState) -> bool {
    let flag: Option<bool> = app.engine.db(|s| repo::settings::get(s, "setup_done")).await.ok().flatten();
    flag.unwrap_or(false) || app.engine.overlays().await.map(|o| !o.is_empty()).unwrap_or(false)
}
