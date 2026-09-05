//! A fake Razorpay API (`/v1`) for the relay's billing tests: plans, subscriptions,
//! cancelling and refunds, with the shapes from Razorpay's API reference. It checks HTTP
//! basic auth against one key pair, records every request, and can be told to answer
//! everything with one status (an outage) or to change a subscription the way Razorpay
//! would after a charge, a failed renewal or a refund.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde_json::{Value, json};

/// Test-mode credentials the fake accepts. Not real keys.
pub const KEY_ID: &str = "rzp_test_FakeKey000001";
pub const KEY_SECRET: &str = "FakeSecretForTestsOnly00";

#[derive(Debug, Default)]
pub struct FakeState {
    pub plans: HashMap<String, Value>,
    pub subscriptions: HashMap<String, Value>,
    /// Refunds per payment id.
    pub refunds: HashMap<String, Vec<Value>>,
    /// Every request answers this status (with a Razorpay-shaped error) while set.
    pub fail_all: Option<u16>,
    /// Every request: method and path.
    pub requests: Vec<(String, String)>,
    next: u32,
}

#[derive(Debug, Clone)]
pub struct FakeRazorpay {
    /// Base URL including `/v1`, like `https://api.razorpay.com/v1`.
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn error(status: StatusCode, description: &str) -> Response {
    let code = if status.is_client_error() { "BAD_REQUEST_ERROR" } else { "SERVER_ERROR" };
    (status, axum::Json(json!({ "error": { "code": code, "description": description } }))).into_response()
}

impl FakeRazorpay {
    pub async fn start() -> FakeRazorpay {
        let state: Arc<Mutex<FakeState>> = Arc::default();
        let app = Router::new()
            .route("/v1/plans", post(create_plan).get(list_plans))
            .route("/v1/plans/{id}", get(fetch_plan))
            .route("/v1/subscriptions", post(create_subscription))
            .route("/v1/subscriptions/{id}", get(fetch_subscription))
            .route("/v1/subscriptions/{id}/cancel", post(cancel_subscription))
            .route("/v1/payments/{id}/refunds", get(payment_refunds))
            .with_state(state.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        FakeRazorpay { base, state }
    }

    fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// Adds a plan as `create-plans` would have, returning its id.
    pub fn add_plan(&self, period: &str, amount: i64) -> String {
        self.with(|s| {
            s.next += 1;
            let id = format!("plan_Fake{:010}", s.next);
            s.plans.insert(id.clone(), plan_entity(&id, period, amount, "INR"));
            id
        })
    }

    /// A plan with any currency or amount (to test that the relay refuses wrong ones).
    pub fn add_plan_raw(&self, period: &str, amount: i64, currency: &str) -> String {
        self.with(|s| {
            s.next += 1;
            let id = format!("plan_Fake{:010}", s.next);
            s.plans.insert(id.clone(), plan_entity(&id, period, amount, currency));
            id
        })
    }

    /// What Razorpay does after the first charge (or a renewal): active, with a period.
    pub fn activate(&self, sub: &str, days: i64) {
        self.set_status(sub, "active", Some(days));
    }

    /// Sets a subscription's status; `days` sets the current period from now.
    pub fn set_status(&self, sub: &str, status: &str, days: Option<i64>) {
        self.with(|s| {
            let e = s.subscriptions.get_mut(sub).expect("no such fake subscription");
            e["status"] = json!(status);
            if let Some(d) = days {
                let now = now_s();
                e["current_start"] = json!(now);
                e["current_end"] = json!(now + d * 86_400);
                e["charge_at"] = json!(now + d * 86_400);
            }
            if matches!(status, "cancelled" | "completed" | "expired") {
                e["ended_at"] = json!(now_s());
            }
        });
    }

    /// Records a refund on a payment, as Razorpay's refunds list would show it.
    pub fn add_refund(&self, payment: &str, amount: i64, status: &str) -> String {
        self.with(|s| {
            s.next += 1;
            let id = format!("rfnd_Fake{:010}", s.next);
            s.refunds.entry(payment.to_string()).or_default().push(json!({
                "id": id, "entity": "refund", "amount": amount, "currency": "INR",
                "payment_id": payment, "status": status, "created_at": now_s(),
            }));
            id
        })
    }

    pub fn subscription(&self, id: &str) -> Option<Value> {
        self.with(|s| s.subscriptions.get(id).cloned())
    }

    pub fn fail_all(&self, status: Option<u16>) {
        self.with(|s| s.fail_all = status);
    }

    pub fn requests(&self) -> Vec<(String, String)> {
        self.with(|s| s.requests.clone())
    }

    /// Requests whose path starts with `prefix` (e.g. `/v1/subscriptions/sub_`).
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.requests().iter().filter(|(m, p)| m == method && p.starts_with(prefix)).count()
    }
}

fn plan_entity(id: &str, period: &str, amount: i64, currency: &str) -> Value {
    json!({
        "id": id, "entity": "plan", "interval": 1, "period": period,
        "item": { "id": format!("item_{id}"), "active": true, "name": "Ferrocast Pro", "amount": amount,
                  "unit_amount": amount, "currency": currency },
        "notes": { "product": "ferrocast-pro" }, "created_at": now_s(),
    })
}

type St = State<Arc<Mutex<FakeState>>>;

/// Records the request and checks basic auth; `Some` is the error response to send.
fn gate(st: &St, headers: &HeaderMap, method: &str, path: String) -> Option<Response> {
    let mut s = st.lock().unwrap();
    s.requests.push((method.to_string(), path));
    if let Some(code) = s.fail_all {
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return Some(error(status, "The fake is answering errors on purpose."));
    }
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{KEY_ID}:{KEY_SECRET}").as_bytes())
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
        return Some(error(StatusCode::UNAUTHORIZED, "The api key provided is invalid"));
    }
    None
}

