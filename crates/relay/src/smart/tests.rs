//! Smart features through the real `router()` with a fake model: the gate (401/402/503),
//! every body check, the per-subscription and relay-wide caps (the model is never called
//! past them), give-backs on failure, and that nothing is stored but counters.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;

struct World {
    base: String,
    db: Arc<Db>,
    fake: Arc<FakeSmart>,
    backup: Arc<FakeSmart>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn world_with(fake: Option<FakeSmart>, daily: i64) -> World {
    world_backed(fake, daily, None, DEFAULT_BACKUP_DAILY_CALLS).await
}

async fn world_backed(fake: Option<FakeSmart>, daily: i64, backup: Option<FakeSmart>, backup_daily: i64) -> World {
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
    let on = fake.is_some();
    let fake = Arc::new(fake.unwrap_or_default());
    let provider: Option<Arc<dyn SmartProvider>> = on.then(|| fake.clone() as Arc<dyn SmartProvider>);
    let backed = backup.is_some();
    let backup = Arc::new(backup.unwrap_or_default());
    let second: Option<Arc<dyn SmartProvider>> = backed.then(|| backup.clone() as Arc<dyn SmartProvider>);
    let state = AppState::new(cfg, db).with_smart(provider, daily).with_smart_backup(second, backup_daily);
    let db = state.db.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, fake, backup, http: reqwest::Client::new(), _dir: dir }
}

async fn world(answer: &str) -> World {
    world_with(Some(FakeSmart::answering(answer)), DEFAULT_DAILY_CALLS).await
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
        auth
    }

    async fn send(&self, method: reqwest::Method, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
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

    async fn post(&self, path: &str, auth: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(auth), Some(body)).await
    }
}

fn msgs(n: usize) -> Value {
    json!({ "messages": (0..n).map(|i| json!({ "id": format!("m{i}"), "text": format!("hello {i}"), "user": "v" })).collect::<Vec<_>>() })
}

fn reply_body() -> Value {
    json!({ "question": "when do you stream?", "about": "Live Mon-Fri at 8pm IST.", "channel": "Arsh" })
}

fn recap_body() -> Value {
    json!({ "minutes": 120, "peak_viewers": 30, "follows": 4, "subs": 1, "messages": 300, "bits": 0,
            "tips": ["INR 200.00"], "raids": [], "thanks": ["ana", "bo"] })
}

const ROUTES: [&str; 3] = ["/v1/pro/smart/moderate", "/v1/pro/smart/reply", "/v1/pro/smart/recap"];

#[tokio::test]
async fn the_gate_answers_401_402_then_503_without_a_key() {
    let w = world_with(None, DEFAULT_DAILY_CALLS).await;
    let free = w.install("free");
    let pro = w.pro("p", "sub_1");
    for path in ROUTES.iter().copied().chain(["/v1/pro/smart"]) {
        let get = path == "/v1/pro/smart";
        let m = if get { reqwest::Method::GET } else { reqwest::Method::POST };
        let body = (!get).then(|| json!({ "nonsense": true }));
        let (s, v) = w.send(m.clone(), path, None, body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")), "{path}");
        let (s, v) = w.send(m.clone(), path, Some(&free), body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{path}");
        let (s, v) = w.send(m, path, Some(&pro), body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (503, Some("smart_off")), "{path}: {v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("everything else works"));
    }
    let (_, v) = w.send(reqwest::Method::GET, "/v1/pro/status", Some(&pro), None).await;
    assert_eq!(v["features"]["smart"], false);
}

#[tokio::test]
async fn moderation_reads_hinglish_answers_and_checks_every_field() {
    let answer = r#"{"results":[{"id":"m1","action":"timeout","category":"abuse","reason":"Hinglish abuse at a viewer","confidence":0.92},{"id":"m0","action":"none","confidence":0.1}]}"#;
    let w = world(answer).await;
    let auth = w.pro("p", "sub_1");
    let body = json!({ "messages": [
        { "id": "m0", "text": "bhai gg", "user": "ana" },
        { "id": "m1", "text": "tu ch*tiya hai\u{202E}", "user": "bo", "first_time": true } ], "rules": "no spoilers" });
    let (s, v) = w.post("/v1/pro/smart/moderate", &auth, body).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["action"], "none");
    assert_eq!(v["results"][1]["action"], "timeout");
    assert_eq!(v["results"][1]["confidence"], 0.92);
    let calls = w.fake.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].system.contains("Hinglish") && calls[0].json);
    assert!(calls[0].user.contains("no spoilers") && !calls[0].user.contains('\u{202E}'), "direction marks go");
    // A garbled answer: 502, and the check is given back.
    w.fake.then(Ok("sorry I can't".into()));
    let (s, v) = w.post("/v1/pro/smart/moderate", &auth, msgs(1)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (502, Some("smart_unavailable")), "{v}");
    let (_, v) = w.send(reqwest::Method::GET, "/v1/pro/smart", Some(&auth), None).await;
    assert_eq!(v["quotas"]["smart_moderation"]["used"], 2, "{v}");
}

