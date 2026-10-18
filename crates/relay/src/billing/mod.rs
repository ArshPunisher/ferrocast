//! Ferrocast Pro billing through Razorpay Subscriptions (ARCHITECTURE.md §6, API.md §8).
//!
//! Identity is the relay install the desktop already has (`Bearer <install>.<secret>`):
//! no Ferrocast account, no password, no email login. Checkout makes a Razorpay
//! subscription for that install; Razorpay's own page takes the money. The email given at
//! checkout is kept only as a keyed hash (HMAC with the server's `PRO_EMAIL_KEY`), so the same person can move Pro to a new
//! install later (`/v1/pro/restore` with the subscription id from Razorpay's email).
//!
//! Razorpay owns the money; the relay owns access. Every change (webhook, refresh, cancel,
//! the daily re-check) re-reads the subscription from Razorpay and copies that, so a
//! duplicate, forged-looking or out-of-order event can't leave access wrong. Webhooks
//! are verified on the raw body (HMAC-SHA256, constant time) and handled once per event id.
//!
//! One Razorpay account (one key pair, one webhook stream) serves several of the owner's
//! products, so events for subscriptions and payments that aren't Ferrocast Pro's are
//! answered 200 and ignored, without calling Razorpay.
//!
//! `PRO_SALES_OPEN` is off by default: checkout answers 409 until Pro's features exist.

pub mod access;
pub(crate) mod gate;
mod pages;
pub mod razorpay;
pub mod store;

pub(crate) use pages::date;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use access::Plan;
use razorpay::{Razorpay, RzpError};
use store::{Applied, SubRow, Synced};

use crate::{ApiError, AppState, authed, now_ms};

/// Billing's routes: the app's Pro calls, the manage page, Razorpay's webhook.
pub(crate) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/pro/status", get(status))
        .route("/v1/pro/checkout", post(checkout))
        .route("/v1/pro/cancel", post(cancel))
        .route("/v1/pro/manage-link", post(manage_link))
        .route("/v1/pro/restore", post(restore))
        .route("/pro/manage/{token}", get(manage_page))
        .route("/pro/manage/{token}/cancel", post(manage_cancel))
        .route("/billing/razorpay/webhook", post(webhook))
}

/// `notes.product` on every plan and subscription Pro makes, to tell them apart from the
/// other products on the same Razorpay account.
pub const PRODUCT: &str = "ferrocast-pro";
/// A checkout link Razorpay stops accepting after this long.
pub const CHECKOUT_EXPIRES_S: i64 = 2 * 86_400;
/// How long a manage-page link works.
pub const MANAGE_LINK_MS: i64 = 30 * 60_000;
/// Refunds are re-checked daily for payments this recent.
pub const REFUND_RECHECK_MS: i64 = 60 * 86_400_000;

/// Billing settings, read from the environment at start (`deploy/razorpay-keys.sh` writes
/// them on the server). Debug output never shows the secrets.
#[derive(Clone)]
pub struct BillingConfig {
    pub key_id: Option<String>,
    pub key_secret: Option<String>,
    pub webhook_secret: Option<String>,
    /// Keys the email hash, so a leaked database or Razorpay notes can't be matched
    /// against a list of addresses. Made once by razorpay-keys.sh and never changed.
    pub email_key: Option<String>,
    pub plan_monthly: Option<String>,
    pub plan_yearly: Option<String>,
    /// Razorpay's API base, `https://api.razorpay.com/v1` (tests point it at a fake).
    pub api_url: String,
    /// `PRO_SALES_OPEN`: false until Pro's features exist.
    pub sales_open: bool,
}

impl Default for BillingConfig {
    fn default() -> Self {
        BillingConfig {
            key_id: None,
            key_secret: None,
            webhook_secret: None,
            email_key: None,
            plan_monthly: None,
            plan_yearly: None,
            api_url: "https://api.razorpay.com/v1".into(),
            sales_open: false,
        }
    }
}

impl std::fmt::Debug for BillingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingConfig")
            .field("key_id", &self.key_id)
            .field("key_secret", &self.key_secret.as_ref().map(|_| "<set>"))
            .field("webhook_secret", &self.webhook_secret.as_ref().map(|_| "<set>"))
            .field("email_key", &self.email_key.as_ref().map(|_| "<set>"))
            .field("plan_monthly", &self.plan_monthly)
            .field("plan_yearly", &self.plan_yearly)
            .field("api_url", &self.api_url)
            .field("sales_open", &self.sales_open)
            .finish()
    }
}

