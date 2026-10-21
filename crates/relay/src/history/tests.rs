//! Stream history: the pure parts, then every route through the real `router()` (every
//! status each can answer), retention, and the lapse rules.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;

// ── Unit ─────────────────────────────────────────────────────────────────────────────

#[test]
fn weeks_start_on_monday_utc() {
    // 2026-10-19 12:00 UTC is a Monday.
    let monday = 1_792_411_200_000 - 12 * 3_600_000;
    assert_eq!(week_start(monday + 12 * 3_600_000), monday);
    assert_eq!(week_start(monday + 6 * DAY + 23 * 3_600_000), monday, "Sunday night is the same week");
    assert_eq!(week_start(monday - 1), monday - 7 * DAY, "a minute before is last week");
    assert_eq!(iso(monday + 5 * 60_000), "2026-10-19 00:05");
}

fn out(platform: &str, started_at: i64, minutes: i64, avg: i64, peak: i64) -> StreamOut {
    StreamOut {
        platform: platform.into(),
        key: format!("{platform}:{started_at}"),
        started_at,
        ended_at: Some(started_at + minutes * 60_000),
        title: None,
        category: None,
        peak_viewers: peak,
        avg_viewers: avg,
        chat_lines: 10,
        chatters: 3,
        new_chatters: 1,
        follows: 2,
        subs: 1,
        tips: vec![],
        platform_value: vec![],
        updated_at: started_at,
    }
}

#[test]
fn weekly_totals_cover_every_week_and_weight_by_length() {
    let monday = 1_792_368_000_000; // 2026-10-19 00:00 UTC
    let from = monday - 14 * DAY;
    let to = monday + 3 * DAY;
    let streams = vec![
        out("twitch", monday + DAY, 60, 100, 150),
        out("kick", monday + DAY, 180, 20, 30),
        out("youtube", from + 2 * DAY, 30, 5, 9),
    ];
    let w = weekly(&streams, from, to, to);
    assert_eq!(w.len(), 3, "two weeks back plus this one, the empty one too");
    assert_eq!(w.iter().map(|x| x.streams).collect::<Vec<_>>(), [1, 0, 2]);
    let this = &w[2];
    assert_eq!(this.week_start, monday);
    assert_eq!((this.by_platform["twitch"], this.by_platform["kick"], this.by_platform["youtube"]), (1, 1, 0));
    assert_eq!(this.minutes, 240);
    assert_eq!(this.peak_viewers, 150);
    assert_eq!(this.avg_viewers, 40, "(100×60 + 20×180) / 240");
    assert_eq!((this.follows, this.subs, this.chat_lines), (4, 2, 20));
    assert_eq!(w[1].avg_viewers, 0);
}

#[test]
fn csv_quotes_text_and_defuses_formulas() {
    let mut s = out("twitch", 1_792_368_000_000, 90, 12, 20);
    s.title = Some("=HYPERLINK(\"x\") \"big\" day".into());
    s.category = Some("Just Chatting".into());
    s.tips = vec![Money { currency: "INR".into(), minor: 50_050 }, Money { currency: "JPY".into(), minor: 500 }];
    s.platform_value = vec![PlatformAmount { unit: "bits".into(), amount: 300 }];
    let text = csv(&[s], 0);
    let mut lines = text.split("\r\n");
    assert!(lines.next().unwrap().starts_with("platform,started_utc,ended_utc,minutes,title"));
    let row = lines.next().unwrap();
    assert_eq!(
        row,
        "twitch,2026-10-19 00:00,2026-10-19 01:30,90,\"'=HYPERLINK(\"\"x\"\") \"\"big\"\" day\",\"Just Chatting\",20,12,10,3,1,2,1,\"INR 500.50; JPY 500\",\"300 bits\""
    );
}

#[test]
fn titles_lose_control_and_direction_characters() {
    assert_eq!(clean(Some("  \u{202E}Hi\u{0007} there\u{200B} "), 300).as_deref(), Some("Hi there"));
    assert_eq!(clean(Some(" \u{200B} "), 300), None);
    assert_eq!(clean(Some(&"é".repeat(400)), 300).map(|s| s.chars().count()), Some(300));
}

