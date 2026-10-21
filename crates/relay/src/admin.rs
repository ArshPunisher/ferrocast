//! The owner's read-only numbers for the Control Center (`GET /admin/v1/stats`).
//!
//! Aggregate counts only: how many installs, how many Pro subscriptions and of which plan,
//! how much each Pro feature is used. Never an install id, a subscription id, an email or
//! its hash, a handle, a server name, a token, or any money a viewer tipped.
//!
//! Guarded three ways:
//! - only from the server's own Docker network: anything that came through Cloudflare (the
//!   public hostnames) is refused with 403, so the endpoint can't be tried from the internet;
//! - a bearer token (`RELAY_ADMIN_TOKEN`, 32+ characters, made on the server by
//!   `deploy/control-center-key.sh`), compared as SHA-256 digests in constant time;
//! - at most [`MAX_PER_MINUTE`] requests a minute, relay-wide.
//!
//! Off (503 with a hint) until the token is set.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::billing::access::{self, Plan};
use crate::billing::comp;
use crate::billing::store::SubRow;
use crate::db::Db;
use crate::quota::Period;
use crate::{ApiError, AppState, now_ms};

/// The shortest token the relay accepts. `control-center-key.sh` makes 64 hex characters.
pub const MIN_TOKEN_LEN: usize = 32;
/// Requests a minute, relay-wide (the Control Center asks about once a page view).
pub const MAX_PER_MINUTE: u32 = 60;
/// The file the nightly backup writes next to `relay.db` after a good run
/// (`deploy/relay-backup.sh`): one UTC time, `2026-10-20T21:30:12Z`.
pub const BACKUP_MARKER: &str = "backup-last-ok";

const DAY_MS: i64 = 86_400_000;

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/admin/v1/stats", axum::routing::get(stats))
}

/// The admin token and where the backup marker is, read once at start.
#[derive(Clone, Default)]
pub struct AdminConfig {
    /// SHA-256 of the token; `None` when it isn't set or is too short.
    token_hash: Option<[u8; 32]>,
    /// Set but shorter than [`MIN_TOKEN_LEN`]: refused, with a hint saying so.
    too_short: bool,
    backup_marker: Option<PathBuf>,
}

impl std::fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token, not even its hash.
        f.debug_struct("AdminConfig")
            .field("token", &if self.token_hash.is_some() { "set" } else { "missing" })
            .field("too_short", &self.too_short)
            .field("backup_marker", &self.backup_marker)
            .finish()
    }
}

impl AdminConfig {
    /// `RELAY_ADMIN_TOKEN`, and the backup marker next to the database at `data`.
    pub fn from_env(data: &Path) -> AdminConfig {
        let token = std::env::var("RELAY_ADMIN_TOKEN").ok();
        let dir = data.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        AdminConfig::new(token.as_deref(), Some(dir.join(BACKUP_MARKER)))
    }

    pub fn new(token: Option<&str>, backup_marker: Option<PathBuf>) -> AdminConfig {
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        let too_short = token.is_some_and(|t| t.len() < MIN_TOKEN_LEN);
        let token_hash = token.filter(|_| !too_short).map(digest);
        AdminConfig { token_hash, too_short, backup_marker }
    }

    /// For the start-up log line.
    pub fn state(&self) -> &'static str {
        match (self.token_hash.is_some(), self.too_short) {
            (true, _) => "on",
            (false, true) => "too_short",
            (false, false) => "off",
        }
    }
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// The admin settings, when the relay started, and this minute's request count.
#[derive(Debug)]
pub(crate) struct Runtime {
    cfg: AdminConfig,
    started_at: i64,
    minute: Mutex<(i64, u32)>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(AdminConfig::default())
    }
}

impl Runtime {
    pub(crate) fn new(cfg: AdminConfig) -> Runtime {
        Runtime { cfg, started_at: now_ms(), minute: Mutex::new((0, 0)) }
    }

    /// Counts one request against this minute's allowance; `Err(seconds to wait)` when it's used up.
    fn take(&self, now: i64) -> Result<(), i64> {
        let minute = now.div_euclid(60_000);
        let mut m = self.minute.lock().unwrap_or_else(|p| p.into_inner());
        if m.0 != minute {
            *m = (minute, 0);
        }
        if m.1 >= MAX_PER_MINUTE {
            return Err(((minute + 1) * 60_000 - now).div_euclid(1000).max(1));
        }
        m.1 += 1;
        Ok(())
    }
}

/// Did this request come in through Cloudflare (a public hostname) or another proxy?
/// Cloudflare's edge always sets these, and a caller can't remove them; a call from the
/// server's own Docker network has none.
fn proxied(headers: &HeaderMap) -> bool {
    ["cf-connecting-ip", "cf-ray", "cdn-loop", "x-forwarded-for", "forwarded"].iter().any(|h| headers.contains_key(*h))
}

