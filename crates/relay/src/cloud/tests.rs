//! Cloud mode through the real `router()`: turning it on, overlays (URLs, rotation, the
//! validators), alert rules, designs (built-ins read-only, branding, the design in use), the
//! desktop's sync (versions, conflicts, bad documents) and every refusal (off, signed out,
//! CSRF, cloud off, limits, rates).

use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};

use super::*;
use crate::accounts::store::Account;
use crate::remote::RemoteConfig;
use crate::{Config, Db, router};

const ALERT: &str = include_str!("../../../core/fixtures/designs/alert-design.json");
const SCENE: &str = include_str!("../../../core/fixtures/designs/scene-design.json");

// Built in pieces so nothing here looks like a real key.
fn secret() -> Vec<u8> {
    [b"cloud-test-".as_slice(), &[7u8; 24]].concat()
}

/// A built-in alert design and a built-in scene with its mark.
pub(super) fn catalogue() -> Vec<Design> {
    let mut alert: Value = serde_json::from_str(ALERT).unwrap();
    alert["id"] = json!("builtin.molten");
    alert["branded"] = json!(true);
    let mut scene: Value = serde_json::from_str(SCENE).unwrap();
    scene["id"] = json!("builtin.brb");
    scene["branded"] = json!(true);
    scene["content"]["layers"].as_array_mut().unwrap().push(json!({
        "id": "spark", "kind": "layer", "x": 1840, "y": 1010, "w": 56, "h": 48, "config": { "type": "brand", "variant": "mark" }
    }));
    let mut built = vec![alert, scene];
    for d in &mut built {
        d["built_in"] = json!(true);
        d["origin"] = json!({ "type": "built_in" });
    }
    built.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect()
}

pub(super) struct World {
    pub base: String,
    pub http: reqwest::Client,
    pub state: AppState,
    pub db: Arc<Db>,
    pub _dir: tempfile::TempDir,
    /// Twitch, Kick's bot client and Kick's web API (sign-in, users, event subscriptions).
    pub tw: Arc<crate::bot::tests::FakeTwitch>,
    pub kk: Arc<crate::bot::tests::FakeKick>,
    pub kick_web: Arc<KickWeb>,
}

/// What the fake Kick web API was asked: event subscriptions made (body) and deleted (ids).
#[derive(Debug, Default)]
pub(super) struct KickWeb {
    pub subscribed: std::sync::Mutex<Vec<Value>>,
    pub deleted: std::sync::Mutex<Vec<String>>,
}

/// One small key for Kick's webhook signatures (RSA keys are slow to make in debug builds).
pub(super) fn kick_key() -> &'static rsa::RsaPrivateKey {
    static KEY: std::sync::OnceLock<rsa::RsaPrivateKey> = std::sync::OnceLock::new();
    KEY.get_or_init(|| rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 1024).unwrap())
}

fn kick_public_pem() -> String {
    use rsa::pkcs8::EncodePublicKey;
    rsa::RsaPublicKey::from(kick_key()).to_public_key_pem(rsa::pkcs8::LineEnding::LF).unwrap()
}

/// The Kick channel the fake Kick signs in as.
pub(super) const KICK_CHANNEL: &str = "668";

