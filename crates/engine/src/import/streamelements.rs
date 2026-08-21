//! StreamElements: read with the streamer's JWT (GET requests only, nothing on their
//! account changes) and mapped to Ferrocast's commands, timers, points and tip history.
//!
//! Endpoints (API base `https://api.streamelements.com/kappa/v2`): `GET /channels/me`,
//! `/bot/commands/{channel}`, `/bot/timers/{channel}`, `/loyalty/{channel}`,
//! `/points/{channel}/top` (1,000 a page), `/tips/{channel}` (100 a page),
//! `/overlays/{channel}`, `/store/{channel}/items` and `/bot/modules/{channel}` (counted for
//! the "not imported" list). Responses are read leniently: one odd entry is skipped with a
//! reason, it never fails the whole import.

use std::time::Duration;

use ferrocast_core::command::{Access, Command, Timer};
use ferrocast_core::event::Platform;
use ferrocast_core::loyalty::{MAX_AWARD, MAX_BALANCE};
use ferrocast_store::import::{Incoming, Kind, NotImported, PointsRow, Source, TipRow};
use serde_json::{Map, Value, json};

use super::text::{command_name, parse_datetime, translate_se, usable_command_name};
use super::{ImportAccount, refused};
use crate::EngineError;

pub const DEFAULT_API: &str = "https://api.streamelements.com/kappa/v2";
/// Most balances and tips read from one account (far beyond a real channel).
pub const MAX_POINTS: usize = 200_000;
pub const MAX_TIPS: usize = 20_000;
const POINTS_PAGE: usize = 1_000;
const TIPS_PAGE: usize = 100;
/// A short pause between pages, so a big channel never bursts their API.
const PAGE_GAP: Duration = Duration::from_millis(120);
const WHERE_TO_FIND: &str =
    "Copy a fresh JWT from StreamElements → Account → Channels → Show secrets (JWT Token), then paste it here.";

/// Checks the pasted token looks like a JWT (three dot-separated parts) before it leaves
/// this computer. Accepts it with or without "Bearer ".
pub fn check_token(raw: &str) -> Result<String, EngineError> {
    let t = raw.trim();
    let t = t.strip_prefix("Bearer ").or_else(|| t.strip_prefix("bearer ")).unwrap_or(t).trim();
    let parts: Vec<&str> = t.split('.').collect();
    let ok = (20..=8_192).contains(&t.len())
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    if !ok {
        return Err(EngineError::Invalid(format!(
            "token: that doesn't look like a StreamElements JWT (it's long, with two dots in it). {WHERE_TO_FIND}"
        )));
    }
    Ok(t.to_string())
}

/// A GET that failed in a way the streamer should hear about.
fn upstream(status: u16, retry_after: Option<u64>) -> EngineError {
    match status {
        401 | 403 => refused(
            401,
            "streamelements_token_rejected",
            "StreamElements didn't accept that token. It may have expired, or it isn't the JWT token.",
            WHERE_TO_FIND,
        ),
        429 => {
            let secs = retry_after.unwrap_or(30).clamp(1, 3_600);
            EngineError::Refused {
                status: 429,
                code: "streamelements_rate_limited",
                message: format!("StreamElements is limiting requests right now. Try again in {secs} seconds."),
                hint: "Nothing was imported. Wait a moment, then run the preview again.".into(),
                retry_after: Some(secs),
            }
        }
        s if s >= 500 => refused(
            502,
            "streamelements_unavailable",
            format!("StreamElements had a problem (HTTP {s}). Nothing was imported."),
            "Try again in a few minutes. If it keeps happening, check status.streamelements.com.",
        ),
        s => refused(
            502,
            "streamelements_error",
            format!("StreamElements refused the request (HTTP {s}). Nothing was imported."),
            "Try again. If it keeps happening, copy a fresh JWT and try once more.",
        ),
    }
}

fn unreachable() -> EngineError {
    refused(
        502,
        "streamelements_unreachable",
        "Couldn't reach StreamElements. Nothing was imported.",
        "Check your internet connection, then try again.",
    )
}

