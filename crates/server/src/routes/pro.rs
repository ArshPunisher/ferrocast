//! Ferrocast Pro for the dashboard's Settings › Pro (API.md §4.15). The engine asks the
//! relay; these routes only shape requests and answers. Pro is never needed for anything
//! the free app does (R-05).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

/// `GET /pro`: status (remembered for 10 minutes; offline it holds for 7 days).
pub async fn status(State(app): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(app.engine.pro_status(false).await).unwrap_or(Value::Null))
}

/// `POST /pro/refresh`: ask the relay (and Razorpay) now, e.g. after paying.
pub async fn refresh(State(app): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(app.engine.pro_status(true).await).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize)]
pub struct Checkout {
    plan: String,
    /// Optional while signed in to a Ferrocast account (its email is used).
    #[serde(default)]
    email: String,
    /// A discount code (optional).
    #[serde(default)]
    code: Option<String>,
}

/// `POST /pro/checkout {plan, email}` → 201 `{checkout_url, …}` to open in the browser.
pub async fn checkout(State(app): State<AppState>, Body(b): Body<Checkout>) -> ApiResult<(StatusCode, Json<Value>)> {
    if !matches!(b.plan.as_str(), "monthly" | "yearly") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_plan",
            "Pick the monthly or the yearly plan.",
        ));
    }
    let mut b = b;
    if b.email.trim().is_empty()
        && let Some(e) = app.engine.account_email().await
    {
        b.email = e;
    }
    if b.email.trim().is_empty() || b.email.len() > 254 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_email",
            "Add the email for Razorpay's receipts.",
        ));
    }
    if b.code.as_ref().is_some_and(|c| c.len() > 64) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_code", "That code is too long."));
    }
    let v = app.engine.pro_checkout(&b.plan, &b.email, b.code.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(v)))
}

/// `POST /pro/manage` → 201 `{url, expires_at}`: the relay's manage page, valid 30 minutes.
pub async fn manage(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(app.engine.pro_manage_link().await?)))
}

/// `POST /pro/cancel`: stop renewing at the end of the paid period.
pub async fn cancel(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let v = app.engine.pro_cancel().await?;
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}

#[derive(Debug, Deserialize)]
pub struct Redeem {
    code: String,
}

/// `POST /pro/redeem {code}`: a grant code gives free Pro; a discount code says what it
/// takes off, for checkout.
pub async fn redeem(State(app): State<AppState>, Body(b): Body<Redeem>) -> ApiResult<Json<Value>> {
    if b.code.trim().is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_code", "Type the code first."));
    }
    if b.code.len() > 64 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_code", "That code is too long."));
    }
    Ok(Json(app.engine.pro_redeem(&b.code).await?))
}

#[derive(Debug, Deserialize)]
pub struct Restore {
    subscription_id: String,
    email: String,
}

/// `POST /pro/restore {subscription_id, email}`: move Pro to this install.
pub async fn restore(State(app): State<AppState>, Body(b): Body<Restore>) -> ApiResult<Json<Value>> {
    if b.subscription_id.len() > 64 || b.email.len() > 254 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "That subscription id or email is too long.",
        ));
    }
    let v = app.engine.pro_restore(&b.subscription_id, &b.email).await?;
    Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null)))
}

#[derive(Debug, Deserialize, Default)]
pub struct SignIn {
    /// What this PC is called in the account's device list (optional, at most 40 characters).
    #[serde(default)]
    label: Option<String>,
}

/// `GET /pro/account?refresh=1`: the Ferrocast account on this PC (ACCOUNTS.md §5): `state` is
/// `signed_out`, `pending` (the browser is open), `signed_in` or `disabled`.
pub async fn account(
    State(app): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let refresh = q.get("refresh").is_some_and(|v| v == "1" || v == "true");
    Json(app.engine.account_status(refresh).await)
}

/// `POST /pro/account/sign-in {label?}` → 201 `{url, expires_at}` to open in the browser.
pub async fn account_sign_in(
    State(app): State<AppState>,
    Body(b): Body<SignIn>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if b.label.as_ref().is_some_and(|l| l.chars().count() > 40) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", "Keep the PC's name to 40 characters."));
    }
    Ok((StatusCode::CREATED, Json(app.engine.account_sign_in(b.label).await?)))
}

/// `POST /pro/account/sign-in/cancel`: stop waiting for the browser.
pub async fn account_sign_in_cancel(State(app): State<AppState>) -> StatusCode {
    app.engine.account_sign_in_cancel();
    StatusCode::NO_CONTENT
}

#[derive(Debug, Deserialize, Default)]
pub struct SignOut {
    #[serde(default)]
    everywhere: bool,
}