async fn fake_kick(calls: Arc<KickWeb>) -> String {
    use axum::routing::{get, post};
    let (c1, c2) = (calls.clone(), calls);
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            post(|| async {
                axum::Json(json!({ "access_token": "kick-at", "refresh_token": "kick-rt", "expires_in": 3600 }))
            }),
        )
        .route(
            "/public/v1/public-key",
            get(|| async { axum::Json(json!({ "data": { "public_key": kick_public_pem() } })) }),
        )
        .route(
            "/public/v1/users",
            get(|| async {
                axum::Json(json!({ "data": [{ "user_id": 668, "name": "asha_kick", "profile_picture": "https://kick.test/a.png" }] }))
            }),
        )
        .route(
            "/public/v1/events/subscriptions",
            post(move |axum::Json(b): axum::Json<Value>| async move {
                let n = b["events"].as_array().map_or(0, Vec::len);
                c1.subscribed.lock().unwrap().push(b);
                let data: Vec<Value> = (0..n).map(|i| json!({ "subscription_id": format!("ks-{i}") })).collect();
                axum::Json(json!({ "data": data }))
            })
            .delete(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                c2.deleted.lock().unwrap().push(q.get("id").cloned().unwrap_or_default());
                axum::http::StatusCode::NO_CONTENT
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

/// A signed-in browser (cookie + CSRF) and the same account's app session.
pub(super) struct User {
    pub account: Account,
    pub cookie: String,
    pub csrf: String,
    pub app: String,
}

pub(super) async fn world() -> World {
    world_with(true).await
}

pub(super) async fn world_with(keyed: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let kick_web = Arc::new(KickWeb::default());
    let kick_base = fake_kick(kick_web.clone()).await;
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: Some("kick-client".into()),
        kick_client_secret: Some("kick-secret".into()),
        kick_id_url: kick_base.clone(),
        kick_api_url: kick_base,
    };
    let s = secret();
    let tw = Arc::new(crate::bot::tests::FakeTwitch::default());
    let kk = Arc::new(crate::bot::tests::FakeKick::default());
    let bot = crate::bot::Runtime::new(
        Some(crate::bot::tests::twitch_cfg()),
        Some(tw.clone() as Arc<dyn crate::bot::TwitchCloud>),
        Some(kk.clone() as Arc<dyn crate::bot::KickBot>),
    );
    // A tiny overlay build: the page, one script and the design catalogue.
    let build = dir.path().join("overlays");
    std::fs::create_dir_all(build.join("assets")).unwrap();
    std::fs::write(
        build.join("index.html"),
        "<!doctype html><title>Overlay</title><script src=./assets/app.js></script>",
    )
    .unwrap();
    std::fs::write(build.join("assets/app.js"), "console.log('overlay')").unwrap();
    let blobs = crate::blob::FsBlobStore::new(dir.path().join("blobs")).unwrap();
    let state = AppState::new(cfg, db)
        .with_vault(Some(crate::bot::tests::vault()))
        .with_blobs(Some(Arc::new(blobs)))
        .with_bot(bot)
        .with_remote(RemoteConfig::new(Some(&base), Some(b"test pepper, at least sixteen bytes"), None))
        .with_cloud(CloudConfig::new(Some("http://overlays.test"), Some(build), keyed.then_some(s.as_slice())));
    state.cloud.set_builtins(catalogue());
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, http, state, db, _dir: dir, tw, kk, kick_web }
}

impl World {
    pub(super) fn user(&self, email: &str) -> User {
        let (account, _) = self.db.account_sign_in(email, None, now_ms()).unwrap();
        let (web, _) = crate::accounts::new_session(&self.state, &account, "web", "Chrome on macOS", None).unwrap();
        let (app, _) = crate::accounts::new_session(&self.state, &account, "app", "Studio PC", None).unwrap();
        let secret = web.split_once('.').unwrap().1.to_string();
        User {
            account,
            cookie: format!("{}={web}", crate::accounts::SESSION_COOKIE),
            csrf: crate::accounts::csrf_token(&secret),
            app: format!("fca_{app}"),
        }
    }

