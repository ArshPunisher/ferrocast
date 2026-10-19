//! The dashboard from anywhere end to end: a real relay, this engine and server, a phone's
//! requests and live socket through the relay's tunnel, the role checked on the PC as well
//! as on the relay, and the tunnel coming back after the connection drops.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
use ferrocast_engine::{Engine, PlatformApps, RemoteHandler, RemoteRequest};
use ferrocast_relay::billing::store::Synced;
use ferrocast_secrets::Secrets;
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::routes::remote_scope::{self, Role};
use crate::{AppState, ServeOptions, auth, router, serve};

const PEPPER: &[u8] = b"server test pepper, 16+ bytes";

/// A TCP hop between the engine and the relay that can drop every connection at once.
#[derive(Clone, Default)]
struct Hop {
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Hop {
    async fn start(to: String) -> (Hop, String) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let hop = Hop::default();
        let conns = hop.conns.clone();
        tokio::spawn(async move {
            while let Ok((mut a, _)) = l.accept().await {
                let to = to.clone();
                let task = tokio::spawn(async move {
                    if let Ok(mut b) = tokio::net::TcpStream::connect(to.trim_start_matches("http://")).await {
                        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                    }
                });
                conns.lock().unwrap().push(task);
            }
        });
        (hop, url)
    }

