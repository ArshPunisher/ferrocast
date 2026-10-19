//! Ferrocast Pro F7, the weekly email (docs/plan/PRO.md F7, milestone M11; API.md §9.8).
//!
//! A short report of the week's streams, built from the stored stream history and sent
//! through Brevo behind a [`Mailer`] (a [`FakeMailer`] in tests).
//!
//! - **Double opt-in.** `PUT /v1/pro/history/email` seals the address with the vault and
//!   sends a confirm email; its link (signed, 24 h, single use) opens a script-free page
//!   whose button POSTs the confirm, so a mail scanner fetching the link confirms nothing.
//!   A confirmed address keeps getting the report while a new one waits for its confirm;
//!   confirming swaps them.
//! - **Stored.** The address only sealed (`(key_v, nonce, ct)`, AAD `sub_id|mail_address`),
//!   never in plain text and never logged; the confirm link's nonce only as a SHA-256; per
//!   (subscription, week) when the report went. Deleted on Stop emails / unsubscribe, a week
//!   after an unconfirmed link expired, or 30 days after Pro ended (the hourly sweep).
//! - **One-click unsubscribe** (RFC 8058): every report has `List-Unsubscribe` and
//!   `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, and a link to a page with a button.
//! - **When.** Each subscription has a day, `SHA-256(sub_id) % 7` (0 = Monday), at
//!   [`SEND_HOUR`] UTC; the report covers the seven days before that moment. A week without
//!   a stream sends nothing. `mail_sent` makes it once per (subscription, week), even across
//!   restarts.
//! - **Budget.** At most `MAIL_DAILY_BUDGET` emails a UTC day relay-wide (default
//!   [`DAILY_BUDGET`], under Brevo's free 300), confirms and reports together.
//!
//! `features.weekly_email` is true with `BREVO_API_KEY`, the vault and a link key
//! (`MAIL_TOKEN_KEY`, else derived from `RELAY_VAULT_KEY`).

mod brevo;
mod fake;
mod mailer;
mod pages;
pub mod report;
mod store;
#[cfg(test)]
mod tests;
mod weekly;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rsa::sha2::{Digest, Sha256};
use serde::Deserialize;
use serde_json::{Value, json};

pub use brevo::BrevoMailer;
pub use fake::FakeMailer;
pub use mailer::{DEFAULT_FROM, Email, MailError, Mailer, SendFuture, Sender};
pub(crate) use store::SCHEMA;
pub use weekly::{RunStats, run_weekly, sweep};

use crate::billing::gate::{pro_required, require_pro};
use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, authed, now_ms};

/// The vault purpose (AAD) of a sealed address.
pub const PURPOSE: &str = "mail_address";
/// A confirm link works this long.
pub const CONFIRM_TTL_MS: i64 = 86_400_000;
/// Confirm emails per subscription per UTC day (resends included).
pub const CONFIRMS: Limit = Limit { name: "mail_confirms", period: Period::Day, limit: 3 };
/// The hour (UTC) the weekly report goes out on each subscription's day: 09:30 in India.
pub const SEND_HOUR: i64 = 4;
/// A report whose moment was missed (the relay was down) still goes this long after it.
pub const CATCH_UP_MS: i64 = 2 * 86_400_000;
/// Emails a UTC day, relay-wide (confirms and reports), unless `MAIL_DAILY_BUDGET` says
/// otherwise: under Brevo's free plan's 300.
pub const DAILY_BUDGET: i64 = 270;
/// Brevo's free plan: a `MAIL_DAILY_BUDGET` above it is used but warned about.
const FREE_PLAN_DAILY: i64 = 300;
/// One send, start to end.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const DAY_MS: i64 = 86_400_000;
const WEEK_MS: i64 = 7 * DAY_MS;

/// The sender, the key that signs links and the daily budget. `Debug` never shows the key.
#[derive(Clone)]
pub struct MailConfig {
    pub from: Sender,
    key: Option<[u8; 32]>,
    /// Emails a UTC day, relay-wide.
    pub budget: i64,
}

impl std::fmt::Debug for MailConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailConfig")
            .field("from", &self.from)
            .field("key", &self.key.map(|_| "<redacted>"))
            .field("budget", &self.budget)
            .finish()
    }
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig::new(None, None)
    }
}

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key");
    m.update(msg);
    m.finalize().into_bytes().into()
}