    /// A call from the signed-in browser on the web host.
    pub(super) async fn web(&self, u: &User, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header("cookie", &u.cookie)
            .header("origin", &self.base);
        if method != Method::GET {
            r = r.header("x-csrf-token", &u.csrf);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// A call from the signed-in desktop app.
    pub(super) async fn app(&self, u: &User, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", u.app));
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// A signed-in user with cloud mode on.
    pub(super) async fn cloud_user(&self, email: &str) -> User {
        let u = self.user(email);
        let (s, v) = self.web(&u, Method::POST, "/c/v1/cloud/enable", None).await;
        assert_eq!(s, 200, "{v}");
        u
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn off_without_the_key_signed_out_and_csrf() {
    let w = world_with(false).await;
    let u = w.user("a@example.com");
    let (s, v) = w.web(&u, Method::GET, "/c/v1/cloud", None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
    assert!(v["error"]["hint"].is_string());

    let w = world().await;
    let res = w.http.get(format!("{}/c/v1/cloud", w.base)).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let u = w.user("a@example.com");
    // A write without the CSRF token, and one from another site.
    let res = w.http.post(format!("{}/c/v1/cloud/enable", w.base)).header("cookie", &u.cookie).send().await.unwrap();
    assert_eq!(res.status(), 403);
    let res = w
        .http
        .post(format!("{}/c/v1/cloud/enable", w.base))
        .header("cookie", &u.cookie)
        .header("x-csrf-token", &u.csrf)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // Not the web host: the cookie isn't read there.
    let res = w
        .http
        .get(format!("{}/c/v1/cloud", w.base))
        .header("host", "ferrocast-relay.example")
        .header("cookie", &u.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn a_bad_since_is_a_422_in_the_error_shape() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (s, v) = w.app(&u, Method::GET, "/v1/cloud/docs?since=abc", None).await;
    assert_eq!((s, code(&v)), (422, "invalid"), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().starts_with("since:"));
}

#[tokio::test]
async fn turning_it_on_seeds_rules_and_a_starter_overlay_once() {
    let w = world().await;
    let u = w.user("a@example.com");
    let (s, v) = w.web(&u, Method::GET, "/c/v1/cloud", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["overlay_origin"], "http://overlays.test");
    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    assert_eq!((s, code(&v)), (409, "cloud_off"));

    for _ in 0..2 {
        let (s, v) = w.web(&u, Method::POST, "/c/v1/cloud/enable", None).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["enabled"], true);
    }
    let (_, rules) = w.web(&u, Method::GET, "/c/api/v1/alert-rules", None).await;
    assert_eq!(rules.as_array().unwrap().len(), ferrocast_core::alert::default_rules().len());
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "Starter");
    let url = list[0]["url"].as_str().unwrap();
    let token = url.strip_prefix("http://overlays.test/o/").unwrap().trim_end_matches('/');
    assert_eq!(token.len(), 40);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(list[0].get("token").is_none());
    // The URL is the same every time, and only its hash is kept.
    let (_, again) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    assert_eq!(again[0]["url"], list[0]["url"]);
    assert_eq!(overlay_for_token(&w.state, token).map(|(a, _)| a), Some(u.account.id.clone()));
    let stored: i64 =
        w.db.c()
            .query_row("SELECT COUNT(*) FROM cloud_overlay_tokens WHERE token_hash = ?1", [token], |r| r.get(0))
            .unwrap();
    assert_eq!(stored, 0);

    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/status", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["cloud"], true);
    assert_eq!(v["connectors"]["twitch"]["state"], "off");
    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/polls", None).await;
    assert_eq!((s, code(&v)), (404, "not_in_cloud"));
}

#[tokio::test]
async fn overlays_edit_rotate_delete_and_refuse_bad_layouts() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (s, o) =
        w.web(&u, Method::POST, "/c/api/v1/overlays", Some(json!({ "name": "Chat", "preset": "chat_only" }))).await;
    assert_eq!(s, 201, "{o}");
    let id = o["id"].as_str().unwrap().to_string();
    assert_eq!(o["widgets"][0]["kind"], "chat");
    let path = format!("/c/api/v1/overlays/{id}");

    let mut edited = o.clone();
    edited["name"] = json!("Just chat");
    edited["token"] = json!("sneaky");
    let (s, v) = w.web(&u, Method::PUT, &path, Some(edited.clone())).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["name"], "Just chat");
    assert_eq!(v["url"], o["url"]);
    let stored = w.db.cloud_doc(&u.account.id, "overlay", &id).unwrap().unwrap();
    assert!(stored.json.unwrap().contains("\"token\":\"\""));

    let mut bad = edited.clone();
    bad["widgets"][0]["x"] = json!(5000);
    let (s, v) = w.web(&u, Method::PUT, &path, Some(bad)).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().contains("outside the canvas"));

    let (s, v) = w.web(&u, Method::POST, &format!("{path}/regenerate-url"), None).await;
    assert_eq!(s, 200);
    assert_ne!(v["url"], o["url"]);
    let old = o["url"].as_str().unwrap().trim_end_matches('/').rsplit('/').next().unwrap().to_string();
    assert!(overlay_for_token(&w.state, &old).is_none());

    let (s, _) = w.web(&u, Method::DELETE, &path, None).await;
    assert_eq!(s, 204);
    let (s, v) = w.web(&u, Method::GET, &path, None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));

    // Another account can't see or touch it.
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    let starter = list[0]["id"].as_str().unwrap().to_string();
    let other = w.cloud_user("b@example.com").await;
    let (s, _) = w.web(&other, Method::GET, &format!("/c/api/v1/overlays/{starter}"), None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn alert_rules_use_the_engine_checks() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, rules) = w.web(&u, Method::GET, "/c/api/v1/alert-rules", None).await;
    let mut r = rules[0].clone();
    r["id"] = json!("");
    r["name"] = json!("Big follows");
    let (s, made) = w.web(&u, Method::POST, "/c/api/v1/alert-rules", Some(r.clone())).await;
    assert_eq!(s, 201, "{made}");
    let id = made["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());
    let mut bad = made.clone();
    bad["kinds"] = json!(["chat_message"]);
    let (s, v) = w.web(&u, Method::PUT, &format!("/c/api/v1/alert-rules/{id}"), Some(bad)).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().contains("chat_message"));
    let (s, _) = w.web(&u, Method::DELETE, &format!("/c/api/v1/alert-rules/{id}"), None).await;
    assert_eq!(s, 204);
    let (s, _) = w.web(&u, Method::DELETE, &format!("/c/api/v1/alert-rules/{id}"), None).await;
    assert_eq!(s, 404);
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/alert-rules", Some(json!({ "name": "x" }))).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
}

#[tokio::test]
async fn designs_built_ins_branding_and_the_one_in_use() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/designs?kind=alert", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let builtin = list[0].clone();
    assert_eq!(builtin["id"], "builtin.molten");