// ── Through the router ───────────────────────────────────────────────────────────────

struct World {
    base: String,
    db: Arc<Db>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
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
    let state = AppState::new(cfg, db);
    let db = state.db.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), _dir: dir }
}

impl World {
    /// Signs install `id` in to the account of `email` (made if new): its app session.
    fn sign_in(&self, id: &str, email: &str) -> String {
        let (a, _) = self.db.account_sign_in(email, None, now_ms()).unwrap();
        let sid = format!("se_{id}");
        self.db
            .account_add_session(&sid, &a.id, &crate::hash(&sid), "app", "PC", Some(id), now_ms(), DAY, 90 * DAY)
            .unwrap();
        a.id
    }

    /// An install, with no Pro.
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    /// An install with an active subscription `sub`.
    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        self.set_sub(sub, "active", Some(now_ms() + 30 * DAY));
        auth
    }

    fn set_sub(&self, sub: &str, status: &str, current_end: Option<i64>) {
        let s = Synced { status: status.into(), current_end, cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
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

    async fn get(&self, path: &str, auth: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, Some(auth), None).await
    }

    async fn csv(&self, path: &str, auth: &str) -> (u16, String, reqwest::header::HeaderMap) {
        let res = self.http.get(format!("{}{path}", self.base)).bearer_auth(auth).send().await.unwrap();
        let s = res.status().as_u16();
        let h = res.headers().clone();
        (s, res.text().await.unwrap(), h)
    }
}

fn stream(key: &str, started_at: i64, ended_at: Option<i64>) -> Value {
    json!({
        "platform": "twitch", "key": key, "started_at": started_at, "ended_at": ended_at,
        "title": "Ranked grind", "category": "VALORANT", "peak_viewers": 140, "avg_viewers": 96,
        "chat_lines": 1200, "chatters": 80, "new_chatters": 12, "follows": 9, "subs": 3,
        "tips": [{ "currency": "INR", "minor": 50000 }], "platform_value": [{ "unit": "bits", "amount": 500 }]
    })
}

#[tokio::test]
async fn every_route_needs_a_known_install_with_pro_or_an_account() {
    let w = world().await;
    let free = w.install("free");
    let routes: [(reqwest::Method, &str); 4] = [
        (reqwest::Method::GET, "/v1/pro/history"),
        (reqwest::Method::POST, "/v1/pro/history/streams"),
        (reqwest::Method::POST, "/v1/pro/history/samples"),
        (reqwest::Method::GET, "/v1/pro/history/export.csv"),
    ];
    for (m, path) in routes {
        // The gate answers before the body is looked at.
        let body = (m == reqwest::Method::POST).then(|| json!({ "nonsense": true }));
        let (s, v) = w.send(m.clone(), path, None, body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")), "{path}");
        let (s, v) = w.send(m.clone(), path, Some("nobody.secret"), body.clone()).await;
        assert_eq!(s, 401, "{path} unknown install: {v}");
        let (s, v) = w.send(m, path, Some(&free), body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("account_required")), "{path}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Account"));
    }
}

