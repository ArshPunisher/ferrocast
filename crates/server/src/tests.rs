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
    engine: Engine,
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
    /// A body that isn't JSON at all, with the JSON content type.
    async fn raw(&self, method: reqwest::Method, path: &str, body: &'static str) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }
    /// The same request without the session token.
    async fn anonymous(&self, method: reqwest::Method, path: &str) -> reqwest::Response {
        self.client.request(method, format!("{}{path}", self.base)).json(&json!({})).send().await.unwrap()
    }
}

/// A person who chatted once, so they have a viewer card (and a balance).
async fn seen(r: &Running, platform: ferrocast_core::event::Platform, user: &str) {
    use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis};
    let mut a = Actor::named(platform, user);
    a.id = Some(format!("{user}-id"));
    let kind = EventKind::ChatMessage { actor: a, content: Content::plain("hi"), reply_to: None, is_action: false };
    r.engine.ingest(Event::live(platform, Some(format!("seen-{user}")), Millis(0), kind)).await;
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        engine,
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
        tip_url: None,
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
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    let r = Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        engine,
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
    assert_eq!(stats["live"], false);
    for k in ["started_at", "follows", "subs", "tips", "bits", "messages", "peak_viewers", "viewers"] {
        assert!(stats.get(k).is_some(), "stats.{k}");
    }
    let up: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(up["channel"], ferrocast_engine::update_channel(env!("CARGO_PKG_VERSION")));
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
    assert!(s.get("update_channel").is_none(), "the channel follows the build, not a setting");
    // An older dashboard may still send it: accepted and ignored.
    assert_eq!(r.put("/api/v1/settings", json!({ "update_channel": "beta" })).await.status(), StatusCode::OK);
    let s: Value = r.get("/api/v1/settings").await.json().await.unwrap();
    assert!(s.get("update_channel").is_none());
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
        auto: true,
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

#[tokio::test]
async fn lan_pairing_links_point_at_this_computer_on_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine, ServeOptions { port: 0, lan: true, dev_origins: vec![] }).await.unwrap();
    let res = reqwest::Client::new()
        .post(format!("{}/api/v1/session/pair-code", server.base_url()))
        .bearer_auth(server.sessions.desktop_token())
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let v: Value = res.json().await.unwrap();
    let url = v["url"].as_str().unwrap();
    assert!(url.contains(&format!(":{}/app/#pair=", server.addr.port())), "{url}");
    // On a machine with a network, the phone gets a reachable address, not its own loopback.
    if let Some(ip) = super::routes::lan_ip() {
        assert!(url.starts_with(&format!("http://{ip}:")), "{url}");
        assert_eq!(v["lan"], json!({ "active": true, "restart_needed": false }));
    }
}

#[tokio::test]
async fn pairing_links_say_when_other_devices_must_wait_for_a_restart() {
    let r = start().await;
    let v: Value = r.post("/api/v1/session/pair-code", json!({})).await.json().await.unwrap();
    assert!(v["url"].as_str().unwrap().starts_with("http://127.0.0.1:"), "{v}");
    assert_eq!(v["lan"], json!({ "active": false, "restart_needed": false }));
    // Switched on now, but the server still listens on loopback until the next start.
    assert_eq!(r.put("/api/v1/settings", json!({ "lan_access": true })).await.status(), StatusCode::OK);
    let v: Value = r.post("/api/v1/session/pair-code", json!({})).await.json().await.unwrap();
    assert_eq!(v["lan"], json!({ "active": false, "restart_needed": true }));
}

