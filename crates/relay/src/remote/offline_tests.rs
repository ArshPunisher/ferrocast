//! The phone's dashboard while the PC is off (M10) through the real `router()`, with the
//! cloud bot's fake Twitch and Kick clients (no network): every offline route and status,
//! the role rules, the moderation opt-in (403 without the scope), the shared rates, audit
//! lines with source `cloud`, the cloud ring's chat as the PC's events, and the live socket
//! (the `remote` frame, cloud chat lines, the handover when the PC's tunnel comes back).

use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{Millis, Platform};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::*;
use crate::billing::store::Synced;
use crate::bot::tests::{BOT_ID, FakeKick, FakeTwitch, KICK_BOT, KICK_STREAMER, STREAMER_ID, twitch_cfg, vault};
use crate::bot::twitch::{BOT_SCOPES, MOD_SCOPES};
use crate::bot::{KickBot, KickError, TwitchCloud, TwitchError};
use crate::{Config, Db, router};

const PEPPER: &[u8] = b"test pepper, at least sixteen bytes";
const PLAIN: &str = "Your streaming PC is off.";
const OWNER_NO_BANS: &str = "Your PC is off. Chat and history still work.";

struct World {
    base: String,
    db: Arc<Db>,
    http: reqwest::Client,
    state: AppState,
    tw: Arc<FakeTwitch>,
    kk: Arc<FakeKick>,
    _dir: tempfile::TempDir,
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or("")
    }
    fn message(&self) -> &str {
        self.json["error"]["message"].as_str().unwrap_or("")
    }
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: Some("kick-client".into()),
        kick_client_secret: Some("kick-secret".into()),
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let tw = Arc::new(FakeTwitch::default());
    let kk = Arc::new(FakeKick::default());
    let rt = crate::bot::Runtime::new(
        Some(twitch_cfg()),
        Some(tw.clone() as Arc<dyn TwitchCloud>),
        Some(kk.clone() as Arc<dyn KickBot>),
    );
    let state = AppState::new(cfg, db)
        .with_vault(Some(vault()))
        .with_remote(RemoteConfig::new(Some(&base), Some(PEPPER), None))
        .with_bot(rt);
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), state, tw, kk, _dir: dir }
}

impl World {
    /// A Pro install (the streaming PC); its install auth.
    fn pro(&self, id: &str, sub: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY_MS), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
        format!("{id}.secret")
    }

    async fn call(&self, method: Method, path: &str, auth: Option<&str>, body: Option<Value>) -> Answer {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let text = res.text().await.unwrap();
        Answer { status, headers, json: serde_json::from_str(&text).unwrap_or(Value::Null) }
    }

    async fn get(&self, path: &str, token: &str) -> Answer {
        self.call(Method::GET, path, Some(token), None).await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Answer {
        self.call(Method::POST, path, Some(token), Some(body)).await
    }

    async fn put(&self, path: &str, token: &str, body: Value) -> Answer {
        self.call(Method::PUT, path, Some(token), Some(body)).await
    }

    /// A paired phone's device token.
    async fn device(&self, auth: &str, role: &str) -> String {
        let a = self.post("/v1/pro/remote/codes", auth, json!({ "role": role, "name": format!("{role} phone") })).await;
        assert_eq!(a.status, 201, "{}", a.json);
        let code = a.json["code"].as_str().unwrap().to_string();
        let a = self.call(Method::POST, "/r/v1/pair", None, Some(json!({ "code": code }))).await;
        assert_eq!(a.status, 200, "{}", a.json);
        a.json["token"].as_str().unwrap().to_string()
    }

    /// The cloud bot connected on Twitch (with or without the moderator scopes) and on.
    fn twitch_ready(&self, sub: &str, install: &str, moderation: bool) {
        let mut scopes: Vec<&str> = BOT_SCOPES.to_vec();
        if moderation {
            scopes.extend_from_slice(MOD_SCOPES);
        }
        self.db
            .bot_set_account(sub, "twitch", (BOT_ID, "ashabot"), None, &scopes.join(" "), moderation, install, now_ms())
            .unwrap();
        self.db.bot_set_channel(sub, "twitch", STREAMER_ID, "asha_live").unwrap();
        crate::bot::seal_token(&self.state, sub, "twitch", "bot", "rt-bot");
        self.db.bot_set_enabled(sub, true, now_ms()).unwrap();
    }

    /// The cloud bot connected on Kick, with `scopes`.
    fn kick_ready(&self, sub: &str, install: &str, scopes: &str) {
        let moderation = scopes.contains("moderation:");
        self.db
            .bot_set_account(
                sub,
                "kick",
                (KICK_BOT, "ashabot"),
                Some((KICK_STREAMER, "asha")),
                scopes,
                moderation,
                install,
                now_ms(),
            )
            .unwrap();
        crate::bot::seal_token(&self.state, sub, "kick", "bot", "krt");
        self.db.bot_set_enabled(sub, true, now_ms()).unwrap();
    }

    /// A Twitch chat line reaching the cloud bot (EventSub).
    async fn chat(&self, sub: &str, id: &str, who: (&str, &str), text: &str) {
        let ch = self.db.bot_channel(sub, "twitch").unwrap().unwrap();
        let event = json!({
            "broadcaster_user_id": STREAMER_ID, "chatter_user_id": who.0, "chatter_user_login": who.1,
            "chatter_user_name": who.1, "message_id": id, "message": { "text": text, "fragments": [] },
            "badges": [], "color": "#FF0000",
        });
        let e = ferrocast_twitch::map::map("channel.chat.message", "delivery", &event, Millis(now_ms()));
        crate::bot::on_chat(&self.state, &ch, Platform::Twitch, e).await;
    }

    fn audit(&self, install: &str) -> Vec<(String, i64, String)> {
        self.db.remote_audit_list(install, 100).unwrap().into_iter().map(|a| (a.action, a.status, a.source)).collect()
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.replacen("http", "ws", 1))
    }
}

