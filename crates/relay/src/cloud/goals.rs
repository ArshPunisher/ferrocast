//! Goals in cloud mode (docs/CLOUD.md §7.1): account documents of kind `goal` (the core
//! `Goal`, progress in minor units), checked with the engine's own rules (`GoalView::into_goal`,
//! a goal with progress keeps its kind and currency), moved by live events on the account's
//! bus (`goal::contribution`) and by hand (`adjust`), shown in overlays (`state { goals }`)
//! and on the dashboard (`goals` frames). The engine's paths, so the dashboard's goal screens
//! work unchanged. Goals aren't synced with the desktop app (its goals count its own events).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ferrocast_core::event::{Event, Origin};
use ferrocast_core::goal::{Goal, GoalView};
use serde::Deserialize;
use serde_json::json;

use super::bus::{Audience, Bus};
use super::{body, created, e, invalid, json, limit, no_store, not_found, typed, web_account};
use crate::{ApiError, AppState, now_ms};

pub(crate) const GOAL: &str = "goal";
/// Goals per account.
pub const MAX_GOALS: i64 = 20;
/// The largest change by hand, in the goal's stored units (as in the engine).
const MAX_ADJUST: i64 = 1_000_000_000_000;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/api/v1/goals", get(list).post(create))
        .route("/c/api/v1/goals/{id}", put(update).delete(remove))
        .route("/c/api/v1/goals/{id}/adjust", post(adjust))
}

/// The account's goals, oldest first.
pub(crate) fn stored(st: &AppState, account: &str) -> Result<Vec<Goal>, ApiError> {
    Ok(st
        .db
        .cloud_docs(account, GOAL)
        .map_err(ApiError::db)?
        .iter()
        .filter_map(|d| d.json.as_deref().and_then(|j| serde_json::from_str(j).ok()))
        .collect())
}

/// Reads the goals into the bus once (overlay snapshots and the pipeline use them).
pub(crate) fn load(st: &AppState, bus: &Bus) {
    if bus.state().goals.is_some() {
        return;
    }
    if let Ok(g) = stored(st, &bus.account) {
        bus.state().goals = Some(g);
    }
}

fn views(goals: &[Goal]) -> Vec<GoalView> {
    goals.iter().map(Goal::view).collect()
}

/// The goals changed: read again, and every open overlay and dashboard hears it.
fn changed(st: &AppState, account: &str) {
    let Some(bus) = st.cloud.buses.get(account) else { return };
    let Ok(goals) = stored(st, account) else { return };
    let v = views(&goals);
    bus.state().goals = Some(goals);
    push(&bus, &v);
}

fn push(bus: &Bus, v: &[GoalView]) {
    bus.send_overlay(Audience::All, "state", &json!({ "goals": v }));
    bus.send_dashboard("goals", &v);
}

fn encode(g: &Goal) -> Result<String, ApiError> {
    serde_json::to_string(g).map_err(|_| invalid("That goal can't be saved."))
}

fn goal_error(message: String) -> ApiError {
    invalid(message)
}

/// `GET /c/api/v1/goals` → `Goal[]` (tips goals in major units).
async fn list(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    Ok(json(json!(views(&stored(&st, &a.account.id)?))))
}

/// `POST /c/api/v1/goals` → 201 `Goal`.
async fn create(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    let mut v = body(&headers, &b)?;
    if v.get("id").is_none() {
        v["id"] = json!("");
    }
    let view: GoalView = typed(v)?;
    let mut g = view.into_goal().map_err(goal_error)?;
    if g.id.trim().is_empty() {
        g.id = super::docs::new_id();
    }
    if g.id.len() > 80 || g.id.chars().any(|c| c.is_control() || c == '/') {
        return Err(invalid("id: 1 to 80 characters, no slashes."));
    }
    if g.starts_at.0 == 0 {
        g.starts_at = ferrocast_core::event::Millis(now_ms());
    }
    if st.db.cloud_doc(account, GOAL, &g.id).map_err(ApiError::db)?.is_some_and(|d| d.deleted_at.is_none()) {
        return Err(e(StatusCode::CONFLICT, "conflict", "A goal with that id already exists."));
    }
    // Counted and written in one transaction, so two at once can't pass the cap.
    if !st.db.cloud_create_capped(account, GOAL, &g.id, &encode(&g)?, MAX_GOALS, now_ms()).map_err(ApiError::db)? {
        return Err(limit(format!("Cloud mode keeps up to {MAX_GOALS} goals.")).hint("Delete a goal you've finished."));
    }
    changed(&st, account);
    Ok(created(json!(g.view())))
}

