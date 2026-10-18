//! The dashboard from anywhere through the real `router()`: pairing (every status), the
//! device token (expiry, sliding, revoking), the proxy's statuses and role checks with a
//! fake PC on a real tunnel socket, live frames per role, the audit log, housekeeping and
//! the phone page.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const PEPPER: &[u8] = b"test pepper, at least sixteen bytes";

struct World {
    base: String,
    db: Arc<Db>,
    http: reqwest::Client,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn world_with(remote: bool, dashboard: Option<std::path::PathBuf>) -> World {
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
    let rc = if remote { RemoteConfig::new(Some(&base), Some(PEPPER), dashboard) } else { RemoteConfig::default() };
    let state = AppState::new(cfg, db).with_remote(rc);
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), state, _dir: dir }
}

async fn world() -> World {
    world_with(true, None).await
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        self.set_sub(sub, "active", Some(now_ms() + 30 * DAY_MS));
        auth
    }

    fn set_sub(&self, sub: &str, status: &str, current_end: Option<i64>) {
        let s = Synced { status: status.into(), current_end, cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
    }

    async fn send(&self, method: Method, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
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

    async fn code(&self, auth: &str, role: &str) -> (u16, Value) {
        self.send(
            Method::POST,
            "/v1/pro/remote/codes",
            Some(auth),
            Some(json!({ "role": role, "name": "Asha's phone" })),
        )
        .await
    }

    async fn pair(&self, code: &str) -> (u16, Value) {
        self.send(Method::POST, "/r/v1/pair", None, Some(json!({ "code": code }))).await
    }

    /// A paired device's token.
    async fn device(&self, auth: &str, role: &str) -> String {
        let (s, v) = self.code(auth, role).await;
        assert_eq!(s, 201, "{v}");
        let (s, v) = self.pair(v["code"].as_str().unwrap()).await;
        assert_eq!(s, 200, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    fn device_id(token: &str) -> String {
        token.strip_prefix("fcr_").unwrap().split_once('.').unwrap().0.to_string()
    }

    fn audit(&self, install: &str) -> Vec<(String, i64)> {
        self.db.remote_audit_list(install, 100).unwrap().into_iter().map(|a| (a.action, a.status)).collect()
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.replacen("http", "ws", 1))
    }
}

// ── A fake PC on the tunnel ──────────────────────────────────────────────────────────

/// What the fake PC saw, and a way to send frames as the PC.
struct Pc {
    frames: mpsc::UnboundedReceiver<Value>,
    out: mpsc::UnboundedSender<WsMsg>,
}

#[derive(Clone, Copy, PartialEq)]
enum PcMode {
    /// Answers every `req` with 200 and an echo of what it got.
    Echo,
    /// Never answers.
    Silent,
    /// Answers every `req` with 404 (a route this PC doesn't have).
    NotFound,
}

async fn connect_pc(w: &World, auth: &str, mode: PcMode) -> Pc {
    let mut req = w.ws_url("/v1/remote/tunnel").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut tx, mut rx) = ws.split();
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMsg>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                m = out_rx.recv() => match m {
                    Some(m) => if tx.send(m).await.is_err() { return },
                    None => { let _ = tx.close().await; return }
                },
                m = rx.next() => match m {
                    Some(Ok(WsMsg::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        if v["t"] == "req" && mode != PcMode::Silent {
                            let body = json!({
                                "method": v["method"], "path": v["path"], "headers": v["headers"],
                                "body_b64": v["body_b64"], "role": v["role"], "device_id": v["device_id"],
                            });
                            let status = if mode == PcMode::NotFound { 404 } else if v["method"] == "POST" { 201 } else { 200 };
                            let res = json!({
                                "t": "res", "id": v["id"], "status": status,
                                "headers": [["content-type", "application/json"], ["set-cookie", "x=1"], ["x-quota-reset", "5"],
                                            ["content-range", "bytes 0-9/100"], ["accept-ranges", "bytes"], ["server", "pc"]],
                                "body_b64": base64::engine::general_purpose::STANDARD.encode(body.to_string()),
                            });
                            if tx.send(WsMsg::text(res.to_string())).await.is_err() { return }
                        }
                        let _ = seen_tx.send(v);
                    }
                    Some(Ok(_)) => {}
                    _ => return,
                }
            }
        }
    });
    let mut pc = Pc { frames: seen_rx, out: out_tx };
    let hello = pc.next_of("hello").await;
    assert!(hello["devices"].is_i64());
    pc
}

impl Pc {
    async fn next_of(&mut self, t: &str) -> Value {
        loop {
            let v = tokio::time::timeout(Duration::from_secs(5), self.frames.recv()).await.unwrap().unwrap();
            if v["t"] == t {
                return v;
            }
        }
    }

    fn live(&self, v: Value) {
        self.out.send(WsMsg::text(v.to_string())).unwrap();
    }

    fn close(&self) {
        self.out.send(WsMsg::Close(None)).unwrap();
    }
}