fn unauthorized(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
        .hint(
            "Send Authorization: Bearer <RELAY_ADMIN_TOKEN>. On the server, control-center-key.sh gives the relay \
             and the Control Center the same token.",
        )
        .header(header::WWW_AUTHENTICATE, "Bearer")
}

/// The checks every admin route makes, in this order: only from the server's own network
/// (403), within the allowance (429), the admin API switched on (503), the right token (401).
pub(crate) fn guard(st: &AppState, peer: std::net::SocketAddr, headers: &HeaderMap, now: i64) -> Result<(), ApiError> {
    if proxied(headers) || !crate::is_private(peer.ip()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "internal_only",
            "The admin API is only served inside the server, not through a public address.",
        )
        .hint("Call http://ferrocast-relay:8787/admin/v1/… from a container on the relay's Docker network."));
    }
    if let Err(wait) = st.admin.take(now) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("Too many admin requests: at most {MAX_PER_MINUTE} a minute."),
        )
        .hint(format!("Try again in {wait} seconds."))
        .header(header::RETRY_AFTER, wait.to_string()));
    }
    let Some(want) = st.admin.cfg.token_hash else {
        let (message, hint) = if st.admin.cfg.too_short {
            (
                format!("RELAY_ADMIN_TOKEN is shorter than {MIN_TOKEN_LEN} characters, so the admin API is off."),
                "Run control-center-key.sh --rotate on the server to make a proper one.",
            )
        } else {
            (
                "The admin API is off: RELAY_ADMIN_TOKEN isn't set on this relay.".to_string(),
                "Run control-center-key.sh on the server; it sets the token for the relay and the Control Center.",
            )
        };
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "admin_off", message).hint(hint));
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| unauthorized("The admin token is missing."))?;
    // Digests of equal length, so neither the length nor the content leaks through timing.
    if !bool::from(digest(given).ct_eq(&want)) {
        return Err(unauthorized("The admin token is wrong."));
    }
    Ok(())
}

/// `GET /admin/v1/stats`: the Control Center's numbers (see the module docs).
async fn stats(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let now = now_ms();
    guard(&st, peer, &headers, now)?;
    let read = st.clone();
    let body =
        tokio::task::spawn_blocking(move || collect(&read, now)).await.map_err(|_| unreadable())?.map_err(|e| {
            tracing::error!(error = %e, "couldn't read the admin stats");
            unreadable()
        })?;
    let mut res = axum::Json(body).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

pub(crate) fn unreadable() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The relay couldn't read its database just now.")
        .hint("Try again in a minute; if it keeps happening, check the relay's logs.")
}

/// Everything the endpoint answers, at `now`.
pub(crate) fn collect(st: &AppState, now: i64) -> rusqlite::Result<Value> {
    let c = st.db.counts(now)?;
    Ok(json!({
        "relay": {
            "version": env!("CARGO_PKG_VERSION"),
            "started_at": st.admin.started_at,
            "uptime_seconds": (now - st.admin.started_at).max(0) / 1000,
            "now": now,
        },
        "billing": { "mode": st.billing_cfg.mode(), "sales_open": st.billing_cfg.sales_open },
        "features": crate::billing::features(st),
        "installs": c.installs,
        "pro": c.pro,
        "usage": {
            "tts": {
                "month": c.tts_month,
                "chars": c.tts_chars,
                "requests": c.tts_requests,
                "cap": st.tts.global_cap(),
            },
            "cloud_bot": c.cloud_bot,
            "discord": { "guilds": c.discord_guilds },
            "weekly_email": { "opted_in": c.mail_on },
            "cloud_backups": { "subscriptions": c.backups },
            "remote": { "devices": c.remote_devices },
            "tips": c.tips,
            "smart": crate::smart::admin_view(st),
            "history": { "free_accounts": c.history_free, "pro_subscriptions": c.history_pro },
            "youtube": youtube_units(st, now)?,
            "mail": mail_today(st, now)?,
        },
        "backup": { "last_ok_at": st.admin.cfg.backup_marker.as_deref().and_then(read_marker) },
    }))
}

/// The cloud bot's YouTube Data API units today, relay-wide (every subscription's calls
/// together), against `YOUTUBE_CLOUD_DAILY_UNITS`. Google's quota day is Pacific, so it resets
/// at Pacific midnight. `daily_cap` is null while YouTube from the cloud is off.
fn youtube_units(st: &AppState, now: i64) -> rusqlite::Result<Value> {
    use crate::bot::youtube::{RELAY_SUB, pacific_day};
    let used = st.db.yt_units_used(RELAY_SUB, None, now)?;
    let (_, resets_at) = pacific_day(now);
    let cap = st.bot.youtube_cfg().map(|c| c.units);
    Ok(json!({ "units_today": used, "daily_cap": cap, "resets_at": resets_at }))
}

