//! The phone's dashboard while the streaming PC is off (Pro F2's offline part, milestone
//! M10; API.md §9.5). Instead of 503 `pc_offline` for everything, the relay answers a small
//! set itself:
//!
//! - `GET /chat` from the cloud bot's in-memory ring (the last 200 lines per channel, never
//!   stored, R-51), as the same `Event`s the PC's chat sends;
//! - `POST /mod/delete|timeout|ban` with the cloud bot's sign-in (needs the moderation
//!   opt-in: 403 `needs_moderation_scope` without it), in the channel's 20 a minute; bans and
//!   timeouts are queued for the PC (link item `cloud_moderation`), which records them in its
//!   ban list and viewer cards when it's back, without asking the platform again;
//! - `GET /pro/history` from the stored stream history (F7), owners only;
//! - `PUT /pro/bot/enabled`, the cloud bot's switch (owners only).
//!
//! Chat and moderation need the cloud bot switched on with a ready channel. Everything else
//! is 503 `pc_offline` with what still works for this phone ([`offline_message`]: by role
//! and whether the cloud bot can moderate) when the cloud bot stands in, the plain "Your
//! streaming PC is off." otherwise. The role was checked before this (the same table as
//! online), and changes are audited with source `cloud`.
//!
//! The live socket: the relay's `remote` frame says `{pc_online: false, cloud}`, and the
//! cloud's chat lines go to the phones as the same `event` frames the PC sends (mods get only
//! what `MOD_FRAMES` allows). When the PC's tunnel comes back the PC takes over the socket
//! with `hello` and `resync`, so the phone refetches from the PC.

use std::sync::Arc;

use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrocast_core::event::{Event, Platform};
use serde_json::{Value, json};

use super::proxy;
use super::scope::Role;
use super::store::Device;
use crate::billing::gate::ProCtx;
use crate::bot::CloudState;
use crate::bot::moderate::{self, Action, MAX_TIMEOUT_S};
use crate::{ApiError, AppState, now_ms};

/// What a phone hears for anything else while the cloud bot stands in: what still works for
/// its role (owners also have history) and whether the cloud bot can moderate anywhere. The
/// dashboard's banner says the same (`packages/api` `remoteOfflineMessage`).
pub(crate) fn offline_message(role: Role, cloud: CloudState) -> &'static str {
    match (role, cloud.moderation.any()) {
        (Role::Owner, true) => "Your PC is off. Chat, bans and history still work.",
        (Role::Owner, false) => "Your PC is off. Chat and history still work.",
        (Role::Mod, true) => "Your PC is off. Chat and bans still work.",
        (Role::Mod, false) => "Your PC is off. Chat still works.",
    }
}
/// Chat lines a phone may ask for at once (the ring keeps 200 per channel).
const MAX_CHAT: usize = 500;
const DEFAULT_CHAT: usize = 200;
/// The longest moderation reason passed on (Twitch takes 500 characters).
const MAX_REASON: usize = 500;
/// A validation message names the field and stays short (never the parser's own words).
const MAX_INVALID: usize = 200;
/// A viewer's name as passed on to the PC's ban list.
const MAX_NAME: usize = 64;

/// 503 `pc_offline` for a route the relay can't answer itself.
pub(crate) fn pc_offline(role: Role, cloud: CloudState) -> ApiError {
    if cloud.on {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "pc_offline", offline_message(role, cloud))
            .hint("Settings and everything else need your PC. Turn it on and open Ferrocast.")
    } else {
        proxy::pc_offline()
    }
}