/// Waits until the relay has (or no longer has) the install's tunnel.
async fn tunnel_is(w: &World, install: &str, up: bool) {
    for _ in 0..200 {
        if w.state.remote.pc_online(install) == up {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the tunnel never became {up}");
}

type PhoneWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Phone = Arc<AsyncMutex<PhoneWs>>;

impl World {
    /// A ticket from `POST /r/v1/ticket`.
    async fn ticket(&self, token: &str, what: &str) -> String {
        let (s, v) = self.send(Method::POST, "/r/v1/ticket", Some(token), Some(json!({ "for": what }))).await;
        assert_eq!(s, 200, "{v}");
        v["ticket"].as_str().unwrap().to_string()
    }
}

async fn phone(w: &World, token: &str) -> Phone {
    let t = w.ticket(token, "live").await;
    let url = w.ws_url(&format!("/r/api/v1/live?ticket={t}"));
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    Arc::new(AsyncMutex::new(ws))
}

fn ws_status(r: Result<impl std::fmt::Debug, tokio_tungstenite::tungstenite::Error>) -> u16 {
    match r {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => r.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

/// The next text frame on a phone socket, as JSON (`None`: closed, with the code).
async fn phone_next(p: &Phone) -> Result<Value, Option<u16>> {
    let mut rx = p.lock().await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.next()).await.expect("a frame") {
            Some(Ok(WsMsg::Text(t))) => return Ok(serde_json::from_str(&t).unwrap()),
            Some(Ok(WsMsg::Close(c))) => return Err(c.map(|c| u16::from(c.code))),
            Some(Ok(_)) => {}
            _ => return Err(None),
        }
    }
}

async fn phone_quiet(p: &Phone) {
    let mut rx = p.lock().await;
    let got = tokio::time::timeout(Duration::from_millis(250), rx.next()).await;
    assert!(got.is_err(), "expected nothing, got {got:?}");
}

// ── Pure parts ───────────────────────────────────────────────────────────────────────

#[test]
fn codes_tokens_and_names() {
    let c = new_code();
    assert_eq!(c.len(), CODE_LEN);
    assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{c}");
    assert_ne!(new_code(), new_code());
    assert_eq!(normal_code(" ab-cd 23 "), "ABCD23");
    assert_eq!(clean_name("  Asha\u{202e}'s\n  phone "), "Asha 's phone");
    assert_eq!(clean_name(&"x".repeat(80)).chars().count(), MAX_NAME);
    let cfg = RemoteConfig::new(Some("https://remote.example.com/app/x"), Some(PEPPER), None);
    assert_eq!(cfg.origin.as_deref(), Some("https://remote.example.com"));
    assert_eq!(cfg.host(), Some("remote.example.com"));
    assert_eq!(cfg.hash("a"), cfg.hash("a"));
    assert_ne!(cfg.hash("a"), RemoteConfig::new(None, Some(b"another pepper of 16+ bytes"), None).hash("a"));
    assert!(RemoteConfig::new(None, Some(b"short"), None).hash("a").is_none(), "a short pepper is no pepper");
    assert!(!format!("{cfg:?}").contains("test pepper"));
    assert_eq!(proxy_strip("session=abc&since=4"), Some("since=4".into()));
    assert_eq!(proxy_strip("session=abc"), None);
}

fn proxy_strip(q: &str) -> Option<String> {
    super::proxy::tests_strip_session(q)
}

// ── Pairing ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn codes_need_an_install_pro_and_the_feature() {
    let w = world().await;
    let (s, _) = w.send(Method::POST, "/v1/pro/remote/codes", None, Some(json!({ "role": "owner" }))).await;
    assert_eq!(s, 401);
    let free = w.install("free");
    let (s, v) = w.code(&free, "owner").await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
    let pro = w.pro("i1", "sub_1");
    let (s, v) = w.code(&pro, "boss").await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("invalid")));
    let res = w
        .http
        .post(format!("{}/v1/pro/remote/codes", w.base))
        .bearer_auth(&pro)
        .body("role=owner")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 415);
    let (s, v) = w.code(&pro, "owner").await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["code"].as_str().unwrap().len(), CODE_LEN);
    assert_eq!(v["url"], format!("{}/app/#pair={}", w.base, v["code"].as_str().unwrap()));
    assert_eq!(v["role"], "owner");
    assert_eq!(v["name"], "Asha's phone");
    assert!(v["expires_at"].as_i64().unwrap() > now_ms() + CODE_TTL_MS - 5_000);
    assert_eq!(v["expires_in"], 600, "seconds from now, so the desktop's clock doesn't matter");
    // Stored hashed: the code itself is nowhere in the database.
    let rows: Vec<String> = {
        let c = w.db.c();
        let mut q = c.prepare("SELECT code_hash FROM remote_codes").unwrap();
        q.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0], v["code"].as_str().unwrap());

    // Without a remote host or pepper the feature is off.
    let off = world_with(false, None).await;
    let pro = off.pro("i1", "sub_1");
    let (s, v) = off.code(&pro, "owner").await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("remote_unavailable")));
    assert!(!available(&off.state));
    assert!(available(&w.state));
}

#[tokio::test]
async fn a_code_works_once_and_every_miss_looks_the_same() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let (_, v) = w.code(&pro, "mod").await;
    let code = v["code"].as_str().unwrap().to_string();
    // Typed with spaces and lower case still works.
    let typed = format!("{} {}", code[..5].to_lowercase(), &code[5..]);
    let (s, v) = w.pair(&typed).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["role"], "mod");
    assert_eq!(v["name"], "Asha's phone");
    let token = v["token"].as_str().unwrap();
    assert!(token.starts_with("fcr_dv_"));
    // The token is kept hashed.
    let d = w.db.remote_device(&World::device_id(token)).unwrap().unwrap();
    assert!(!token.contains(&d.token_hash));
    assert_eq!(d.expires_at - d.created_at, TOKEN_TTL_MS);
    let (s, used) = w.pair(&code).await;
    // (Five tries per address: this test uses all of them, the next one would be a 429.)
    assert_eq!(s, 404);
    let (s, wrong) = w.pair("ZZZZZZZZZZ").await;
    assert_eq!(s, 404);
    assert_eq!(used, wrong, "a used code and a wrong one answer alike");
    assert_eq!(used["error"]["code"], "not_found");
    // Expired: same answer, and it's gone.
    let (_, v) = w.code(&pro, "owner").await;
    w.db.c().execute("UPDATE remote_codes SET expires_at = 1", []).unwrap();
    let (s, expired) = w.pair(v["code"].as_str().unwrap()).await;
    assert_eq!((s, &expired), (404, &wrong));
    // The PC lost Pro after making the code: same answer too.
    let (_, v) = w.code(&pro, "owner").await;
    w.set_sub("sub_1", "halted", Some(now_ms() - 10 * DAY_MS));
    let (s, lapsed) = w.pair(v["code"].as_str().unwrap()).await;
    assert_eq!((s, &lapsed), (404, &wrong));
}

