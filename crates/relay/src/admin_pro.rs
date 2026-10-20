//! The Control Center's Ferrocast Pro tabs: subscribers, subscriptions, payments, free Pro
//! grants, codes, prices and sales (`/admin/v1/…`, contract in docs/ADMIN-API.md).
//!
//! Same guard as the stats (`admin::guard`: Docker network only, bearer token, 60 a minute);
//! every write also needs `X-Admin-Email` and is written to `admin_audit`. Never an email (the
//! relay has only a keyed hash) and install ids only masked.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::billing::access::{self, Plan};
use crate::billing::codes::{self, CodeRow};
use crate::billing::comp::{self, CompError, GrantBy};
use crate::billing::pricing::{self, SaleRow};
use crate::{ApiError, AppState, now_ms};

const DAY_MS: i64 = 86_400_000;
const STATUSES: [&str; 9] =
    ["created", "authenticated", "active", "pending", "halted", "paused", "cancelled", "completed", "expired"];

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post, put};
    axum::Router::new()
        .route("/admin/v1/users", get(users))
        .route("/admin/v1/users/{id}", get(user))
        .route("/admin/v1/subscriptions", get(subscriptions))
        .route("/admin/v1/payments", get(payments))
        .route("/admin/v1/grants", get(grants).post(grant))
        .route("/admin/v1/grants/{id}/revoke", post(revoke))
        .route("/admin/v1/entitlements/{id}/revoke", post(revoke))
        .route("/admin/v1/codes", get(list_codes).post(create_code))
        .route("/admin/v1/codes/{code}/disable", post(disable_code))
        .route("/admin/v1/pricing", get(get_pricing))
        .route("/admin/v1/prices/{plan}/{period}/{currency}", put(put_price))
        .route("/admin/v1/sales", post(create_sale))
        .route("/admin/v1/sales/{id}", put(update_sale))
        .route("/admin/v1/sales/{id}/stop", post(stop_sale))
        .route("/admin/v1/audit", get(audit))
}

pub(crate) type Peer = ConnectInfo<std::net::SocketAddr>;
type Q = Query<HashMap<String, String>>;

// ── Helpers ──────────────────────────────────────────────────────────────────────────