impl BillingConfig {
    pub fn from_env() -> BillingConfig {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        BillingConfig {
            key_id: env("RAZORPAY_KEY_ID"),
            key_secret: env("RAZORPAY_KEY_SECRET"),
            webhook_secret: env("RAZORPAY_WEBHOOK_SECRET"),
            email_key: env("PRO_EMAIL_KEY").filter(|k| k.len() >= 32),
            plan_monthly: env("RAZORPAY_PLAN_MONTHLY"),
            plan_yearly: env("RAZORPAY_PLAN_YEARLY"),
            api_url: env("RAZORPAY_API_URL").unwrap_or_else(|| "https://api.razorpay.com/v1".into()),
            sales_open: env("PRO_SALES_OPEN")
                .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes")),
        }
    }

    /// `test` or `live`, from the key id.
    pub fn mode(&self) -> Option<&'static str> {
        let id = self.key_id.as_deref()?;
        if id.starts_with("rzp_live_") {
            Some("live")
        } else if id.starts_with("rzp_test_") {
            Some("test")
        } else {
            None
        }
    }

    fn plan_id(&self, plan: Plan) -> Option<&str> {
        match plan {
            Plan::Monthly => self.plan_monthly.as_deref(),
            Plan::Yearly => self.plan_yearly.as_deref(),
        }
    }
}

