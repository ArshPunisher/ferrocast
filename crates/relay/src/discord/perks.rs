//! Role perks: the rules, the events that trigger them, giving roles and taking them back.
//!
//! The desktop (and later the cloud bot) sends what happened on stream: a sub, a YouTube
//! membership, a tip, each with the platform's user id. A viewer who linked their Discord at
//! `/d/<code>/link` is found by that id, every matching rule gives its role through Discord's
//! REST API (`PUT /guilds/{g}/members/{u}/roles/{r}`), and the grant is kept with its end
//! (`keep_days` after the event). The hourly sweep takes ended roles back.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};

use super::api::{DiscordError, codes};
use super::store::{RuleRow, Ungranted};
use super::{DAY_MS, ROLE_HINT, discord_error, not_connected};
use crate::{ApiError, AppState, now_ms};
use axum::http::StatusCode;

pub const MAX_RULES: i64 = 10;
pub const MAX_EVENTS: usize = 100;
pub const MAX_KEEP_DAYS: i64 = 366;
pub const DEFAULT_KEEP_DAYS: i64 = 31;
/// Events older than this aren't worth a role any more (an offline queue weeks old).
pub const MAX_EVENT_AGE_MS: i64 = 30 * DAY_MS;
/// Grants taken back per sweep, so a backlog can't hammer Discord.
pub const SWEEP_BATCH: i64 = 500;
/// A role that can't be taken back after this many sweeps is forgotten (it stays on).
pub const SWEEP_MAX_ATTEMPTS: i64 = 5;
/// The first retry after a failed removal; it doubles each time, up to a day.
const SWEEP_RETRY_MS: i64 = 3_600_000;
const SWEEP_RETRY_MAX_MS: i64 = DAY_MS;

pub const KINDS: [&str; 4] = ["sub", "sub_tier", "member", "tip_min"];
pub const PLATFORMS: [&str; 3] = ["twitch", "youtube", "kick"];

/// A rule as the dashboard sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleIn {
    pub kind: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub min_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    pub role_id: String,
    #[serde(default)]
    pub keep_days: Option<i64>,
    /// Sent back by a dashboard that edits what it got: ignored.
    #[serde(default)]
    #[allow(dead_code)]
    pub id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub created_at: Option<i64>,
}

fn invalid_rule(m: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_rule", m)
}

pub(crate) fn is_snowflake(s: &str) -> bool {
    (15..=21).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// Checks a rule and keeps only the fields its kind uses.
pub(crate) fn check_rule(r: RuleIn, id: String, created_at: i64) -> Result<RuleRow, ApiError> {
    let kind = r.kind.trim().to_ascii_lowercase();
    if !KINDS.contains(&kind.as_str()) {
        return Err(invalid_rule("kind: one of sub, sub_tier, member or tip_min."));
    }
    let platform = match r.platform.as_deref().map(|p| p.trim().to_ascii_lowercase()) {
        None => None,
        Some(p) if p.is_empty() => None,
        Some(p) if PLATFORMS.contains(&p.as_str()) => Some(p),
        Some(_) => return Err(invalid_rule("platform: twitch, youtube, kick, or leave it out for any.")),
    };
    if !is_snowflake(r.role_id.trim()) {
        return Err(invalid_rule("role_id: pick a role from your server."));
    }
    let keep_days = r.keep_days.unwrap_or(DEFAULT_KEEP_DAYS);
    if !(0..=MAX_KEEP_DAYS).contains(&keep_days) {
        return Err(invalid_rule("keep_days: 0 (keep for good) to 366 days."));
    }
    let tier = r.tier.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let mut row = RuleRow {
        id,
        kind: kind.clone(),
        platform,
        tier: None,
        min_minor: None,
        currency: None,
        role_id: r.role_id.trim().to_string(),
        keep_days,
        created_at,
    };
    match kind.as_str() {
        "sub" => {
            if row.platform.as_deref() == Some("youtube") {
                return Err(invalid_rule("platform: YouTube has memberships; use the member kind."));
            }
        }
        "sub_tier" => {
            if row.platform.as_deref() == Some("youtube") {
                return Err(invalid_rule("platform: YouTube has memberships; use the member kind."));
            }
            match tier.as_deref() {
                Some(t @ ("1" | "2" | "3")) => row.tier = Some(t.to_string()),
                _ => return Err(invalid_rule("tier: 1, 2 or 3 (that tier or higher).")),
            }
        }
        "member" => {
            if row.platform.as_deref().is_some_and(|p| p != "youtube") {
                return Err(invalid_rule("platform: memberships are YouTube's."));
            }
            row.platform = Some("youtube".into());
            if let Some(t) = tier {
                if t.chars().count() > 100 || t.chars().any(char::is_control) {
                    return Err(invalid_rule("tier: a membership level name, up to 100 characters."));
                }
                row.tier = Some(t);
            }
        }
        _ => {
            // tip_min
            let currency = r.currency.map(|c| c.trim().to_ascii_uppercase()).unwrap_or_default();
            if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(invalid_rule("currency: three letters, like INR."));
            }
            match r.min_minor {
                Some(m) if (1..=1_000_000_000_000).contains(&m) => row.min_minor = Some(m),
                _ => {
                    return Err(invalid_rule("min_minor: the smallest tip, in paise or cents."));
                }
            }
            row.currency = Some(currency);
        }
    }
    Ok(row)
}