#[tokio::test]
async fn pairing_is_limited_per_address() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let (_, v) = w.code(&pro, "owner").await;
    let (s, _) = w.send(Method::POST, "/r/v1/pair", None, None).await;
    assert_eq!(s, 415, "not JSON (and that counts as a try)");
    for _ in 1..PAIR_TRIES {
        assert_eq!(w.pair("WRONGWRONG").await.0, 404);
    }
    // Even the right code waits now.
    let (s, e) = w.pair(v["code"].as_str().unwrap()).await;
    assert_eq!((s, e["error"]["code"].as_str()), (429, Some("rate_limited")));
}

#[tokio::test]
async fn five_own_devices_and_ten_mods_unused_codes_included() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    for _ in 0..3 {
        w.device(&pro, "owner").await;
    }
    for _ in 0..2 {
        assert_eq!(w.code(&pro, "owner").await.0, 201);
    }
    let (s, v) = w.code(&pro, "owner").await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("too_many_devices")));
    assert!(v["error"]["message"].as_str().unwrap().contains("5 of your own devices"));
    for _ in 0..MAX_MOD_DEVICES {
        assert_eq!(w.code(&pro, "mod").await.0, 201);
    }
    assert_eq!(w.code(&pro, "mod").await.0, 409);
    // Expired codes stop counting.
    w.db.c().execute("UPDATE remote_codes SET expires_at = 1 WHERE role = 'mod'", []).unwrap();
    assert_eq!(w.code(&pro, "mod").await.0, 201);
}

#[tokio::test]
async fn pairing_only_on_the_remote_host() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let (_, v) = w.code(&pro, "owner").await;
    let res = w
        .http
        .post(format!("{}/r/v1/pair", w.base))
        .header("host", "ferrocast-relay.example.com")
        .json(&json!({ "code": v["code"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    // Another site's page can't pair (or use) a phone's session.
    let res = w
        .http
        .post(format!("{}/r/v1/pair", w.base))
        .header("origin", "https://evil.example")
        .json(&json!({ "code": v["code"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 403);
    // The code wasn't spent by either.
    assert_eq!(w.pair(v["code"].as_str().unwrap()).await.0, 200);
}

// ── Devices ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn devices_are_listed_and_removed_by_their_pc_only() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let other = w.pro("i2", "sub_2");
    let token = w.device(&pro, "mod").await;
    let id = World::device_id(&token);
    let (s, v) = w.send(Method::GET, "/v1/pro/remote/devices", Some(&pro), None).await;
    assert_eq!(s, 200);
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);
    assert_eq!(v["devices"][0]["id"], id);
    assert_eq!(v["devices"][0]["role"], "mod");
    assert!(v["devices"][0].get("token_hash").is_none());
    assert_eq!(v["limits"], json!({ "owner": 5, "mod": 10 }));
    assert_eq!(v["pc_online"], false);
    assert_eq!(v["audit"][0]["action"], "PAIR device");
    let (_, v) = w.send(Method::GET, "/v1/pro/remote/devices", Some(&other), None).await;
    assert_eq!(v["devices"], json!([]));
    let (s, _) = w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&other), None).await;
    assert_eq!(s, 404, "another PC can't remove it");
    // Listing and removing work without Pro (a lapsed streamer can still revoke).
    w.set_sub("sub_1", "halted", Some(now_ms() - 10 * DAY_MS));
    let (s, _) = w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&pro), None).await;
    assert_eq!(s, 204);
    let (s, _) = w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&pro), None).await;
    assert_eq!(s, 404);
    assert_eq!(w.audit("i1")[0], ("REVOKE device".to_string(), 204));
    let (s, _) = w.send(Method::GET, "/v1/pro/remote/devices", None, None).await;
    assert_eq!(s, 401);
}