    let (s, v) = w.web(&u, Method::PUT, "/c/api/v1/designs/builtin.molten", Some(builtin.clone())).await;
    assert_eq!((s, code(&v)), (409, "built_in"));
    let (s, v) = w.web(&u, Method::DELETE, "/c/api/v1/designs/builtin.molten", None).await;
    assert_eq!((s, code(&v)), (409, "built_in"));

    let (s, copy) = w.web(&u, Method::POST, "/c/api/v1/designs/builtin.molten/duplicate", None).await;
    assert_eq!(s, 201, "{copy}");
    assert_eq!(copy["branded"], false);
    assert_eq!(copy["built_in"], false);
    let cid = copy["id"].as_str().unwrap().to_string();

    let (s, v) = w.web(&u, Method::POST, &format!("/c/api/v1/designs/{cid}/use"), None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["alert_design"], cid.as_str());
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/designs/active-alert", None).await;
    assert_eq!(v["design_id"], cid.as_str());
    let (s, v) = w.web(&u, Method::DELETE, &format!("/c/api/v1/designs/{cid}"), None).await;
    assert_eq!((s, code(&v)), (409, "in_use"));
    let (s, v) =
        w.web(&u, Method::PUT, "/c/api/v1/designs/active-alert", Some(json!({ "design_id": "builtin.brb" }))).await;
    assert_eq!((s, code(&v)), (422, "invalid"));