/// Two Pro PCs' worth of setup: PC `i1` (sub `s1`) with an owner and a mod phone.
async fn paired() -> (World, String, String, String) {
    let w = world().await;
    let auth = w.pro("i1", "s1");
    let owner = w.device(&auth, "owner").await;
    let moder = w.device(&auth, "mod").await;
    (w, auth, owner, moder)
}

// ── Pure parts ───────────────────────────────────────────────────────────────────────

#[test]
fn ids_platforms_and_reasons_are_checked() {
    assert!(offline::tests_id_ok("abc-123_X", 64));
    for bad in ["", "a b", "a/b", "../x", "é", "x?y"] {
        assert!(!offline::tests_id_ok(bad, 64), "{bad:?}");
    }
    assert!(!offline::tests_id_ok(&"9".repeat(65), 64));
    assert_eq!(offline::tests_platform("twitch"), Ok(Platform::Twitch));
    assert_eq!(offline::tests_platform("kick"), Ok(Platform::Kick));
    assert_eq!(offline::tests_platform("youtube"), Ok(Platform::Youtube));
    assert_eq!(offline::tests_platform("tiktok"), Err(400));
    assert_eq!(offline::tests_reason("  spam\u{202e}links\n "), Some("spamlinks".into()));
    assert_eq!(offline::tests_reason("   "), None);
    assert_eq!(offline::tests_reason(&"x".repeat(900)).map(|r| r.len()), Some(500));
    assert_eq!(
        crate::bot::moderate::scope_for(Platform::Kick, &crate::bot::moderate::Action::Delete { message_id: "m" }),
        "moderation:chat_message:manage"
    );
}

// ── Without the cloud bot ────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_the_cloud_bot_only_history_and_the_switch_answer() {
    let (w, _auth, owner, moder) = paired().await;
    let me = w.get("/r/v1/me", &owner).await;
    assert_eq!((me.json["pc_online"].clone(), me.json["cloud"].clone()), (json!(false), json!(false)));
    assert_eq!(me.json["cloud_paused"], false);
    for (m, p, b) in [
        (Method::GET, "/r/api/v1/chat?limit=50", None),
        (Method::POST, "/r/api/v1/mod/ban", Some(json!({ "user": { "platform": "twitch", "user_id": "9" } }))),
        (Method::POST, "/r/api/v1/mod/delete", Some(json!({ "platform": "twitch", "message_id": "m1" }))),
        (Method::GET, "/r/api/v1/status", None),
        (Method::GET, "/r/api/v1/settings", None),
    ] {
        let a = w.call(m.clone(), p, Some(&owner), b).await;
        assert_eq!((a.status, a.code()), (503, "pc_offline"), "{m} {p}");
        assert_eq!(a.message(), PLAIN, "{m} {p}: the plain message without the cloud bot");
    }
    // History is stored: it answers without the cloud bot.
    let a = w.get("/r/api/v1/pro/history", &owner).await;
    assert_eq!(a.status, 200, "{}", a.json);
    assert_eq!(a.headers["cache-control"], "no-store");
    // The switch, with nothing to switch on.
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "enabled": true })).await;
    assert_eq!((a.status, a.code()), (422, "no_platform"), "{}", a.json);
    // A mod: chat 503 like everyone, history and the switch refused by role first.
    let a = w.get("/r/api/v1/chat", &moder).await;
    assert_eq!((a.status, a.code()), (503, "pc_offline"));
    let a = w.get("/r/api/v1/pro/history", &moder).await;
    assert_eq!((a.status, a.code()), (403, "not_allowed_for_role"));
}