pub(crate) fn e(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

pub(crate) fn bad(code: &'static str, message: impl Into<String>) -> ApiError {
    e(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

pub(crate) fn not_found(what: &str) -> ApiError {
    e(StatusCode::NOT_FOUND, "not_found", format!("No {what} with that id."))
}

fn db(err: rusqlite::Error) -> ApiError {
    ApiError::db(err)
}

/// Guard for a read; the time now.
pub(crate) fn reader(st: &AppState, peer: Peer, headers: &HeaderMap) -> Result<i64, ApiError> {
    let now = now_ms();
    crate::admin::guard(st, peer.0, headers, now)?;
    Ok(now)
}

/// Guard for a write; the time now and the admin's email.
pub(crate) fn writer(st: &AppState, peer: Peer, headers: &HeaderMap) -> Result<(i64, String), ApiError> {
    let now = reader(st, peer, headers)?;
    let actor = headers
        .get("x-admin-email")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| {
            (3..=320).contains(&v.len())
                && v.contains('@')
                && !v.starts_with('@')
                && !v.ends_with('@')
                && !v.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .ok_or_else(|| {
            e(
                StatusCode::BAD_REQUEST,
                "admin_email_required",
                "X-Admin-Email is required (the admin making the change).",
            )
            .hint("Send the signed-in admin's email in the X-Admin-Email header.")
        })?;
    Ok((now, actor))
}

pub(crate) fn body(headers: &HeaderMap, bytes: &[u8]) -> Result<Value, ApiError> {
    if let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        && !ct.trim_start().to_ascii_lowercase().starts_with("application/json")
    {
        return Err(e(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the request as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| e(StatusCode::BAD_REQUEST, "bad_json", "The request body isn't a JSON object."))
}

pub(crate) fn reply(status: StatusCode, v: Value) -> Response {
    let mut r = (status, axum::Json(v)).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub(crate) fn ok(v: Value) -> Result<Response, ApiError> {
    Ok(reply(StatusCode::OK, v))
}

/// Unix ms → `2026-10-20T10:00:00Z`.
pub(crate) fn iso(ms: i64) -> String {
    let (y, m, d) = crate::quota::civil(ms.div_euclid(DAY_MS));
    let s = ms.rem_euclid(DAY_MS) / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

pub(crate) fn iso_opt(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |m| json!(iso(m)))
}

/// A time in a request: null, Unix ms, RFC 3339, or (with `date_end`) a `YYYY-MM-DD` date
/// meaning the end of that UTC day. `Err` for anything else.
fn time_in(v: &Value, date_end: bool) -> Result<Option<i64>, ()> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_i64().filter(|n| *n > 0).map(Some).ok_or(()),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => {
            let s = s.trim();
            if let Some(ms) = crate::kick::rfc3339_ms(s) {
                return Ok(Some(ms));
            }
            let mut p = s.split('-');
            let (Some(y), Some(m), Some(d), None) = (p.next(), p.next(), p.next(), p.next()) else { return Err(()) };
            let (y, m, d): (i64, i64, i64) =
                (y.parse().map_err(|_| ())?, m.parse().map_err(|_| ())?, d.parse().map_err(|_| ())?);
            if !date_end || !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 2000 {
                return Err(());
            }
            Ok(Some((crate::quota::days_from_civil(y, m, d) + 1) * DAY_MS - 1000))
        }
        _ => Err(()),
    }
}

/// `AbC1…`: enough to tell installs apart, not enough to use one.
pub(crate) fn mask(install: &str) -> String {
    let head: String = install.chars().take(4).collect();
    if head.is_empty() { String::new() } else { format!("{head}…") }
}

pub(crate) fn int(
    v: &Value,
    field: &str,
    code: &'static str,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Option<i64>, ApiError> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => match n.as_i64() {
            Some(x) if range.contains(&x) => Ok(Some(x)),
            _ => Err(bad(code, format!("{field} must be a whole number from {} to {}.", range.start(), range.end()))),
        },
        _ => Err(bad(code, format!("{field} must be a whole number from {} to {}.", range.start(), range.end()))),
    }
}

pub(crate) fn text(v: &Value, field: &str, max: usize) -> Result<Option<String>, ApiError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => {
            let s = s.trim();
            if s.chars().count() > max {
                return Err(bad("invalid_text", format!("{field} is longer than {max} characters.")));
            }
            Ok((!s.is_empty()).then(|| s.to_string()))
        }
        _ => Err(bad("invalid_text", format!("{field} must be text."))),
    }
}

pub(crate) fn audit_log(st: &AppState, now: i64, actor: &str, action: &str, target: &str, detail: Value) {
    let r = st.db.c().execute(
        "INSERT INTO admin_audit(at, actor, action, target, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now, actor, action, target, detail.to_string()],
    );
    if let Err(err) = r {
        tracing::error!(error = %err, action, "couldn't write the admin audit row");
    }
    tracing::info!(actor, action, target, "admin change");
}

// ── Subscribers ──────────────────────────────────────────────────────────────────────

/// One `pro_subs` row with what the tabs show about it.
struct Sub {
    id: String,
    plan: String,
    status: String,
    current_end: Option<i64>,
    cancel_at_end: bool,
    refunded_at: Option<i64>,
    created_at: i64,
    email_on_file: bool,
    installs: i64,
    amount: Option<i64>,
    list_amount: Option<i64>,
    code: Option<String>,
    sale_id: Option<String>,
}

const SUB_COLS: &str = "s.id, s.plan, s.status, s.current_end, s.cancel_at_end, s.refunded_at, s.created_at,
    s.email_hash <> '', (SELECT COUNT(*) FROM pro_installs i WHERE i.sub_id = s.id),
    p.amount, p.list_amount, p.code, p.sale_id,
    (SELECT amount FROM pro_payments x WHERE x.sub_id = s.id ORDER BY x.at LIMIT 1)";
const SUB_FROM: &str = "FROM pro_subs s LEFT JOIN pro_sub_prices p ON p.sub_id = s.id";

fn sub_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Sub> {
    let plan: String = r.get(1)?;
    let stored: Option<i64> = r.get(9)?;
    let first_paid: Option<i64> = r.get(13)?;
    let default = Plan::parse(&plan).map(Plan::amount_paise);
    Ok(Sub {
        id: r.get(0)?,
        status: r.get(2)?,
        current_end: r.get(3)?,
        cancel_at_end: r.get::<_, i64>(4)? != 0,
        refunded_at: r.get(5)?,
        created_at: r.get(6)?,
        email_on_file: r.get(7)?,
        installs: r.get(8)?,
        amount: stored.or(first_paid).or(default),
        list_amount: r.get::<_, Option<i64>>(10)?.or(default),
        code: r.get(11)?,
        sale_id: r.get(12)?,
        plan,
    })
}

impl Sub {
    fn grant(&self) -> bool {
        comp::is_comp(&self.id)
    }
    fn state(&self) -> access::SubState {
        access::SubState {
            status: self.status.clone(),
            current_end: self.current_end,
            cancel_at_end: self.cancel_at_end,
            refunded: self.refunded_at.is_some(),
        }
    }
    fn period(&self) -> Value {
        Plan::parse(&self.plan).map_or(Value::Null, |p| json!(p.key()))
    }

    fn user(&self, now: i64) -> Value {
        let a = self.state();
        let pro = access::is_pro(&a, now);
        json!({
            "id": self.id, "email": null, "email_on_file": self.email_on_file, "display_name": null,
            "auth_provider": if self.grant() { "grant" } else { "razorpay" },
            "source": if self.grant() { "grant" } else { "paid" },
            "plan": if pro { "pro" } else { "free" }, "pro": pro,
            "period": self.period(), "status": self.status,
            "amount": if self.grant() { None } else { self.amount }, "currency": "INR",
            "current_end": iso_opt(self.current_end), "access_until": iso_opt(access::access_until(&a)),
            "cancel_at_period_end": self.cancel_at_end, "refunded_at": iso_opt(self.refunded_at),
            "created_at": iso(self.created_at), "jobs": 0, "accounts": self.installs, "installs": self.installs,
        })
    }

    fn subscription_row(&self) -> Value {
        json!({
            "gateway_subscription_id": self.id, "plan": "pro", "period": self.period(), "status": self.status,
            "amount": self.amount, "list_amount": self.list_amount, "currency": "INR",
            "current_end": iso_opt(self.current_end), "access_until": iso_opt(access::access_until(&self.state())),
            "cancel_at_period_end": self.cancel_at_end, "refunded_at": iso_opt(self.refunded_at),
            "code": self.code, "sale_id": self.sale_id, "installs": self.installs, "created_at": iso(self.created_at),
        })
    }
}

pub(crate) fn page_arg(
    q: &HashMap<String, String>,
    k: &str,
    default: i64,
    range: std::ops::RangeInclusive<i64>,
) -> Result<i64, ApiError> {
    match q.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(v) => v.parse::<i64>().ok().filter(|n| range.contains(n)).ok_or_else(|| {
            e(
                StatusCode::BAD_REQUEST,
                "bad_query",
                format!("{k} must be a number from {} to {}.", range.start(), range.end()),
            )
        }),
    }
}