/// Tries per (kind, key): the window they count in and how many.
type Limits = HashMap<(&'static str, String), (i64, u32)>;

/// Billing's in-memory parts: the Razorpay client, plans already checked, rate limits.
#[derive(Debug, Default)]
pub struct Runtime {
    client: OnceLock<Option<Razorpay>>,
    plans_ok: Mutex<HashMap<String, bool>>,
    limits: Mutex<Limits>,
    checkouts: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl Runtime {
    fn checkout_lock(&self, install: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut m = self.checkouts.lock().unwrap_or_else(|p| p.into_inner());
        // Locks nobody holds or waits on can go.
        if m.len() > 1_000 {
            m.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
        }
        m.entry(install.to_string()).or_default().clone()
    }

    /// Counts one try; `false` once `key` made more than `max` in this `window_ms`.
    pub fn allow(&self, kind: &'static str, key: &str, max: u32, window_ms: i64) -> bool {
        let mut m = self.limits.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_ms();
        let window = now / window_ms.max(1);
        // Old windows go, so the map stays small.
        if m.len() > 10_000 {
            m.retain(|_, (w, _)| *w == window);
        }
        let e = m.entry((kind, key.to_string())).or_insert((window, 0));
        if e.0 != window {
            *e = (window, 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

fn err(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn not_set_up() -> ApiError {
    err(StatusCode::SERVICE_UNAVAILABLE, "billing_unavailable", "Ferrocast Pro billing isn't set up on this relay yet.")
        .hint("Nothing to do on your side; try again later.")
}

fn upstream(e: &RzpError) -> ApiError {
    tracing::warn!(error = %e, "Razorpay call failed");
    err(StatusCode::BAD_GATEWAY, "razorpay_unavailable", "Razorpay, our payment provider, didn't answer properly.")
        .hint("Try again in a minute. You haven't been charged for anything that failed.")
}

fn client(st: &AppState) -> Result<&Razorpay, ApiError> {
    st.billing
        .client
        .get_or_init(|| {
            let c = &st.billing_cfg;
            match (&c.key_id, &c.key_secret) {
                (Some(id), Some(secret)) => Some(Razorpay::new(&c.api_url, id, secret)),
                _ => None,
            }
        })
        .as_ref()
        .ok_or_else(not_set_up)
}

/// The email is kept only as this hash: hex HMAC-SHA256 of the trimmed, lowercased
/// address, keyed with `PRO_EMAIL_KEY` (never sent anywhere by the relay).
pub fn email_hash(key: &str, email: &str) -> String {
    use hmac::{Hmac, Mac};
    let Ok(mut mac) = Hmac::<rsa::sha2::Sha256>::new_from_slice(key.as_bytes()) else { return String::new() };
    mac.update(email.trim().to_lowercase().as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn email_key(st: &AppState) -> Result<&str, ApiError> {
    st.billing_cfg.email_key.as_deref().filter(|k| !k.is_empty()).ok_or_else(not_set_up)
}

/// A plain check, not RFC 5322: one `@`, something on both sides, a dot in the domain,
/// no spaces or control characters, at most 254 bytes.
pub fn valid_email(email: &str) -> bool {
    let e = email.trim();
    let Some((local, domain)) = e.split_once('@') else { return false };
    (3..=254).contains(&e.len())
        && !local.is_empty()
        && local.len() <= 64
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn ms(v: &Value) -> Option<i64> {
    v.as_i64().filter(|s| *s > 0).map(|s| s.saturating_mul(1000))
}

/// Razorpay's subscription entity → what the relay keeps.
fn synced(v: &Value, cancel_at_end: Option<bool>) -> Result<Synced, ApiError> {
    let status = v["status"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32)
        .ok_or_else(|| upstream(&RzpError::BadAnswer))?;
    Ok(Synced { status: status.to_string(), current_end: ms(&v["current_end"]), cancel_at_end })
}

/// Re-reads one subscription from Razorpay and stores it.
async fn refresh(st: &AppState, id: &str) -> Result<Option<SubRow>, ApiError> {
    let v = client(st)?.fetch_subscription(id).await.map_err(|e| upstream(&e))?;
    let s = synced(&v, None)?;
    st.db.sync_pro_sub(None, id, &s).map_err(ApiError::db)?;
    st.db.pro_sub(id).map_err(ApiError::db)
}

/// Which Pro features work on this server: built and configured (the dashboard shows a Pro
/// panel only for these). Each feature module answers for itself.
fn features(st: &AppState) -> Value {
    json!({
        "cloud_bot": crate::bot::available(st),
        "remote": crate::remote::available(st),
        "tts": crate::tts::available(st),
        "discord": crate::discord::available(st),
        "backups": crate::backups::available(st),
        "tip_brand": crate::tips::brand_available(st),
        "history": crate::history::available(st),
    })
}

/// The subscription's counters, `{ name: { used, limit, period, reset_at } }`.
fn quotas(st: &AppState, sub: &str) -> Value {
    let all = [
        crate::bot::QUOTAS,
        crate::remote::QUOTAS,
        crate::tts::QUOTAS,
        crate::discord::QUOTAS,
        crate::backups::QUOTAS,
        crate::tips::QUOTAS,
        crate::history::QUOTAS,
    ];
    let mut m = serde_json::Map::new();
    for l in all.into_iter().flatten() {
        match st.db.usage(sub, l) {
            Ok(u) => {
                m.insert(l.name.to_string(), json!(u));
            }
            Err(e) => tracing::warn!(error = %e, quota = l.name, "couldn't read a quota"),
        }
    }
    Value::Object(m)
}

/// What the app and the manage page show.
fn view(st: &AppState, sub: Option<&SubRow>) -> Value {
    let now = now_ms();
    let prices =
        json!({ "currency": "INR", "monthly": Plan::Monthly.amount_paise(), "yearly": Plan::Yearly.amount_paise() });
    let Some(s) = sub else {
        return json!({ "pro": false, "status": "none", "plan": null, "subscription_id": null, "current_end": null,
                       "access_until": null, "cancel_at_period_end": false, "refunded": false, "installs": 0,
                       "sales_open": st.billing_cfg.sales_open, "prices": prices, "checked_at": now,
                       "features": features(st), "quotas": {} });
    };
    let a = s.access();
    json!({
        "pro": access::is_pro(&a, now),
        "status": s.status,
        "plan": s.plan,
        "subscription_id": s.id,
        "current_end": s.current_end,
        "access_until": access::access_until(&a),
        "cancel_at_period_end": s.cancel_at_end,
        "refunded": s.refunded_at.is_some(),
        "installs": st.db.pro_installs(&s.id).unwrap_or(0),
        "sales_open": st.billing_cfg.sales_open,
        "prices": prices,
        "checked_at": now,
        "features": features(st),
        "quotas": quotas(st, &s.id),
    })
}

fn json_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    if let Some(ct) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        && !ct.trim_start().to_ascii_lowercase().starts_with("application/json")
    {
        return Err(err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the request as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad_json", "The request body isn't a JSON object."))
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    refresh: Option<String>,
}

/// `GET /v1/pro/status`: this install's Pro, from the relay's copy; `?refresh=1` asks
/// Razorpay first (at most every 10 s per install), e.g. right after checkout.
pub async fn status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StatusQuery>,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let mut sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?;
    let mut warning = None;
    if q.refresh.as_deref().is_some_and(|r| r == "1" || r == "true")
        && let Some(s) = &sub
        && access::OPEN.contains(&s.status.as_str())
        && s.refunded_at.is_none()
        && st.billing.allow("refresh", &install, 1, 10_000)
    {
        match refresh(&st, &s.id).await {
            Ok(fresh) => sub = fresh,
            Err(e) => {
                tracing::debug!(?e, "status refresh failed");
                warning = Some("Razorpay couldn't be reached just now, so this is the last known state.");
            }
        }
    }
    let mut v = view(&st, sub.as_ref());
    if let Some(w) = warning {
        v["warning"] = json!(w);
    }
    Ok(axum::Json(v).into_response())
}

/// `POST /v1/pro/checkout` `{plan: "monthly"|"yearly", email}` → 201 with Razorpay's
/// checkout link for a new subscription.
pub async fn checkout(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    if !st.billing_cfg.sales_open {
        return Err(err(StatusCode::CONFLICT, "pro_not_on_sale", "Ferrocast Pro isn't on sale yet.")
            .hint("Pro launches after version 1. Everything Ferrocast does today is free, and stays free."));
    }
    let rzp = client(&st)?;
    let b = json_body(&headers, &body)?;
    let Some(plan) = b["plan"].as_str().and_then(Plan::parse) else {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_plan", "Pick the monthly or the yearly plan."));
    };
    let email = b["email"].as_str().unwrap_or("").trim().to_string();
    if !valid_email(&email) {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email", "That email address doesn't look right.")
            .hint("Use the email you want Razorpay's receipts at; it also lets you move Pro to a new PC."));
    }
    let Some(plan_id) = st.billing_cfg.plan_id(plan).map(str::to_string) else { return Err(not_set_up()) };
    if !st.billing.allow("checkout", &install, 10, 3_600_000) {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many checkouts started from this install.",
        )
        .hint("Wait an hour, or finish the checkout page you already opened."));
    }
    // One checkout at a time per install, so two quick clicks can't make two subscriptions.
    let lock = st.billing.checkout_lock(&install);
    let _held = lock.lock().await;
    if let Some(s) = st.db.pro_sub_for_install(&install).map_err(ApiError::db)? {
        let a = s.access();
        if access::BLOCKING.contains(&s.status.as_str()) && s.refunded_at.is_none() {
            return Err(err(
                StatusCode::CONFLICT,
                "already_pro",
                "This install already has a Ferrocast Pro subscription.",
            )
            .hint("Manage it from Settings › Pro."));
        }
        if access::is_pro(&a, now_ms()) {
            return Err(err(StatusCode::CONFLICT, "already_pro", "Your Pro is paid until the end of this period.")
                .hint("You can subscribe again once it ends."));
        }
        // An unpaid earlier checkout must not stay payable (and invisible) once a new one
        // replaces it: cancel it at Razorpay first.
        if s.status == "created" {
            // Ask Razorpay first: it may have been paid a moment ago (then it's this
            // install's subscription, never cancelled here).
            let now = rzp.fetch_subscription(&s.id).await.map_err(|e| upstream(&e))?;
            let mut current = synced(&now, None)?;
            if current.status == "created" {
                let v = match rzp.cancel_subscription(&s.id, false).await {
                    Ok(v) => v,
                    Err(RzpError::Refused(code, _)) if (400..500).contains(&code) => {
                        rzp.fetch_subscription(&s.id).await.map_err(|e| upstream(&e))?
                    }
                    Err(e) => return Err(upstream(&e)),
                };
                current = synced(&v, None)?;
            }
            st.db.sync_pro_sub(None, &s.id, &current).map_err(ApiError::db)?;
            if !matches!(current.status.as_str(), "cancelled" | "expired") {
                return Err(err(
                    StatusCode::CONFLICT,
                    "already_pro",
                    "Your earlier checkout just went through, so this install has Pro.",
                )
                .hint("Press Check again in Settings › Pro."));
            }
        }
    }
    check_plan(&st, rzp, plan, &plan_id).await?;
    let hash = email_hash(email_key(&st)?, &email);
    let req = json!({
        "plan_id": plan_id,
        "total_count": plan.total_count(),
        "quantity": 1,
        "customer_notify": 1,
        "expire_by": now_ms() / 1000 + CHECKOUT_EXPIRES_S,
        "notes": { "product": PRODUCT, "plan": plan.key(), "install": install, "email_hash": hash },
    });
    let v = rzp.create_subscription(req).await.map_err(|e| upstream(&e))?;
    let id = v["id"].as_str().unwrap_or("");
    let url = v["short_url"].as_str().unwrap_or("");
    if !razorpay::is_id(id, "sub_") || v["plan_id"].as_str() != Some(plan_id.as_str()) || !checkout_url_ok(url) {
        tracing::error!(%id, "Razorpay made a subscription that doesn't match the request");
        return Err(upstream(&RzpError::BadAnswer));
    }
    let status = v["status"].as_str().unwrap_or("created");
    st.db.add_pro_sub(id, plan.key(), &hash, status, &install).map_err(ApiError::db)?;
    tracing::info!(subscription = %id, plan = plan.key(), "Pro checkout started");
    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "subscription_id": id,
            "checkout_url": url,
            "key_id": st.billing_cfg.key_id,
            "plan": plan.key(),
            "amount": plan.amount_paise(),
            "currency": "INR",
        })),
    )
        .into_response())
}

/// Razorpay's hosted checkout links live on rzp.io or razorpay.com.
fn checkout_url_ok(u: &str) -> bool {
    url::Url::parse(u).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some_and(|h| h == "rzp.io" || h == "razorpay.com" || h.ends_with(".razorpay.com"))
    })
}

