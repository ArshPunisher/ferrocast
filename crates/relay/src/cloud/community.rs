//! Polls and giveaways in cloud mode (docs/CLOUD.md §7.2): the engine's own rules from core
//! (`poll::PollSpec::cleaned`, `Poll::vote`, `giveaway::GiveawaySpec::cleaned`,
//! `Giveaway::enter`, `pick_weighted` with the OS's randomness), held on the account's bus in
//! memory as the app holds them (a relay restart forgets them, like an app restart). Viewers
//! vote with `!vote 2` and enter with the keyword in live chat from any connected platform;
//! the results go to overlays with a poll or giveaway widget (`state`) and to the dashboard
//! (`poll`, `giveaway` frames), at most every 250 ms while votes pour in. Start, result and
//! winner are posted in chat by the cloud bot where it's on (`bot::announce`).
//!
//! The engine's paths (`/c/api/v1/polls…`, `/c/api/v1/giveaways…`), so the Community screen's
//! panels work unchanged. Points need the app (the viewer database), so a cloud giveaway is
//! free to enter.

use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ferrocast_core::event::{Event, EventKind, Millis, Origin};
use ferrocast_core::giveaway::{Giveaway, GiveawaySpec};
use ferrocast_core::overlay::WidgetKind;
use ferrocast_core::poll::{self, Poll, PollSpec};
use serde_json::{Value, json};

use super::bus::{Audience, Bus, platform_key};
use super::{body, created, e, invalid, json, no_store, web_account};
use crate::{ApiError, AppState, now_ms};

/// Results are pushed at most this often while votes or entries come in.
const FLUSH: Duration = Duration::from_millis(250);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/c/api/v1/polls", post(start_poll))
        .route("/c/api/v1/polls/current", get(current_poll).delete(clear_poll))
        .route("/c/api/v1/polls/current/end", post(end_poll))
        .route("/c/api/v1/giveaways", post(start_giveaway))
        .route("/c/api/v1/giveaways/current", get(current_giveaway).delete(cancel_giveaway))
        .route("/c/api/v1/giveaways/current/close", post(close_giveaway))
        .route("/c/api/v1/giveaways/current/draw", post(draw))
        .route("/c/api/v1/giveaways/current/redraw", post(redraw))
        .route("/c/api/v1/giveaways/current/end", post(end_giveaway))
}

fn no_poll() -> ApiError {
    e(StatusCode::NOT_FOUND, "poll_not_found", "There's no poll running.").hint("Start one from Community, Polls.")
}

fn no_giveaway() -> ApiError {
    e(StatusCode::NOT_FOUND, "giveaway_not_found", "There's no giveaway running.")
        .hint("Start one from Community, Giveaway.")
}

fn conflict(message: &str) -> ApiError {
    e(StatusCode::CONFLICT, "conflict", message)
}

fn bus_of(st: &AppState, account: &str) -> Result<Arc<Bus>, ApiError> {
    st.cloud.buses.open(account, st.cloud.cfg.max_active).map_err(|_| {
        e(StatusCode::SERVICE_UNAVAILABLE, "cloud_busy", "Ferrocast's servers are busy right now.")
            .hint("Try again in a minute.")
    })
}

/// Posts in chat in the background (the dashboard doesn't wait for the platforms).
fn announce(st: &AppState, account: &str, text: String) {
    let (st, account) = (st.clone(), account.to_string());
    tokio::spawn(async move {
        crate::bot::announce(&st, &account, &text).await;
    });
}

fn push_poll(bus: &Bus) {
    let view = bus.state().poll.as_ref().map(Poll::view);
    bus.send_overlay(Audience::Widget(WidgetKind::Poll), "state", &json!({ "poll": view }));
    bus.send_dashboard("poll", &json!({ "poll": view }));
}