impl MailConfig {
    /// `from`: `MAIL_FROM` (default [`DEFAULT_FROM`]); `secret`: any secret bytes (16+).
    pub fn new(from: Option<&str>, secret: Option<&[u8]>) -> MailConfig {
        let from = from.and_then(Sender::parse).unwrap_or_else(|| {
            if from.is_some() {
                tracing::warn!("MAIL_FROM isn't an address; using the default sender");
            }
            #[allow(clippy::expect_used)] // a constant that parses (tested)
            Sender::parse(DEFAULT_FROM).expect("the default sender parses")
        });
        MailConfig {
            from,
            key: secret.filter(|s| s.len() >= 16).map(|s| hmac(s, b"ferrocast mail links v1")),
            budget: DAILY_BUDGET,
        }
    }

    /// The same, with `budget` emails a day.
    pub fn with_budget(mut self, budget: i64) -> MailConfig {
        self.budget = budget.max(0);
        self
    }

    /// `MAIL_FROM`, and `MAIL_TOKEN_KEY` (else the bytes of `RELAY_VAULT_KEY`) for links.
    pub fn from_env() -> MailConfig {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let secret: Option<Vec<u8>> = env("MAIL_TOKEN_KEY")
            .map(String::into_bytes)
            .or_else(|| env("RELAY_VAULT_KEY").and_then(|k| base64::engine::general_purpose::STANDARD.decode(k).ok()));
        let budget = match env("MAIL_DAILY_BUDGET").map(|v| v.parse::<i64>()) {
            None => DAILY_BUDGET,
            Some(Ok(n)) if n >= 1 => {
                if n > FREE_PLAN_DAILY {
                    tracing::warn!(
                        budget = n,
                        "MAIL_DAILY_BUDGET is above Brevo's free 300 a day: make sure the plan allows it"
                    );
                }
                n
            }
            Some(_) => {
                tracing::warn!("MAIL_DAILY_BUDGET isn't a whole number above 0; using {DAILY_BUDGET}");
                DAILY_BUDGET
            }
        };
        MailConfig::new(env("MAIL_FROM").as_deref(), secret.as_deref()).with_budget(budget)
    }

    fn mac(&self, payload: &str) -> Option<String> {
        self.key.map(|k| hmac(&k, payload.as_bytes()).iter().map(|b| format!("{b:02x}")).collect())
    }

    /// `c1.<sub>.<expires>.<nonce>.<mac>` and the nonce's hash to store.
    fn confirm_token(&self, sub: &str, expires: i64) -> Option<(String, String)> {
        let nonce = crate::random(18);
        let payload = format!("c1.{sub}.{expires}.{nonce}");
        let mac = self.mac(&payload)?;
        Some((format!("{payload}.{mac}"), sha_hex(&nonce)))
    }

    /// The tag an address is matched by: HMAC of the normalized address (lowercased, so
    /// `A@x.com` and `a@x.com` are one), 32 hex characters. The same address always has the
    /// same tag, so saving it again keeps old unsubscribe links working.
    pub(crate) fn addr_tag(&self, address: &str) -> Option<String> {
        let k = self.key?;
        let m = hmac(&k, format!("addr|{}", address.trim().to_lowercase()).as_bytes());
        Some(m[..16].iter().map(|b| format!("{b:02x}")).collect())
    }

    /// `u1.<sub>.<address tag>.<mac>`: no expiry (it's in every report); it only ever
    /// removes the address it was sent to.
    pub(crate) fn unsubscribe_token(&self, sub: &str, tag: &str) -> Option<String> {
        let payload = format!("u1.{sub}.{tag}");
        Some(format!("{payload}.{}", self.mac(&payload)?))
    }

    /// The parts of a token whose signature is right, after its kind (`c1`/`u1`).
    fn check<'a>(&self, raw: &'a str, kind: &str) -> Option<Vec<&'a str>> {
        let raw = raw.trim();
        if raw.len() > 300 {
            return None;
        }
        let (payload, mac) = raw.rsplit_once('.')?;
        let want = self.mac(payload)?;
        use subtle::ConstantTimeEq;
        if !bool::from(want.as_bytes().ct_eq(mac.as_bytes())) {
            return None;
        }
        let mut parts = payload.split('.');
        (parts.next()? == kind).then(|| parts.collect())
    }
}

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A confirm link, checked: the subscription, its expiry and the nonce's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmLink {
    Good { sub: String, hash: String },
    Expired,
    Bad,
}