// ── Chat from the cloud ring ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_comes_from_the_cloud_ring_as_the_pcs_events() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", false);
    for i in 0..3 {
        w.chat("s1", &format!("tw-{i}"), ("77", "viewer77"), &format!("hello {i}")).await;
    }
    let me = w.get("/r/v1/me", &owner).await;
    assert_eq!((me.json["cloud"].clone(), me.json["cloud_paused"].clone()), (json!(true), json!(false)));
    let a = w.get("/r/api/v1/chat?limit=200", &owner).await;
    assert_eq!(a.status, 200, "{}", a.json);
    let list = a.json.as_array().unwrap();
    assert_eq!(list.len(), 3);
    let e = &list[2];
    // The dashboard's chat shape (API.md §3 Event), as the PC sends it.
    assert_eq!(e["platform"], "twitch");
    assert_eq!(e["source_id"], "tw-2");
    assert_eq!(e["origin"], "live");
    assert_eq!(e["kind"]["type"], "chat_message");
    assert_eq!(e["kind"]["actor"]["id"], "77");
    assert_eq!(e["kind"]["actor"]["login"], "viewer77");
    assert_eq!(e["kind"]["content"]["text"], "hello 2");
    assert!(e["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(e["seq"].is_u64());
    assert!(e.get("raw").is_none(), "the platform's payload never goes out");
    // Oldest first, the newest `limit`.
    let a = w.get("/r/api/v1/chat?limit=2", &moder).await;
    let texts: Vec<&str> =
        a.json.as_array().unwrap().iter().map(|e| e["kind"]["content"]["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["hello 1", "hello 2"]);
    // Everything else: what still works, for an owner without the moderation opt-in.
    for p in ["/r/api/v1/status", "/r/api/v1/mod/bans", "/r/api/v1/mod/held", "/r/api/v1/settings"] {
        let a = w.get(p, &owner).await;
        assert_eq!((a.status, a.code()), (503, "pc_offline"), "{p}");
        assert_eq!(a.message(), OWNER_NO_BANS, "{p}");
        assert!(a.json["error"]["hint"].as_str().unwrap().contains("need your PC"));
    }
    let a = w.post("/r/api/v1/mod/unban", &owner, json!({ "platform": "twitch", "user_id": "77" })).await;
    assert_eq!((a.status, a.message()), (503, OWNER_NO_BANS));
    assert!(w.audit("i1").iter().all(|(a, _, _)| a != "POST /mod/unban"), "a 503 changes nothing");
}

// ── Moderation ───────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn moderation_needs_the_opt_in_scope() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", false);
    for (p, b) in [
        (
            "/r/api/v1/mod/ban",
            json!({ "user": { "platform": "twitch", "user_id": "77", "login": "v" }, "everywhere": true }),
        ),
        ("/r/api/v1/mod/timeout", json!({ "platform": "twitch", "user_id": "77", "seconds": 600 })),
        ("/r/api/v1/mod/delete", json!({ "platform": "twitch", "message_id": "tw-1" })),
    ] {
        let a = w.post(p, &moder, b).await;
        assert_eq!((a.status, a.code()), (403, "needs_moderation_scope"), "{p}");
        assert!(a.json["error"]["hint"].as_str().unwrap().contains("Let the cloud bot moderate"), "{}", a.json);
    }
    assert!(w.tw.with(|t| t.mods.is_empty()), "nothing reached Twitch");
    // Refusals are audited as the cloud's, once per route a minute.
    let a = w.post("/r/api/v1/mod/ban", &owner, json!({ "user": { "platform": "twitch", "user_id": "77" } })).await;
    assert_eq!(a.status, 403);
    let lines = w.audit("i1");
    for action in ["POST /mod/ban", "POST /mod/timeout", "POST /mod/delete"] {
        assert!(lines.iter().any(|(a, s, src)| a == action && *s == 403 && src == "cloud"), "{action}: {lines:?}");
    }
    // A grant without the delete scope still can't delete.
    w.db.bot_set_account(
        "s1",
        "twitch",
        (BOT_ID, "ashabot"),
        None,
        "user:bot moderator:manage:banned_users",
        true,
        "i1",
        1,
    )
    .unwrap();
    let a = w.post("/r/api/v1/mod/delete", &moder, json!({ "platform": "twitch", "message_id": "tw-1" })).await;
    assert_eq!((a.status, a.code()), (403, "needs_moderation_scope"));
    let a =
        w.post("/r/api/v1/mod/timeout", &moder, json!({ "platform": "twitch", "user_id": "77", "seconds": 60 })).await;
    assert_eq!(a.status, 204, "{}", a.json);
}

#[tokio::test]
async fn twitch_moderation_goes_out_with_the_bot_accounts_own_token() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    let a = w
        .post(
            "/r/api/v1/mod/ban",
            &moder,
            json!({ "user": { "platform": "twitch", "user_id": "77", "login": "viewer77", "display_name": "V" },
                    "reason": " spam\u{0007} ", "everywhere": true }),
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.json);
    assert_eq!(a.json, json!({ "banned": ["twitch"], "failed": [] }));
    let a = w
        .post(
            "/r/api/v1/mod/timeout",
            &owner,
            json!({ "platform": "twitch", "user_id": "78", "seconds": 600, "reason": "calm" }),
        )
        .await;
    assert_eq!(a.status, 204, "{}", a.json);
    let a = w.post("/r/api/v1/mod/delete", &moder, json!({ "platform": "twitch", "message_id": "abc-123" })).await;
    assert_eq!(a.status, 204, "{}", a.json);
    let mods = w.tw.with(|t| t.mods.clone());
    let tok = "at-from-rt-bot".to_string();
    assert_eq!(
        mods,
        vec![
            ("ban".into(), tok.clone(), STREAMER_ID.into(), BOT_ID.into(), "77".into(), None, Some("spam".into())),
            (
                "timeout".into(),
                tok.clone(),
                STREAMER_ID.into(),
                BOT_ID.into(),
                "78".into(),
                Some(600),
                Some("calm".into())
            ),
            ("delete".into(), tok, STREAMER_ID.into(), BOT_ID.into(), "abc-123".into(), None, None),
        ]
    );
    assert_eq!(w.tw.with(|t| t.refreshes), 1, "the user token is refreshed once and kept");
    // Audited like online changes, by the device, from the cloud.
    let lines = w.audit("i1");
    assert!(lines.contains(&("POST /mod/ban".into(), 200, "cloud".into())), "{lines:?}");
    assert!(lines.contains(&("POST /mod/timeout".into(), 204, "cloud".into())));
    assert!(lines.contains(&("POST /mod/delete".into(), 204, "cloud".into())));
    let listed = w.call(Method::GET, "/v1/pro/remote/devices", Some("i1.secret"), None).await;
    let first = &listed.json["audit"][0];
    assert_eq!(first["source"], "cloud", "{first}");
    assert_eq!(first["device_name"], "mod phone");
    // Twitch's refusals keep their meaning.
    for (fail, status, code) in [
        (TwitchError::Forbidden("not a mod".into()), 403, "bot_not_moderator"),
        (TwitchError::BadRequest("user is already banned".into()), 422, "platform_refused"),
        (TwitchError::NotFound, 404, "message_not_found"),
        (TwitchError::Conflict, 409, "platform_busy"),
        (TwitchError::RateLimited { retry_after_s: 7 }, 429, "platform_rate"),
        (TwitchError::Unavailable("timeout".into()), 502, "platform_unreachable"),
    ] {
        w.tw.with(|t| t.mod_fails = Some(fail.clone()));
        let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": "m9" })).await;
        assert_eq!((a.status, a.code()), (status, code), "{fail:?}: {}", a.json);
        assert!(a.json["error"]["hint"].as_str().is_some_and(|h| !h.is_empty()), "{code} has a hint");
        if status == 429 {
            assert_eq!(a.headers["retry-after"], "7");
        }
        if code == "bot_not_moderator" {
            assert!(a.json["error"]["hint"].as_str().unwrap().contains("/mod ashabot"));
        }
    }
    w.tw.with(|t| t.mod_fails = None);
    // Twitch refusing the sign-in itself marks the channel signed out.
    w.state.bot.forget_twitch_user("s1");
    w.tw.with(|t| t.refresh_fails = true);
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": "m9" })).await;
    assert_eq!((a.status, a.code()), (409, "cloud_signed_out"));
    assert_eq!(w.db.bot_channel("s1", "twitch").unwrap().unwrap().error_code(), Some("twitch_signed_out"));
}

#[tokio::test]
async fn kick_moderation_uses_its_moderation_api() {
    let (w, _auth, owner, _moder) = paired().await;
    w.kick_ready("s1", "i1", "user:read chat:write moderation:ban moderation:chat_message:manage");
    let a =
        w.post("/r/api/v1/mod/timeout", &owner, json!({ "platform": "kick", "user_id": "4242", "seconds": 90 })).await;
    assert_eq!(a.status, 204, "{}", a.json);
    let a = w
        .post(
            "/r/api/v1/mod/ban",
            &owner,
            json!({ "user": { "platform": "kick", "user_id": "4242" }, "reason": "bye" }),
        )
        .await;
    assert_eq!((a.status, a.json.clone()), (200, json!({ "banned": ["kick"], "failed": [] })));
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "kick", "message_id": "k-msg-1" })).await;
    assert_eq!(a.status, 204, "{}", a.json);
    let mods = w.kk.with(|k| k.mods.clone());
    assert_eq!(mods.len(), 3);
    assert_eq!(
        (mods[0].0.as_str(), mods[0].2.as_str(), mods[0].3.as_str(), mods[0].4),
        ("timeout", KICK_STREAMER, "4242", Some(2))
    );
    assert_eq!((mods[1].0.as_str(), mods[1].4, mods[1].5.as_deref()), ("ban", None, Some("bye")));
    assert_eq!((mods[2].0.as_str(), mods[2].3.as_str()), ("delete", "k-msg-1"));
    assert!(mods.iter().all(|m| m.1 == mods[0].1 && m.1.starts_with("kat-")), "one Kick token, refreshed once");
    // Kick takes 7 days at most.
    let a = w
        .post("/r/api/v1/mod/timeout", &owner, json!({ "platform": "kick", "user_id": "4242", "seconds": 700_000 }))
        .await;
    assert_eq!((a.status, a.code()), (400, "invalid"));
    for (fail, status, code) in [
        (KickError::Forbidden("no".into()), 403, "bot_not_moderator"),
        (KickError::Dropped("can't ban the owner".into()), 422, "platform_refused"),
        (KickError::RateLimited { retry_after_s: 30 }, 429, "platform_rate"),
        (KickError::Unavailable("status 404".into()), 404, "message_not_found"),
        (KickError::Unavailable("status 500".into()), 502, "platform_unreachable"),
    ] {
        w.kk.with(|k| k.mod_fails = Some(fail.clone()));
        let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "kick", "message_id": "k2" })).await;
        assert_eq!((a.status, a.code()), (status, code), "{fail:?}");
    }
    // An older opt-in (ban only, before M10) can't delete on Kick.
    w.kk.with(|k| k.mod_fails = None);
    w.kick_ready("s1", "i1", "user:read chat:write moderation:ban");
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "kick", "message_id": "k2" })).await;
    assert_eq!((a.status, a.code()), (403, "needs_moderation_scope"));
}