    // A branded scene becomes an overlay that keeps its mark.
    let (s, used) = w.web(&u, Method::POST, "/c/api/v1/designs/builtin.brb/use", None).await;
    assert_eq!(s, 200, "{used}");
    let o = used["overlays"][0].clone();
    assert!(o["url"].as_str().unwrap().starts_with("http://overlays.test/o/"));
    let mut unbranded = o.clone();
    unbranded["widgets"].as_array_mut().unwrap().retain(|w| w["config"]["type"] != "brand");
    let (s, v) =
        w.web(&u, Method::PUT, &format!("/c/api/v1/overlays/{}", o["id"].as_str().unwrap()), Some(unbranded)).await;
    assert_eq!((s, code(&v)), (409, "branding"));

    // A copy can be edited; a branded flag on it is refused.
    let mut edit = copy.clone();
    edit["name"] = json!("Mine");
    let (s, v) = w.web(&u, Method::PUT, &format!("/c/api/v1/designs/{cid}"), Some(edit.clone())).await;
    assert_eq!(s, 200, "{v}");
    edit["branded"] = json!(true);
    let (s, _) = w.web(&u, Method::PUT, &format!("/c/api/v1/designs/{cid}"), Some(edit)).await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn the_app_syncs_with_versions_and_conflicts() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    // The cookie alone can't sync; the app's session can.
    let res = w.http.get(format!("{}/v1/cloud/docs", w.base)).header("cookie", &u.cookie).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let (s, all) = w.app(&u, Method::GET, "/v1/cloud/docs?since=0", None).await;
    assert_eq!(s, 200, "{all}");
    let version = all["version"].as_i64().unwrap();
    let starter = all["docs"].as_array().unwrap().iter().find(|d| d["kind"] == "overlay").unwrap().clone();
    let sv = starter["version"].as_i64().unwrap();

    // The web edits the overlay: the app's push from the old version is a conflict.
    let oid = starter["id"].as_str().unwrap();
    let (_, view) = w.web(&u, Method::GET, &format!("/c/api/v1/overlays/{oid}"), None).await;
    let mut web_edit = view.clone();
    web_edit["name"] = json!("From the web");
    w.web(&u, Method::PUT, &format!("/c/api/v1/overlays/{oid}"), Some(web_edit)).await;
    let mut body = starter["body"].clone();
    body["name"] = json!("From the PC");
    body["token"] = json!("0123456789abcdef0123456789abcdef");
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "overlay", "id": oid, "base_version": sv, "body": body } ] })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"].as_array().unwrap().len(), 0);
    assert_eq!(v["conflicts"][0]["server"]["body"]["name"], "From the web");

    // From the current version it goes in, without the PC's own token.
    let cur = v["conflicts"][0]["server"]["version"].as_i64().unwrap();
    let (_, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "overlay", "id": oid, "base_version": cur, "body": body } ] })),
        )
        .await;
    assert_eq!(v["applied"][0]["id"], oid);
    let stored = w.db.cloud_doc(&u.account.id, "overlay", oid).unwrap().unwrap().json.unwrap();
    assert!(!stored.contains("0123456789abcdef"));

    // New documents start at 0; deletes are tombstones the next pull carries.
    let rule = &all["docs"].as_array().unwrap().iter().find(|d| d["kind"] == "alert_rule").unwrap()["body"];
    let mut r = rule.clone();
    r["name"] = json!("From the PC");
    let (_, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "alert_rule", "id": "pc-rule", "base_version": 0, "body": r } ] })),
        )
        .await;
    let rv = v["applied"][0]["version"].as_i64().unwrap();
    let (_, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "alert_rule", "id": "pc-rule", "base_version": rv, "body": null } ] })),
        )
        .await;
    assert_eq!(v["applied"].as_array().unwrap().len(), 1);
    let (_, changes) = w.app(&u, Method::GET, &format!("/v1/cloud/docs?since={version}"), None).await;
    let gone = changes["docs"].as_array().unwrap().iter().find(|d| d["id"] == "pc-rule").unwrap();
    assert_eq!(gone["deleted"], true);
    assert_eq!(gone["body"], Value::Null);

    // A bad document changes nothing and names itself.
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [
                { "kind": "alert_rule", "id": "ok-rule", "base_version": 0, "body": r },
                { "kind": "alert_rule", "id": "bad", "base_version": 0, "body": { "name": "x" } } ] })),
        )
        .await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().starts_with("docs[1]"));
    assert!(w.db.cloud_doc(&u.account.id, "alert_rule", "ok-rule").unwrap().is_none());
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "design", "id": "builtin.x", "base_version": 0, "body": {} } ] })),
        )
        .await;
    assert_eq!((s, code(&v)), (422, "invalid"));

    let (s, list) = w.app(&u, Method::GET, "/v1/cloud/overlays", None).await;
    assert_eq!(s, 200);
    assert!(list[0]["url"].as_str().unwrap().starts_with("http://overlays.test/o/"));
}