/// `GET /admin/v1/users?q=&source=&page=&per_page=`
async fn users(State(st): State<AppState>, peer: Peer, headers: HeaderMap, Query(q): Q) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let page = page_arg(&q, "page", 1, 1..=100_000)?;
    let per_page = page_arg(&q, "per_page", 50, 1..=100)?;
    let search = q.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s.len() <= 128);
    let source = match q.get("source").map(String::as_str).filter(|s| !s.is_empty()) {
        None => "",
        Some("paid") => " AND s.id NOT LIKE 'comp\\_%' ESCAPE '\\'",
        Some("grant") => " AND s.id LIKE 'comp\\_%' ESCAPE '\\'",
        Some(_) => return Err(e(StatusCode::BAD_REQUEST, "bad_query", "source must be paid or grant.")),
    };
    let filter = format!(
        "WHERE s.status <> 'created'{source} AND (?1 IS NULL OR instr(lower(s.id), lower(?1)) > 0
           OR s.id IN (SELECT sub_id FROM pro_install_subs WHERE install_id = ?1))"
    );
    let c = st.db.c();
    let total: i64 =
        c.query_row(&format!("SELECT COUNT(*) {SUB_FROM} {filter}"), [&search], |r| r.get(0)).map_err(db)?;
    let mut stmt = c
        .prepare(&format!("SELECT {SUB_COLS} {SUB_FROM} {filter} ORDER BY s.created_at DESC, s.id LIMIT ?2 OFFSET ?3"))
        .map_err(db)?;
    let items = stmt
        .query_map(params![search, per_page, (page - 1) * per_page], sub_row)
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(db)?;
    ok(json!({ "total": total, "page": page, "per_page": per_page,
               "items": items.iter().map(|s| s.user(now)).collect::<Vec<_>>() }))
}

pub(crate) fn grant_history(c: &rusqlite::Connection, sub: &str) -> rusqlite::Result<Vec<Value>> {
    let mut q = c.prepare(
        "SELECT kind, source, code, actor, note, days, until, at FROM pro_grants WHERE sub_id = ?1 ORDER BY at, id",
    )?;
    let rows = q.query_map([sub], |r| {
        Ok(json!({
            "kind": r.get::<_, String>(0)?, "source": r.get::<_, String>(1)?, "code": r.get::<_, Option<String>>(2)?,
            "actor": r.get::<_, Option<String>>(3)?, "note": r.get::<_, Option<String>>(4)?,
            "days": r.get::<_, i64>(5)?, "until": iso(r.get(6)?), "at": iso(r.get(7)?),
        }))
    })?;
    rows.collect()
}

/// A grant as the Give Pro tab lists it.
fn grant_json(c: &rusqlite::Connection, s: &Sub, now: i64) -> rusqlite::Result<Value> {
    let history = grant_history(c, &s.id)?;
    let last_grant = history.iter().rev().find(|h| h["kind"] == "grant");
    let last_revoke = history.iter().rev().find(|h| h["kind"] == "revoke");
    let install: Option<String> = c
        .query_row(
            "SELECT install_id FROM pro_install_subs WHERE sub_id = ?1 ORDER BY linked_at DESC LIMIT 1",
            [&s.id],
            |r| r.get(0),
        )
        .optional()?;
    let revoked = (s.status == "cancelled").then(|| last_revoke.map(|r| r["at"].clone())).flatten();
    Ok(json!({
        "id": s.id, "plan": "pro",
        "source": last_grant.map_or(json!("unknown"), |g| g["source"].clone()),
        "code": last_grant.map_or(Value::Null, |g| g["code"].clone()),
        "created_by": last_grant.map_or(Value::Null, |g| g["actor"].clone()),
        "note": last_grant.map_or(Value::Null, |g| g["note"].clone()),
        "install": install.as_deref().map(mask),
        "active": access::is_pro(&s.state(), now),
        "starts_at": iso(s.created_at), "ends_at": iso_opt(s.current_end),
        "revoked_at": revoked.unwrap_or(Value::Null), "created_at": iso(s.created_at),
        "history": history,
    }))
}