/// One thing that happened on stream, as the desktop sends it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct PerkEvent {
    pub platform: String,
    pub platform_user_id: String,
    /// `sub` (Twitch, Kick), `member` (YouTube), `tip` (money: Super Chats, tips).
    pub kind: String,
    #[serde(default)]
    pub amount_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    /// `1`, `2`, `3` or `prime` for a sub; the level name for a membership.
    #[serde(default)]
    pub tier: Option<String>,
    pub at: i64,
}

/// Why an event can't be used (it's skipped and counted as `invalid`, the rest still count).
/// A time in the future (a PC clock running ahead) isn't one of them: it's taken as now.
pub(crate) fn check_event(e: &PerkEvent) -> Result<(), &'static str> {
    if !PLATFORMS.contains(&e.platform.as_str()) {
        return Err("platform must be twitch, youtube or kick");
    }
    let id = &e.platform_user_id;
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("platform_user_id isn't a platform user id");
    }
    match e.kind.as_str() {
        "sub" | "member" => {}
        "tip" => {
            if !e.amount_minor.is_some_and(|a| a > 0) {
                return Err("a tip needs amount_minor above 0");
            }
            if !e.currency.as_deref().is_some_and(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())) {
                return Err("a tip needs a three-letter currency");
            }
        }
        _ => return Err("kind must be sub, member or tip"),
    }
    if e.tier.as_ref().is_some_and(|t| t.chars().count() > 100) {
        return Err("tier is too long");
    }
    Ok(())
}

/// The events of a request body that can be used, and how many couldn't (each is skipped on
/// its own, so one bad event never costs the others their roles). `at` is clamped to `now`.
pub(crate) fn parse_events(raw: Vec<Value>, now: i64) -> (Vec<PerkEvent>, usize) {
    let mut events = Vec::with_capacity(raw.len());
    let mut invalid = 0;
    for (i, v) in raw.into_iter().enumerate() {
        match serde_json::from_value::<PerkEvent>(v) {
            Ok(mut e) => match check_event(&e) {
                Ok(()) => {
                    e.at = e.at.min(now);
                    events.push(e);
                }
                Err(why) => {
                    tracing::debug!(event = i, why, "a perk event was skipped");
                    invalid += 1;
                }
            },
            Err(err) => {
                tracing::debug!(event = i, error = %err, "a perk event was skipped");
                invalid += 1;
            }
        }
    }
    (events, invalid)
}

/// The "some roles couldn't be given" problem for the dashboard: what, and what to do.
pub(crate) fn issue_view(raw: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let (message, hint) = match v["code"].as_str()? {
        "missing_permission" => ("Some roles couldn't be given: Ferrocast's role is below them.", ROLE_HINT),
        "role_missing" => {
            ("Some roles couldn't be given: the role was deleted in Discord.", "Edit the rule and pick another role.")
        }
        "guild_missing" => (
            "Roles couldn't be given: Ferrocast isn't in your server any more.",
            "Disconnect, then connect the server again.",
        ),
        _ => return None,
    };
    Some(json!({ "rule_id": v["rule_id"], "code": v["code"], "message": message, "hint": hint, "at": v["at"] }))
}

/// Prime counts as tier 1; anything unknown as 1.
fn rank(tier: Option<&str>) -> u8 {
    match tier.map(str::to_ascii_lowercase).as_deref() {
        Some("2") | Some("tier2") => 2,
        Some("3") | Some("tier3") => 3,
        _ => 1,
    }
}