#[tokio::test]
async fn a_stream_upload_is_idempotent_and_samples_follow_it() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let start = now_ms() - 3 * 3_600_000;
    // Live: the desktop sends the stream when it starts, then samples.
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:1", start, None)).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["stream"]["title"], "Ranked grind");
    assert_eq!(v["stream"]["ended_at"], Value::Null);
    let samples = json!({ "platform": "twitch", "key": "twitch:1", "samples": [
        { "at": start + 300_000, "viewers": 50 }, { "at": start + 600_000, "viewers": 210 } ] });
    let (s, v) = w.post("/v1/pro/history/samples", &auth, samples.clone()).await;
    assert_eq!((s, v["saved"].as_i64()), (200, Some(2)), "{v}");
    // A retry of the same samples changes nothing.
    assert_eq!(w.post("/v1/pro/history/samples", &auth, samples).await.0, 200);
    assert_eq!(
        w.db.stream_samples("sub_1", "twitch", "twitch:1").unwrap().unwrap(),
        [(start + 300_000, 50), (start + 600_000, 210)]
    );
    let (_, v) = w.get("/v1/pro/history", &auth).await;
    let live = &v["streams"][0];
    assert_eq!(
        (live["peak_viewers"].as_i64(), live["avg_viewers"].as_i64()),
        (Some(210), Some(130)),
        "samples while live"
    );
    // The end: totals replace what was there, the peak never goes down.
    let end = now_ms() - 60_000;
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:1", start, Some(end))).await;
    assert_eq!(s, 200, "the same key is the same stream: {v}");
    assert_eq!((v["stream"]["peak_viewers"].as_i64(), v["stream"]["avg_viewers"].as_i64()), (Some(210), Some(96)));
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:1", start, Some(end))).await;
    assert_eq!(s, 200, "retried: {v}");
    let (s, v) = w.get("/v1/pro/history", &auth).await;
    assert_eq!(s, 200);
    assert_eq!(v["streams"].as_array().unwrap().len(), 1);
    assert_eq!(v["truncated"], false);
    let st = &v["streams"][0];
    assert_eq!(st["ended_at"].as_i64(), Some(end));
    assert_eq!(st["tips"], json!([{ "currency": "INR", "minor": 50000 }]));
    assert_eq!(st["platform_value"], json!([{ "unit": "bits", "amount": 500 }]));
    let weeks = v["weekly"].as_array().unwrap();
    assert!((4..=5).contains(&weeks.len()), "four weeks by default: {}", weeks.len());
    assert_eq!(weeks.iter().map(|w| w["streams"].as_i64().unwrap()).sum::<i64>(), 1);
    // Another subscription never sees it.
    let other = w.pro("i2", "sub_2");
    let (_, v) = w.get("/v1/pro/history", &other).await;
    assert_eq!(v["streams"], json!([]));
    let (s, v) = w
        .post(
            "/v1/pro/history/samples",
            &other,
            json!({ "platform": "twitch", "key": "twitch:1", "samples": [{ "at": start, "viewers": 1 }] }),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("stream_not_found")));
    // A second install on the same subscription shares it.
    w.db.add_install("i3", &crate::hash("secret")).unwrap();
    w.db.link_pro_install("i3", "sub_1").unwrap();
    let (_, v) = w.get("/v1/pro/history", "i3.secret").await;
    assert_eq!(v["streams"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn bad_bodies_get_400_415_422_or_413() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let now = now_ms();
    let url = format!("{}/v1/pro/history/streams", w.base);
    // Not JSON at all, and JSON without the content type.
    let res =
        w.http.post(&url).bearer_auth(&auth).header("content-type", "application/json").body("{").send().await.unwrap();
    assert_eq!(res.status(), 400);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid");
    let res = w.http.post(&url).bearer_auth(&auth).body(stream("k", now, None).to_string()).send().await.unwrap();
    assert_eq!(res.status(), 415);
    // A missing field is named.
    let (s, v) = w.post("/v1/pro/history/streams", &auth, json!({ "platform": "twitch", "key": "k" })).await;
    assert_eq!(s, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("started_at"), "{v}");
    let (s, v) = w
        .post("/v1/pro/history/streams", &auth, json!({ "platform": "twitch", "key": "k", "started_at": "soon" }))
        .await;
    assert_eq!(s, 400, "{v}");
    // Valid JSON, values that can't be right: 422 naming the field.
    let cases: Vec<(Value, &str)> = vec![
        (json!({ "platform": "tiktok" }), "platform"),
        (json!({ "key": "has space" }), "key"),
        (json!({ "key": "" }), "key"),
        (json!({ "started_at": now + DAY }), "started_at"),
        (json!({ "started_at": now - 500 * DAY }), "started_at"),
        (json!({ "ended_at": now - 10 * DAY }), "ended_at"),
        (json!({ "peak_viewers": -1 }), "peak_viewers"),
        (json!({ "avg_viewers": 500 }), "avg_viewers"),
        (json!({ "new_chatters": 90 }), "new_chatters"),
        (json!({ "tips": [{ "currency": "inr", "minor": 1 }] }), "tips[0].currency"),
        (json!({ "tips": [{ "currency": "INR", "minor": -1 }] }), "tips[0].minor"),
        (
            json!({ "tips": [{ "currency": "INR", "minor": 1 }, { "currency": "INR", "minor": 2 }] }),
            "one entry per currency",
        ),
        (json!({ "platform_value": [{ "unit": "Bits!", "amount": 1 }] }), "platform_value[0].unit"),
    ];
    for (patch, field) in cases {
        let mut body = stream("twitch:x", now - 3_600_000, None);
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, v) = w.post("/v1/pro/history/streams", &auth, body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_stream")), "{field}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(field), "{field}: {v}");
    }
    // Over the relay's body limit.
    let mut big = stream("twitch:big", now - 3_600_000, None);
    big["title"] = json!("x".repeat(crate::BODY_LIMIT + 10));
    let (s, v) = w.post("/v1/pro/history/streams", &auth, big).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_large")), "{v}");
    assert!(w.get("/v1/pro/history", &auth).await.1["streams"].as_array().unwrap().is_empty(), "nothing saved");
}