/// The plan id in the config must be the price the relay means to charge: same amount,
/// INR, the right period, one cycle at a time. Checked once per plan id.
async fn check_plan(st: &AppState, rzp: &Razorpay, plan: Plan, id: &str) -> Result<(), ApiError> {
    if st.billing.plans_ok.lock().unwrap_or_else(|p| p.into_inner()).get(id) == Some(&true) {
        return Ok(());
    }
    let v = rzp.fetch_plan(id).await.map_err(|e| upstream(&e))?;
    let ok = v.pointer("/item/amount").and_then(Value::as_i64) == Some(plan.amount_paise())
        && v.pointer("/item/currency").and_then(Value::as_str) == Some("INR")
        && v["period"].as_str() == Some(plan.period())
        && v["interval"].as_i64() == Some(1)
        && plan.amount_paise() < access::MANDATE_NO_OTP_LIMIT_PAISE;
    if !ok {
        tracing::error!(plan = plan.key(), %id, "the configured Razorpay plan doesn't match Pro's price; checkout refused");
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "billing_misconfigured",
            "Pro checkout is switched off while we fix a setting.",
        )
        .hint("Nothing was charged. Try again later."));
    }
    st.billing.plans_ok.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_string(), true);
    Ok(())
}

/// Stops renewing (Pro lasts until the end of the paid period).
async fn cancel_sub(st: &AppState, s: &SubRow) -> Result<SubRow, ApiError> {
    if !access::BLOCKING.contains(&s.status.as_str()) || s.refunded_at.is_some() {
        return Err(err(StatusCode::NOT_FOUND, "no_subscription", "There's no renewing Pro subscription to cancel."));
    }
    if s.cancel_at_end {
        return Err(err(StatusCode::CONFLICT, "already_cancelled", "Pro is already set to end with this period.")
            .hint("Nothing more to do: it won't renew."));
    }
    let v = client(st)?.cancel_subscription(&s.id, true).await.map_err(|e| match e {
        RzpError::Refused(code, d) if (400..500).contains(&code) => err(
            StatusCode::CONFLICT,
            "not_cancellable",
            "Razorpay says this subscription can't be cancelled right now.",
        )
        .hint(format!("Razorpay said: {d}")),
        e => upstream(&e),
    })?;
    let synced = synced(&v, Some(true))?;
    st.db.sync_pro_sub(None, &s.id, &synced).map_err(ApiError::db)?;
    tracing::info!(subscription = %s.id, "Pro set to end with the period");
    st.db.pro_sub(&s.id).map_err(ApiError::db)?.ok_or_else(|| ApiError::db(rusqlite::Error::QueryReturnedNoRows))
}

