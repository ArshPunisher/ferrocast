//! Bot commands, timers and TTS routes.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use ferrocast_core::command::{Command, Timer};
use ferrocast_core::money::Money;
use ferrocast_core::tts::TtsPolicy;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn commands(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.commands()))
}

pub async fn create_command(State(app): State<AppState>, Body(mut c): Body<Command>) -> ApiResult<impl IntoResponse> {
    if c.id.trim().is_empty() {
        c.id = ferrocast_store::new_id();
    }
    if app.engine.commands().iter().any(|x| x.id == c.id) {
        return Err(ApiError::new(StatusCode::CONFLICT, "conflict", "A command with that id already exists."));
    }
    Ok((StatusCode::CREATED, Json(json!(app.engine.save_command(c).await?))))
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

pub async fn delete_command(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_command(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// "Add starter commands": `!socials`, `!lurk`, `!uptime`, `!so`, skipping names in use.
pub async fn add_starter(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.add_starter_commands().await?)))
}

pub async fn timers(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.timers()))
}

pub async fn create_timer(State(app): State<AppState>, Body(mut t): Body<Timer>) -> ApiResult<impl IntoResponse> {
    if t.id.trim().is_empty() {
        t.id = ferrocast_store::new_id();
    }
    if app.engine.timers().iter().any(|x| x.id == t.id) {
        return Err(ApiError::new(StatusCode::CONFLICT, "conflict", "A timer with that id already exists."));
    }
    Ok((StatusCode::CREATED, Json(json!(app.engine.save_timer(t).await?))))
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

pub async fn delete_timer(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.delete_timer(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// TTS settings in the dashboard's shape: `min_amount` is the minimum in the streamer's
/// main currency (Settings `currency`), in major units; every per-currency minimum is in
/// `min_amounts` alongside.
fn tts_view(p: &TtsPolicy, main: &str) -> Value {
    let min = p.min_amounts.iter().find(|m| m.currency == main).cloned().unwrap_or_else(|| Money::new(0, main));
    let mut v = json!(p);
    v["voice"] = json!(p.voice.clone().unwrap_or_default());
    v["min_amount"] = json!(min.major());
    v["currency"] = json!(main);
    v
}

pub async fn tts(State(app): State<AppState>) -> Json<Value> {
    Json(tts_view(&app.engine.tts_policy(), &app.engine.settings().currency))
}

/// Every field is optional: what isn't sent keeps its saved value.
#[derive(Deserialize)]
pub struct TtsBody {
    enabled: Option<bool>,
    voice: Option<String>,
    /// Major units of the main currency. Applied after `min_amounts`, so it wins for that
    /// currency.
    min_amount: Option<f64>,
    /// The full per-currency list.
    min_amounts: Option<Vec<Money>>,
    min_platform_value: Option<u64>,
    max_chars: Option<usize>,
    hold_for_approval: Option<bool>,
    read_names: Option<bool>,
}

fn currency_ok(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())
}

pub async fn save_tts(State(app): State<AppState>, Body(b): Body<TtsBody>) -> ApiResult<Json<Value>> {
    let current = app.engine.tts_policy();
    let main = app.engine.settings().currency;
    let mut min_amounts: Vec<Money> = Vec::new();
    for m in b.min_amounts.unwrap_or_else(|| current.min_amounts.clone()) {
        let m = Money::new(m.minor, m.currency.trim());
        if !currency_ok(&m.currency) {
            return Err(ApiError::invalid(format!("min_amounts: \"{}\" isn't a currency code.", m.currency)));
        }
        if m.minor < 0 {
            return Err(ApiError::invalid("min_amounts: minimums can't be negative."));
        }
        // One entry per currency; a later duplicate replaces an earlier one.
        min_amounts.retain(|x| x.currency != m.currency);
        min_amounts.push(m);
    }
    if let Some(shown) = b.min_amount {
        if !shown.is_finite() || !(0.0..=10_000_000.0).contains(&shown) {
            return Err(ApiError::invalid("min_amount: must be between 0 and 10,000,000."));
        }
        let minor = (shown * 10f64.powi(ferrocast_core::money::exponent(&main) as i32)).round() as i64;
        min_amounts.retain(|m| m.currency != main);
        min_amounts.insert(0, Money::new(minor, &main));
    }
    let voice = match b.voice {
        Some(v) => Some(v.trim().to_string()).filter(|v| !v.is_empty()),
        None => current.voice.clone(),
    };
    let p = TtsPolicy {
        enabled: b.enabled.unwrap_or(current.enabled),
        voice,
        min_amounts,
        min_platform_value: b.min_platform_value.unwrap_or(current.min_platform_value),
        max_chars: b.max_chars.unwrap_or(current.max_chars),
        read_names: b.read_names.unwrap_or(current.read_names),
        hold_for_approval: b.hold_for_approval.unwrap_or(current.hold_for_approval),
    };
    Ok(Json(tts_view(&app.engine.save_tts(p).await?, &main)))
}

pub async fn tts_pending(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.tts_pending().await?)))
}

pub async fn tts_approve(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.decide_tts(&id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn tts_reject(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.decide_tts(&id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}