fn unreadable(what: &str) -> EngineError {
    refused(
        502,
        "streamelements_bad_response",
        format!("StreamElements sent {what} in a shape Ferrocast can't read. Nothing was imported."),
        "Try again in a few minutes. If it keeps happening, use Report a problem in Settings.",
    )
}

/// Seconds to wait, from `Retry-After` or StreamElements' `x-ratelimit-reset` (epoch ms).
fn wait_hint(h: &reqwest::header::HeaderMap) -> Option<u64> {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(s) = get("retry-after").and_then(|v| v.parse::<f64>().ok()) {
        return Some(s.ceil().max(1.0) as u64);
    }
    let reset = get("x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok())?;
    let now = crate::now().0;
    let ms = if reset > 10_000_000_000 { reset - now } else { reset * 1000 - now };
    Some((ms.max(1000) / 1000) as u64)
}

pub(crate) struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Api").field("base", &self.base).finish_non_exhaustive()
    }
}

impl Api {
    pub(crate) fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("Ferrocast/", env!("CARGO_PKG_VERSION"), " (importer)"))
            .build()
            .unwrap_or_default();
        Api { http, base: base.trim_end_matches('/').to_string(), token: token.to_string() }
    }

    /// GET `path`; `Ok(None)` on 404 (StreamElements answers 404 for things never set up).
    async fn get(&self, path: &str, what: &str) -> Result<Option<Value>, EngineError> {
        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                // The URL holds no secret, but keep logs short and token-free anyway.
                tracing::warn!(error = %e.without_url(), what, "couldn't reach StreamElements");
                unreachable()
            })?;
        let status = res.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            tracing::info!(status, what, "StreamElements refused an import request");
            return Err(upstream(status, wait_hint(res.headers())));
        }
        // Bound what we read, whatever the server says.
        let bytes = res.bytes().await.map_err(|_| unreachable())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(unreadable(what));
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|_| unreadable(what))
    }
}

/// Reads everything importable from the channel behind `token`.
pub async fn fetch(base: &str, token: &str) -> Result<(Incoming, ImportAccount), EngineError> {
    let api = Api::new(base, token);
    let me = api.get("/channels/me", "your channel").await?.ok_or_else(|| upstream(401, None))?;
    let channel =
        me.get("_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| unreadable("your channel"))?;
    let channel = channel.to_string();
    if channel.len() > 64 || !channel.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(unreadable("your channel"));
    }
    let platform = match me.get("provider").and_then(Value::as_str) {
        Some("twitch") => Some(Platform::Twitch),
        Some("youtube") => Some(Platform::Youtube),
        Some("kick") => Some(Platform::Kick),
        _ => None,
    };
    let name = ["displayName", "username"]
        .iter()
        .find_map(|k| me.get(*k).and_then(Value::as_str).filter(|s| !s.trim().is_empty()))
        .unwrap_or("your channel")
        .to_string();
    let provider = me.get("provider").and_then(Value::as_str).unwrap_or("").to_string();

    let commands = api.get(&format!("/bot/commands/{channel}"), "your commands").await?;
    let timers = api.get(&format!("/bot/timers/{channel}"), "your timers").await?;
    let loyalty = api.get(&format!("/loyalty/{channel}"), "your points settings").await?;
    let mut points = Vec::new();
    let mut total = None;
    while points.len() < MAX_POINTS {
        let page = api
            .get(&format!("/points/{channel}/top?limit={POINTS_PAGE}&offset={}", points.len()), "your points")
            .await?;
        let Some(page) = page else { break };
        total = total.or_else(|| page.get("_total").and_then(Value::as_u64));
        let users = page.get("users").and_then(Value::as_array).ok_or_else(|| unreadable("your points"))?;
        let n = users.len();
        points.extend(users.iter().cloned());
        if n < POINTS_PAGE || total.is_some_and(|t| points.len() as u64 >= t) {
            break;
        }
        tokio::time::sleep(PAGE_GAP).await;
    }
    let mut tips = Vec::new();
    while tips.len() < MAX_TIPS {
        let page = api
            .get(&format!("/tips/{channel}?limit={TIPS_PAGE}&offset={}&sort=createdAt", tips.len()), "your tips")
            .await?;
        let Some(page) = page else { break };
        let docs = page.get("docs").and_then(Value::as_array).ok_or_else(|| unreadable("your tips"))?;
        let n = docs.len();
        tips.extend(docs.iter().cloned());
        let total = page.get("total").and_then(Value::as_u64);
        if n < TIPS_PAGE || total.is_some_and(|t| tips.len() as u64 >= t) {
            break;
        }
        tokio::time::sleep(PAGE_GAP).await;
    }
    // Only counted, for the "not imported" list: a failure here never stops the import.
    let overlays = api.get(&format!("/overlays/{channel}"), "your overlays").await.ok().flatten();
    let store_items = api.get(&format!("/store/{channel}/items"), "your store").await.ok().flatten();
    let modules = api.get(&format!("/bot/modules/{channel}"), "your bot modules").await.ok().flatten();

    let raw = Raw {
        platform,
        provider,
        commands,
        timers,
        loyalty,
        points,
        points_capped: total.is_some_and(|t| t as usize > MAX_POINTS),
        tips,
        overlays,
        store_items,
        modules,
    };
    Ok((map(raw), ImportAccount { name, platform }))
}

