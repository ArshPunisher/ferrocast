//! The bot in cloud mode (docs/CLOUD.md §6): the existing cloud bot, keyed by the account,
//! with its commands and timers edited in the web dashboard through the engine's own paths
//! (`/c/api/v1/commands`, `/c/api/v1/timers`), so the Bot screen works unchanged. They're kept
//! as the bot's config (`bot_config`, the same `CloudBotConfig`, checked by the same code).

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ferrocast_core::cloudbot::BotConfig;
use ferrocast_core::command::{Command, Timer};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{body, created, e, invalid, json, no_store, not_found, typed, web_account};
use crate::{ApiError, AppState};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/api/v1/pro/bot", get(view))
        .route("/c/api/v1/pro/bot/enabled", put(enabled))
        .route("/c/api/v1/bot/config", get(get_config).put(put_config))
        .route("/c/api/v1/commands", get(list_commands).post(create_command))
        .route("/c/api/v1/commands/starter", post(starter))
        .route("/c/api/v1/commands/{id}", put(put_command).delete(delete_command))
        .route("/c/api/v1/timers", get(list_timers).post(create_timer))
        .route("/c/api/v1/timers/{id}", put(put_timer).delete(delete_timer))
}

async fn view(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(crate::bot::account_view(&st, &a.account.id)?))
}

#[derive(Deserialize)]
struct EnabledIn {
    enabled: bool,
}

async fn enabled(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let b: EnabledIn = typed(body(&headers, &b)?)?;
    Ok(json(crate::bot::account_set_enabled(&st, &a.account.id, b.enabled).await?))
}

async fn get_config(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let (version, config) = crate::bot::account_config(&st, &a.account.id)?;
    Ok(json(json!({ "version": version, "config": config })))
}

#[derive(Deserialize)]
struct ConfigIn {
    version: i64,
    config: Value,
}

async fn put_config(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let b: ConfigIn = typed(body(&headers, &b)?)?;
    if b.version < 0 {
        return Err(invalid("version: a whole number from 0."));
    }
    let cfg: BotConfig = serde_json::from_value(b.config).map_err(|err| invalid(format!("config: {err}")))?;
    let version = crate::bot::account_save_config(&st, &a.account.id, b.version, cfg)?;
    Ok(json(json!({ "version": version })))
}

/// Reads, changes and saves the account's bot config (once more if another tab saved
/// meanwhile); what `f` returns.
fn edit<T>(st: &AppState, account: &str, f: impl Fn(&mut BotConfig) -> Result<T, ApiError>) -> Result<T, ApiError> {
    for attempt in 0..2 {
        let (version, mut cfg) = crate::bot::account_config(st, account)?;
        let out = f(&mut cfg)?;
        match crate::bot::account_save_config(st, account, version, cfg) {
            Ok(_) => return Ok(out),
            Err(err) if err.message().contains("another tab") && attempt == 0 => {}
            Err(err) => return Err(err),
        }
    }
    Err(e(StatusCode::CONFLICT, "stale_version", "The bot settings keep changing in another tab.")
        .hint("Reload the Bot page and try again."))
}

fn command_error(err: ferrocast_core::command::CommandError) -> ApiError {
    let field = match err {
        ferrocast_core::command::CommandError::BadName | ferrocast_core::command::CommandError::Taken(_) => "name",
        _ => "response",
    };
    invalid(format!("{field}: {err}"))
}

fn fresh_id() -> String {
    super::docs::new_id()
}

// ── Commands ─────────────────────────────────────────────────────────────────────────

async fn list_commands(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!(crate::bot::account_config(&st, &a.account.id)?.1.commands)))
}

async fn create_command(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    if v.get("id").is_none() {
        v["id"] = json!("");
    }
    let mut c: Command = typed(v)?;
    if c.id.trim().is_empty() {
        c.id = fresh_id();
    }
    let saved = edit(&st, &a.account.id, |cfg| {
        if cfg.commands.iter().any(|x| x.id == c.id) {
            return Err(e(StatusCode::CONFLICT, "conflict", "A command with that id already exists."));
        }
        c.validate(&cfg.commands).map_err(command_error)?;
        cfg.commands.push(c.clone());
        Ok(c.clone())
    })?;
    Ok(created(json!(saved)))
}

async fn put_command(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    v["id"] = json!(id);
    let c: Command = typed(v)?;
    let saved = edit(&st, &a.account.id, |cfg| {
        let others: Vec<Command> = cfg.commands.iter().filter(|x| x.id != id).cloned().collect();
        let Some(slot) = cfg.commands.iter_mut().find(|x| x.id == id) else { return Err(not_found("command")) };
        c.validate(&others).map_err(command_error)?;
        *slot = c.clone();
        Ok(c.clone())
    })?;
    Ok(json(json!(saved)))
}

async fn delete_command(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    edit(&st, &a.account.id, |cfg| {
        let before = cfg.commands.len();
        cfg.commands.retain(|x| x.id != id);
        if cfg.commands.len() == before { Err(not_found("command")) } else { Ok(()) }
    })?;
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

/// `POST /c/api/v1/commands/starter` → `{ added, skipped }` (names already used are skipped).
async fn starter(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let (added, skipped) = edit(&st, &a.account.id, |cfg| {
        let mut added = Vec::new();
        let mut skipped = Vec::new();
        for mut c in ferrocast_core::starter::starter_commands() {
            c.id = fresh_id();
            if c.validate(&cfg.commands).is_ok() {
                cfg.commands.push(c.clone());
                added.push(c);
            } else {
                skipped.push(c.name);
            }
        }
        Ok((added, skipped))
    })?;
    Ok(json(json!({ "added": added, "skipped": skipped })))
}

// ── Timers ───────────────────────────────────────────────────────────────────────────

async fn list_timers(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!(crate::bot::account_config(&st, &a.account.id)?.1.timers)))
}

fn check_timer(t: &Timer) -> Result<(), ApiError> {
    if t.every_min == 0 || t.every_min > 1_440 {
        return Err(invalid("every_min: from 1 to 1,440 minutes."));
    }
    t.validate().map_err(|err| invalid(format!("message: {err}")))
}

async fn create_timer(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    if v.get("id").is_none() {
        v["id"] = json!("");
    }
    let mut t: Timer = typed(v)?;
    if t.id.trim().is_empty() {
        t.id = fresh_id();
    }
    check_timer(&t)?;
    let saved = edit(&st, &a.account.id, |cfg| {
        if cfg.timers.iter().any(|x| x.id == t.id) {
            return Err(e(StatusCode::CONFLICT, "conflict", "A timer with that id already exists."));
        }
        cfg.timers.push(t.clone());
        Ok(t.clone())
    })?;
    Ok(created(json!(saved)))
}

async fn put_timer(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    v["id"] = json!(id);
    let t: Timer = typed(v)?;
    check_timer(&t)?;
    let saved = edit(&st, &a.account.id, |cfg| {
        let Some(slot) = cfg.timers.iter_mut().find(|x| x.id == id) else { return Err(not_found("timer")) };
        *slot = t.clone();
        Ok(t.clone())
    })?;
    Ok(json(json!(saved)))
}

async fn delete_timer(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    edit(&st, &a.account.id, |cfg| {
        let before = cfg.timers.len();
        cfg.timers.retain(|x| x.id != id);
        if cfg.timers.len() == before { Err(not_found("timer")) } else { Ok(()) }
    })?;
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}