pub(crate) fn read_confirm(cfg: &MailConfig, raw: &str, now: i64) -> ConfirmLink {
    let Some(p) = cfg.check(raw, "c1") else { return ConfirmLink::Bad };
    let [sub, exp, nonce] = p.as_slice() else { return ConfirmLink::Bad };
    let Ok(exp) = exp.parse::<i64>() else { return ConfirmLink::Bad };
    if !sub_ok(sub) {
        return ConfirmLink::Bad;
    }
    if now > exp {
        return ConfirmLink::Expired;
    }
    ConfirmLink::Good { sub: (*sub).to_string(), hash: sha_hex(nonce) }
}

/// An unsubscribe link, checked: the subscription and its address's tag.
pub(crate) fn read_unsubscribe(cfg: &MailConfig, raw: &str) -> Option<(String, String)> {
    let p = cfg.check(raw, "u1")?;
    let [sub, tag] = p.as_slice() else { return None };
    if !sub_ok(sub) || tag.len() != 32 || !tag.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(((*sub).to_string(), (*tag).to_string()))
}

/// Razorpay subscription ids: letters, digits and `_` (never a `.` or `|`).
fn sub_ok(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The mail sender and its settings, shared by every request.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    mailer: Option<Arc<dyn Mailer>>,
    cfg: MailConfig,
}

impl Runtime {
    pub(crate) fn new(mailer: Option<Arc<dyn Mailer>>, cfg: MailConfig) -> Runtime {
        Runtime { mailer, cfg }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/history/email", get(view).put(save).delete(stop))
        .route("/mail/confirm", get(pages::confirm_page).post(pages::confirm_post))
        .route("/mail/unsubscribe", get(pages::unsubscribe_page).post(pages::unsubscribe_post))
}

/// `features.weekly_email`: a mail sender (`BREVO_API_KEY`), the vault and a link key.
pub(crate) fn available(st: &AppState) -> bool {
    st.mail.mailer.is_some() && st.mail.cfg.key.is_some() && st.vault().is_ok()
}

/// Not a quota (it's a resend limit), so not in `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[];

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "mail_unavailable",
        "Weekly emails aren't switched on on Ferrocast's servers yet.",
    )
    .hint("Nothing to do on your side; try again later. Your history is still kept.")
}

fn mail_busy() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "mail_busy", "Ferrocast has sent all the email it can for today.")
        .hint("Try again tomorrow.")
}

fn mail_failed() -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "mail_failed", "The confirm email couldn't be sent right now.")
        .hint("Try again in a few minutes.")
}

fn invalid_email(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email", message.to_string())
        .hint("Check it for typos, or use another address.")
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", message)
}

/// The JSON body, read as bytes so the Pro gate answers before the body is judged.
#[derive(Debug)]
struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Send just the address.")
            } else {
                bad_request("The request body couldn't be read.")
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct SaveIn {
    address: String,
}

fn parse(headers: &HeaderMap, body: &[u8]) -> Result<SaveIn, ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if !json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the address as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<SaveIn>(body).map_err(|e| {
        let what = if e.is_syntax() || e.is_eof() { "isn't JSON" } else { "needs an address" };
        bad_request(format!("The request {what}."))
    })
}

/// Trimmed, the domain lowercased (the part before `@` is the mail server's business).
pub(crate) fn normalize(address: &str) -> Option<String> {
    let a = address.trim();
    if !crate::billing::valid_email(a) || a.len() > 254 {
        return None;
    }
    let (local, domain) = a.rsplit_once('@')?;
    Some(format!("{local}@{}", domain.to_ascii_lowercase()))
}

/// `a•••@gmail.com`: enough to recognise it, not enough to read it.
pub fn address_hint(address: &str) -> String {
    let (local, domain) = address.rsplit_once('@').unwrap_or((address, ""));
    let first: String = local.chars().take(1).collect();
    format!("{first}•••@{domain}")
}

// ── Schedule ─────────────────────────────────────────────────────────────────────────

/// This subscription's day, 0 = Monday: SHA-256 of the id, so it stays the same across
/// restarts and relays (std's hasher is seeded randomly per process).
pub fn send_day(sub: &str) -> i64 {
    let d = Sha256::digest(sub.as_bytes());
    let n = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    i64::try_from(n % 7).unwrap_or(0)
}

/// When this week's report is due for `sub` (the week `now` is in, Monday 00:00 UTC first).
pub fn due_at(sub: &str, now: i64) -> i64 {
    crate::history::week_start(now) + send_day(sub) * DAY_MS + SEND_HOUR * 3_600_000
}