#[tokio::test]
async fn tokens_expire_slide_and_can_be_forgotten() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let token = w.device(&pro, "owner").await;
    let id = World::device_id(&token);
    let (s, v) = w.send(Method::GET, "/r/v1/me", Some(&token), None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["role"].as_str(), v["pc_online"].as_bool()), (Some("owner"), Some(false)));
    // Used again after a while: 30 days from now.
    w.db.c().execute("UPDATE remote_devices SET last_used_at = 5, expires_at = ?1", [now_ms() + 1_000]).unwrap();
    assert_eq!(w.send(Method::GET, "/r/v1/me", Some(&token), None).await.0, 200);
    let d = w.db.remote_device(&id).unwrap().unwrap();
    assert!(d.expires_at > now_ms() + TOKEN_TTL_MS - 60_000, "slid forward");
    // Expired: 401 and gone.
    w.db.c().execute("UPDATE remote_devices SET expires_at = 1", []).unwrap();
    let (s, v) = w.send(Method::GET, "/r/v1/me", Some(&token), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")));
    assert!(w.db.remote_device(&id).unwrap().is_none());
    // A wrong secret with a real id, garbage, nothing: 401.
    let t2 = w.device(&pro, "owner").await;
    let forged = format!("fcr_{}.{}", World::device_id(&t2), "x".repeat(43));
    for bad in [forged.as_str(), "fcr_dv_x", "Bearer", ""] {
        assert_eq!(w.send(Method::GET, "/r/v1/me", Some(bad), None).await.0, 401, "{bad}");
    }
    // Forget: the phone removes itself.
    assert_eq!(w.send(Method::POST, "/r/v1/forget", Some(&t2), None).await.0, 204);
    assert_eq!(w.send(Method::GET, "/r/v1/me", Some(&t2), None).await.0, 401);
    // Pro lapsed: 402 for a device that's still paired.
    let t3 = w.device(&pro, "owner").await;
    w.set_sub("sub_1", "halted", Some(now_ms() - 10 * DAY_MS));
    let (s, v) = w.send(Method::GET, "/r/api/v1/status", Some(&t3), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
}

// ── The proxy ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_proxy_status() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let moder = w.device(&pro, "mod").await;
    // 401 without a token.
    assert_eq!(w.send(Method::GET, "/r/api/v1/status", None, None).await.0, 401);
    // 503 pc_offline without a tunnel.
    let (s, v) = w.send(Method::GET, "/r/api/v1/status", Some(&owner), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("pc_offline")));
    assert_eq!(v["error"]["message"], "Your streaming PC is off.");
    // 403 for a role, before the tunnel is even looked at.
    let (s, v) = w.send(Method::GET, "/r/api/v1/settings", Some(&moder), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("not_allowed_for_role")));
    let (s, _) = w.send(Method::GET, "/r/api/v1/accounts", Some(&owner), None).await;
    assert_eq!(s, 403);
    let (s, _) = w.send(Method::POST, "/r/api/v1/pro/remote/codes", Some(&owner), Some(json!({}))).await;
    assert_eq!(s, 403, "a phone can't pair more phones");
    // 413 over 2 MB.
    let big = vec![b'a'; MAX_BODY + 1];
    let res = w
        .http
        .post(format!("{}/r/api/v1/chat/send", w.base))
        .bearer_auth(&owner)
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 413);
    // Through a PC that answers.
    let mut pc = connect_pc(&w, &pro, PcMode::Echo).await;
    tunnel_is(&w, "i1", true).await;
    let (s, v) = w.send(Method::GET, "/r/api/v1/status", Some(&owner), None).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = w.send(Method::GET, "/r/v1/me", Some(&owner), None).await;
    assert_eq!((s, v["pc_online"].as_bool()), (200, Some(true)));
    // 429 over 20 a second.
    let mut statuses = Vec::new();
    for _ in 0..(DEVICE_RATE + 5) {
        statuses.push(w.send(Method::GET, "/r/api/v1/chat", Some(&moder), None).await.0);
    }
    assert!(statuses.contains(&429), "{statuses:?}");
    drop(pc.frames.try_recv());
    pc.close();
    tunnel_is(&w, "i1", false).await;
    // 504 pc_timeout from a PC that never answers.
    w.state.remote.set_pc_timeout_ms(200);
    let _silent = connect_pc(&w, &pro, PcMode::Silent).await;
    tunnel_is(&w, "i1", true).await;
    let (s, v) = w.send(Method::POST, "/r/api/v1/queue/pause", Some(&owner), Some(json!({}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (504, Some("pc_timeout")));
    assert!(w.audit("i1").contains(&("POST /queue/pause".to_string(), 504)));
}

#[tokio::test]
async fn a_request_round_trip_sends_only_what_it_should() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let moder = w.device(&pro, "mod").await;
    let _pc = connect_pc(&w, &pro, PcMode::Echo).await;
    tunnel_is(&w, "i1", true).await;
    let res = w
        .http
        .post(format!("{}/r/api/v1/mod/ban?x=1&session=abc", w.base))
        .bearer_auth(&moder)
        .header("cookie", "a=b")
        .header("x-forwarded-for", "1.2.3.4")
        .json(&json!({ "platform": "twitch", "user_id": "42", "reason": "spam" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let h = res.headers().clone();
    assert_eq!(h["cache-control"], "no-store");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(h["content-security-policy"].to_str().unwrap().contains("sandbox"));
    assert!(h.get("set-cookie").is_none(), "only a few headers come back");
    assert_eq!(h["x-quota-reset"], "5");
    assert!(h.get("access-control-allow-origin").is_none(), "no CORS");
    let echo: Value = res.json().await.unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["path"], "/api/v1/mod/ban?x=1", "the token is never passed on");
    assert_eq!(echo["role"], "mod");
    assert_eq!(echo["device_id"], World::device_id(&moder));
    let sent: Vec<String> =
        echo["headers"].as_array().unwrap().iter().map(|p| p[0].as_str().unwrap().to_string()).collect();
    assert_eq!(sent, vec!["content-type", "accept"], "no cookies, no forwarding headers, no Authorization");
    let body = base64::engine::general_purpose::STANDARD.decode(echo["body_b64"].as_str().unwrap()).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["reason"], "spam");
    // The device token never works in a URL any more, not even for an <img>.
    let (s, _) = w.send(Method::GET, &format!("/r/api/v1/media/m1?session={owner}"), None, None).await;
    assert_eq!(s, 401);
    let res = w.http.post(format!("{}/r/api/v1/test?session={owner}", w.base)).json(&json!({})).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 401);
    // The audit keeps the change (route template and status), not the read, never a body.
    let audit = w.audit("i1");
    assert!(audit.contains(&("POST /mod/ban".to_string(), 201)), "{audit:?}");
    assert!(!audit.iter().any(|(a, _)| a.contains("media")), "reads aren't logged: {audit:?}");
    let all: String = {
        let c = w.db.c();
        let mut q = c.prepare("SELECT action || device_name FROM remote_audit").unwrap();
        q.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    assert!(!all.contains("spam") && !all.contains("42"), "{all}");
    // Refusals are logged too.
    assert_eq!(w.send(Method::PUT, "/r/api/v1/mod/filters", Some(&moder), Some(json!({}))).await.0, 403);
    assert!(w.audit("i1").contains(&("PUT /mod/filters".to_string(), 403)));
    // Odd paths are refused before anything else.
    let res = w.http.get(format!("{}/r/api/v1/mod/%2e%2e/accounts", w.base)).bearer_auth(&owner).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 403);
}