/// `POST /v1/pro/cancel`: stop renewing this install's subscription.
pub async fn cancel(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let Some(s) = st.db.pro_sub_for_install(&install).map_err(ApiError::db)? else {
        return Err(err(StatusCode::NOT_FOUND, "no_subscription", "This install has no Pro subscription."));
    };
    let s = cancel_sub(&st, &s).await?;
    Ok(axum::Json(view(&st, Some(&s))).into_response())
}

/// `POST /v1/pro/manage-link` → a link to the manage page, valid for 30 minutes.
pub async fn manage_link(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let Some(s) = st.db.pro_sub_for_install(&install).map_err(ApiError::db)? else {
        return Err(err(StatusCode::NOT_FOUND, "no_subscription", "This install has no Pro subscription to manage.")
            .hint("Buy Pro, or restore it with your subscription id, in Settings › Pro."));
    };
    if !st.billing.allow("manage", &install, 20, 3_600_000) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many manage links. Wait a while."));
    }
    let token = crate::random(32);
    let expires = now_ms() + MANAGE_LINK_MS;
    st.db.put_pro_manage(&crate::hash(&token), &s.id, expires).map_err(ApiError::db)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(json!({ "url": format!("{}/pro/manage/{token}", st.cfg.public_url), "expires_at": expires })),
    )
        .into_response())
}