#[tokio::test]
async fn bad_bodies_and_missing_channels_say_what_is_wrong() {
    let (w, _auth, owner, _moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    let raw = w
        .http
        .post(format!("{}/r/api/v1/mod/delete", w.base))
        .bearer_auth(&owner)
        .header("content-type", "text/plain")
        .body("platform=twitch")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 415);
    for (p, b, status, code) in [
        ("/r/api/v1/mod/delete", json!({ "platform": "twitch" }), 400, "invalid"),
        ("/r/api/v1/mod/delete", json!({ "platform": "twitch", "message_id": "../x" }), 400, "invalid"),
        ("/r/api/v1/mod/timeout", json!({ "platform": "twitch", "user_id": "1", "seconds": 0 }), 400, "invalid"),
        (
            "/r/api/v1/mod/timeout",
            json!({ "platform": "twitch", "user_id": "1", "seconds": 1_209_601 }),
            400,
            "invalid",
        ),
        (
            "/r/api/v1/mod/timeout",
            json!({ "platform": "youtube", "user_id": "1", "seconds": 60 }),
            503,
            "platform_unavailable",
        ),
        ("/r/api/v1/mod/ban", json!({ "user": { "platform": "tiktok", "user_id": "1" } }), 400, "invalid"),
        ("/r/api/v1/mod/ban", json!({ "user": { "platform": "kick", "user_id": "1" } }), 422, "cloud_not_connected"),
    ] {
        let a = w.post(p, &owner, b.clone()).await;
        assert_eq!((a.status, a.code()), (status, code), "{p} {b}: {}", a.json);
        assert!(!a.message().is_empty());
    }
    assert!(w.tw.with(|t| t.mods.is_empty()));
}