#[tokio::test]
async fn a_newer_tunnel_replaces_the_old_and_needs_pro() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let mut first = connect_pc(&w, &pro, PcMode::Silent).await;
    let _second = connect_pc(&w, &pro, PcMode::Echo).await;
    assert_eq!(w.send(Method::GET, "/r/api/v1/status", Some(&owner), None).await.0, 200, "the newer one answers");
    // The hello said how many devices there are.
    drop(first.frames.try_recv());
    // A free install can't open one.
    let free = w.install("free");
    let mut req = w.ws_url("/v1/remote/tunnel").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {free}").parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status().as_u16(), 402),
        other => panic!("expected 402, got {other:?}"),
    }
    let req = w.ws_url("/v1/remote/tunnel").into_client_request().unwrap();
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status().as_u16(), 401),
        other => panic!("expected 401, got {other:?}"),
    }
}

#[tokio::test]
async fn the_pc_hears_when_devices_change() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let mut pc = connect_pc(&w, &pro, PcMode::Echo).await;
    let (_, v) = w.code(&pro, "owner").await;
    let f = pc.next_of("devices").await;
    assert_eq!((f["devices"].as_i64(), f["codes"].as_i64()), (Some(0), Some(1)));
    let (_, p) = w.pair(v["code"].as_str().unwrap()).await;
    let f = pc.next_of("devices").await;
    assert_eq!((f["devices"].as_i64(), f["codes"].as_i64()), (Some(1), Some(0)));
    let id = World::device_id(p["token"].as_str().unwrap());
    w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&pro), None).await;
    let f = pc.next_of("devices").await;
    assert_eq!((f["devices"].as_i64(), f["codes"].as_i64()), (Some(0), Some(0)));
}

