//! Server tests over a real socket: every way a request can be wrong gets the right status
//! and our error shape (API.md §1).

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::*;

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Running {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(format!("{}{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client.post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.client.put(format!("{}{path}", self.base)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine, ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

async fn error_code(res: reqwest::Response) -> (StatusCode, String) {
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    (status, v["error"]["code"].as_str().unwrap_or("<not our shape>").to_string())
}

#[tokio::test]
async fn auth_host_and_origin_are_enforced() {
    let r = start().await;
    let no_token = r.client.get(format!("{}/api/v1/status", r.base)).send().await.unwrap();
    assert_eq!(error_code(no_token).await, (StatusCode::UNAUTHORIZED, "unauthorized".into()));
    let wrong = r.client.get(format!("{}/api/v1/status", r.base)).bearer_auth("nope").send().await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let rebinding = r
        .client
        .get(format!("{}/api/v1/status", r.base))
        .bearer_auth(&r.token)
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(rebinding).await.0, StatusCode::FORBIDDEN);
    let cross = r
        .client
        .post(format!("{}/api/v1/test", r.base))
        .bearer_auth(&r.token)
        .header("Origin", "https://evil.example")
        .json(&json!({ "kind": "follow" }))
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(cross).await, (StatusCode::FORBIDDEN, "forbidden".into()));
    let ok = r.get("/api/v1/status").await;
    assert_eq!(ok.status(), StatusCode::OK);
    let v: Value = ok.json().await.unwrap();
    assert_eq!(v["setup_done"], false);
}

#[tokio::test]
async fn bad_requests_get_our_error_shape() {
    let r = start().await;
    let bad_json = r
        .client
        .post(format!("{}/api/v1/test", r.base))
        .bearer_auth(&r.token)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(bad_json).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    let no_type =
        r.client.post(format!("{}/api/v1/test", r.base)).bearer_auth(&r.token).body("{}").send().await.unwrap();
    assert_eq!(error_code(no_type).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "wrong_content_type".into()));
    let missing_field = r.post("/api/v1/test", json!({})).await;
    assert_eq!(error_code(missing_field).await.0, StatusCode::BAD_REQUEST);
    let huge = r.post("/api/v1/chat/send", json!({ "text": "x".repeat(2 * 1024 * 1024), "platforms": "all" })).await;
    assert_eq!(error_code(huge).await, (StatusCode::PAYLOAD_TOO_LARGE, "too_large".into()));
    let unknown_kind = r.post("/api/v1/test", json!({ "kind": "explode" })).await;
    assert_eq!(error_code(unknown_kind).await.0, StatusCode::BAD_REQUEST);
    let bad_platform = r.post("/api/v1/test", json!({ "kind": "follow", "platform": "tiktok" })).await;
    assert_eq!(error_code(bad_platform).await.0, StatusCode::BAD_REQUEST);
    let nowhere = r.get("/api/v1/nope").await;
    assert_eq!(error_code(nowhere).await, (StatusCode::NOT_FOUND, "no_route".into()));
    let missing = r.get("/api/v1/overlays/does-not-exist").await;
    assert_eq!(error_code(missing).await.0, StatusCode::NOT_FOUND);
    let not_connected = r.post("/api/v1/mod/delete", json!({ "platform": "twitch", "message_id": "x" })).await;
    assert_eq!(error_code(not_connected).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let empty = r.post("/api/v1/chat/send", json!({ "text": "   ", "platforms": "all" })).await;
    assert_eq!(error_code(empty).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn overlays_validate_and_revoke_old_urls() {
    let r = start().await;
    let created = r.post("/api/v1/overlays", json!({ "name": "Main", "preset": "starter" })).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let o: Value = created.json().await.unwrap();
    let id = o["id"].as_str().unwrap().to_string();
    let url = o["url"].as_str().unwrap().to_string();
    assert_eq!(r.client.get(&url).send().await.unwrap().status(), StatusCode::OK);
    let mut bad = o.clone();
    bad["widgets"][0]["x"] = json!(99_999);
    let res = r.put(&format!("/api/v1/overlays/{id}"), bad).await;
    assert_eq!(error_code(res).await.0, StatusCode::BAD_REQUEST);
    let fresh: Value = r.post(&format!("/api/v1/overlays/{id}/regenerate-url"), json!({})).await.json().await.unwrap();
    assert_ne!(fresh["url"], o["url"]);
    assert_eq!(r.client.get(&url).send().await.unwrap().status(), StatusCode::NOT_FOUND, "old URL is dead");
    let obs = r.post(&format!("/api/v1/overlays/{id}/add-to-obs"), json!({})).await;
    assert_eq!(error_code(obs).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.get("/o/not-a-token/").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn overlay_socket_gets_snapshot_then_alerts_and_ignores_forged_frames() {
    let r = start().await;
    let o: Value = r.post("/api/v1/overlays", json!({ "name": "Main" })).await.json().await.unwrap();
    let ws_url = o["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let first: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(first["type"], "snapshot");
    assert!(first["data"]["overlay"].get("url").is_none(), "the page never sees its own token");
    // A page tries to fake a $100M tip. It must be ignored.
    ws.send(Message::text(json!({ "type": "event", "data": { "kind": { "type": "tip" } } }).to_string()))
        .await
        .unwrap();
    ws.send(Message::text("garbage{")).await.unwrap();
    assert_eq!(r.post("/api/v1/test", json!({ "kind": "follow" })).await.status(), StatusCode::ACCEPTED);
    let alert = loop {
        let v: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if v["type"] == "alert" {
            break v;
        }
    };
    assert_eq!(alert["data"]["kind"], "follow");
    let id = alert["data"]["alert_id"].as_str().unwrap();
    ws.send(Message::text(json!({ "type": "ack", "data": { "alert_id": id, "phase": "finished" } }).to_string()))
        .await
        .unwrap();
    let mut done = false;
    for _ in 0..100 {
        let q: Value = r.get("/api/v1/queue").await.json().await.unwrap();
        if q["playing"].is_null() {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(done, "the ack finished the alert");
    let events: Value = r.get("/api/v1/events").await.json().await.unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 0, "nothing forged reached the log");
}

#[tokio::test]
async fn media_uploads_refuse_bad_files_and_serve_ranges() {
    let r = start().await;
    let up = |name: &'static str, bytes: Vec<u8>| {
        let form = reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::bytes(bytes).file_name(name));
        r.client.post(format!("{}/api/v1/media", r.base)).bearer_auth(&r.token).multipart(form).send()
    };
    let svg = up("logo.png", b"<svg onload=alert(1)></svg>".to_vec()).await.unwrap();
    assert_eq!(error_code(svg).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type".into()));
    let mut big = b"ID3".to_vec();
    big.resize(6 * 1024 * 1024, 7);
    let too_big = up("song.mp3", big).await.unwrap();
    let (status, code) = error_code(too_big).await;
    assert_eq!((status, code.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend((0..1000u32).map(|i| (i % 251) as u8));
    let ok = up("ok.png", png.clone()).await.unwrap();
    assert_eq!(ok.status(), StatusCode::CREATED);
    let m: Value = ok.json().await.unwrap();
    let url = format!("{}{}", r.base, m["url"].as_str().unwrap());
    let part = r.client.get(&url).bearer_auth(&r.token).header("Range", "bytes=8-15").send().await.unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.bytes().await.unwrap().as_ref(), &png[8..16]);
    let with_query = r.client.get(format!("{url}?session={}", r.token)).send().await.unwrap();
    assert_eq!(with_query.status(), StatusCode::OK, "img tags can use ?session=");
    let csp = with_query.headers().get("content-security-policy").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(csp.contains("sandbox"), "media on our origin can never run as a page");
    let bad_range = r.client.get(&url).bearer_auth(&r.token).header("Range", "bytes=99999-").send().await.unwrap();
    assert_eq!(bad_range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let none = up("x", vec![]).await.unwrap();
    assert_eq!(error_code(none).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pairing_and_the_live_socket() {
    let r = start().await;
    let code: Value = r.post("/api/v1/session/pair-code", json!({})).await.json().await.unwrap();
    let c = code["code"].as_str().unwrap();
    assert!(code["url"].as_str().unwrap().ends_with(&format!("#pair={c}")));
    let paired: Value = r
        .client
        .post(format!("{}/api/v1/session/pair", r.base))
        .json(&json!({ "code": c }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = paired["token"].as_str().unwrap();
    let again =
        r.client.post(format!("{}/api/v1/session/pair", r.base)).json(&json!({ "code": c })).send().await.unwrap();
    assert_eq!(error_code(again).await, (StatusCode::UNAUTHORIZED, "bad_code".into()));
    let ws_base = r.base.replace("http://", "ws://");
    assert!(tokio_tungstenite::connect_async(format!("{ws_base}/api/v1/live")).await.is_err(), "no session, no socket");
    let (mut live, _) =
        tokio_tungstenite::connect_async(format!("{ws_base}/api/v1/live?session={token}")).await.unwrap();
    let hello: Value = serde_json::from_str(live.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
}

#[tokio::test]
async fn dashboard_routes_fall_back_to_the_app() {
    let r = start().await;
    let page = r.client.get(format!("{}/app/settings/accounts", r.base)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers().get("content-security-policy").is_some());
    assert_eq!(
        r.client.get(format!("{}/app/missing.js", r.base)).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn byte_ranges() {
    use crate::assets::parse_range;
    assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
    assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
    assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
    assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
    for bad in ["bytes=100-", "bytes=9-0", "bytes=-0", "bytes=0-1,5-9", "items=0-1", "bytes=a-b", "bytes=-", ""] {
        assert_eq!(parse_range(bad, 100), None, "{bad}");
    }
    assert_eq!(parse_range("bytes=0-0", 0), None);
}

#[tokio::test]
async fn export_then_import_round_trips_and_bad_files_get_415() {
    let r = start().await;
    let o: Value = r.post("/api/v1/overlays", json!({ "name": "Main" })).await.json().await.unwrap();
    let pack = r.client.post(format!("{}/api/v1/export", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(pack.status(), StatusCode::OK);
    assert!(pack.headers()["content-disposition"].to_str().unwrap().contains(".ferropack"));
    let bytes = pack.bytes().await.unwrap().to_vec();
    r.client
        .delete(format!("{}/api/v1/overlays/{}", r.base, o["id"].as_str().unwrap()))
        .bearer_auth(&r.token)
        .send()
        .await
        .unwrap();
    let up = |b: Vec<u8>| {
        let form =
            reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::bytes(b).file_name("x.ferropack"));
        r.client.post(format!("{}/api/v1/import", r.base)).bearer_auth(&r.token).multipart(form).send()
    };
    let res = up(bytes).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list: Value = r.get("/api/v1/overlays").await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "the deleted overlay came back");
    let junk = up(b"definitely not a zip".to_vec()).await.unwrap();
    assert_eq!(error_code(junk).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_pack".into()));
}

/// A dashboard, an engine and a real relay, all in-process.
async fn start_with_relay() -> (Running, tempfile::TempDir) {
    let rdir = tempfile::tempdir().unwrap();
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("http://{}", rl.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: relay.clone(),
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: relay.clone(),
        kick_api_url: relay.clone(),
    };
    let rapp = ferrocast_relay::router(ferrocast_relay::AppState::new(
        cfg,
        ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap(),
    ))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(rl, rapp).await });
    let dir = tempfile::tempdir().unwrap();
    let apps = ferrocast_engine::PlatformApps { relay_url: Some(relay), ..Default::default() };
    let engine = Engine::start_with(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()), apps)
        .await
        .unwrap();
    let server = serve(engine, ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    let r = Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        _dir: dir,
    };
    (r, rdir)
}

#[tokio::test]
async fn the_tip_page_saves_through_the_relay_without_keeping_secrets() {
    let (r, _relay) = start_with_relay().await;
    assert_eq!(
        error_code(r.put("/api/v1/tips", json!([1, 2])).await).await,
        (StatusCode::BAD_REQUEST, "invalid".into())
    );
    let page = json!({ "handle": "  NoVa ", "display_name": "Nova", "message": "",
        "razorpay": { "page_url": "https://razorpay.me/@nova", "secret": "rzp-secret-1" } });
    let res = r.put("/api/v1/tips", page.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["handle"], "nova", "the dashboard shows the name the page really has");
    assert_eq!(v["razorpay"]["has_secret"], true);
    assert!(v["page_url"].as_str().unwrap().ends_with("/t/nova"));
    assert!(v["webhooks"]["razorpay"].as_str().is_some());
    assert!(!v.to_string().contains("rzp-secret-1"));
    // Editing without re-entering the secret keeps it.
    let mut edit = page.clone();
    edit["message"] = json!("thanks!");
    edit["razorpay"]["secret"] = json!("");
    let v: Value = r.put("/api/v1/tips", edit).await.json().await.unwrap();
    assert_eq!(v["razorpay"]["has_secret"], true);
    assert_eq!(v["message"], "thanks!");
    let stored: Value = r.get("/api/v1/tips").await.json().await.unwrap();
    assert!(!stored.to_string().contains("rzp-secret-1"), "secrets never land in the local settings");
    let mut bad = page.clone();
    bad["razorpay"]["page_url"] = json!("https://razorpay.me.evil.com/x");
    assert_eq!(error_code(r.put("/api/v1/tips", bad).await).await.0, StatusCode::BAD_REQUEST);
    // Three new names a day; the fourth is refused with 429, not a generic error.
    for name in ["nova-two", "nova-three"] {
        let mut p = page.clone();
        p["handle"] = json!(name);
        assert_eq!(r.put("/api/v1/tips", p).await.status(), StatusCode::OK, "{name}");
    }
    let mut p = page.clone();
    p["handle"] = json!("nova-four");
    assert_eq!(
        error_code(r.put("/api/v1/tips", p).await).await,
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited".into())
    );
}

#[tokio::test]
async fn tips_without_a_relay_say_so() {
    let r = start().await;
    let page = json!({ "handle": "nova", "display_name": "Nova",
        "razorpay": { "page_url": "https://razorpay.me/@nova", "secret": "rzp-secret-1" } });
    let (status, _) = error_code(r.put("/api/v1/tips", page).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let v: Value = r.get("/api/v1/tips").await.json().await.unwrap();
    assert_eq!(v["page_url"], Value::Null);
}

#[tokio::test]
async fn imports_have_their_own_size_limit_and_clear_errors() {
    let r = start().await;
    let upload = |bytes: Vec<u8>| {
        let form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(bytes).file_name("x.ferropack"));
        r.client.post(format!("{}/api/v1/import", r.base)).bearer_auth(&r.token).multipart(form).send()
    };
    // Bigger than any one media file: the media upload limit must not apply to packs.
    let res = upload(vec![0u8; 60 * 1024 * 1024]).await.unwrap();
    assert_eq!(error_code(res).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_pack".into()));
    let res = upload(Vec::new()).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Value {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test]
async fn app_routes_have_the_dashboard_shapes() {
    let r = start().await;
    let themes: Value = r.get("/api/v1/themes").await.json().await.unwrap();
    let ids: Vec<_> = themes.as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["molten", "daylight", "arcade", "minimal", "sticker"]);
    let stats: Value = r.get("/api/v1/stats/session").await.json().await.unwrap();
    for k in ["started_at", "follows", "subs", "tips", "bits", "messages", "peak_viewers", "viewers"] {
        assert!(stats.get(k).is_some(), "stats.{k}");
    }
    let up: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(up["channel"], "stable");
    assert_eq!(up["blocked_while_live"], false);
    let status: Value = r.get("/api/v1/status").await.json().await.unwrap();
    assert_eq!(status["setup_done"], false);
    assert_eq!(r.post("/api/v1/setup/complete", json!({})).await.status(), StatusCode::NO_CONTENT);
    let status: Value = r.get("/api/v1/status").await.json().await.unwrap();
    assert_eq!(status["setup_done"], true);
}

#[tokio::test]
async fn settings_save_what_was_sent_and_name_bad_fields() {
    let r = start().await;
    let res = r.put("/api/v1/settings", json!({ "theme": "dark", "close_to_tray": false })).await;
    assert_eq!(res.status(), StatusCode::OK);
    let s: Value = r.get("/api/v1/settings").await.json().await.unwrap();
    assert_eq!((s["theme"].as_str(), s["close_to_tray"].as_bool()), (Some("dark"), Some(false)));
    assert_eq!(s["keep_chat_days"], 30, "fields that weren't sent are kept");
    assert_eq!(s["update_channel"], "stable");
    let bad = r.put("/api/v1/settings", json!({ "theme": "neon" })).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let v: Value = bad.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().starts_with("theme"), "{v}");
    assert_eq!(r.put("/api/v1/settings", json!({ "lan_access": "yes" })).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(r.put("/api/v1/settings", json!([1])).await.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tts_goals_and_commands_speak_the_dashboards_language() {
    let r = start().await;
    // The default main currency comes from the computer's region; pin it for the test.
    r.put("/api/v1/settings", json!({ "currency": "INR" })).await;
    let t: Value = r.get("/api/v1/tts").await.json().await.unwrap();
    assert_eq!((t["min_amount"].as_f64(), t["currency"].as_str()), (Some(100.0), Some("INR")));
    let saved: Value = r
        .put(
            "/api/v1/tts",
            json!({ "enabled": true, "voice": "", "min_amount": 50, "max_chars": 200,
            "hold_for_approval": true, "read_names": true }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(saved["min_amount"], 50.0);
    assert_eq!(saved["min_amounts"][0]["minor"], 5_000, "₹50 is 5,000 paise");
    assert_eq!(saved["min_amounts"].as_array().unwrap().len(), 3, "other currencies keep their minimum");
    let bad = r.put("/api/v1/tts", json!({ "enabled": true, "min_amount": -1, "max_chars": 200 })).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(r.get("/api/v1/tts/pending").await.json::<Value>().await.unwrap().as_array().unwrap().is_empty());

    let g: Value = r
        .post("/api/v1/goals", json!({ "title": "Mic", "kind": "tips", "target": 5000, "currency": "INR" }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(g["target"], 5000.0, "tips goals are in rupees, not paise");
    let id = g["id"].as_str().unwrap();
    let adj: Value =
        r.post(&format!("/api/v1/goals/{id}/adjust"), json!({ "delta": 12.5 })).await.json().await.unwrap();
    assert_eq!(adj["current"], 12.5);
    assert_eq!(
        r.post("/api/v1/goals", json!({ "title": "No currency", "kind": "tips", "target": 5 })).await.status(),
        StatusCode::BAD_REQUEST
    );

    let c: Value = r
        .post(
            "/api/v1/commands",
            json!({ "name": "!dc", "aliases": [], "response": "hi", "roles": "mods",
            "cooldown_s": 5, "user_cooldown_s": 0, "platforms": [], "enabled": true }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(c["roles"], "mods");
    assert!(!c["id"].as_str().unwrap().is_empty(), "the engine picks an id");
}

#[tokio::test]
async fn moderation_routes_have_the_dashboard_shapes() {
    let r = start().await;
    assert!(r.get("/api/v1/mod/bans").await.json::<Value>().await.unwrap().is_array());
    let panic: Value = r.get("/api/v1/mod/panic/config").await.json().await.unwrap();
    assert_eq!(panic["hotkey"], "Ctrl+Alt+Shift+P");
    let mut slow = panic.clone();
    slow["slow_mode_seconds"] = json!(500);
    let bad: Value = r.put("/api/v1/mod/panic/config", slow).await.json().await.unwrap();
    assert!(bad["error"]["message"].as_str().unwrap().starts_with("slow_mode_seconds"));
    let filters: Value = r.get("/api/v1/mod/filters").await.json().await.unwrap();
    assert_eq!(filters["solicitation"]["action"], "delete");
    let mut f = filters.clone();
    f["timeout_seconds"] = json!(0);
    let bad: Value = r.put("/api/v1/mod/filters", f).await.json().await.unwrap();
    assert!(bad["error"]["message"].as_str().unwrap().starts_with("timeout_seconds"));
    assert_eq!(r.put("/api/v1/mod/filters", filters).await.status(), StatusCode::OK, "what we send back is accepted");
    assert_eq!(
        error_code(r.post("/api/v1/mod/held/nope/release", json!({})).await).await,
        (StatusCode::NOT_FOUND, "held_not_found".into())
    );
    assert_eq!(
        error_code(r.get("/api/v1/viewers/twitch/stranger").await).await,
        (StatusCode::NOT_FOUND, "viewer_not_found".into())
    );
    let note: Value = r.get("/api/v1/viewers/twitch/stranger/note").await.json().await.unwrap();
    assert_eq!(note["text"], "");
    let panic_on: Value = r.post("/api/v1/mod/panic", json!({ "on": true })).await.json().await.unwrap();
    assert_eq!(panic_on["on"], true);
    assert!(panic_on["applied"].is_array());
}

#[tokio::test]
async fn overlay_sockets_close_with_4404_and_preview_in_test_mode() {
    let r = start().await;
    let ws_base = r.base.replace("http://", "ws://");
    let (mut gone, _) = tokio_tungstenite::connect_async(format!("{ws_base}/o/not-a-token/ws")).await.unwrap();
    match tokio::time::timeout(std::time::Duration::from_secs(5), gone.next()).await.unwrap() {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 4404),
        other => panic!("expected a 4404 close, got {other:?}"),
    }
    let o: Value = r.post("/api/v1/overlays", json!({ "name": "Main" })).await.json().await.unwrap();
    let url = o["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut test, _) = tokio_tungstenite::connect_async(format!("{url}?test")).await.unwrap();
    let snap = next_json(&mut test).await;
    assert_eq!(snap["type"], "snapshot");
    assert!(snap["data"]["state"]["events"].as_array().unwrap().is_empty(), "no real events in a preview");
    let sample = next_json(&mut test).await;
    assert_eq!(sample["type"], "event", "a sample arrives at once");
    assert!(sample["data"]["id"].as_str().unwrap().starts_with("preview-"));
    let preview_alert = next_json(&mut test).await;
    assert_eq!((preview_alert["type"].as_str(), preview_alert["data"]["test"].as_bool()), (Some("alert"), Some(true)));
    // Overlay pages drop frames at or below the last seq they saw.
    let seqs = [&snap, &sample, &preview_alert].map(|f| f["seq"].as_u64().unwrap());
    assert!(seqs[0] < seqs[1] && seqs[1] < seqs[2], "each frame gets its own, growing seq: {seqs:?}");
    // The real overlay gets a real test alert; the preview socket must not.
    let (mut real, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    assert_eq!(next_json(&mut real).await["type"], "snapshot");
    r.post("/api/v1/test", json!({ "kind": "raid" })).await;
    loop {
        let v = next_json(&mut real).await;
        if v["type"] == "alert" {
            break;
        }
    }
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(500), test.next()).await;
    assert!(quiet.is_err(), "nothing from the real queue reaches a preview: {quiet:?}");
    // Regenerating the URL tells the page, then closes with 4404.
    let id = o["id"].as_str().unwrap();
    r.post(&format!("/api/v1/overlays/{id}/regenerate-url"), json!({})).await;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), real.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "revoked" {
                    break;
                }
            }
            other => panic!("expected the revoked frame first, got {other:?}"),
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), real.next()).await.unwrap() {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 4404),
        other => panic!("expected a 4404 close, got {other:?}"),
    }
}

#[tokio::test]
async fn a_dashboard_that_reconnects_catches_up_on_missed_events() {
    let r = start().await;
    let ws_base = r.base.replace("http://", "ws://");
    let live_url = format!("{ws_base}/api/v1/live?session={}", r.token);
    let (mut live, _) = tokio_tungstenite::connect_async(&live_url).await.unwrap();
    let hello = next_json(&mut live).await;
    let seen = hello["data"]["seq"].as_u64().unwrap();
    drop(live);
    for kind in ["follow", "raid"] {
        r.post("/api/v1/test", json!({ "kind": kind })).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (mut again, _) = tokio_tungstenite::connect_async(format!("{live_url}&since={seen}")).await.unwrap();
    let mut kinds = Vec::new();
    for _ in 0..8 {
        let v = next_json(&mut again).await;
        if v["type"] == "event" {
            kinds.push(v["data"]["kind"]["type"].as_str().unwrap_or("").to_string());
            if kinds.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(kinds, ["follow", "raid"]);
    // A `since` from before this start (an earlier run's seq) asks for a refetch.
    let (mut stale, _) = tokio_tungstenite::connect_async(format!("{live_url}&since={}", seen / 2)).await.unwrap();
    let mut types = Vec::new();
    for _ in 0..4 {
        types.push(next_json(&mut stale).await["type"].as_str().unwrap_or("").to_string());
    }
    assert!(types.contains(&"resync".to_string()), "{types:?}");
    // A malformed query is ignored, not a plain-text error page.
    let (mut odd, _) = tokio_tungstenite::connect_async(format!("{live_url}&since=abc&since=")).await.unwrap();
    assert_eq!(next_json(&mut odd).await["type"], "hello");
}

#[tokio::test]
async fn held_tts_shows_who_and_what_even_for_test_events() {
    let r = start().await;
    r.put("/api/v1/tts", json!({ "enabled": true, "min_amount": 0, "hold_for_approval": true })).await;
    r.post("/api/v1/test", json!({ "kind": "tip" })).await;
    let mut pending = Value::Null;
    for _ in 0..100 {
        pending = r.get("/api/v1/tts/pending").await.json().await.unwrap();
        if !pending.as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let item = &pending[0];
    assert_ne!(item["name"], "Someone", "{item}");
    assert!(item["platform"].is_string());
    assert!(item["amount"].as_str().is_some_and(|a| !a.is_empty()), "{item}");
}

#[tokio::test]
async fn settings_and_goal_errors_say_which_field_and_why() {
    let r = start().await;
    let bad: Value = r.put("/api/v1/settings", json!({ "lan_access": "yes" })).await.json().await.unwrap();
    assert_eq!(bad["error"]["message"], "lan_access: must be true or false.");
    let bad: Value = r.put("/api/v1/settings", json!({ "currency": "rupees" })).await.json().await.unwrap();
    assert!(bad["error"]["message"].as_str().unwrap().starts_with("currency"));
    let saved: Value = r.put("/api/v1/settings", json!({ "currency": "USD" })).await.json().await.unwrap();
    assert_eq!(saved["currency"], "USD");
    let t: Value = r.put("/api/v1/tts", json!({ "min_amount": 5 })).await.json().await.unwrap();
    assert_eq!((t["currency"].as_str(), t["min_amount"].as_f64()), (Some("USD"), Some(5.0)), "the main currency");
    let usd = t["min_amounts"].as_array().unwrap().iter().filter(|m| m["currency"] == "USD").count();
    assert_eq!(usd, 1, "one minimum per currency");
    let bad = r.put("/api/v1/tts", json!({ "min_amounts": [{ "minor": -1, "currency": "INR" }] })).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        r.post("/api/v1/goals", json!({ "title": "Half", "kind": "follows", "target": 2.5 })).await.status(),
        StatusCode::BAD_REQUEST
    );
    let g: Value = r
        .post("/api/v1/goals", json!({ "title": "Mic", "kind": "tips", "target": 50, "currency": "INR" }))
        .await
        .json()
        .await
        .unwrap();
    let id = g["id"].as_str().unwrap();
    r.post(&format!("/api/v1/goals/{id}/adjust"), json!({ "delta": 10 })).await;
    let mut edit = g.clone();
    edit["title"] = json!("New mic");
    edit["current"] = json!(0);
    let saved: Value = r.put(&format!("/api/v1/goals/{id}"), edit.clone()).await.json().await.unwrap();
    assert_eq!((saved["title"].as_str(), saved["current"].as_f64()), (Some("New mic"), Some(10.0)), "progress kept");
    edit["currency"] = json!("JPY");
    assert_eq!(r.put(&format!("/api/v1/goals/{id}"), edit).await.status(), StatusCode::CONFLICT);
    let gone = r.client.delete(format!("{}/api/v1/tips", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(gone.status(), StatusCode::UNPROCESSABLE_ENTITY, "no relay in this build");
}

#[test]
fn obs_version_is_the_bare_number() {
    let view = ferrocast_engine::ObsView {
        state: "connected".into(),
        host: "127.0.0.1".into(),
        port: 4455,
        has_password: false,
        detail: None,
        status: Some(ferrocast_obs::ObsStatus {
            obs_version: "31.1.2".into(),
            websocket_version: "5.6".into(),
            scene: "Gameplay".into(),
        }),
    };
    let v = crate::routes::obs_view_for_tests(&view);
    assert_eq!((v["version"].as_str(), v["scene"].as_str()), (Some("31.1.2"), Some("Gameplay")));
}