#[tokio::test]
async fn moderation_shares_the_channels_twenty_a_minute_and_the_devices_rate() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    w.db.try_take("s1", "bot_messages_twitch", crate::quota::Period::Minute, 20, crate::bot::MESSAGES_PER_MINUTE)
        .unwrap();
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": "m1" })).await;
    assert_eq!((a.status, a.code()), (429, "cloud_rate"), "{}", a.json);
    assert!(a.headers.contains_key("retry-after") && a.headers.contains_key("x-quota-reset"));
    assert!(w.tw.with(|t| t.mods.is_empty()));
    // The device's 20 a second counts offline answers too.
    let mut limited = false;
    for _ in 0..30 {
        if w.get("/r/api/v1/chat", &moder).await.status == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "20 requests a second per device");
}

#[tokio::test]
async fn a_fresh_token_refused_too_marks_the_channel_signed_out() {
    let (w, _auth, owner, _moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    w.tw.with(|t| t.mod_fails = Some(TwitchError::Unauthorized));
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": "m9" })).await;
    assert_eq!((a.status, a.code()), (409, "cloud_signed_out"), "{}", a.json);
    assert_eq!(w.tw.with(|t| t.refreshes), 2, "a refused token is refreshed once more");
    assert_eq!(
        w.db.bot_channel("s1", "twitch").unwrap().unwrap().error_code(),
        Some("twitch_signed_out"),
        "the PC's Bot page shows the sign-in is gone"
    );
}

#[tokio::test]
async fn phones_moderating_at_once_share_one_twitch_refresh() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    w.tw.with(|t| t.refresh_delay_ms = 150);
    let calls = (0..4).map(|i| {
        let token = if i % 2 == 0 { owner.clone() } else { moder.clone() };
        let w = &w;
        async move {
            w.post(
                "/r/api/v1/mod/timeout",
                &token,
                json!({ "platform": "twitch", "user_id": format!("{}", 70 + i), "seconds": 60 }),
            )
            .await
            .status
        }
    });
    let statuses = futures_util::future::join_all(calls).await;
    assert_eq!(statuses, [204, 204, 204, 204]);
    assert_eq!(w.tw.with(|t| t.refreshes), 1, "one refresh, the others wait for it under the lock");
}

#[tokio::test]
async fn already_banned_or_timed_out_is_a_success() {
    let (w, _auth, owner, _moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    w.tw.with(|t| {
        t.mod_fails = Some(TwitchError::BadRequest("The user specified in the user_id field is already banned.".into()))
    });
    let a = w.post("/r/api/v1/mod/ban", &owner, json!({ "user": { "platform": "twitch", "user_id": "77" } })).await;
    assert_eq!((a.status, a.json.clone()), (200, json!({ "banned": ["twitch"], "failed": [] })));
    let a =
        w.post("/r/api/v1/mod/timeout", &owner, json!({ "platform": "twitch", "user_id": "77", "seconds": 60 })).await;
    assert_eq!(a.status, 204, "{}", a.json);
    // Other refusals still are refusals.
    w.tw.with(|t| t.mod_fails = Some(TwitchError::BadRequest("The user can't be banned.".into())));
    let a = w.post("/r/api/v1/mod/ban", &owner, json!({ "user": { "platform": "twitch", "user_id": "77" } })).await;
    assert_eq!((a.status, a.code()), (422, "platform_refused"));
    // Kick says it in its own words.
    w.kick_ready("s1", "i1", "user:read chat:write moderation:ban moderation:chat_message:manage");
    w.kk.with(|k| k.mod_fails = Some(KickError::Dropped("User is already banned".into())));
    let a = w.post("/r/api/v1/mod/ban", &owner, json!({ "user": { "platform": "kick", "user_id": "4242" } })).await;
    assert_eq!(a.status, 200, "{}", a.json);
    // A delete of a gone message isn't "already done".
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "kick", "message_id": "k1" })).await;
    assert_eq!((a.status, a.code()), (422, "platform_refused"));
}

#[tokio::test]
async fn bans_and_timeouts_are_queued_for_the_pc_to_record() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    let a = w
        .post(
            "/r/api/v1/mod/ban",
            &moder,
            json!({ "user": { "platform": "twitch", "user_id": "77", "login": "viewer77", "display_name": "V\u{202e}77" },
                    "reason": "spam" }),
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.json);
    let a =
        w.post("/r/api/v1/mod/timeout", &owner, json!({ "platform": "twitch", "user_id": "78", "seconds": 600 })).await;
    assert_eq!(a.status, 204);
    let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": "abc" })).await;
    assert_eq!(a.status, 204);
    // A refused ban isn't queued.
    w.tw.with(|t| t.mod_fails = Some(TwitchError::Forbidden("no".into())));
    let a = w.post("/r/api/v1/mod/ban", &owner, json!({ "user": { "platform": "twitch", "user_id": "79" } })).await;
    assert_eq!(a.status, 403);
    let items: Vec<Value> =
        w.db.pending("i1", 0, 100)
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == "cloud_moderation")
            .map(|i| serde_json::from_str(&i.payload).unwrap())
            .collect();
    assert_eq!(items.len(), 2, "the ban and the timeout, not the delete or the refusal: {items:?}");
    let ban = &items[0];
    assert_eq!(
        (&ban["platform"], &ban["action"], &ban["target_id"], &ban["target_login"], &ban["target_name"]),
        (&json!("twitch"), &json!("ban"), &json!("77"), &json!("viewer77"), &json!("V77"))
    );
    assert_eq!(
        (&ban["reason"], &ban["by_device"], &ban["by_role"]),
        (&json!("spam"), &json!("mod phone"), &json!("mod"))
    );
    assert!(ban["at"].as_i64().is_some_and(|t| t > 0));
    let to = &items[1];
    assert_eq!(
        (&to["action"], &to["target_id"], &to["duration"], &to["by_role"]),
        (&json!("timeout"), &json!("78"), &json!(600), &json!("owner"))
    );
}