#[tokio::test]
async fn bodies_are_checked_before_the_model_is_asked() {
    let w = world("{}").await;
    let auth = w.pro("p", "sub_1");
    let res = w
        .http
        .post(format!("{}/v1/pro/smart/moderate", w.base))
        .bearer_auth(&auth)
        .header("content-type", "text/plain")
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 415);
    let cases = [
        ("/v1/pro/smart/moderate", json!({ "messages": [] }), 422, "invalid"),
        ("/v1/pro/smart/moderate", msgs(21), 413, "too_many_messages"),
        (
            "/v1/pro/smart/moderate",
            json!({ "messages": [{ "id": "a", "text": "x" }, { "id": "a", "text": "y" }] }),
            422,
            "invalid",
        ),
        ("/v1/pro/smart/moderate", json!({ "messages": [{ "id": "a", "text": "  " }] }), 422, "invalid"),
        (
            "/v1/pro/smart/moderate",
            json!({ "messages": [{ "id": "a", "text": "x" }], "rules": "r".repeat(501) }),
            422,
            "invalid",
        ),
        ("/v1/pro/smart/moderate", json!({ "nope": 1 }), 400, "invalid"),
        ("/v1/pro/smart/reply", json!({ "question": "", "about": "x" }), 422, "invalid"),
        ("/v1/pro/smart/reply", json!({ "question": "q", "about": "" }), 422, "invalid"),
        ("/v1/pro/smart/reply", json!({ "question": "q".repeat(301), "about": "x" }), 422, "invalid"),
        ("/v1/pro/smart/recap", json!({ "minutes": 0 }), 422, "invalid"),
    ];
    for (path, body, status, code) in cases {
        let (s, v) = w.post(path, &auth, body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (status, Some(code)), "{path} {body}: {v}");
        assert!(v["error"]["message"].is_string());
    }
    assert!(w.fake.calls().is_empty(), "the model was never asked");
}

#[tokio::test]
async fn replies_come_back_or_are_null_and_never_carry_strange_links() {
    let w = world("Mon to Fri at 8pm IST!").await;
    let auth = w.pro("p", "sub_1");
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!((s, v["reply"].as_str()), (200, Some("Mon to Fri at 8pm IST!")), "{v}");
    assert!(w.fake.calls()[0].system.contains("Live Mon-Fri at 8pm IST."));
    w.fake.then(Ok("NO_ANSWER".into()));
    let (_, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!(v["reply"], Value::Null);
    w.fake.then(Ok("free skins at https://scam.example".into()));
    let (_, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!(v["reply"], Value::Null);
}

#[tokio::test]
async fn a_recap_comes_back() {
    let w = world("A steady two hours.\nThank: ana, bo\n- Start on time").await;
    let auth = w.pro("p", "sub_1");
    let (s, v) = w.post("/v1/pro/smart/recap", &auth, recap_body()).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["summary"].as_str().unwrap().starts_with("A steady"));
    assert!(w.fake.calls()[0].user.contains("\"minutes_live\":120"));
}

#[tokio::test]
async fn the_daily_quota_per_subscription_then_429_with_reset() {
    let w = world("Fine.").await;
    let auth = w.pro("p", "sub_1");
    for _ in 0..RECAPS.limit {
        assert_eq!(w.post("/v1/pro/smart/recap", &auth, recap_body()).await.0, 200);
    }
    let res = w
        .http
        .post(format!("{}/v1/pro/smart/recap", w.base))
        .bearer_auth(&auth)
        .json(&recap_body())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after") && res.headers().contains_key("x-quota-reset"));
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "smart_quota");
    assert!(v["error"]["hint"].as_str().unwrap().contains("UTC"));
    assert_eq!(w.fake.calls().len(), RECAPS.limit as usize, "the model isn't asked past the quota");
    // Another subscription has its own.
    let other = w.pro("q", "sub_2");
    assert_eq!(w.post("/v1/pro/smart/recap", &other, recap_body()).await.0, 200);
    // The status lists the counters.
    let (_, v) = w.send(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!(v["features"]["smart"], true);
    assert_eq!(v["quotas"]["smart_recaps"]["used"], RECAPS.limit);
    assert_eq!(v["quotas"]["smart_replies"]["limit"], REPLIES.limit);
}

#[tokio::test]
async fn the_relay_wide_cap_rests_everyone_until_midnight() {
    let w = world_with(Some(FakeSmart::answering("ok")), 2).await;
    let a = w.pro("a", "sub_a");
    let b = w.pro("b", "sub_b");
    assert_eq!(w.post("/v1/pro/smart/reply", &a, reply_body()).await.0, 200);
    assert_eq!(w.post("/v1/pro/smart/reply", &b, reply_body()).await.0, 200);
    let (s, v) = w.post("/v1/pro/smart/reply", &a, reply_body()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("smart_resting")), "{v}");
    assert_eq!(w.fake.calls().len(), 2, "never past the cap");
    let (_, v) = w.send(reqwest::Method::GET, "/v1/pro/smart", Some(&a), None).await;
    assert_eq!(
        (v["resting"].as_bool(), v["quotas"]["smart_replies"]["used"].as_i64()),
        (Some(true), Some(1)),
        "given back: {v}"
    );
}