/// `PUT /c/api/v1/goals/{id}` → `Goal`: progress is never taken from the body; a goal with
/// progress can't change its kind or currency (409).
async fn update(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    let mut v = body(&headers, &b)?;
    v["id"] = json!(id);
    let view: GoalView = typed(v)?;
    let mut g = view.into_goal().map_err(goal_error)?;
    let edited = st
        .db
        .cloud_doc_edit(account, GOAL, &id, now_ms(), |old| {
            let old: Goal = serde_json::from_str(old).map_err(|_| invalid("The stored goal can't be read."))?;
            let unit_changed = old.kind != g.kind || old.currency != g.currency;
            if unit_changed && old.current != 0 {
                return Err(e(
                    StatusCode::CONFLICT,
                    "conflict",
                    "This goal already has progress, so its kind and currency can't change.",
                )
                .hint("Make a new goal instead."));
            }
            g.current = if unit_changed { 0 } else { old.current };
            if g.starts_at.0 == 0 {
                g.starts_at = old.starts_at;
            }
            encode(&g).map(Some)
        })
        .map_err(ApiError::db)??;
    if edited.is_none() {
        return Err(not_found("goal"));
    }
    changed(&st, account);
    Ok(json(json!(g.view())))
}

/// `DELETE /c/api/v1/goals/{id}` → 204.
async fn remove(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    if !st.db.cloud_doc(account, GOAL, &id).map_err(ApiError::db)?.is_some_and(|d| d.deleted_at.is_none()) {
        return Err(not_found("goal"));
    }
    st.db.cloud_write(account, GOAL, &id, None, None, now_ms()).map_err(ApiError::db)?;
    changed(&st, account);
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

#[derive(Deserialize)]
struct Adjust {
    /// In the goal's display units (₹ for a rupee tips goal).
    delta: f64,
}

/// `POST /c/api/v1/goals/{id}/adjust { delta }` → `Goal` ("+1 death", a tip given in cash).
async fn adjust(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let account = &a.account.id;
    let req: Adjust = typed(body(&headers, &b)?)?;
    if !req.delta.is_finite() {
        return Err(invalid("delta: a number."));
    }
    let mut out: Option<Goal> = None;
    let edited = st
        .db
        .cloud_doc_edit(account, GOAL, &id, now_ms(), |old| {
            let mut g: Goal = serde_json::from_str(old).map_err(|_| invalid("The stored goal can't be read."))?;
            let delta = g.to_units(req.delta).map_err(|m| invalid(format!("delta: {m}")))?;
            if delta.unsigned_abs() > MAX_ADJUST.unsigned_abs() {
                return Err(invalid("delta: that change is too large."));
            }
            ferrocast_core::goal::apply(&mut g, delta);
            let j = encode(&g)?;
            out = Some(g);
            Ok(Some(j))
        })
        .map_err(ApiError::db)??;
    match (edited, out) {
        (Some(_), Some(g)) => {
            changed(&st, account);
            Ok(json(json!(g.view())))
        }
        _ => Err(not_found("goal")),
    }
}

/// A live event through the pipeline: every goal it counts toward moves (saved at once, so
/// progress outlives the bus and a relay restart). Test events never count. The amount is
/// worked out from the stored goal inside its write, so an edit in between (a new kind or
/// currency) never gets the old unit's amount.
pub(crate) fn on_event(st: &AppState, bus: &Arc<Bus>, ev: &Event) {
    if ev.origin != Origin::Live {
        return;
    }
    load(st, bus);
    // Only goals the cached copy says could count are written (most events count for none).
    let due: Vec<String> = {
        let s = bus.state();
        s.goals
            .iter()
            .flatten()
            .filter(|g| ferrocast_core::goal::contribution(g, ev) != 0)
            .map(|g| g.id.clone())
            .collect()
    };
    if due.is_empty() {
        return;
    }
    let mut moved = Vec::new();
    for id in due {
        let r = st.db.cloud_doc_edit(&bus.account, GOAL, &id, now_ms(), |old| {
            let mut g: Goal = serde_json::from_str(old).map_err(|_| ())?;
            let delta = ferrocast_core::goal::contribution(&g, ev);
            if delta == 0 {
                return Ok(None);
            }
            ferrocast_core::goal::apply(&mut g, delta);
            serde_json::to_string(&g).map(Some).map_err(|_| ())
        });
        match r {
            Ok(Ok(Some((_, json)))) => {
                if let Ok(g) = serde_json::from_str::<Goal>(&json) {
                    moved.push(g);
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "couldn't count an event toward a cloud goal"),
        }
    }
    if moved.is_empty() {
        return;
    }
    // The bus's copy from what was written (no second read).
    let v = {
        let mut s = bus.state();
        let goals = s.goals.get_or_insert_with(Vec::new);
        for g in moved {
            if let Some(slot) = goals.iter_mut().find(|x| x.id == g.id) {
                *slot = g;
            }
        }
        views(goals)
    };
    push(bus, &v);
}

/// How many goals an account has (the Cloud page's usage).
pub(crate) fn count(st: &AppState, account: &str) -> Result<i64, ApiError> {
    st.db.cloud_doc_count(account, GOAL).map_err(ApiError::db)
}