/// `POST /pro/account/sign-out {everywhere}`.
pub async fn account_sign_out(State(app): State<AppState>, Body(b): Body<SignOut>) -> ApiResult<StatusCode> {
    app.engine.account_sign_out(b.everywhere).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /pro/account/sessions`: the account's signed-in apps and browsers.
pub async fn account_sessions(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.account_sessions().await?))
}

/// `DELETE /pro/account/sessions/{id}`: sign one of them out.
pub async fn account_revoke(
    State(app): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<StatusCode> {
    app.engine.account_revoke(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay → fake Razorpay.
#[cfg(test)]
mod tests {
    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::BillingConfig;
    use ferrocast_testkit::razorpay::{FakeRazorpay, KEY_ID, KEY_SECRET};
    use reqwest::StatusCode;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    const WEBHOOK_SECRET: &str = "whsec-e2e-0123456789";

    struct World {
        base: String,
        relay: String,
        token: String,
        http: reqwest::Client,
        rzp: FakeRazorpay,
        engine: Engine,
        relay_db: std::sync::Arc<ferrocast_relay::Db>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    async fn world(sales_open: bool, relay_up: bool) -> World {
        let rzp = FakeRazorpay::start().await;
        let billing = BillingConfig {
            key_id: Some(KEY_ID.into()),
            key_secret: Some(KEY_SECRET.into()),
            webhook_secret: Some(WEBHOOK_SECRET.into()),
            email_key: Some("email-key-for-e2e-tests-0123456789abcdef".into()),
            plan_monthly: Some(rzp.add_plan("monthly", 19_900)),
            plan_yearly: Some(rzp.add_plan("yearly", 199_900)),
            api_url: rzp.base.clone(),
            sales_open,
        };
        let rdir = tempfile::tempdir().unwrap();
        let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = format!("http://{}", rl.local_addr().unwrap());
        let cfg = ferrocast_relay::Config {
            public_url: relay.clone(),
            tip_url: None,
            kick_client_id: None,
            kick_client_secret: None,
            kick_id_url: relay.clone(),
            kick_api_url: relay.clone(),
        };
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_billing(billing);
        let relay_db = state.db.clone();
        let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        if relay_up {
            tokio::spawn(async move { axum::serve(rl, rapp).await });
        } else {
            drop(rl); // nothing listens: the relay is "down"
        }
        let dir = tempfile::tempdir().unwrap();
        let apps = PlatformApps { relay_url: Some(relay.clone()), ..Default::default() };
        let engine =
            Engine::start_with(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()), apps)
                .await
                .unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        World {
            base: server.base_url(),
            relay,
            token: server.sessions.desktop_token().to_string(),
            http: reqwest::Client::new(),
            rzp,
            engine,
            relay_db,
            _dirs: (dir, rdir),
        }
    }

    impl World {
        async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
            let mut r = self.http.request(method, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            let res = r.send().await.unwrap();
            let s = res.status();
            (s, res.json().await.unwrap_or(Value::Null))
        }

        async fn webhook(&self, event_id: &str, sub: &str) {
            let body = json!({ "event": "subscription.activated",
                               "payload": { "subscription": { "entity": { "id": sub } } } })
            .to_string();
            let res = self
                .http
                .post(format!("{}/billing/razorpay/webhook", self.relay))
                .header(
                    "x-razorpay-signature",
                    ferrocast_relay::billing::razorpay::sign(WEBHOOK_SECRET, body.as_bytes()),
                )
                .header("x-razorpay-event-id", event_id)
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
        }
    }

    fn add_code(w: &World, code: &str, kind: &str) {
        use ferrocast_relay::billing::codes::CodeRow;
        let grant = kind == "grant";
        w.relay_db
            .add_code(&CodeRow {
                code: code.into(),
                kind: kind.into(),
                days: grant.then_some(7),
                percent_off: (!grant).then_some(30),
                applies_period: None,
                max_uses: None,
                valid_until: None,
                active: true,
                note: None,
                created_by: "owner@example.com".into(),
                created_at: 1,
                used: 0,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn codes_redeem_through_the_dashboard_and_a_discount_reaches_checkout() {
        let w = world(true, true).await;
        add_code(&w, "FRIENDS", "grant");
        add_code(&w, "SAVE30", "discount");
        let (s, v) = w.call(reqwest::Method::POST, "/pro/redeem", Some(json!({ "code": "save30" }))).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!((v["kind"].as_str(), v["prices"]["monthly"]["amount"].as_i64()), (Some("discount"), Some(13_900)));
        let body = json!({ "plan": "monthly", "email": "a@b.co", "code": "SAVE30" });
        let (s, v) = w.call(reqwest::Method::POST, "/pro/checkout", Some(body)).await;
        assert_eq!(s, 201, "{v}");
        assert_eq!((v["amount"].as_i64(), v["code_applied"].as_bool()), (Some(13_900), Some(true)));
        let (s, v) = w.call(reqwest::Method::POST, "/pro/redeem", Some(json!({ "code": "NOPE1" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (404, Some("code_not_found")), "{v}");
        let (s, v) = w.call(reqwest::Method::POST, "/pro/redeem", Some(json!({ "code": "FRIENDS" }))).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!((v["kind"].as_str(), v["status"]["active"].as_bool()), (Some("grant"), Some(true)));
        let (_, v) = w.call(reqwest::Method::GET, "/pro", None).await;
        assert_eq!(v["active"], true, "the redeemed Pro is remembered");
        assert_eq!(v["prices"]["monthly"], 19_900);
    }

    #[tokio::test]
    async fn settings_pro_shows_not_on_sale_and_refuses_checkout() {
        let w = world(false, true).await;
        let (s, v) = w.call(reqwest::Method::GET, "/pro", None).await;
        assert_eq!(s, 200);
        assert_eq!(
            (v["active"].as_bool(), v["status"].as_str(), v["sales_open"].as_bool()),
            (Some(false), Some("none"), Some(false))
        );
        assert!(v["launch_note"].as_str().unwrap().contains("after version 1"));
        assert_eq!(v["offline_grace_days"], 7);
        // The relay's feature list and counters come through (history is built, M1).
        assert_eq!(
            v["features"],
            json!({ "cloud_bot": false, "remote": false, "tts": false, "discord": false, "backups": false,
                    "tip_brand": false, "history": true, "weekly_email": false, "smart": false })
        );
        assert_eq!(v["quotas"], json!({}));
        let (s, v) =
            w.call(reqwest::Method::POST, "/pro/checkout", Some(json!({ "plan": "monthly", "email": "a@b.co" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (409, Some("pro_not_on_sale")));
        assert!(
            v["error"]["hint"].as_str().unwrap().contains("after version 1"),
            "the relay's hint reaches the dashboard"
        );
        let (s, _) = w.call(reqwest::Method::POST, "/pro/manage", None).await;
        assert_eq!(s, 404, "nothing to manage yet");
    }

    #[tokio::test]
    async fn buying_pro_end_to_end_and_it_holds_offline() {
        let w = world(true, true).await;
        let (s, v) = w
            .call(reqwest::Method::POST, "/pro/checkout", Some(json!({ "plan": "lifetime", "email": "a@b.co" })))
            .await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid_plan")));
        let (s, v) = w.call(reqwest::Method::POST, "/pro/checkout", Some(json!({ "plan": "monthly" }))).await;
        // Not signed in to an account: the email is needed (422, not a bare 400).
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid_email")), "{v}");
        let (s, v) = w
            .call(
                reqwest::Method::POST,
                "/pro/checkout",
                Some(json!({ "plan": "monthly", "email": "streamer@example.com" })),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        assert!(v["checkout_url"].as_str().unwrap().starts_with("https://rzp.io/"));
        let sub = v["subscription_id"].as_str().unwrap().to_string();
        // Paid at Razorpay; the webhook arrives; Refresh shows it.
        w.rzp.activate(&sub, 30);
        w.webhook("evt_e2e_1", &sub).await;
        let (_, v) = w.call(reqwest::Method::POST, "/pro/refresh", None).await;
        assert_eq!(
            (v["active"].as_bool(), v["status"].as_str(), v["stale"].as_bool()),
            (Some(true), Some("active"), Some(false))
        );
        assert!(w.engine.is_pro().await);
        let (s, v) = w.call(reqwest::Method::POST, "/pro/manage", None).await;
        assert_eq!(s, 201);
        assert!(v["url"].as_str().unwrap().contains("/pro/manage/"));
        // Razorpay down: cancelling says so (502), and nothing changes.
        w.rzp.fail_all(Some(500));
        let (s, v) = w.call(reqwest::Method::POST, "/pro/cancel", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (502, Some("razorpay_unavailable")));
        w.rzp.fail_all(None);
        let (s, v) = w.call(reqwest::Method::POST, "/pro/cancel", None).await;
        assert_eq!(s, 200);
        assert_eq!((v["active"].as_bool(), v["cancel_at_period_end"].as_bool()), (Some(true), Some(true)));
        let (s, v) = w.call(reqwest::Method::POST, "/pro/cancel", None).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (409, Some("already_cancelled")));
    }

    #[tokio::test]
    async fn with_the_relay_down_pro_is_off_and_actions_say_why() {
        let w = world(true, false).await;
        let (s, v) = w.call(reqwest::Method::GET, "/pro", None).await;
        assert_eq!(s, 200, "a status, not an error");
        assert_eq!((v["active"].as_bool(), v["stale"].as_bool()), (Some(false), Some(true)));
        assert!(v["warning"].as_str().is_some());
        let (s, v) =
            w.call(reqwest::Method::POST, "/pro/checkout", Some(json!({ "plan": "yearly", "email": "a@b.co" }))).await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (502, Some("relay_unreachable")));
        assert!(v["error"]["hint"].as_str().unwrap().contains("keeps working"));
        let (s, v) = w
            .call(
                reqwest::Method::POST,
                "/pro/restore",
                Some(json!({ "subscription_id": "sub_x", "email": "x".repeat(300) })),
            )
            .await;
        assert_eq!((s.as_u16(), v["error"]["code"].as_str()), (422, Some("invalid")));
    }
}
