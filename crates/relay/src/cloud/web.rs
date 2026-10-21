//! The web dashboard's cloud mode routes on the web host (docs/CLOUD.md §9.1): the cookie
//! session (CSRF on writes), the engine's own paths under `/c/api/v1`, so the dashboard's
//! screens talk to the cloud with the same client.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{any, get, post};
use ferrocast_core::alert::AlertRule;
use ferrocast_core::design::{Design, DesignKind};
use ferrocast_core::overlay::{Overlay, Preset};
use serde::Deserialize;
use serde_json::{Value, json};

use super::docs::{self, UseRequest};
use super::{body, created, json, typed, web_account};
use crate::{ApiError, AppState, now_ms};

type Bytes = axum::body::Bytes;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/v1/cloud", get(summary))
        .route("/c/v1/cloud/enable", post(enable))
        .route("/c/api/v1/status", get(status))
        .route("/c/api/v1/overlays", get(list_overlays).post(create_overlay))
        .route("/c/api/v1/overlays/{id}", get(get_overlay).put(put_overlay).delete(delete_overlay))
        .route("/c/api/v1/overlays/{id}/regenerate-url", post(regenerate))
        .route("/c/api/v1/alert-rules", get(list_rules).post(create_rule))
        .route("/c/api/v1/alert-rules/{id}", axum::routing::put(put_rule).delete(delete_rule))
        .route("/c/api/v1/designs", get(list_designs).post(create_design))
        .route("/c/api/v1/designs/active-alert", get(active_alert).put(set_active_alert))
        .route("/c/api/v1/designs/{id}", get(get_design).put(put_design).delete(delete_design))
        .route("/c/api/v1/designs/{id}/duplicate", post(duplicate_design))
        .route("/c/api/v1/designs/{id}/use", post(use_design))
        .route("/c/api/v1/{*rest}", any(not_in_cloud))
}

/// Anything the cloud doesn't do: the app does it.
async fn not_in_cloud(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    web_account(&st, &headers, &method, false)?;
    Err(super::e(StatusCode::NOT_FOUND, "not_in_cloud", "That needs the Ferrocast app on your PC.")
        .hint("Cloud mode runs alerts, designs, overlays, chat and the bot. The rest stays in the app."))
}

// ── Cloud mode itself ────────────────────────────────────────────────────────────────

/// What the Cloud page shows: on or off, sleeping, usage and limits, channels.
pub(crate) fn summary_json(st: &AppState, account: &str) -> Result<Value, ApiError> {
    let row = st.db.cloud_account(account).map_err(ApiError::db)?;
    let enabled = row.is_some();
    let usage = if enabled {
        json!({
            "bytes": st.db.cloud_media_bytes(account).map_err(ApiError::db)?,
            "bytes_limit": super::STORAGE_LIMIT,
            "overlays": st.db.cloud_doc_count(account, docs::OVERLAY).map_err(ApiError::db)?,
            "rules": st.db.cloud_doc_count(account, docs::RULE).map_err(ApiError::db)?,
            "designs": st.db.cloud_doc_count(account, docs::DESIGN).map_err(ApiError::db)?,
        })
    } else {
        Value::Null
    };
    let channels: Vec<Value> = st
        .db
        .cloud_channels(account)
        .map_err(ApiError::db)?
        .into_iter()
        .map(|c| {
            json!({ "platform": c.platform, "login": c.login, "name": c.name, "avatar_url": c.avatar_url,
                    "connected_at": c.connected_at, "error": c.error.and_then(|e| serde_json::from_str::<Value>(&e).ok()) })
        })
        .collect();
    Ok(json!({
        "enabled": enabled,
        "sleeping": row.as_ref().is_some_and(|r| r.sleeping_at.is_some()),
        "enabled_at": row.as_ref().map(|r| r.enabled_at),
        "overlay_origin": super::overlay_origin(st),
        "usage": usage,
        "limits": {
            "bytes": super::STORAGE_LIMIT, "file_bytes": super::FILE_LIMIT, "overlays": super::MAX_OVERLAYS,
            "rules": super::MAX_RULES, "designs": super::MAX_DESIGNS, "history_days": super::HISTORY_MS / 86_400_000,
            "sleep_after_days": super::SLEEP_AFTER_MS / 86_400_000,
        },
        "channels": channels,
        "features": { "twitch": st.bot.twitch_cfg().is_some(), "kick": st.cfg.kick_client_id.is_some(), "youtube": false },
    }))
}