#[tokio::test]
async fn limits_and_rates() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    // The Starter overlay plus 49 more is the most; the next is refused with the limit.
    for i in 1..MAX_OVERLAYS {
        w.db.cloud_write(&u.account.id, "overlay", &format!("o{i}"), Some("{}"), None, now_ms()).unwrap();
    }
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/overlays", Some(json!({ "name": "One more" }))).await;
    assert_eq!((s, code(&v)), (409, "cloud_limit"));
    assert!(v["error"]["message"].as_str().unwrap().contains("50"));

    let r = w.user("rate@example.com");
    let mut last = 0;
    for _ in 0..=WRITES_PER_MINUTE {
        last = w.web(&r, Method::POST, "/c/v1/cloud/enable", None).await.0;
    }
    assert_eq!(last, 429);
    let res = w
        .http
        .post(format!("{}/c/v1/cloud/enable", w.base))
        .header("cookie", &r.cookie)
        .header("x-csrf-token", &r.csrf)
        .send()
        .await
        .unwrap();
    assert!(res.headers().get("retry-after").is_some());
}

#[tokio::test]
async fn enabling_again_never_reseeds_and_a_deleted_overlays_url_never_comes_back() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    let oid = list[0]["id"].as_str().unwrap().to_string();
    let url = list[0]["url"].as_str().unwrap().to_string();
    let token = url.strip_prefix("http://overlays.test/o/").unwrap().trim_end_matches('/').to_string();
    let (s, _) = w.web(&u, Method::DELETE, &format!("/c/api/v1/overlays/{oid}"), None).await;
    assert_eq!(s, 204);
    assert!(overlay_for_token(&w.state, &token).is_none());
    // Turning it on again doesn't bring the Starter overlay back.
    w.web(&u, Method::POST, "/c/v1/cloud/enable", None).await;
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/overlays", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    // The PC brings the overlay back over the tombstone: it gets a new URL, never the old one.
    let tomb = w.db.cloud_doc(&u.account.id, "overlay", &oid).unwrap().unwrap();
    let body = json!({ "id": oid, "name": "Back", "token": "", "width": 1920, "height": 1080, "widgets": [], "updated_at": 0, "design": null });
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "overlay", "id": oid, "base_version": tomb.version, "body": body } ] })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["applied"].as_array().unwrap().len(), 1, "{v}");
    let (_, back) = w.web(&u, Method::GET, &format!("/c/api/v1/overlays/{oid}"), None).await;
    assert_ne!(back["url"].as_str().unwrap(), url);
    assert!(overlay_for_token(&w.state, &token).is_none());
}

#[tokio::test]
async fn a_push_keeps_a_built_in_scenes_mark_like_the_web_does() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/designs/builtin.brb/use", None).await;
    assert_eq!(s, 200, "{v}");
    let oid = v["overlays"][0]["id"].as_str().unwrap().to_string();
    let doc = w.db.cloud_doc(&u.account.id, "overlay", &oid).unwrap().unwrap();
    let mut body: Value = serde_json::from_str(doc.json.as_deref().unwrap()).unwrap();
    body["widgets"].as_array_mut().unwrap().retain(|l| l["config"]["type"] != "brand");
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [ { "kind": "overlay", "id": oid, "base_version": doc.version, "body": body } ] })),
        )
        .await;
    assert_eq!((s, code(&v)), (422, "invalid"), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("mark"), "{v}");
}