fn push_giveaway(bus: &Bus) {
    let (view, card) = {
        let s = bus.state();
        (s.giveaway.as_ref().map(Giveaway::view), s.giveaway.as_ref().map(Giveaway::card))
    };
    bus.send_overlay(Audience::Widget(WidgetKind::Giveaway), "state", &json!({ "giveaway": card }));
    bus.send_dashboard("giveaway", &json!({ "giveaway": view }));
}

/// Votes or entries came in: one push within 250 ms, however many arrive.
fn soon(bus: &Arc<Bus>) {
    {
        let mut s = bus.state();
        s.community_dirty = true;
        if s.community_flush {
            return;
        }
        s.community_flush = true;
    }
    let w = Arc::downgrade(bus);
    tokio::spawn(async move {
        tokio::time::sleep(FLUSH).await;
        let Some(bus) = w.upgrade() else { return };
        {
            let mut s = bus.state();
            s.community_flush = false;
            if !std::mem::take(&mut s.community_dirty) {
                return;
            }
        }
        push_poll(&bus);
        push_giveaway(&bus);
    });
}

/// A live chat line on the bus: a vote (`!vote 2`) or a giveaway entry. One per person and
/// platform account; Shared Chat from a partner channel and tests never count.
pub(crate) fn on_event(_st: &AppState, bus: &Arc<Bus>, ev: &Event) {
    let EventKind::ChatMessage { actor, content, .. } = &ev.kind else { return };
    if ev.origin != Origin::Live || ev.channel.is_some() {
        return;
    }
    let Some(uid) = actor.id.as_deref() else { return };
    let person = format!("{}:{uid}", platform_key(ev.platform));
    let now = Millis(now_ms());
    let mut moved = false;
    {
        let mut s = bus.state();
        if let Some(choice) = poll::parse_vote(&content.text)
            && let Some(p) = s.poll.as_mut()
            && matches!(p.vote(&person, choice, now), Ok(poll::Voted::New | poll::Voted::Changed))
        {
            moved = true;
        }
        if let Some(g) = s.giveaway.as_mut()
            && g.is_keyword(&content.text)
            && g.enter(&person, actor, 0).is_ok()
        {
            moved = true;
        }
    }
    if moved {
        soon(bus);
    }
}

// ── Polls ────────────────────────────────────────────────────────────────────────────

/// `GET /c/api/v1/polls/current` → `PollView | null`.
async fn current_poll(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let view = st.cloud.buses.get(&a.account.id).and_then(|b| b.state().poll.as_ref().map(Poll::view));
    Ok(json(json!(view)))
}

/// The engine's body, read by hand so each wrong type names its field.
fn poll_spec(b: &Value) -> Result<PollSpec, ApiError> {
    let question = b["question"].as_str().ok_or_else(|| invalid("question: must be text."))?.to_string();
    let options = match &b["options"] {
        Value::Array(list) => list
            .iter()
            .map(|o| o.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("options: every answer must be text."))?,
        _ => return Err(invalid("options: must be a list of 2 to 5 answers.")),
    };
    let duration_s = match &b["duration_s"] {
        Value::Null => 60,
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("duration_s: polls run for 10 seconds to 30 minutes."))?,
        _ => return Err(invalid("duration_s: must be a whole number of seconds.")),
    };
    PollSpec { question, options, duration_s }.cleaned().map_err(invalid)
}

/// `POST /c/api/v1/polls` → 201 `PollView`, announced in chat. 409 while another is open.
async fn start_poll(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let spec = poll_spec(&body(&headers, &b)?)?;
    let bus = bus_of(&st, &a.account.id)?;
    let now = Millis(now_ms());
    let poll = {
        let mut s = bus.state();
        if s.poll.as_ref().is_some_and(|p| p.is_open(now)) {
            return Err(conflict("A poll is already running. End it before starting another."));
        }
        let p = Poll::start(super::docs::new_id(), spec, now);
        s.poll = Some(p.clone());
        p
    };
    push_poll(&bus);
    announce(&st, &a.account.id, poll.announcement());
    // Closes on time, unless it was ended or replaced first.
    let (w, st2, account, id) = (Arc::downgrade(&bus), st.clone(), a.account.id.clone(), poll.id.clone());
    let wait = u64::try_from((poll.ends_at.0 - now_ms()).max(0)).unwrap_or(0);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(wait)).await;
        let _ = finish_poll(&st2, &account, &w, Some(&id));
    });
    Ok(created(json!(poll.view())))
}