/// `GET /c/v1/cloud`
async fn summary(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, false)?;
    Ok(json(summary_json(&st, &a.account.id)?))
}

/// `POST /c/v1/cloud/enable`: free, idempotent; only the first time seeds the default rules
/// and a Starter overlay.
async fn enable(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, false)?;
    let made = st.db.cloud_enable(&a.account.id, now_ms()).map_err(ApiError::db)?;
    if made {
        // Only the first time: defaults the streamer deleted never come back.
        tracing::info!("an account turned cloud mode on");
        docs::seed(&st, &a.account.id)?;
    }
    Ok(json(summary_json(&st, &a.account.id)?))
}

/// The engine's `Status` shape for a dashboard in cloud mode: connectors from the channels,
/// live from the bus.
pub(crate) fn status_json(st: &AppState, account: &str) -> Result<Value, ApiError> {
    let channels = st.db.cloud_channels(account).map_err(ApiError::db)?;
    let live = st.cloud.buses.get(account).map(|b| b.state().live.clone()).unwrap_or_default();
    let connector = |p: &str| {
        let ch = channels.iter().find(|c| c.platform == p);
        match ch {
            Some(c) => json!({
                "state": if c.error.is_some() { "sign_in_needed" } else { "connected" },
                "account": { "name": c.name.clone().unwrap_or_else(|| c.login.clone()), "avatar_url": c.avatar_url },
                "detail": c.error.as_deref().and_then(|e| serde_json::from_str::<Value>(e).ok()).and_then(|e| e["message"].as_str().map(String::from)),
                "last_event_at": null,
            }),
            None => json!({ "state": "off", "account": null, "detail": null, "last_event_at": null }),
        }
    };
    let is_live = |p: &str| live.get(p).copied().unwrap_or(false);
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"), "port": 0, "cloud": true,
        "live": { "twitch": is_live("twitch"), "youtube": is_live("youtube"), "kick": is_live("kick") },
        "connectors": { "twitch": connector("twitch"), "youtube": connector("youtube"), "kick": connector("kick") },
        "setup_done": true,
    }))
}

/// `GET /c/api/v1/status`
async fn status(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(status_json(&st, &a.account.id)?))
}

// ── Overlays ─────────────────────────────────────────────────────────────────────────

async fn list_overlays(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let id = &a.account.id;
    let views =
        docs::overlays(&st, id)?.iter().map(|o| docs::overlay_view(&st, id, o)).collect::<Result<Vec<_>, _>>()?;
    Ok(json(json!(views)))
}

#[derive(Deserialize)]
struct NewOverlay {
    name: String,
    #[serde(default)]
    preset: Option<Preset>,
}

async fn create_overlay(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let n: NewOverlay = typed(body(&headers, &b)?)?;
    if n.name.trim().is_empty() {
        return Err(super::invalid("name: give the overlay a name."));
    }
    let o = docs::create_overlay(&st, &a.account.id, &n.name, n.preset.unwrap_or(Preset::Starter))?;
    Ok(created(docs::overlay_view(&st, &a.account.id, &o)?))
}

async fn get_overlay(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let o = docs::overlay(&st, &a.account.id, &id)?;
    Ok(json(docs::overlay_view(&st, &a.account.id, &o)?))
}

async fn put_overlay(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    // The dashboard sends the view back (with `url`); the token is never the client's.
    v["token"] = json!("");
    v["id"] = json!(id);
    if v.get("updated_at").is_none() {
        v["updated_at"] = json!(0);
    }
    let o: Overlay = typed(v)?;
    let saved = docs::update_overlay(&st, &a.account.id, &id, o)?;
    super::changed_overlay(&st, &a.account.id, &saved);
    Ok(json(docs::overlay_view(&st, &a.account.id, &saved)?))
}

