//! Pro billing: unit tests for the pure parts, then the relay's real routes against the
//! fake Razorpay in `ferrocast-testkit` (every error status the routes can answer).

use ferrocast_testkit::razorpay::{FakeRazorpay, KEY_ID, KEY_SECRET};
use serde_json::{Value, json};

use super::access::{self, Plan, SubState};
use super::razorpay::{sign, webhook_signature_ok};
use super::*;
use crate::{Db, router};

const WEBHOOK_SECRET: &str = "whsec-test-0123456789abcdef";
const EMAIL: &str = "Streamer@Example.com";
const EMAIL_KEY: &str = "email-key-for-tests-0123456789abcdef";

// ── Unit: signatures ─────────────────────────────────────────────────────────────────

#[test]
fn webhook_signature_matches_only_the_exact_body_and_secret() {
    let body = br#"{"event":"subscription.activated"}"#;
    let sig = sign(WEBHOOK_SECRET, body);
    assert_eq!(sig.len(), 64);
    assert!(webhook_signature_ok(WEBHOOK_SECRET, body, &sig));
    assert!(webhook_signature_ok(WEBHOOK_SECRET, body, &sig.to_uppercase()), "hex case doesn't matter");
    assert!(webhook_signature_ok(WEBHOOK_SECRET, body, &format!(" {sig}\n")), "surrounding space is trimmed");
    // Tampered body, other secret, truncated, padded, non-hex, empty.
    assert!(!webhook_signature_ok(WEBHOOK_SECRET, br#"{"event":"subscription.activatee"}"#, &sig));
    assert!(!webhook_signature_ok("another-secret", body, &sig));
    assert!(!webhook_signature_ok(WEBHOOK_SECRET, body, &sig[..63]));
    assert!(!webhook_signature_ok(WEBHOOK_SECRET, body, &format!("{sig}00")));
    let plus = format!("+{}", &sig[1..]);
    assert!(!webhook_signature_ok(WEBHOOK_SECRET, body, &plus), "only hex digits");
    assert!(!webhook_signature_ok(WEBHOOK_SECRET, body, ""));
    assert!(!webhook_signature_ok("", body, &sign("", body)), "an empty secret never matches");
    // One flipped digit anywhere fails (the compare is constant-time `verify_slice`, so
    // where the difference is doesn't change how long it takes).
    for i in [0, 31, 63] {
        let mut s: Vec<char> = sig.chars().collect();
        s[i] = if s[i] == '0' { '1' } else { '0' };
        assert!(!webhook_signature_ok(WEBHOOK_SECRET, body, &s.into_iter().collect::<String>()), "digit {i}");
    }
}

// ── Unit: plans and access ───────────────────────────────────────────────────────────

#[test]
fn plan_amounts_match_the_price_list_and_stay_under_the_mandate_limit() {
    assert_eq!(Plan::Monthly.amount_paise(), 19_900, "₹199 a month");
    assert_eq!(Plan::Yearly.amount_paise(), 199_900, "₹1,999 a year");
    for p in Plan::ALL {
        assert!(p.amount_paise() < access::MANDATE_NO_OTP_LIMIT_PAISE, "{p:?} must stay under ₹15,000");
        assert_eq!(Plan::parse(p.key()), Some(p));
        assert_eq!(p.period(), p.key());
    }
    assert_eq!(Plan::Monthly.total_count(), 120);
    assert_eq!(Plan::Yearly.total_count(), 10);
    assert_eq!(Plan::parse("weekly"), None);
    assert_eq!(Plan::parse("Monthly"), None);
}

fn sub(status: &str, end: Option<i64>, cancel: bool, refunded: bool) -> SubState {
    SubState { status: status.into(), current_end: end, cancel_at_end: cancel, refunded }
}

#[test]
fn access_follows_every_subscription_state() {
    let end = 1_000_000_000_000;
    let g = access::GRACE_MS;
    assert_eq!(access::access_until(&sub("active", Some(end), false, false)), Some(end + g));
    assert_eq!(access::access_until(&sub("pending", Some(end), false, false)), Some(end + g), "renewal retrying");
    assert_eq!(access::access_until(&sub("active", Some(end), true, false)), Some(end), "cancelled: no grace");
    for ended in ["cancelled", "completed"] {
        assert_eq!(access::access_until(&sub(ended, Some(end), false, false)), Some(end), "{ended}: paid period");
    }
    for none in ["created", "authenticated", "halted", "paused", "expired", "something-new", ""] {
        assert_eq!(access::access_until(&sub(none, Some(end), false, false)), None, "{none}");
    }
    assert_eq!(access::access_until(&sub("active", None, false, false)), None, "no period yet");
    assert_eq!(access::access_until(&sub("active", Some(end), false, true)), None, "refunded ends it");
    assert!(access::is_pro(&sub("active", Some(end), false, false), end + g - 1));
    assert!(!access::is_pro(&sub("active", Some(end), false, false), end + g));
    assert!(!access::is_pro(&sub("cancelled", Some(end), false, false), end));
    assert!(access::is_pro(&sub("cancelled", Some(end), false, false), end - 1));
    assert!(access::is_pro(&sub("active", Some(i64::MAX), false, false), 0), "no overflow");
}

#[test]
fn emails_are_checked_plainly_and_hashed_case_blind() {
    for ok in ["a@b.co", "streamer+pro@example.in", "x.y@sub.example.com"] {
        assert!(valid_email(ok), "{ok}");
    }
    let long_local = format!("{}@example.com", "a".repeat(65));
    for bad in ["", "a", "@b.co", "a@", "a@b", "a@@b.co", "a b@c.co", "a\u{7}@b.co", "a@.co", "a@co.", &long_local] {
        assert!(!valid_email(bad), "{bad:?}");
    }
    let k = EMAIL_KEY;
    assert_eq!(email_hash(k, " Streamer@Example.COM "), email_hash(k, "streamer@example.com"));
    assert_ne!(email_hash(k, "a@b.co"), email_hash(k, "a@b.com"));
    assert_ne!(email_hash(k, "a@b.co"), email_hash("another-key-0123456789abcdef0123", "a@b.co"), "keyed");
    assert_eq!(email_hash(k, "a@b.co").len(), 64);
    assert!(!email_hash(k, "a@b.co").contains('@'), "never the email itself");
}

#[test]
fn rate_limits_count_per_key_and_kind() {
    let rt = Runtime::default();
    for _ in 0..3 {
        assert!(rt.allow("x", "k", 3, 60_000));
    }
    assert!(!rt.allow("x", "k", 3, 60_000));
    assert!(rt.allow("x", "other", 3, 60_000), "another key has its own count");
    assert!(rt.allow("y", "k", 3, 60_000), "another kind has its own count");
}

#[test]
fn dates_read_like_people_write_them() {
    assert_eq!(pages::date(0), "1 Jan 1970");
    assert_eq!(pages::date(1_792_281_600_000), "18 Oct 2026");
    assert_eq!(pages::date(951_782_400_000), "29 Feb 2000");
}

#[test]
fn config_debug_never_shows_secrets() {
    let c = BillingConfig {
        key_id: Some("rzp_test_abc".into()),
        key_secret: Some("SUPERSECRET".into()),
        webhook_secret: Some("WEBHOOKSECRET".into()),
        ..BillingConfig::default()
    };
    let d = format!("{c:?}");
    assert!(!d.contains("SUPERSECRET") && !d.contains("WEBHOOKSECRET"), "{d}");
    assert_eq!(c.mode(), Some("test"));
    assert!(!c.sales_open, "sales are closed unless switched on");
}

// ── Unit: idempotency in the database ────────────────────────────────────────────────

#[test]
fn an_event_id_changes_state_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    db.add_install("inst", "hash").unwrap();
    db.add_pro_sub("sub_1", "monthly", "eh", "created", "inst").unwrap();
    let active = store::Synced { status: "active".into(), current_end: Some(5), cancel_at_end: None };
    assert_eq!(db.sync_pro_sub(Some("evt_1"), "sub_1", &active).unwrap(), Applied::Done);
    let halted = store::Synced { status: "halted".into(), current_end: None, cancel_at_end: None };
    assert_eq!(db.sync_pro_sub(Some("evt_1"), "sub_1", &halted).unwrap(), Applied::Duplicate);
    let s = db.pro_sub("sub_1").unwrap().unwrap();
    assert_eq!((s.status.as_str(), s.current_end), ("active", Some(5)), "the replay changed nothing");
    assert!(db.billing_event_seen("evt_1").unwrap());
    assert_eq!(
        db.record_pro_charge("evt_1", "sub_1", Some(("pay_1", 19_900)), &active).unwrap(),
        Applied::Duplicate,
        "a charge event replayed records no payment"
    );
    assert_eq!(db.pro_payment("pay_1").unwrap(), None);
}

#[test]
fn a_subscription_is_shared_by_at_most_three_installs() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    for i in 0..5 {
        db.add_install(&format!("i{i}"), "h").unwrap();
    }
    db.add_pro_sub("sub_1", "yearly", "eh", "active", "i0").unwrap();
    for i in 1..5 {
        std::thread::sleep(std::time::Duration::from_millis(3));
        db.link_pro_install(&format!("i{i}"), "sub_1").unwrap();
    }
    assert_eq!(db.pro_installs("sub_1").unwrap(), store::MAX_INSTALLS);
    assert!(db.pro_sub_for_install("i0").unwrap().is_none(), "the oldest link dropped off");
    assert!(db.pro_sub_for_install("i4").unwrap().is_some());
}