#[tokio::test]
async fn the_capability_and_the_words_follow_role_and_opt_in() {
    let (w, _auth, owner, moder) = paired().await;
    // Connected without the opt-in: chat only.
    w.twitch_ready("s1", "i1", false);
    let me = w.get("/r/v1/me", &moder).await;
    assert_eq!(me.json["cloud_moderation"], json!({ "twitch": false, "kick": false, "youtube": false }));
    assert_eq!(
        (me.json["history"].clone(), w.get("/r/v1/me", &owner).await.json["history"].clone()),
        (json!(false), json!(true))
    );
    let a = w.get("/r/api/v1/status", &moder).await;
    assert_eq!(a.message(), "Your PC is off. Chat still works.");
    // With it: bans too.
    w.twitch_ready("s1", "i1", true);
    let me = w.get("/r/v1/me", &moder).await;
    assert_eq!(me.json["cloud_moderation"], json!({ "twitch": true, "kick": false, "youtube": false }));
    assert_eq!(w.get("/r/api/v1/status", &moder).await.message(), "Your PC is off. Chat and bans still work.");
    assert_eq!(w.get("/r/api/v1/status", &owner).await.message(), "Your PC is off. Chat, bans and history still work.");
    // A partial grant (bans but not deletes) isn't "can moderate".
    w.kick_ready("s1", "i1", "user:read chat:write moderation:ban");
    assert_eq!(w.get("/r/v1/me", &owner).await.json["cloud_moderation"]["kick"], false);
    // Switched off: nothing works on the cloud's side.
    w.db.bot_set_enabled("s1", false, now_ms()).unwrap();
    let me = w.get("/r/v1/me", &owner).await;
    assert_eq!(me.json["cloud_moderation"], json!({ "twitch": false, "kick": false, "youtube": false }));
    assert_eq!(w.get("/r/api/v1/status", &owner).await.message(), PLAIN);
}

#[tokio::test]
async fn validation_names_the_field_and_cloud_ids_are_refused() {
    let (w, _auth, owner, _moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    for (p, b, want) in [
        ("/r/api/v1/mod/delete", json!({ "platform": "twitch" }), "message_id: missing."),
        ("/r/api/v1/mod/delete", json!({ "platform": 7, "message_id": "m" }), "platform: must be text."),
        (
            "/r/api/v1/mod/timeout",
            json!({ "platform": "twitch", "user_id": "1", "seconds": "x" }),
            "seconds: from 1 second to 14 days.",
        ),
        (
            "/r/api/v1/mod/timeout",
            json!({ "platform": "twitch", "user_id": "1", "seconds": 5, "reason": 3 }),
            "reason: must be text.",
        ),
        ("/r/api/v1/mod/ban", json!({ "user": "77" }), "user: must be an object."),
        ("/r/api/v1/mod/ban", json!({}), "user: missing."),
        ("/r/api/v1/mod/ban", json!({ "user": { "platform": "twitch" } }), "user.user_id: missing."),
        ("/r/api/v1/mod/ban", json!([1, 2]), "The body must be a JSON object."),
    ] {
        let a = w.post(p, &owner, b.clone()).await;
        assert_eq!((a.status, a.code(), a.message()), (400, "invalid", want), "{p} {b}");
        assert!(a.message().chars().count() <= 200);
    }
    for id in ["cl-twitch-abc", "cl_0123456789ab"] {
        let a = w.post("/r/api/v1/mod/delete", &owner, json!({ "platform": "twitch", "message_id": id })).await;
        assert_eq!((a.status, a.code()), (422, "invalid_message"), "{id}");
        assert!(a.json["error"]["hint"].as_str().unwrap().contains("source_id"));
    }
    assert!(w.tw.with(|t| t.mods.is_empty()));
}

#[tokio::test]
async fn a_phone_paired_under_another_subscription_pairs_again() {
    let (w, _auth, owner, _moder) = paired().await;
    assert_eq!(w.get("/r/v1/me", &owner).await.status, 200);
    // Pro restored onto a new subscription for the same PC.
    w.db.add_pro_sub("s2", "monthly", "e2", "created", "i1").unwrap();
    let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY_MS), cancel_at_end: None };
    w.db.sync_pro_sub(None, "s2", &s).unwrap();
    assert_eq!(w.db.pro_sub_for_install("i1").unwrap().unwrap().id, "s2");
    for p in ["/r/v1/me", "/r/api/v1/pro/history", "/r/api/v1/chat"] {
        let a = w.get(p, &owner).await;
        assert_eq!((a.status, a.code()), (401, "device_moved"), "{p}");
        assert!(a.json["error"]["hint"].as_str().unwrap().contains("Pair this phone again"));
    }
}