async fn create_plan(st: St, headers: HeaderMap, axum::Json(b): axum::Json<Value>) -> Response {
    if let Some(r) = gate(&st, &headers, "POST", "/v1/plans".into()) {
        return r;
    }
    let period = b["period"].as_str().unwrap_or("");
    let amount = b.pointer("/item/amount").and_then(Value::as_i64).unwrap_or(0);
    if !matches!(period, "daily" | "weekly" | "monthly" | "yearly") || amount < 100 || b["interval"] != 1 {
        return error(StatusCode::BAD_REQUEST, "The period, interval or amount is invalid.");
    }
    let currency = b.pointer("/item/currency").and_then(Value::as_str).unwrap_or("INR").to_string();
    let mut s = st.lock().unwrap();
    s.next += 1;
    let id = format!("plan_Fake{:010}", s.next);
    let mut e = plan_entity(&id, period, amount, &currency);
    e["notes"] = b["notes"].clone();
    s.plans.insert(id, e.clone());
    axum::Json(e).into_response()
}

async fn list_plans(st: St, headers: HeaderMap) -> Response {
    if let Some(r) = gate(&st, &headers, "GET", "/v1/plans".into()) {
        return r;
    }
    let s = st.lock().unwrap();
    let items: Vec<Value> = s.plans.values().cloned().collect();
    axum::Json(json!({ "entity": "collection", "count": items.len(), "items": items })).into_response()
}

async fn fetch_plan(st: St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = gate(&st, &headers, "GET", format!("/v1/plans/{id}")) {
        return r;
    }
    match st.lock().unwrap().plans.get(&id) {
        Some(p) => axum::Json(p.clone()).into_response(),
        None => error(StatusCode::BAD_REQUEST, "The id provided does not exist"),
    }
}

async fn create_subscription(st: St, headers: HeaderMap, axum::Json(b): axum::Json<Value>) -> Response {
    if let Some(r) = gate(&st, &headers, "POST", "/v1/subscriptions".into()) {
        return r;
    }
    let mut s = st.lock().unwrap();
    let plan = b["plan_id"].as_str().unwrap_or("").to_string();
    if !s.plans.contains_key(&plan) {
        return error(StatusCode::BAD_REQUEST, "The id provided does not exist");
    }
    if b["total_count"].as_i64().is_none_or(|n| n < 1) {
        return error(StatusCode::BAD_REQUEST, "The total count field is required.");
    }
    s.next += 1;
    let id = format!("sub_Fake{:010}", s.next);
    let e = json!({
        "id": id, "entity": "subscription", "plan_id": plan, "customer_id": null, "status": "created",
        "current_start": null, "current_end": null, "ended_at": null, "quantity": 1,
        "notes": b["notes"], "charge_at": null, "start_at": null, "end_at": null, "auth_attempts": 0,
        "total_count": b["total_count"], "paid_count": 0, "customer_notify": true, "created_at": now_s(),
        "expire_by": b["expire_by"], "short_url": format!("https://rzp.io/rzp/fake{}", s.next),
        "has_scheduled_changes": false, "change_scheduled_at": null, "remaining_count": b["total_count"],
    });
    s.subscriptions.insert(id, e.clone());
    axum::Json(e).into_response()
}

async fn fetch_subscription(st: St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = gate(&st, &headers, "GET", format!("/v1/subscriptions/{id}")) {
        return r;
    }
    match st.lock().unwrap().subscriptions.get(&id) {
        Some(e) => axum::Json(e.clone()).into_response(),
        None => error(StatusCode::BAD_REQUEST, "The id provided does not exist"),
    }
}

async fn cancel_subscription(
    st: St,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(b): axum::Json<Value>,
) -> Response {
    if let Some(r) = gate(&st, &headers, "POST", format!("/v1/subscriptions/{id}/cancel")) {
        return r;
    }
    let mut s = st.lock().unwrap();
    let Some(e) = s.subscriptions.get_mut(&id) else {
        return error(StatusCode::BAD_REQUEST, "The id provided does not exist");
    };
    let status = e["status"].as_str().unwrap_or("").to_string();
    if !matches!(status.as_str(), "created" | "authenticated" | "active" | "pending" | "halted" | "paused") {
        return error(StatusCode::BAD_REQUEST, &format!("Subscription is not cancellable in {status} status."));
    }
    let at_end = b["cancel_at_cycle_end"] == 1 || b["cancel_at_cycle_end"] == true;
    if at_end && status == "active" {
        // Razorpay keeps it active until the period ends, then cancels it.
        e["has_scheduled_changes"] = json!(true);
        e["change_scheduled_at"] = e["current_end"].clone();
    } else {
        e["status"] = json!("cancelled");
        e["ended_at"] = json!(now_s());
    }
    axum::Json(e.clone()).into_response()
}

async fn payment_refunds(st: St, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = gate(&st, &headers, "GET", format!("/v1/payments/{id}/refunds")) {
        return r;
    }
    let items = st.lock().unwrap().refunds.get(&id).cloned().unwrap_or_default();
    axum::Json(json!({ "entity": "collection", "count": items.len(), "items": items })).into_response()
}