async fn delete_overlay(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    docs::delete_overlay(&st, &a.account.id, &id)?;
    super::revoked_overlay(&st, &a.account.id, &id);
    Ok(super::no_store(axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT)))
}

async fn regenerate(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let o = docs::overlay(&st, &a.account.id, &id)?;
    super::rotate_url(&st, &a.account.id, &id)?;
    super::revoked_overlay(&st, &a.account.id, &id);
    Ok(json(docs::overlay_view(&st, &a.account.id, &o)?))
}

// ── Alert rules ──────────────────────────────────────────────────────────────────────

async fn list_rules(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!(docs::rules(&st, &a.account.id)?)))
}

async fn create_rule(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    if v.get("id").is_none() {
        v["id"] = json!("");
    }
    let r: AlertRule = typed(v)?;
    Ok(created(json!(docs::create_rule(&st, &a.account.id, r)?)))
}

async fn put_rule(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    v["id"] = json!(id);
    let r: AlertRule = typed(v)?;
    Ok(json(json!(docs::update_rule(&st, &a.account.id, &id, r)?)))
}

async fn delete_rule(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    docs::delete_rule(&st, &a.account.id, &id)?;
    Ok(super::no_store(axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT)))
}

// ── Designs ──────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct KindQuery {
    #[serde(default)]
    kind: Option<String>,
}

fn kind_of(s: &str) -> Result<DesignKind, ApiError> {
    match s {
        "alert" => Ok(DesignKind::Alert),
        "theme" => Ok(DesignKind::Theme),
        "scene" => Ok(DesignKind::Scene),
        "widget" => Ok(DesignKind::Widget),
        _ => Err(super::invalid(format!("kind: \"{s}\" isn't a design kind; use alert, theme, scene or widget."))),
    }
}

async fn list_designs(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Query(q): Query<KindQuery>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let kind = q.kind.as_deref().filter(|k| !k.is_empty()).map(kind_of).transpose()?;
    Ok(json(json!(docs::designs(&st, &a.account.id, kind)?)))
}

async fn create_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let d: Design = typed(body(&headers, &b)?)?;
    Ok(created(json!(docs::create_design(&st, &a.account.id, d)?)))
}

async fn get_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!(docs::design(&st, &a.account.id, &id)?)))
}

async fn put_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let mut v = body(&headers, &b)?;
    v["id"] = json!(id);
    let d: Design = typed(v)?;
    Ok(json(json!(docs::update_design(&st, &a.account.id, &id, d)?)))
}

async fn delete_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    docs::delete_design(&st, &a.account.id, &id)?;
    Ok(super::no_store(axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT)))
}

#[derive(Deserialize, Default)]
struct DuplicateBody {
    #[serde(default)]
    name: Option<String>,
}

async fn duplicate_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let d: DuplicateBody = typed(body(&headers, &b)?)?;
    Ok(created(json!(docs::duplicate_design(&st, &a.account.id, &id, d.name.as_deref())?)))
}

async fn use_design(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let req: UseRequest = typed(body(&headers, &b)?)?;
    let account = &a.account.id;
    let (alert_design, changed) = docs::use_design(&st, account, &id, req)?;
    for o in &changed {
        super::changed_overlay(&st, account, o);
    }
    let views = changed.iter().map(|o| docs::overlay_view(&st, account, o)).collect::<Result<Vec<_>, _>>()?;
    Ok(json(json!({ "alert_design": alert_design, "overlays": views })))
}

async fn active_alert(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!({ "design_id": docs::settings(&st, &a.account.id)?.active_alert_design })))
}

#[derive(Deserialize)]
struct ActiveBody {
    design_id: Option<String>,
}

async fn set_active_alert(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let req: ActiveBody = typed(body(&headers, &b)?)?;
    let id = docs::set_active_alert_design(&st, &a.account.id, req.design_id.as_deref())?;
    Ok(json(json!({ "design_id": id })))
}