#[tokio::test]
async fn the_providers_own_limit_rests_too_and_failures_give_back() {
    let w = world("ok").await;
    let auth = w.pro("p", "sub_1");
    w.fake.then(Err(SmartError::Unavailable("down".into())));
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (502, Some("smart_unavailable")), "{v}");
    w.fake.then(Err(SmartError::Quota));
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("smart_resting")), "{v}");
    let calls = w.fake.calls().len();
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 503, "resting: not asked again");
    assert_eq!(w.fake.calls().len(), calls);
    assert_eq!(w.db.quota_used("sub_1", REPLIES.name, Period::Day).unwrap(), 0, "both failures given back");
}

#[tokio::test]
async fn ten_calls_a_minute_per_subscription() {
    let w = world_with(Some(FakeSmart::answering("ok")), 100).await;
    let auth = w.pro("p", "sub_1");
    for _ in 0..SUB_RATE.limit {
        assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    }
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    // Unless the minute turned over between the calls.
    if s != 200 {
        assert_eq!((s, v["error"]["code"].as_str()), (429, Some("smart_rate")), "{v}");
    }
}

#[tokio::test]
async fn nothing_but_counters_is_stored() {
    let w = world("ok").await;
    let auth = w.pro("p", "sub_1");
    let mut b = reply_body();
    b["question"] = json!("secret-question-text");
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, b).await.0, 200);
    let c = w.db.c();
    let tables: Vec<String> = {
        let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    for t in tables {
        let mut st = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
        let cols = st.column_count();
        let mut rows = st.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            for i in 0..cols {
                if let Ok(rusqlite::types::Value::Text(s)) = r.get::<_, rusqlite::types::Value>(i) {
                    assert!(!s.contains("secret-question"), "{t} keeps the text");
                }
            }
        }
    }
}

#[test]
fn the_daily_cap_setting_is_bounded() {
    assert_eq!(parse_daily(None), DEFAULT_DAILY_CALLS);
    assert_eq!(parse_daily(Some("1_000")), 1_000);
    assert_eq!(parse_daily(Some("99999")), MAX_DAILY_CALLS);
    assert_eq!(parse_daily(Some("-3")), DEFAULT_DAILY_CALLS);
    assert_eq!(parse_daily(Some("0")), 0);
}

#[tokio::test]
async fn the_backup_answers_once_the_provider_rests() {
    let w = world_backed(Some(FakeSmart::answering("ok")), 100, Some(FakeSmart::answering("ok")), 100).await;
    let auth = w.pro("p", "sub_1");
    w.fake.then(Err(SmartError::Quota));
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200, "the backup answered");
    assert_eq!(w.backup.calls().len(), 1);
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    assert_eq!((w.fake.calls().len(), w.backup.calls().len()), (1, 2), "the provider rests until midnight");
    let (_, v) = w.send(reqwest::Method::GET, "/v1/pro/smart", Some(&auth), None).await;
    assert_eq!(v["resting"].as_bool(), Some(false), "{v}");
}

#[tokio::test]
async fn the_backup_takes_over_at_the_cap_and_rests_at_its_own() {
    let w = world_backed(Some(FakeSmart::answering("ok")), 1, Some(FakeSmart::answering("ok")), 1).await;
    let auth = w.pro("p", "sub_1");
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    assert_eq!((w.fake.calls().len(), w.backup.calls().len()), (1, 1));
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("smart_resting")), "{v}");
    assert_eq!((w.fake.calls().len(), w.backup.calls().len()), (1, 1), "never past either cap");
    assert_eq!(w.db.quota_used("sub_1", REPLIES.name, Period::Day).unwrap(), 2, "the refused call is given back");
}

#[tokio::test]
async fn a_provider_that_is_down_falls_back_without_resting() {
    let w = world_backed(Some(FakeSmart::answering("ok")), 100, Some(FakeSmart::answering("ok")), 100).await;
    let auth = w.pro("p", "sub_1");
    w.fake.then(Err(SmartError::Unavailable("down".into())));
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    assert_eq!(w.post("/v1/pro/smart/reply", &auth, reply_body()).await.0, 200);
    assert_eq!((w.fake.calls().len(), w.backup.calls().len()), (2, 1), "asked again next time");
    w.fake.then(Err(SmartError::Unavailable("down".into())));
    w.backup.then(Err(SmartError::Unavailable("down too".into())));
    let (s, v) = w.post("/v1/pro/smart/reply", &auth, reply_body()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (502, Some("smart_unavailable")), "{v}");
}

#[test]
fn the_backup_cap_reads_with_its_own_default() {
    assert_eq!(parse_backup_daily(None), DEFAULT_BACKUP_DAILY_CALLS);
    assert_eq!(parse_backup_daily(Some("abc")), DEFAULT_BACKUP_DAILY_CALLS);
    assert_eq!(parse_backup_daily(Some("-1")), DEFAULT_BACKUP_DAILY_CALLS);
    assert_eq!(parse_backup_daily(Some("2_000")), MAX_DAILY_CALLS);
    assert_eq!(parse_backup_daily(Some(" 300 ")), 300);
}