/// `POST /v1/pro/restore` `{subscription_id, email}`: moves Pro onto this install (a new
/// PC, a reinstall). Both must match; a wrong pair always gets the same 404.
pub async fn restore(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let b = json_body(&headers, &body)?;
    let id = b["subscription_id"].as_str().unwrap_or("").trim().to_string();
    let email = b["email"].as_str().unwrap_or("").trim().to_string();
    if !razorpay::is_id(&id, "sub_") {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_subscription", "That isn't a subscription id.")
            .hint("It starts with sub_ and is in Razorpay's emails about your Pro subscription."));
    }
    if !valid_email(&email) {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email", "That email address doesn't look right."));
    }
    let ip = crate::client_ip(peer, &headers).to_string();
    if !st.billing.allow("restore", &install, 5, 3_600_000) || !st.billing.allow("restore-ip", &ip, 20, 3_600_000) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many tries to restore Pro.")
            .hint("Wait an hour and check the id and email before trying again."));
    }
    let hash = email_hash(email_key(&st)?, &email);
    let not_found = || {
        err(StatusCode::NOT_FOUND, "not_found", "We couldn't find Ferrocast Pro with that subscription id and email.")
            .hint("Check the id (sub_…) in Razorpay's email, and use the email you gave at checkout.")
    };
    let sub = match st.db.pro_sub(&id).map_err(ApiError::db)? {
        Some(s) => {
            if !bool::from(s.email_hash.as_bytes().ct_eq(hash.as_bytes())) {
                return Err(not_found());
            }
            s
        }
        // Not in the relay's database (it was rebuilt): Razorpay's notes carry the same hash.
        None => {
            let v = match client(&st)?.fetch_subscription(&id).await {
                Ok(v) => v,
                Err(RzpError::Refused(code, _)) if (400..500).contains(&code) => return Err(not_found()),
                Err(e) => return Err(upstream(&e)),
            };
            let note = |k: &str| v.pointer(&format!("/notes/{k}")).and_then(Value::as_str).unwrap_or("").to_string();
            if note("product") != PRODUCT || !bool::from(note("email_hash").as_bytes().ct_eq(hash.as_bytes())) {
                return Err(not_found());
            }
            let plan = Plan::parse(&note("plan")).unwrap_or(Plan::Monthly);
            let s = synced(&v, None)?;
            st.db.add_pro_sub(&id, plan.key(), &hash, &s.status, &install).map_err(ApiError::db)?;
            st.db.sync_pro_sub(None, &id, &s).map_err(ApiError::db)?;
            st.db.pro_sub(&id).map_err(ApiError::db)?.ok_or_else(not_found)?
        }
    };
    st.db.link_pro_install(&install, &sub.id).map_err(ApiError::db)?;
    let fresh = if access::OPEN.contains(&sub.status.as_str()) && client(&st).is_ok() {
        refresh(&st, &sub.id).await.ok().flatten().unwrap_or(sub)
    } else {
        sub
    };
    tracing::info!(subscription = %fresh.id, "Pro restored on another install");
    Ok(axum::Json(view(&st, Some(&fresh))).into_response())
}

fn webhook_ok(status: &str) -> Response {
    (StatusCode::OK, axum::Json(json!({ "status": status }))).into_response()
}

/// `POST /billing/razorpay/webhook`. 2xx means "handled, don't retry": anything that
/// failed on our side (Razorpay unreachable, storage) answers 5xx and leaves the event
/// unrecorded, so Razorpay's retry gets another go.
pub async fn webhook(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    match handle_webhook(&st, &headers, &body).await {
        Ok(status) => webhook_ok(status),
        Err(e) => e.into_response(),
    }
}