/// Everything read from StreamElements, before mapping (tests build this from fixtures).
#[derive(Debug, Default)]
pub struct Raw {
    pub platform: Option<Platform>,
    pub provider: String,
    pub commands: Option<Value>,
    pub timers: Option<Value>,
    pub loyalty: Option<Value>,
    pub points: Vec<Value>,
    pub points_capped: bool,
    pub tips: Vec<Value>,
    pub overlays: Option<Value>,
    pub store_items: Option<Value>,
    pub modules: Option<Value>,
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn secs(v: Option<f64>) -> u32 {
    v.map(|n| n.clamp(0.0, 86_400.0) as u32).unwrap_or(0)
}

/// Maps what was read to Ferrocast's shapes. Pure, so fixtures test it.
pub fn map(raw: Raw) -> Incoming {
    let mut inc = Incoming::new(Source::StreamElements, raw.platform);
    inc.kinds = vec![Kind::Commands, Kind::Timers, Kind::Points, Kind::LoyaltySettings, Kind::Tips];
    map_commands(&mut inc, raw.commands.as_ref());
    map_timers(&mut inc, raw.timers.as_ref());
    map_points(&mut inc, &raw);
    map_loyalty(&mut inc, raw.loyalty.as_ref());
    map_tips(&mut inc, &raw.tips);
    not_imported(&mut inc, &raw);
    inc
}

fn map_commands(inc: &mut Incoming, list: Option<&Value>) {
    let Some(list) = list.and_then(Value::as_array) else { return };
    for (i, c) in list.iter().enumerate() {
        let raw_name = c.get("command").and_then(Value::as_str).unwrap_or("").trim();
        if raw_name.is_empty() {
            inc.skip(Kind::Commands, format!("Command {}", i + 1), "It has no name.");
            continue;
        }
        let name = command_name(raw_name);
        if !usable_command_name(&name) {
            inc.skip(
                Kind::Commands,
                &name,
                "Ferrocast command names are one word of letters, numbers, - and _ (up to 32).",
            );
            continue;
        }
        let Some(reply) = c.get("reply").and_then(Value::as_str).filter(|r| !r.trim().is_empty()) else {
            inc.skip(Kind::Commands, &name, "It has no reply.");
            continue;
        };
        let (response, unknown) = translate_se(reply);
        let mut aliases = Vec::new();
        for a in c.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            let a = command_name(a);
            if usable_command_name(&a) {
                aliases.push(a);
            } else {
                inc.note(Kind::Commands, format!("{name}: the alias {a} isn't a single word, so it was left off."));
            }
        }
        let level = c.get("accessLevel").and_then(Value::as_i64).unwrap_or(100);
        let access = match level {
            ..=249 => Access::Everyone,
            250..=299 => Access::Subs,
            300..=499 => Access::Vips,
            _ => Access::Mods,
        };
        if (300..400).contains(&level) {
            inc.note(Kind::Commands, "Regulars-only commands became VIP-only: Ferrocast has no regulars list.");
        }
        if level >= 1000 {
            inc.note(Kind::Commands, "Super moderator and broadcaster-only commands became mods-only.");
        }
        let cooldown = c.get("cooldown");
        let mut enabled = c.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && (c.get("enabledOnline").and_then(Value::as_bool).unwrap_or(true)
                || c.get("enabledOffline").and_then(Value::as_bool).unwrap_or(true));
        if !unknown.is_empty() {
            enabled = false;
            inc.note(
                Kind::Commands,
                format!(
                    "{name} is turned off: it uses {} which Ferrocast doesn't have. Check its reply, then turn it on.",
                    unknown.iter().map(|u| format!("${{{u}}}")).collect::<Vec<_>>().join(", ")
                ),
            );
        }
        if c.get("type").and_then(Value::as_str) == Some("whisper") {
            inc.note(Kind::Commands, format!("{name} whispered on StreamElements; in Ferrocast it replies in chat."));
        }
        if num(c, "cost").is_some_and(|n| n > 0.0) {
            inc.note(
                Kind::Commands,
                format!("{name} cost points on StreamElements; Ferrocast commands are free to use."),
            );
        }
        let has_keywords = c.get("keywords").and_then(Value::as_array).is_some_and(|k| !k.is_empty())
            || c.get("regex").and_then(Value::as_str).is_some_and(|r| !r.is_empty());
        if has_keywords {
            inc.note(
                Kind::Commands,
                format!("{name}: keyword and regex triggers aren't supported; it answers to {name} only."),
            );
        }
        inc.commands.push(Command {
            id: String::new(),
            name,
            aliases,
            response,
            access,
            cooldown_s: secs(cooldown.and_then(|cd| num(cd, "global"))),
            user_cooldown_s: secs(cooldown.and_then(|cd| num(cd, "user"))),
            platforms: vec![],
            enabled,
        });
    }
}