// ── Live sockets ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn live_frames_reach_each_phone_by_role() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let moder = w.device(&pro, "mod").await;
    // Offline first: the phone hears "PC offline" and stays connected.
    let o = phone(&w, &owner).await;
    assert_eq!(phone_next(&o).await.unwrap()["data"], json!({ "pc_online": false, "role": "owner" }));
    let mut pc = connect_pc(&w, &pro, PcMode::Echo).await;
    // The PC came: the phone hears it, and the PC is asked for that phone's frames.
    assert_eq!(phone_next(&o).await.unwrap()["data"]["pc_online"], true);
    let open_o = pc.next_of("live_open").await;
    assert_eq!(open_o["role"], "owner");
    let m = phone(&w, &moder).await;
    assert_eq!(phone_next(&m).await.unwrap()["data"], json!({ "pc_online": true, "role": "mod" }));
    let open_m = pc.next_of("live_open").await;
    assert_eq!(open_m["device_id"], World::device_id(&moder));
    // A hello for one socket only.
    pc.live(json!({ "t": "live", "to": open_m["sock"], "data": r#"{"v":1,"type":"hello","data":{"seq":7}}"# }));
    assert_eq!(phone_next(&m).await.unwrap()["data"]["seq"], 7);
    phone_quiet(&o).await;
    // Chat reaches both; a tip only the owner (the relay checks the role too).
    let chat = r#"{"v":1,"seq":8,"type":"event","data":{"kind":{"type":"chat_message"}}}"#;
    let tip = r#"{"v":1,"seq":9,"type":"event","data":{"kind":{"type":"tip"}}}"#;
    pc.live(json!({ "t": "live", "data": chat }));
    pc.live(json!({ "t": "live", "data": tip }));
    assert_eq!(phone_next(&o).await.unwrap()["seq"], 8);
    assert_eq!(phone_next(&o).await.unwrap()["seq"], 9);
    assert_eq!(phone_next(&m).await.unwrap()["seq"], 8);
    phone_quiet(&m).await;
    // `role: owner` frames skip mods whatever they are.
    pc.live(json!({ "t": "live", "role": "owner", "data": r#"{"v":1,"type":"status","data":{}}"# }));
    assert_eq!(phone_next(&o).await.unwrap()["type"], "status");
    phone_quiet(&m).await;
    // The PC goes away: phones hear it.
    pc.close();
    assert_eq!(phone_next(&o).await.unwrap()["data"]["pc_online"], false);
    assert_eq!(phone_next(&m).await.unwrap()["data"]["pc_online"], false);
    // It reconnects: phones are opened again on the new tunnel (and told it's back).
    let mut pc2 = connect_pc(&w, &pro, PcMode::Echo).await;
    let a = pc2.next_of("live_open").await;
    let b = pc2.next_of("live_open").await;
    assert_ne!(a["sock"], b["sock"]);
    assert_eq!(phone_next(&o).await.unwrap()["data"]["pc_online"], true);
    // Removing the mod's device closes its socket with 4401, and tells the PC.
    let id = World::device_id(&moder);
    w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&pro), None).await;
    loop {
        match phone_next(&m).await {
            Ok(_) => continue,
            Err(code) => {
                assert_eq!(code, Some(proxy::CLOSE_REVOKED));
                break;
            }
        }
    }
    let closed = pc2.next_of("live_close").await;
    assert!(closed["sock"].is_u64());
}

#[tokio::test]
async fn the_live_socket_checks_the_token_and_origin() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    assert_eq!(ws_status(tokio_tungstenite::connect_async(w.ws_url("/r/api/v1/live?ticket=lt_nope")).await), 401);
    // The device token itself never opens it (it would sit in a URL).
    assert_eq!(
        ws_status(tokio_tungstenite::connect_async(w.ws_url(&format!("/r/api/v1/live?session={owner}"))).await),
        401
    );
    let t = w.ticket(&owner, "live").await;
    let mut req = w.ws_url(&format!("/r/api/v1/live?ticket={t}")).into_client_request().unwrap();
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    assert_eq!(ws_status(tokio_tungstenite::connect_async(req).await), 403);
    let mut req = w.ws_url(&format!("/r/api/v1/live?ticket={t}")).into_client_request().unwrap();
    req.headers_mut().insert("origin", w.base.parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok(), "the page's own origin is fine");
    // A ticket works once.
    assert_eq!(ws_status(tokio_tungstenite::connect_async(w.ws_url(&format!("/r/api/v1/live?ticket={t}"))).await), 401);
}

// ── Housekeeping and the page ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_sweep_drops_expired_rows_and_old_audit_lines() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let token = w.device(&pro, "owner").await;
    w.code(&pro, "mod").await;
    w.db.remote_audit("sub_1", "i1", "dv_x", "Old", "POST /mod/ban", 200, now_ms() - AUDIT_KEEP_MS - 1).unwrap();
    w.db.remote_audit("sub_1", "i1", "dv_x", "New", "POST /mod/ban", 200, now_ms() - 1_000).unwrap();
    sweep(&w.state).await;
    let names: Vec<String> = w.db.remote_audit_list("i1", 10).unwrap().into_iter().map(|a| a.device_name).collect();
    assert!(names.contains(&"New".to_string()) && !names.contains(&"Old".to_string()), "{names:?}");
    assert_eq!(w.db.remote_totals("i1", now_ms()).unwrap(), (1, 1));
    w.db.c().execute("UPDATE remote_devices SET expires_at = 1", []).unwrap();
    w.db.c().execute("UPDATE remote_codes SET expires_at = 1", []).unwrap();
    sweep(&w.state).await;
    let n: i64 = w.db.c().query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0)).unwrap();
    let c: i64 = w.db.c().query_row("SELECT COUNT(*) FROM remote_codes", [], |r| r.get(0)).unwrap();
    assert_eq!((n, c), (0, 0));
    assert_eq!(w.send(Method::GET, "/r/v1/me", Some(&token), None).await.0, 401);
}

#[tokio::test]
async fn the_phone_page_is_the_dashboard_with_its_config() {
    let dist = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dist.path().join("assets")).unwrap();
    std::fs::write(
        dist.path().join("index.html"),
        "<!doctype html><html><head><title>Ferrocast</title><script src=\"theme-boot.js\"></script></head><body></body></html>",
    )
    .unwrap();
    std::fs::write(dist.path().join("assets/app-1.js"), "console.log(1)").unwrap();
    std::fs::write(dist.path().join("theme-boot.js"), "1").unwrap();
    let w = world_with(true, Some(dist.path().to_path_buf())).await;
    let get = |p: &str| w.http.get(format!("{}{p}", w.base)).send();
    let res = get("/app/").await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let csp = res.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(!csp.contains("unsafe-eval"));
    let html = res.text().await.unwrap();
    assert!(
        html.starts_with("<!doctype html><html><head><script src=\"/app/ferrocast-remote.js\"></script><title>"),
        "{html}"
    );
    let js = get("/app/ferrocast-remote.js").await.unwrap().text().await.unwrap();
    assert!(js.contains(&format!("api: \"{}/r\"", w.base)) && js.contains("remote: true"), "{js}");
    let asset = get("/app/assets/app-1.js").await.unwrap();
    assert_eq!(asset.headers()["content-type"], "text/javascript; charset=utf-8");
    assert!(asset.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    // A route of the app is the app; a missing file and anything outside are 404.
    assert!(get("/app/settings").await.unwrap().text().await.unwrap().contains("ferrocast-remote.js"));
    assert_eq!(get("/app/nope.js").await.unwrap().status().as_u16(), 404);
    assert_eq!(get("/app/..%2Frelay.db").await.unwrap().status().as_u16(), 404);
    assert_eq!(get("/app/.hidden").await.unwrap().status().as_u16(), 404);
    assert_eq!(get("/app").await.unwrap().url().path(), "/app/");
    // Not on the remote host: nothing.
    let res = w.http.get(format!("{}/app/", w.base)).header("host", "tip.example.com").send().await.unwrap();
    assert_eq!(res.status().as_u16(), 404);
    // No build configured: a plain "not yet" page.
    let bare = world().await;
    let res = bare.http.get(format!("{}/app/", bare.base)).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 503);
}