/// The report moments a run at `now` may still send: this week's and last week's, each from
/// its moment until [`CATCH_UP_MS`] after it (a Sunday report missed by a Monday-morning
/// outage still goes, in last week's slot).
pub(crate) fn due_moments(sub: &str, now: i64) -> impl Iterator<Item = i64> {
    let this = due_at(sub, now);
    [this - WEEK_MS, this].into_iter().filter(move |due| *due <= now && now <= due + CATCH_UP_MS)
}

/// The next report moment that may still go: one still in its catch-up window and not
/// looked at yet, else the next one ahead. Moments before the address was confirmed don't
/// count (its first report is the one after).
fn next_at(st: &AppState, sub: &str, confirmed_at: Option<i64>, now: i64) -> i64 {
    let this = due_at(sub, now);
    for due in [this - WEEK_MS, this, this + WEEK_MS] {
        if now > due + CATCH_UP_MS || confirmed_at.is_some_and(|c| c > due) {
            continue;
        }
        if due <= now && st.db.mail_sent_at(sub, crate::history::week_start(due)).ok().flatten().is_some() {
            continue;
        }
        return due;
    }
    this + 2 * WEEK_MS
}

/// The UTC day number the daily budget counts in.
pub(crate) fn budget_day(now: i64) -> i64 {
    now.div_euclid(DAY_MS)
}

const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

pub fn day_name(sub: &str) -> &'static str {
    usize::try_from(send_day(sub)).ok().and_then(|i| DAYS.get(i)).copied().unwrap_or("Monday")
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

fn open_address(st: &AppState, sub: &str, sealed: &crate::vault::Sealed) -> Result<String, ApiError> {
    let vault = st.vault()?;
    let bytes = vault.open(sub, PURPOSE, sealed).map_err(|e| {
        tracing::error!(error = %e, "a sealed mail address didn't open");
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault_unavailable",
            "Ferrocast couldn't read the saved address.",
        )
        .hint("Save your address again in Ferrocast › History.")
    })?;
    String::from_utf8(bytes).map_err(|_| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "vault_unavailable", "The saved address is damaged.")
            .hint("Save your address again.")
    })
}

/// `{ status, address_hint, pending_hint, expires_at, day, next_at, last_sent_at,
/// paused_reason }`. `address_hint` is the address `status` is about (the pending one when
/// `status` is `pending`); `pending_hint` a new address waiting for its confirm (also next
/// to `on`).
fn view_json(st: &AppState, sub: &str, now: i64) -> Result<Value, ApiError> {
    let Some(row) = st.db.mail_row(sub).map_err(ApiError::db)? else {
        return Ok(json!({ "status": "none", "address_hint": null, "pending_hint": null, "expires_at": null,
                          "day": day_name(sub), "next_at": null, "last_sent_at": null, "paused_reason": null }));
    };
    let hint =
        |a: Option<&store::Addr>| a.and_then(|a| open_address(st, sub, &a.sealed).ok()).map(|a| address_hint(&a));
    let status = row.view_status();
    let pending = hint(row.pending.as_ref());
    let address = if status == "pending" { pending.clone() } else { hint(row.active.as_ref()) };
    Ok(json!({
        "status": status,
        "address_hint": address,
        "pending_hint": pending,
        "expires_at": row.pending.as_ref().and(row.confirm_expires),
        "day": day_name(sub),
        "next_at": (status == "on").then(|| next_at(st, sub, row.confirmed_at, now)),
        "last_sent_at": st.db.mail_last_sent(sub).map_err(ApiError::db)?,
        "paused_reason": if status == "paused" { row.paused_reason.clone() } else { None },
    }))
}

fn no_store(v: Value, status: StatusCode) -> Response {
    let mut res = (status, axum::Json(v)).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    res
}

/// `GET /v1/pro/history/email` → the view.
async fn view(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    if !available(&st) {
        return Err(unavailable());
    }
    Ok(no_store(view_json(&st, &ctx.sub_id, now_ms())?, StatusCode::OK))
}

