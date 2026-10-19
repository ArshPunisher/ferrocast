//! Pro tip goal page (docs/plan/PRO.md F6): `<tip host>/<handle>/goal` (and
//! `/t/<handle>/goal` on the relay) shows a title, a progress bar and raised / target.
//!
//! The relay already checks and forwards every tip; for a Pro install with a goal it also
//! keeps a row per tip in the goal's currency: `tip_goal_payments(goal_id, payment_hash,
//! amount_minor)`, where the hash is SHA-256 of `install|provider|payment id` (never the id
//! itself, never a name or message). Tips in other currencies aren't counted (the page says
//! so); a full refund or a dispute deletes that tip's row, so it comes off (R-32), and a tip
//! whose refund arrived first is never counted. Rows go 60 days after the goal ends, with
//! the goal.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use rsa::sha2::{Digest, Sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::brand::Palette;
use super::{Body, Notice, TipConfig, clean_text, esc, provider_digits};
use crate::billing::gate::require_pro;
use crate::db::Db;
use crate::{ApiError, AppState, now_ms};

/// Migration 3's goal tables.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tip_goals (id TEXT PRIMARY KEY, install_id TEXT NOT NULL UNIQUE, title TEXT NOT NULL, target_minor INTEGER NOT NULL, currency TEXT NOT NULL, starts_at INTEGER NOT NULL, ends_at INTEGER, updated_at INTEGER NOT NULL, lapsed_at INTEGER);
CREATE TABLE IF NOT EXISTS tip_goal_payments (goal_id TEXT NOT NULL REFERENCES tip_goals(id) ON DELETE CASCADE, payment_hash TEXT NOT NULL, amount_minor INTEGER NOT NULL, PRIMARY KEY (goal_id, payment_hash));
";

/// A goal's payment rows (and the goal) are deleted this long after it ends.
pub const KEEP_AFTER_END_MS: i64 = 60 * 86_400_000;
pub const TITLE_MAX: usize = 60;
pub const TARGET_MAX: i64 = 1_000_000_000_000;
/// The furthest `ends_at` can be.
pub const MAX_LENGTH_MS: i64 = 366 * 86_400_000;
/// The OBS variant reloads itself this often (no scripts on the page).
pub const OVERLAY_REFRESH_S: u32 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub id: String,
    pub title: String,
    /// Minor units of `currency` (₹500 is 50000).
    pub target: i64,
    pub currency: String,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
}

impl Goal {
    pub fn ended(&self, now: i64) -> bool {
        self.ends_at.is_some_and(|e| now >= e)
    }
}

/// The digits a currency's minor unit has (ISO 4217, as the providers send them).
pub fn digits(currency: &str) -> u32 {
    provider_digits("razorpay", currency)
}

/// `amount` in `from` digits, in `to` digits (truncating when there are fewer).
fn rescale(amount: i64, from: u32, to: u32) -> Option<i64> {
    if to >= from {
        amount.checked_mul(10i64.checked_pow(to - from)?)
    } else {
        Some(amount / 10i64.checked_pow(from - to)?)
    }
}