    fn cut(&self) {
        for t in self.conns.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

struct World {
    /// This PC's dashboard API.
    base: String,
    token: String,
    /// The relay, as the phone reaches it.
    relay: String,
    http: reqwest::Client,
    relay_db: Arc<ferrocast_relay::Db>,
    secrets: Arc<ferrocast_secrets::MemorySecrets>,
    engine: Engine,
    hop: Hop,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn world() -> World {
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
        .with_remote(ferrocast_relay::remote::RemoteConfig::new(Some(&relay), Some(PEPPER), None));
    let relay_db = state.db.clone();
    let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(rl, rapp).await });
    let (hop, via) = Hop::start(relay.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let apps = PlatformApps { relay_url: Some(via), ..Default::default() };
    let engine = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    World {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        relay,
        http: reqwest::Client::new(),
        relay_db,
        secrets,
        engine,
        hop,
        _dirs: (dir, rdir),
    }
}

impl World {
    /// The desktop dashboard calling this PC.
    async fn desk(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    /// A phone calling the relay.
    async fn phone(&self, method: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.relay)).bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    /// Registers the engine with the relay, gives it Pro, and has it remember that.
    async fn make_pro(&self) {
        let _ = self.desk(Method::GET, "/pro", None).await;
        let auth = self.secrets.get("relay-install").unwrap().expect("registered with the relay");
        let install = auth.split('.').next().unwrap().to_string();
        self.relay_db.add_pro_sub("sub_r", "monthly", "e", "created", &install).unwrap();
        let s = Synced {
            status: "active".into(),
            current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
            cancel_at_end: None,
        };
        self.relay_db.sync_pro_sub(None, "sub_r", &s).unwrap();
        let (st, v) = self.desk(Method::POST, "/pro/refresh", Some(json!({}))).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["features"]["remote"], true, "{v}");
    }

    /// A paired phone's token (made on this PC's Settings › Dashboard anywhere).
    async fn pair(&self, role: &str) -> String {
        let (s, v) = self.desk(Method::POST, "/pro/remote/codes", Some(json!({ "role": role, "name": "Phone" }))).await;
        assert_eq!(s, 201, "{v}");
        let res = self.http.post(format!("{}/r/v1/pair", self.relay)).json(&json!({ "code": v["code"] })).send().await;
        let v: Value = res.unwrap().json().await.unwrap();
        v["token"].as_str().expect("a token").to_string()
    }

    async fn pc_online(&self, token: &str, want: bool) {
        for _ in 0..500 {
            let (_, v) = self.phone(Method::GET, "/r/v1/me", token, None).await;
            if v["pc_online"].as_bool() == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the PC never became {}", if want { "online" } else { "offline" });
    }
}

async fn chat(engine: &Engine, id: &str, kind: EventKind) {
    engine.ingest(Event::live(Platform::Twitch, Some(id.into()), Millis(0), kind)).await;
}

fn actor(name: &str) -> Actor {
    let mut a = Actor::named(Platform::Twitch, name);
    a.id = Some(format!("{name}-id"));
    a
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn live(w: &World, token: &str) -> Ws {
    // The device token never goes in the URL: a single-use ticket does.
    let (s, t) = w.phone(Method::POST, "/r/v1/ticket", token, Some(json!({ "for": "live" }))).await;
    assert_eq!(s, 200, "{t}");
    let url = format!("{}/r/api/v1/live?ticket={}", w.relay.replacen("http", "ws", 1), t["ticket"].as_str().unwrap());
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

/// The next frame of `ty` (frames of other types are skipped and returned too).
async fn until(ws: &mut Ws, ty: &str) -> (Value, Vec<Value>) {
    let mut skipped = Vec::new();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a frame").unwrap().unwrap();
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == ty {
                return (v, skipped);
            }
            skipped.push(v);
        }
    }
}

#[tokio::test]
async fn a_phone_reaches_this_pc_through_the_relay() {
    let w = world().await;
    w.make_pro().await;
    let owner = w.pair("owner").await;
    let moder = w.pair("mod").await;
    w.pc_online(&owner, true).await;

    // Through the tunnel, the real router answers.
    let (s, v) = w.phone(Method::GET, "/r/api/v1/status", &owner, None).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["connectors"].is_object(), "{v}");
    let (s, v) = w.phone(Method::GET, "/r/api/v1/chat", &moder, None).await;
    assert_eq!(s, 200, "{v}");
    // An owner can change a setting from the phone, but not LAN access.
    let (s, v) = w.phone(Method::PUT, "/r/api/v1/settings", &owner, Some(json!({ "theme": "light" }))).await;
    assert_eq!((s, v["theme"].as_str()), (200, Some("light")), "{v}");
    let lan = !json!(w.engine.settings())["lan_access"].as_bool().unwrap();
    let (s, v) = w.phone(Method::PUT, "/r/api/v1/settings", &owner, Some(json!({ "lan_access": lan }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "{v}");
    // Refused on the relay before reaching the PC.
    for (token, method, path) in [
        (&moder, Method::GET, "/r/api/v1/settings"),
        (&owner, Method::GET, "/r/api/v1/accounts"),
        (&owner, Method::POST, "/r/api/v1/pro/remote/codes"),
        (&owner, Method::POST, "/r/api/v1/session/pair-code"),
    ] {
        let (s, v) = w.phone(method, path, token, Some(json!({}))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "{path}: {v}");
    }
    // Unknown routes are this PC's own 404.
    let (s, v) = w.phone(Method::GET, "/r/api/v1/nothing-here", &owner, None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("no_route")), "{v}");

    // Settings › Dashboard anywhere on the PC lists both, with the audit.
    let (s, v) = w.desk(Method::GET, "/pro/remote", None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["devices"].as_array().unwrap().len(), 2);
    assert_eq!(v["tunnel"]["state"], "connected");
    assert_eq!(v["available"], true);
    assert!(v["audit"].as_array().unwrap().iter().any(|a| a["action"] == "PUT /settings" && a["status"] == 200), "{v}");
    // The phone can't see it, the desktop can revoke.
    let id = v["devices"][0]["id"].as_str().unwrap().to_string();
    let (s, _) = w.desk(Method::DELETE, &format!("/pro/remote/devices/{id}"), None).await;
    assert_eq!(s, 204);
    let (s, v) = w.desk(Method::DELETE, "/pro/remote/devices/dv_nope", None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")), "{v}");
    let (s, v) = w.desk(Method::POST, "/pro/remote/codes", Some(json!({ "role": "boss" }))).await;
    assert_eq!(s, 400, "{v}");
}

#[tokio::test]
async fn live_frames_by_role_and_the_tunnel_comes_back() {
    let w = world().await;
    w.make_pro().await;
    let owner = w.pair("owner").await;
    let moder = w.pair("mod").await;
    w.pc_online(&owner, true).await;
    let mut o = live(&w, &owner).await;
    let mut m = live(&w, &moder).await;
    let (first, _) = until(&mut o, "remote").await;
    assert_eq!(first["data"]["pc_online"], true);
    let (_, before) = until(&mut o, "status").await;
    assert!(before.iter().any(|f| f["type"] == "hello"), "{before:?}");
    let (hello, _) = until(&mut m, "hello").await;
    assert!(hello["data"]["seq"].is_u64());
    let (_, _) = until(&mut m, "status").await;

    // A follow reaches the owner only; chat reaches both.
    chat(&w.engine, "f1", EventKind::Follow { actor: actor("newfan") }).await;
    let kind = EventKind::ChatMessage {
        actor: actor("viewer"),
        content: Content::plain("hi"),
        reply_to: None,
        is_action: false,
    };
    chat(&w.engine, "c1", kind).await;
    let mut owner_kinds = Vec::new();
    while owner_kinds.len() < 2 {
        let (e, _) = until(&mut o, "event").await;
        owner_kinds.push(e["data"]["kind"]["type"].as_str().unwrap().to_string());
    }
    assert_eq!(owner_kinds, ["follow", "chat_message"]);
    let (e, skipped) = until(&mut m, "event").await;
    assert_eq!(e["data"]["kind"]["type"], "chat_message", "the mod never sees the follow");
    assert!(skipped.iter().all(|f| f["type"] != "event" && f["type"] != "queue"), "{skipped:?}");

    // The connection between the PC and the relay drops: phones hear it, and the PC
    // reconnects by itself (backoff) and phones get a fresh hello.
    w.hop.cut();
    let (gone, _) = until(&mut o, "remote").await;
    assert_eq!(gone["data"]["pc_online"], false);
    let (back, _) = until(&mut o, "remote").await;
    assert_eq!(back["data"]["pc_online"], true);
    let (_, _) = until(&mut o, "hello").await;
    let (resync, _) = until(&mut o, "resync").await;
    assert_eq!(resync["data"]["missed"], 0);
    let (s, _) = w.phone(Method::GET, "/r/api/v1/status", &owner, None).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn without_pairing_or_pro_the_tunnel_stays_closed() {
    let w = world().await;
    // Listing (and removing) devices never needs Pro, so a lapsed streamer can still revoke.
    let (s, v) = w.desk(Method::GET, "/pro/remote", None).await;
    assert_eq!((s, v["available"].as_bool(), v["tunnel"]["state"].as_str()), (200, Some(false), Some("off")), "{v}");
    let (s, v) = w.desk(Method::POST, "/pro/remote/codes", Some(json!({ "role": "owner" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
    assert_eq!(w.engine.remote_tunnel_view().state, "off");
    w.make_pro().await;
    let (s, v) = w.desk(Method::GET, "/pro/remote", None).await;
    assert_eq!((s, v["available"].as_bool()), (200, Some(true)), "{v}");
    assert_eq!(v["devices"], json!([]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.engine.remote_tunnel_view().state, "off", "nothing paired: no tunnel");
}

// ── The PC's own check, without the relay ────────────────────────────────────────────

async fn handler() -> (crate::remote::Handler, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    let state = AppState {
        engine: engine.clone(),
        sessions: Arc::new(auth::Sessions::new()),
        guard: auth::Guard { port: 47999, lan: false, dev_origins: vec![] },
    };
    (crate::remote::Handler::new(router(state), engine, 47999), dir)
}

async fn ask(h: &crate::remote::Handler, role: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
    let r = RemoteRequest {
        method: method.into(),
        path: path.into(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
        device_id: "dv_test".into(),
        role: role.into(),
    };
    let res = h.handle(r).await;
    (res.status, serde_json::from_slice(&res.body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_pc_refuses_what_a_role_may_not_do_even_if_the_relay_let_it_through() {
    let (h, _dir) = handler().await;
    for (method, path) in [
        ("GET", "/api/v1/accounts"),
        ("POST", "/api/v1/accounts/twitch/connect"),
        ("POST", "/api/v1/session/pair-code"),
        ("GET", "/api/v1/pro"),
        ("POST", "/api/v1/pro/remote/codes"),
        ("GET", "/api/v1/pro/backups/key"),
        ("PUT", "/api/v1/tips"),
        ("GET", "/api/v1/discord"),
        ("PUT", "/api/v1/obs"),
        ("POST", "/api/v1/export"),
        ("POST", "/api/v1/import"),
        ("POST", "/api/v1/media"),
        ("POST", "/api/v1/backups/b1/restore"),
        ("POST", "/api/v1/updates/install"),
        ("GET", "/api/v1/live"),
        ("GET", "/api/v1/mod/%2e%2e/accounts"),
    ] {
        let (s, v) = ask(&h, "owner", method, path, "{}").await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "owner {method} {path}");
    }
    for (method, path) in [
        ("GET", "/api/v1/settings"),
        ("POST", "/api/v1/chat/send"),
        ("PUT", "/api/v1/mod/filters"),
        ("GET", "/api/v1/events"),
        ("GET", "/api/v1/queue"),
        ("POST", "/api/v1/test"),
        ("PUT", "/api/v1/viewers/twitch/1/note"),
    ] {
        let (s, v) = ask(&h, "mod", method, path, "{}").await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "mod {method} {path}");
    }
    // Allowed ones reach the routes (no session token needed through the tunnel).
    assert_eq!(ask(&h, "mod", "GET", "/api/v1/chat", "").await.0, 200);
    assert_eq!(ask(&h, "mod", "GET", "/api/v1/mod/panic", "").await.0, 200);
    assert_eq!(ask(&h, "mod", "GET", "/api/v1/status", "").await.0, 200);
    assert_eq!(ask(&h, "owner", "GET", "/api/v1/settings", "").await.0, 200);
    let (s, v) = ask(&h, "owner", "GET", "/api/v1/settings?x=1", "").await;
    assert_eq!(s, 200, "{v}");
    // Outside /api/v1, an unknown role: refused.
    assert_eq!(ask(&h, "owner", "GET", "/app/", "").await.0, 404);
    assert_eq!(ask(&h, "owner", "GET", "/o/abc/", "").await.0, 404);
    assert_eq!(ask(&h, "admin", "GET", "/api/v1/status", "").await.0, 403);
    // Live frames: mods get chat, not a tip or the queue.
    assert!(h.live_allowed("mod", r#"{"v":1,"type":"event","data":{"kind":{"type":"chat_message"}}}"#));
    assert!(!h.live_allowed("mod", r#"{"v":1,"type":"event","data":{"kind":{"type":"tip"}}}"#));
    assert!(!h.live_allowed("mod", r#"{"v":1,"type":"queue","data":{}}"#));
    assert!(h.live_allowed("owner", r#"{"v":1,"type":"queue","data":{}}"#));
    assert!(!h.live_allowed("admin", r#"{"v":1,"type":"hello","data":{}}"#));
}

#[tokio::test]
async fn a_request_from_the_network_cant_pretend_to_be_a_phone() {
    // Without the tunnel's extension, the session token rules apply as always.
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    let server = serve(engine, ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    let res = reqwest::Client::new()
        .get(format!("{}/api/v1/status", server.base_url()))
        .header("x-ferrocast-role", "owner")
        .header("authorization", "Bearer fcr_dv_x.y")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 401);
}

#[test]
fn the_relay_and_the_pc_have_the_same_rules() {
    use ferrocast_relay::remote::scope as relay;
    assert_eq!(remote_scope::MOD_ALLOWED, relay::MOD_ALLOWED);
    assert_eq!(remote_scope::OWNER_DENIED, relay::OWNER_DENIED);
    assert_eq!(remote_scope::OWNER_ALLOWED, relay::OWNER_ALLOWED);
    assert_eq!(remote_scope::MOD_EVENT_KINDS, relay::MOD_EVENT_KINDS);
    assert_eq!(remote_scope::MOD_FRAMES, relay::MOD_FRAMES);
    let paths = [
        "/status",
        "/chat",
        "/mod/ban",
        "/mod/held/e1/drop",
        "/viewers/twitch/1",
        "/settings",
        "/accounts",
        "/pro/remote",
        "/pro/history",
        "/pro/history/email",
        "/pro/bot",
        "/pro/bot/enabled",
        "/pro/bot/twitch",
        "/pro/bot/twitch/connect",
        "/pro/bot/config",
        "/tips",
        "/tips/brand",
        "/tips/brand/avatar",
        "/pro/tips/domain",
        "/pro/tips/domainx",
        "/media",
        "/media/m1",
        "/a/../b",
        "/x%2fy",
        "/designs/d1/export",
        "/live",
        "/obs",
        "/obs/detect",
        "/session/pair",
    ];
    for path in paths {
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
            for (ours, theirs) in [(Role::Owner, relay::Role::Owner), (Role::Mod, relay::Role::Mod)] {
                assert_eq!(
                    remote_scope::allowed(ours, method, path),
                    relay::allowed(theirs, method, path),
                    "{ours} {method} {path}"
                );
            }
        }
    }
}

/// The relay never lets more requests wait on a PC than the PC answers at once (else they'd
/// wait there past the phone's timeout). Checked when the tests compile.
const _RELAY_QUEUE_FITS_THE_PC: () =
    assert!(ferrocast_relay::remote::MAX_IN_FLIGHT <= ferrocast_engine::REMOTE_IN_FLIGHT);

#[test]
fn media_ranges_are_cut_to_fit_one_answer() {
    use crate::remote::{RANGE_CHUNK, clamp_range};
    assert_eq!(clamp_range("bytes=0-"), format!("bytes=0-{}", RANGE_CHUNK - 1));
    assert_eq!(clamp_range("bytes=100-"), format!("bytes=100-{}", 100 + RANGE_CHUNK - 1));
    assert_eq!(clamp_range("bytes=0-99"), "bytes=0-99");
    assert_eq!(clamp_range("bytes=0-99999999"), format!("bytes=0-{}", RANGE_CHUNK - 1));
    assert_eq!(clamp_range("bytes=-500"), "bytes=-500");
    assert_eq!(clamp_range("bytes=-99999999"), format!("bytes=-{RANGE_CHUNK}"));
    assert_eq!(clamp_range("bytes=0-1,5-9"), "bytes=0-1,5-9", "several ranges: the route refuses");
    assert_eq!(clamp_range("items=1-2"), "items=1-2");
}

#[tokio::test]
async fn a_phone_streams_a_big_video_in_parts() {
    let (h, _dir) = handler().await;
    let mut data = vec![0u8; 10 * 1024 * 1024];
    data[4..8].copy_from_slice(b"ftyp");
    let media = h
        .engine_for_tests()
        .db(move |s| Ok(ferrocast_store::repo::media::save(s, "clip.mp4", &data).expect("saved")))
        .await
        .unwrap();
    let get = |range: Option<&str>| RemoteRequest {
        method: "GET".into(),
        path: format!("/api/v1/media/{}", media.id),
        headers: range.map(|r| vec![("range".to_string(), r.to_string())]).unwrap_or_default(),
        body: Vec::new(),
        device_id: "dv_test".into(),
        role: "owner".into(),
    };
    // A video's first ask (`bytes=0-`) gets the first 4 MB, with what it needs to ask on.
    let res = h.handle(get(Some("bytes=0-"))).await;
    assert_eq!(res.status, 206);
    assert_eq!(res.body.len() as u64, crate::remote::RANGE_CHUNK);
    let header = |n: &str| res.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(
        header("content-range"),
        Some(format!("bytes 0-{}/{}", crate::remote::RANGE_CHUNK - 1, 10 * 1024 * 1024))
    );
    assert_eq!(header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(header("content-type").as_deref(), Some("video/mp4"));
    // The last part.
    let res = h.handle(get(Some("bytes=10000000-"))).await;
    assert_eq!((res.status, res.body.len()), (206, 10 * 1024 * 1024 - 10_000_000));
    // The whole file at once is too big for one answer: a clear 413, not a broken page.
    let res = h.handle(get(None)).await;
    assert_eq!(res.status, 413);
    let v: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(v["error"]["code"], "too_large");
}

#[tokio::test]
async fn a_phone_may_look_at_and_switch_the_cloud_bot_but_never_connect_it() {
    let (h, _dir) = handler().await;
    // An owner's phone reaches the two routes (the PC then asks the relay: no relay here, so
    // not 403 but the engine's own answer).
    for (method, path, body) in
        [("GET", "/api/v1/pro/bot", ""), ("PUT", "/api/v1/pro/bot/enabled", r#"{"enabled":false}"#)]
    {
        let (s, v) = ask(&h, "owner", method, path, body).await;
        assert_ne!(v["error"]["code"].as_str(), Some("not_allowed_for_role"), "owner {method} {path}: {s} {v}");
    }
    // Connecting, disconnecting and the rest of /pro stay on the PC.
    for (method, path) in [
        ("POST", "/api/v1/pro/bot/twitch/connect"),
        ("POST", "/api/v1/pro/bot/kick/connect"),
        ("DELETE", "/api/v1/pro/bot/twitch"),
        ("PUT", "/api/v1/pro/bot"),
        ("GET", "/api/v1/pro/bot/enabled"),
        ("PUT", "/api/v1/pro/bot/config"),
    ] {
        let (s, v) = ask(&h, "owner", method, path, "{}").await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "owner {method} {path}");
    }
    // A mod: nothing.
    for (method, path) in [("GET", "/api/v1/pro/bot"), ("PUT", "/api/v1/pro/bot/enabled")] {
        let (s, v) = ask(&h, "mod", method, path, r#"{"enabled":true}"#).await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "mod {method} {path}");
    }
    use ferrocast_relay::remote::scope as relay;
    assert!(relay::allowed(relay::Role::Owner, "GET", "/pro/bot"));
    assert!(relay::allowed(relay::Role::Owner, "PUT", "/pro/bot/enabled"));
    assert!(!relay::allowed(relay::Role::Owner, "POST", "/pro/bot/twitch/connect"));
    assert!(!relay::allowed(relay::Role::Mod, "GET", "/pro/bot"));
}

#[tokio::test]
async fn a_phone_may_look_at_the_tip_address_but_never_change_it() {
    let (h, _dir) = handler().await;
    let (s, v) = ask(&h, "owner", "GET", "/api/v1/pro/tips/domain", "").await;
    assert_ne!(v["error"]["code"].as_str(), Some("not_allowed_for_role"), "owner GET: {s} {v}");
    for (role, method) in [("owner", "PUT"), ("owner", "DELETE"), ("mod", "GET"), ("mod", "PUT")] {
        let (s, v) = ask(&h, role, method, "/api/v1/pro/tips/domain", r#"{"hostname":"tips.x.com"}"#).await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")), "{role} {method}");
    }
    use ferrocast_relay::remote::scope as relay;
    assert!(relay::allowed(relay::Role::Owner, "GET", "/pro/tips/domain"));
    assert!(!relay::allowed(relay::Role::Owner, "PUT", "/pro/tips/domain"));
    assert!(!relay::allowed(relay::Role::Owner, "DELETE", "/pro/tips/domain"));
    assert!(!relay::allowed(relay::Role::Mod, "GET", "/pro/tips/domain"));
}