/// 400 `invalid` naming the field (at most [`MAX_INVALID`] characters).
fn invalid(m: impl Into<String>) -> ApiError {
    let m: String = m.into();
    let m = if m.chars().count() > MAX_INVALID { m.chars().take(MAX_INVALID).collect() } else { m };
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

/// Answers a phone's request while its PC has no tunnel. `true` with the answer when the
/// relay handled it (changes are then audited as `cloud`), `false` for a 503 `pc_offline`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn answer(
    st: &AppState,
    device: &Device,
    role: Role,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
) -> (Result<Response, ApiError>, bool) {
    let cloud = crate::bot::cloud_state(st, &device.sub_id);
    let ctx = ProCtx { install: device.install_id.clone(), sub_id: device.sub_id.clone() };
    // The role was checked already (the same table as online); owners only here anyway.
    let owner = role == Role::Owner;
    match (method.as_str(), path) {
        ("GET", "/pro/history") if owner => {
            (crate::history::phone_list(st, &ctx.sub_id, query).map(|v| axum::Json(v).into_response()), true)
        }
        ("PUT", "/pro/bot/enabled") if owner && crate::bot::available(st) => {
            let res = crate::bot::phone_set_enabled(st, &ctx, headers, body).await.map(|mut v| {
                // The PC's answer adds these (API.md §9.7): what this relay can say about them.
                let version = v["version"].as_i64().unwrap_or(0);
                v["pro_available"] = json!(true);
                v["sync"] = json!({
                    "state": if version > 0 { "synced" } else { "off" },
                    "version": version, "synced_at": v["config_updated_at"], "error": null,
                });
                axum::Json(v).into_response()
            });
            (res, true)
        }
        ("GET", "/chat") if cloud.on => (Ok(chat(st, &ctx.sub_id, query)), true),
        ("POST", "/mod/delete" | "/mod/timeout" | "/mod/ban") if cloud.on => {
            (moderation(st, device, role, path, headers, body).await, true)
        }
        _ => (Err(pc_offline(role, cloud)), false),
    }
}

/// `GET /chat?limit=` → the cloud's last lines for every channel, oldest first.
fn chat(st: &AppState, sub: &str, query: Option<&str>) -> Response {
    let limit = url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .find(|(k, _)| k == "limit")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_CHAT)
        .clamp(1, MAX_CHAT);
    let events = crate::bot::recent_events(st, sub, limit);
    let list: Vec<&Event> = events.iter().map(AsRef::as_ref).collect();
    axum::Json(json!(list)).into_response()
}

fn platform(raw: &str) -> Result<Platform, ApiError> {
    match raw {
        "twitch" => Ok(Platform::Twitch),
        "kick" => Ok(Platform::Kick),
        "youtube" => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "cloud_platform",
            "The cloud bot doesn't moderate YouTube yet.",
        )
        .hint("Do this once your PC is on.")),
        _ => Err(invalid("platform: twitch or kick.")),
    }
}

/// A platform id (a user or a message): letters, digits, `-` and `_`, at most `max`.
fn id_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// A reason as passed on: control and direction characters out, at most 500 characters.
fn reason(r: Option<&str>) -> Option<String> {
    let cleaned: String = r?
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_REASON)
        .collect();
    let t = cleaned.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The body as a JSON object (415 / 400 otherwise).
fn object(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    let v = super::json_body(headers, body)?;
    if v.is_object() { Ok(v) } else { Err(invalid("The body must be a JSON object.")) }
}

/// A required text field (`name` is how the message calls it, e.g. `user.user_id`).
fn text<'a>(o: &'a Value, key: &str, name: &str) -> Result<&'a str, ApiError> {
    match o.get(key) {
        Some(Value::String(s)) => Ok(s),
        None | Some(Value::Null) => Err(invalid(format!("{name}: missing."))),
        Some(_) => Err(invalid(format!("{name}: must be text."))),
    }
}

/// An optional text field (missing or `null`: `None`).
fn text_opt<'a>(o: &'a Value, key: &str, name: &str) -> Result<Option<&'a str>, ApiError> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(invalid(format!("{name}: must be text."))),
    }
}