/// Emails sent today (UTC), relay-wide (confirms and weekly reports together), against
/// `MAIL_DAILY_BUDGET`.
fn mail_today(st: &AppState, now: i64) -> rusqlite::Result<Value> {
    let day = crate::mail::budget_day(now);
    let sent = st.db.mail_budget_used(day)?;
    Ok(json!({ "sent_today": sent, "daily_budget": st.mail.budget(), "resets_at": (day + 1) * 86_400_000 }))
}

/// The nightly backup's last good run, if the marker holds exactly one UTC time.
fn read_marker(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut s = String::new();
    std::fs::File::open(path).ok()?.take(64).read_to_string(&mut s).ok()?;
    let s = s.trim();
    let b = s.as_bytes();
    let shape = b.len() == 20
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        });
    (shape && crate::kick::rfc3339_ms(s).is_some()).then(|| s.to_string())
}

/// The counts, read in one transaction so they agree with each other.
pub(crate) struct Counts {
    installs: Value,
    pro: Value,
    tts_month: String,
    tts_chars: i64,
    /// Accounts and subscriptions with stream history kept (counts only).
    history_free: i64,
    history_pro: i64,
    tts_requests: i64,
    cloud_bot: Value,
    discord_guilds: usize,
    mail_on: usize,
    backups: usize,
    remote_devices: usize,
    tips: Value,
}

/// One subscription as the counts need it (its id never leaves this module).
struct Sub {
    row: SubRow,
    first_paid: Option<i64>,
    installs: i64,
}

/// MRR in paise: monthly subscriptions plus a twelfth of the yearly ones, rounded once.
pub fn mrr_paise(monthly: i64, yearly: i64) -> i64 {
    let year = yearly * Plan::Yearly.amount_paise();
    monthly * Plan::Monthly.amount_paise() + (year + 6) / 12
}

/// The first moment (ms) of the UTC month `now` is in.
fn month_start(now: i64) -> i64 {
    let (y, m, _) = crate::quota::civil(now.div_euclid(DAY_MS));
    crate::quota::days_from_civil(y, m, 1) * DAY_MS
}