// ── History and the switch ───────────────────────────────────────────────────────────

#[tokio::test]
async fn history_comes_from_the_stored_streams() {
    let (w, auth, owner, _moder) = paired().await;
    let started = now_ms() - 3 * 3_600_000;
    let a = w
        .post(
            "/v1/pro/history/streams",
            &auth,
            json!({ "platform": "twitch", "key": format!("twitch:{started}"), "started_at": started,
                    "ended_at": started + 7_200_000, "peak_viewers": 40, "avg_viewers": 20 }),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.json);
    let a = w.get("/r/api/v1/pro/history", &owner).await;
    assert_eq!(a.status, 200, "{}", a.json);
    assert_eq!(a.json["streams"].as_array().map(Vec::len), Some(1));
    assert_eq!(a.json["streams"][0]["peak_viewers"], 40);
    assert!(a.json["weekly"].is_array());
    let a = w.get("/r/api/v1/pro/history?from=10&to=5", &owner).await;
    assert_eq!((a.status, a.code()), (422, "invalid_range"));
    // Export stays on the PC.
    let a = w.get("/r/api/v1/pro/history/export.csv", &owner).await;
    assert_eq!((a.status, a.code()), (403, "not_allowed_for_role"));
    assert!(w.audit("i1").iter().all(|(a, _, _)| !a.contains("history")), "reads aren't audited");
}

#[tokio::test]
async fn the_cloud_bot_switch_answers_offline() {
    let (w, _auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "enabled": false })).await;
    assert_eq!(a.status, 200, "{}", a.json);
    assert_eq!(a.json["enabled"], false);
    assert_eq!(a.json["pro_available"], true);
    assert!(a.json["sync"]["state"].is_string());
    assert!(w.audit("i1").contains(&("PUT /pro/bot/enabled".into(), 200, "cloud".into())));
    let me = w.get("/r/v1/me", &owner).await;
    assert_eq!((me.json["cloud"].clone(), me.json["cloud_paused"].clone()), (json!(false), json!(true)));
    let a = w.get("/r/api/v1/chat", &owner).await;
    assert_eq!((a.status, a.message()), (503, PLAIN), "paused: chat needs the PC");
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "enabled": true })).await;
    assert_eq!((a.status, a.json["enabled"].clone()), (200, json!(true)));
    assert_eq!(w.get("/r/api/v1/chat", &owner).await.status, 200);
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "on": true })).await;
    assert_eq!((a.status, a.code()), (400, "invalid"));
    let a = w.put("/r/api/v1/pro/bot/enabled", &moder, json!({ "enabled": false })).await;
    assert_eq!((a.status, a.code()), (403, "not_allowed_for_role"));
}

// ── The live socket ──────────────────────────────────────────────────────────────────

type PhoneWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Phone = Arc<AsyncMutex<PhoneWs>>;

async fn phone(w: &World, token: &str) -> Phone {
    let a = w.post("/r/v1/ticket", token, json!({ "for": "live" })).await;
    assert_eq!(a.status, 200, "{}", a.json);
    let url = w.ws_url(&format!("/r/api/v1/live?ticket={}", a.json["ticket"].as_str().unwrap()));
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    Arc::new(AsyncMutex::new(ws))
}

async fn next(p: &Phone) -> Value {
    let mut rx = p.lock().await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.next()).await.expect("a frame") {
            Some(Ok(WsMsg::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(WsMsg::Close(c))) => panic!("closed: {c:?}"),
            Some(Ok(_)) => {}
            other => panic!("socket ended: {other:?}"),
        }
    }
}

async fn quiet(p: &Phone) {
    let mut rx = p.lock().await;
    let got = tokio::time::timeout(Duration::from_millis(250), rx.next()).await;
    assert!(got.is_err(), "expected nothing, got {got:?}");
}

/// A fake PC on the tunnel: what it got, and a way to send frames as the PC.
struct Pc {
    frames: mpsc::UnboundedReceiver<Value>,
    out: mpsc::UnboundedSender<WsMsg>,
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
}

async fn connect_pc(w: &World, auth: &str) -> Pc {
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
                    Some(Ok(WsMsg::Text(t))) => { let _ = seen_tx.send(serde_json::from_str::<Value>(&t).unwrap()); }
                    Some(Ok(_)) => {}
                    _ => return,
                }
            }
        }
    });
    Pc { frames: seen_rx, out: out_tx }
}