async fn handle_webhook(st: &AppState, headers: &HeaderMap, body: &[u8]) -> Result<&'static str, ApiError> {
    let Some(secret) = st.billing_cfg.webhook_secret.as_deref() else { return Err(not_set_up()) };
    let sig = headers.get("x-razorpay-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !razorpay::webhook_signature_ok(secret, body, sig) {
        tracing::warn!("refused a Razorpay webhook with a bad signature");
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            "This webhook isn't signed with the relay's secret.",
        )
        .hint("In Razorpay → Webhooks, the secret must be the one razorpay-keys.sh printed."));
    }
    let event_id = headers.get("x-razorpay-event-id").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    if event_id.is_empty() || event_id.len() > 128 {
        return Err(err(StatusCode::BAD_REQUEST, "missing_event_id", "The X-Razorpay-Event-Id header is missing."));
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Err(err(StatusCode::BAD_REQUEST, "bad_json", "The webhook body isn't JSON."));
    };
    let Some(event) = v["event"].as_str() else {
        return Err(err(StatusCode::BAD_REQUEST, "bad_event", "The webhook has no event name."));
    };
    if st.db.billing_event_seen(event_id).map_err(ApiError::db)? {
        return Ok("duplicate");
    }
    if event.starts_with("subscription.") {
        return subscription_event(st, event_id, event, &v).await;
    }
    if event.starts_with("refund.") {
        return refund_event(st, event_id, &v).await;
    }
    Ok("ignored")
}

async fn subscription_event(st: &AppState, event_id: &str, event: &str, v: &Value) -> Result<&'static str, ApiError> {
    let entity = v.pointer("/payload/subscription/entity").unwrap_or(&Value::Null);
    let Some(id) = entity["id"].as_str().filter(|id| razorpay::is_id(id, "sub_")) else { return Ok("ignored") };
    if st.db.pro_sub(id).map_err(ApiError::db)?.is_none() {
        if entity.pointer("/notes/product").and_then(Value::as_str) == Some(PRODUCT) {
            tracing::warn!(subscription = %id, event, "a Pro subscription the relay doesn't know (restore links it)");
        }
        // Another product's subscription on the same account: not ours to touch.
        return Ok("ignored");
    }
    // The current truth, not the event's snapshot (events can arrive out of order).
    let current = client(st)?.fetch_subscription(id).await.map_err(|e| upstream(&e))?;
    let s = synced(&current, None)?;
    let applied = if event == "subscription.charged" {
        let pay = v.pointer("/payload/payment/entity").unwrap_or(&Value::Null);
        let payment = match (pay["id"].as_str(), pay["amount"].as_i64()) {
            (Some(p), Some(a)) if razorpay::is_id(p, "pay_") => Some((p, a)),
            _ => None,
        };
        st.db.record_pro_charge(event_id, id, payment, &s).map_err(ApiError::db)?
    } else {
        st.db.sync_pro_sub(Some(event_id), id, &s).map_err(ApiError::db)?
    };
    tracing::info!(subscription = %id, event, status = %s.status, "Pro subscription synced");
    Ok(if applied == Applied::Duplicate { "duplicate" } else { "ok" })
}

/// Any refund event: re-read every refund on the payment, and end Pro if the payment is
/// now refunded in full. A partial refund (goodwill) keeps Pro.
async fn refund_event(st: &AppState, event_id: &str, v: &Value) -> Result<&'static str, ApiError> {
    let refund = v.pointer("/payload/refund/entity").unwrap_or(&Value::Null);
    let Some(payment) = refund["payment_id"].as_str().filter(|p| razorpay::is_id(p, "pay_")) else {
        return Ok("ignored");
    };
    let Some((sub_id, _)) = st.db.pro_payment(payment).map_err(ApiError::db)? else {
        return Ok("ignored"); // not a Pro payment
    };
    let ended = check_refunds(st, payment, Some(event_id)).await?;
    if !ended {
        st.db.note_billing_event(event_id).map_err(ApiError::db)?;
    }
    tracing::info!(subscription = %sub_id, %payment, ended, "Pro refund handled");
    Ok("ok")
}

