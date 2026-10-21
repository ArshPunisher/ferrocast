//! The Control Center's account tabs (`/admin/v1/accounts…`, docs/ADMIN-API.md § Accounts).
//! Same guard as every admin route (`admin::guard`); writes need `X-Admin-Email` and are
//! written to `admin_audit`. Emails are shown here (accounts have them); install ids only masked.

use std::collections::HashMap;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use rusqlite::params;
use serde_json::{Value, json};

use super::store::Account;
use crate::admin_pro::{
    Peer, audit_log, bad, body, e, grant_history, int, iso, iso_opt, mask, not_found, ok, page_arg, reader, reply,
    text, writer,
};
use crate::billing::access;
use crate::billing::comp::{self, GrantBy};
use crate::db::Db;
use crate::{ApiError, AppState};

const DAY_MS: i64 = 86_400_000;

pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/admin/v1/accounts", get(list))
        .route("/admin/v1/accounts/stats", get(stats))
        .route("/admin/v1/accounts/{id}", get(detail))
        .route("/admin/v1/accounts/{id}/grant", post(grant))
        .route("/admin/v1/accounts/{id}/revoke", post(revoke))
        .route("/admin/v1/accounts/{id}/sign-out", post(sign_out))
        .route("/admin/v1/accounts/{id}/disable", post(disable))
        .route("/admin/v1/accounts/{id}/enable", post(enable))
}

fn db(err: rusqlite::Error) -> ApiError {
    ApiError::db(err)
}

fn account_or_404(st: &AppState, id: &str) -> Result<Account, ApiError> {
    if id.len() > 64 {
        return Err(not_found("account"));
    }
    st.db.account(id).map_err(db)?.ok_or_else(|| not_found("account"))
}

/// The account's plan now: (`pro`|`free`, `subscription`|`grant`|null).
fn plan_of(st: &AppState, id: &str, now: i64) -> rusqlite::Result<(&'static str, Value)> {
    Ok(match st.db.account_sub(id, now)? {
        Some(s) if access::is_pro(&s.access(), now) => {
            ("pro", json!(if comp::is_comp(&s.id) { "grant" } else { "subscription" }))
        }
        _ => ("free", Value::Null),
    })
}

fn row_json(st: &AppState, a: &Account, now: i64) -> rusqlite::Result<Value> {
    let (plan, source) = plan_of(st, &a.id, now)?;
    Ok(json!({
        "id": a.id, "email": a.email, "name": a.name, "avatar_url": a.avatar_url,
        "created_at": iso(a.created_at), "last_seen_at": iso(a.last_seen_at),
        "disabled": a.disabled_at.is_some(), "disabled_at": iso_opt(a.disabled_at),
        "plan": plan, "pro_source": source,
        "providers": st.db.account_providers(&a.id)?,
        "pcs": st.db.account_pcs(&a.id, now)?.len(),
        "sessions": st.db.account_sessions(&a.id, now)?.len(),
    }))
}

/// `GET /admin/v1/accounts?q=&plan=pro|free&status=active|disabled&page=&per_page=`
async fn list(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let page = page_arg(&q, "page", 1, 1..=100_000)?;
    let per_page = page_arg(&q, "per_page", 50, 1..=100)?;
    let search = q.get("q").map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty() && s.len() <= 128);
    let plan = match q.get("plan").map(String::as_str).filter(|s| !s.is_empty()) {
        None => None,
        Some(p @ ("pro" | "free")) => Some(p.to_string()),
        Some(_) => return Err(e(StatusCode::BAD_REQUEST, "bad_query", "plan must be pro or free.")),
    };
    let status = match q.get("status").map(String::as_str).filter(|s| !s.is_empty()) {
        None => "",
        Some("active") => " AND disabled_at IS NULL",
        Some("disabled") => " AND disabled_at IS NOT NULL",
        Some(_) => return Err(e(StatusCode::BAD_REQUEST, "bad_query", "status must be active or disabled.")),
    };
    let rows: Vec<Account> = {
        let c = st.db.c();
        let mut stmt = c
            .prepare(&format!(
                "SELECT id, email, name, avatar_url, created_at, last_seen_at, disabled_at FROM accounts
                  WHERE (?1 IS NULL OR instr(email, ?1) > 0 OR instr(lower(COALESCE(name, '')), ?1) > 0 OR id = ?1){status}
                  ORDER BY created_at DESC, id LIMIT 10000"
            ))
            .map_err(db)?;
        stmt.query_map([&search], |r| {
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                name: r.get(2)?,
                avatar_url: r.get(3)?,
                created_at: r.get(4)?,
                last_seen_at: r.get(5)?,
                disabled_at: r.get(6)?,
            })
        })
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(db)?
    };
    let mut items = Vec::new();
    for a in &rows {
        if let Some(p) = &plan
            && plan_of(&st, &a.id, now).map_err(db)?.0 != p
        {
            continue;
        }
        items.push(a);
    }
    let total = items.len();
    let shown = items
        .into_iter()
        .skip(usize::try_from((page - 1) * per_page).unwrap_or(usize::MAX))
        .take(usize::try_from(per_page).unwrap_or(50))
        .map(|a| row_json(&st, a, now))
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db)?;
    ok(json!({ "total": total, "page": page, "per_page": per_page, "items": shown }))
}