fn load_sub(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Sub>> {
    c.query_row(&format!("SELECT {SUB_COLS} {SUB_FROM} WHERE s.id = ?1"), [id], sub_row).optional()
}

/// `GET /admin/v1/users/{id}`
async fn user(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let c = st.db.c();
    let Some(s) = load_sub(&c, &id).map_err(db)? else { return Err(not_found("subscriber")) };
    let read = || -> rusqlite::Result<Value> {
        let mut q = c.prepare(
            "SELECT p.payment_id, p.amount, p.at,
                COALESCE((SELECT SUM(amount) FROM pro_refunds r WHERE r.payment_id = p.payment_id AND r.status = 'processed'), 0),
                (SELECT COUNT(*) FROM pro_refunds r WHERE r.payment_id = p.payment_id AND r.status = 'pending')
               FROM pro_payments p WHERE p.sub_id = ?1 ORDER BY p.at DESC",
        )?;
        let payments = q
            .query_map([&s.id], |r| {
                Ok(json!({ "payment_id": r.get::<_, String>(0)?, "amount": r.get::<_, i64>(1)?, "currency": "INR",
                           "paid_at": iso(r.get(2)?), "refunded": r.get::<_, i64>(3)?, "pending_refunds": r.get::<_, i64>(4)? }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut q = c.prepare(
            "SELECT r.refund_id, r.payment_id, r.amount, r.status, r.at FROM pro_refunds r
               JOIN pro_payments p ON p.payment_id = r.payment_id WHERE p.sub_id = ?1 ORDER BY r.at DESC",
        )?;
        let refunds = q
            .query_map([&s.id], |r| {
                Ok(json!({ "refund_id": r.get::<_, String>(0)?, "payment_id": r.get::<_, String>(1)?,
                           "amount": r.get::<_, i64>(2)?, "status": r.get::<_, String>(3)?, "refunded_at": iso(r.get(4)?) }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut q = c.prepare(
            "SELECT h.install_id, h.linked_at, i.last_seen, EXISTS(SELECT 1 FROM pro_installs p WHERE p.install_id = h.install_id AND p.sub_id = h.sub_id)
               FROM pro_install_subs h LEFT JOIN installs i ON i.id = h.install_id WHERE h.sub_id = ?1 ORDER BY h.linked_at DESC",
        )?;
        let installs = q
            .query_map([&s.id], |r| {
                Ok(json!({ "install": mask(&r.get::<_, String>(0)?), "linked_at": iso(r.get(1)?),
                           "last_seen": iso_opt(r.get(2)?), "current": r.get::<_, bool>(3)? }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut q = c.prepare(
            "SELECT u.code, COALESCE(k.kind, ''), u.at FROM pro_code_uses u LEFT JOIN pro_codes k ON k.code = u.code
               WHERE u.sub_id = ?1 ORDER BY u.at DESC",
        )?;
        let redemptions = q
            .query_map([&s.id], |r| {
                Ok(json!({ "code": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "redeemed_at": iso(r.get(2)?) }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let a = s.state();
        let mut u = s.user(now);
        let user = json!({ "id": s.id, "email": null, "email_on_file": s.email_on_file, "display_name": null,
                           "auth_provider": u["auth_provider"].take(), "created_at": iso(s.created_at) });
        Ok(json!({
            "user": user,
            "subscription": {
                "id": s.id, "source": if s.grant() { "grant" } else { "paid" }, "pro": access::is_pro(&a, now),
                "period": s.period(), "status": s.status,
                "amount": if s.grant() { None } else { s.amount }, "list_amount": if s.grant() { None } else { s.list_amount },
                "currency": "INR", "code": s.code, "sale_id": s.sale_id,
                "current_end": iso_opt(s.current_end), "access_until": iso_opt(access::access_until(&a)),
                "cancel_at_period_end": s.cancel_at_end, "refunded_at": iso_opt(s.refunded_at), "created_at": iso(s.created_at),
            },
            "subscriptions": if s.grant() { vec![] } else { vec![s.subscription_row()] },
            "entitlements": if s.grant() { vec![grant_json(&c, &s, now)?] } else { vec![] },
            "payments": payments,
            "refunds": refunds,
            "installs": installs,
            "redemptions": redemptions,
            "team": null,
        }))
    };
    let v = read().map_err(db)?;
    ok(v)
}

/// `GET /admin/v1/subscriptions?status=`
async fn subscriptions(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Query(q): Q,
) -> Result<Response, ApiError> {
    reader(&st, peer, &headers)?;
    let status = q.get("status").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(s) = &status
        && !STATUSES.contains(&s.as_str())
    {
        return Err(e(
            StatusCode::BAD_REQUEST,
            "bad_query",
            format!("status must be one of: {}.", STATUSES.join(", ")),
        ));
    }
    let c = st.db.c();
    let mut stmt = c
        .prepare(&format!(
            "SELECT {SUB_COLS} {SUB_FROM} WHERE s.id NOT LIKE 'comp\\_%' ESCAPE '\\'
               AND (CASE WHEN ?1 IS NULL THEN s.status <> 'created' ELSE s.status = ?1 END)
             ORDER BY s.created_at DESC LIMIT 500"
        ))
        .map_err(db)?;
    let rows = stmt.query_map([&status], sub_row).and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()).map_err(db)?;
    ok(Value::Array(rows.iter().map(Sub::subscription_row).collect()))
}

/// `GET /admin/v1/payments?days=&refunded_only=`
async fn payments(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Query(q): Q,
) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let days = page_arg(&q, "days", 30, 1..=3650)?;
    let refunded_only = matches!(q.get("refunded_only").map(String::as_str), Some("1" | "true" | "yes"));
    let since = now - days * DAY_MS;
    let c = st.db.c();
    let read = || -> rusqlite::Result<Value> {
        let mut stmt = c.prepare(
            "SELECT p.payment_id, p.sub_id, s.plan, p.amount, p.at, s.refunded_at,
                COALESCE((SELECT SUM(amount) FROM pro_refunds r WHERE r.payment_id = p.payment_id AND r.status = 'processed'), 0),
                (SELECT COUNT(*) FROM pro_refunds r WHERE r.payment_id = p.payment_id AND r.status = 'pending'),
                (SELECT MAX(at) FROM pro_refunds r WHERE r.payment_id = p.payment_id)
               FROM pro_payments p LEFT JOIN pro_subs s ON s.id = p.sub_id WHERE p.at > ?1 ORDER BY p.at DESC",
        )?;
        let all = stmt
            .query_map([since], |r| {
                let plan: Option<String> = r.get(2)?;
                Ok(json!({
                    "payment_id": r.get::<_, String>(0)?, "gateway": "razorpay", "email": null,
                    "subscription_id": r.get::<_, String>(1)?, "plan": "pro",
                    "period": plan.as_deref().and_then(Plan::parse).map(Plan::key),
                    "amount": r.get::<_, i64>(3)?, "currency": "INR", "paid_at": iso(r.get(4)?),
                    "refunded": r.get::<_, i64>(6)?, "pending_refunds": r.get::<_, i64>(7)?,
                    "last_refund_at": iso_opt(r.get(8)?), "plan_ended_at": iso_opt(r.get(5)?),
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let gross: i64 = all.iter().filter_map(|p| p["amount"].as_i64()).sum();
        let refunded: i64 = all.iter().filter_map(|p| p["refunded"].as_i64()).sum();
        let totals = if all.is_empty() {
            vec![]
        } else {
            vec![json!({ "currency": "INR", "payments": all.len(), "gross": gross, "refunded": refunded,
                         "net": gross - refunded })]
        };
        let shown: Vec<Value> = all
            .into_iter()
            .filter(|p| !refunded_only || p["refunded"].as_i64() > Some(0) || p["pending_refunds"].as_i64() > Some(0))
            .take(500)
            .collect();
        Ok(json!({ "payments": shown, "totals": totals, "unmatched_refunds": [] }))
    };
    let v = read().map_err(db)?;
    ok(v)
}

// ── Give Pro ─────────────────────────────────────────────────────────────────────────

/// `GET /admin/v1/grants`
async fn grants(State(st): State<AppState>, peer: Peer, headers: HeaderMap) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let c = st.db.c();
    let read = || -> rusqlite::Result<Vec<Value>> {
        let mut stmt = c.prepare(&format!(
            "SELECT {SUB_COLS} {SUB_FROM} WHERE s.id LIKE 'comp\\_%' ESCAPE '\\' ORDER BY s.created_at DESC LIMIT 1000"
        ))?;
        let subs = stmt.query_map([], sub_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        subs.iter().map(|s| grant_json(&c, s, now)).collect()
    };
    let v = read().map_err(db)?;
    ok(Value::Array(v))
}

/// `POST /admin/v1/grants {install_id, days, note?}`
async fn grant(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let b = body(&headers, &raw)?;
    if !b["email"].is_null() || b["forever"].as_bool() == Some(true) {
        return Err(bad("unsupported", "Ferrocast has no accounts and no forever grants.")
            .hint("Give Pro to an install id for 1 to 90 days."));
    }
    if b["plan"].as_str().is_some_and(|p| p != "pro") {
        return Err(bad("unsupported", "Ferrocast has one plan: pro."));
    }
    let install =
        b["install_id"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 128).ok_or_else(|| {
            bad("invalid_install", "install_id is required.")
                .hint("It's the part before the dot in the app's relay-install secret.")
        })?;
    let days = int(&b["days"], "days", "invalid_days", 1..=comp::MAX_DAYS)?
        .ok_or_else(|| bad("invalid_days", format!("days must be a whole number from 1 to {}.", comp::MAX_DAYS)))?;
    let note = text(&b["note"], "note", 255)?;
    let by = GrantBy { source: "admin", code: None, actor: Some(actor.clone()), note: note.clone() };
    let g = st.db.comp_grant_by(install, days, &by).map_err(|err| match err {
        CompError::Days => bad("invalid_days", format!("days must be a whole number from 1 to {}.", comp::MAX_DAYS)),
        CompError::UnknownInstall(_) => {
            e(StatusCode::NOT_FOUND, "unknown_install", "No install with that id on this relay.")
                .hint("Check the id: the part before the dot in the app's relay-install secret.")
        }
        CompError::RealSubscription { status, .. } => e(
            StatusCode::CONFLICT,
            "has_subscription",
            format!("That install pays for Pro (subscription {status}). Nothing was changed."),
        )
        .hint("It already has Pro; a free grant can be given once that subscription ends."),
        CompError::Db(err) => ApiError::db(err),
    })?;
    audit_log(
        &st,
        now,
        &actor,
        "grant.create",
        &g.sub_id,
        json!({ "install": mask(install), "days": days, "until": g.until, "extended": g.extended, "note": note }),
    );
    Ok(reply(
        StatusCode::CREATED,
        json!({ "id": g.sub_id, "plan": "pro", "starts_at": iso(now), "ends_at": iso(g.until), "extended": g.extended }),
    ))
}

/// `POST /admin/v1/grants/{id}/revoke`
async fn revoke(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let Some(s) = st.db.pro_sub(&id).map_err(db)?.filter(|s| comp::is_comp(&s.id)) else {
        return Err(not_found("free Pro grant"));
    };
    if !access::is_pro(&s.access(), now) {
        return Err(e(StatusCode::CONFLICT, "already_ended", "That grant has already ended."));
    }
    let by = GrantBy { source: "admin", code: None, actor: Some(actor.clone()), note: None };
    let g = st.db.comp_revoke_sub(&id, &by).map_err(db)?.ok_or_else(|| not_found("free Pro grant"))?;
    audit_log(&st, now, &actor, "grant.revoke", &id, json!({}));
    ok(json!({ "id": g.sub_id, "revoked_at": iso(g.until) }))
}

// ── Codes ────────────────────────────────────────────────────────────────────────────

fn code_json(c: &CodeRow) -> Value {
    json!({
        "id": c.code, "code": c.code, "kind": c.kind, "plan": "pro", "duration_days": c.days,
        "percent_off": c.percent_off, "applies_period": c.applies_period, "max_redemptions": c.max_uses,
        "valid_until": iso_opt(c.valid_until), "allowed_emails": null, "active": c.active, "note": c.note,
        "created_by": c.created_by, "created_at": iso(c.created_at), "used": c.used,
    })
}

/// `GET /admin/v1/codes`
async fn list_codes(State(st): State<AppState>, peer: Peer, headers: HeaderMap) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let rows = st.db.codes(now).map_err(db)?;
    ok(Value::Array(rows.iter().map(code_json).collect()))
}

/// `POST /admin/v1/codes`
async fn create_code(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let b = body(&headers, &raw)?;
    if b["plan"].as_str().is_some_and(|p| p != "pro") {
        return Err(bad("unsupported", "Ferrocast has one plan: pro."));
    }
    if b["emails"].as_array().is_some_and(|a| !a.is_empty()) || b["forever"].as_bool() == Some(true) {
        return Err(bad("unsupported", "Ferrocast codes can't be limited to emails or be forever."));
    }
    let kind = b["kind"].as_str().unwrap_or("grant");
    if !codes::KINDS.contains(&kind) {
        return Err(bad("invalid_kind", "kind must be grant or discount."));
    }
    let (days, percent_off, applies_period) = if kind == "grant" {
        let d = int(&b["days"], "days", "invalid_days", 1..=comp::MAX_DAYS)?
            .ok_or_else(|| bad("invalid_days", format!("days must be a whole number from 1 to {}.", comp::MAX_DAYS)))?;
        (Some(d), None, None)
    } else {
        let p =
            int(&b["percent_off"], "percent_off", "invalid_percent", 1..=pricing::MAX_PERCENT)?.ok_or_else(|| {
                bad(
                    "invalid_percent",
                    format!("percent_off must be a whole number from 1 to {}.", pricing::MAX_PERCENT),
                )
            })?;
        let period = match b["period"].as_str().or(b["applies_period"].as_str()).filter(|s| !s.is_empty()) {
            None => None,
            Some(p) if Plan::parse(p).is_some() => Some(p.to_string()),
            Some(_) => return Err(bad("invalid_period", "period must be monthly, yearly or empty (both).")),
        };
        (None, Some(p), period)
    };
    let max_uses = int(&b["max_uses"], "max_uses", "invalid_max_uses", 1..=1_000_000)?;
    let until_v = if b["until"].is_null() { &b["valid_until"] } else { &b["until"] };
    let valid_until =
        time_in(until_v, true).map_err(|()| bad("invalid_dates", "until must be a date like 2026-12-31."))?;
    if valid_until.is_some_and(|u| u <= now) {
        return Err(bad("invalid_dates", "until is already in the past."));
    }
    let note = text(&b["note"], "note", 255)?;
    let prefix = b["prefix"].as_str().unwrap_or("").trim();
    if prefix.len() > 16 || !prefix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err(bad("invalid_code", "prefix may be up to 16 letters, digits or dashes."));
    }
    let given = b["code"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let code = match given {
        Some(c) => {
            codes::normalize(c).ok_or_else(|| bad("invalid_code", "A code is 4 to 32 letters, digits or dashes."))?
        }
        None => codes::generate(prefix),
    };
    let row = CodeRow {
        code: code.clone(),
        kind: kind.into(),
        days,
        percent_off,
        applies_period,
        max_uses,
        valid_until,
        active: true,
        note,
        created_by: actor.clone(),
        created_at: now,
        used: 0,
    };
    // A discount gets its Razorpay plans now, so the first buyer doesn't wait on it.
    if let Some(pct) = percent_off
        && let Ok(rzp) = crate::billing::client(&st)
    {
        for p in Plan::ALL.into_iter().filter(|p| row.applies_to(p.key())) {
            let (list, _) = st.db.list_price(p).map_err(db)?;
            crate::billing::plan_for(&st, rzp, p, pricing::discounted(list, pct)).await?;
        }
    }
    if !st.db.add_code(&row).map_err(db)? {
        return Err(
            e(StatusCode::CONFLICT, "code_taken", format!("There's already a code {code}.")).hint("Pick another name.")
        );
    }
    audit_log(
        &st,
        now,
        &actor,
        "code.create",
        &code,
        json!({ "kind": kind, "days": days, "percent_off": percent_off,
                                                             "max_uses": max_uses, "valid_until": valid_until }),
    );
    Ok(reply(StatusCode::CREATED, code_json(&row)))
}

/// `POST /admin/v1/codes/{code}/disable`
async fn disable_code(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let code = codes::normalize(&code).ok_or_else(|| not_found("code"))?;
    let row = st.db.disable_code(&code, now).map_err(db)?.ok_or_else(|| not_found("code"))?;
    audit_log(&st, now, &actor, "code.disable", &code, json!({}));
    ok(json!({ "code": row.code, "active": row.active }))
}

// ── Prices and sales ─────────────────────────────────────────────────────────────────

fn gateway_ready(st: &AppState, plan: Plan, amount: i64) -> bool {
    (amount == plan.amount_paise() && st.billing_cfg_plan(plan))
        || st.db.rzp_plan(plan, amount).ok().flatten().is_some()
}

fn sale_json(s: &SaleRow, lists: &BTreeMap<String, i64>, now: i64) -> Value {
    let mut amounts = serde_json::Map::new();
    let mut periods = vec![];
    for p in Plan::ALL {
        let list = lists.get(&pricing::key(p)).copied().unwrap_or(p.amount_paise());
        if let Some(a) = s.amount(p, list) {
            amounts.insert(pricing::key(p), json!(a));
            periods.push(p.key());
        }
    }
    json!({
        "id": s.id, "name": s.name, "headline": s.headline, "message": s.message,
        "percent_off": s.percent_off(lists), "discounts": s.discounts, "prices": s.prices, "amounts": amounts,
        "plans": ["pro"], "periods": periods, "currencies": ["INR"],
        "starts_at": iso(s.starts_at), "ends_at": iso_opt(s.ends_at), "stopped_at": iso_opt(s.stopped_at),
        "show_countdown": s.show_countdown, "show_banner": s.show_banner, "theme": s.theme,
        "state": s.state(now), "created_by": s.created_by, "updated_by": s.updated_by,
        "created_at": iso(s.created_at), "updated_at": iso(s.updated_at),
    })
}

/// `GET /admin/v1/pricing`
async fn get_pricing(State(st): State<AppState>, peer: Peer, headers: HeaderMap) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let lists = st.db.list_prices().map_err(db)?;
    let live = st.db.live_sale(now).map_err(db)?;
    let mut prices = vec![];
    for p in Plan::ALL {
        let (amount, row) = st.db.list_price(p).map_err(db)?;
        prices.push(json!({
            "plan": "pro", "period": p.key(), "currency": "INR", "amount": amount, "default": row.is_none(),
            "gateway_ready": gateway_ready(&st, p, amount),
            "changed_by": row.as_ref().map(|r| r.created_by.clone()), "changed_at": iso_opt(row.map(|r| r.created_at)),
            "sale_amount": live.as_ref().and_then(|s| s.amount(p, amount)),
        }));
    }
    let history: Vec<Value> = st
        .db
        .price_history()
        .map_err(db)?
        .iter()
        .map(|r| {
            json!({ "id": r.id, "plan": "pro", "period": r.period, "currency": "INR", "amount": r.amount,
                    "active": r.retired_at.is_none(), "created_by": r.created_by, "created_at": iso(r.created_at),
                    "retired_at": iso_opt(r.retired_at) })
        })
        .collect();
    let sales: Vec<Value> = st.db.sales().map_err(db)?.iter().map(|s| sale_json(s, &lists, now)).collect();
    let gateway = crate::billing::client(&st).is_ok();
    ok(json!({
        "prices": prices, "history": history, "gateways": { "INR": gateway },
        "sales_open": st.billing_cfg.sales_open, "sales": sales, "themes": pricing::THEMES, "now": iso(now),
    }))
}

/// `PUT /admin/v1/prices/pro/{period}/INR {amount, expected_amount?}`
async fn put_price(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path((plan, period, currency)): Path<(String, String, String)>,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let Some(p) = Plan::parse(&period).filter(|_| plan == "pro" && currency.eq_ignore_ascii_case("INR")) else {
        return Err(e(
            StatusCode::NOT_FOUND,
            "unknown_price",
            "Ferrocast has pro/monthly/INR and pro/yearly/INR only.",
        ));
    };
    let b = body(&headers, &raw)?;
    let range = pricing::MIN_AMOUNT..=access::MANDATE_NO_OTP_LIMIT_PAISE - 1;
    let amount = int(&b["amount"], "amount", "invalid_amount", range.clone())?.ok_or_else(|| {
        bad("invalid_amount", "amount (paise) is required: from 100 (₹1) to 1,499,999 (under ₹15,000).")
    })?;
    let (current, row) = st.db.list_price(p).map_err(db)?;
    if let Some(expected) = int(&b["expected_amount"], "expected_amount", "invalid_amount", 0..=i64::MAX)?
        && expected != current
    {
        return Err(e(StatusCode::CONFLICT, "price_changed", "The price changed since you opened it.")
            .hint(format!("It is {current} paise now. Reload and try again.")));
    }
    let row_json = |r: &pricing::PriceRow| {
        json!({ "id": r.id, "plan": "pro", "period": r.period, "currency": "INR", "amount": r.amount, "active": true,
                "gateway_ready": gateway_ready(&st, p, r.amount), "created_by": r.created_by,
                "created_at": iso(r.created_at), "retired_at": null })
    };
    if amount == current {
        let r = row.unwrap_or(pricing::PriceRow {
            id: format!("default_{}", p.key()),
            period: p.key().into(),
            amount,
            created_by: "default".into(),
            created_at: 0,
            retired_at: None,
        });
        return ok(row_json(&r));
    }
    if let Ok(rzp) = crate::billing::client(&st) {
        crate::billing::plan_for(&st, rzp, p, amount).await?;
    }
    let r = st.db.set_price(p, amount, &actor, now).map_err(db)?;
    audit_log(&st, now, &actor, "price.set", &pricing::key(p), json!({ "amount": amount, "was": current }));
    ok(row_json(&r))
}

/// The sale fields from a request body, on top of `base` (an existing sale, or a new one).
fn sale_from(b: &Value, base: SaleRow, now: i64) -> Result<SaleRow, ApiError> {
    let mut s = base;
    s.name = text(&b["name"], "name", 200)?.ok_or_else(|| bad("invalid_name", "name is required."))?;
    s.headline = text(&b["headline"], "headline", 200)?.unwrap_or_else(|| s.name.clone());
    s.message = text(&b["message"], "message", 400)?;
    if b["plans"].as_array().is_some_and(|a| a.iter().any(|p| p != "pro")) {
        return Err(bad("unsupported", "Ferrocast has one plan: pro."));
    }
    if b["currencies"].as_array().is_some_and(|a| a.iter().any(|c| c != "INR")) {
        return Err(bad("unsupported_currency", "Ferrocast sells in INR only."));
    }
    let keys: Vec<String> = Plan::ALL.iter().map(|p| pricing::key(*p)).collect();
    let pct_msg =
        || bad("invalid_percent", format!("Each discount must be a whole percent from 1 to {}.", pricing::MAX_PERCENT));
    let mut discounts = BTreeMap::new();
    if let Some(m) = b["discounts"].as_object() {
        for (k, v) in m {
            if !keys.contains(k) {
                return Err(bad("unsupported", format!("{k} isn't a Ferrocast plan; use pro_monthly or pro_yearly.")));
            }
            if v.is_null() {
                continue;
            }
            let x = v.as_i64().filter(|x| (1..=pricing::MAX_PERCENT).contains(x)).ok_or_else(pct_msg)?;
            discounts.insert(k.clone(), x);
        }
    } else if !b["percent_off"].is_null() {
        let x = b["percent_off"].as_i64().filter(|x| (1..=pricing::MAX_PERCENT).contains(x)).ok_or_else(pct_msg)?;
        let periods: Vec<String> =
            b["periods"].as_array().map_or(vec![], |a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect());
        for p in Plan::ALL {
            if periods.is_empty() || periods.iter().any(|x| x == p.key()) {
                discounts.insert(pricing::key(p), x);
            }
        }
    }
    let mut prices = BTreeMap::new();
    if let Some(m) = b["prices"].as_object() {
        for (k, v) in m {
            if !keys.contains(k) {
                return Err(bad("unsupported", format!("{k} isn't a Ferrocast plan; use pro_monthly or pro_yearly.")));
            }
            if v.is_null() {
                continue;
            }
            let x = v.as_i64().filter(|x| pricing::amount_ok(*x)).ok_or_else(|| {
                bad("invalid_amount", "A sale price is paise from 100 (₹1) to 1,499,999 (under ₹15,000).")
            })?;
            prices.insert(k.clone(), x);
        }
    }
    if discounts.is_empty() && prices.is_empty() {
        return Err(bad("nothing_on_sale", "Put at least one plan on sale."));
    }
    s.discounts = discounts;
    s.prices = prices;
    let bad_time = |_| bad("invalid_dates", "starts_at and ends_at must be times like 2026-10-28T00:00:00Z.");
    s.starts_at = time_in(&b["starts_at"], false).map_err(bad_time)?.unwrap_or(now);
    s.ends_at = time_in(&b["ends_at"], false).map_err(bad_time)?;
    if s.ends_at.is_some_and(|e| e <= s.starts_at) {
        return Err(bad("invalid_dates", "ends_at must be after starts_at."));
    }
    if s.ends_at.is_some_and(|e| e <= now) {
        return Err(bad("invalid_dates", "ends_at is already in the past."));
    }
    s.show_countdown = b["show_countdown"].as_bool().unwrap_or(false);
    s.show_banner = b["show_banner"].as_bool().unwrap_or(false);
    s.theme = match b["theme"].as_str().filter(|t| !t.is_empty()) {
        None => "violet".into(),
        Some(t) if pricing::THEMES.contains(&t) => t.into(),
        Some(_) => return Err(bad("invalid_theme", format!("theme must be one of: {}.", pricing::THEMES.join(", ")))),
    };
    Ok(s)
}

/// Saves a sale after checking it doesn't overlap another and making its Razorpay plans.
async fn save_sale(st: &AppState, s: &SaleRow, now: i64) -> Result<(), ApiError> {
    for other in st.db.sales().map_err(db)? {
        if other.id == s.id || other.stopped_at.is_some() || other.ends_at.is_some_and(|e| e <= now) {
            continue;
        }
        let a_end = s.ends_at.unwrap_or(i64::MAX);
        let b_end = other.ends_at.unwrap_or(i64::MAX);
        if s.starts_at < b_end && other.starts_at < a_end {
            return Err(e(StatusCode::CONFLICT, "sale_overlap", format!("It overlaps the sale \"{}\".", other.name))
                .hint("Change the dates, or stop the other sale first."));
        }
    }
    if let Ok(rzp) = crate::billing::client(st) {
        for p in Plan::ALL {
            let (list, _) = st.db.list_price(p).map_err(db)?;
            if let Some(a) = s.amount(p, list) {
                crate::billing::plan_for(st, rzp, p, a).await?;
            }
        }
    }
    st.db.put_sale(s).map_err(db)
}

/// `POST /admin/v1/sales`
async fn create_sale(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let b = body(&headers, &raw)?;
    let base = SaleRow {
        id: format!("sale_{}", crate::random(9)),
        name: String::new(),
        headline: String::new(),
        message: None,
        discounts: BTreeMap::new(),
        prices: BTreeMap::new(),
        starts_at: 0,
        ends_at: None,
        stopped_at: None,
        show_countdown: false,
        show_banner: false,
        theme: "violet".into(),
        created_by: actor.clone(),
        updated_by: actor.clone(),
        created_at: now,
        updated_at: now,
    };
    let s = sale_from(&b, base, now)?;
    save_sale(&st, &s, now).await?;
    audit_log(
        &st,
        now,
        &actor,
        "sale.create",
        &s.id,
        json!({ "name": s.name, "discounts": s.discounts,
                                                             "prices": s.prices, "starts_at": s.starts_at, "ends_at": s.ends_at }),
    );
    let lists = st.db.list_prices().map_err(db)?;
    Ok(reply(StatusCode::CREATED, sale_json(&s, &lists, now)))
}

/// `PUT /admin/v1/sales/{id}`
async fn update_sale(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let b = body(&headers, &raw)?;
    let old = st.db.sale(&id).map_err(db)?.ok_or_else(|| not_found("sale"))?;
    if matches!(old.state(now), "ended" | "stopped") {
        return Err(
            e(StatusCode::CONFLICT, "sale_over", "That sale is over and can't be changed.").hint("Make a new sale.")
        );
    }
    let mut s = sale_from(&b, old.clone(), now)?;
    // Left out on an update: keep when it started.
    if b["starts_at"].is_null() {
        s.starts_at = old.starts_at;
        if s.ends_at.is_some_and(|e| e <= s.starts_at) {
            return Err(bad("invalid_dates", "ends_at must be after starts_at."));
        }
    }
    s.updated_by = actor.clone();
    s.updated_at = now;
    save_sale(&st, &s, now).await?;
    audit_log(
        &st,
        now,
        &actor,
        "sale.update",
        &s.id,
        json!({ "name": s.name, "discounts": s.discounts,
                                                             "prices": s.prices, "starts_at": s.starts_at, "ends_at": s.ends_at }),
    );
    let lists = st.db.list_prices().map_err(db)?;
    ok(sale_json(&s, &lists, now))
}

/// `POST /admin/v1/sales/{id}/stop`
async fn stop_sale(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let mut s = st.db.sale(&id).map_err(db)?.ok_or_else(|| not_found("sale"))?;
    if matches!(s.state(now), "ended" | "stopped") {
        return Err(e(StatusCode::CONFLICT, "sale_over", "That sale has already ended."));
    }
    s.stopped_at = Some(now);
    s.updated_by = actor.clone();
    s.updated_at = now;
    st.db.put_sale(&s).map_err(db)?;
    audit_log(&st, now, &actor, "sale.stop", &s.id, json!({}));
    let lists = st.db.list_prices().map_err(db)?;
    ok(sale_json(&s, &lists, now))
}

/// `GET /admin/v1/audit?limit=`
async fn audit(State(st): State<AppState>, peer: Peer, headers: HeaderMap, Query(q): Q) -> Result<Response, ApiError> {
    reader(&st, peer, &headers)?;
    let limit = page_arg(&q, "limit", 100, 1..=500)?;
    let c = st.db.c();
    let mut stmt = c
        .prepare("SELECT id, at, actor, action, target, detail FROM admin_audit ORDER BY id DESC LIMIT ?1")
        .map_err(db)?;
    let rows = stmt
        .query_map([limit], |r| {
            let detail: String = r.get(5)?;
            Ok(json!({ "id": r.get::<_, i64>(0)?, "at": iso(r.get(1)?), "actor": r.get::<_, String>(2)?,
                       "action": r.get::<_, String>(3)?, "target": r.get::<_, Option<String>>(4)?,
                       "detail": serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null) }))
        })
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(db)?;
    ok(Value::Array(rows))
}

impl AppState {
    /// A Razorpay plan id is configured for this plan's default price.
    fn billing_cfg_plan(&self, plan: Plan) -> bool {
        match plan {
            Plan::Monthly => self.billing_cfg.plan_monthly.is_some(),
            Plan::Yearly => self.billing_cfg.plan_yearly.is_some(),
        }
    }
}

#[cfg(test)]
#[path = "admin_pro_tests.rs"]
mod tests;