#[tokio::test]
async fn loyalty_routes_refuse_every_kind_of_bad_request() {
    use reqwest::Method;
    let r = start().await;
    seen(&r, ferrocast_core::event::Platform::Twitch, "sam").await;
    let cfg: Value = r.get("/api/v1/loyalty").await.json().await.unwrap();
    assert_eq!(
        (cfg["name"].as_str(), cfg["per_interval"].as_u64(), cfg["enabled"].as_bool()),
        (Some("points"), Some(10), Some(true))
    );
    // Missing auth on every loyalty route.
    for (m, path) in [
        (Method::GET, "/api/v1/loyalty"),
        (Method::PUT, "/api/v1/loyalty"),
        (Method::GET, "/api/v1/loyalty/leaderboard"),
        (Method::GET, "/api/v1/loyalty/viewers/twitch/sam-id"),
        (Method::POST, "/api/v1/loyalty/viewers/twitch/sam-id/adjust"),
    ] {
        assert_eq!(error_code(r.anonymous(m, path).await).await.0, StatusCode::UNAUTHORIZED, "{path}");
    }
    // Bad JSON and wrong shapes.
    let bad = r.raw(Method::PUT, "/api/v1/loyalty", "{nope").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    let bad = r.raw(Method::POST, "/api/v1/loyalty/viewers/twitch/sam-id/adjust", "[1,").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    let msg = |res: reqwest::Response| async move {
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        (status, v["error"]["message"].as_str().unwrap_or("").to_string())
    };
    let (s, m) = msg(r.put("/api/v1/loyalty", json!(["not", "an object"])).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{m}");
    for (body, field) in [
        (json!({ "per_interval": "ten" }), "per_interval:"),
        (json!({ "per_interval": -1 }), "per_interval:"),
        (json!({ "per_interval": 2_000_000 }), "per_interval:"),
        (json!({ "interval_min": 0 }), "interval_min:"),
        (json!({ "enabled": "yes" }), "enabled:"),
        (json!({ "name": "" }), "name:"),
        (json!({ "name": 5 }), "name:"),
        (json!({ "raid_bonus": 1.5 }), "raid_bonus:"),
    ] {
        let (s, m) = msg(r.put("/api/v1/loyalty", body.clone()).await).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        assert!(m.starts_with(field), "{body}: {m}");
    }
    let saved: Value =
        r.put("/api/v1/loyalty", json!({ "name": "Coins", "follow_bonus": 0 })).await.json().await.unwrap();
    assert_eq!(
        (saved["name"].as_str(), saved["follow_bonus"].as_u64(), saved["sub_bonus"].as_u64()),
        (Some("Coins"), Some(0), Some(250))
    );
    // Adjust: wrong types, out of range, unknown people and platforms.
    let adjust = "/api/v1/loyalty/viewers/twitch/sam-id/adjust";
    for body in [
        json!({}),
        json!({ "delta": "5" }),
        json!({ "delta": 1.5 }),
        json!({ "delta": 0 }),
        json!({ "delta": 1e12 }),
        json!({ "delta": null }),
    ] {
        let (s, m) = msg(r.post(adjust, body.clone()).await).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        assert!(m.starts_with("delta:"), "{body}: {m}");
    }
    let ok: Value = r.post(adjust, json!({ "delta": 300 })).await.json().await.unwrap();
    assert_eq!((ok["points"].as_i64(), ok["name"].as_str()), (Some(300), Some("Coins")));
    let (s, m) = msg(r.post(adjust, json!({ "delta": -301 })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(m.contains("at most 300"), "{m}");
    let unknown = r.post("/api/v1/loyalty/viewers/kick/ghost/adjust", json!({ "delta": 5 })).await;
    assert_eq!(error_code(unknown).await, (StatusCode::NOT_FOUND, "viewer_not_found".into()));
    let unknown = r.get("/api/v1/loyalty/viewers/youtube/ghost").await;
    assert_eq!(error_code(unknown).await, (StatusCode::NOT_FOUND, "viewer_not_found".into()));
    let platform = r.get("/api/v1/loyalty/viewers/tiktok/sam-id").await;
    assert_eq!(error_code(platform).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    // Leaderboard and the viewer card.
    for q in ["?limit=0", "?limit=101", "?limit=ten", "?limit=-1"] {
        let res = r.get(&format!("/api/v1/loyalty/leaderboard{q}")).await;
        assert_eq!(error_code(res).await, (StatusCode::BAD_REQUEST, "invalid".into()), "{q}");
    }
    let top: Value = r.get("/api/v1/loyalty/leaderboard?limit=5").await.json().await.unwrap();
    assert_eq!(top[0]["display_name"], "sam");
    assert_eq!((top[0]["rank"].as_u64(), top[0]["points"].as_i64()), (Some(1), Some(300)));
    let card: Value = r.get("/api/v1/viewers/twitch/sam-id").await.json().await.unwrap();
    assert_eq!(card["points"], 300);
    let bal: Value = r.get("/api/v1/loyalty/viewers/twitch/sam-id").await.json().await.unwrap();
    assert_eq!(bal["points"], 300);
}

#[tokio::test]
async fn poll_routes_validate_and_refuse_races() {
    use reqwest::Method;
    let r = start().await;
    assert_eq!(r.get("/api/v1/polls/current").await.json::<Value>().await.unwrap(), Value::Null);
    for (m, path) in [
        (Method::GET, "/api/v1/polls/current"),
        (Method::POST, "/api/v1/polls"),
        (Method::POST, "/api/v1/polls/current/end"),
        (Method::DELETE, "/api/v1/polls/current"),
    ] {
        assert_eq!(error_code(r.anonymous(m, path).await).await.0, StatusCode::UNAUTHORIZED, "{path}");
    }
    let bad = r.raw(Method::POST, "/api/v1/polls", "{\"question\":").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    let msg = |res: reqwest::Response| async move {
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        (status, v["error"]["message"].as_str().unwrap_or("").to_string())
    };
    let ok = json!({ "question": "Which map?", "options": ["Ascent", "Bind"], "duration_s": 60 });
    let mut cases = vec![
        (json!([]), "Send the poll"),
        (json!({ "options": ["a", "b"] }), "question:"),
        (json!({ "question": 5, "options": ["a", "b"] }), "question:"),
        (json!({ "question": " ", "options": ["a", "b"] }), "question:"),
        (json!({ "question": "q" }), "options:"),
        (json!({ "question": "q", "options": "a,b" }), "options:"),
        (json!({ "question": "q", "options": ["a", 2] }), "options:"),
        (json!({ "question": "q", "options": ["a"] }), "options:"),
        (json!({ "question": "q", "options": ["a", "b", "c", "d", "e", "f"] }), "options:"),
        (json!({ "question": "q", "options": ["a", "A"] }), "options:"),
    ];
    for d in [json!(5), json!(1801), json!(-1), json!(1.5), json!("60"), json!(1e20)] {
        let mut b = ok.clone();
        b["duration_s"] = d;
        cases.push((b, "duration_s:"));
    }
    for (body, prefix) in cases {
        let (s, m) = msg(r.post("/api/v1/polls", body.clone()).await).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        assert!(m.starts_with(prefix), "{body}: {m}");
    }
    let none = r.post("/api/v1/polls/current/end", json!({})).await;
    assert_eq!(error_code(none).await, (StatusCode::NOT_FOUND, "poll_not_found".into()));
    let created = r.post("/api/v1/polls", ok.clone()).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let p: Value = created.json().await.unwrap();
    assert_eq!((p["closed"].as_bool(), p["total"].as_u64()), (Some(false), Some(0)));
    let again = r.post("/api/v1/polls", ok.clone()).await;
    assert_eq!(error_code(again).await, (StatusCode::CONFLICT, "conflict".into()));
    // Two dashboards press "End" at once: one wins, the other gets a conflict.
    let (a, b) =
        tokio::join!(r.post("/api/v1/polls/current/end", json!({})), r.post("/api/v1/polls/current/end", json!({})));
    let mut codes = vec![a.status(), b.status()];
    codes.sort();
    assert_eq!(codes, vec![StatusCode::OK, StatusCode::CONFLICT]);
    let current: Value = r.get("/api/v1/polls/current").await.json().await.unwrap();
    assert_eq!(current["closed"], true, "the finished poll stays visible");
    assert_eq!(
        r.client
            .delete(format!("{}/api/v1/polls/current", r.base))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let gone = r.client.delete(format!("{}/api/v1/polls/current", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(error_code(gone).await, (StatusCode::NOT_FOUND, "poll_not_found".into()));
    let fresh = r.post("/api/v1/polls", ok).await;
    assert_eq!(fresh.status(), StatusCode::CREATED, "a new poll after the last one ended");
}

#[tokio::test]
async fn only_overlays_with_a_poll_widget_get_live_results() {
    let r = start().await;
    let with: Value =
        r.post("/api/v1/overlays", json!({ "name": "Poll", "preset": "blank" })).await.json().await.unwrap();
    let id = with["id"].as_str().unwrap().to_string();
    let mut edit = with.clone();
    edit["widgets"] =
        json!([{ "id": "p", "kind": "poll", "x": 0, "y": 0, "w": 600, "h": 400, "config": { "theme": "molten" } }]);
    assert_eq!(r.put(&format!("/api/v1/overlays/{id}"), edit).await.status(), StatusCode::OK);
    let without: Value =
        r.post("/api/v1/overlays", json!({ "name": "Chat", "preset": "chat_only" })).await.json().await.unwrap();
    let open = |o: &Value| o["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(open(&with)).await.unwrap();
    let (mut other, _) = tokio_tungstenite::connect_async(open(&without)).await.unwrap();
    let snap: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snap["data"]["state"]["poll"], Value::Null);
    let _ = other.next().await; // its snapshot
    r.post("/api/v1/polls", json!({ "question": "Pizza?", "options": ["Yes", "No"], "duration_s": 30 })).await;
    let state = loop {
        let v: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if v["type"] == "state" {
            break v;
        }
    };
    assert_eq!(state["data"]["poll"]["question"], "Pizza?");
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(300), other.next()).await;
    assert!(quiet.is_err(), "a chat-only overlay gets no poll frames");
}

#[tokio::test]
async fn giveaway_routes_validate_and_refuse_races() {
    use reqwest::Method;
    let r = start().await;
    assert_eq!(r.get("/api/v1/giveaways/current").await.json::<Value>().await.unwrap(), Value::Null);
    for (m, path) in [
        (Method::GET, "/api/v1/giveaways/current"),
        (Method::POST, "/api/v1/giveaways"),
        (Method::POST, "/api/v1/giveaways/current/close"),
        (Method::POST, "/api/v1/giveaways/current/draw"),
        (Method::POST, "/api/v1/giveaways/current/redraw"),
        (Method::POST, "/api/v1/giveaways/current/end"),
        (Method::DELETE, "/api/v1/giveaways/current"),
    ] {
        assert_eq!(error_code(r.anonymous(m, path).await).await.0, StatusCode::UNAUTHORIZED, "{path}");
    }
    let bad = r.raw(Method::POST, "/api/v1/giveaways", "{\"prize\"").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    let msg = |res: reqwest::Response| async move {
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        (status, v["error"]["message"].as_str().unwrap_or("").to_string())
    };
    for (body, prefix) in [
        (json!("prize"), "Send the giveaway"),
        (json!({}), "prize:"),
        (json!({ "prize": 5 }), "prize:"),
        (json!({ "prize": "  " }), "prize:"),
        (json!({ "prize": "Key", "keyword": "join" }), "keyword:"),
        (json!({ "prize": "Key", "keyword": 1 }), "keyword:"),
        (json!({ "prize": "Key", "cost": -1 }), "cost:"),
        (json!({ "prize": "Key", "cost": 1.5 }), "cost:"),
        (json!({ "prize": "Key", "cost": "10" }), "cost:"),
        (json!({ "prize": "Key", "cost": 2_000_000_000 }), "cost:"),
        (json!({ "prize": "Key", "subscriber_bonus": "yes" }), "subscriber_bonus:"),
    ] {
        let (s, m) = msg(r.post("/api/v1/giveaways", body.clone()).await).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        assert!(m.starts_with(prefix), "{body}: {m}");
    }
    for path in ["close", "draw", "redraw", "end"] {
        let res = r.post(&format!("/api/v1/giveaways/current/{path}"), json!({})).await;
        assert_eq!(error_code(res).await, (StatusCode::NOT_FOUND, "giveaway_not_found".into()), "{path}");
    }
    let created = r.post("/api/v1/giveaways", json!({ "prize": "A game key", "subscriber_bonus": true })).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let g: Value = created.json().await.unwrap();
    assert_eq!((g["keyword"].as_str(), g["open"].as_bool(), g["cost"].as_i64()), (Some("!join"), Some(true), Some(0)));
    let again = r.post("/api/v1/giveaways", json!({ "prize": "Another" })).await;
    assert_eq!(error_code(again).await, (StatusCode::CONFLICT, "conflict".into()));
    let nobody = r.post("/api/v1/giveaways/current/draw", json!({})).await;
    assert_eq!(error_code(nobody).await, (StatusCode::UNPROCESSABLE_ENTITY, "unavailable".into()));
    seen(&r, ferrocast_core::event::Platform::Twitch, "ana").await;
    seen(&r, ferrocast_core::event::Platform::Twitch, "bo").await;
    use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
    for user in ["ana", "bo"] {
        let mut a = Actor::named(Platform::Twitch, user);
        a.id = Some(format!("{user}-id"));
        let kind =
            EventKind::ChatMessage { actor: a, content: Content::plain("!join"), reply_to: None, is_action: false };
        r.engine.ingest(Event::live(Platform::Twitch, Some(format!("join-{user}")), Millis(0), kind)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let (a, b) = tokio::join!(
        r.post("/api/v1/giveaways/current/draw", json!({})),
        r.post("/api/v1/giveaways/current/draw", json!({}))
    );
    let mut codes = vec![a.status(), b.status()];
    codes.sort();
    assert_eq!(codes, vec![StatusCode::OK, StatusCode::CONFLICT], "a double-draw draws once");
    let re: Value = r.post("/api/v1/giveaways/current/redraw", json!({})).await.json().await.unwrap();
    assert_eq!(re["winners"].as_array().unwrap().len(), 2);
    let none_left = r.post("/api/v1/giveaways/current/redraw", json!({})).await;
    assert_eq!(error_code(none_left).await, (StatusCode::UNPROCESSABLE_ENTITY, "unavailable".into()));
    assert_eq!(r.post("/api/v1/giveaways/current/end", json!({})).await.status(), StatusCode::OK);
    let twice = r.post("/api/v1/giveaways/current/end", json!({})).await;
    assert_eq!(error_code(twice).await, (StatusCode::CONFLICT, "conflict".into()));
    let cancelled =
        r.client.delete(format!("{}/api/v1/giveaways/current", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(cancelled.json::<Value>().await.unwrap()["refunded"], 0);
    // Paid entries need points turned on.
    r.put("/api/v1/loyalty", json!({ "enabled": false })).await;
    let (s, m) = msg(r.post("/api/v1/giveaways", json!({ "prize": "Key", "cost": 10 })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(m.starts_with("cost:"), "{m}");
}

#[tokio::test]
async fn welcome_settings_name_bad_fields() {
    use reqwest::Method;
    let r = start().await;
    let cfg: Value = r.get("/api/v1/welcome").await.json().await.unwrap();
    assert_eq!((cfg["enabled"].as_bool(), cfg["returning_days"].as_u64()), (Some(false), Some(30)));
    for m in [Method::GET, Method::PUT] {
        assert_eq!(error_code(r.anonymous(m, "/api/v1/welcome").await).await.0, StatusCode::UNAUTHORIZED);
    }
    let bad = r.raw(Method::PUT, "/api/v1/welcome", "nope").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    for (body, field) in [
        (json!(5), "The welcome"),
        (json!({ "enabled": 1 }), "enabled:"),
        (json!({ "template": "" }), "template:"),
        (json!({ "template": "{name" }), "template:"),
        (json!({ "template": 5 }), "template:"),
        (json!({ "returning_days": 0 }), "returning_days:"),
        (json!({ "returning_days": -3 }), "returning_days:"),
        (json!({ "returning_badge": "no" }), "returning_badge:"),
    ] {
        let res = r.put("/api/v1/welcome", body.clone()).await;
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(v["error"]["message"].as_str().unwrap().starts_with(field), "{body}: {v}");
    }
    let saved: Value =
        r.put("/api/v1/welcome", json!({ "enabled": true, "returning_days": 60 })).await.json().await.unwrap();
    assert_eq!((saved["enabled"].as_bool(), saved["returning_days"].as_u64()), (Some(true), Some(60)));
}

#[tokio::test]
async fn recap_route_checks_since() {
    use reqwest::Method;
    let r = start().await;
    assert_eq!(error_code(r.anonymous(Method::GET, "/api/v1/recap").await).await.0, StatusCode::UNAUTHORIZED);
    let empty: Value = r.get("/api/v1/recap").await.json().await.unwrap();
    assert_eq!(empty["started_at"], Value::Null);
    assert_eq!(empty["thanks"], json!([]));
    for q in ["?since=-1", "?since=soon", "?since=99999999999999", "?since=1.5"] {
        let res = r.get(&format!("/api/v1/recap{q}")).await;
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
        assert!(v["error"]["message"].as_str().unwrap().starts_with("since:"), "{q}");
    }
    let ok: Value = r.get("/api/v1/recap?since=0").await.json().await.unwrap();
    assert_eq!(ok["started_at"], 0);
    for key in [
        "follows",
        "subs",
        "tips",
        "bits",
        "messages",
        "peak_viewers",
        "new_followers",
        "subscribers",
        "tippers",
        "raiders",
        "top_chatters",
        "live",
        "ended_at",
    ] {
        assert!(ok.get(key).is_some(), "{key}");
    }
}

#[tokio::test]
async fn discord_settings_keep_the_webhook_secret() {
    use reqwest::Method;
    let r = start().await;
    for (m, p) in
        [(Method::GET, "/api/v1/discord"), (Method::PUT, "/api/v1/discord"), (Method::POST, "/api/v1/discord/test")]
    {
        assert_eq!(error_code(r.anonymous(m, p).await).await.0, StatusCode::UNAUTHORIZED, "{p}");
    }
    let v: Value = r.get("/api/v1/discord").await.json().await.unwrap();
    assert_eq!((v["has_webhook"].as_bool(), v["webhook_hint"].clone()), (Some(false), Value::Null));
    let no_hook = r.post("/api/v1/discord/test", json!({})).await;
    assert_eq!(error_code(no_hook).await, (StatusCode::UNPROCESSABLE_ENTITY, "unavailable".into()));
    let bad = r.raw(Method::PUT, "/api/v1/discord", "{").await;
    assert_eq!(error_code(bad).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    for (body, field) in [
        (json!([]), "The Discord"),
        (json!({ "webhook_url": "https://evil.example/api/webhooks/1/2" }), "webhook_url:"),
        (
            json!({ "webhook_url": "http://discord.com/api/webhooks/112233445566778899/AbCdEfGhIjKlMnOpQrStUvWx" }),
            "webhook_url:",
        ),
        (json!({ "webhook_url": 5 }), "webhook_url:"),
        (json!({ "template": "" }), "template:"),
        (json!({ "template": "{x" }), "template:"),
        (json!({ "enabled": "yes" }), "enabled:"),
        (json!({ "mention_everyone": 1 }), "mention_everyone:"),
    ] {
        let res = r.put("/api/v1/discord", body.clone()).await;
        let status = res.status();
        let v: Value = res.json().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(v["error"]["message"].as_str().unwrap().starts_with(field), "{body}: {v}");
    }
    let url = "https://discord.com/api/webhooks/112233445566778899/AbCdEfGhIjKlMnOpQrStUvWx_yz-0123";
    let saved = r.put("/api/v1/discord", json!({ "webhook_url": url })).await.text().await.unwrap();
    assert!(!saved.contains("AbCdEf"), "never echoed: {saved}");
    let v: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        (v["has_webhook"].as_bool(), v["webhook_hint"].as_str()),
        (Some(true), Some("discord.com webhook …8899"))
    );
    // It's not in the database, so it's never in an export either.
    let stored: String = r
        .engine
        .store()
        .read(|c| Ok(c.query_row("SELECT COALESCE(group_concat(value), '') FROM settings", [], |row| row.get(0))?))
        .unwrap();
    assert!(stored.contains("template") && !stored.contains("AbCdEf"), "settings hold the template, never the webhook");
    let cleared: Value = r.put("/api/v1/discord", json!({ "webhook_url": null })).await.json().await.unwrap();
    assert_eq!(cleared["has_webhook"], false);
}

#[tokio::test]
async fn obs_detect_answers_with_a_reason_and_never_a_password() {
    use reqwest::Method;
    let r = start().await;
    for m in [Method::GET, Method::POST] {
        assert_eq!(error_code(r.anonymous(m, "/api/v1/obs/detect").await).await.0, StatusCode::UNAUTHORIZED);
    }
    let res = r.get("/api/v1/obs/detect").await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: Value = res.json().await.unwrap();
    for key in ["found", "port", "has_password", "server_enabled", "in_use", "reason"] {
        assert!(v.get(key).is_some(), "{key}");
    }
    assert!(v.get("password").is_none());
    if v["found"] == false {
        let used = r.post("/api/v1/obs/detect", json!({})).await;
        assert_eq!(error_code(used).await, (StatusCode::UNPROCESSABLE_ENTITY, "unavailable".into()));
    }
}

#[tokio::test]
async fn starter_commands_are_added_once() {
    use reqwest::Method;
    let r = start().await;
    assert_eq!(
        error_code(r.anonymous(Method::POST, "/api/v1/commands/starter").await).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (x, y) =
        tokio::join!(r.post("/api/v1/commands/starter", json!({})), r.post("/api/v1/commands/starter", json!({})));
    assert_eq!((x.status(), y.status()), (StatusCode::OK, StatusCode::OK), "a double click on a fresh install is fine");
    let (x, y): (Value, Value) = (x.json().await.unwrap(), y.json().await.unwrap());
    assert_eq!(x["added"].as_array().unwrap().len() + y["added"].as_array().unwrap().len(), 4);
    let first = if x["added"].as_array().unwrap().is_empty() { y } else { x };
    assert!(first["added"].as_array().unwrap().iter().all(|c| !c["id"].as_str().unwrap().is_empty()));
    let (a, b) =
        tokio::join!(r.post("/api/v1/commands/starter", json!({})), r.post("/api/v1/commands/starter", json!({})));
    for res in [a, b] {
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["added"], json!([]), "never twice, even with two clicks at once");
    }
    let all: Value = r.get("/api/v1/commands").await.json().await.unwrap();
    assert_eq!(all.as_array().unwrap().len(), 4);
}

async fn go_live(r: &Running, on: bool) {
    use ferrocast_core::event::{Event, EventKind, Millis, Platform};
    let kind = if on { EventKind::StreamOnline { title: None } } else { EventKind::StreamOffline };
    r.engine.ingest(Event::live(Platform::Twitch, Some(format!("live-{on}")), Millis(i64::from(on)), kind)).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while r.engine.is_live() != on && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(r.engine.is_live(), on);
}

fn found(version: &str) -> ferrocast_engine::AvailableUpdate {
    ferrocast_engine::AvailableUpdate {
        version: version.into(),
        notes_url: Some("https://example.com/notes".into()),
        notes: Some("- Faster alerts".into()),
        published_at: Some("2026-10-20T11:16:05Z".into()),
    }
}

#[tokio::test]
async fn updates_are_offered_only_when_found_and_never_while_live() {
    let r = start().await;
    let none: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(none["latest"], Value::Null);
    assert_eq!(none["checked_at"], Value::Null, "not checked yet is not the same as up to date");
    assert_eq!((none["checking"].as_bool(), none["can_install"].as_bool()), (Some(false), Some(false)));
    assert_eq!(none["install"]["phase"], "idle");
    let nothing = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(error_code(nothing).await, (StatusCode::NOT_FOUND, "no_update".into()));

    r.engine.attach_updater();
    r.engine.set_available_update(Some(found("9.9.9")));
    let found: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!((found["latest"].as_str(), found["blocked_while_live"].as_bool()), (Some("9.9.9"), Some(false)));
    assert_eq!((found["notes"].as_str(), found["can_install"].as_bool()), (Some("- Faster alerts"), Some(true)));
    assert!(found["checked_at"].as_i64().is_some());
    let no_token = r.client.post(format!("{}/api/v1/updates/install", r.base)).send().await.unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

    // Live, with "never update while live" on by default: the update waits.
    go_live(&r, true).await;
    let live: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(live["blocked_while_live"], true);
    let blocked = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(error_code(blocked).await, (StatusCode::CONFLICT, "live".into()));

    go_live(&r, false).await;
    let ok = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(ok.status(), StatusCode::ACCEPTED);
    let v: Value = ok.json().await.unwrap();
    assert_eq!(v["installing"], "9.9.9");
    // The data was backed up first, labelled with why and by which version.
    let backups: Value = r.get("/api/v1/backups").await.json().await.unwrap();
    let before =
        backups.as_array().unwrap().iter().find(|b| b["reason"] == "update").expect("a backup before the update");
    assert_eq!(before["version"], env!("CARGO_PKG_VERSION"));
    // A second click while it downloads starts nothing.
    let twice = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(error_code(twice).await, (StatusCode::CONFLICT, "update_busy".into()));
}

#[tokio::test]
async fn an_install_shows_progress_then_waits_for_the_restart_never_while_live() {
    let r = start().await;
    let mut live = r.engine.subscribe_live();
    let early = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(error_code(early).await, (StatusCode::CONFLICT, "no_updater".into()));
    r.engine.attach_updater();
    let not_ready = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(error_code(not_ready).await, (StatusCode::CONFLICT, "not_ready".into()));

    r.engine.set_available_update(Some(found("9.9.9")));
    assert_eq!(r.post("/api/v1/updates/install", json!({})).await.status(), StatusCode::ACCEPTED);
    r.engine.update_progress("9.9.9", 512, Some(2048));
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(
        v["install"],
        json!({ "phase": "downloading", "version": "9.9.9", "downloaded": 512, "total": 2048, "error": null })
    );
    r.engine.update_installing("9.9.9");
    r.engine.update_ready("9.9.9");
    // A check after the install doesn't take the "restart" away.
    r.engine.set_available_update(None);
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!((v["install"]["phase"].as_str(), v["latest"].as_str()), (Some("ready"), Some("9.9.9")));
    let again = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(error_code(again).await, (StatusCode::CONFLICT, "restart_needed".into()));

    go_live(&r, true).await;
    let while_live = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(error_code(while_live).await, (StatusCode::CONFLICT, "live".into()));
    go_live(&r, false).await;
    let restart = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(restart.status(), StatusCode::ACCEPTED);
    assert_eq!(restart.json::<Value>().await.unwrap()["restarting"], "9.9.9");
    // The shell's wait wakes up.
    tokio::time::timeout(std::time::Duration::from_secs(1), r.engine.update_restart_requested()).await.unwrap();

    // Every change went out on the live socket as an `update` frame.
    let mut phases = Vec::new();
    while let Ok(f) = live.try_recv() {
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "update" {
            phases.push(v["data"]["install"]["phase"].as_str().unwrap().to_string());
        }
    }
    phases.dedup();
    assert_eq!(phases, ["idle", "downloading", "installing", "ready"]);
}

#[tokio::test]
async fn check_for_updates_asks_the_desktop_and_says_what_went_wrong() {
    let r = start().await;
    // The headless app has no updater: say where to download instead.
    let headless = r.post("/api/v1/updates/check", json!({})).await;
    let (status, body) = (headless.status(), headless.json::<Value>().await.unwrap());
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("no_updater")));
    assert!(body["error"]["hint"].as_str().unwrap().contains("ferrocast.clearcraftlabs.com"));

    r.engine.attach_updater();
    // A fake desktop shell: offline the first time, then it finds 9.9.9.
    let shell = r.engine.clone();
    tokio::spawn(async move {
        shell.update_check_requested().await;
        shell.update_check_started();
        shell.update_checked(Err(ferrocast_engine::UpdateProblem::new(
            "offline",
            "Ferrocast couldn't reach the update server.",
            "Check that this computer is online.",
        )));
        shell.update_check_requested().await;
        shell.update_checked(Ok(Some(found("9.9.9"))));
    });
    let first: Value = r.post("/api/v1/updates/check", json!({})).await.json().await.unwrap();
    assert_eq!(first["error"]["code"], "offline");
    assert_eq!((first["latest"].as_str(), first["checking"].as_bool()), (None, Some(false)));
    assert!(first["checked_at"].as_i64().is_some());
    let second: Value = r.post("/api/v1/updates/check", json!({})).await.json().await.unwrap();
    assert_eq!((second["latest"].as_str(), second["error"].is_null()), (Some("9.9.9"), true));
    // An offline check later keeps the release it found, and says why this one failed.
    r.engine.update_checked(Err(ferrocast_engine::UpdateProblem::new("offline", "x", "y")));
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!((v["latest"].as_str(), v["error"]["code"].as_str()), (Some("9.9.9"), Some("offline")));
}

#[tokio::test]
async fn a_failed_install_says_why_and_can_be_tried_again() {
    let r = start().await;
    r.engine.attach_updater();
    r.engine.set_available_update(Some(found("9.9.9")));
    assert_eq!(r.post("/api/v1/updates/install", json!({})).await.status(), StatusCode::ACCEPTED);
    r.engine.update_install_failed(ferrocast_engine::UpdateProblem::new(
        "bad_signature",
        "The download failed its signature check, so it was not installed.",
        "Try again later.",
    ));
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(
        (v["install"]["phase"].as_str(), v["install"]["error"]["code"].as_str()),
        (Some("failed"), Some("bad_signature"))
    );
    assert_eq!(r.post("/api/v1/updates/install", json!({})).await.status(), StatusCode::ACCEPTED, "try again");
}

#[tokio::test]
async fn a_daily_backup_is_taken_at_start_and_listed_with_its_reason() {
    let r = start().await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let daily = loop {
        let list: Value = r.get("/api/v1/backups").await.json().await.unwrap();
        if let Some(b) = list.as_array().unwrap().iter().find(|b| b["reason"] == "daily") {
            break b.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "no daily backup at start");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(daily["version"], env!("CARGO_PKG_VERSION"));
    assert!(daily["created_at"].as_i64().unwrap() > 0 && daily["bytes"].as_u64().unwrap() > 0);
    // A day hasn't passed: no second one.
    assert!(!r.engine.daily_backup_if_due().await.unwrap());
    let manual = r.post("/api/v1/backups", json!({})).await;
    assert_eq!(manual.status(), StatusCode::CREATED);
    let list: Value = manual.json().await.unwrap();
    assert_eq!(list[0]["reason"], "manual");
}

#[tokio::test]
async fn streamelements_import_preview_and_apply() {
    use ferrocast_testkit::streamelements as se;
    let r = start().await;
    let fake = se::fake().await;
    r.engine.set_streamelements_api(&fake.base);
    let path = "/api/v1/import/streamelements/preview";
    let anon = r.anonymous(reqwest::Method::POST, path).await;
    assert_eq!(error_code(anon).await, (StatusCode::UNAUTHORIZED, "unauthorized".into()));
    let shape = r.post(path, json!({ "token": "my-overlay-token" })).await;
    let (status, body) = (shape.status(), shape.json::<Value>().await.unwrap());
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].as_str().unwrap().starts_with("token:"));
    assert_eq!(error_code(r.post(path, json!({})).await).await.0, StatusCode::BAD_REQUEST);
    let wrong = r.post(path, json!({ "token": "eyJhbGciOiJIUzI1NiJ9.eyJ4IjoxfQ.d3Jvbmctc2lnbmF0dXJlLXRleHQ" })).await;
    let (status, body) = (wrong.status(), wrong.json::<Value>().await.unwrap());
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("streamelements_token_rejected"))
    );
    assert!(body["error"]["hint"].as_str().unwrap().contains("Account → Channels → Show secrets"));
    fake.set(|s| {
        s.fail.insert("/tips".into(), 500);
    });
    assert_eq!(
        error_code(r.post(path, json!({ "token": se::TOKEN })).await).await,
        (StatusCode::BAD_GATEWAY, "streamelements_unavailable".into())
    );
    fake.set(|s| {
        s.fail.clear();
        s.fail.insert("/bot/commands".into(), 429);
    });
    let limited = r.post(path, json!({ "token": se::TOKEN })).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers().get("retry-after").and_then(|v| v.to_str().ok()), Some("7"));
    fake.set(|s| s.fail.clear());

    let ok = r.post(path, json!({ "token": format!("Bearer {}", se::TOKEN) })).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let preview: Value = ok.json().await.unwrap();
    assert_eq!(preview["source"], "streamelements");
    assert_eq!(preview["account"]["platform"], "twitch");
    assert!(preview["sections"].as_array().unwrap().iter().any(|s| s["kind"] == "commands" && s["new"] == 3));
    assert!(!preview.to_string().contains(se::TOKEN), "the token never comes back");
    let id = preview["preview_id"].as_str().unwrap().to_string();
    let bad_choice = r.post("/api/v1/import/apply", json!({ "preview_id": id, "on_conflict": "merge" })).await;
    assert_eq!(error_code(bad_choice).await.0, StatusCode::BAD_REQUEST);
    let applied = r.post("/api/v1/import/apply", json!({ "preview_id": id, "on_conflict": "keep" })).await;
    assert_eq!(applied.status(), StatusCode::OK);
    let report: Value = applied.json().await.unwrap();
    assert_eq!(report["on_conflict"], "keep");
    assert!(report["backup"].as_str().unwrap().contains("before-streamelements-import"));
    let cmds: Value = r.get("/api/v1/commands").await.json().await.unwrap();
    assert_eq!(cmds.as_array().unwrap().len(), 3);
    let again = r.post("/api/v1/import/apply", json!({ "preview_id": id, "on_conflict": "keep" })).await;
    assert_eq!(error_code(again).await, (StatusCode::NOT_FOUND, "preview_expired".into()));
    assert!(fake.requests().iter().all(|(m, _)| m == "GET"));
}

#[tokio::test]
async fn streamlabs_import_files_are_checked() {
    let r = start().await;
    let send = |parts: Vec<(&'static str, Option<Vec<u8>>, &'static str)>| {
        let mut form = reqwest::multipart::Form::new();
        for (name, bytes, text) in parts {
            form = match bytes {
                Some(b) => form.part(name, reqwest::multipart::Part::bytes(b).file_name(text)),
                None => form.text(name, text),
            };
        }
        r.client
            .post(format!("{}/api/v1/import/streamlabs/preview", r.base))
            .bearer_auth(&r.token)
            .multipart(form)
            .send()
    };
    let csv = b"Name,Rank,Points,Hours\nsam,Viewer,1500,12\n".to_vec();
    let ok = send(vec![("platform", None, "twitch"), ("points", Some(csv.clone()), "Currency.csv")]).await.unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let v: Value = ok.json().await.unwrap();
    assert_eq!(v["source"], "streamlabs");
    assert_eq!(v["sections"][0]["waiting"], 1);
    let no_platform = send(vec![("points", Some(csv.clone()), "Currency.csv")]).await.unwrap();
    assert_eq!(error_code(no_platform).await.0, StatusCode::BAD_REQUEST);
    let bad_platform = send(vec![("platform", None, "tiktok"), ("points", Some(csv), "c.csv")]).await.unwrap();
    assert_eq!(error_code(bad_platform).await.0, StatusCode::BAD_REQUEST);
    let nothing = send(vec![("platform", None, "twitch")]).await.unwrap();
    assert_eq!(error_code(nothing).await.0, StatusCode::BAD_REQUEST);
    let png = send(vec![("platform", None, "twitch"), ("points", Some(b"\x89PNG\r\n\x1a\n\0\0".to_vec()), "a.png")])
        .await
        .unwrap();
    assert_eq!(error_code(png).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type".into()));
    let broken =
        send(vec![("platform", None, "twitch"), ("points", Some(b"Name,Points\nsam,many\n".to_vec()), "c.csv")])
            .await
            .unwrap();
    let (status, body) = (broken.status(), broken.json::<Value>().await.unwrap());
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"]["message"].as_str().unwrap().contains("row 2, column \"Points\""));
    let huge = vec![b'a'; 10 * 1024 * 1024 + 1];
    let big = send(vec![("platform", None, "twitch"), ("points", Some(huge), "c.csv")]).await.unwrap();
    assert_eq!(error_code(big).await, (StatusCode::PAYLOAD_TOO_LARGE, "too_large".into()));
}

#[tokio::test]
async fn ferrocast_voices_without_pro_answer_calmly() {
    let r = start().await;
    let v: Value = r.get("/api/v1/tts/cloud").await.json().await.unwrap();
    assert_eq!(
        (v["available"].as_bool(), v["voice"].is_null(), v["max_chars"].as_u64()),
        (Some(false), true, Some(300))
    );
    let voices: Value = r.get("/api/v1/tts/voices").await.json().await.unwrap();
    assert!(voices.as_array().unwrap().iter().all(|x| x["pro"] == false), "only this computer's voices");
    let res = r.put("/api/v1/tts/cloud", json!({ "voice": "google:hi-IN-Neural2-A" })).await;
    assert_eq!(res.status(), StatusCode::PAYMENT_REQUIRED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "pro_required");
    assert!(body["error"]["hint"].as_str().unwrap().contains("Settings › Pro"));
    let back: Value = r.put("/api/v1/tts/cloud", json!({ "voice": null })).await.json().await.unwrap();
    assert!(back["voice"].is_null(), "the system voice is always allowed");
    let raw = reqwest::Client::new()
        .post(format!("{}/api/v1/tts/preview", r.base))
        .bearer_auth(&r.token)
        .body("voice")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// ---------------------------------------------------------------- Go live everywhere

#[tokio::test]
async fn go_live_routes_refuse_what_is_wrong() {
    let r = start().await;
    r.engine.start_golive(0).await;
    let v: Value = r.get("/api/v1/golive").await.json().await.unwrap();
    assert!(v["ingest"].as_str().unwrap().starts_with("rtmp://127.0.0.1:"));
    assert_eq!(v["destinations"].as_array().unwrap().len(), 3);
    assert_eq!(v["destinations"][0]["id"], "twitch");
    assert_eq!(v["destinations"][0]["enabled"], false);
    assert_eq!(v["live"], false);
    // The order must list every destination once.
    let short =
        r.put("/api/v1/golive/destinations", json!({ "destinations": [{ "id": "twitch", "enabled": true }] })).await;
    assert_eq!(error_code(short).await.0, StatusCode::BAD_REQUEST);
    let twice = json!({ "destinations": [
        { "id": "twitch", "enabled": true }, { "id": "twitch", "enabled": true }, { "id": "kick", "enabled": true } ] });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", twice).await).await.0, StatusCode::BAD_REQUEST);
    let unknown = json!({ "destinations": [
        { "id": "twitch", "enabled": true }, { "id": "nope", "enabled": true }, { "id": "kick", "enabled": true } ] });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", unknown).await).await.0, StatusCode::BAD_REQUEST);
    let extra = json!({ "destinations": [], "secret": 1 });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", extra).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error_code(r.raw(reqwest::Method::PUT, "/api/v1/golive/destinations", "{nope").await).await.0,
        StatusCode::BAD_REQUEST
    );
    // A custom server must be rtmp:// or rtmps://, named, with a key.
    for body in [
        json!({ "name": "Backup", "url": "https://example.com/live", "key": "k" }),
        json!({ "name": "", "url": "rtmp://example.com/live", "key": "k" }),
        json!({ "name": "Backup", "url": "rtmp://example.com/live", "key": "has space" }),
        json!({ "name": "x".repeat(41), "url": "rtmp://example.com/live", "key": "k" }),
    ] {
        let (status, _) = error_code(r.post("/api/v1/golive/destinations", body.clone()).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    // Platforms can be switched off, not removed; unknown ids are 404.
    let del =
        |id: &str| r.client.delete(format!("{}/api/v1/golive/destinations/{id}", r.base)).bearer_auth(&r.token).send();
    assert_eq!(error_code(del("twitch").await.unwrap()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(del("c_nope").await.unwrap()).await.0, StatusCode::NOT_FOUND);
    // Nothing ticked: the upload check says so. No OBS: setup says how to fix it.
    assert_eq!(
        error_code(r.post("/api/v1/golive/upload-check", json!({})).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "golive_none".into())
    );
    assert_eq!(
        error_code(r.post("/api/v1/golive/obs/restore", json!({})).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "golive_nothing_saved".into())
    );
    assert_eq!(
        error_code(r.post("/api/v1/golive/obs/setup", json!({})).await).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Upload protection: on by default, a switch, and nothing to resume when nothing paused.
    assert_eq!(v["protect"], true);
    let off: Value = r.put("/api/v1/golive/protection", json!({ "on": false })).await.json().await.unwrap();
    assert_eq!(off["protect"], false);
    let v: Value = r.get("/api/v1/golive").await.json().await.unwrap();
    assert_eq!(v["protect"], false);
    assert_eq!(
        error_code(r.put("/api/v1/golive/protection", json!({ "on": "yes" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    // Aitum Multistream: "on purpose" is saved; no OBS means no plugin to report.
    assert_eq!(v["aitum_ok"], false);
    assert_eq!(v["obs"]["aitum"], Value::Null);
    let ok: Value = r.put("/api/v1/golive/aitum", json!({ "ok": true })).await.json().await.unwrap();
    assert_eq!(ok["aitum_ok"], true);
    assert_eq!(error_code(r.put("/api/v1/golive/aitum", json!({ "ok": 1 })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error_code(r.post("/api/v1/golive/resume/kick", json!({})).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "golive_not_paused".into())
    );
    // No session: 401 like every route.
    assert_eq!(r.anonymous(reqwest::Method::GET, "/api/v1/golive/ingest").await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_custom_destination_gets_the_stream_end_to_end() {
    use ferrocast_testkit::rtmp::{Got, Obs, next, platform};
    let r = start().await;
    r.engine.start_golive(0).await;
    let (port, mut got) = platform("dest-key").await;
    let res = r
        .post(
            "/api/v1/golive/destinations",
            json!({ "name": "Backup PC", "url": format!("rtmp://127.0.0.1:{port}/app"), "key": "dest-key" }),
        )
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let added: Value = res.json().await.unwrap();
    let id = added["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("c_"));
    assert!(added.get("key").is_none(), "the key never comes back");
    let v: Value = r.get("/api/v1/golive").await.json().await.unwrap();
    let row = v["destinations"].as_array().unwrap().iter().find(|d| d["id"] == id.as_str()).unwrap().clone();
    assert_eq!(
        (row["name"].as_str(), row["account"].as_str(), row["enabled"].as_bool()),
        (Some("Backup PC"), Some("127.0.0.1"), Some(true))
    );

    // OBS streams to the local ingest with the install's key: the destination gets it.
    let ing: Value = r.get("/api/v1/golive/ingest").await.json().await.unwrap();
    let ingest_port: u16 =
        ing["server"].as_str().unwrap().rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    assert!(ing["key"].as_str().unwrap().starts_with("fc_"));
    let mut obs = Obs::publish(ingest_port, "live", ing["key"].as_str().unwrap()).await.unwrap();
    assert_eq!(next(&mut got).await, Got::Publish("dest-key".into()));
    assert!(matches!(obs.keyframes_until_seen(&mut got).await, Got::Video(0, _)));
    let v: Value = r.get("/api/v1/golive").await.json().await.unwrap();
    assert_eq!(v["live"], true);
    let row = v["destinations"].as_array().unwrap().iter().find(|d| d["id"] == id.as_str()).unwrap().clone();
    assert_eq!(row["health"]["state"], "live", "{row}");

    // Unticked while live: it stops at once, the stream goes on.
    let order: Vec<Value> = v["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| json!({ "id": d["id"], "enabled": d["id"] != id.as_str() && d["enabled"].as_bool().unwrap() }))
        .collect();
    assert_eq!(r.put("/api/v1/golive/destinations", json!({ "destinations": order })).await.status(), StatusCode::OK);
    assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(5), got.recv()).await.unwrap(), Some(Got::Closed));
    // Removed: gone from the list, its key deleted.
    let gone = r
        .client
        .delete(format!("{}/api/v1/golive/destinations/{id}", r.base))
        .bearer_auth(&r.token)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NO_CONTENT);
    let v: Value = r.get("/api/v1/golive").await.json().await.unwrap();
    assert_eq!(v["destinations"].as_array().unwrap().len(), 3);
}