/// "3.50" → 350 with two digits. `None` for anything that isn't a plain positive decimal.
pub fn decimal_minor(s: &str, digits: u32) -> Option<i64> {
    let s = s.trim().replace(',', "");
    let (whole, frac) = s.split_once('.').unwrap_or((&s, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let d = usize::try_from(digits).ok()?;
    let mut f: String = frac.chars().take(d).collect();
    while f.len() < d {
        f.push('0');
    }
    let w: i64 = whole.parse().ok()?;
    let f: i64 = if f.is_empty() { 0 } else { f.parse().ok()? };
    w.checked_mul(10i64.checked_pow(digits)?)?.checked_add(f)
}

fn payment_hash(install: &str, provider: &str, payment_id: &str) -> String {
    Sha256::digest(format!("{install}|{provider}|{payment_id}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Db {
    pub fn tip_goal(&self, install: &str) -> rusqlite::Result<Option<Goal>> {
        self.c()
            .query_row(
                "SELECT id, title, target_minor, currency, starts_at, ends_at FROM tip_goals WHERE install_id = ?1",
                [install],
                |r| {
                    Ok(Goal {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        target: r.get(2)?,
                        currency: r.get(3)?,
                        starts_at: r.get(4)?,
                        ends_at: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    /// Saves a goal. The same currency keeps the goal and what it raised; another currency
    /// starts a new one (the old one's rows go with it).
    pub fn set_tip_goal(
        &self,
        install: &str,
        title: &str,
        target: i64,
        currency: &str,
        ends_at: Option<i64>,
    ) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let old: Option<(String, String)> = tx
            .query_row("SELECT id, currency FROM tip_goals WHERE install_id = ?1", [install], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match old {
            Some((id, cur)) if cur == currency => {
                tx.execute(
                    "UPDATE tip_goals SET title = ?2, target_minor = ?3, ends_at = ?4, updated_at = ?5 WHERE id = ?1",
                    params![id, title, target, ends_at, now],
                )?;
            }
            old => {
                if let Some((id, _)) = old {
                    tx.execute("DELETE FROM tip_goals WHERE id = ?1", [id])?;
                }
                tx.execute(
                    "INSERT INTO tip_goals(id, install_id, title, target_minor, currency, starts_at, ends_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6)",
                    params![crate::random(12), install, title, target, currency, now, ends_at],
                )?;
            }
        }
        tx.commit()
    }

    pub fn delete_tip_goal(&self, install: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM tip_goals WHERE install_id = ?1", [install])? > 0)
    }

    /// (raised in minor units, tips counted).
    pub fn tip_goal_raised(&self, goal_id: &str) -> rusqlite::Result<(i64, i64)> {
        self.c().query_row(
            "SELECT COALESCE(SUM(amount_minor), 0), COUNT(*) FROM tip_goal_payments WHERE goal_id = ?1",
            [goal_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn add_goal_payment(&self, goal_id: &str, hash: &str, amount: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "INSERT OR IGNORE INTO tip_goal_payments(goal_id, payment_hash, amount_minor) VALUES (?1, ?2, ?3)",
            params![goal_id, hash, amount],
        )? > 0)
    }

    fn remove_goal_payment(&self, install: &str, hash: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute(
            "DELETE FROM tip_goal_payments WHERE payment_hash = ?2
               AND goal_id IN (SELECT id FROM tip_goals WHERE install_id = ?1)",
            params![install, hash],
        )? > 0)
    }

    /// Goals that ended over 60 days ago go, with their rows.
    pub(crate) fn prune_tip_goals(&self, now: i64) -> rusqlite::Result<usize> {
        self.c().execute("DELETE FROM tip_goals WHERE ends_at IS NOT NULL AND ends_at < ?1", [now - KEEP_AFTER_END_MS])
    }

    /// The install behind a public tip page name.
    pub fn tip_install_by_handle(&self, handle: &str) -> rusqlite::Result<Option<(String, String)>> {
        self.c()
            .query_row("SELECT install_id, config FROM tip_pages WHERE handle = ?1", [handle], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
    }
}

/// Counts a tip the relay just queued towards this install's goal, or takes a reversed
/// one back. Never fails the webhook: the tip itself is already queued.
pub(super) fn record(st: &AppState, install: &str, n: &Notice<'_>) {
    if let Err(e) = try_record(st, install, n) {
        tracing::warn!(error = %e, "couldn't update a tip goal");
    }
}

fn try_record(st: &AppState, install: &str, n: &Notice<'_>) -> rusqlite::Result<()> {
    let hash = payment_hash(install, n.provider, n.payment_id);
    if matches!(n.status, "refunded" | "disputed") {
        st.db.remove_goal_payment(install, &hash)?;
        return Ok(());
    }
    if n.status != "captured" {
        return Ok(());
    }
    let Some(goal) = st.db.tip_goal(install)? else { return Ok(()) };
    let now = now_ms();
    if now < goal.starts_at || goal.ended(now) || !n.currency.eq_ignore_ascii_case(&goal.currency) {
        return Ok(());
    }
    // A lapsed install's goal stops counting (R-04: cloud work stops, data stays a while).
    if !super::install_is_pro(st, install) {
        return Ok(());
    }
    // The refund or dispute came first: this tip was taken back before it arrived.
    for undo in ["refunded", "disputed"] {
        if st.db.seen(&format!("tip:{install}:{}:{}:{undo}", n.provider, n.payment_id))? {
            return Ok(());
        }
    }
    let want = digits(&goal.currency);
    let amount = match (n.amount_minor, n.digits, n.amount) {
        (Some(m), Some(d), _) => rescale(m, d, want),
        (_, _, Some(dec)) => decimal_minor(dec, want),
        _ => None,
    };
    if let Some(a) = amount.filter(|a| *a > 0) {
        st.db.add_goal_payment(&goal.id, &hash, a)?;
    }
    Ok(())
}

// ── Money on the page ────────────────────────────────────────────────────────────────

fn symbol(currency: &str) -> Option<&'static str> {
    Some(match currency {
        "INR" => "₹",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "AUD" => "A$",
        "CAD" => "C$",
        "BRL" => "R$",
        "KRW" => "₩",
        "PHP" => "₱",
        "TRY" => "₺",
        _ => return None,
    })
}

/// Thousands grouping; Indian (lakh) grouping for INR.
fn group(n: i64, indian: bool) -> String {
    let s = n.to_string();
    let (head, tail) = if s.len() > 3 { s.split_at(s.len() - 3) } else { return s };
    let step = if indian { 2 } else { 3 };
    let mut parts = Vec::new();
    let mut h = head;
    while h.len() > step {
        let (a, b) = h.split_at(h.len() - step);
        parts.push(b);
        h = a;
    }
    parts.push(h);
    parts.reverse();
    format!("{},{tail}", parts.join(","))
}

/// "₹2,500", "$12.50", "CHF 40".
pub fn money(minor: i64, currency: &str) -> String {
    let d = digits(currency);
    let scale = 10i64.pow(d);
    let (whole, frac) = (minor / scale, minor % scale);
    let mut n = group(whole, currency == "INR");
    if frac != 0 {
        n.push_str(&format!(".{:0width$}", frac, width = usize::try_from(d).unwrap_or(2)));
    }
    match symbol(currency) {
        Some(s) => format!("{s}{n}"),
        None => format!("{currency} {n}"),
    }
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// The goal page's public links (`None` without a tip page).
fn links(st: &AppState, install: &str) -> (Option<String>, Option<String>) {
    let handle = st
        .db
        .tip_config(install)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<TipConfig>(&j).ok())
        .map(|c| c.handle);
    match handle {
        Some(h) => {
            let url = format!("{}/goal", super::page_url(&st.cfg, &h));
            (Some(url.clone()), Some(format!("{url}?overlay=1")))
        }
        None => (None, None),
    }
}

fn view(st: &AppState, install: &str, g: &Goal) -> Result<Value, ApiError> {
    let (raised, tips) = st.db.tip_goal_raised(&g.id).map_err(ApiError::db)?;
    let (page_url, overlay_url) = links(st, install);
    Ok(json!({
        "title": g.title, "target": g.target, "currency": g.currency, "digits": digits(&g.currency),
        "starts_at": g.starts_at, "ends_at": g.ends_at, "ended": g.ended(now_ms()),
        "raised": raised, "tips": tips, "page_url": page_url, "overlay_url": overlay_url,
    }))
}

fn no_goal() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's no goal yet.").hint("Set one up on the Tips screen.")
}

/// `GET /v1/pro/tips/goal`
pub async fn get_goal(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let g = st.db.tip_goal(&ctx.install).map_err(ApiError::db)?.ok_or_else(no_goal)?;
    Ok(axum::Json(view(&st, &ctx.install, &g)?).into_response())
}

#[derive(Debug, Deserialize)]
pub struct GoalBody {
    title: String,
    target: i64,
    currency: String,
    #[serde(default)]
    ends_at: Option<i64>,
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

/// `PUT /v1/pro/tips/goal {title, target, currency, ends_at?}`
pub async fn put_goal(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Body<GoalBody>, ApiError>,
) -> Result<Response, ApiError> {
    // The gate first (401/402), then the body (400/413/415).
    let ctx = require_pro(&st, &headers).await?;
    let Body(b) = body?;
    let title = clean_text(&b.title, usize::MAX);
    if title.is_empty() || title.chars().count() > TITLE_MAX {
        return Err(invalid(format!("The goal title must be 1 to {TITLE_MAX} characters.")));
    }
    let currency = b.currency.trim().to_ascii_uppercase();
    if currency.len() != 3 || !currency.bytes().all(|c| c.is_ascii_uppercase()) {
        return Err(invalid("currency must be a 3-letter code like INR or USD."));
    }
    if !(1..=TARGET_MAX).contains(&b.target) {
        return Err(invalid("target must be a whole number above 0 in the currency's smallest unit (₹500 is 50000)."));
    }
    let now = now_ms();
    if let Some(e) = b.ends_at
        && (e <= now || e > now + MAX_LENGTH_MS)
    {
        return Err(invalid("ends_at must be in the future and within a year."));
    }
    if st.db.tip_config(&ctx.install).map_err(ApiError::db)?.is_none() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "no_tip_page", "Create your tip page first.")
            .hint("The goal page lives next to your tip page, at its link plus /goal."));
    }
    st.db.set_tip_goal(&ctx.install, &title, b.target, &currency, b.ends_at).map_err(ApiError::db)?;
    let g = st.db.tip_goal(&ctx.install).map_err(ApiError::db)?.ok_or_else(no_goal)?;
    Ok(axum::Json(view(&st, &ctx.install, &g)?).into_response())
}

/// `DELETE /v1/pro/tips/goal`: the goal page goes, with what it counted.
pub async fn delete_goal(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    st.db.delete_tip_goal(&ctx.install).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    overlay: Option<String>,
}

/// `GET /{handle}/goal` on the tip host.
pub async fn tip_host_goal(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if !super::on_tip_host(&st, &headers) {
        return super::not_found();
    }
    page_for(&st, &handle, q.overlay.is_some_and(|o| o == "1"), None)
}

/// `GET /t/{handle}/goal`: the public goal page (`?overlay=1` for OBS). No scripts.
pub async fn page(State(st): State<AppState>, Path(handle): Path<String>, Query(q): Query<PageQuery>) -> Response {
    page_for(&st, &handle, q.overlay.is_some_and(|o| o == "1"), None)
}

/// The progress numbers for a page: percent (0–100), raised and target as text.
pub(crate) struct Progress {
    pub pct: i64,
    pub raised: String,
    pub target: String,
}

pub(crate) fn progress(st: &AppState, g: &Goal) -> Option<Progress> {
    let (raised, _) = st.db.tip_goal_raised(&g.id).ok()?;
    let pct = if g.target > 0 { (raised.saturating_mul(100) / g.target).clamp(0, 100) } else { 0 };
    Some(Progress { pct, raised: money(raised, &g.currency), target: money(g.target, &g.currency) })
}

pub(crate) fn bar(pct: i64, label: &str) -> String {
    format!(
        "<div class=bar role=progressbar aria-label=\"{}\" aria-valuemin=0 aria-valuemax=100 aria-valuenow={pct}><span style=\"width:{pct}%\"></span></div>",
        esc(label)
    )
}

/// The goal page for `handle`; `tip_href` is where its Tip button goes (default: the tip
/// page next to it, `../<handle>`; `/` on a custom address).
pub(crate) fn page_for(st: &AppState, handle: &str, overlay: bool, tip_href: Option<&str>) -> Response {
    let handle = handle.to_lowercase();
    let found = st.db.tip_install_by_handle(&handle).ok().flatten().and_then(|(install, cfg)| {
        let cfg = serde_json::from_str::<TipConfig>(&cfg).ok()?;
        if !super::install_is_pro(st, &install) {
            return None;
        }
        let goal = st.db.tip_goal(&install).ok().flatten()?;
        Some((install, cfg, goal))
    });
    let Some((install, cfg, goal)) = found else {
        let body = "<p class=lead>There's no goal page here.</p>";
        let mut res = (StatusCode::NOT_FOUND, Html(super::shell("Not found", body))).into_response();
        super::page_headers(&mut res);
        return res;
    };
    let brand = st.db.tip_brand(&install).ok().flatten().unwrap_or_default();
    let palette: Palette = brand.palette();
    let Some(p) = progress(st, &goal) else {
        return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The goal can't be read right now.")
            .into_response();
    };
    let now = now_ms();
    let when = match goal.ends_at {
        Some(e) if goal.ended(now) => format!("<p class=muted>Ended {}</p>", crate::billing::date(e)),
        Some(e) => format!("<p class=muted>Ends {}</p>", crate::billing::date(e)),
        None => String::new(),
    };
    let title = esc(&goal.title);
    let numbers = format!(
        "<p class=nums><strong>{}</strong> <span class=muted>of {}</span> <span class=pct>{}%</span></p>",
        esc(&p.raised),
        esc(&p.target),
        p.pct
    );
    let body = if overlay {
        format!("<div class=ov><p class=ov-title>{title}</p>{}{numbers}</div>", bar(p.pct, &goal.title))
    } else {
        let avatar = brand
            .avatar
            .as_deref()
            .map(|a| format!("<img class=avatar src=\"{}\" alt=\"\" width=96 height=96>", super::brand::asset_path(a)))
            .unwrap_or_default();
        format!(
            "{avatar}<p class=eyebrow>{name}'s goal</p><h1>{title}</h1>{}{numbers}{when}\
             <div class=buttons><a class=\"btn primary\" href=\"{tip}\">Tip {name}</a></div>\
             <p class=fine>Counts verified tips in {cur} since {since}. Tips in other currencies aren't counted. Refunds come off.</p>",
            bar(p.pct, &goal.title),
            name = esc(&cfg.display_name),
            tip = esc(&tip_href.map_or_else(|| format!("../{handle}"), str::to_string)),
            cur = esc(&goal.currency),
            since = crate::billing::date(goal.starts_at),
        )
    };
    let head = if overlay { format!("<meta http-equiv=refresh content={OVERLAY_REFRESH_S}>") } else { String::new() };
    let html = super::shell_with(&format!("{} · {}", goal.title, cfg.display_name), &body, &palette, &head, overlay);
    let mut res = Html(html).into_response();
    super::page_headers(&mut res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_reads_like_people_write_it() {
        assert_eq!(money(250_000, "INR"), "₹2,500");
        assert_eq!(money(100_000_000, "INR"), "₹10,00,000");
        assert_eq!(money(1_250, "USD"), "$12.50");
        assert_eq!(money(123_456_789, "USD"), "$1,234,567.89");
        assert_eq!(money(500, "JPY"), "¥500");
        assert_eq!(money(4_000, "CHF"), "CHF 40");
        assert_eq!(money(1_500, "KWD"), "KWD 1.500");
        assert_eq!(money(0, "INR"), "₹0");
    }

    #[test]
    fn amounts_convert_between_digit_counts() {
        assert_eq!(decimal_minor("3.00", 2), Some(300));
        assert_eq!(decimal_minor("3.5", 2), Some(350));
        assert_eq!(decimal_minor("1,000.25", 2), Some(100_025));
        assert_eq!(decimal_minor("500", 0), Some(500));
        assert_eq!(decimal_minor("12.999", 2), Some(1299), "extra digits are cut, never rounded up");
        for bad in ["", "-3", "3.x", "abc", ".5", "1e3"] {
            assert_eq!(decimal_minor(bad, 2), None, "{bad}");
        }
        // Stripe sends ISK in hundredths; the goal counts whole krónur.
        assert_eq!(rescale(10_000, 2, 0), Some(100));
        assert_eq!(rescale(100, 0, 2), Some(10_000));
        assert_eq!(rescale(i64::MAX, 0, 3), None);
    }

    #[test]
    fn payment_hashes_are_per_install_and_provider() {
        let a = payment_hash("i1", "razorpay", "pay_1");
        assert_eq!(a.len(), 64);
        assert_ne!(a, payment_hash("i2", "razorpay", "pay_1"));
        assert_ne!(a, payment_hash("i1", "stripe", "pay_1"));
        assert!(!a.contains("pay_1"));
    }
}