// ── Review fixes (M6) ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tickets_keep_the_device_token_out_of_urls() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let other = w.device(&pro, "owner").await;
    let _pc = connect_pc(&w, &pro, PcMode::Echo).await;
    tunnel_is(&w, "i1", true).await;
    // Bearer only; a bad `for` is a clear 400.
    assert_eq!(w.send(Method::POST, "/r/v1/ticket", None, Some(json!({ "for": "live" }))).await.0, 401);
    let (s, v) = w.send(Method::POST, "/r/v1/ticket", Some(&owner), Some(json!({ "for": "admin" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("invalid")));
    let (s, v) = w.send(Method::POST, "/r/v1/ticket", Some(&owner), Some(json!({ "for": "live" }))).await;
    assert_eq!((s, v["expires_in"].as_i64()), (200, Some(60)));
    assert!(!v["ticket"].as_str().unwrap().contains(&owner));
    // A media ticket opens a preview (and is never passed on to the PC)…
    let mt = w.ticket(&owner, "media").await;
    let (s, v) = w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={mt}&w=64"), None, None).await;
    assert_eq!((s, v["path"].as_str()), (200, Some("/api/v1/media/m1?w=64")));
    assert_eq!(v["device_id"], World::device_id(&owner));
    // …works again within its 5 minutes (a video asks for many ranges)…
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={mt}"), None, None).await.0, 200);
    // …and opens nothing else.
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/status?ticket={mt}"), None, None).await.0, 401);
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media?ticket={mt}"), None, None).await.0, 401);
    let res = w.http.delete(format!("{}/r/api/v1/media/m1?ticket={mt}", w.base)).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 401);
    // Tampered, made for another device, or expired: 401.
    let forged = mt.replace(&World::device_id(&owner), &World::device_id(&other));
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={forged}"), None, None).await.0, 401);
    let (dev, exp) = {
        let mut p = mt.strip_prefix("mt1.").unwrap().split('.');
        (p.next().unwrap().to_string(), p.next().unwrap().parse::<i64>().unwrap())
    };
    let later = format!("mt1.{dev}.{}.{}", exp + 60_000, mt.rsplit('.').next().unwrap());
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={later}"), None, None).await.0, 401);
    let old_exp = now_ms() - 1;
    let old = format!("mt1.{dev}.{old_exp}.{}", w.state.remote.media_sig(&dev, old_exp).unwrap());
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={old}"), None, None).await.0, 401);
    // A removed device's ticket dies with it.
    w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{dev}"), Some(&pro), None).await;
    assert_eq!(w.send(Method::GET, &format!("/r/api/v1/media/m1?ticket={mt}"), None, None).await.0, 401);
}

#[tokio::test]
async fn a_live_ticket_expires_after_a_minute() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let t = w.ticket(&owner, "live").await;
    // Age it past its 60 s.
    for v in w.state.remote.tickets.lock().unwrap().values_mut() {
        v.1 = now_ms() - 1;
    }
    assert_eq!(ws_status(tokio_tungstenite::connect_async(w.ws_url(&format!("/r/api/v1/live?ticket={t}"))).await), 401);
}

#[tokio::test]
async fn a_device_keeps_at_most_three_live_sockets_and_opens_count_in_its_rate() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let mut socks = Vec::new();
    for _ in 0..MAX_LIVE_PER_DEVICE {
        let p = phone(&w, &owner).await;
        assert_eq!(phone_next(&p).await.unwrap()["type"], "remote");
        socks.push(p);
    }
    assert_eq!(proxy::tests_phone_count(&w.state, "i1"), MAX_LIVE_PER_DEVICE);
    // A fourth tab: the oldest closes with its own code, the count stays at three.
    let fourth = phone(&w, &owner).await;
    assert_eq!(phone_next(&fourth).await.unwrap()["type"], "remote");
    assert_eq!(phone_next(&socks[0]).await.err(), Some(Some(proxy::CLOSE_TOO_MANY)));
    assert_eq!(proxy::tests_phone_count(&w.state, "i1"), MAX_LIVE_PER_DEVICE);
    // Opening sockets (and asking for their tickets) uses the device's 20 a second.
    let mut refused = false;
    for _ in 0..(DEVICE_RATE + 5) {
        let (s, _) = w.send(Method::POST, "/r/v1/ticket", Some(&owner), Some(json!({ "for": "live" }))).await;
        if s == 429 {
            refused = true;
            break;
        }
    }
    assert!(refused, "ticket and socket opens are rate limited");
}

#[tokio::test]
async fn a_stuck_phone_is_dropped_and_a_removal_closes_it_even_with_a_full_queue() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let stuck = phone(&w, &owner).await;
    assert_eq!(proxy::tests_phone_count(&w.state, "i1"), 1);
    // The phone stops reading; the PC keeps sending big frames (one shared copy in memory).
    let big = format!(r#"{{"v":1,"type":"status","data":{{"x":"{}"}}}}"#, "a".repeat(64 * 1024));
    for _ in 0..4_000 {
        proxy::fan_out(&w.state, "i1", None, false, &big);
        if proxy::tests_phone_count(&w.state, "i1") == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(proxy::tests_phone_count(&w.state, "i1"), 0, "a full queue takes the phone out at once");
    // Its socket ends soon after (a 4408 close if it gets through, else the connection drops).
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        let mut rx = stuck.lock().await;
        loop {
            match rx.next().await {
                Some(Ok(WsMsg::Close(c))) => return c.map(|c| u16::from(c.code)),
                Some(Ok(_)) => continue,
                _ => return None,
            }
        }
    })
    .await
    .expect("the stuck phone's socket ended");
    assert!(matches!(ended, None | Some(proxy::CLOSE_LAGGED)), "{ended:?}");
    // A removed device closes with 4401 at once, however full its queue is.
    let second = phone(&w, &owner).await;
    for _ in 0..(proxy::PHONE_QUEUE - 1) {
        proxy::fan_out(&w.state, "i1", None, false, r#"{"v":1,"type":"status","data":{}}"#);
    }
    let id = World::device_id(&owner);
    w.send(Method::DELETE, &format!("/v1/pro/remote/devices/{id}"), Some(&pro), None).await;
    assert_eq!(proxy::tests_phone_count(&w.state, "i1"), 0);
    loop {
        match phone_next(&second).await {
            Ok(_) => continue,
            Err(code) => {
                assert_eq!(code, Some(proxy::CLOSE_REVOKED));
                break;
            }
        }
    }
}