fn map_timers(inc: &mut Incoming, list: Option<&Value>) {
    let Some(list) = list.and_then(Value::as_array) else { return };
    for (i, t) in list.iter().enumerate() {
        let name = t
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(60).collect::<String>())
            .unwrap_or_else(|| format!("Timer {}", i + 1));
        let mut messages: Vec<String> = t
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(t.get("message").and_then(Value::as_str))
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect();
        messages.dedup();
        let Some(first) = messages.first() else {
            inc.skip(Kind::Timers, &name, "It has no message.");
            continue;
        };
        if messages.len() > 1 {
            inc.note(
                Kind::Timers,
                format!(
                    "{name}: only the first of its {} messages came over (a Ferrocast timer posts one message).",
                    messages.len()
                ),
            );
        }
        let (message, unknown) = translate_se(first);
        let online = t.get("online");
        let online_on = online.and_then(|o| o.get("enabled")).and_then(Value::as_bool).unwrap_or(true);
        let interval = online
            .filter(|_| online_on)
            .or_else(|| t.get("offline"))
            .and_then(|o| num(o, "interval"))
            .unwrap_or(15.0)
            .clamp(1.0, 1_440.0) as u32;
        let mut enabled = t.get("enabled").and_then(Value::as_bool).unwrap_or(true) && online_on;
        if !online_on {
            inc.note(
                Kind::Timers,
                format!("{name} only ran while offline; Ferrocast timers post while you're live, so it's off."),
            );
        }
        if interval < 5 {
            inc.note(
                Kind::Timers,
                format!("{name} ran every {interval} minutes; Ferrocast posts timers at most every 5."),
            );
        }
        if !unknown.is_empty() {
            enabled = false;
            inc.note(Kind::Timers, format!("{name} is turned off: its message uses variables Ferrocast doesn't have."));
        }
        inc.timers.push(Timer {
            id: String::new(),
            name,
            message,
            every_min: interval.max(5),
            min_chat_lines: num(t, "chatLines").map(|n| n.clamp(0.0, 1_000.0) as u32).unwrap_or(0),
            platforms: vec![],
            enabled,
        });
    }
}