/// Closes the poll (only `id`, when given) once and posts the result.
fn finish_poll(st: &AppState, account: &str, bus: &Weak<Bus>, id: Option<&str>) -> Result<Value, ApiError> {
    let bus = bus.upgrade().ok_or_else(no_poll)?;
    let poll = {
        let mut s = bus.state();
        let p = s.poll.as_mut().ok_or_else(no_poll)?;
        if id.is_some_and(|id| id != p.id) {
            return Err(no_poll());
        }
        if !p.close() {
            return Err(conflict("That poll already ended."));
        }
        p.clone()
    };
    push_poll(&bus);
    announce(st, account, poll.result());
    Ok(json!(poll.view()))
}

/// `POST /c/api/v1/polls/current/end` → `PollView`.
async fn end_poll(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let bus = st.cloud.buses.get(&a.account.id).ok_or_else(no_poll)?;
    Ok(json(finish_poll(&st, &a.account.id, &Arc::downgrade(&bus), None)?))
}

/// `DELETE /c/api/v1/polls/current` → 204, nothing posted.
async fn clear_poll(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let bus = st.cloud.buses.get(&a.account.id).ok_or_else(no_poll)?;
    if bus.state().poll.take().is_none() {
        return Err(no_poll());
    }
    push_poll(&bus);
    Ok(no_store(StatusCode::NO_CONTENT.into_response()))
}

// ── Giveaways ────────────────────────────────────────────────────────────────────────

/// `GET /c/api/v1/giveaways/current` → `GiveawayView | null`.
async fn current_giveaway(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let view = st.cloud.buses.get(&a.account.id).and_then(|b| b.state().giveaway.as_ref().map(Giveaway::view));
    Ok(json(json!(view)))
}

fn giveaway_spec(b: &Value) -> Result<GiveawaySpec, ApiError> {
    let prize = b["prize"].as_str().ok_or_else(|| invalid("prize: must be text."))?.to_string();
    let keyword = match &b["keyword"] {
        Value::Null => "!join".to_string(),
        Value::String(s) => s.clone(),
        _ => return Err(invalid("keyword: must be text, like !join.")),
    };
    let cost = match &b["cost"] {
        Value::Null => 0,
        Value::Number(n) => n.as_i64().ok_or_else(|| invalid("cost: must be a whole number of points."))?,
        _ => return Err(invalid("cost: must be a whole number of points.")),
    };
    let subscriber_bonus = match &b["subscriber_bonus"] {
        Value::Null => false,
        Value::Bool(v) => *v,
        _ => return Err(invalid("subscriber_bonus: must be true or false.")),
    };
    let spec = GiveawaySpec { prize, keyword, cost, subscriber_bonus }.cleaned().map_err(invalid)?;
    if spec.cost != 0 {
        return Err(invalid("cost: points need the Ferrocast app on your PC, so a cloud giveaway is free to enter.")
            .hint("Set the cost to 0."));
    }
    Ok(spec)
}

/// `POST /c/api/v1/giveaways` → 201 `GiveawayView`, announced. 409 while another hasn't ended.
async fn start_giveaway(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    b: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let spec = giveaway_spec(&body(&headers, &b)?)?;
    let bus = bus_of(&st, &a.account.id)?;
    let g = {
        let mut s = bus.state();
        if s.giveaway.as_ref().is_some_and(|g| !g.ended) {
            return Err(conflict("A giveaway is already running. End it before starting another."));
        }
        let g = Giveaway::start(super::docs::new_id(), spec, Millis(now_ms()));
        s.giveaway = Some(g.clone());
        g
    };
    push_giveaway(&bus);
    announce(&st, &a.account.id, g.announcement());
    Ok(created(json!(g.view())))
}

