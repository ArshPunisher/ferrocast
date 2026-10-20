//! The Control Center's Pro tabs and the app's codes, sales and prices, through real
//! requests against fake Razorpay (plans are made there, never at the real Razorpay).

use ferrocast_testkit::razorpay::{FakeRazorpay, KEY_ID, KEY_SECRET};
use serde_json::{Value, json};

use crate::admin::AdminConfig;
use crate::billing::BillingConfig;
use crate::{AppState, Config, Db, now_ms, router};

/// 64 hex characters, built in pieces so it never looks like a real secret.
fn token() -> String {
    ["fedcba9876543210"; 4].concat()
}
const ADMIN: &str = "Owner@Example.com";
const EMAIL_KEY: &str = "email-key-for-tests-0123456789abcdef";

struct T {
    base: String,
    rzp: FakeRazorpay,
    st: AppState,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn start(open: bool) -> T {
    let rzp = FakeRazorpay::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let billing = BillingConfig {
        key_id: Some(KEY_ID.into()),
        key_secret: Some(KEY_SECRET.into()),
        webhook_secret: Some("whsec-test-0123456789abcdef".into()),
        email_key: Some(EMAIL_KEY.into()),
        plan_monthly: Some(rzp.add_plan("monthly", 19_900)),
        plan_yearly: Some(rzp.add_plan("yearly", 199_900)),
        api_url: rzp.base.clone(),
        sales_open: open,
    };
    let st = AppState::new(cfg, db).with_billing(billing).with_admin(AdminConfig::new(Some(&token()), None));
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    T { base, rzp, st, http: reqwest::Client::new(), _dir: dir }
}

impl T {
    async fn admin(&self, method: &str, path: &str, body: Option<Value>, email: Option<&str>) -> (u16, Value) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut r = self.http.request(m, format!("{}/admin/v1{path}", self.base)).bearer_auth(token());
        if let Some(e) = email {
            r = r.header("x-admin-email", e);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }
    async fn get(&self, path: &str) -> Value {
        let (s, v) = self.admin("GET", path, None, None).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }
    async fn write(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        self.admin(method, path, Some(body), Some(ADMIN)).await
    }
    /// A new install: (auth, install id).
    async fn install(&self) -> (String, String) {
        let v: Value = self.http.post(format!("{}/v1/installs", self.base)).send().await.unwrap().json().await.unwrap();
        let id = v["install_id"].as_str().unwrap().to_string();
        (format!("{id}.{}", v["install_secret"].as_str().unwrap()), id)
    }
    async fn app(&self, path: &str, auth: &str, body: Value) -> (u16, Value) {
        let res = self.http.post(format!("{}{path}", self.base)).bearer_auth(auth).json(&body).send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }
    async fn status(&self, auth: &str) -> Value {
        self.http
            .get(format!("{}/v1/pro/status", self.base))
            .bearer_auth(auth)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    fn plan_amount(&self, sub: &str) -> i64 {
        let s = self.rzp.subscription(sub).unwrap();
        let plan = s["plan_id"].as_str().unwrap().to_string();
        self.rzp.state.lock().unwrap().plans[&plan]["item"]["amount"].as_i64().unwrap()
    }
}

#[tokio::test]
async fn writes_need_the_admin_email_and_everything_needs_the_token() {
    let t = start(true).await;
    let (_, install) = t.install().await;
    let body = json!({ "install_id": install, "days": 7 });
    let (s, v) = t.admin("POST", "/grants", Some(body.clone()), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("admin_email_required")), "{v}");
    let (s, _) = t.admin("POST", "/grants", Some(body.clone()), Some("not-an-email")).await;
    assert_eq!(s, 400);
    let res = t.http.get(format!("{}/admin/v1/users", t.base)).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = t
        .http
        .get(format!("{}/admin/v1/codes", t.base))
        .bearer_auth(token())
        .header("cf-ray", "x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // Reads don't need the email.
    t.get("/users").await;
    // A bad query is a 400 with the error shape.
    let (s, v) = t.admin("GET", "/subscriptions?status=weird", None, None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_query")));
    let (s, v) = t.admin("GET", "/users/sub_nope", None, None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn give_pro_list_it_revoke_it_and_audit_it() {
    let t = start(true).await;
    let (auth, install) = t.install().await;
    let (s, v) = t.write("POST", "/grants", json!({ "install_id": install, "days": 14, "note": "Giveaway" })).await;
    assert_eq!(s, 201, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("comp_"));
    assert_eq!(t.status(&auth).await["pro"], true);
    let (s, v) = t.write("POST", "/grants", json!({ "install_id": "nobody", "days": 14 })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("unknown_install")));
    for bad in [
        json!({ "install_id": install, "days": 0 }),
        json!({ "install_id": install, "days": 91 }),
        json!({ "email": "a@b.co", "days": 3 }),
    ] {
        assert_eq!(t.write("POST", "/grants", bad.clone()).await.0, 422, "{bad}");
    }
    let grants = t.get("/grants").await;
    assert_eq!(grants[0]["id"], id.as_str());
    assert_eq!(grants[0]["source"], "admin");
    assert_eq!(grants[0]["created_by"], "owner@example.com", "lower-cased");
    assert_eq!(grants[0]["note"], "Giveaway");
    assert!(grants[0]["install"].as_str().unwrap().ends_with('…'), "masked");
    assert!(!grants.to_string().contains(&install), "never the raw install id");
    let users = t.get(&format!("/users?q={install}")).await;
    assert_eq!(users["total"], 1, "an install id finds its subscriber");
    assert_eq!(users["items"][0]["source"], "grant");
    assert_eq!(users["items"][0]["email"], Value::Null);
    let detail = t.get(&format!("/users/{id}")).await;
    assert_eq!(detail["entitlements"][0]["source"], "admin");
    assert_eq!(detail["subscriptions"], json!([]));
    let (s, v) = t.write("POST", &format!("/grants/{id}/revoke"), json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(t.status(&auth).await["pro"], false);
    let (s, v) = t.write("POST", &format!("/entitlements/{id}/revoke"), json!({})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_ended")));
    assert_eq!(t.write("POST", "/grants/sub_real/revoke", json!({})).await.0, 404);
    let audit = t.get("/audit").await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().filter_map(|a| a["action"].as_str()).collect();
    assert_eq!(actions, ["grant.revoke", "grant.create"]);
}

#[tokio::test]
async fn grant_codes_are_redeemed_in_the_app_and_labelled() {
    let t = start(false).await;
    let (s, v) =
        t.write("POST", "/codes", json!({ "kind": "grant", "days": 10, "code": "friends-1", "max_uses": 1 })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["code"], "FRIENDS-1");
    assert_eq!(t.write("POST", "/codes", json!({ "kind": "grant", "days": 10, "code": "FRIENDS-1" })).await.0, 409);
    let (s, made) = t.write("POST", "/codes", json!({ "kind": "grant", "days": 3, "prefix": "FC-" })).await;
    assert_eq!(s, 201);
    assert!(made["code"].as_str().unwrap().starts_with("FC-"));
    let (a1, _) = t.install().await;
    let (a2, _) = t.install().await;
    let (s, v) = t.app("/v1/pro/redeem", &a1, json!({ "code": " friends-1 " })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["pro"].as_bool(), v["redeemed"]["days"].as_i64()), (Some(true), Some(10)));
    let (s, v) = t.app("/v1/pro/redeem", &a1, json!({ "code": "FRIENDS-1" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("code_used_up")), "max_uses reached first");
    let (s, v) = t.app("/v1/pro/redeem", &a2, json!({ "code": "NOPE-NOPE" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("code_not_found")));
    let (s, _) = t.write("POST", "/codes/FRIENDS-1/disable", json!({})).await;
    assert_eq!(s, 200);
    let codes = t.get("/codes").await;
    let c = codes.as_array().unwrap().iter().find(|c| c["code"] == "FRIENDS-1").unwrap();
    assert_eq!((c["used"].as_i64(), c["active"].as_bool()), (Some(1), Some(false)));
    let grants = t.get("/grants").await;
    assert_eq!((grants[0]["source"].as_str(), grants[0]["code"].as_str()), (Some("code"), Some("FRIENDS-1")));
    for bad in [
        json!({ "kind": "grant" }),
        json!({ "kind": "discount", "percent_off": 95 }),
        json!({ "kind": "x", "days": 3 }),
        json!({ "kind": "grant", "days": 3, "until": "2001-01-01" }),
        json!({ "kind": "grant", "days": 3, "code": "a b" }),
    ] {
        assert_eq!(t.write("POST", "/codes", bad.clone()).await.0, 422, "{bad}");
    }
}

#[tokio::test]
async fn a_discount_code_makes_a_cheaper_plan_and_the_buyer_keeps_it() {
    let t = start(true).await;
    let (s, v) = t
        .write(
            "POST",
            "/codes",
            json!({ "kind": "discount", "percent_off": 30, "period": "monthly", "code": "SAVE30" }),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(t.rzp.count("POST", "/v1/plans"), 1, "its plan is made up front");
    let (auth, _) = t.install().await;
    let (s, v) = t.app("/v1/pro/redeem", &auth, json!({ "code": "save30" })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["kind"], "discount");
    assert_eq!(v["prices"]["monthly"], json!({ "list": 19_900, "amount": 13_900, "code_applies": true }));
    assert_eq!(v["prices"]["yearly"]["code_applies"], false);
    let (s, v) =
        t.app("/v1/pro/checkout", &auth, json!({ "plan": "yearly", "email": "a@b.co", "code": "SAVE30" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("code_not_for_plan")));
    let (s, v) =
        t.app("/v1/pro/checkout", &auth, json!({ "plan": "monthly", "email": "a@b.co", "code": "SAVE30" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(
        (v["amount"].as_i64(), v["list_amount"].as_i64(), v["code_applied"].as_bool()),
        (Some(13_900), Some(19_900), Some(true))
    );
    let sub = v["subscription_id"].as_str().unwrap().to_string();
    assert_eq!(t.plan_amount(&sub), 13_900, "Razorpay bills the discounted plan on every renewal");
    assert_eq!(t.rzp.count("POST", "/v1/plans"), 1, "the plan is reused");
    let subs = t.get("/subscriptions?status=created").await;
    assert_eq!((subs[0]["amount"].as_i64(), subs[0]["code"].as_str()), (Some(13_900), Some("SAVE30")));
    // A grant code at checkout is pointed to redeem instead.
    t.write("POST", "/codes", json!({ "kind": "grant", "days": 3, "code": "FREEBIE" })).await;
    let (b2, _) = t.install().await;
    let (s, v) =
        t.app("/v1/pro/checkout", &b2, json!({ "plan": "monthly", "email": "a@b.co", "code": "FREEBIE" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("grant_code")));
}

#[tokio::test]
async fn a_new_price_is_for_new_buyers_and_sales_lower_it_for_a_while() {
    let t = start(true).await;
    let (a1, _) = t.install().await;
    let (_, v) = t.app("/v1/pro/checkout", &a1, json!({ "plan": "monthly", "email": "a@b.co" })).await;
    let old_sub = v["subscription_id"].as_str().unwrap().to_string();
    assert_eq!(t.plan_amount(&old_sub), 19_900);
    let (s, v) = t.write("PUT", "/prices/pro/monthly/INR", json!({ "amount": 24_900, "expected_amount": 10 })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("price_changed")));
    let (s, v) =
        t.write("PUT", "/prices/pro/monthly/INR", json!({ "amount": 24_900, "expected_amount": 19_900 })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["gateway_ready"], true);
    assert_eq!(t.write("PUT", "/prices/pro/weekly/INR", json!({ "amount": 100 })).await.0, 404);
    assert_eq!(t.write("PUT", "/prices/pro/monthly/INR", json!({ "amount": 1_500_000 })).await.0, 422);
    assert_eq!(t.plan_amount(&old_sub), 19_900, "the earlier buyer's plan is untouched");
    let (a2, _) = t.install().await;
    let (_, v) = t.app("/v1/pro/checkout", &a2, json!({ "plan": "monthly", "email": "a@b.co" })).await;
    assert_eq!(v["amount"], 24_900);
    let pricing = t.get("/pricing").await;
    assert_eq!(
        (pricing["prices"][0]["amount"].as_i64(), pricing["prices"][0]["default"].as_bool()),
        (Some(24_900), Some(false))
    );
    assert_eq!(pricing["history"][0]["created_by"], "owner@example.com");

    // A sale: 20% off monthly from now for a day.
    let ends = now_ms() + 86_400_000;
    let sale = json!({ "name": "Diwali", "headline": "20% off", "discounts": { "pro_monthly": 20 }, "currencies": ["INR"],
                       "starts_at": null, "ends_at": ends, "show_countdown": true, "show_banner": false, "theme": "festive" });
    let (s, v) = t.write("POST", "/sales", sale.clone()).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!((v["state"].as_str(), v["amounts"]["pro_monthly"].as_i64()), (Some("live"), Some(19_900)));
    let sale_id = v["id"].as_str().unwrap().to_string();
    let (s, v) = t.write("POST", "/sales", sale.clone()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("sale_overlap")));
    let st = t.status(&a1).await;
    assert_eq!(st["prices"]["monthly"], 19_900);
    assert_eq!(st["sale"]["was"]["monthly"], 24_900);
    assert_eq!(st["sale"]["name"], "Diwali");
    let (a3, _) = t.install().await;
    let (_, v) = t.app("/v1/pro/checkout", &a3, json!({ "plan": "monthly", "email": "a@b.co" })).await;
    assert_eq!((v["amount"].as_i64(), v["sale"].as_str()), (Some(19_900), Some(sale_id.as_str())));
    // The website's prices: CORS for the site only, cached a minute.
    let res = t
        .http
        .get(format!("{}/v1/pro/prices", t.base))
        .header("origin", "https://ferrocast.clearcraftlabs.com")
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()["access-control-allow-origin"], "https://ferrocast.clearcraftlabs.com");
    assert_eq!(res.headers()["cache-control"], "public, max-age=60");
    let p: Value = res.json().await.unwrap();
    assert_eq!(p["prices"]["monthly"], 19_900);
    assert_eq!(p["sale"]["ends_at"], ends);
    let res =
        t.http.get(format!("{}/v1/pro/prices", t.base)).header("origin", "https://evil.example").send().await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
    // Edit, stop, and it's over.
    let mut edit = sale.clone();
    edit["discounts"] = json!({ "pro_monthly": 40 });
    let (s, v) = t.write("PUT", &format!("/sales/{sale_id}"), edit.clone()).await;
    assert_eq!((s, v["amounts"]["pro_monthly"].as_i64()), (200, Some(14_900)));
    let (s, v) = t.write("POST", &format!("/sales/{sale_id}/stop"), json!({})).await;
    assert_eq!((s, v["state"].as_str()), (200, Some("stopped")));
    assert_eq!(t.write("PUT", &format!("/sales/{sale_id}"), edit).await.0, 409);
    assert_eq!(t.status(&a1).await["sale"], Value::Null);
    for bad in [
        json!({ "name": "" , "discounts": { "pro_monthly": 20 } }),
        json!({ "name": "x", "discounts": {} }),
        json!({ "name": "x", "discounts": { "pro_monthly": 20 }, "currencies": ["USD"] }),
        json!({ "name": "x", "discounts": { "pro_monthly": 20 }, "ends_at": 5 }),
        json!({ "name": "x", "discounts": { "pro_monthly": 20 }, "theme": "neon" }),
    ] {
        assert_eq!(t.write("POST", "/sales", bad.clone()).await.0, 422, "{bad}");
    }
    let audit = t.get("/audit?limit=10").await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "price.set" && a["detail"]["was"] == 19_900));
}

#[tokio::test]
async fn payments_and_refunds_show_for_paid_subscribers() {
    let t = start(true).await;
    let (auth, install) = t.install().await;
    let (_, v) = t.app("/v1/pro/checkout", &auth, json!({ "plan": "monthly", "email": "a@b.co" })).await;
    let sub = v["subscription_id"].as_str().unwrap().to_string();
    t.rzp.activate(&sub, 30);
    t.st.db
        .record_pro_charge(
            "evt_1",
            &sub,
            Some(("pay_T1", 19_900)),
            &crate::billing::store::Synced {
                status: "active".into(),
                current_end: Some(now_ms() + 86_400_000),
                cancel_at_end: None,
            },
        )
        .unwrap();
    t.st.db.put_refunds("pay_T1", &[("rfnd_T1".into(), 5_000, "processed".into(), now_ms())]).unwrap();
    let p = t.get("/payments?days=7").await;
    assert_eq!(p["payments"][0]["payment_id"], "pay_T1");
    assert_eq!((p["payments"][0]["refunded"].as_i64(), p["payments"][0]["email"].clone()), (Some(5_000), Value::Null));
    assert_eq!(
        p["totals"][0],
        json!({ "currency": "INR", "payments": 1, "gross": 19_900, "refunded": 5_000, "net": 14_900 })
    );
    assert_eq!(t.get("/payments?refunded_only=true").await["payments"].as_array().unwrap().len(), 1);
    let d = t.get(&format!("/users/{sub}")).await;
    assert_eq!(d["subscription"]["source"], "paid");
    assert_eq!(d["refunds"][0]["amount"], 5_000);
    assert_eq!(d["installs"][0]["current"], true);
    assert!(!d.to_string().contains(&install), "install ids stay masked");
    let subs = t.get("/subscriptions").await;
    assert_eq!(subs[0]["gateway_subscription_id"], sub.as_str());
    // A paid install can't be given a free grant or redeem one.
    let (s, v) = t.write("POST", "/grants", json!({ "install_id": install, "days": 5 })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("has_subscription")));
    t.write("POST", "/codes", json!({ "kind": "grant", "days": 3, "code": "FREEBIE" })).await;
    let (s, v) = t.app("/v1/pro/redeem", &auth, json!({ "code": "FREEBIE" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_pro")));
}

#[test]
fn times_read_and_write_in_utc() {
    use super::{iso, time_in};
    assert_eq!(iso(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso(1_792_281_600_000), "2026-10-18T00:00:00Z");
    assert_eq!(time_in(&json!("2026-10-18"), true), Ok(Some(1_792_367_999_000)), "through the end of the day");
    assert_eq!(time_in(&json!("2026-10-18T00:00:00.000Z"), false), Ok(Some(1_792_281_600_000)));
    assert_eq!(time_in(&json!(null), false), Ok(None));
    assert!(time_in(&json!("2026-10-18"), false).is_err());
    assert!(time_in(&json!("soon"), true).is_err());
}