/// Whether a rule fires for an event.
pub(crate) fn matches(rule: &RuleRow, e: &PerkEvent) -> bool {
    if rule.platform.as_deref().is_some_and(|p| p != e.platform) {
        return false;
    }
    match rule.kind.as_str() {
        "sub" => e.kind == "sub",
        "sub_tier" => e.kind == "sub" && rank(e.tier.as_deref()) >= rank(rule.tier.as_deref()),
        "member" => {
            e.kind == "member"
                && rule.tier.as_deref().is_none_or(|t| e.tier.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(t)))
        }
        "tip_min" => {
            e.kind == "tip"
                && e.currency == rule.currency
                && e.amount_minor.zip(rule.min_minor).is_some_and(|(a, min)| a >= min)
        }
        _ => false,
    }
}

/// When a grant from this rule for an event at `at` ends (`None`: kept for good).
pub(crate) fn ends(rule: &RuleRow, at: i64) -> Option<i64> {
    (rule.keep_days > 0).then(|| at.saturating_add(rule.keep_days * DAY_MS))
}

/// `POST /v1/pro/discord/perk-events`: the events are already checked (`invalid` couldn't be).
///
/// Each grant stands alone: a role above Ferrocast's (50013), a deleted role, a member who
/// left, or a server the bot was removed from (50001) is counted and the others still go out;
/// only Discord being busy or down (429, 5xx) stops the batch, so the desktop sends it again.
/// The first problem is kept for the dashboard (`perk_issue` in the view).
pub(crate) async fn apply(st: &AppState, sub: &str, events: &[PerkEvent], invalid: usize) -> Result<Value, ApiError> {
    let guild = st.db.discord_guild(sub).map_err(ApiError::db)?.ok_or_else(not_connected)?;
    let api = st.discord.api()?;
    let rules = st.db.discord_rules(sub).map_err(ApiError::db)?;
    let now = now_ms();
    let (mut not_linked, mut too_old) = (0usize, 0usize);
    // (user, role) → (rule id, end), the latest end winning; None = kept for good.
    let mut want: BTreeMap<(String, String), (String, Option<i64>)> = BTreeMap::new();
    for e in events {
        let at = e.at.min(now);
        if now - at > MAX_EVENT_AGE_MS {
            too_old += 1;
            continue;
        }
        let firing: Vec<&RuleRow> = rules.iter().filter(|r| matches(r, e)).collect();
        if firing.is_empty() {
            continue;
        }
        let users = st.db.discord_users_for(&guild.guild_id, &e.platform, &e.platform_user_id).map_err(ApiError::db)?;
        if users.is_empty() {
            not_linked += 1;
            continue;
        }
        for r in firing {
            let end = ends(r, at);
            if end.is_some_and(|t| t <= now) {
                continue; // it would already be over
            }
            for u in &users {
                let slot = want.entry((u.clone(), r.role_id.clone())).or_insert_with(|| (r.id.clone(), end));
                slot.1 = match (slot.1, end) {
                    (None, _) | (_, None) => None,
                    (Some(a), Some(b)) => Some(a.max(b)),
                };
                if slot.1 == end {
                    slot.0 = r.id.clone();
                }
            }
        }
    }
    let (mut granted, mut blocked, mut not_member, mut role_gone, mut refused) = (0usize, 0usize, 0usize, 0usize, 0);
    let mut guild_gone = false;
    let mut issue: Option<(String, &'static str)> = None;
    let mut working: BTreeSet<String> = BTreeSet::new();
    for ((user, role), (rule, end)) in want {
        if guild_gone {
            blocked += 1;
            continue;
        }
        match api.add_role(&guild.guild_id, &user, &role).await {
            Ok(()) => {
                st.db.discord_grant(&guild.guild_id, &user, &role, &rule, end, now).map_err(ApiError::db)?;
                granted += 1;
                working.insert(rule);
            }
            // The viewer linked but isn't (or is no longer) in the server.
            Err(DiscordError::NotFound(codes::UNKNOWN_MEMBER)) => not_member += 1,
            // The streamer deleted the role: the rule can't fire until they pick another.
            Err(DiscordError::NotFound(codes::UNKNOWN_ROLE)) => {
                role_gone += 1;
                issue.get_or_insert((rule, "role_missing"));
            }
            // 50013: this role is above Ferrocast's. Others may still work.
            Err(DiscordError::MissingPermission) => {
                blocked += 1;
                issue.get_or_insert((rule, "missing_permission"));
            }
            // 50001 / unknown server: the bot was removed. Nothing else can work now.
            Err(DiscordError::MissingAccess | DiscordError::NotFound(codes::UNKNOWN_GUILD)) => {
                guild_gone = true;
                blocked += 1;
                issue = Some((rule, "guild_missing"));
            }
            // Busy, down, or our token refused: the whole batch comes again later.
            Err(e @ (DiscordError::RateLimited { .. } | DiscordError::Unavailable(_) | DiscordError::Unauthorized)) => {
                return Err(discord_error(e));
            }
            Err(e) => {
                tracing::warn!(error = %e, "Discord refused a perk role");
                refused += 1;
            }
        }
    }
    match &issue {
        Some((rule, code)) => {
            let raw = json!({ "rule_id": rule, "code": code, "at": now }).to_string();
            st.db.discord_set_perk_issue(sub, Some(&raw)).map_err(ApiError::db)?;
        }
        None => {
            for rule in &working {
                st.db.discord_clear_perk_issue_for(sub, rule).map_err(ApiError::db)?;
            }
        }
    }
    let skipped = not_linked + not_member + role_gone + too_old + refused;
    Ok(json!({
        "events": events.len() + invalid, "granted": granted, "blocked": blocked, "skipped": skipped,
        "invalid": invalid, "not_linked": not_linked, "not_member": not_member, "role_missing": role_gone,
        "too_old": too_old, "guild_missing": guild_gone,
        "issue": issue.map(|(rule, code)| json!({ "rule_id": rule, "code": code })),
    }))
}

/// The hourly sweep: ended roles are taken back (a few hundred at a time). One that's already
/// gone (member left, role deleted, server gone) is just forgotten. Rate limits stop the
/// round. Any other failure (permissions, Discord down) backs that grant off (1 h, 2 h, 4 h …
/// up to a day) and after 5 tries it's forgotten, so a role that can never come off can't
/// keep the others waiting. A grant renewed while it was being removed gets its role back.
pub async fn sweep(st: &AppState) {
    if st.discord.api().is_err() {
        return;
    }
    let now = now_ms();
    let due = match st.db.discord_grants_due(now, SWEEP_BATCH) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list ended Discord perks");
            return;
        }
    };
    sweep_grants(st, due, now).await;
    super::sweep_lapsed(st, now).await;
}

