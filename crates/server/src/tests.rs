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

/// The dashboard's ten looks are a setting; `dark` and `light` stay as aliases of Slate and Daylight.
#[tokio::test]
async fn settings_take_each_of_the_ten_themes_and_still_the_old_three() {
    let r = start().await;
    for theme in [
        "system",
        "dark",
        "light",
        "graphite",
        "slate",
        "moonlight",
        "forest",
        "ember",
        "obsidian",
        "daylight",
        "mist",
        "linen",
        "contrast",
    ] {
        let res = r.put("/api/v1/settings", json!({ "theme": theme })).await;
        assert_eq!(res.status(), StatusCode::OK, "{theme}");
        let s: Value = r.get("/api/v1/settings").await.json().await.unwrap();
        assert_eq!(s["theme"], theme);
    }
    let bad = r.put("/api/v1/settings", json!({ "theme": "Slate" })).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "ids are lower case");
    let v: Value = bad.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().starts_with("theme"), "{v}");
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
    assert!(r.post("/api/v1/polls/current/end", json!({})).await.status().is_success());

    // Omitted `duration_s` defaults to 60s; explicit JSON `null` creates a No-timer poll.
    let defaulted = r.post("/api/v1/polls", json!({ "question": "Default?", "options": ["A", "B"] })).await;
    assert_eq!(defaulted.status(), StatusCode::CREATED);
    let dv: Value = defaulted.json().await.unwrap();
    assert!(dv["ends_at"].is_i64(), "omitted duration_s defaults to timed: {dv}");
    assert!(r.post("/api/v1/polls/current/end", json!({})).await.status().is_success());

    let bad_fb =
        r.post("/api/v1/polls", json!({ "question": "Bad?", "options": ["A", "B"], "chat_fallback": "yes" })).await;
    assert_eq!(bad_fb.status(), StatusCode::BAD_REQUEST);

    let untimed = r
        .post("/api/v1/polls", json!({ "question": "No timer?", "options": ["Stay", "Go"], "duration_s": null }))
        .await;
    assert_eq!(untimed.status(), StatusCode::CREATED);
    let uv: Value = untimed.json().await.unwrap();
    assert_eq!(uv["ends_at"], Value::Null, "explicit null creates an untimed poll");
    assert_eq!(r.post("/api/v1/polls/current/end", json!({})).await.status(), StatusCode::OK);
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
async fn a_store_install_points_to_the_store_for_updates() {
    let r = start().await;
    r.engine.updates_from_store();
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!((v["store"].as_bool(), v["can_install"].as_bool()), (Some(true), Some(false)));
    // Checks and restarts say where updates come from, not "download it yourself".
    let check = r.post("/api/v1/updates/check", json!({})).await;
    assert_eq!(error_code(check).await, (StatusCode::CONFLICT, "store_updates".into()));
    let restart = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(error_code(restart).await, (StatusCode::CONFLICT, "store_updates".into()));
    r.engine.set_available_update(Some(found("9.9.9")));
    let install = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(error_code(install).await, (StatusCode::CONFLICT, "store_updates".into()));
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
async fn updates_refuse_install_and_restart_while_studio_recording_or_replay_active() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ferrocast_engine::studio_host::{
        LocalCaptureBlock, PreviewRect, StudioAct, StudioHost, StudioSettings, StudioStatus,
    };

    #[derive(Debug, Default)]
    struct CaptureStateHost {
        recording: AtomicBool,
        replay: AtomicBool,
        hold_replay: tokio::sync::Mutex<()>,
        replay_entered: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl StudioHost for CaptureStateHost {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn options(&self, _kind: String) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn preview(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn act(&self, _act: StudioAct) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn settings(&self) -> StudioSettings {
            StudioSettings::default()
        }
        async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn record(&self, on: bool) -> Result<Value, String> {
            self.recording.store(on, Ordering::Release);
            Ok(Value::Null)
        }
        async fn record_source(&self, _name: String, on: bool) -> Result<Value, String> {
            self.recording.store(on, Ordering::Release);
            Ok(Value::Null)
        }
        async fn record_pause(&self, _on: bool) -> Result<(), String> {
            Ok(())
        }
        async fn replay(&self, on: bool) -> Result<Value, String> {
            if on {
                self.replay_entered.notify_one();
                let _hold = self.hold_replay.lock().await;
            }
            self.replay.store(on, Ordering::Release);
            Ok(Value::Null)
        }
        async fn monitoring(&self) -> Result<Value, String> {
            Ok(json!({ "devices": [] }))
        }
        async fn stream_start(&self, _s: String, _k: String, _t: Vec<(u32, u32, u32)>) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn import_obs(&self, _id: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        fn local_capture_block(&self) -> LocalCaptureBlock {
            LocalCaptureBlock {
                recording: self.recording.load(Ordering::Acquire),
                replay: self.replay.load(Ordering::Acquire),
            }
        }
    }

    let r = start().await;
    let host = Arc::new(CaptureStateHost::default());
    r.engine.set_studio_host(host.clone());
    r.engine.attach_updater();
    r.engine.set_available_update(Some(found("9.9.9")));

    // Active local recording blocks both install and update_view without conflating Engine::is_live().
    host.recording.store(true, Ordering::Release);
    assert!(!r.engine.is_live(), "local recording must not report stream live status");
    assert!(r.engine.update_blocked(), "local recording must block updates");
    let v: Value = r.get("/api/v1/updates").await.json().await.unwrap();
    assert_eq!(v["blocked_while_live"], json!(true));
    let rec_install = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(rec_install.status(), StatusCode::CONFLICT);
    let rec_body: Value = rec_install.json().await.unwrap();
    assert_eq!(rec_body["error"]["code"], "live");
    assert!(
        rec_body["error"]["message"].as_str().unwrap_or_default().to_ascii_lowercase().contains("record"),
        "expected recording message, got {rec_body}"
    );

    // Active replay buffer also blocks both install and update_view.
    host.recording.store(false, Ordering::Release);
    host.replay.store(true, Ordering::Release);
    assert!(r.engine.update_blocked(), "active replay buffer must block updates");
    let rep_install = r.post("/api/v1/updates/install", json!({})).await;
    assert_eq!(rep_install.status(), StatusCode::CONFLICT);
    let rep_body: Value = rep_install.json().await.unwrap();
    assert_eq!(rep_body["error"]["code"], "live");
    assert!(
        rep_body["error"]["message"].as_str().unwrap_or_default().to_ascii_lowercase().contains("replay"),
        "expected replay message, got {rep_body}"
    );

    // Stop replay so download succeeds and becomes Ready, then confirm restart refuses for recording and replay.
    host.replay.store(false, Ordering::Release);
    assert_eq!(r.post("/api/v1/updates/install", json!({})).await.status(), StatusCode::ACCEPTED);
    r.engine.update_ready("9.9.9");

    host.recording.store(true, Ordering::Release);
    let rec_restart = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(rec_restart.status(), StatusCode::CONFLICT);
    let rec_restart_body: Value = rec_restart.json().await.unwrap();
    assert_eq!(rec_restart_body["error"]["code"], "live");
    assert!(
        rec_restart_body["error"]["message"].as_str().unwrap_or_default().to_ascii_lowercase().contains("record"),
        "expected recording restart refusal, got {rec_restart_body}"
    );

    host.recording.store(false, Ordering::Release);
    host.replay.store(true, Ordering::Release);
    let rep_restart = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(rep_restart.status(), StatusCode::CONFLICT);
    let rep_restart_body: Value = rep_restart.json().await.unwrap();
    assert_eq!(rep_restart_body["error"]["code"], "live");
    assert!(
        rep_restart_body["error"]["message"].as_str().unwrap_or_default().to_ascii_lowercase().contains("replay"),
        "expected replay restart refusal, got {rep_restart_body}"
    );

    host.replay.store(false, Ordering::Release);
    // An in-flight replay/recording start holding `output_gate` must block restart admission too.
    let hold_replay = host.hold_replay.lock().await;
    let entered = host.replay_entered.notified();
    let starting_rep = tokio::spawn({
        let eng = r.engine.clone();
        async move { eng.studio_replay(true).await }
    });
    tokio::pin!(starting_rep);
    tokio::select! {
        () = entered => {},
        res = &mut starting_rep => panic!("studio_replay returned before entering host: {res:?}"),
    }
    let starting_restart = r.post("/api/v1/updates/restart", json!({})).await;
    assert_eq!(starting_restart.status(), StatusCode::CONFLICT);
    assert_eq!(starting_restart.json::<Value>().await.unwrap()["error"]["code"], "live");
    drop(hold_replay);
    starting_rep.await.unwrap().unwrap();
    r.engine.studio_replay(false).await.unwrap();

    assert_eq!(r.post("/api/v1/updates/restart", json!({})).await.status(), StatusCode::ACCEPTED);
    // Once restart is admitted under `output_gate`, late recording/replay starts are refused until failure/reset.
    let late_rec = r.engine.studio_record(true).await.unwrap_err();
    assert!(
        matches!(late_rec, ferrocast_engine::EngineError::Refused { code: "updating", .. }),
        "expected updating refusal after restart admission, got {late_rec:?}"
    );
    let late_rep = r.engine.studio_replay(true).await.unwrap_err();
    assert!(
        matches!(late_rep, ferrocast_engine::EngineError::Refused { code: "updating", .. }),
        "expected updating refusal on replay after restart admission, got {late_rep:?}"
    );
    let late_src_rec = r.engine.studio_record_source("Cam", true).await.unwrap_err();
    assert!(
        matches!(late_src_rec, ferrocast_engine::EngineError::Refused { code: "updating", .. }),
        "expected updating refusal on isolated record after restart admission, got {late_src_rec:?}"
    );

    r.engine.update_failed("restart aborted");
    assert_eq!(r.post("/api/v1/updates/restart", json!({})).await.status(), StatusCode::CONFLICT);
    r.engine.studio_replay(true).await.expect("failure resets update_committing lock");
    r.engine.studio_replay(false).await.unwrap();
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
    assert_eq!(v["destinations"].as_array().unwrap().len(), 4);
    assert_eq!(v["destinations"][0]["id"], "twitch");
    assert_eq!(
        (v["destinations"][3]["id"].as_str(), v["destinations"][3]["canvas"].as_str()),
        (Some("youtube_vertical"), Some("vertical"))
    );
    assert_eq!(v["destinations"][3]["name"], "YouTube Shorts live");
    assert_eq!(v["vertical_live"], false);
    assert_eq!(v["destinations"][0]["enabled"], false);
    assert_eq!(v["live"], false);
    // The lighter stream: on a main platform, never the vertical one or an unknown one.
    let v: Value = r.put("/api/v1/golive/destinations/kick/light", json!({ "on": true })).await.json().await.unwrap();
    let kick = v["destinations"].as_array().unwrap().iter().find(|d| d["id"] == "kick").cloned().unwrap();
    assert_eq!(kick["light"], true);
    let vertical = r.put("/api/v1/golive/destinations/youtube_vertical/light", json!({ "on": true })).await;
    assert_eq!(error_code(vertical).await.0, StatusCode::BAD_REQUEST);
    let nobody = r.put("/api/v1/golive/destinations/nope/light", json!({ "on": true })).await;
    assert_eq!(error_code(nobody).await.0, StatusCode::NOT_FOUND);
    let v: Value = r.put("/api/v1/golive/destinations/kick/light", json!({ "on": false })).await.json().await.unwrap();
    assert!(v["destinations"].as_array().unwrap().iter().all(|d| d.get("light").is_none()), "off isn't saved");
    // The order must list every destination once.
    let short =
        r.put("/api/v1/golive/destinations", json!({ "destinations": [{ "id": "twitch", "enabled": true }] })).await;
    assert_eq!(error_code(short).await.0, StatusCode::BAD_REQUEST);
    let twice = json!({ "destinations": [
        { "id": "twitch", "enabled": true }, { "id": "twitch", "enabled": true }, { "id": "kick", "enabled": true },
        { "id": "youtube_vertical", "enabled": false } ] });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", twice).await).await.0, StatusCode::BAD_REQUEST);
    let unknown = json!({ "destinations": [
        { "id": "twitch", "enabled": true }, { "id": "nope", "enabled": true }, { "id": "kick", "enabled": true },
        { "id": "youtube_vertical", "enabled": false } ] });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", unknown).await).await.0, StatusCode::BAD_REQUEST);
    let extra = json!({ "destinations": [], "secret": 1 });
    assert_eq!(error_code(r.put("/api/v1/golive/destinations", extra).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error_code(r.raw(reqwest::Method::PUT, "/api/v1/golive/destinations", "{nope").await).await.0,
        StatusCode::BAD_REQUEST
    );
    // A custom server is rtmp://, rtmps:// (with a key), or srt://, rist:// or WebRTC over
    // https:// (plain http only on this PC, never with a user name); always named.
    for body in [
        json!({ "name": "Backup", "url": "ftp://example.com/live", "key": "k" }),
        json!({ "name": "Backup", "url": "http://example.com/whip", "key": "k" }),
        json!({ "name": "Backup", "url": "https://me:pw@example.com/whip", "key": "" }),
        json!({ "name": "Backup", "url": "rtmp://example.com/live", "key": "" }),
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
    // No OBS: setup says how to fix it.
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
    // Advanced settings: checked by field; saved and shown.
    let mut adv = v["advanced"].clone();
    adv["rules"]["calm_s"] = json!(500);
    let (status, _) = error_code(r.put("/api/v1/golive/advanced", adv.clone()).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    adv["rules"]["calm_s"] = json!(20);
    adv["keep_free_pct"] = json!(25);
    let saved: Value = r.put("/api/v1/golive/advanced", adv).await.json().await.unwrap();
    assert_eq!(saved["advanced"]["keep_free_pct"], 25);
    // Aitum Multistream: "on purpose" is saved; no OBS means no plugin to report.
    assert_eq!(v["aitum_ok"], false);
    assert_eq!(v["obs"]["aitum"], Value::Null);
    let ok: Value = r.put("/api/v1/golive/aitum", json!({ "ok": true })).await.json().await.unwrap();
    assert_eq!(ok["aitum_ok"], true);
    assert_eq!(error_code(r.put("/api/v1/golive/aitum", json!({ "ok": 1 })).await).await.0, StatusCode::BAD_REQUEST);
    // Auto quality: on unless turned off.
    assert_eq!(v["auto_quality"], true);
    let off: Value = r.put("/api/v1/golive/auto-quality", json!({ "on": false })).await.json().await.unwrap();
    assert_eq!(off["auto_quality"], false);
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
    // It was the only one ticked, so taking it away asks first and changes nothing...
    let ask = r.put("/api/v1/golive/destinations", json!({ "destinations": order.clone() })).await;
    assert_eq!(ask.status(), StatusCode::CONFLICT);
    assert_eq!(ask.json::<Value>().await.unwrap()["error"]["code"], "golive_last_destination");
    // ...and confirmed, it stops at once while the stream goes on.
    assert_eq!(
        r.put("/api/v1/golive/destinations", json!({ "destinations": order, "confirm_last": true })).await.status(),
        StatusCode::OK
    );
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
    assert_eq!(v["destinations"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn theme_in_obs_routes_answer_in_our_shape() {
    let r = start().await;
    let none = r.get("/api/v1/obs/theme").await;
    assert_eq!(none.status(), StatusCode::OK);
    assert_eq!(none.json::<Value>().await.unwrap(), Value::Null);
    let undo = r.client.delete(format!("{}/api/v1/obs/theme", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(error_code(undo).await, (StatusCode::NOT_FOUND, "no_theme_in_obs".into()));
    let bad = r
        .client
        .post(format!("{}/api/v1/designs/whatever/obs", r.base))
        .bearer_auth(&r.token)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(bad).await.0, StatusCode::BAD_REQUEST);
    let unknown = r.post("/api/v1/designs/whatever/obs", json!({ "prefix": "x", "screens": [], "extra": 1 })).await;
    assert_eq!(error_code(unknown).await.0, StatusCode::BAD_REQUEST, "unknown fields are refused");
    assert_eq!(r.get("/api/v1/designs/whatever/obs").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn chat_shows_this_stream_not_the_last_one() {
    use ferrocast_core::event::{Actor, Content, Event, EventKind, Platform};
    let r = start().await;
    let now = ferrocast_engine::now().0;
    let say = |id: &str, at: i64| {
        let mut a = Actor::named(Platform::Twitch, "viewer");
        a.id = Some("viewer-id".into());
        Event::live(
            Platform::Twitch,
            Some(id.into()),
            ferrocast_core::event::Millis(at),
            EventKind::ChatMessage { actor: a, content: Content::plain(id), reply_to: None, is_action: false },
        )
    };
    r.engine.ingest(say("two days ago", now - 2 * 86_400_000)).await;
    r.engine.ingest(say("just now", now - 60_000)).await;
    let chat: Value = r.get("/api/v1/chat").await.json().await.unwrap();
    let ids: Vec<&str> = chat.as_array().unwrap().iter().filter_map(|e| e["source_id"].as_str()).collect();
    assert_eq!(ids, ["just now"]);
}

#[tokio::test]
async fn diagnostics_routes_export_redacted_zip_and_enforce_validation() {
    use std::io::Read;

    let r = start().await;
    let folder = tempfile::tempdir().unwrap();
    let logs = r._dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("ferrocast.log"),
        "2026-11-22T03:30:00Z ERROR user=route@test.example ip=198.51.100.77 token=routeSecret999\n",
    )
    .unwrap();

    // Unauthenticated denied
    assert_eq!(
        r.anonymous(reqwest::Method::GET, "/api/v1/diagnostics/preview").await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Preview returns structured JSON with likely_causes right on it
    let preview = r.get("/api/v1/diagnostics/preview").await;
    assert_eq!(preview.status(), StatusCode::OK);
    let pv: Value = preview.json().await.unwrap();
    assert!(pv["likely_causes"].is_array());
    assert!(pv["app"]["version"].is_string());
    let pv_text = pv.to_string();
    assert!(
        !pv_text.contains("route@test.example")
            && !pv_text.contains("198.51.100.77")
            && !pv_text.contains("routeSecret999")
    );

    // Missing / invalid folder returns 400
    let bad_dir = r.post("/api/v1/diagnostics/report", json!({ "folder": "/no/such/ferrocast-folder-xyz" })).await;
    assert_eq!(error_code(bad_dir).await.0, StatusCode::BAD_REQUEST);

    // Valid folder writes real zip archive
    let created = r.post("/api/v1/diagnostics/report", json!({ "folder": folder.path().display().to_string() })).await;
    assert_eq!(created.status(), StatusCode::OK);
    let body: Value = created.json().await.unwrap();
    assert_eq!(body["cancelled"], false);
    let out_file = std::path::PathBuf::from(body["path"].as_str().unwrap());
    assert!(out_file.is_file());
    let mut arc = zip::ZipArchive::new(std::fs::File::open(&out_file).unwrap()).unwrap();
    let mut summary = String::new();
    arc.by_name("SUMMARY.txt").unwrap().read_to_string(&mut summary).unwrap();
    assert!(summary.contains("=== LIKELY CAUSES ==="));

    // Binary .zip endpoint also works for browsers
    let zip_res = r.post("/api/v1/diagnostics/report.zip", json!({})).await;
    assert_eq!(zip_res.status(), StatusCode::OK);
    assert_eq!(zip_res.headers().get("content-type").and_then(|v| v.to_str().ok()), Some("application/zip"));
    let bytes = zip_res.bytes().await.unwrap();
    let arc2 = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    assert!(arc2.len() >= 2);
}

#[tokio::test]
async fn diagnostics_routes_reject_non_loopback_peers_and_expose_content_disposition() {
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let out_folder = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let sessions = std::sync::Arc::new(crate::auth::Sessions::new());
    let paired_token = sessions.pair(&sessions.new_code()).unwrap();
    let state = crate::AppState {
        engine,
        sessions,
        guard: crate::auth::Guard { port: 47820, lan: true, dev_origins: vec!["http://localhost:5173".into()] },
        api_limiter: crate::rate_limit::ApiKeyRateLimiter::default(),
    };
    let app = crate::router(state);
    let lan_peer: SocketAddr = "192.168.1.50:54321".parse().unwrap();

    // Paired LAN client (non-loopback) must get 403 on all diagnostics routes.
    for (method, path, body) in [
        ("GET", "/api/v1/diagnostics/preview", String::new()),
        ("POST", "/api/v1/diagnostics/report", serde_json::json!({ "folder": out_folder.path() }).to_string()),
        ("POST", "/api/v1/diagnostics/report.zip", "{}".into()),
    ] {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "192.168.1.50:47820")
            .header("authorization", format!("Bearer {paired_token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.clone()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(lan_peer));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "LAN peer unexpectedly allowed on {method} {path}");
    }

    // Missing ConnectInfo (e.g. non-TCP/synthetic caller) must also fail closed with 403.
    let no_ip_req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/diagnostics/preview")
        .header("host", "127.0.0.1:47820")
        .header("authorization", format!("Bearer {paired_token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(no_ip_req).await.unwrap().status(), StatusCode::FORBIDDEN);

    // Loopback peer gets 200 and dev-origin CORS exposes Content-Disposition.
    let loopback_peer: SocketAddr = "127.0.0.1:54322".parse().unwrap();
    let mut local_req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/diagnostics/report.zip")
        .header("host", "127.0.0.1:47820")
        .header("origin", "http://localhost:5173")
        .header("authorization", format!("Bearer {paired_token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    local_req.extensions_mut().insert(ConnectInfo(loopback_peer));
    let ok_res = app.oneshot(local_req).await.unwrap();
    assert_eq!(ok_res.status(), StatusCode::OK);
    let exposed = ok_res
        .headers()
        .get(axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        exposed.to_ascii_lowercase().contains("content-disposition"),
        "exposed headers missing Content-Disposition: {exposed:?}"
    );
}

#[tokio::test]
async fn overlays_validate_new_label_options_and_reject_bad_train_settings() {
    let r = start().await;
    let created: Value =
        r.post("/api/v1/overlays", json!({ "name": "Labels", "preset": "blank" })).await.json().await.unwrap();
    let id = created["id"].as_str().unwrap();

    let mut bad = created.clone();
    bad["widgets"] = json!([{
        "id": "w1",
        "kind": "label",
        "x": 10, "y": 10, "w": 300, "h": 50,
        "config": { "source": "hype_train", "template": "{value}", "theme": "molten", "train_window_s": 2 }
    }]);
    let res_bad = r.put(&format!("/api/v1/overlays/{id}"), bad).await;
    assert_eq!(error_code(res_bad).await.0, StatusCode::BAD_REQUEST);

    let mut ok_ov = created.clone();
    ok_ov["widgets"] = json!([{
        "id": "w1",
        "kind": "label",
        "x": 10, "y": 10, "w": 300, "h": 50,
        "config": { "source": "monthly_tips", "template": "Month: {value}", "theme": "molten", "timezone": "Europe/London", "timezone_offset_min": 0 }
    }]);
    let res_ok = r.put(&format!("/api/v1/overlays/{id}"), ok_ov).await;
    assert_eq!(res_ok.status(), StatusCode::OK);

    let mut bad_tz = created.clone();
    bad_tz["widgets"] = json!([{
        "id": "w1",
        "kind": "label",
        "x": 10, "y": 10, "w": 300, "h": 50,
        "config": { "source": "monthly_tips", "template": "Month: {value}", "theme": "molten", "timezone": "Mars/Olympus" }
    }]);
    let res_bad_tz = r.put(&format!("/api/v1/overlays/{id}"), bad_tz).await;
    assert_eq!(error_code(res_bad_tz).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn preview_socket_hides_real_credits_and_the_pinned_message() {
    use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
    let r = start().await;
    let o: Value = r.post("/api/v1/overlays", json!({ "name": "Priv", "preset": "blank" })).await.json().await.unwrap();
    let mut edit = o.clone();
    edit["widgets"] = json!([
        { "id": "ec", "kind": "end_credits", "x": 0, "y": 0, "w": 800, "h": 600, "config": {} },
        { "id": "hl", "kind": "chat_highlight", "x": 0, "y": 700, "w": 800, "h": 200, "config": {} }
    ]);
    let url = o["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    assert_eq!(r.put(&format!("/api/v1/overlays/{}", o["id"].as_str().unwrap()), edit).await.status(), StatusCode::OK);
    let t0 = ferrocast_engine::now().0;
    r.engine
        .ingest(Event::live(
            Platform::Twitch,
            Some("pv-f".into()),
            Millis(t0 - 2000),
            EventKind::Follow { actor: Actor::named(Platform::Twitch, "RealFollower") },
        ))
        .await;
    r.engine
        .ingest(Event::live(
            Platform::Twitch,
            Some("pv-m".into()),
            Millis(t0 - 1000),
            EventKind::ChatMessage {
                actor: Actor::named(Platform::Twitch, "RealChatter"),
                content: Content::plain("secret pinned words"),
                reply_to: None,
                is_action: false,
            },
        ))
        .await;
    let chat: Value = r.get("/api/v1/chat?limit=5").await.json().await.unwrap();
    let id = chat.as_array().unwrap()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(r.post("/api/v1/chat/highlight", json!({ "event_id": id })).await.status(), StatusCode::OK);

    let (mut real, _) = tokio_tungstenite::connect_async(url.clone()).await.unwrap();
    let real_snap = next_json(&mut real).await;
    assert!(real_snap.to_string().contains("RealFollower"), "the real overlay sees the roster: {real_snap}");
    let (mut test, _) = tokio_tungstenite::connect_async(format!("{url}?test")).await.unwrap();
    let snap = next_json(&mut test).await;
    let text = snap.to_string();
    for secret in ["RealFollower", "RealChatter", "secret pinned words"] {
        assert!(!text.contains(secret), "a ?test preview must not carry {secret}: {text}");
    }
}

#[tokio::test]
async fn new_widget_configs_validate_and_isolate_overlay_socket_feeds() {
    use ferrocast_core::event::{Actor, Content, Event, EventKind, Platform};
    use reqwest::Method;
    let r = start().await;

    // 1. Auth required on /credits and /chat/highlight.
    for (m, p) in [
        (Method::GET, "/api/v1/credits"),
        (Method::GET, "/api/v1/chat/highlight"),
        (Method::POST, "/api/v1/chat/highlight"),
        (Method::DELETE, "/api/v1/chat/highlight"),
    ] {
        assert_eq!(error_code(r.anonymous(m, p).await).await.0, StatusCode::UNAUTHORIZED, "{p}");
    }
    assert_eq!(r.get("/api/v1/chat/highlight").await.json::<Value>().await.unwrap(), Value::Null);

    // 2. Overlay widget validation rejects invalid speeds, physics, sections, and caps.
    let base_overlay: Value =
        r.post("/api/v1/overlays", json!({ "name": "TestNew", "preset": "blank" })).await.json().await.unwrap();
    let oid = base_overlay["id"].as_str().unwrap().to_string();
    let mut bad_ew = base_overlay.clone();
    bad_ew["widgets"] = json!([
        { "id": "ew", "kind": "emote_wall", "x": 0, "y": 0, "w": 800, "h": 600, "config": { "max_emotes": 500, "physics": "bounce" } }
    ]);
    assert_eq!(r.put(&format!("/api/v1/overlays/{oid}"), bad_ew).await.status(), StatusCode::BAD_REQUEST);

    let mut bad_ec = base_overlay.clone();
    bad_ec["widgets"] = json!([
        { "id": "ec", "kind": "end_credits", "x": 0, "y": 0, "w": 800, "h": 600, "config": { "sections": ["followers"], "speed": 999 } }
    ]);
    assert_eq!(r.put(&format!("/api/v1/overlays/{oid}"), bad_ec).await.status(), StatusCode::BAD_REQUEST);

    // 3. Create overlays tailored to each new widget + a baseline chat_only overlay.
    let hl_ov: Value =
        r.post("/api/v1/overlays", json!({ "name": "HL", "preset": "blank" })).await.json().await.unwrap();
    let mut hl_edit = hl_ov.clone();
    hl_edit["widgets"] = json!([
        { "id": "hl", "kind": "chat_highlight", "x": 40, "y": 800, "w": 620, "h": 220, "config": { "theme": "molten", "duration_s": null, "font_size": 24, "show_badges": true, "show_platform": true } }
    ]);
    assert_eq!(
        r.put(&format!("/api/v1/overlays/{}", hl_ov["id"].as_str().unwrap()), hl_edit).await.status(),
        StatusCode::OK
    );

    let ec_ov: Value =
        r.post("/api/v1/overlays", json!({ "name": "EC", "preset": "blank" })).await.json().await.unwrap();
    let mut ec_edit = ec_ov.clone();
    ec_edit["widgets"] = json!([
        { "id": "ec", "kind": "end_credits", "x": 100, "y": 100, "w": 800, "h": 700, "config": { "theme": "molten", "sections": ["followers", "mods"], "speed": 60, "font": "system", "loop": true } }
    ]);
    assert_eq!(
        r.put(&format!("/api/v1/overlays/{}", ec_ov["id"].as_str().unwrap()), ec_edit).await.status(),
        StatusCode::OK
    );

    let ew_ov: Value =
        r.post("/api/v1/overlays", json!({ "name": "EW", "preset": "blank" })).await.json().await.unwrap();
    let mut ew_edit = ew_ov.clone();
    ew_edit["widgets"] = json!([
        { "id": "ew", "kind": "emote_wall", "x": 0, "y": 0, "w": 1920, "h": 1080, "config": { "theme": "molten", "platforms": [], "max_emotes": 40, "size": 56, "lifetime_s": 6, "physics": "rain" } }
    ]);
    assert_eq!(
        r.put(&format!("/api/v1/overlays/{}", ew_ov["id"].as_str().unwrap()), ew_edit).await.status(),
        StatusCode::OK
    );

    let chat_only: Value =
        r.post("/api/v1/overlays", json!({ "name": "Plain", "preset": "chat_only" })).await.json().await.unwrap();

    // Seed an earlier chat message BEFORE opening emote_wall socket (must NOT backfill emote wall snapshot).
    let t0 = ferrocast_engine::now().0;
    let mut old_msg = Event::live(
        Platform::Twitch,
        Some("m-old".into()),
        ferrocast_core::event::Millis(t0 - 1000),
        EventKind::ChatMessage {
            actor: Actor::named(Platform::Twitch, "OldChatter"),
            content: Content::plain("hello earlier"),
            reply_to: None,
            is_action: false,
        },
    );
    old_msg.id = "ev-old".into();
    r.engine.ingest(old_msg).await;
    let chat_log: Value = r.get("/api/v1/chat?limit=10").await.json().await.unwrap();
    let old_id = chat_log.as_array().unwrap()[0]["id"].as_str().unwrap().to_string();

    let open = |o: &Value| o["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut ws_hl, _) = tokio_tungstenite::connect_async(open(&hl_ov)).await.unwrap();
    let (mut ws_ec, _) = tokio_tungstenite::connect_async(open(&ec_ov)).await.unwrap();
    let (mut ws_ew, _) = tokio_tungstenite::connect_async(open(&ew_ov)).await.unwrap();
    let (mut ws_chat, _) = tokio_tungstenite::connect_async(open(&chat_only)).await.unwrap();

    let snap_hl: Value = serde_json::from_str(ws_hl.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let snap_ec: Value = serde_json::from_str(ws_ec.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let snap_ew: Value = serde_json::from_str(ws_ew.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    let snap_chat: Value = serde_json::from_str(ws_chat.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();

    assert!(snap_hl["data"]["state"]["chat_highlight"].is_null());
    assert!(snap_ec["data"]["state"]["credits"].is_object());
    assert!(snap_chat["data"]["state"]["credits"].is_null());
    assert_eq!(snap_ew["data"]["state"]["chat"].as_array().unwrap().len(), 0, "emote_wall needs no chat backfill");

    // 4. Pin highlight validation + live state push strictly to ChatHighlight overlay.
    let bad_pin = r.post("/api/v1/chat/highlight", json!({ "event_id": 123 })).await;
    assert_eq!(bad_pin.status(), StatusCode::BAD_REQUEST);
    let bad_dur = r.post("/api/v1/chat/highlight", json!({ "event_id": old_id, "duration_s": 2 })).await;
    assert_eq!(bad_dur.status(), StatusCode::BAD_REQUEST);
    let missing_pin = r.post("/api/v1/chat/highlight", json!({ "event_id": "nope" })).await;
    assert_eq!(error_code(missing_pin).await, (StatusCode::NOT_FOUND, "highlight_not_found".into()));

    let pin_res = r.post("/api/v1/chat/highlight", json!({ "event_id": old_id, "duration_s": 45 })).await;
    assert_eq!(pin_res.status(), StatusCode::OK);
    let pinned: Value = pin_res.json().await.unwrap();
    assert_eq!(pinned["event_id"], old_id);

    let hl_frame: Value = serde_json::from_str(ws_hl.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hl_frame["type"], "state");
    assert_eq!(hl_frame["data"]["chat_highlight"]["content"]["text"], "hello earlier");

    // Emote wall + EndCredits + ChatOnly should NOT receive the chat_highlight frame!
    assert!(tokio::time::timeout(std::time::Duration::from_millis(120), ws_ew.next()).await.is_err());

    // 5. Dismiss highlight -> sends null to ChatHighlight overlay.
    let del = r.client.delete(format!("{}/api/v1/chat/highlight", r.base)).bearer_auth(&r.token).send().await.unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let clear_frame: Value = serde_json::from_str(ws_hl.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(clear_frame["type"], "state");
    assert!(clear_frame["data"]["chat_highlight"].is_null());

    // 6. Live follow + live emote chat -> EndCredits gets credits frame, EmoteWall gets live chat event.
    let follow_ev = Event::live(
        Platform::Twitch,
        Some("f-live".into()),
        ferrocast_core::event::Millis(t0 + 10),
        EventKind::Follow { actor: Actor::named(Platform::Twitch, "CreditFollower") },
    );
    r.engine.ingest(follow_ev).await;
    let ec_frame: Value = serde_json::from_str(ws_ec.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(ec_frame["type"], "state");
    assert_eq!(ec_frame["data"]["credits"]["followers"][0]["name"], "CreditFollower");
    let cr_http: Value = r.get("/api/v1/credits").await.json().await.unwrap();
    assert_eq!(cr_http["followers"][0]["name"], "CreditFollower");

    let mut emote_msg = Event::live(
        Platform::Twitch,
        Some("m-em".into()),
        ferrocast_core::event::Millis(t0 + 20),
        EventKind::ChatMessage {
            actor: Actor::named(Platform::Twitch, "EmoteChatter"),
            content: Content {
                text: "Kappa".into(),
                fragments: vec![ferrocast_core::event::Fragment::Emote {
                    provider: ferrocast_core::event::EmoteProvider::Native,
                    id: "25".into(),
                    name: "Kappa".into(),
                    url: "https://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/2.0".into(),
                    zero_width: false,
                }],
            },
            reply_to: None,
            is_action: false,
        },
    );
    emote_msg.id = "ev-em-1".into();
    r.engine.ingest(emote_msg).await;
    let ew_frame: Value = serde_json::from_str(ws_ew.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(ew_frame["type"], "event");
    assert_eq!(ew_frame["data"]["kind"]["content"]["fragments"][0]["name"], "Kappa");
}

#[tokio::test]
async fn queue_controls_pause_mute_skip_and_their_state() {
    let r = start().await;
    let q: Value = r.get("/api/v1/queue").await.json().await.unwrap();
    assert_eq!((q["paused"].as_bool(), q["muted"].as_bool()), (Some(false), Some(false)), "{q}");
    for (path, key, want) in [
        ("/api/v1/queue/pause", "paused", true),
        ("/api/v1/queue/mute", "muted", true),
        ("/api/v1/queue/unmute", "muted", false),
        ("/api/v1/queue/resume", "paused", false),
    ] {
        let res = r.post(path, json!({})).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let q: Value = res.json().await.unwrap();
        assert_eq!(q[key].as_bool(), Some(want), "{path}: {q}");
    }
    // Idempotent, and skipping with nothing playing is fine.
    assert_eq!(r.post("/api/v1/queue/mute", json!({})).await.status(), StatusCode::OK);
    let q: Value = r.post("/api/v1/queue/mute", json!({})).await.json().await.unwrap();
    assert_eq!(q["muted"], true);
    assert_eq!(r.post("/api/v1/queue/skip", json!({})).await.status(), StatusCode::OK);
    // Without the session token every one is refused.
    for p in ["/api/v1/queue/mute", "/api/v1/queue/unmute", "/api/v1/queue/skip"] {
        assert_eq!(r.anonymous(reqwest::Method::POST, p).await.status(), StatusCode::UNAUTHORIZED, "{p}");
    }
    assert_eq!(r.get("/api/v1/queue/mute").await.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn alert_rules_take_weight_and_interrupt_and_refuse_bad_weights() {
    let r = start().await;
    let rules: Value = r.get("/api/v1/alert-rules").await.json().await.unwrap();
    let mut rule = rules[0].clone();
    assert!(rule["variations"][0]["weight"].is_number() && rule["interrupt"] == false, "{rule}");
    rule["interrupt"] = json!(true);
    rule["variations"][0]["weight"] = json!(7);
    let id = rule["id"].as_str().unwrap().to_string();
    let res = r.put(&format!("/api/v1/alert-rules/{id}"), rule.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let back: Value = res.json().await.unwrap();
    assert_eq!((back["interrupt"].as_bool(), back["variations"][0]["weight"].as_u64()), (Some(true), Some(7)));
    rule["variations"][0]["weight"] = json!(1001);
    let res = r.put(&format!("/api/v1/alert-rules/{id}"), rule).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let e: Value = res.json().await.unwrap();
    assert!(e.to_string().contains("weight"), "{e}");
}

#[tokio::test]
async fn creating_a_rule_checks_weight_and_interrupt_too() {
    let r = start().await;
    let rules: Value = r.get("/api/v1/alert-rules").await.json().await.unwrap();
    let mut base = rules[0].clone();
    base["id"] = json!("mine");
    base["name"] = json!("Mine");
    for (what, edit) in
        [("over the limit", json!(1001)), ("negative", json!(-1)), ("fractional", json!(1.5)), ("text", json!("heavy"))]
    {
        let mut rule = base.clone();
        rule["variations"][0]["weight"] = edit;
        let res = r.post("/api/v1/alert-rules", rule).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{what}");
    }
    let mut rule = base.clone();
    rule["interrupt"] = json!("yes");
    assert_eq!(
        r.post("/api/v1/alert-rules", rule).await.status(),
        StatusCode::BAD_REQUEST,
        "interrupt must be a boolean"
    );
    let mut ok = base.clone();
    ok["variations"][0]["weight"] = json!(1000);
    ok["interrupt"] = json!(true);
    assert_eq!(r.post("/api/v1/alert-rules", ok).await.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn the_schedule_routes_plan_list_and_delete_with_plain_errors() {
    let r = start().await;
    let soon = ferrocast_engine::now().0 + 3_600_000;
    // A past time is a 400 that names the field.
    let bad = r
        .post("/api/v1/schedule", json!({ "info": { "title": "Show" }, "platforms": ["twitch"], "start_ms": 1000 }))
        .await;
    assert_eq!(bad.status(), 400);
    let e: Value = bad.json().await.unwrap();
    assert!(e["error"]["message"].as_str().unwrap().starts_with("start"), "{e}");
    // A good plan is 201 and is listed.
    let ok = r
        .post(
            "/api/v1/schedule",
            json!({ "info": { "title": "Show" }, "platforms": ["twitch", "facebook"], "start_ms": soon }),
        )
        .await;
    assert_eq!(ok.status(), 201);
    let item: Value = ok.json().await.unwrap();
    assert_eq!(item["broadcasts"].as_array().unwrap().len(), 2);
    assert_eq!(item["reminded"], false);
    let list: Value = r.get("/api/v1/schedule").await.json().await.unwrap();
    assert_eq!(list["streams"].as_array().unwrap().len(), 1);
    // Nothing went live.
    let st: Value = r.get("/api/v1/status").await.json().await.unwrap();
    assert!(st["live"].as_object().unwrap().values().all(|v| v == &json!(false)), "{st}");
    // Delete, then an unknown id is 404.
    let id = item["id"].as_str().unwrap();
    assert_eq!(
        r.client
            .delete(format!("{}/api/v1/schedule/{id}", r.base))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        r.client
            .delete(format!("{}/api/v1/schedule/{id}", r.base))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // An unknown field is refused, not ignored.
    let odd = r
        .post("/api/v1/schedule", json!({ "info": { "title": "x" }, "platforms": ["tiktok"], "start_ms": soon }))
        .await;
    assert!(odd.status().is_client_error());
}

#[tokio::test]
async fn accounts_include_login_and_platform_urls() {
    use ferrocast_store::repo::settings::{Account, save_account};
    let r = start().await;
    r.engine
        .db(|s| {
            save_account(
                s,
                &Account {
                    platform: "twitch".into(),
                    user_id: "t1".into(),
                    login: "NovaGG".into(),
                    display_name: "Nova GG".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 1,
                },
            )?;
            save_account(
                s,
                &Account {
                    platform: "youtube".into(),
                    user_id: "UC_HANDLE".into(),
                    login: "@NovaPlays".into(),
                    display_name: "Nova Plays".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 2,
                },
            )?;
            save_account(
                s,
                &Account {
                    platform: "kick".into(),
                    user_id: "k1".into(),
                    login: "nova_kick".into(),
                    display_name: "NovaKick".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 3,
                },
            )?;
            save_account(
                s,
                &Account {
                    platform: "facebook".into(),
                    user_id: "fb1".into(),
                    login: "Nova Page".into(),
                    display_name: "Nova Page".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 4,
                },
            )
        })
        .await
        .unwrap();

    let list: Vec<Value> = r.get("/api/v1/accounts").await.json().await.unwrap();
    let by = |p: &str| list.iter().find(|a| a["platform"] == p).unwrap().clone();
    assert_eq!(by("twitch")["login"], "NovaGG");
    assert_eq!(by("twitch")["url"], "twitch.tv/novagg");
    assert_eq!(by("youtube")["login"], "@NovaPlays");
    assert_eq!(by("youtube")["url"], "youtube.com/@NovaPlays");
    assert_eq!(by("kick")["login"], "nova_kick");
    assert_eq!(by("kick")["url"], "kick.com/nova_kick");
    assert_eq!(by("facebook")["login"], "Nova Page");
    assert!(by("facebook")["url"].is_null());

    // Legacy YouTube row without an @handle falls back to /channel/<id> instead of display title.
    r.engine
        .db(|s| {
            save_account(
                s,
                &Account {
                    platform: "youtube".into(),
                    user_id: "UC_LEGACY_1234567890".into(),
                    login: "Nova Plays Live".into(),
                    display_name: "Nova Plays Live".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 5,
                },
            )
        })
        .await
        .unwrap();
    let list2: Vec<Value> = r.get("/api/v1/accounts").await.json().await.unwrap();
    let yt2 = list2.iter().find(|a| a["platform"] == "youtube").unwrap();
    assert_eq!(yt2["url"], "youtube.com/channel/UC_LEGACY_1234567890");
}

#[tokio::test]
async fn links_endpoints_validate_persist_and_push_overlay_state() {
    use ferrocast_store::repo::settings::{Account, save_account};
    let r = start().await;
    let init = r.get("/api/v1/links").await;
    assert_eq!(init.status(), StatusCode::OK);
    let init_v: Value = init.json().await.unwrap();
    assert!(init_v["resolved"]["socials"].as_array().unwrap().is_empty());

    for (payload, field) in [
        (json!({ "discord": "javascript:alert(1)" }), "discord"),
        (json!({ "website": "  JaVaScRiPt:void(0)" }), "website"),
        (json!({ "instagram": "bad\nline" }), "instagram"),
        (json!({ "x": "bad\0nul" }), "x"),
        (json!({ "tiktok": "a".repeat(121) }), "tiktok"),
        (
            json!({ "custom": (0..7).map(|i| json!({ "label": format!("L{i}"), "value": "x.com/a" })).collect::<Vec<_>>() }),
            "custom",
        ),
        (json!({ "custom": [{ "label": "", "value": "x.com/a" }] }), "custom"),
    ] {
        let bad = r.put("/api/v1/links", payload).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "expected 400 for {field}");
        let err: Value = bad.json().await.unwrap();
        let msg = err["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains(field), "expected error naming {field}: {err}");
    }

    r.engine
        .db(|s| {
            save_account(
                s,
                &Account {
                    platform: "twitch".into(),
                    user_id: "t1".into(),
                    login: "novaplays".into(),
                    display_name: "NovaPlays".into(),
                    avatar_url: None,
                    scopes: vec![],
                    connected_at: 1,
                },
            )
        })
        .await
        .unwrap();

    let ov: Value = r.post("/api/v1/overlays", json!({ "name": "Main" })).await.json().await.unwrap();
    let ws_url = ov["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let snap: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snap["type"], "snapshot");
    assert_eq!(snap["data"]["state"]["links"]["twitch"], "twitch.tv/novaplays");
    assert_eq!(snap["data"]["state"]["links"]["channel_name"], "NovaPlays");

    let saved = r
        .put(
            "/api/v1/links",
            json!({
                "discord": "https://discord.gg/nova/",
                "instagram": "@novagram",
                "schedule_text": "Fridays, 8 PM",
                "custom": [{ "label": "Merch", "value": "https://shop.nova.gg/" }]
            }),
        )
        .await;
    assert_eq!(saved.status(), StatusCode::OK);
    let saved_v: Value = saved.json().await.unwrap();
    assert_eq!(saved_v["discord"], "https://discord.gg/nova/");
    assert_eq!(saved_v["resolved"]["discord"], "discord.gg/nova");
    assert_eq!(saved_v["resolved"]["instagram"], "instagram.com/novagram");
    assert_eq!(saved_v["resolved"]["next_stream"], "Fridays, 8 PM");

    let next_state = async |ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >| loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        let f: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        if f["type"] == "state" && f["data"].get("links").is_some() {
            break f["data"]["links"].clone();
        }
    };
    let pushed = next_state(&mut ws).await;
    assert_eq!(pushed["discord"], "discord.gg/nova");
    assert_eq!(pushed["next_stream"], "Fridays, 8 PM");

    // Creating a scheduled stream pushes an updated resolved next_stream to open overlays.
    let soon =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 2 * 86_400_000;
    let created = r
        .post("/api/v1/schedule", json!({ "info": { "title": "Cozy Fri" }, "platforms": ["twitch"], "start_ms": soon }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let sched_item: Value = created.json().await.unwrap();
    let pushed_sched = next_state(&mut ws).await;
    assert_eq!(pushed_sched["next_stream_at"], soon);

    // Deleting the scheduled stream restores the schedule_text fallback and pushes again.
    let sid = sched_item["id"].as_str().unwrap();
    assert_eq!(
        r.client
            .delete(format!("{}/api/v1/schedule/{sid}", r.base))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let pushed_back = next_state(&mut ws).await;
    assert!(pushed_back["next_stream_at"].is_null());
    assert_eq!(pushed_back["next_stream"], "Fridays, 8 PM");

    // Partial PUT preserves omitted known fields AND unknown future keys, while "" clears and non-strings 400.
    let with_future = r.put("/api/v1/links", json!({ "bluesky": "bsky.app/profile/nova" })).await;
    assert_eq!(with_future.status(), StatusCode::OK);
    let old_client_patch = r.put("/api/v1/links", json!({ "x": "novaplays" })).await;
    assert_eq!(old_client_patch.status(), StatusCode::OK);
    let patch_v: Value = old_client_patch.json().await.unwrap();
    assert_eq!(patch_v["x"], "novaplays");
    assert_eq!(patch_v["discord"], "https://discord.gg/nova/");
    assert_eq!(patch_v["schedule_text"], "Fridays, 8 PM");
    assert_eq!(patch_v["custom"], json!([{ "label": "Merch", "value": "https://shop.nova.gg/" }]));
    assert_eq!(patch_v["bluesky"], "bsky.app/profile/nova");

    let cleared_x = r.put("/api/v1/links", json!({ "x": "" })).await;
    assert_eq!(cleared_x.status(), StatusCode::OK);
    let cleared_v: Value = cleared_x.json().await.unwrap();
    assert!(cleared_v["x"].is_null());
    assert_eq!(cleared_v["discord"], "https://discord.gg/nova/");
    assert_eq!(r.put("/api/v1/links", json!({ "discord": 123 })).await.status(), StatusCode::BAD_REQUEST);

    // Verify persistence in a fresh Engine started on the same store dir.
    drop(ws);
    let dir = r._dir;
    drop(r.engine);
    let fresh =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let from_disk = fresh.links().await.unwrap();
    assert_eq!(from_disk.links.discord.as_deref(), Some("https://discord.gg/nova/"));
    assert_eq!(json!(from_disk)["bluesky"], "bsky.app/profile/nova");
}

#[tokio::test]
async fn links_and_labels_enforce_auth_and_scopes() {
    let r = start().await;
    assert_eq!(r.anonymous(reqwest::Method::GET, "/api/v1/links").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.anonymous(reqwest::Method::PUT, "/api/v1/links").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.anonymous(reqwest::Method::GET, "/api/v1/labels").await.status(), StatusCode::UNAUTHORIZED);

    let key_v: Value = r
        .post("/api/v1/api-keys", json!({ "name": "Chat", "scopes": ["chat"], "expires_at": null }))
        .await
        .json()
        .await
        .unwrap();
    let tok = key_v["token"].as_str().unwrap();
    assert_eq!(
        r.client
            .put(format!("{}/api/v1/links", r.base))
            .bearer_auth(tok)
            .json(&json!({ "discord": "x" }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        r.client.get(format!("{}/api/v1/links", r.base)).bearer_auth(tok).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        r.client.get(format!("{}/api/v1/labels", r.base)).bearer_auth(tok).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn corrupt_creator_links_row_does_not_black_out_overlay_snapshots() {
    let r = start().await;
    r.engine
        .db(|s| {
            s.write(|tx| {
                tx.execute(
                    "INSERT INTO settings(key, value, updated_at) VALUES ('creator_links', '{corrupt-json', 1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [],
                )?;
                Ok(())
            })
        })
        .await
        .unwrap();

    let ov: Value = r.post("/api/v1/overlays", json!({ "name": "Main" })).await.json().await.unwrap();
    let ws_url = ov["url"].as_str().unwrap().replace("http://", "ws://") + "ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let snap: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snap["type"], "snapshot");
    assert!(snap["data"]["overlay"].is_object(), "overlay must not null out: {snap}");
    assert!(snap["data"]["state"]["links"]["socials"].is_array());
}

#[tokio::test]
async fn labels_endpoint_returns_real_labels_and_empty_when_unseen() {
    use ferrocast_core::event::{Actor, Event, EventKind, Millis, PaymentStatus, Platform, TipProvider};
    use ferrocast_core::money::Money;
    let r = start().await;
    let empty = r.get("/api/v1/labels").await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(empty.json::<Value>().await.unwrap(), json!({}));

    r.engine
        .ingest(Event::live(
            Platform::Twitch,
            Some("f1".into()),
            Millis(1),
            EventKind::Follow { actor: Actor::named(Platform::Twitch, "LunaLagoon") },
        ))
        .await;
    r.engine
        .ingest(Event::live(
            Platform::Tips,
            Some("t1".into()),
            Millis(2),
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Asha"),
                money: Money::new(1000, "USD"),
                message: None,
                provider: TipProvider::Stripe,
                payment_ref: "p1".into(),
                status: PaymentStatus::Captured,
            },
        ))
        .await;

    let labels: Value = r.get("/api/v1/labels").await.json().await.unwrap();
    assert_eq!(labels["latest_follower"], "LunaLagoon");
    assert_eq!(labels["latest_tip"], "Asha $10");
    assert_eq!(labels["session_follows"], "1");
    assert_eq!(labels["alltime_follows"], "1");

    // Monthly totals respect ?tz_offset_min=... across UTC month boundary.
    let now_utc =
        Millis(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64);
    let (s_0, _) = ferrocast_core::labels::month_bounds(now_utc, 0);
    let (s_840, _) = ferrocast_core::labels::month_bounds(now_utc, 840);
    let edge_ts = if s_840 < s_0 { s_0.0 - 60_000 } else { s_0.0 + 60_000 };
    r.engine
        .ingest(Event::live(
            Platform::Twitch,
            Some("f-boundary".into()),
            Millis(edge_ts),
            EventKind::Follow { actor: Actor::named(Platform::Twitch, "BoundaryFan") },
        ))
        .await;
    let utc_labels: Value = r.get("/api/v1/labels?tz_offset_min=0").await.json().await.unwrap();
    let shifted_labels: Value = r.get("/api/v1/labels?tz_offset_min=840").await.json().await.unwrap();
    assert_ne!(utc_labels["monthly_follows"], shifted_labels["monthly_follows"]);
}

#[tokio::test]
async fn overlay_test_preview_socket_receives_live_links_state_updates() {
    let r = start().await;
    let ov: Value = r.post("/api/v1/overlays", json!({ "name": "LivePreview" })).await.json().await.unwrap();
    let url = ov["url"].as_str().unwrap().replace("http://", "ws://") + "ws?test";
    let (mut test_ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let snap: Value = serde_json::from_str(test_ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(snap["type"], "snapshot");

    assert_eq!(
        r.put("/api/v1/links", json!({ "discord": "https://discord.gg/liveview/" })).await.status(),
        StatusCode::OK
    );

    let updated = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let msg = test_ws.next().await.unwrap().unwrap();
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            if v["type"] == "state" && v["data"].get("links").is_some() {
                break v["data"]["links"].clone();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(updated["discord"], "discord.gg/liveview");
}

#[tokio::test]
async fn put_links_refuses_to_overwrite_corrupt_creator_links_row() {
    let r = start().await;
    let corrupt_json = r#"{"discord":"discord.gg/keepme","x":123}"#;
    r.engine
        .db(move |s| {
            s.write(|tx| {
                tx.execute(
                    "INSERT INTO settings(key, value, updated_at) VALUES ('creator_links', ?1, 1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [corrupt_json],
                )?;
                Ok(())
            })
        })
        .await
        .unwrap();

    let res = r.put("/api/v1/links", json!({ "twitch": "twitch.tv/nova" })).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let err: Value = res.json().await.unwrap();
    assert_eq!(err["error"]["code"], "storage");
    let msg = err["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.to_lowercase().contains("my links") && msg.to_lowercase().contains("read"),
        "expected clear message on failed PUT /links read: {err}"
    );

    let stored_after: String = r
        .engine
        .db(|s| {
            s.read(|c| Ok(c.query_row("SELECT value FROM settings WHERE key = 'creator_links'", [], |r| r.get(0))?))
        })
        .await
        .unwrap();
    assert_eq!(stored_after, corrupt_json);

    // Best-effort GET /api/v1/links display path still returns 200 with empty fallback.
    assert_eq!(r.get("/api/v1/links").await.status(), StatusCode::OK);
}