#[tokio::test]
async fn samples_are_checked_and_capped_at_100() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let start = now_ms() - 3_600_000;
    assert_eq!(w.post("/v1/pro/history/streams", &auth, stream("twitch:s", start, None)).await.0, 201);
    let many: Vec<Value> = (0..101).map(|i| json!({ "at": start + i * 1000, "viewers": 1 })).collect();
    let body = |samples: Value| json!({ "platform": "twitch", "key": "twitch:s", "samples": samples });
    let (s, v) = w.post("/v1/pro/history/samples", &auth, body(json!(many))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_many_samples")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("100"));
    let exactly: Vec<Value> = (0..100).map(|i| json!({ "at": start + i * 1000, "viewers": i })).collect();
    assert_eq!(w.post("/v1/pro/history/samples", &auth, body(json!(exactly))).await.0, 200);
    for (samples, what) in [
        (json!([]), "samples"),
        (json!([{ "at": start, "viewers": -3 }]), "samples[0].viewers"),
        (json!([{ "at": start - DAY, "viewers": 3 }]), "outside"),
        (json!([{ "at": now_ms() + DAY, "viewers": 3 }]), "outside"),
    ] {
        let (s, v) = w.post("/v1/pro/history/samples", &auth, body(samples)).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_sample")), "{what}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(what), "{v}");
    }
    let (s, _) =
        w.post("/v1/pro/history/samples", &auth, json!({ "platform": "kick", "key": "nope!", "samples": [] })).await;
    assert_eq!(s, 422);
    let (s, _) = w.post("/v1/pro/history/samples", &auth, json!({ "platform": "twitch", "key": "twitch:s" })).await;
    assert_eq!(s, 400, "samples missing");
}

#[tokio::test]
async fn an_ended_stream_locks_after_a_day() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let start = now_ms() - 3 * DAY;
    let end = start + 2 * 3_600_000;
    // Uploaded late (the PC was offline for days): new, so it's taken.
    assert_eq!(w.post("/v1/pro/history/streams", &auth, stream("twitch:old", start, Some(end))).await.0, 201);
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:old", start, Some(end))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("ended_stream_locked")), "{v}");
    let (s, v) = w
        .post(
            "/v1/pro/history/samples",
            &auth,
            json!({ "platform": "twitch", "key": "twitch:old", "samples": [{ "at": start, "viewers": 1 }] }),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("ended_stream_locked")));
    // Within the day it can still be corrected.
    let recent = now_ms() - 2 * 3_600_000;
    assert_eq!(
        w.post("/v1/pro/history/streams", &auth, stream("twitch:new", recent, Some(recent + 60_000))).await.0,
        201
    );
    assert_eq!(
        w.post("/v1/pro/history/streams", &auth, stream("twitch:new", recent, Some(recent + 90_000))).await.0,
        200
    );
}