// ── Integration: the relay's routes against fake Razorpay ────────────────────────────

struct Billing {
    base: String,
    rzp: FakeRazorpay,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn start(rzp: FakeRazorpay, cfg: BillingConfig) -> Billing {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let relay_cfg = crate::Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let state = crate::AppState::new(relay_cfg, db).with_billing(cfg);
    // A stand-in Pro feature route, to test the gate through real requests.
    let probe = axum::Router::new().route("/test/pro-only", axum::routing::get(gate_probe)).with_state(state.clone());
    let app = probe.merge(router(state)).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    Billing { base, rzp, http: reqwest::Client::new(), _dir: dir }
}

async fn gate_probe(State(st): State<AppState>, headers: HeaderMap) -> Result<axum::Json<Value>, ApiError> {
    let ctx = gate::require_pro(&st, &headers).await?;
    Ok(axum::Json(json!({ "install": ctx.install, "sub_id": ctx.sub_id })))
}

fn cfg(rzp: &FakeRazorpay, open: bool) -> BillingConfig {
    BillingConfig {
        key_id: Some(KEY_ID.into()),
        key_secret: Some(KEY_SECRET.into()),
        webhook_secret: Some(WEBHOOK_SECRET.into()),
        email_key: Some(EMAIL_KEY.into()),
        plan_monthly: Some(rzp.add_plan("monthly", 19_900)),
        plan_yearly: Some(rzp.add_plan("yearly", 199_900)),
        api_url: rzp.base.clone(),
        sales_open: open,
    }
}

async fn billing(open: bool) -> Billing {
    let rzp = FakeRazorpay::start().await;
    let c = cfg(&rzp, open);
    start(rzp, c).await
}

impl Billing {
    async fn install(&self) -> String {
        let v: Value = self.http.post(format!("{}/v1/installs", self.base)).send().await.unwrap().json().await.unwrap();
        format!("{}.{}", v["install_id"].as_str().unwrap(), v["install_secret"].as_str().unwrap())
    }