/// Takes back the roles of these ended grants (the sweep's list).
pub(crate) async fn sweep_grants(st: &AppState, due: Vec<super::store::GrantRow>, now: i64) {
    let Ok(api) = st.discord.api() else { return };
    let (mut removed, mut later, mut gave_up, mut restored) = (0, 0, 0, 0);
    for g in due {
        match api.remove_role(&g.guild_id, &g.discord_user_id, &g.role_id).await {
            Ok(()) | Err(DiscordError::NotFound(_)) => match st.db.discord_ungrant(&g) {
                Ok(Ungranted::Forgotten) => removed += 1,
                Ok(Ungranted::Renewed) => {
                    // A new sub came in meanwhile: the role goes back on.
                    match api.add_role(&g.guild_id, &g.discord_user_id, &g.role_id).await {
                        Ok(()) => restored += 1,
                        Err(e) => tracing::warn!(error = %e, "couldn't give back a renewed Discord perk"),
                    }
                }
                Err(e) => tracing::warn!(error = %e, "couldn't forget an ended Discord perk"),
            },
            Err(DiscordError::RateLimited { retry_after_s }) => {
                later += 1;
                tracing::info!(retry_after_s, "Discord is rate limiting the perk sweep; the rest waits");
                break;
            }
            Err(e) => {
                later += 1;
                tracing::debug!(error = %e, attempts = g.attempts + 1, "an ended Discord perk couldn't be removed yet");
                let wait = SWEEP_RETRY_MS.saturating_mul(1 << g.attempts.clamp(0, 10)).min(SWEEP_RETRY_MAX_MS);
                match st.db.discord_grant_failed(&g, now + wait, SWEEP_MAX_ATTEMPTS) {
                    Ok(true) => gave_up += 1,
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "couldn't note a failed Discord perk removal"),
                }
            }
        }
        if !st.discord.sweep_pause.is_zero() {
            tokio::time::sleep(st.discord.sweep_pause).await;
        }
    }
    if removed + later + restored > 0 {
        tracing::info!(removed, later, gave_up, restored, "Discord perk sweep done");
    }
}