/// Ends Pro if `payment` is refunded in full (refunds that didn't fail add up to its
/// amount): cancels the subscription now so it can't charge again. `true` if it ended.
async fn check_refunds(st: &AppState, payment: &str, event: Option<&str>) -> Result<bool, ApiError> {
    let Some((sub_id, amount)) = st.db.pro_payment(payment).map_err(ApiError::db)? else { return Ok(false) };
    let rzp = client(st)?;
    let refunds = rzp.payment_refunds(payment).await.map_err(|e| upstream(&e))?;
    let refunded: i64 =
        refunds.iter().filter(|r| r["status"].as_str() != Some("failed")).filter_map(|r| r["amount"].as_i64()).sum();
    if refunded < amount {
        return Ok(false);
    }
    let Some(sub) = st.db.pro_sub(&sub_id).map_err(ApiError::db)? else { return Ok(false) };
    let synced = if access::OPEN.contains(&sub.status.as_str()) {
        let v = match rzp.cancel_subscription(&sub_id, false).await {
            Ok(v) => v,
            // Already ended at Razorpay (our copy was behind): read what it is now.
            Err(RzpError::Refused(code, _)) if (400..500).contains(&code) => {
                rzp.fetch_subscription(&sub_id).await.map_err(|e| upstream(&e))?
            }
            Err(e) => return Err(upstream(&e)),
        };
        Some(synced(&v, None)?)
    } else {
        None
    };
    st.db.refund_pro_sub(event, &sub_id, synced.as_ref()).map_err(ApiError::db)?;
    tracing::info!(subscription = %sub_id, %payment, "Pro ended by a full refund");
    Ok(true)
}

/// The daily safety net: re-read unfinished subscriptions and recent payments' refunds,
/// so access is right even if webhooks were lost. Returns subscriptions checked.
pub async fn reconcile(st: &AppState) -> usize {
    if client(st).is_err() {
        return 0;
    }
    let subs = st.db.open_pro_subs().unwrap_or_default();
    for id in &subs {
        if let Err(e) = refresh(st, id).await {
            tracing::warn!(subscription = %id, ?e, "daily re-check failed");
        }
    }
    for payment in st.db.recent_pro_payments(now_ms() - REFUND_RECHECK_MS).unwrap_or_default() {
        if let Err(e) = check_refunds(st, &payment, None).await {
            tracing::warn!(%payment, ?e, "daily refund re-check failed");
        }
    }
    subs.len()
}

/// `GET /pro/manage/{token}`: the streamer's page to see and cancel Pro.
pub async fn manage_page(State(st): State<AppState>, Path(token): Path<String>) -> Response {
    let sub = match manage_sub(&st, &token).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    pages::manage(&st, &sub, &token, None)
}

/// `POST /pro/manage/{token}/cancel` (the page's button).
pub async fn manage_cancel(State(st): State<AppState>, Path(token): Path<String>) -> Response {
    let sub = match manage_sub(&st, &token).await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match cancel_sub(&st, &sub).await {
        Ok(s) => {
            pages::manage(&st, &s, &token, Some(Ok("Done. Pro won't renew; it lasts until the end of this period.")))
        }
        Err(e) => {
            let status = e.status();
            let mut r = pages::manage(&st, &sub, &token, Some(Err(e.message().to_string())));
            *r.status_mut() = status;
            r
        }
    }
}

async fn manage_sub(st: &AppState, token: &str) -> Result<SubRow, Box<Response>> {
    let expired = || {
        Box::new(pages::message(
            StatusCode::NOT_FOUND,
            "This link has expired",
            "Manage links work for 30 minutes. Open Ferrocast › Settings › Pro and press Manage again.",
        ))
    };
    if token.len() < 20 || token.len() > 64 {
        return Err(expired());
    }
    let sub_id = match st.db.pro_manage(&crate::hash(token)) {
        Ok(Some(id)) => id,
        Ok(None) => return Err(expired()),
        Err(e) => {
            tracing::error!(error = %e, "manage link lookup failed");
            return Err(Box::new(pages::message(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again shortly",
                "The relay couldn't read that just now.",
            )));
        }
    };
    let Ok(Some(sub)) = st.db.pro_sub(&sub_id) else { return Err(expired()) };
    // Fresh from Razorpay when it's been a minute (a failure just shows the stored state).
    if access::OPEN.contains(&sub.status.as_str()) && now_ms() - sub.synced_at > 60_000 && client(st).is_ok() {
        return Ok(refresh(st, &sub.id).await.ok().flatten().unwrap_or(sub));
    }
    Ok(sub)
}

#[cfg(test)]
mod tests;