#[tokio::test]
async fn refused_reads_arent_logged_and_refused_changes_once_a_minute() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let moder = w.device(&pro, "mod").await;
    let owner = w.device(&pro, "owner").await;
    let before = w.audit("i1").len();
    // The dashboard's boot reads a few things a mod can't: no noise.
    for p in ["/pro", "/settings", "/events", "/queue"] {
        assert_eq!(w.send(Method::GET, &format!("/r/api/v1{p}"), Some(&moder), None).await.0, 403);
    }
    assert_eq!(w.audit("i1").len(), before, "{:?}", w.audit("i1"));
    // A refused change is kept, once per route a minute however often it's tried.
    for _ in 0..5 {
        assert_eq!(w.send(Method::PUT, "/r/api/v1/settings", Some(&moder), Some(json!({}))).await.0, 403);
    }
    let lines: Vec<_> = w.audit("i1").into_iter().filter(|(a, _)| a == "PUT /settings").collect();
    assert_eq!(lines, vec![("PUT /settings".to_string(), 403)]);
    // Words a phone makes up never reach the log.
    assert_eq!(w.send(Method::POST, "/r/api/v1/call-me-maybe", Some(&moder), Some(json!({}))).await.0, 403);
    let _pc = connect_pc(&w, &pro, PcMode::NotFound).await;
    tunnel_is(&w, "i1", true).await;
    assert_eq!(w.send(Method::POST, "/r/api/v1/hello-world", Some(&owner), Some(json!({}))).await.0, 404);
    let all = w.audit("i1");
    assert!(all.contains(&("POST {unknown}".to_string(), 403)), "{all:?}");
    assert!(all.contains(&("POST {unknown}".to_string(), 404)), "{all:?}");
    assert!(!all.iter().any(|(a, _)| a.contains("call-me") || a.contains("hello")), "{all:?}");
}

#[tokio::test]
async fn video_previews_come_in_ranges() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    let _pc = connect_pc(&w, &pro, PcMode::Echo).await;
    tunnel_is(&w, "i1", true).await;
    let mt = w.ticket(&owner, "media").await;
    let res = w
        .http
        .get(format!("{}/r/api/v1/media/m1?ticket={mt}", w.base))
        .header("range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    let h = res.headers().clone();
    assert_eq!(h["content-range"], "bytes 0-9/100");
    assert_eq!(h["accept-ranges"], "bytes");
    assert!(h.get("server").is_none_or(|v| v != "pc"), "other headers still stay behind");
    let echo: Value = res.json().await.unwrap();
    assert!(echo["headers"].as_array().unwrap().contains(&json!(["range", "bytes=0-9"])), "{echo}");
}

#[tokio::test]
async fn two_links_at_once_cant_pass_the_device_limit() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let mut calls = Vec::new();
    for _ in 0..(MAX_OWNER_DEVICES * 3) {
        let (http, base, pro) = (w.http.clone(), w.base.clone(), pro.clone());
        calls.push(tokio::spawn(async move {
            http.post(format!("{base}/v1/pro/remote/codes"))
                .bearer_auth(pro)
                .json(&json!({ "role": "owner" }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    let mut made = 0;
    for c in calls {
        match c.await.unwrap() {
            201 => made += 1,
            409 => {}
            s => panic!("unexpected {s}"),
        }
    }
    assert_eq!(made, MAX_OWNER_DEVICES);
    assert_eq!(w.db.remote_totals("i1", now_ms()).unwrap(), (0, MAX_OWNER_DEVICES));
}

#[tokio::test]
async fn pairing_waits_are_said_in_minutes_from_a_sliding_limit() {
    let w = world().await;
    for _ in 0..PAIR_TRIES {
        assert_eq!(w.pair("WRONGWRONG").await.0, 404);
    }
    let res = w.http.post(format!("{}/r/v1/pair", w.base)).json(&json!({ "code": "WRONGWRONG" })).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 429);
    // One try comes back every two minutes, not all five after a fixed window.
    assert_eq!(res.headers()["retry-after"], "120");
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["hint"], "Wait 2 minutes, then scan a fresh code.");
}

#[tokio::test]
async fn a_lapsed_pro_says_so_and_keeps_the_phone_paired() {
    let w = world().await;
    let pro = w.pro("i1", "sub_1");
    let owner = w.device(&pro, "owner").await;
    w.set_sub("sub_1", "halted", Some(now_ms() - 10 * DAY_MS));
    let (s, v) = w.send(Method::GET, "/r/v1/me", Some(&owner), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")));
    assert!(v["error"]["hint"].as_str().unwrap().contains("stays paired"), "{v}");
    assert!(w.db.remote_device(&World::device_id(&owner)).unwrap().is_some(), "not removed");
}