fn map_points(inc: &mut Incoming, raw: &Raw) {
    if raw.platform.is_none() && !raw.points.is_empty() {
        let p = if raw.provider.is_empty() { "a platform".to_string() } else { raw.provider.clone() };
        inc.note(Kind::Points, format!("This StreamElements channel is on {p}, which Ferrocast doesn't connect to, so balances can't be matched to viewers."));
    }
    if raw.platform == Some(Platform::Youtube) && raw.points.iter().any(|u| u.get("providerId").is_none()) {
        inc.note(
            Kind::Points,
            "StreamElements lists YouTube viewers by name only, and YouTube names aren't unique, so these balances need a match: Ferrocast won't pay them automatically.",
        );
    }
    for (i, u) in raw.points.iter().enumerate() {
        let Some(name) = u.get("username").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) else {
            inc.skip(Kind::Points, format!("Entry {}", i + 1), "It has no name.");
            continue;
        };
        let Some(points) = num(u, "points") else {
            inc.skip(Kind::Points, name, "Its balance isn't a number.");
            continue;
        };
        // The platform's own id (YouTube channel id, Twitch user id) when StreamElements sends it.
        let user_id = u
            .get("providerId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 64)
            .map(String::from);
        inc.points.push(PointsRow {
            name: name.to_string(),
            user_id,
            points: points.floor().clamp(0.0, MAX_BALANCE as f64) as i64,
        });
    }
    if raw.points_capped {
        inc.note(Kind::Points, format!("Only the top {MAX_POINTS} balances were read."));
    }
}

fn map_loyalty(inc: &mut Incoming, raw: Option<&Value>) {
    let Some(l) = raw.and_then(|v| v.get("loyalty")) else { return };
    let award = |v: Option<f64>| json!(v.unwrap_or(0.0).clamp(0.0, f64::from(MAX_AWARD)) as u32);
    let bonuses = l.get("bonuses").cloned().unwrap_or(Value::Null);
    let mut patch = Map::new();
    if let Some(e) = l.get("enabled").and_then(Value::as_bool) {
        patch.insert("enabled".into(), json!(e));
    }
    match l.get("name").and_then(Value::as_str).map(str::trim) {
        Some(n)
            if !n.is_empty()
                && n.chars().count() <= 20
                && n.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_') =>
        {
            patch.insert("name".into(), json!(n));
        }
        Some(n) if !n.is_empty() => inc.note(
            Kind::LoyaltySettings,
            format!("The points name \"{n}\" doesn't fit Ferrocast (1–20 letters, numbers, spaces, - and _), so yours stays."),
        ),
        _ => {}
    }
    if let Some(a) = num(l, "amount") {
        patch.insert("per_interval".into(), award(Some(a)));
        patch.insert("interval_min".into(), json!(10));
        inc.note(
            Kind::LoyaltySettings,
            "StreamElements pays every 10 minutes to everyone watching; Ferrocast pays the same amount every 10 minutes to people who chatted.",
        );
    }
    patch.insert("follow_bonus".into(), award(num(&bonuses, "follow")));
    patch.insert("sub_bonus".into(), award(num(&bonuses, "subscriber")));
    patch.insert("tip_bonus".into(), award(num(&bonuses, "tip")));
    patch.insert("raid_bonus".into(), award(num(&bonuses, "host")));
    if num(&bonuses, "cheer").unwrap_or(0.0) != num(&bonuses, "tip").unwrap_or(0.0) {
        inc.note(
            Kind::LoyaltySettings,
            "Ferrocast has one bonus for tips, Bits and Super Chats; it uses your tip bonus.",
        );
    }
    if num(l, "subscriberMultiplier").is_some_and(|m| m > 1.0) {
        inc.note(
            Kind::LoyaltySettings,
            "Ferrocast has no subscriber multiplier; subscribers earn the same as everyone.",
        );
    }
    inc.loyalty = Some(patch);
}