/// Runs `f` on the giveaway, then pushes it: its answer.
fn with_giveaway(
    st: &AppState,
    account: &str,
    f: impl FnOnce(&mut Giveaway) -> Result<Option<String>, ApiError>,
) -> Result<Response, ApiError> {
    let bus = st.cloud.buses.get(account).ok_or_else(no_giveaway)?;
    let (view, said) = {
        let mut s = bus.state();
        let g = s.giveaway.as_mut().ok_or_else(no_giveaway)?;
        let said = f(g)?;
        (g.view(), said)
    };
    push_giveaway(&bus);
    if let Some(text) = said {
        announce(st, account, text);
    }
    Ok(json(json!(view)))
}

fn os_random() -> Result<u64, ApiError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|_| {
        e(StatusCode::SERVICE_UNAVAILABLE, "random_unavailable", "The server's random number source isn't working.")
            .hint("No winner was drawn. Try again in a minute.")
    })?;
    Ok(u64::from_le_bytes(b))
}

fn draw_one(g: &mut Giveaway, redraw: bool) -> Result<Option<String>, ApiError> {
    if g.ended {
        return Err(conflict("This giveaway has ended."));
    }
    match (redraw, g.winners.is_empty()) {
        (false, false) => return Err(conflict("A winner was already drawn. Use Redraw to pick another.")),
        (true, true) => return Err(conflict("Draw a winner first.")),
        _ => {}
    }
    if g.entrants.is_empty() {
        return Err(e(StatusCode::UNPROCESSABLE_ENTITY, "nobody_entered", "Nobody has entered yet.")
            .hint(format!("Viewers enter by typing {} in chat.", g.keyword)));
    }
    let Some(w) = g.draw(os_random)? else {
        return Err(e(StatusCode::UNPROCESSABLE_ENTITY, "everyone_won", "Everyone who entered has already won."));
    };
    Ok(Some(g.winner_message(&w)))
}

/// `POST /c/api/v1/giveaways/current/close` → stop taking entries (409 when closed).
async fn close_giveaway(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    with_giveaway(&st, &a.account.id, |g| {
        if g.ended || !g.open {
            return Err(conflict("Entries are already closed."));
        }
        g.open = false;
        Ok(None)
    })
}

/// `POST /c/api/v1/giveaways/current/draw` → the first winner, announced.
async fn draw(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    with_giveaway(&st, &a.account.id, |g| draw_one(g, false))
}

/// `POST /c/api/v1/giveaways/current/redraw` → another winner, never one who won.
async fn redraw(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    with_giveaway(&st, &a.account.id, |g| draw_one(g, true))
}

/// `POST /c/api/v1/giveaways/current/end` → finished (409 when it already ended).
async fn end_giveaway(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    with_giveaway(&st, &a.account.id, |g| {
        if g.ended {
            return Err(conflict("This giveaway has already ended."));
        }
        g.ended = true;
        g.open = false;
        Ok(None)
    })
}

/// `DELETE /c/api/v1/giveaways/current` → `{ refunded: 0 }` (cloud giveaways cost nothing).
async fn cancel_giveaway(State(st): State<AppState>, method: Method, headers: HeaderMap) -> Result<Response, ApiError> {
    let a = web_account(&st, &headers, &method, true)?;
    let bus = st.cloud.buses.get(&a.account.id).ok_or_else(no_giveaway)?;
    if bus.state().giveaway.take().is_none() {
        return Err(no_giveaway());
    }
    push_giveaway(&bus);
    Ok(json(json!({ "refunded": 0 })))
}