#[tokio::test]
async fn ranges_and_platforms_filter_and_bad_ones_are_refused() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let now = now_ms();
    for (i, (platform, ago)) in [("twitch", 2), ("kick", 2), ("youtube", 40), ("twitch", 200)].iter().enumerate() {
        let mut b = stream(&format!("{platform}:{i}"), now - ago * DAY, Some(now - ago * DAY + 3_600_000));
        b["platform"] = json!(platform);
        assert_eq!(w.post("/v1/pro/history/streams", &auth, b).await.0, 201);
    }
    let count = |v: &Value| v["streams"].as_array().unwrap().len();
    let (_, v) = w.get("/v1/pro/history", &auth).await;
    assert_eq!(count(&v), 2, "four weeks by default");
    let (_, v) = w.get(&format!("/v1/pro/history?from={}", now - 365 * DAY), &auth).await;
    assert_eq!(count(&v), 4);
    assert!((53..=54).contains(&v["weekly"].as_array().unwrap().len()));
    let started: Vec<i64> =
        v["streams"].as_array().unwrap().iter().map(|s| s["started_at"].as_i64().unwrap()).collect();
    assert!(started.windows(2).all(|p| p[0] >= p[1]), "newest first");
    let (_, v) = w.get(&format!("/v1/pro/history?from={}&platform=twitch", now - 365 * DAY), &auth).await;
    assert_eq!(count(&v), 2);
    let (_, v) = w.get(&format!("/v1/pro/history?from={}&to={}", now - 100 * DAY, now - 10 * DAY), &auth).await;
    assert_eq!(count(&v), 1, "the YouTube one only");
    for (q, status, code) in [
        ("from=abc", 400, "invalid"),
        (&*format!("from={now}&to={}", now - 1), 422, "invalid_range"),
        (&*format!("from={}", now - 500 * DAY), 422, "invalid_range"),
        ("platform=tiktok", 422, "invalid_platform"),
    ] {
        let (s, v) = w.get(&format!("/v1/pro/history?{q}"), &auth).await;
        assert_eq!((s, v["error"]["code"].as_str()), (status, Some(code)), "{q}: {v}");
        let (s, _, _) = w.csv(&format!("/v1/pro/history/export.csv?{q}"), &auth).await;
        assert_eq!(s, status, "export {q}");
    }
}

#[tokio::test]
async fn sixty_requests_a_minute_per_subscription_then_429() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let second = {
        w.db.add_install("i2", &crate::hash("secret")).unwrap();
        w.db.link_pro_install("i2", "sub_1").unwrap();
        "i2.secret".to_string()
    };
    let other = w.pro("i3", "sub_2");
    let mut ok = 0;
    let mut limited = None;
    for i in 0..70 {
        // Two PCs on one subscription share the rate.
        let a = if i % 2 == 0 { &auth } else { &second };
        let res = w.http.get(format!("{}/v1/pro/history", w.base)).bearer_auth(a).send().await.unwrap();
        if res.status() == 200 {
            ok += 1;
        } else {
            limited = Some(res);
            break;
        }
    }
    // A minute can roll over mid-test; then more than 60 get through overall, never fewer.
    assert!(ok >= 60, "{ok}");
    let res = limited.expect("over the rate");
    assert_eq!(res.status(), 429);
    let reset: i64 = res.headers()["x-quota-reset"].to_str().unwrap().parse().unwrap();
    assert!(reset > now_ms() && reset <= now_ms() + 61_000);
    let wait: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait));
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "history_rate");
    assert!(v["error"]["hint"].is_string());
    assert_eq!(w.get("/v1/pro/history", &other).await.0, 200, "another subscription has its own");
}