/// A viewer's login or display name for the PC's ban list: cleaned like a reason, 64 at most.
fn name(r: Option<&str>) -> Option<String> {
    reason(r).map(|n| n.chars().take(MAX_NAME).collect())
}

/// `POST /mod/delete|timeout|ban`, the PC's own bodies (API.md §4), by the cloud bot. Bans
/// and timeouts that went through are queued for the PC ([`queue_for_pc`]).
async fn moderation(
    st: &AppState,
    device: &Device,
    role: Role,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, ApiError> {
    let sub = device.sub_id.as_str();
    let b = object(headers, body)?;
    match path {
        "/mod/delete" => {
            let p = platform(text(&b, "platform", "platform")?)?;
            let message_id = text(&b, "message_id", "message_id")?;
            if message_id.starts_with("cl-") || message_id.starts_with("cl_") {
                // Ferrocast's own id for a cloud line, not the platform's (`source_id`).
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_message",
                    "That's Ferrocast's id for the line, not the platform's.",
                )
                .hint("Send the message's source_id. A line without one can't be deleted."));
            }
            if !id_ok(message_id, 128) {
                return Err(invalid("message_id: the platform's message id."));
            }
            moderate::run(st, sub, p, Action::Delete { message_id }).await?;
            // The phones grey the line out, as when the PC passes the platform's deletion on.
            deleted(st, sub, p, message_id);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        "/mod/timeout" => {
            let p = platform(text(&b, "platform", "platform")?)?;
            let user_id = text(&b, "user_id", "user_id")?;
            if !id_ok(user_id, 64) {
                return Err(invalid("user_id: the platform's user id."));
            }
            let seconds = b
                .get("seconds")
                .and_then(Value::as_i64)
                .and_then(|s| u32::try_from(s).ok())
                .filter(|s| (1..=MAX_TIMEOUT_S).contains(s))
                .ok_or_else(|| invalid("seconds: from 1 second to 14 days."))?;
            let r = reason(text_opt(&b, "reason", "reason")?);
            moderate::run(st, sub, p, Action::Timeout { user_id, seconds, reason: r.as_deref() }).await?;
            queue_for_pc(
                st,
                device,
                role,
                Done {
                    platform: p,
                    action: "timeout",
                    user_id,
                    login: None,
                    display_name: None,
                    seconds: Some(seconds),
                    reason: r,
                },
            );
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        _ => {
            let user = match b.get("user") {
                Some(u) if u.is_object() => u,
                None | Some(Value::Null) => return Err(invalid("user: missing.")),
                Some(_) => return Err(invalid("user: must be an object.")),
            };
            let p = platform(text(user, "platform", "user.platform")?)?;
            let user_id = text(user, "user_id", "user.user_id")?;
            if !id_ok(user_id, 64) {
                return Err(invalid("user.user_id: the platform's user id."));
            }
            let login = text_opt(user, "login", "user.login")?.filter(|l| id_ok(l, MAX_NAME));
            let display_name = name(text_opt(user, "display_name", "user.display_name")?);
            let r = reason(text_opt(&b, "reason", "reason")?);
            moderate::run(st, sub, p, Action::Ban { user_id, reason: r.as_deref() }).await?;
            queue_for_pc(
                st,
                device,
                role,
                Done {
                    platform: p,
                    action: "ban",
                    user_id,
                    login: login.map(str::to_string),
                    display_name,
                    seconds: None,
                    reason: r,
                },
            );
            // `everywhere` needs the PC's linked accounts: offline it's this platform only.
            Ok(axum::Json(json!({ "banned": [key(p)], "failed": [] })).into_response())
        }
    }
}

fn key(p: Platform) -> &'static str {
    if p == Platform::Kick { "kick" } else { "twitch" }
}

/// A ban or timeout the cloud bot made for a phone.
struct Done<'a> {
    platform: Platform,
    action: &'static str,
    user_id: &'a str,
    login: Option<String>,
    display_name: Option<String>,
    seconds: Option<u32>,
    reason: Option<String>,
}