/// `GET /admin/v1/accounts/{id}`
async fn detail(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let now = reader(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    let mut v = row_json(&st, &a, now).map_err(db)?;
    let subs = st.db.account_subs(&a.id).map_err(db)?;
    let c = st.db.c();
    let read = || -> rusqlite::Result<Value> {
        let mut q = c.prepare(
            "SELECT provider, email, created_at, last_used_at FROM account_identities WHERE account_id = ?1 ORDER BY created_at",
        )?;
        let identities = q
            .query_map([&a.id], |r| {
                Ok(json!({ "provider": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?,
                           "created_at": iso(r.get(2)?), "last_used_at": iso(r.get(3)?) }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut q = c.prepare(
            "SELECT id, kind, label, install_id, created_at, last_used_at, MIN(expires_at, max_expires_at), revoked_at, revoked_by
               FROM account_sessions WHERE account_id = ?1 ORDER BY last_used_at DESC LIMIT 50",
        )?;
        let sessions = q
            .query_map([&a.id], |r| {
                let exp: i64 = r.get(6)?;
                let revoked: Option<i64> = r.get(7)?;
                Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "label": r.get::<_, String>(2)?,
                           "install": r.get::<_, Option<String>>(3)?.as_deref().map(mask),
                           "created_at": iso(r.get(4)?), "last_used_at": iso(r.get(5)?), "expires_at": iso(exp),
                           "revoked_at": iso_opt(revoked), "revoked_by": r.get::<_, Option<String>>(8)?,
                           "active": revoked.is_none() && exp > now }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut q = c.prepare(
            "SELECT i.install_id, i.first_at, i.last_at, EXISTS(SELECT 1 FROM account_sessions s WHERE s.account_id = i.account_id
                AND s.install_id = i.install_id AND s.kind = 'app' AND s.revoked_at IS NULL AND s.expires_at > ?2 AND s.max_expires_at > ?2)
               FROM account_installs i WHERE i.account_id = ?1 ORDER BY i.last_at DESC LIMIT 100",
        )?;
        let installs = q
            .query_map(params![a.id, now], |r| {
                Ok(json!({ "install": mask(&r.get::<_, String>(0)?), "first_at": iso(r.get(1)?),
                           "last_at": iso(r.get(2)?), "signed_in": r.get::<_, bool>(3)? }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut subscriptions = Vec::new();
        let mut grants = Vec::new();
        for s in &subs {
            let st_ = s.access();
            let link: (String, i64) =
                c.query_row("SELECT source, linked_at FROM account_subs WHERE sub_id = ?1", [&s.id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            let row = json!({
                "id": s.id, "plan": s.plan, "status": s.status, "pro": access::is_pro(&st_, now),
                "current_end": iso_opt(s.current_end), "access_until": iso_opt(access::access_until(&st_)),
                "cancel_at_period_end": s.cancel_at_end, "refunded_at": iso_opt(s.refunded_at),
                "linked_by": link.0, "linked_at": iso(link.1),
            });
            if comp::is_comp(&s.id) {
                let mut g = row;
                g["history"] = json!(grant_history(&c, &s.id)?);
                grants.push(g);
            } else {
                subscriptions.push(row);
            }
        }
        let mut q = c.prepare(
            "SELECT u.code, COALESCE(k.kind, ''), u.at FROM pro_code_uses u LEFT JOIN pro_codes k ON k.code = u.code
              WHERE u.install_id = ?2 OR u.sub_id IN (SELECT sub_id FROM account_subs WHERE account_id = ?1)
                 OR u.install_id IN (SELECT install_id FROM account_installs WHERE account_id = ?1)
              ORDER BY u.at DESC LIMIT 100",
        )?;
        let codes = q
            .query_map(params![a.id, format!("acct:{}", a.id)], |r| {
                Ok(json!({ "code": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "used_at": iso(r.get(2)?) }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "identities": identities, "sessions": sessions, "installs": installs,
                   "subscriptions": subscriptions, "grants": grants, "codes_used": codes }))
    };
    let more = read().map_err(db)?;
    for k in ["identities", "sessions", "installs", "subscriptions", "grants", "codes_used"] {
        v[k] = more[k].clone();
    }
    ok(v)
}

/// Why an account grant didn't happen.
enum GrantRefused {
    Paying(String),
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for GrantRefused {
    fn from(e: rusqlite::Error) -> Self {
        GrantRefused::Db(e)
    }
}

impl Db {
    /// Gives (or extends) free Pro to an account for `days`, capped at 90 days from now, like
    /// an install's grant: one `comp_` subscription that belongs to the account.
    fn account_grant(
        &self,
        account: &str,
        days: i64,
        by: &GrantBy,
        now: i64,
    ) -> Result<(String, i64, bool), GrantRefused> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut q = tx.prepare(
            "SELECT s.id, s.status, s.current_end, s.cancel_at_end, s.refunded_at IS NOT NULL
               FROM account_subs a JOIN pro_subs s ON s.id = a.sub_id WHERE a.account_id = ?1 ORDER BY a.linked_at DESC",
        )?;
        let subs = q
            .query_map([account], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<(String, String, Option<i64>, bool, bool)>>>()?;
        drop(q);
        for (id, status, end, cae, refunded) in &subs {
            if comp::is_comp(id) {
                continue;
            }
            let a = access::SubState {
                status: status.clone(),
                current_end: *end,
                cancel_at_end: *cae,
                refunded: *refunded,
            };
            if (access::BLOCKING.contains(&status.as_str()) && !refunded) || access::is_pro(&a, now) {
                return Err(GrantRefused::Paying(status.clone()));
            }
        }
        let cap = now + comp::MAX_DAYS * DAY_MS;
        let existing = subs.iter().find(|s| comp::is_comp(&s.0));
        let (id, until, extended) = match existing {
            Some((id, status, end, ..)) => {
                let from = end.filter(|e| status == "active" && *e > now).unwrap_or(now);
                let until = (from + days * DAY_MS).min(cap);
                tx.execute(
                    "UPDATE pro_subs SET status = 'active', current_end = ?2, cancel_at_end = 1, refunded_at = NULL,
                       synced_at = ?3 WHERE id = ?1",
                    params![id, until, now],
                )?;
                (id.clone(), until, true)
            }
            None => {
                let id = format!("{}{}", comp::PREFIX, crate::random(12));
                let until = now + days * DAY_MS;
                tx.execute(
                    "INSERT INTO pro_subs(id, plan, email_hash, status, current_end, cancel_at_end, created_at, synced_at)
                     VALUES (?1, ?2, '', 'active', ?3, 1, ?4, ?4)",
                    params![id, comp::PLAN, until, now],
                )?;
                tx.execute(
                    "INSERT INTO account_subs(sub_id, account_id, source, linked_at) VALUES (?1, ?2, 'grant', ?3)",
                    params![id, account, now],
                )?;
                (id, until, false)
            }
        };
        tx.execute(
            "INSERT INTO pro_grants(sub_id, install_id, kind, source, code, actor, note, days, until, at)
             VALUES (?1, '', 'grant', ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
            params![id, by.source, by.actor, by.note, days, until, now],
        )?;
        tx.commit()?;
        Ok((id, until, extended))
    }

    /// The account's grants that give Pro now.
    fn account_live_grants(&self, account: &str, now: i64) -> rusqlite::Result<Vec<String>> {
        Ok(self
            .account_subs(account)?
            .into_iter()
            .filter(|s| comp::is_comp(&s.id) && access::is_pro(&s.access(), now))
            .map(|s| s.id)
            .collect())
    }
}

/// `POST /admin/v1/accounts/{id}/grant {days, note?}`
async fn grant(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    let b = body(&headers, &raw)?;
    let days = int(&b["days"], "days", "invalid_days", 1..=comp::MAX_DAYS)?
        .ok_or_else(|| bad("invalid_days", format!("days must be a whole number from 1 to {}.", comp::MAX_DAYS)))?;
    let note = text(&b["note"], "note", 255)?;
    let by = GrantBy { source: "admin", code: None, actor: Some(actor.clone()), note: note.clone() };
    let (sub, until, extended) = st.db.account_grant(&a.id, days, &by, now).map_err(|err| match err {
        GrantRefused::Paying(status) => e(
            StatusCode::CONFLICT,
            "has_subscription",
            format!("That account pays for Pro (subscription {status}). Nothing was changed."),
        )
        .hint("It already has Pro; free Pro can be given once that subscription ends."),
        GrantRefused::Db(err) => db(err),
    })?;
    audit_log(
        &st,
        now,
        &actor,
        "account.grant",
        &a.id,
        json!({ "sub": sub, "days": days, "until": until,
        "extended": extended, "note": note }),
    );
    Ok(reply(
        StatusCode::CREATED,
        json!({ "id": sub, "account_id": a.id, "plan": "pro", "starts_at": iso(now), "ends_at": iso(until), "extended": extended }),
    ))
}

/// `POST /admin/v1/accounts/{id}/revoke`: ends the account's free Pro now.
async fn revoke(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    let live = st.db.account_live_grants(&a.id, now).map_err(db)?;
    if live.is_empty() {
        return Err(e(StatusCode::CONFLICT, "already_ended", "That account has no free Pro running.")
            .hint("Paid subscriptions are cancelled or refunded in Razorpay, not here."));
    }
    let by = GrantBy { source: "admin", code: None, actor: Some(actor.clone()), note: None };
    for g in &live {
        st.db.comp_revoke_sub(g, &by).map_err(db)?;
    }
    audit_log(&st, now, &actor, "account.revoke", &a.id, json!({ "grants": live }));
    ok(json!({ "account_id": a.id, "revoked": live, "revoked_at": iso(now) }))
}

/// `POST /admin/v1/accounts/{id}/sign-out`: every session of the account ends now.
async fn sign_out(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    let n = st.db.account_revoke_all(&a.id, &format!("admin:{actor}"), now).map_err(db)?;
    audit_log(&st, now, &actor, "account.sign_out", &a.id, json!({ "sessions": n }));
    ok(json!({ "account_id": a.id, "sessions_ended": n }))
}

/// `POST /admin/v1/accounts/{id}/disable {reason?}`: refused at sign-in, signed out everywhere.
async fn disable(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    let b = body(&headers, &raw)?;
    let reason = text(&b["reason"], "reason", 255)?;
    st.db.account_set_disabled(&a.id, Some(&actor), now).map_err(db)?;
    // Cloud mode: its ingest stops and its overlays close at once (docs/CLOUD.md §10).
    crate::cloud::stop_account(&st, &a.id);
    audit_log(&st, now, &actor, "account.disable", &a.id, json!({ "reason": reason }));
    let a = account_or_404(&st, &id)?;
    ok(row_json(&st, &a, now).map_err(db)?)
}

/// `POST /admin/v1/accounts/{id}/enable`
async fn enable(
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (now, actor) = writer(&st, peer, &headers)?;
    let a = account_or_404(&st, &id)?;
    st.db.account_set_disabled(&a.id, None, now).map_err(db)?;
    audit_log(&st, now, &actor, "account.enable", &a.id, json!({}));
    let a = account_or_404(&st, &id)?;
    ok(row_json(&st, &a, now).map_err(db)?)
}

/// `GET /admin/v1/accounts/stats?days=30`
async fn stats(
    State(st): State<AppState>,
    ConnectInfo(p): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let now = reader(&st, ConnectInfo(p), &headers)?;
    let days = page_arg(&q, "days", 30, 1..=365)?;
    let since_day = now.div_euclid(DAY_MS) - (days - 1);
    let c = st.db.c();
    let read = || -> rusqlite::Result<Value> {
        let (total, a7, a30, disabled): (i64, i64, i64, i64) = c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(last_seen_at >= ?1), 0), COALESCE(SUM(last_seen_at >= ?2), 0),
                    COALESCE(SUM(disabled_at IS NOT NULL), 0) FROM accounts",
            params![now - 7 * DAY_MS, now - 30 * DAY_MS],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut q =
            c.prepare("SELECT created_at / ?2 AS d, COUNT(*) FROM accounts WHERE created_at >= ?1 GROUP BY d")?;
        let by_day: HashMap<i64, i64> = q
            .query_map(params![since_day * DAY_MS, DAY_MS], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let signups: Vec<Value> = (since_day..=now.div_euclid(DAY_MS))
            .map(|d| json!({ "date": &iso(d * DAY_MS)[..10], "count": by_day.get(&d).copied().unwrap_or(0) }))
            .collect();
        // Paid: a Razorpay subscription with at least one payment belongs to the account.
        let paid_sql = "SELECT COUNT(DISTINCT a.account_id) FROM account_subs a JOIN accounts x ON x.id = a.account_id
              WHERE a.sub_id NOT LIKE 'comp\\_%' ESCAPE '\\' AND EXISTS(SELECT 1 FROM pro_payments p WHERE p.sub_id = a.sub_id)";
        let paid_ever: i64 = c.query_row(paid_sql, [], |r| r.get(0))?;
        let new_window: i64 =
            c.query_row("SELECT COUNT(*) FROM accounts WHERE created_at >= ?1", [since_day * DAY_MS], |r| r.get(0))?;
        let paid_window: i64 =
            c.query_row(&format!("{paid_sql} AND x.created_at >= ?1"), [since_day * DAY_MS], |r| r.get(0))?;
        let ids: Vec<String> = {
            let mut q = c.prepare("SELECT DISTINCT account_id FROM account_subs")?;
            q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        Ok(json!({ "total": total, "active_7d": a7, "active_30d": a30, "disabled": disabled, "days": days,
                   "signups": signups, "_ids": ids, "paid_ever": paid_ever, "new_window": new_window,
                   "paid_window": paid_window }))
    };
    let mut v = read().map_err(db)?;
    drop(c);
    let (mut pro_now, mut paid_now) = (0i64, 0i64);
    for id in v["_ids"].as_array().cloned().unwrap_or_default() {
        if let Some(s) = st.db.account_sub(id.as_str().unwrap_or(""), now).map_err(db)?
            && access::is_pro(&s.access(), now)
        {
            pro_now += 1;
            if !comp::is_comp(&s.id) {
                paid_now += 1;
            }
        }
    }
    let rate =
        |n: i64, d: i64| if d == 0 { Value::Null } else { json!((n as f64 / d as f64 * 10_000.0).round() / 10_000.0) };
    let total = v["total"].as_i64().unwrap_or(0);
    let (pe, nw, pw) = (
        v["paid_ever"].as_i64().unwrap_or(0),
        v["new_window"].as_i64().unwrap_or(0),
        v["paid_window"].as_i64().unwrap_or(0),
    );
    let out = json!({
        "accounts": { "total": total, "active_7d": v["active_7d"], "active_30d": v["active_30d"], "disabled": v["disabled"] },
        "signups": { "days": v["days"], "per_day": v["signups"].take(), "in_window": nw },
        "pro": { "accounts_pro_now": pro_now, "accounts_paying_now": paid_now },
        "conversion": {
            "paid_ever": pe, "rate_all_time": rate(pe, total),
            "window_signups": nw, "window_paid": pw, "rate_window": rate(pw, nw),
        },
        "now": iso(now),
    });
    ok(out)
}