#[tokio::test]
async fn export_is_a_csv_and_stays_open_after_pro_ends() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let start = now_ms() - 5 * 3_600_000;
    let mut b = stream("twitch:e", start, Some(start + 3_600_000));
    b["title"] = json!("@everyone \"come\" in");
    assert_eq!(w.post("/v1/pro/history/streams", &auth, b).await.0, 201);
    let (s, text, h) = w.csv("/v1/pro/history/export.csv", &auth).await;
    assert_eq!(s, 200);
    assert_eq!(h["content-type"], "text/csv; charset=utf-8");
    assert!(h["content-disposition"].to_str().unwrap().contains("ferrocast-history.csv"));
    assert_eq!(h["cache-control"], "no-store");
    let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[1].starts_with("twitch,"));
    assert!(lines[1].contains("\"'@everyone \"\"come\"\" in\""), "{}", lines[1]);
    assert!(lines[1].contains("\"INR 500.00\""));
    // Pro lapses (every renewal failed) on a PC that isn't signed in: the list closes, the
    // export stays open (R-04).
    w.set_sub("sub_1", "halted", None);
    assert_eq!(w.get("/v1/pro/history", &auth).await.0, 403);
    assert_eq!(w.post("/v1/pro/history/streams", &auth, stream("twitch:z", start, None)).await.0, 403);
    let (s, after, _) = w.csv("/v1/pro/history/export.csv", &auth).await;
    assert_eq!((s, after.lines().count()), (200, 2));
    // A lapsed subscription with nothing kept, or no subscription at all: 403.
    let empty = w.pro("i2", "sub_2");
    w.set_sub("sub_2", "halted", None);
    assert_eq!(w.csv("/v1/pro/history/export.csv", &empty).await.0, 403);
    let free = w.install("i3");
    assert_eq!(w.csv("/v1/pro/history/export.csv", &free).await.0, 403);
}

#[tokio::test]
async fn history_goes_30_days_after_pro_ends_and_old_samples_go() {
    let w = world().await;
    let now = now_ms();
    let mk = |id: &str, sub: &str| {
        let a = w.pro(id, sub);
        (a, sub.to_string())
    };
    let subs = [mk("a", "sub_active"), mk("b", "sub_recent"), mk("c", "sub_gone"), mk("d", "sub_refunded")];
    for (auth, _) in &subs {
        let start = now - 3_600_000;
        assert_eq!(w.post("/v1/pro/history/streams", auth, stream("twitch:1", start, None)).await.0, 201);
        let s = json!({ "platform": "twitch", "key": "twitch:1", "samples": [{ "at": start, "viewers": 4 }] });
        assert_eq!(w.post("/v1/pro/history/samples", auth, s).await.0, 200);
    }
    // Ended 10 days ago (cancelled, period over): kept for now.
    w.set_sub("sub_recent", "cancelled", Some(now - 10 * DAY));
    // Ended 31 days ago: goes.
    w.set_sub("sub_gone", "cancelled", Some(now - 31 * DAY));
    // Refunded 40 days ago: goes.
    w.db.c().execute("UPDATE pro_subs SET refunded_at = ?1 WHERE id = 'sub_refunded'", [now - 40 * DAY]).unwrap();
    // A sample from long ago on the active one (a year and more).
    w.db.c()
        .execute(
            "INSERT INTO stream_samples (stream_id, at, viewers) SELECT id, ?1, 9 FROM streams WHERE sub_id = 'sub_active'",
            [now - SAMPLE_KEEP_MS - DAY],
        )
        .unwrap();
    w.db.prune().unwrap();
    let left: Vec<String> = {
        let c = w.db.c();
        let mut st = c.prepare("SELECT sub_id FROM streams ORDER BY sub_id").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(left, ["sub_active", "sub_recent"]);
    let samples: i64 = w.db.c().query_row("SELECT COUNT(*) FROM stream_samples", [], |r| r.get(0)).unwrap();
    assert_eq!(samples, 2, "the deleted streams' samples went with them, the old one too");
    assert_eq!(w.db.stream_samples("sub_active", "twitch", "twitch:1").unwrap().unwrap().len(), 1);
}

#[tokio::test]
async fn status_says_history_is_here() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let (s, v) = w.get("/v1/pro/status", &auth).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["features"]["history"], true);
    assert!(v["quotas"].get("history_requests").is_none(), "a request rate, not a quota");
}