/// `PUT /v1/pro/history/email {address}` → 202 pending, with a confirm email sent.
async fn save(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let ctx = require_pro(&st, &headers).await?;
        let mailer = st.mail.mailer.clone().ok_or_else(unavailable)?;
        if !available(&st) {
            return Err(unavailable());
        }
        let b = parse(&headers, &body)?;
        let address = normalize(&b.address).ok_or_else(|| invalid_email("That email address doesn't look right."))?;
        let now = now_ms();
        let tag = st.mail.cfg.addr_tag(&address).ok_or_else(unavailable)?;
        if let Some(row) = st.db.mail_row(&ctx.sub_id).map_err(ApiError::db)?
            && row.on().is_some_and(|a| a.tag == tag)
        {
            return Err(ApiError::new(StatusCode::CONFLICT, "already_on", "Weekly emails already go to that address.")
                .hint("Nothing to do. To use another address, type it and press Save."));
        }
        match st.db.try_take(&ctx.sub_id, CONFIRMS.name, CONFIRMS.period, 1, CONFIRMS.limit) {
            Ok(()) => {}
            Err(TakeErr::Over(q)) => {
                return Err(q
                    .into_api("mail_rate", format!("That's {} confirm emails today, the most we send.", CONFIRMS.limit))
                    .hint(format!(
                        "Look in your spam folder for the last one, or try again after {} (UTC).",
                        crate::billing::date(q.reset_at)
                    )));
            }
            Err(TakeErr::Db(e)) => return Err(ApiError::db(e)),
        }
        let give_back = || {
            if let Err(e) = st.db.give_back(&ctx.sub_id, CONFIRMS.name, CONFIRMS.period, 1) {
                tracing::warn!(error = %e, "couldn't give back a confirm email");
            }
        };
        let day = budget_day(now);
        match st.db.mail_budget_take(day, st.mail.cfg.budget) {
            Ok(true) => {}
            Ok(false) => {
                give_back();
                return Err(mail_busy());
            }
            Err(e) => {
                give_back();
                return Err(ApiError::db(e));
            }
        }
        let give_back_all = || {
            give_back();
            if let Err(e) = st.db.mail_budget_give_back(day) {
                tracing::warn!(error = %e, "couldn't give back the daily mail budget");
            }
        };
        let sealed = match st.vault()?.seal(&ctx.sub_id, PURPOSE, address.as_bytes()) {
            Ok(s) => s,
            Err(e) => {
                give_back_all();
                return Err(crate::vault::damaged(&e));
            }
        };
        let expires = now + CONFIRM_TTL_MS;
        let Some((token, hash)) = st.mail.cfg.confirm_token(&ctx.sub_id, expires) else {
            give_back_all();
            return Err(unavailable());
        };
        let link = format!("{}/mail/confirm?t={token}", st.cfg.public_url);
        let r = report::confirm(&link);
        let email =
            Email { to: address, subject: r.subject, html: r.html, text: r.text, headers: Vec::new(), tag: "confirm" };
        let sent = match tokio::time::timeout(SEND_TIMEOUT, mailer.send(&st.mail.cfg.from, &email)).await {
            Ok(r) => r,
            Err(_) => Err(MailError::Uncertain("timed out".into())),
        };
        if let Err(e) = sent {
            tracing::warn!(error = %e, "a confirm email didn't go");
            return Err(match e {
                MailError::Rejected(_) => {
                    give_back_all();
                    invalid_email("That address can't get email from us.")
                }
                MailError::Misconfigured(_) => {
                    give_back_all();
                    tracing::error!(
                        "the mail provider refused the relay's account: check BREVO_API_KEY and the sender"
                    );
                    unavailable()
                }
                MailError::Limited(_) => {
                    give_back();
                    if let Err(e) = st.db.mail_budget_exhaust(day, st.mail.cfg.budget) {
                        tracing::warn!(error = %e, "couldn't stop today's mail budget");
                    }
                    tracing::error!("the mail provider's sending limit is reached: no more email today");
                    mail_busy()
                }
                MailError::Unavailable(_) => {
                    give_back_all();
                    mail_failed()
                }
                // It may have gone: it counts against the day's budget, not the person's three.
                MailError::Uncertain(_) => {
                    give_back();
                    mail_failed()
                }
            });
        }
        // Saved only once the email went, so a failure never replaces an address that works;
        // a confirmed address keeps working until this one is confirmed.
        let pending = store::Addr { sealed, tag };
        st.db.mail_put_pending(&ctx.sub_id, &pending, &hash, expires, now).map_err(ApiError::db)?;
        Ok::<_, ApiError>(no_store(view_json(&st, &ctx.sub_id, now)?, StatusCode::ACCEPTED))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

/// `DELETE /v1/pro/history/email` → 204. Open after Pro ends (it only removes data):
/// 402 only without any subscription.
async fn stop(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?.ok_or_else(pro_required)?;
    st.db.mail_delete(&sub.id).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