    async fn post(&self, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.post(format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn status(&self, auth: &str, refresh: bool) -> Value {
        let q = if refresh { "?refresh=1" } else { "" };
        let res = self.http.get(format!("{}/v1/pro/status{q}", self.base)).bearer_auth(auth).send().await.unwrap();
        assert_eq!(res.status(), 200);
        res.json().await.unwrap()
    }

    async fn probe(&self, auth: Option<&str>) -> (u16, Value) {
        let mut r = self.http.get(format!("{}/test/pro-only", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn checkout(&self, auth: &str, plan: &str) -> (u16, Value) {
        self.post("/v1/pro/checkout", Some(auth), Some(json!({ "plan": plan, "email": EMAIL }))).await
    }

    /// Sends a webhook the way Razorpay does; `sig: None` signs it properly.
    async fn hook(&self, event_id: &str, body: &Value, sig: Option<&str>) -> (u16, Value) {
        let raw = body.to_string();
        let sig = sig.map(String::from).unwrap_or_else(|| sign(WEBHOOK_SECRET, raw.as_bytes()));
        let res = self
            .http
            .post(format!("{}/billing/razorpay/webhook", self.base))
            .header("x-razorpay-signature", sig)
            .header("x-razorpay-event-id", event_id)
            .header("content-type", "application/json")
            .body(raw)
            .send()
            .await
            .unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    /// An install with an active monthly subscription (checkout, then Razorpay's charge).
    async fn pro_install(&self) -> (String, String) {
        let auth = self.install().await;
        let (s, v) = self.checkout(&auth, "monthly").await;
        assert_eq!(s, 201, "{v}");
        let id = v["subscription_id"].as_str().unwrap().to_string();
        self.rzp.activate(&id, 30);
        let (s, _) = self.hook(&format!("evt_act_{id}"), &sub_event("subscription.activated", &id), None).await;
        assert_eq!(s, 200);
        (auth, id)
    }
}

fn sub_event(event: &str, id: &str) -> Value {
    json!({
        "entity": "event", "account_id": "acc_Fake", "event": event, "contains": ["subscription"],
        "payload": { "subscription": { "entity": { "id": id, "entity": "subscription", "status": "stale-snapshot",
                                                    "notes": { "product": "ferrocast-pro" } } } },
        "created_at": now_ms() / 1000,
    })
}

fn charged_event(id: &str, payment: &str, amount: i64) -> Value {
    let mut v = sub_event("subscription.charged", id);
    v["contains"] = json!(["subscription", "payment"]);
    v["payload"]["payment"] = json!({ "entity": { "id": payment, "entity": "payment", "amount": amount, "currency": "INR",
                                                  "status": "captured" } });
    v
}

fn refund_event(payment: &str, amount: i64) -> Value {
    json!({
        "entity": "event", "event": "refund.processed", "contains": ["refund", "payment"],
        "payload": { "refund": { "entity": { "id": "rfnd_x", "payment_id": payment, "amount": amount, "status": "processed" } } },
        "created_at": now_ms() / 1000,
    })
}

#[tokio::test]
async fn smoke_the_relay_boots_and_billing_routes_answer() {
    let b = billing(false).await;
    let health = b.http.get(format!("{}/health", b.base)).send().await.unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.unwrap(), "ok");
    // Every billing route answers with the error shape, not a bare failure.
    for (method, path) in [
        ("GET", "/v1/pro/status"),
        ("POST", "/v1/pro/checkout"),
        ("POST", "/v1/pro/cancel"),
        ("POST", "/v1/pro/manage-link"),
        ("POST", "/v1/pro/restore"),
    ] {
        let req = if method == "GET" {
            b.http.get(format!("{}{path}", b.base))
        } else {
            b.http.post(format!("{}{path}", b.base))
        };
        let res = req.send().await.unwrap();
        assert_eq!(res.status(), 401, "{path} without an install");
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["error"]["code"], "unauthorized", "{path}");
    }
    let (s, v) = b.hook("evt_smoke", &json!({ "event": "payment.captured" }), Some("00")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("bad_signature")));
    let page = b.http.get(format!("{}/pro/manage/not-a-real-token-at-all-xx", b.base)).send().await.unwrap();
    assert_eq!(page.status(), 404);
}

#[tokio::test]
async fn a_new_install_has_no_pro_and_sees_the_prices() {
    let b = billing(false).await;
    let auth = b.install().await;
    let v = b.status(&auth, false).await;
    assert_eq!(v["pro"], false);
    assert_eq!(v["status"], "none");
    assert_eq!(v["sales_open"], false);
    assert_eq!(v["prices"], json!({ "currency": "INR", "monthly": 19_900, "yearly": 199_900 }));
}

#[tokio::test]
async fn status_lists_which_pro_features_this_server_has() {
    let b = billing(true).await;
    let none = b.status(&b.install().await, false).await;
    let (auth, _) = b.pro_install().await;
    let pro = b.status(&auth, false).await;
    for v in [&none, &pro] {
        // Nothing is built yet (M0), so every feature is off and there are no counters.
        assert_eq!(
            v["features"],
            json!({ "cloud_bot": false, "remote": false, "tts": false, "discord": false, "backups": false,
                    "tip_brand": false, "history": false }),
            "{v}"
        );
        assert_eq!(v["quotas"], json!({}));
    }
}

#[tokio::test]
async fn the_pro_gate_lets_only_current_subscribers_through() {
    let b = billing(true).await;
    let (s, v) = b.probe(None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")));
    let (s, v) = b.probe(Some("nobody.nothing")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")), "an unknown install");
    let free = b.install().await;
    let (s, v) = b.probe(Some(&free)).await;
    assert_eq!(s, 402, "{v}");
    assert_eq!(v["error"]["code"], "pro_required");
    assert_eq!(v["error"]["message"], "This runs on Ferrocast's servers and is part of Pro.");
    assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Pro"), "{v}");
    // An unpaid checkout isn't Pro either.
    let (s, _) = b.checkout(&free, "monthly").await;
    assert_eq!(s, 201);
    assert_eq!(b.probe(Some(&free)).await.0, 402, "checkout started but not paid");

    let (auth, id) = b.pro_install().await;
    let (s, v) = b.probe(Some(&auth)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["sub_id"], id.as_str());
    assert_eq!(v["install"].as_str(), auth.split('.').next());
    // Lapsed (every renewal failed): the gate closes with the status that says so.
    b.rzp.set_status(&id, "halted", None);
    b.hook("evt_gate_h", &sub_event("subscription.halted", &id), None).await;
    assert_eq!(b.status(&auth, false).await["pro"], false);
    assert_eq!(b.probe(Some(&auth)).await.0, 402);
    // Renewed again: open again.
    b.rzp.activate(&id, 30);
    b.hook("evt_gate_a", &sub_event("subscription.activated", &id), None).await;
    assert_eq!(b.probe(Some(&auth)).await.0, 200);
}

#[tokio::test]
async fn a_refund_closes_the_pro_gate() {
    let b = billing(true).await;
    let (auth, id) = b.pro_install().await;
    b.hook("evt_gc1", &charged_event(&id, "pay_Gate00001", 19_900), None).await;
    assert_eq!(b.probe(Some(&auth)).await.0, 200);
    b.rzp.add_refund("pay_Gate00001", 19_900, "processed");
    b.hook("evt_gr1", &refund_event("pay_Gate00001", 19_900), None).await;
    let (s, v) = b.probe(Some(&auth)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
}

#[tokio::test]
async fn checkout_is_refused_while_pro_is_not_on_sale() {
    let b = billing(false).await;
    let auth = b.install().await;
    let (s, v) = b.checkout(&auth, "monthly").await;
    assert_eq!(s, 409);
    assert_eq!(v["error"]["code"], "pro_not_on_sale");
    assert!(v["error"]["hint"].as_str().unwrap().contains("after version 1"));
    assert_eq!(b.rzp.count("POST", "/v1/subscriptions"), 0, "nothing reaches Razorpay");
}

#[tokio::test]
async fn checkout_makes_a_subscription_with_razorpays_link() {
    let b = billing(true).await;
    let auth = b.install().await;
    let (s, v) = b.checkout(&auth, "yearly").await;
    assert_eq!(s, 201, "{v}");
    let id = v["subscription_id"].as_str().unwrap();
    assert!(v["checkout_url"].as_str().unwrap().starts_with("https://rzp.io/"));
    assert_eq!(
        (v["amount"].as_i64(), v["currency"].as_str(), v["plan"].as_str()),
        (Some(199_900), Some("INR"), Some("yearly"))
    );
    assert_eq!(v["key_id"], KEY_ID, "the public key id, for Razorpay Checkout");
    let made = b.rzp.subscription(id).unwrap();
    assert_eq!(made["total_count"], 10);
    assert_eq!(made["notes"]["product"], "ferrocast-pro");
    assert_eq!(made["notes"]["email_hash"], email_hash(EMAIL_KEY, EMAIL));
    assert!(!made.to_string().to_lowercase().contains("streamer@example.com"), "the email isn't sent on");
    let st = b.status(&auth, false).await;
    assert_eq!((st["status"].as_str(), st["pro"].as_bool()), (Some("created"), Some(false)), "not paid yet");
    // The plan is checked against Pro's price once, then remembered.
    b.checkout(&b.install().await, "yearly").await;
    assert_eq!(b.rzp.count("GET", "/v1/plans/"), 1);
}

#[tokio::test]
async fn checkout_errors_each_have_their_status() {
    let b = billing(true).await;
    let auth = b.install().await;
    let url = format!("{}/v1/pro/checkout", b.base);
    let bad_json = b
        .http
        .post(&url)
        .bearer_auth(&auth)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_json.status(), 400);
    let wrong_type =
        b.http.post(&url).bearer_auth(&auth).header("content-type", "text/plain").body("{}").send().await.unwrap();
    assert_eq!(wrong_type.status(), 415);
    let (s, v) = b.post("/v1/pro/checkout", Some(&auth), Some(json!({ "plan": "lifetime", "email": EMAIL }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_plan")));
    let (s, v) = b.post("/v1/pro/checkout", Some(&auth), Some(json!({ "plan": "monthly", "email": "nope" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_email")));
    let (s, _) =
        b.post("/v1/pro/checkout", Some("wrong.secret"), Some(json!({ "plan": "monthly", "email": EMAIL }))).await;
    assert_eq!(s, 401);
    // Razorpay failing is a 502 with what to do, and nothing is stored.
    b.rzp.fail_all(Some(500));
    let (s, v) = b.checkout(&auth, "monthly").await;
    assert_eq!((s, v["error"]["code"].as_str()), (502, Some("razorpay_unavailable")));
    assert!(v["error"]["hint"].as_str().is_some());
    b.rzp.fail_all(None);
    assert_eq!(b.status(&auth, false).await["status"], "none");
    // Ten checkouts an hour per install, then 429.
    let busy = b.install().await;
    for _ in 0..10 {
        b.checkout(&busy, "monthly").await;
    }
    let (s, v) = b.checkout(&busy, "monthly").await;
    assert_eq!((s, v["error"]["code"].as_str()), (429, Some("rate_limited")));
}

#[tokio::test]
async fn an_active_subscriber_cant_buy_twice() {
    let b = billing(true).await;
    let (auth, _) = b.pro_install().await;
    let (s, v) = b.checkout(&auth, "yearly").await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_pro")));
}

#[tokio::test]
async fn checkout_refuses_a_plan_that_isnt_pros_price() {
    let rzp = FakeRazorpay::start().await;
    let mut c = cfg(&rzp, true);
    c.plan_monthly = Some(rzp.add_plan("monthly", 9_900));
    c.plan_yearly = Some(rzp.add_plan_raw("yearly", 199_900, "USD"));
    let b = start(rzp, c).await;
    let auth = b.install().await;
    for plan in ["monthly", "yearly"] {
        let (s, v) = b.checkout(&auth, plan).await;
        assert_eq!((s, v["error"]["code"].as_str()), (503, Some("billing_misconfigured")), "{plan}");
    }
    assert_eq!(b.rzp.count("POST", "/v1/subscriptions"), 0);
}

#[tokio::test]
async fn without_keys_billing_says_it_isnt_set_up() {
    let rzp = FakeRazorpay::start().await;
    let b = start(rzp, BillingConfig { sales_open: true, ..BillingConfig::default() }).await;
    let auth = b.install().await;
    let (s, v) = b.checkout(&auth, "monthly").await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("billing_unavailable")));
    let (s, v) = b.hook("evt_1", &sub_event("subscription.activated", "sub_x"), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("billing_unavailable")), "Razorpay retries later");
    assert_eq!(b.status(&auth, false).await["pro"], false);
}

#[tokio::test]
async fn webhooks_are_verified_before_anything_else() {
    let b = billing(true).await;
    let (auth, id) = b.pro_install().await;
    let body = sub_event("subscription.halted", &id);
    let fetches = b.rzp.count("GET", "/v1/subscriptions/");
    let (s, _) = b.hook("evt_forged", &body, Some(&sign("not-our-secret", body.to_string().as_bytes()))).await;
    assert_eq!(s, 401);
    let (s, _) = b.hook("evt_forged", &body, Some("")).await;
    assert_eq!(s, 401, "missing signature");
    assert_eq!(b.rzp.count("GET", "/v1/subscriptions/"), fetches, "a forged event never reaches Razorpay");
    assert_eq!(b.status(&auth, false).await["pro"], true);
    // Signed, but no event id / not JSON / no event name: 400.
    let raw = body.to_string();
    let res = b
        .http
        .post(format!("{}/billing/razorpay/webhook", b.base))
        .header("x-razorpay-signature", sign(WEBHOOK_SECRET, raw.as_bytes()))
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let garbage = "not json";
    let res = b
        .http
        .post(format!("{}/billing/razorpay/webhook", b.base))
        .header("x-razorpay-signature", sign(WEBHOOK_SECRET, garbage.as_bytes()))
        .header("x-razorpay-event-id", "evt_g")
        .body(garbage)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let (s, _) = b.hook("evt_noname", &json!({ "payload": {} }), None).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn activation_grants_pro_from_razorpays_current_state() {
    let b = billing(true).await;
    let auth = b.install().await;
    let (_, v) = b.checkout(&auth, "monthly").await;
    let id = v["subscription_id"].as_str().unwrap().to_string();
    // The event says "activated", but Razorpay hasn't charged yet: nothing is granted.
    let (s, _) = b.hook("evt_early", &sub_event("subscription.activated", &id), None).await;
    assert_eq!(s, 200);
    assert_eq!(b.status(&auth, false).await["pro"], false, "the event snapshot alone grants nothing");
    b.rzp.activate(&id, 30);
    let (s, v) = b.hook("evt_real", &sub_event("subscription.activated", &id), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")));
    let st = b.status(&auth, false).await;
    assert_eq!(
        (st["pro"].as_bool(), st["status"].as_str(), st["plan"].as_str()),
        (Some(true), Some("active"), Some("monthly"))
    );
    assert!(st["access_until"].as_i64().unwrap() > st["current_end"].as_i64().unwrap(), "grace for a late renewal");
}

#[tokio::test]
async fn a_repeated_event_is_handled_once() {
    let b = billing(true).await;
    let (auth, id) = b.pro_install().await;
    let fetches = b.rzp.count("GET", "/v1/subscriptions/");
    let (s, v) = b.hook(&format!("evt_act_{id}"), &sub_event("subscription.activated", &id), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("duplicate")));
    assert_eq!(b.rzp.count("GET", "/v1/subscriptions/"), fetches, "a duplicate doesn't call Razorpay");
    // Replaying an old "halted" with a new state at Razorpay: still the current truth.
    b.rzp.set_status(&id, "halted", None);
    b.hook("evt_h1", &sub_event("subscription.halted", &id), None).await;
    assert_eq!(b.status(&auth, false).await["pro"], false);
    b.rzp.activate(&id, 30);
    b.hook("evt_h1", &sub_event("subscription.halted", &id), None).await;
    assert_eq!(b.status(&auth, false).await["status"], "halted", "the same id again changes nothing");
}

#[tokio::test]
async fn a_failed_razorpay_call_leaves_the_event_for_the_retry() {
    let b = billing(true).await;
    let auth = b.install().await;
    let (_, v) = b.checkout(&auth, "monthly").await;
    let id = v["subscription_id"].as_str().unwrap().to_string();
    b.rzp.activate(&id, 30);
    b.rzp.fail_all(Some(503));
    let (s, v) = b.hook("evt_retry", &sub_event("subscription.activated", &id), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (502, Some("razorpay_unavailable")));
    b.rzp.fail_all(None);
    let (s, v) = b.hook("evt_retry", &sub_event("subscription.activated", &id), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")), "the retry is not a duplicate");
    assert_eq!(b.status(&auth, false).await["pro"], true);
}

#[tokio::test]
async fn other_products_events_are_ignored_without_calling_razorpay() {
    let b = billing(true).await;
    let before = b.rzp.requests().len();
    let mut transformx = sub_event("subscription.charged", "sub_TransformX0001");
    transformx["payload"]["subscription"]["entity"]["notes"] = json!({ "user_id": "u1" });
    let (s, v) = b.hook("evt_tx", &transformx, None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ignored")));
    let (s, v) = b.hook("evt_tx_refund", &refund_event("pay_TransformX01", 39_900), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ignored")));
    let (s, v) = b.hook("evt_pay", &json!({ "event": "payment.captured", "payload": {} }), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ignored")));
    assert_eq!(b.rzp.requests().len(), before, "nothing about other products goes to Razorpay");
}

#[tokio::test]
async fn the_state_machine_follows_razorpay_through_a_subscriptions_life() {
    let b = billing(true).await;
    let (auth, id) = b.pro_install().await;
    let step = |status: &'static str, days: Option<i64>, event: &'static str| {
        let b = &b;
        let id = id.clone();
        async move {
            b.rzp.set_status(&id, status, days);
            let (s, _) = b.hook(&format!("evt_{event}_{status}"), &sub_event(event, &id), None).await;
            assert_eq!(s, 200);
        }
    };
    let pro = |want: bool, why: &'static str| {
        let b = &b;
        let auth = auth.clone();
        async move { assert_eq!(b.status(&auth, false).await["pro"], want, "{why}") }
    };
    b.rzp.activate(&id, 30);
    let (s, _) = b.hook("evt_charge_1", &charged_event(&id, "pay_Fake0001", 19_900), None).await;
    assert_eq!(s, 200);
    pro(true, "charged").await;
    step("pending", None, "subscription.pending").await;
    pro(true, "a renewal being retried keeps Pro").await;
    step("halted", None, "subscription.halted").await;
    pro(false, "every retry failed").await;
    step("active", Some(30), "subscription.resumed").await;
    pro(true, "fixed and resumed").await;
    step("paused", None, "subscription.paused").await;
    pro(false, "paused").await;
    step("active", Some(30), "subscription.activated").await;
    step("cancelled", None, "subscription.cancelled").await;
    pro(true, "cancelled keeps the paid period").await;
    step("completed", Some(-1), "subscription.completed").await;
    pro(false, "the last period is over").await;
}

#[tokio::test]
async fn a_full_refund_ends_pro_and_a_partial_one_doesnt() {
    let b = billing(true).await;
    let (auth, id) = b.pro_install().await;
    b.hook("evt_c1", &charged_event(&id, "pay_Fake0002", 19_900), None).await;
    b.rzp.add_refund("pay_Fake0002", 5_000, "processed");
    let (s, _) = b.hook("evt_r1", &refund_event("pay_Fake0002", 5_000), None).await;
    assert_eq!(s, 200);
    assert_eq!(b.status(&auth, false).await["pro"], true, "a partial refund keeps Pro");
    b.rzp.add_refund("pay_Fake0002", 9_000, "failed");
    b.hook("evt_r2", &refund_event("pay_Fake0002", 9_000), None).await;
    assert_eq!(b.status(&auth, false).await["pro"], true, "a failed refund doesn't count");
    b.rzp.add_refund("pay_Fake0002", 14_900, "processed");
    let (s, v) = b.hook("evt_r3", &refund_event("pay_Fake0002", 14_900), None).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")));
    let st = b.status(&auth, false).await;
    assert_eq!((st["pro"].as_bool(), st["refunded"].as_bool()), (Some(false), Some(true)), "{st}");
    assert_eq!(b.rzp.subscription(&id).unwrap()["status"], "cancelled", "it can't charge again");
    // Razorpay re-sending the activation afterwards doesn't bring Pro back.
    b.hook("evt_late", &sub_event("subscription.activated", &id), None).await;
    assert_eq!(b.status(&auth, false).await["pro"], false);
}

#[tokio::test]
async fn cancel_stops_renewal_but_keeps_the_paid_period() {
    let b = billing(true).await;
    let none = b.install().await;
    let (s, v) = b.post("/v1/pro/cancel", Some(&none), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("no_subscription")));
    let (auth, id) = b.pro_install().await;
    b.rzp.fail_all(Some(500));
    let (s, _) = b.post("/v1/pro/cancel", Some(&auth), None).await;
    assert_eq!(s, 502);
    b.rzp.fail_all(None);
    let (s, v) = b.post("/v1/pro/cancel", Some(&auth), None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["pro"].as_bool(), v["cancel_at_period_end"].as_bool()), (Some(true), Some(true)));
    assert_eq!(v["access_until"], v["current_end"], "no grace after a cancel");
    assert!(b.rzp.subscription(&id).unwrap()["has_scheduled_changes"] == true, "cancelled at the cycle's end");
    let (s, v) = b.post("/v1/pro/cancel", Some(&auth), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_cancelled")));
}

#[tokio::test]
async fn the_manage_page_shows_and_cancels() {
    let b = billing(true).await;
    let none = b.install().await;
    let (s, _) = b.post("/v1/pro/manage-link", Some(&none), None).await;
    assert_eq!(s, 404);
    let (auth, id) = b.pro_install().await;
    let (s, v) = b.post("/v1/pro/manage-link", Some(&auth), None).await;
    assert_eq!(s, 201);
    let url = v["url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{}/pro/manage/", b.base)));
    let page = b.http.get(&url).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["referrer-policy"], "no-referrer");
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = page.text().await.unwrap();
    assert!(html.contains(&id) && html.contains("Renews on") && html.contains("₹199 a month"), "{html}");
    let done = b.http.post(format!("{url}/cancel")).send().await.unwrap();
    assert_eq!(done.status(), 200);
    assert!(done.text().await.unwrap().contains("Done. Pro won&#39;t renew"), "escaped, and said");
    let again = b.http.post(format!("{url}/cancel")).send().await.unwrap();
    assert_eq!(again.status(), 409, "already cancelled, said on the page");
    let other = b.http.get(format!("{}/pro/manage/{}", b.base, "x".repeat(43))).send().await.unwrap();
    assert_eq!(other.status(), 404);
}

#[tokio::test]
async fn pro_moves_to_a_new_install_with_the_id_and_email() {
    let b = billing(true).await;
    let (_, id) = b.pro_install().await;
    let fresh = b.install().await;
    let (s, v) = b
        .post("/v1/pro/restore", Some(&fresh), Some(json!({ "subscription_id": id, "email": "someone@else.com" })))
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")), "wrong email");
    let (s, v) = b
        .post("/v1/pro/restore", Some(&fresh), Some(json!({ "subscription_id": "sub_Nope0000001", "email": EMAIL })))
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")), "unknown id, same answer");
    let (s, v) =
        b.post("/v1/pro/restore", Some(&fresh), Some(json!({ "subscription_id": "../plans", "email": EMAIL }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_subscription")));
    let (s, v) = b
        .post("/v1/pro/restore", Some(&fresh), Some(json!({ "subscription_id": id, "email": "streamer@EXAMPLE.com" })))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["pro"].as_bool(), v["installs"].as_i64()), (Some(true), Some(2)));
    // Guessing is limited.
    let guesser = b.install().await;
    for _ in 0..5 {
        b.post("/v1/pro/restore", Some(&guesser), Some(json!({ "subscription_id": id, "email": "x@y.co" }))).await;
    }
    let (s, _) =
        b.post("/v1/pro/restore", Some(&guesser), Some(json!({ "subscription_id": id, "email": EMAIL }))).await;
    assert_eq!(s, 429);
}

#[tokio::test]
async fn restore_works_from_razorpays_notes_if_the_relay_lost_its_copy() {
    let rzp = FakeRazorpay::start().await;
    let c = cfg(&rzp, true);
    let first = start(rzp.clone(), c.clone()).await;
    let (_, id) = first.pro_install().await;
    // A new relay database, same Razorpay account.
    let b = start(rzp, c).await;
    let auth = b.install().await;
    let (s, v) = b.post("/v1/pro/restore", Some(&auth), Some(json!({ "subscription_id": id, "email": EMAIL }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["pro"], true);
    let (s, _) = b
        .post("/v1/pro/restore", Some(&b.install().await), Some(json!({ "subscription_id": id, "email": "a@b.co" })))
        .await;
    assert_eq!(s, 404);
    b.rzp.fail_all(Some(500));
    let (s, _) = b
        .post(
            "/v1/pro/restore",
            Some(&b.install().await),
            Some(json!({ "subscription_id": "sub_Other00001", "email": EMAIL })),
        )
        .await;
    assert_eq!(s, 502, "Razorpay down is not 'not found'");
}

#[tokio::test]
async fn status_refresh_and_the_daily_recheck_catch_missed_webhooks() {
    let rzp = FakeRazorpay::start().await;
    let c = cfg(&rzp, true);
    let b = start(rzp, c.clone()).await;
    let auth = b.install().await;
    let (_, v) = b.checkout(&auth, "monthly").await;
    let id = v["subscription_id"].as_str().unwrap().to_string();
    b.rzp.activate(&id, 30);
    assert_eq!(b.status(&auth, false).await["pro"], false, "no webhook yet");
    assert_eq!(b.status(&auth, true).await["pro"], true, "refresh asks Razorpay");
    // Razorpay unreachable: the stored state, with a warning.
    let other = b.install().await;
    let (_, v) = b.checkout(&other, "monthly").await;
    let other_id = v["subscription_id"].as_str().unwrap().to_string();
    b.rzp.activate(&other_id, 30);
    b.rzp.fail_all(Some(500));
    let v = b.status(&other, true).await;
    assert_eq!(v["status"], "created");
    assert!(v["warning"].as_str().is_some(), "{v}");
    b.rzp.fail_all(None);
    let v = b.status(&other, true).await;
    assert_eq!(v["status"], "created", "refreshes are limited to one per 10 s per install");
}

#[tokio::test]
async fn reconcile_rereads_open_subscriptions_and_refunds() {
    let rzp = FakeRazorpay::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    db.add_install("inst", "h").unwrap();
    let c = cfg(&rzp, true);
    let st = crate::AppState::new(
        crate::Config {
            public_url: "http://127.0.0.1".into(),
            tip_url: None,
            kick_client_id: None,
            kick_client_secret: None,
            kick_id_url: String::new(),
            kick_api_url: String::new(),
        },
        db,
    )
    .with_billing(c.clone());
    let made = super::razorpay::Razorpay::new(&rzp.base, KEY_ID, KEY_SECRET)
        .create_subscription(json!({ "plan_id": c.plan_monthly, "total_count": 120 }))
        .await
        .unwrap();
    let id = made["id"].as_str().unwrap();
    st.db.add_pro_sub(id, "monthly", "eh", "created", "inst").unwrap();
    rzp.activate(id, 30);
    assert_eq!(reconcile(&st).await, 1);
    assert_eq!(st.db.pro_sub(id).unwrap().unwrap().status, "active");
    st.db
        .record_pro_charge(
            "evt_x",
            id,
            Some(("pay_Recon00001", 19_900)),
            &store::Synced { status: "active".into(), current_end: None, cancel_at_end: None },
        )
        .unwrap();
    rzp.add_refund("pay_Recon00001", 19_900, "processed");
    reconcile(&st).await;
    let s = st.db.pro_sub(id).unwrap().unwrap();
    assert!(s.refunded_at.is_some(), "a refund whose webhooks were lost still ends Pro");
}

#[tokio::test]
async fn a_new_checkout_cancels_the_unpaid_one_it_replaces() {
    let b = billing(true).await;
    let auth = b.install().await;
    let (_, v) = b.checkout(&auth, "monthly").await;
    let first = v["subscription_id"].as_str().unwrap().to_string();
    let (s, v) = b.checkout(&auth, "yearly").await;
    assert_eq!(s, 201);
    assert_ne!(v["subscription_id"].as_str().unwrap(), first);
    assert_eq!(b.rzp.subscription(&first).unwrap()["status"], "cancelled", "the old link can't be paid any more");
    // If the earlier one got paid meanwhile, that one stays and no new one is made.
    let auth2 = b.install().await;
    let (_, v) = b.checkout(&auth2, "monthly").await;
    let paid = v["subscription_id"].as_str().unwrap().to_string();
    b.rzp.activate(&paid, 30);
    let before = b.rzp.count("POST", "/v1/subscriptions");
    let (s, v) = b.checkout(&auth2, "yearly").await;
    // Our copy still says "created"; Razorpay says active: refused, and the paid one is kept.
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_pro")), "{v}");
    assert_eq!(b.rzp.count("POST", "/v1/subscriptions"), before);
    assert_eq!(b.rzp.subscription(&paid).unwrap()["status"], "active", "never cancelled");
    assert_eq!(b.status(&auth2, false).await["pro"], true);
}

#[tokio::test]
async fn without_an_email_key_checkout_and_restore_say_billing_isnt_set_up() {
    let rzp = FakeRazorpay::start().await;
    let mut c = cfg(&rzp, true);
    c.email_key = None;
    let b = start(rzp, c).await;
    let auth = b.install().await;
    let (s, _) = b.checkout(&auth, "monthly").await;
    assert_eq!(s, 503);
    let (s, _) =
        b.post("/v1/pro/restore", Some(&auth), Some(json!({ "subscription_id": "sub_Abc123", "email": EMAIL }))).await;
    assert_eq!(s, 503);
}