// ── 1.4: free accounts keep 30 days, Pro a year ─────────────────────────────────────

#[tokio::test]
async fn a_free_account_keeps_30_days_and_sees_the_limit() {
    let w = world().await;
    let auth = w.install("f1");
    let acct = w.sign_in("f1", "free@example.com");
    let now = now_ms();
    let (s, v) = w
        .post("/v1/pro/history/streams", &auth, stream("twitch:a", now - 3 * 3_600_000, Some(now - 2 * 3_600_000)))
        .await;
    assert_eq!(s, 201, "{v}");
    let samples = json!({ "platform": "twitch", "key": "twitch:a", "samples": [{ "at": now - 3 * 3_600_000 + 60_000, "viewers": 7 }] });
    assert_eq!(w.post("/v1/pro/history/samples", &auth, samples).await.0, 200);
    // Older than 30 days isn't taken on a free account.
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:old", now - 31 * DAY, None)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_stream")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("30 days"), "{v}");
    // Asking for 90 days: the 30 kept, and `limited` says there's more to Pro.
    let (s, v) = w.get(&format!("/v1/pro/history?from={}", now - 90 * DAY), &auth).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["plan"].as_str(), v["keep_days"].as_i64(), v["limited"].as_bool()),
        (Some("free"), Some(30), Some(true))
    );
    assert_eq!(v["streams"].as_array().unwrap().len(), 1);
    assert!(v["from"].as_i64().unwrap() >= now - 30 * DAY);
    let (_, v) = w.get("/v1/pro/history", &auth).await;
    assert_eq!(v["limited"], false, "the default four weeks fit");
    // The per-stream chart and the export work for free.
    let (s, v) = w.get("/v1/pro/history/samples?platform=twitch&key=twitch:a", &auth).await;
    assert_eq!((s, v["samples"][0]["viewers"].as_i64()), (200, Some(7)), "{v}");
    let (s, text, _) = w.csv("/v1/pro/history/export.csv", &auth).await;
    assert_eq!((s, text.lines().count()), (200, 2));
    // Rows are the account's, so another PC of the account sees them.
    w.install("f2");
    w.sign_in("f2", "free@example.com");
    let (_, v) = w.get("/v1/pro/history", "f2.secret").await;
    assert_eq!(v["streams"].as_array().unwrap().len(), 1);
    let owner: String = w.db.c().query_row("SELECT sub_id FROM streams", [], |r| r.get(0)).unwrap();
    assert_eq!(owner, acct);
}

#[tokio::test]
async fn samples_are_read_back_per_stream_with_clear_errors() {
    let w = world().await;
    let auth = w.pro("p1", "sub_1");
    let start = now_ms() - 3_600_000;
    assert_eq!(w.post("/v1/pro/history/streams", &auth, stream("twitch:s", start, None)).await.0, 201);
    let b = json!({ "platform": "twitch", "key": "twitch:s", "samples": [
        { "at": start + 600_000, "viewers": 20 }, { "at": start + 300_000, "viewers": 10 } ] });
    assert_eq!(w.post("/v1/pro/history/samples", &auth, b).await.0, 200);
    let (s, v) = w.get("/v1/pro/history/samples?platform=twitch&key=twitch:s", &auth).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["samples"],
        json!([{ "at": start + 300_000, "viewers": 10 }, { "at": start + 600_000, "viewers": 20 }])
    );
    for (q, status, code) in [
        ("platform=tiktok&key=a", 422, "invalid_platform"),
        ("platform=twitch", 422, "invalid_key"),
        ("platform=twitch&key=twitch:none", 404, "stream_not_found"),
    ] {
        let (s, v) = w.get(&format!("/v1/pro/history/samples?{q}"), &auth).await;
        assert_eq!((s, v["error"]["code"].as_str()), (status, Some(code)), "{q}: {v}");
        assert!(v["error"]["hint"].is_string(), "{q}");
    }
    let other = w.pro("p2", "sub_2");
    assert_eq!(w.get("/v1/pro/history/samples?platform=twitch&key=twitch:s", &other).await.0, 404);
}