async fn tunnel_is(w: &World, install: &str, up: bool) {
    for _ in 0..200 {
        if w.state.remote.pc_online(install) == up {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the tunnel never became {up}");
}

#[tokio::test]
async fn the_live_socket_streams_cloud_chat_while_the_pc_is_off_and_hands_over() {
    let (w, auth, owner, moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    let o = phone(&w, &owner).await;
    let m = phone(&w, &moder).await;
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!(f["type"], "remote");
        assert_eq!((f["data"]["pc_online"].clone(), f["data"]["cloud"].clone()), (json!(false), json!(true)));
        assert_eq!(f["data"]["cloud_moderation"], json!({ "twitch": true, "kick": false, "youtube": false }));
        // Fetch the cloud's chat now (a line may have come while the socket opened).
        assert_eq!(next(p).await["type"], "resync");
    }
    w.chat("s1", "tw-1", ("77", "viewer77"), "hi from the cloud").await;
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!((f["v"].clone(), f["type"].clone()), (json!(1), json!("event")));
        assert_eq!(f["data"]["kind"]["type"], "chat_message");
        assert_eq!(f["data"]["kind"]["content"]["text"], "hi from the cloud");
        assert_eq!(f["data"]["seq"], 0, "the phone's resume point stays the PC's");
        assert!(f["data"].get("raw").is_none());
    }
    // A delete from a phone greys the line out on every phone.
    let a = w.post("/r/api/v1/mod/delete", &moder, json!({ "platform": "twitch", "message_id": "tw-1" })).await;
    assert_eq!(a.status, 204);
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!(
            (f["data"]["kind"]["type"].clone(), f["data"]["kind"]["message_id"].clone()),
            (json!("message_deleted"), json!("tw-1"))
        );
    }
    // Pausing the cloud bot: the phones hear chat stopped.
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "enabled": false })).await;
    assert_eq!(a.status, 200);
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!(f["type"], "remote");
        assert_eq!((f["data"]["cloud"].clone(), f["data"]["cloud_paused"].clone()), (json!(false), json!(true)));
    }
    let a = w.put("/r/api/v1/pro/bot/enabled", &owner, json!({ "enabled": true })).await;
    assert_eq!(a.status, 200);
    for p in [&o, &m] {
        assert_eq!(next(p).await["data"]["cloud"], true);
        assert_eq!(next(p).await["type"], "resync");
    }
    // The PC comes back: it takes the sockets over (asked for their frames from 0, so it
    // sends `hello` and `resync` and the phones refetch from it); cloud lines stop.
    let mut pc = connect_pc(&w, &auth).await;
    tunnel_is(&w, "i1", true).await;
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!((f["type"].clone(), f["data"]["pc_online"].clone()), (json!("remote"), json!(true)));
    }
    let opens = [pc.next_of("live_open").await, pc.next_of("live_open").await];
    assert!(opens.iter().all(|f| f["since"] == 0), "{opens:?}");
    w.chat("s1", "tw-2", ("77", "viewer77"), "the PC sends this one").await;
    quiet(&o).await;
    quiet(&m).await;
    pc.out
        .send(WsMsg::text(
            json!({ "t": "live", "data": json!({ "v": 1, "type": "hello", "data": { "seq": 41 } }).to_string() })
                .to_string(),
        ))
        .unwrap();
    assert_eq!(next(&o).await["data"]["seq"], 41);
    assert_eq!(next(&m).await["data"]["seq"], 41);
    // And goes off again: the cloud stands in.
    pc.out.send(WsMsg::Close(None)).unwrap();
    tunnel_is(&w, "i1", false).await;
    for p in [&o, &m] {
        let f = next(p).await;
        assert_eq!((f["data"]["pc_online"].clone(), f["data"]["cloud"].clone()), (json!(false), json!(true)));
        assert_eq!(next(p).await["type"], "resync");
    }
    w.chat("s1", "tw-3", ("77", "viewer77"), "cloud again").await;
    assert_eq!(next(&o).await["data"]["kind"]["content"]["text"], "cloud again");
    // Another subscription's chat never reaches these phones.
    let _ = w.pro("i2", "s2");
    w.db.bot_set_account("s2", "twitch", ("600", "otherbot"), None, "", false, "i2", 1).unwrap();
    w.db.bot_set_channel("s2", "twitch", "601", "other").unwrap();
    w.db.bot_set_enabled("s2", true, 1).unwrap();
    let ch = w.db.bot_channel("s2", "twitch").unwrap().unwrap();
    let event = json!({ "broadcaster_user_id": "601", "chatter_user_id": "5", "chatter_user_login": "x",
                        "chatter_user_name": "x", "message_id": "o-1", "message": { "text": "not yours", "fragments": [] },
                        "badges": [] });
    let e = ferrocast_twitch::map::map("channel.chat.message", "d", &event, Millis(now_ms()));
    crate::bot::on_chat(&w.state, &ch, Platform::Twitch, e).await;
    quiet(&o).await;
}

#[tokio::test]
async fn phones_are_indexed_by_subscription_and_the_index_cleans_itself() {
    let (w, _auth, owner, _moder) = paired().await;
    w.twitch_ready("s1", "i1", true);
    assert!(!proxy::tests_has_phones(&w.state, "s1"));
    let o = phone(&w, &owner).await;
    assert_eq!(next(&o).await["type"], "remote");
    assert!(proxy::tests_has_phones(&w.state, "s1"));
    assert!(!proxy::tests_has_phones(&w.state, "s2"), "another subscription has none");
    assert_eq!(proxy::tests_indexed_installs(&w.state, "s1"), 1);
    o.lock().await.close(None).await.unwrap();
    for _ in 0..100 {
        if proxy::tests_phone_count(&w.state, "i1") == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!proxy::tests_has_phones(&w.state, "s1"), "closed: no phones");
    assert_eq!(proxy::tests_indexed_installs(&w.state, "s1"), 0, "the stale entry went with the look");
}

#[test]
fn migration_11_adds_the_audit_source_and_old_rows_are_the_pcs() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    db.c()
        .execute(
            "INSERT INTO remote_audit(sub_id, install_id, device_id, device_name, action, status, at) VALUES ('s', 'i', 'd', 'n', 'POST /mod/ban', 200, 1)",
            [],
        )
        .unwrap();
    db.remote_audit_from("s", "i", "d", "n", "POST /mod/ban", 204, 2, "cloud").unwrap();
    let lines = db.remote_audit_list("i", 10).unwrap();
    assert_eq!(lines.iter().map(|l| l.source.as_str()).collect::<Vec<_>>(), ["cloud", "pc"]);
}