/// Queues a ban or timeout for the phone's PC (link item `cloud_moderation`, API.md §9.5):
/// when it's back it adds the ban to its list (offered for Unban as usual) or counts the
/// timeout on the viewer's card, marked as done from a phone while the PC was off, without
/// calling the platform again; the link resends it until the PC acks it. The platform
/// already took it, so a queue failure is only logged.
fn queue_for_pc(st: &AppState, device: &Device, role: Role, d: Done<'_>) {
    let payload = json!({
        "platform": key(d.platform), "action": d.action, "target_id": d.user_id,
        "target_login": d.login, "target_name": d.display_name, "duration": d.seconds,
        "reason": d.reason, "by_device": device.name, "by_role": role.as_str(), "at": now_ms(),
    });
    if let Err(e) = st.enqueue(&device.install_id, "cloud_moderation", &payload) {
        tracing::warn!(error = e.message(), "couldn't queue a phone's moderation for its PC");
    }
}

/// A `message_deleted` event for the phones (the PC isn't there to pass the platform's on).
fn deleted(st: &AppState, sub: &str, platform: Platform, message_id: &str) {
    if !proxy::has_phones(st, sub) {
        return;
    }
    let now = now_ms();
    let p = key(platform);
    let frame: Arc<str> = Arc::from(
        json!({ "v": 1, "type": "event", "data": {
            "id": format!("cl_{}", crate::random(12)), "seq": 0, "platform": p, "channel": null,
            "source_id": null, "occurred_at": now, "received_at": now, "origin": "live",
            "kind": { "type": "message_deleted", "message_id": message_id },
        } })
        .to_string(),
    );
    proxy::fan_out_offline(st, sub, |role| for_role(role, &frame));
}

/// The frame if `role` may get it (the same check as the PC's frames).
fn for_role(role: Role, frame: &Arc<str>) -> Vec<Arc<str>> {
    if super::scope::live_frame_allowed(role, frame) { vec![frame.clone()] } else { Vec::new() }
}

/// A chat line the cloud bot kept (`bot::on_chat`): to the phones of `sub` whose PC is off,
/// as the PC's own `event` frame (without the platform's raw payload; `seq` 0, so a phone's
/// resume point stays the PC's).
pub(crate) fn cloud_chat(st: &AppState, sub: &str, e: &Event) {
    if !proxy::has_phones(st, sub) {
        return;
    }
    let Ok(mut data) = serde_json::to_value(e) else { return };
    if let Some(o) = data.as_object_mut() {
        o.remove("raw");
        o.insert("seq".into(), json!(0));
    }
    let frame: Arc<str> = Arc::from(json!({ "v": 1, "type": "event", "data": data }).to_string());
    proxy::fan_out_offline(st, sub, |role| for_role(role, &frame));
}

/// The cloud bot was switched on or off: phones of a PC that's off hear whether chat and
/// bans still work (a `remote` frame; with the cloud on, a `resync` to fetch its chat).
pub(crate) fn cloud_changed(st: &AppState, sub: &str) {
    if !proxy::has_phones(st, sub) {
        return;
    }
    let cloud = crate::bot::cloud_state(st, sub);
    proxy::fan_out_offline(st, sub, |role| {
        let mut f = vec![proxy::remote_frame(st, false, role, cloud)];
        if cloud.on {
            f.push(proxy::resync_frame());
        }
        f
    });
}

#[cfg(test)]
pub(crate) fn tests_reason(r: &str) -> Option<String> {
    reason(Some(r))
}

#[cfg(test)]
pub(crate) fn tests_id_ok(s: &str, max: usize) -> bool {
    id_ok(s, max)
}

#[cfg(test)]
pub(crate) fn tests_platform(raw: &str) -> Result<Platform, u16> {
    platform(raw).map_err(|e| e.status().as_u16())
}