#[tokio::test]
async fn pro_on_the_account_keeps_a_year_and_sees_what_it_kept_while_free() {
    let w = world().await;
    let auth = w.install("a1");
    let acct = w.sign_in("a1", "up@example.com");
    let now = now_ms();
    let start = now - 3_600_000;
    // Started while free...
    assert_eq!(w.post("/v1/pro/history/streams", &auth, stream("twitch:x", start, None)).await.0, 201);
    assert_eq!(
        w.post("/v1/pro/history/streams", &auth, stream("twitch:y", now - 10 * DAY, Some(now - 10 * DAY + 60_000)))
            .await
            .0,
        201
    );
    // ...then Pro on the account.
    w.db.add_pro_sub_unlinked("sub_acct", "monthly", "e", "created").unwrap();
    w.set_sub("sub_acct", "active", Some(now + 30 * DAY));
    w.db.account_attach_sub(&acct, "sub_acct", "checkout", now).unwrap();
    // The end of the same stream now goes to the subscription: one row shows, the newer.
    let mut end = stream("twitch:x", start, Some(now - 60_000));
    end["follows"] = json!(42);
    assert_eq!(w.post("/v1/pro/history/streams", &auth, end).await.0, 201);
    let (_, v) = w.get(&format!("/v1/pro/history?from={}", now - 300 * DAY), &auth).await;
    assert_eq!(
        (v["plan"].as_str(), v["keep_days"].as_i64(), v["limited"].as_bool()),
        (Some("pro"), Some(365), Some(false))
    );
    let streams = v["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 2, "{v}");
    assert_eq!(streams[0]["follows"], 42, "the newer copy wins");
    // A year back is fine on Pro.
    assert_eq!(
        w.post("/v1/pro/history/streams", &auth, stream("twitch:z", now - 200 * DAY, Some(now - 200 * DAY + 60_000)))
            .await
            .0,
        201
    );
    let (s, v) = w.post("/v1/pro/history/streams", &auth, stream("twitch:w", now - 366 * DAY, None)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_stream")), "{v}");
}

#[tokio::test]
async fn the_sweep_keeps_30_days_free_and_a_year_on_pro() {
    let w = world().await;
    let now = now_ms();
    let free = w.sign_in("x", "f@example.com");
    let pro_acct = w.sign_in("y", "p@example.com");
    w.db.add_pro_sub_unlinked("sub_p", "monthly", "e", "created").unwrap();
    w.set_sub("sub_p", "active", Some(now + 30 * DAY));
    w.db.account_attach_sub(&pro_acct, "sub_p", "checkout", now).unwrap();
    w.pro("z", "sub_z");
    let put = |key: &str, platform_key: &str, started: i64| {
        w.db.c()
            .execute(
                "INSERT INTO streams (sub_id, platform, key, install_id, started_at, created_at, updated_at)
                 VALUES (?1, 'twitch', ?2, 'i', ?3, ?3, ?3)",
                rusqlite::params![key, platform_key, started],
            )
            .unwrap();
    };
    put(&free, "f:new", now - 5 * DAY);
    put(&free, "f:old", now - 31 * DAY);
    put(&pro_acct, "pa:mid", now - 100 * DAY);
    put(&pro_acct, "pa:old", now - 370 * DAY);
    put("sub_z", "s:mid", now - 300 * DAY);
    put("sub_z", "s:old", now - 400 * DAY);
    put("ac_gone", "g:new", now - DAY);
    w.db.c()
        .execute("INSERT INTO stream_samples (stream_id, at, viewers) SELECT id, ?1, 3 FROM streams", [now - 2 * DAY])
        .unwrap();
    w.db.prune().unwrap();
    let left: Vec<String> = {
        let c = w.db.c();
        let mut st = c.prepare("SELECT key FROM streams ORDER BY key").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(left, ["f:new", "pa:mid", "s:mid"]);
    let samples: i64 = w.db.c().query_row("SELECT COUNT(*) FROM stream_samples", [], |r| r.get(0)).unwrap();
    assert_eq!(samples, 3, "samples went with their streams");
}
