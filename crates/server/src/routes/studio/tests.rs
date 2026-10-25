//! Studio routes over a real socket: the happy paths and every error status (400, 401, 404,
//! 409, 415, 422, 207 for partial success), with a fake OBS from the testkit and fake
//! platforms. Nothing here can reach a real OBS: OBS is always pointed at a fake or at a
//! port nothing listens on.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrocast_connector::{CategoryHit, Connector, ConnectorError, StreamInfo};
use ferrocast_core::event::*;
use ferrocast_core::moderation::{Capabilities, ModeChange};
use ferrocast_engine::{Engine, PasswordChange};
use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, closed_port, start as start_obs};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{ServeOptions, serve};

#[derive(Debug, Default)]
struct Fake {
    platform: Option<Platform>,
    refuse: bool,
    said: Mutex<Vec<String>>,
}

#[async_trait]
impl Connector for Fake {
    fn platform(&self) -> Platform {
        self.platform.unwrap_or(Platform::Twitch)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: true, can_timeout: true, can_delete: true }
    }
    async fn send_chat(&self, text: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        self.said.lock().unwrap().push(text.into());
        Ok(())
    }
    async fn delete_message(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn timeout(&self, _: &str, _: u32, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ban(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn unban(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn set_mode(&self, _: ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn update_stream_info(&self, info: &StreamInfo) -> ferrocast_connector::Result<Vec<String>> {
        if self.refuse {
            return Err(ConnectorError::RateLimited {
                platform: self.platform(),
                retry_after: std::time::Duration::from_secs(9),
            });
        }
        Ok(info.fields().into_iter().map(String::from).collect())
    }
    async fn search_categories(&self, q: &str) -> ferrocast_connector::Result<Vec<CategoryHit>> {
        Ok(vec![CategoryHit { id: "516575".into(), name: format!("{q}rant"), image_url: None }])
    }
}

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    engine: Engine,
    _dir: tempfile::TempDir,
}

impl Running {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}/api/v1{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.send(reqwest::Method::POST, path, body).await
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.send(reqwest::Method::PUT, path, body).await
    }
    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client.delete(format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    // Never a real OBS: closed until a test brings the fake.
    let port = closed_port().await;
    engine.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        engine,
        _dir: dir,
    }
}

async fn fake_obs(r: &Running) -> ferrocast_testkit::obs::St {
    let st = FakeObs::studio();
    let port = start_obs(st.clone()).await;
    r.engine.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    st
}

async fn code(res: reqwest::Response) -> (StatusCode, String) {
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    (status, v["error"]["code"].as_str().unwrap_or("<no error>").to_string())
}

async fn body(res: reqwest::Response) -> (StatusCode, Value) {
    let s = res.status();
    (s, res.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn every_studio_route_needs_the_session() {
    let r = start().await;
    for path in [
        "/counters",
        "/quotes",
        "/modules",
        "/stream-info",
        "/show",
        "/show/preflight",
        "/obs/scenes",
        "/obs/stream",
        "/moments/lane",
        "/moments/markers/export",
    ] {
        let res = r.client.get(format!("{}/api/v1{path}", r.base)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn counters_routes_and_their_errors() {
    let r = start().await;
    let (s, c) = body(r.post("/counters", json!({ "name": "Boss Deaths", "value": 3 })).await).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        (c["name"].as_str(), c["value"].as_i64(), c["command"].as_str()),
        (Some("boss_deaths"), Some(3), Some("!boss_deaths"))
    );
    assert_eq!(
        code(r.post("/counters", json!({ "name": "boss deaths" })).await).await,
        (StatusCode::CONFLICT, "conflict".into())
    );
    assert_eq!(code(r.post("/counters", json!({ "name": "!!" })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.post("/counters", json!({ "name": "x", "command": 5 })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        code(r.post("/counters", json!({ "name": "y", "reply": "{bad" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let no_json =
        r.client.post(format!("{}/api/v1/counters", r.base)).bearer_auth(&r.token).body("{}").send().await.unwrap();
    assert_eq!(code(no_json).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (s, v) = body(r.post("/counters/boss_deaths/change", json!({ "op": "add", "amount": 2 })).await).await;
    assert_eq!((s, v["value"].as_i64()), (StatusCode::OK, Some(5)));
    let (_, v) = body(r.post("/counters/boss_deaths/change", json!({ "op": "reset" })).await).await;
    assert_eq!(v["value"], 0);
    assert_eq!(
        code(r.post("/counters/boss_deaths/change", json!({ "op": "explode" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/counters/boss_deaths/change", json!({ "op": "set" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/counters/boss_deaths/change", json!({ "op": "add", "amount": 9_000_000_000_i64 })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/counters/ghost/change", json!({ "op": "add" })).await).await,
        (StatusCode::NOT_FOUND, "counter_not_found".into())
    );
    let (s, v) = body(r.put("/counters/boss_deaths", json!({ "enabled": false, "value": 7 })).await).await;
    assert_eq!((s, v["enabled"].as_bool(), v["value"].as_i64()), (StatusCode::OK, Some(false), Some(7)));
    assert_eq!(code(r.put("/counters/boss_deaths", json!({ "enabled": "no" })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.put("/counters/ghost", json!({})).await).await.0, StatusCode::NOT_FOUND);
    let (_, list) = body(r.get("/counters").await).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(r.delete("/counters/boss_deaths").await.status(), StatusCode::NO_CONTENT);
    assert_eq!(code(r.delete("/counters/boss_deaths").await).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn quotes_and_modules_routes() {
    let r = start().await;
    let (s, q) = body(r.post("/quotes", json!({ "text": "gg no re" })).await).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(q["added_by"], "Dashboard");
    let id = q["id"].as_i64().unwrap();
    assert_eq!(code(r.post("/quotes", json!({ "text": "  " })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.post("/quotes", json!({ "text": "x".repeat(401) })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.post("/quotes", json!({})).await).await.0, StatusCode::BAD_REQUEST);
    let (s, e) = body(r.put(&format!("/quotes/{id}"), json!({ "text": "edited" })).await).await;
    assert_eq!((s, e["text"].as_str()), (StatusCode::OK, Some("edited")));
    assert_eq!(code(r.put("/quotes/abc", json!({ "text": "x" })).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        code(r.put("/quotes/999", json!({ "text": "x" })).await).await,
        (StatusCode::NOT_FOUND, "quote_not_found".into())
    );
    let (_, list) = body(r.get("/quotes").await).await;
    assert_eq!(list[0]["text"], "edited");
    assert_eq!(r.delete(&format!("/quotes/{id}")).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(code(r.delete(&format!("/quotes/{id}")).await).await.0, StatusCode::NOT_FOUND);
    let (_, m) = body(r.get("/modules").await).await;
    assert_eq!(m["eight_ball"]["enabled"], true);
    assert_eq!(m["roulette"]["enabled"], false);
    let (s, m) = body(r.put("/modules", json!({ "roulette": { "enabled": true, "win_percent": 30 } })).await).await;
    assert_eq!((s, m["roulette"]["win_percent"].as_u64()), (StatusCode::OK, Some(30)));
    assert_eq!(m["eight_ball"]["enabled"], true, "other modules kept");
    assert_eq!(
        code(r.put("/modules", json!({ "roulette": { "win_percent": 150 } })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(code(r.put("/modules", json!({ "dice": "off" })).await).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stream_info_answers_200_207_or_422_with_each_platforms_result() {
    let r = start().await;
    let info = json!({ "info": { "title": "Ranked" } });
    assert_eq!(
        code(r.post("/stream-info/apply", info.clone()).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "no_platforms".into())
    );
    r.engine.register_connector(Arc::new(Fake { platform: Some(Platform::Twitch), ..Default::default() }));
    let (s, v) = body(r.post("/stream-info/apply", info.clone()).await).await;
    assert_eq!((s, v["ok"].as_u64()), (StatusCode::OK, Some(1)));
    r.engine.register_connector(Arc::new(Fake { platform: Some(Platform::Kick), refuse: true, ..Default::default() }));
    let (s, v) = body(r.post("/stream-info/apply", info.clone()).await).await;
    assert_eq!(s, StatusCode::MULTI_STATUS);
    let kick = v["results"].as_array().unwrap().iter().find(|x| x["platform"] == "kick").unwrap();
    assert_eq!(kick["error"]["code"], "rate_limited");
    assert!(kick["error"]["hint"].is_string());
    let (s, v) =
        body(r.post("/stream-info/apply", json!({ "info": { "title": "x" }, "platforms": ["kick"] })).await).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "stream_info_failed");
    assert_eq!(v["results"][0]["platform"], "kick", "the details come with the error");
    assert_eq!(code(r.post("/stream-info/apply", json!({ "info": {} })).await).await.0, StatusCode::BAD_REQUEST);
    // A platform's own fields: checked like the rest, named by platform; unknown platforms refused.
    let (s, v) = body(
        r.post("/stream-info/apply", json!({ "info": { "title": "x", "per": { "youtube": { "title": "" } } } })).await,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().starts_with("per.youtube.title:"), "{v}");
    assert_eq!(
        code(r.post("/stream-info/apply", json!({ "info": { "title": "x", "per": { "tiktok": {} } } })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/stream-info/apply", json!({ "info": { "title": "x".repeat(141) } })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/stream-info/apply", json!({ "info": { "title": "x" }, "platforms": ["tiktok"] })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/stream-info/apply", json!({ "info": { "tags": ["no spaces please"] } })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, view) = body(r.get("/stream-info").await).await;
    assert_eq!(view["history"].as_array().unwrap().len(), 3, "every push is remembered");
    assert_eq!(view["platforms"].as_array().unwrap().len(), 3);
    // Categories
    let (s, v) = body(r.get("/stream-info/categories?platform=twitch&q=valo").await).await;
    assert_eq!((s, v["items"][0]["name"].as_str()), (StatusCode::OK, Some("valorant")));
    assert_eq!(code(r.get("/stream-info/categories?platform=twitch").await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.get("/stream-info/categories?platform=nope&q=x").await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        code(r.get("/stream-info/categories?platform=youtube&q=x").await).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Presets
    let p = json!({ "name": "Ranked", "info": { "title": "Ranked night" } });
    let (s, saved) = body(r.post("/stream-info/presets", p.clone()).await).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(code(r.post("/stream-info/presets", p).await).await.0, StatusCode::CONFLICT);
    assert_eq!(
        code(r.post("/stream-info/presets", json!({ "name": "", "info": { "title": "x" } })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let id = saved["id"].as_str().unwrap();
    let (s, up) = body(
        r.put(&format!("/stream-info/presets/{id}"), json!({ "name": "Ranked 2", "info": { "title": "y" } })).await,
    )
    .await;
    assert_eq!((s, up["name"].as_str()), (StatusCode::OK, Some("Ranked 2")));
    assert_eq!(
        code(r.put("/stream-info/presets/ghost", json!({ "name": "z", "info": { "title": "y" } })).await).await,
        (StatusCode::NOT_FOUND, "preset_not_found".into())
    );
    assert_eq!(r.delete(&format!("/stream-info/presets/{id}")).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(code(r.delete(&format!("/stream-info/presets/{id}")).await).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn show_routes_with_obs_closed_then_open() {
    let r = start().await;
    let (_, pf) = body(r.get("/show/preflight").await).await;
    assert_eq!(pf["ready"], false);
    assert_eq!(pf["checks"][0]["id"], "obs");
    assert_eq!(pf["checks"][0]["fix"]["href"], "#/settings/obs");
    assert_eq!(
        code(r.post("/show/start", json!({})).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "preflight_failed".into())
    );
    assert_eq!(
        code(r.post("/show/preflight/fix", json!({ "action": "unmute:Mic" })).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "obs_unreachable".into())
    );
    assert_eq!(code(r.put("/show/config", json!({ "countdown_s": 3 })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.put("/show/config", json!({ "countdown_s": "soon" })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        code(r.post("/show/countdown/skip", json!({})).await).await,
        (StatusCode::NOT_FOUND, "no_countdown".into())
    );
    let obs = fake_obs(&r).await;
    let (s, cfg) = body(
        r.put(
            "/show/config",
            json!({ "starting_scene": "Starting Soon", "live_scene": "Gameplay", "countdown_s": 60, "discord": false }),
        )
        .await,
    )
    .await;
    assert_eq!((s, cfg["countdown_s"].as_u64()), (StatusCode::OK, Some(60)));
    assert_eq!(
        code(r.post("/show/preflight/fix", json!({ "action": "launch" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let (s, run) = body(r.post("/show/start", json!({ "force": true })).await).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(run["show"]["phase"], "countdown");
    assert_eq!(obs.lock().unwrap().current, "Starting Soon");
    assert_eq!(code(r.post("/show/start", json!({ "force": true })).await).await.0, StatusCode::CONFLICT);
    let (_, cur) = body(r.get("/show").await).await;
    assert!(cur["show"]["countdown_ends_at"].is_i64());
    let (_, c) = body(r.post("/show/countdown/cancel", json!({})).await).await;
    assert_eq!(c["show"]["phase"], "starting");
    let (_, l) = body(r.post("/show/live-scene", json!({})).await).await;
    assert_eq!(l["show"]["phase"], "live");
    assert_eq!(obs.lock().unwrap().current, "Gameplay");
    let (s, end) = body(r.post("/show/end", json!({})).await).await;
    assert_eq!((s, end["show"]["phase"].as_str()), (StatusCode::OK, Some("ended")));
}

#[tokio::test]
async fn obs_helper_routes_and_their_errors() {
    let r = start().await;
    assert_eq!(code(r.get("/obs/scenes").await).await, (StatusCode::UNPROCESSABLE_ENTITY, "obs_unreachable".into()));
    assert_eq!(code(r.post("/obs/replay/save", json!({})).await).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, h) = body(r.get("/obs/health").await).await;
    assert!(h["health"].is_null());
    let obs = fake_obs(&r).await;
    let (s, scenes) = body(r.get("/obs/scenes").await).await;
    assert_eq!((s, scenes["current"].as_str()), (StatusCode::OK, Some("Starting Soon")));
    let (_, inputs) = body(r.get("/obs/inputs").await).await;
    assert!(inputs.as_array().unwrap().iter().any(|i| i["name"] == "Mic"));
    assert_eq!(r.post("/obs/scene", json!({ "scene": "BRB" })).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(obs.lock().unwrap().current, "BRB");
    assert_eq!(
        code(r.post("/obs/scene", json!({ "scene": "Nope" })).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "obs_refused".into())
    );
    assert_eq!(code(r.post("/obs/scene", json!({})).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.post("/obs/source", json!({ "scene": "Gameplay", "item_id": 2, "visible": false })).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        code(r.post("/obs/source", json!({ "scene": "Gameplay", "item_id": 99, "visible": false })).await).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        r.post("/obs/input/mute", json!({ "input": "Mic", "muted": true })).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(obs.lock().unwrap().inputs[0].muted);
    let (s, v) = body(r.post("/obs/replay/save", json!({})).await).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"]["message"].as_str().unwrap().contains("replay buffer isn't running"));
    assert!(v["error"]["hint"].is_string());
    obs.lock().unwrap().replay_active = true;
    let (s, clip) = body(r.post("/obs/replay/save", json!({ "label": "Ace" })).await).await;
    assert_eq!((s, clip["marker"]["kind"].as_str()), (StatusCode::CREATED, Some("clip")));
    assert_eq!(
        code(r.post("/obs/replay/save", json!({ "label": "x".repeat(201) })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        code(r.post("/obs/brb", json!({ "on": true })).await).await,
        (StatusCode::UNPROCESSABLE_ENTITY, "no_brb_scene".into())
    );
    let (s, _) =
        body(r.put("/obs/switcher", json!({ "brb_scene": "BRB", "hotkeys": ["Gameplay", "BRB"] })).await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        code(r.put("/obs/switcher", json!({ "hotkeys": ["a","b","c","d","e","f","g","h","i","j"] })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let (s, b) = body(r.post("/obs/brb", json!({ "on": true })).await).await;
    assert_eq!((s, b["scene"].as_str()), (StatusCode::OK, Some("BRB")));
    let (s, h) = body(r.post("/obs/health", json!({})).await).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["health"]["disk_gb"].as_f64().unwrap() > 0.0);
    // Start and stop streaming.
    let (s, b) = body(r.get("/obs/stream").await).await;
    assert_eq!((s, b["streaming"].as_bool()), (StatusCode::OK, Some(false)));
    let (s, b) = body(r.post("/obs/stream", json!({ "on": true })).await).await;
    assert_eq!((s, b["streaming"].as_bool()), (StatusCode::OK, Some(true)));
    assert!(obs.lock().unwrap().streaming);
    let (s, b) = body(r.post("/obs/stream", json!({ "on": false })).await).await;
    assert_eq!((s, b["streaming"].as_bool()), (StatusCode::OK, Some(false)));
    assert_eq!(code(r.post("/obs/stream", json!({})).await).await.0, StatusCode::BAD_REQUEST);
    obs.lock().unwrap().stream_unconfigured = true;
    let res = r.post("/obs/stream", json!({ "on": true })).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn moments_routes_lane_markers_and_exports() {
    let r = start().await;
    r.engine.patch_settings(json!({ "currency": "INR" }).as_object().unwrap()).await.unwrap();
    let tip = Event::live(
        Platform::Tips,
        Some("t1".into()),
        Millis(0),
        EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: ferrocast_core::money::Money::new(100_000, "INR"),
            message: None,
            provider: TipProvider::Razorpay,
            payment_ref: "t1".into(),
            status: PaymentStatus::Captured,
        },
    );
    r.engine.ingest(tip).await;
    let (_, lane) = body(r.get("/moments/lane").await).await;
    let id = lane[0]["id"].as_str().unwrap().to_string();
    assert_eq!(lane[0]["title"], "Asha tipped ₹1,000");
    assert_eq!(r.post(&format!("/moments/lane/{id}/dismiss"), json!({})).await.status(), StatusCode::OK);
    assert_eq!(
        code(r.post(&format!("/moments/lane/{id}/dismiss"), json!({})).await).await,
        (StatusCode::NOT_FOUND, "lane_item_not_found".into())
    );
    assert_eq!(r.post("/moments/lane/dismiss-all", json!({})).await.status(), StatusCode::OK);
    let (s, m) = body(r.post("/moments/markers", json!({ "label": "Funny bit" })).await).await;
    assert_eq!((s, m["kind"].as_str()), (StatusCode::CREATED, Some("manual")));
    assert_eq!(
        code(r.post("/moments/markers", json!({ "label": "x".repeat(201) })).await).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, list) = body(r.get("/moments/markers?since=0").await).await;
    assert_eq!(list.as_array().unwrap().len(), 2, "the big tip and the manual one");
    assert_eq!(code(r.get("/moments/markers?since=abc").await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(code(r.get("/moments/markers?since=99999999999999").await).await.0, StatusCode::BAD_REQUEST);
    let csv = r.get("/moments/markers/export?format=csv&since=0").await;
    assert_eq!(csv.status(), StatusCode::OK);
    assert!(csv.headers()["content-type"].to_str().unwrap().starts_with("text/csv"));
    assert!(csv.headers()["content-disposition"].to_str().unwrap().contains(".csv"));
    let text = csv.text().await.unwrap();
    assert!(text.starts_with("timecode,offset_seconds,kind,label"));
    assert!(text.contains("Funny bit"));
    let txt = r.get("/moments/markers/export?format=txt&since=0").await;
    assert!(txt.text().await.unwrap().contains("Funny bit"));
    assert_eq!(code(r.get("/moments/markers/export?format=pdf").await).await.0, StatusCode::BAD_REQUEST);
    let mid = m["id"].as_i64().unwrap();
    assert_eq!(r.delete(&format!("/moments/markers/{mid}")).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        code(r.delete(&format!("/moments/markers/{mid}")).await).await,
        (StatusCode::NOT_FOUND, "marker_not_found".into())
    );
    assert_eq!(code(r.delete("/moments/markers/x").await).await.0, StatusCode::NOT_FOUND);
    let (s, cfg) = body(r.put("/moments/config", json!({ "big_tip": 1000, "first_chatters": false })).await).await;
    assert_eq!((s, cfg["big_tip"].as_i64()), (StatusCode::OK, Some(1000)));
    assert_eq!(code(r.put("/moments/config", json!({ "volume": 500 })).await).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn smoke_every_studio_screen_loads() {
    let r = start().await;
    for path in [
        "/counters",
        "/quotes",
        "/modules",
        "/stream-info",
        "/show",
        "/show/config",
        "/show/preflight",
        "/obs/health",
        "/obs/switcher",
        "/moments/config",
        "/moments/lane",
        "/moments/markers",
    ] {
        let res = r.get(path).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let _: Value = res.json().await.unwrap();
    }
}

#[tokio::test]
async fn the_studio_engine_needs_the_desktop_app() {
    let r = start().await;
    let (s, v) = body(r.get("/studio/engine").await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["available"], false);
    assert!(v["detail"].as_str().is_some_and(|d| d.contains("Ferrocast app")), "{v}");
    for res in [
        r.post("/studio/engine/start", json!({})).await,
        r.get("/studio/engine/kinds").await,
        r.put("/studio/engine/preview", json!({ "rect": { "x": 0, "y": 0, "w": 640, "h": 360 } })).await,
        r.get("/studio/engine/kinds/color_source_v3/defaults").await,
        r.get("/studio/engine/sources").await,
        r.get("/studio/engine/stream").await,
        r.post("/studio/engine/stream", json!({ "on": false })).await,
        r.get("/studio/engine/sources/Back/settings").await,
        r.get("/studio/engine/sources/Back/options").await,
        r.put("/studio/engine/sources/Back/settings", json!({ "settings": { "color": 1 } })).await,
        r.get("/studio/engine/scenes").await,
        r.post("/studio/engine/scenes", json!({ "name": "Gameplay" })).await,
        r.delete("/studio/engine/scenes/Gameplay").await,
        r.post("/studio/engine/scenes/Gameplay/duplicate", json!({ "name": "Gameplay 2" })).await,
        r.post("/studio/engine/scenes/Gameplay/move", json!({ "to": 0 })).await,
        r.post("/studio/engine/scenes/Just%20chatting/current", json!({})).await,
        r.get("/studio/engine/scenes/Gameplay/items").await,
        r.post("/studio/engine/scenes/Gameplay/items", json!({ "kind": "color_source", "name": "Back" })).await,
        r.post("/studio/engine/scenes/Gameplay/items", json!({ "existing": "Back" })).await,
        r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "lock" })).await,
        r.get("/studio/engine/scenes/Gameplay/audio").await,
        r.put("/studio/engine/transition", json!({ "kind": "fade_transition", "ms": 300 })).await,
        r.post("/studio/engine/sources/Back/rename", json!({ "name": "Front" })).await,
        r.put("/studio/engine/sources/Mic/audio", json!({ "volume_db": -6.0, "muted": true, "monitor": "only_me" }))
            .await,
        r.get("/studio/engine/sources/Mic/filters").await,
        r.post("/studio/engine/sources/Mic/filters", json!({ "kind": "noise_gate_filter", "name": "Gate" })).await,
        r.delete("/studio/engine/sources/Mic/filters/Gate").await,
        r.post("/studio/engine/sources/Mic/filters/Gate/edit", json!({ "do": "disable" })).await,
        r.get("/studio/engine/sources/Mic/filters/Gate/settings").await,
        r.put("/studio/engine/sources/Mic/filters/Gate/settings", json!({ "settings": {} })).await,
        r.get("/studio/engine/sources/Mic/filters/Gate/options").await,
        r.put("/studio/engine/channels/3", json!({ "kind": "coreaudio_input_capture", "name": "Mic" })).await,
        r.delete("/studio/engine/channels/3").await,
        r.post("/studio/engine/levels", json!({ "names": ["Mic"] })).await,
        r.get("/studio/engine/settings").await,
        r.put("/studio/engine/settings", json!({ "video_kbps": 4500 })).await,
        r.get("/studio/engine/stats").await,
        r.post("/studio/engine/record", json!({ "on": true })).await,
        r.post("/studio/engine/record/pause", json!({ "on": true })).await,
        r.get("/studio/engine/monitoring").await,
        r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "flip_h" })).await,
        r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "blend_multiply" })).await,
        r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "duplicate" })).await,
        r.post("/studio/engine/sources/Mic/paste-filters", json!({ "from": "Cam" })).await,
        r.post("/studio/engine/sources/Alerts/web", json!({ "do": "interact" })).await,
        r.get("/studio/engine/obs-collections").await,
        r.post("/studio/engine/obs-collections/import", json!({ "id": "Untitled" })).await,
    ] {
        assert_eq!(code(res).await, (StatusCode::UNPROCESSABLE_ENTITY, "studio_unavailable".into()));
    }
    // Bad input is refused before anything else.
    assert_eq!(code(r.get("/studio/engine/kinds/..%2Fx/options").await).await.0, StatusCode::BAD_REQUEST);
    let bad = r.put("/studio/engine/preview", json!({ "rect": { "x": 0, "y": 0, "w": 0, "h": 360 } })).await;
    assert_eq!(code(bad).await.0, StatusCode::BAD_REQUEST);
    for (bad, what) in [
        (
            r.post("/studio/engine/scenes/Gameplay/items", json!({ "kind": "color_source", "name": "" })).await,
            "empty name",
        ),
        (r.post("/studio/engine/scenes/Gameplay/items", json!({ "fit": true })).await, "nothing to add"),
        (r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "explode" })).await, "unknown edit"),
        (r.post("/studio/engine/scenes/Gameplay/items/x/edit", json!({ "do": "lock" })).await, "id not a number"),
        (r.put("/studio/engine/sources/Mic/audio", json!({ "volume_db": 99 })).await, "too loud"),
        (r.put("/studio/engine/sources/Mic/audio", json!({ "monitor": "everyone" })).await, "unknown monitor"),
        (r.put("/studio/engine/transition", json!({ "kind": "fade_transition", "ms": 999999 })).await, "too long"),
        (r.put("/studio/engine/channels/9", json!({ "kind": "x", "name": "Mic" })).await, "channel 9"),
        (r.post("/studio/engine/sources/Mic/filters/Gate/edit", json!({ "do": "lock" })).await, "filters don't lock"),
        (r.put("/studio/engine/settings", json!({ "video_kbps": 1 })).await, "bitrate too low"),
        (r.put("/studio/engine/settings", json!({ "fps": "sixty" })).await, "fps not a number"),
        (r.post("/studio/engine/record", json!({})).await, "record without on"),
        (r.post("/studio/engine/sources/Mic/paste-filters", json!({ "from": "Mic" })).await, "paste onto itself"),
        (r.post("/studio/engine/sources/Alerts/web", json!({ "do": "click" })).await, "unknown web act"),
        (r.post("/studio/engine/obs-collections/import", json!({ "id": "../x" })).await, "id with a path"),
        (r.post("/studio/engine/scenes/Gameplay/items/3/edit", json!({ "do": "blend_glow" })).await, "unknown blend"),
    ] {
        assert_eq!(code(bad).await.0, StatusCode::BAD_REQUEST, "{what}");
    }
}