impl Db {
    pub(crate) fn counts(&self, now: i64) -> rusqlite::Result<Counts> {
        let c = self.c();
        let tx = c.unchecked_transaction()?;
        let (d7, d30, month) = (now - 7 * DAY_MS, now - 30 * DAY_MS, month_start(now));

        let installs = tx.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(last_seen > created_at AND last_seen >= ?1), 0),
                    COALESCE(SUM(last_seen > created_at AND last_seen >= ?2), 0),
                    COALESCE(SUM(created_at >= ?1), 0),
                    COALESCE(SUM(created_at >= ?2), 0)
               FROM installs",
            [d7, d30],
            |r| {
                Ok(json!({
                    "total": r.get::<_, i64>(0)?,
                    "active_7d": r.get::<_, i64>(1)?,
                    "active_30d": r.get::<_, i64>(2)?,
                    "new_7d": r.get::<_, i64>(3)?,
                    "new_30d": r.get::<_, i64>(4)?,
                }))
            },
        )?;

        // No email hash is read: the counts don't need it.
        let mut q = tx.prepare(
            "SELECT s.id, s.plan, s.status, s.current_end, s.cancel_at_end, s.refunded_at, s.synced_at,
                    (SELECT MIN(p.at) FROM pro_payments p WHERE p.sub_id = s.id),
                    (SELECT COUNT(*) FROM pro_installs i WHERE i.sub_id = s.id)
               FROM pro_subs s",
        )?;
        let subs = q
            .query_map([], |r| {
                Ok(Sub {
                    row: SubRow {
                        id: r.get(0)?,
                        plan: r.get(1)?,
                        email_hash: String::new(),
                        status: r.get(2)?,
                        current_end: r.get(3)?,
                        cancel_at_end: r.get::<_, i64>(4)? != 0,
                        refunded_at: r.get(5)?,
                        synced_at: r.get(6)?,
                    },
                    first_paid: r.get(7)?,
                    installs: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);

        let (mut active, mut monthly, mut yearly, mut other, mut cancelling) = (0i64, 0i64, 0i64, 0i64, 0i64);
        let (mut new_this_month, mut ended_this_month, mut pro_installs) = (0i64, 0i64, 0i64);
        let (mut comp_active, mut comp_installs) = (0i64, 0i64);
        // Subscriptions with Pro right now (test grants too): feature use is counted for these.
        let mut pro_now: HashSet<String> = HashSet::new();
        for s in &subs {
            let is_pro = access::is_pro(&s.row.access(), now);
            if comp::is_comp(&s.row.id) {
                if is_pro {
                    comp_active += 1;
                    comp_installs += s.installs;
                    pro_now.insert(s.row.id.clone());
                }
                continue;
            }
            if is_pro {
                pro_now.insert(s.row.id.clone());
                active += 1;
                pro_installs += s.installs;
                match Plan::parse(&s.row.plan) {
                    Some(Plan::Monthly) => monthly += 1,
                    Some(Plan::Yearly) => yearly += 1,
                    None => other += 1,
                }
                if s.row.cancel_at_end {
                    cancelling += 1;
                }
            }
            if s.first_paid.is_some_and(|t| t >= month) {
                new_this_month += 1;
            }
            // Paid at some point, no Pro now, and its Pro stopped this month (cancelled,
            // refunded, or renewals that failed for good).
            if !is_pro && s.first_paid.is_some() && (month..=now).contains(&s.row.stopped_at()) {
                ended_this_month += 1;
            }
        }
        let pro = json!({
            "active": active,
            "monthly": monthly,
            "yearly": yearly,
            "other": other,
            "cancelling": cancelling,
            "currency": "INR",
            "mrr_paise": mrr_paise(monthly, yearly),
            "prices": { "monthly": Plan::Monthly.amount_paise(), "yearly": Plan::Yearly.amount_paise() },
            "new_this_month": new_this_month,
            "ended_this_month": ended_this_month,
            "installs": pro_installs,
            "comp": { "active": comp_active, "installs": comp_installs },
        });

        let sub_ids = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<String>> {
            let mut q = tx.prepare(sql)?;
            q.query_map(p, |r| r.get::<_, String>(0))?.collect()
        };
        let live = |ids: Vec<String>| ids.into_iter().filter(|id| pro_now.contains(id)).count();

        let mut q = tx.prepare("SELECT sub_id, platform FROM bot_channels")?;
        let channels = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        let on_platform = |p: &str| channels.iter().filter(|(s, pl)| pl == p && pro_now.contains(s)).count();
        let cloud_bot = json!({
            "on": live(sub_ids("SELECT sub_id FROM bot_config WHERE enabled = 1", &[])?),
            "channels": channels.iter().filter(|(s, _)| pro_now.contains(s)).count(),
            "twitch": on_platform("twitch"),
            "kick": on_platform("kick"),
            "youtube": on_platform("youtube"),
        });
        let discord_guilds = live(sub_ids("SELECT sub_id FROM discord_guilds", &[])?);
        let mail_on = live(sub_ids("SELECT sub_id FROM mail_addresses WHERE status = 'on'", &[])?);
        let backups = live(sub_ids("SELECT DISTINCT sub_id FROM cloud_backups WHERE state = 'complete'", &[])?);
        let remote_devices = live(sub_ids("SELECT sub_id FROM remote_devices WHERE expires_at > ?1", &[&now])?);

        let one = |sql: &str| tx.query_row(sql, [], |r| r.get::<_, i64>(0));
        // Counts only: how many tips reached a goal, never their amounts.
        let tips = json!({
            "pages": one("SELECT COUNT(*) FROM tip_pages")?,
            "branded": one("SELECT COUNT(*) FROM tip_brand WHERE lapsed_at IS NULL")?,
            "goals": one("SELECT COUNT(*) FROM tip_goals WHERE lapsed_at IS NULL")?,
            "goal_tips": one("SELECT COUNT(*) FROM tip_goal_payments")?,
        });

        let key = Period::Month.key(now);
        let (tts_chars, tts_requests) = tx
            .query_row("SELECT chars, requests FROM tts_usage WHERE scope = 'all' AND month = ?1", [&key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok((0, 0)),
                e => Err(e),
            })?;

        Ok(Counts {
            installs,
            pro,
            history_free: one("SELECT COUNT(DISTINCT sub_id) FROM streams WHERE sub_id LIKE 'ac\\_%' ESCAPE '\\'")?,
            history_pro: one("SELECT COUNT(DISTINCT sub_id) FROM streams WHERE sub_id NOT LIKE 'ac\\_%' ESCAPE '\\'")?,
            tts_month: key.trim_start_matches('m').to_string(),
            tts_chars,
            tts_requests,
            cloud_bot,
            discord_guilds,
            mail_on,
            backups,
            remote_devices,
            tips,
        })
    }
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