fn map_tips(inc: &mut Incoming, docs: &[Value]) {
    for (i, t) in docs.iter().enumerate() {
        let d = t.get("donation").cloned().unwrap_or(Value::Null);
        let who = d.pointer("/user/username").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let label = who.map(String::from).unwrap_or_else(|| format!("Tip {}", i + 1));
        let Some(id) = t.get("_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 64) else {
            inc.skip(Kind::Tips, label, "It has no id.");
            continue;
        };
        if t.get("deleted").and_then(Value::as_bool) == Some(true) {
            inc.skip(Kind::Tips, label, "You deleted it on StreamElements.");
            continue;
        }
        match t.get("status").and_then(Value::as_str) {
            None | Some("success") => {}
            Some(s) => {
                inc.skip(
                    Kind::Tips,
                    label,
                    format!("Its status on StreamElements is \"{s}\", not a completed payment."),
                );
                continue;
            }
        }
        let currency = d.get("currency").and_then(Value::as_str).unwrap_or("").trim().to_ascii_uppercase();
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            inc.skip(Kind::Tips, label, "It has no currency.");
            continue;
        }
        let Some(amount) = num(&d, "amount").filter(|a| *a >= 0.0 && *a < 1e12) else {
            inc.skip(Kind::Tips, label, "Its amount isn't a number.");
            continue;
        };
        let scale = 10f64.powi(ferrocast_core::money::exponent(&currency) as i32);
        let Some(at) = t.get("createdAt").and_then(Value::as_str).and_then(parse_datetime) else {
            inc.skip(Kind::Tips, label, "Its date can't be read.");
            continue;
        };
        inc.tips.push(TipRow {
            source_id: id.to_string(),
            occurred_at: at,
            name: who.unwrap_or("Anonymous").chars().take(100).collect(),
            amount_minor: (amount * scale).round() as i64,
            currency,
            message: d
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(|m| m.chars().take(1_000).collect()),
            provider: t.get("provider").and_then(Value::as_str).unwrap_or("streamelements").chars().take(30).collect(),
        });
    }
    if docs.len() >= MAX_TIPS {
        inc.note(Kind::Tips, format!("Only the first {MAX_TIPS} tips were read."));
    }
}

fn not_imported(inc: &mut Incoming, raw: &Raw) {
    let count = |v: &Option<Value>| -> usize {
        match v {
            Some(Value::Array(a)) => a.len(),
            Some(Value::Object(o)) => o.get("docs").and_then(Value::as_array).map_or(0, Vec::len),
            _ => 0,
        }
    };
    let overlays = count(&raw.overlays);
    if overlays > 0 {
        inc.not_imported.push(NotImported {
            what: "Overlays and their widgets".into(),
            count: overlays,
            reason: "StreamElements overlays run on StreamElements' own code, including custom HTML and JavaScript widgets, which Ferrocast can't run. Build them again in Overlays; your alert sounds and images can be uploaded there.".into(),
        });
    }
    let items = count(&raw.store_items);
    if items > 0 {
        inc.not_imported.push(NotImported {
            what: "Loyalty store items".into(),
            count: items,
            reason: "Ferrocast has no points store yet. Viewers can spend points on giveaway entries.".into(),
        });
    }
    let modules = raw
        .modules
        .as_ref()
        .and_then(Value::as_object)
        .map_or(0, |m| m.values().filter(|v| v.get("enabled").and_then(Value::as_bool) == Some(true)).count());
    if modules > 0 {
        inc.not_imported.push(NotImported {
            what: "Bot modules (chat alerts, spam filters, games)".into(),
            count: modules,
            reason: "They work differently in Ferrocast. Set up chat filters in Moderation and welcome messages in Community.".into(),
        });
    }
}
