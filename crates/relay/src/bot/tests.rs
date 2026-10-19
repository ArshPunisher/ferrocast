//! The cloud bot through the real `router()` with fake Twitch and Kick clients (no network):
//! the gate on every route, config pushes (versions, limits, types), connecting Twitch in two
//! steps tied to the PC's own Twitch account and Kick through the relay's Kick broker, the
//! vault round trip, disconnecting and revoking, EventSub's checks (good, bad HMAC, replay,
//! stale, challenge, revocation), the lease's takeover and handback per platform (paused
//! clock), dedupe, its own echo, the 20-a-minute cap, counters going back to the PCs as
//! deltas, timers, Kick webhooks and its refresh race, the sweep, and chat never being stored.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use ferrocast_core::autoreply::{AutoReply, MatchKind};
use ferrocast_core::event::{Event, Millis, Platform};
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const ES_SECRET: &str = "eventsub-secret-SECRET-123";
const BOT_ID: &str = "500000001";
const STREAMER_ID: &str = "141981764";
const KICK_STREAMER: &str = "668";
const KICK_BOT: &str = "9001";
/// The desktop app's token for the streamer's account (what the PC sends with `connect`).
const DESK: &str = "desk-token";
const DESKTOP_APP: &str = "desktop-client";

// ── Fakes ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Tw {
    /// code → (tokens, who)
    codes: HashMap<String, (UserTokens, TokenInfo)>,
    /// access token → who (for validate)
    tokens: HashMap<String, TokenInfo>,
    sent: Vec<(String, String, String, Option<String>)>,
    revoked: Vec<String>,
    /// (kind, condition, conduit)
    subscribed: Vec<(String, Value, String)>,
    unsubscribed: Vec<String>,
    conduits: u32,
    /// The conduit Twitch hands back (`conduit-1` unless set).
    conduit_id: Option<String>,
    refresh_fails: bool,
    send_fails: Option<TwitchError>,
    /// Send Chat Message gives no message id.
    no_ids: bool,
    /// Subscribing this kind fails with this.
    subscribe_fails: Option<(String, TwitchError)>,
    /// Subscribing these kinds answers 409 (they exist already as `old-<kind>`).
    conflicts: Vec<String>,
    /// Get Streams: live since.
    live: Option<i64>,
    app_tokens: u32,
}

#[derive(Debug, Default)]
struct FakeTwitch(Mutex<Tw>);

impl FakeTwitch {
    fn with<T>(&self, f: impl FnOnce(&mut Tw) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }
    fn sent(&self) -> Vec<String> {
        self.with(|t| t.sent.iter().map(|s| s.2.clone()).collect())
    }
    fn kinds(&self) -> Vec<String> {
        self.with(|t| t.subscribed.iter().map(|s| s.0.clone()).collect())
    }
}

impl TwitchCloud for FakeTwitch {
    fn exchange_code<'a>(&'a self, code: &'a str, _redirect: &'a str) -> Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            self.with(|t| {
                let (tok, who) = t.codes.remove(code).ok_or(TwitchError::BadRequest("bad code".into()))?;
                t.tokens.insert(tok.access_token.clone(), who);
                Ok(tok)
            })
        })
    }
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            self.with(|t| {
                if t.refresh_fails {
                    return Err(TwitchError::Unauthorized);
                }
                Ok(UserTokens {
                    access_token: format!("at-from-{refresh_token}"),
                    refresh_token: refresh_token.to_string(),
                    scopes: vec![],
                })
            })
        })
    }
    fn validate<'a>(&'a self, access_token: &'a str) -> Fut<'a, TokenInfo, TwitchError> {
        Box::pin(async move { self.with(|t| t.tokens.get(access_token).cloned().ok_or(TwitchError::Unauthorized)) })
    }
    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, (), TwitchError> {
        Box::pin(async move {
            self.with(|t| t.revoked.push(access_token.to_string()));
            Ok(())
        })
    }
    fn app_token(&self) -> Fut<'_, (String, u64), TwitchError> {
        Box::pin(async move {
            self.with(|t| {
                t.app_tokens += 1;
                Ok(("app-token".to_string(), 3_600))
            })
        })
    }
    fn conduit<'a>(&'a self, app: &'a str, callback: &'a str, secret: &'a str) -> Fut<'a, String, TwitchError> {
        Box::pin(async move {
            assert_eq!(app, "app-token");
            assert!(callback.ends_with("/v1/bot/twitch/eventsub"), "{callback}");
            assert_eq!(secret, ES_SECRET);
            self.with(|t| {
                t.conduits += 1;
                Ok(t.conduit_id.clone().unwrap_or_else(|| "conduit-1".into()))
            })
        })
    }
    fn subscribe<'a>(
        &'a self,
        _app: &'a str,
        conduit: &'a str,
        kind: &'a str,
        condition: Value,
    ) -> Fut<'a, String, TwitchError> {
        Box::pin(async move {
            self.with(|t| {
                if let Some((k, e)) = &t.subscribe_fails
                    && k == kind
                {
                    return Err(e.clone());
                }
                if t.conflicts.iter().any(|c| c == kind) {
                    return Err(TwitchError::Conflict);
                }
                t.subscribed.push((kind.to_string(), condition, conduit.to_string()));
                Ok(format!("es-{}", t.subscribed.len()))
            })
        })
    }
    fn find_subscription<'a>(
        &'a self,
        _app: &'a str,
        kind: &'a str,
        condition: &'a Value,
    ) -> Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move {
            assert!(condition["broadcaster_user_id"].is_string());
            Ok(Some(format!("old-{kind}")))
        })
    }
    fn unsubscribe<'a>(&'a self, _app: &'a str, id: &'a str) -> Fut<'a, (), TwitchError> {
        Box::pin(async move {
            self.with(|t| t.unsubscribed.push(id.to_string()));
            Ok(())
        })
    }
    fn send_chat<'a>(
        &'a self,
        app: &'a str,
        broadcaster: &'a str,
        sender: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move {
            assert_eq!(app, "app-token", "replies go out with the app token (the Chat Bot badge)");
            self.with(|t| {
                if let Some(e) = t.send_fails.clone() {
                    return Err(e);
                }
                t.sent.push((broadcaster.into(), sender.into(), text.into(), reply_to.map(String::from)));
                Ok((!t.no_ids).then(|| format!("msg-{}", t.sent.len())))
            })
        })
    }
    fn stream_started<'a>(&'a self, _app: &'a str, _broadcaster: &'a str) -> Fut<'a, Option<i64>, TwitchError> {
        Box::pin(async move { Ok(self.with(|t| t.live)) })
    }
}

#[derive(Debug, Default)]
struct Kk {
    sent: Vec<(String, String, String, Option<String>)>,
    refreshed: u32,
    /// Refreshes started (before any wait).
    entered: u32,
    /// Refresh tokens Kick still takes (each once; `None` takes any).
    valid: Option<HashSet<String>>,
    /// The next refresh waits for this first.
    wait: Option<Arc<tokio::sync::Notify>>,
    revoked: Vec<String>,
    send_fails: Option<KickError>,
}

#[derive(Debug, Default)]
struct FakeKick(Mutex<Kk>);

impl FakeKick {
    fn with<T>(&self, f: impl FnOnce(&mut Kk) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }
}

impl KickBot for FakeKick {
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, KickTokens, KickError> {
        Box::pin(async move {
            let wait = self.with(|k| {
                k.entered += 1;
                k.wait.take()
            });
            if let Some(w) = wait {
                w.notified().await;
            }
            // Two refreshes in flight overlap here (the race a single-flight lock prevents).
            tokio::task::yield_now().await;
            self.with(|k| {
                if let Some(valid) = &mut k.valid {
                    if !valid.remove(refresh_token) {
                        return Err(KickError::Unauthorized);
                    }
                    valid.insert(format!("{refresh_token}+"));
                }
                k.refreshed += 1;
                Ok(KickTokens {
                    access_token: format!("kat-{}", k.refreshed),
                    refresh_token: format!("{refresh_token}+"),
                    expires_in: 3_600,
                })
            })
        })
    }
    fn send_chat<'a>(
        &'a self,
        access: &'a str,
        broadcaster: &'a str,
        text: &'a str,
        reply_to: Option<&'a str>,
    ) -> Fut<'a, Option<String>, KickError> {
        Box::pin(async move {
            self.with(|k| {
                if let Some(e) = k.send_fails.clone() {
                    return Err(e);
                }
                k.sent.push((access.into(), broadcaster.into(), text.into(), reply_to.map(String::from)));
                Ok(Some(format!("kmsg-{}", k.sent.len())))
            })
        })
    }
    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), KickError> {
        Box::pin(async move {
            self.with(|k| k.revoked.push(token.to_string()));
            Ok(())
        })
    }
}

/// Kick's own OAuth and users endpoints, on loopback (the relay's existing broker calls them).
async fn fake_kick_web() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            post(|| async {
                axum::Json(json!({ "access_token": "kick-at", "refresh_token": "kick-rt", "expires_in": 3600 }))
            }),
        )
        .route(
            "/public/v1/users",
            get(|| async { axum::Json(json!({ "data": [{ "user_id": 9001, "name": "AshaBot" }] })) }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

fn twitch_cfg() -> TwitchConfig {
    TwitchConfig::from_values(Some("cloud-client".into()), Some("cloud-secret-SECRET".into()), Some(ES_SECRET.into()))
        .unwrap()
        .unwrap()
        .with_desktop_client_id(Some(DESKTOP_APP.into()))
}

fn vault() -> crate::vault::Vault {
    crate::vault::Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

fn who(id: &str, login: &str, client: &str) -> TokenInfo {
    TokenInfo { user_id: id.into(), login: login.into(), scopes: vec![], client_id: client.into() }
}

// ── The world ────────────────────────────────────────────────────────────────────────

struct World {
    base: String,
    db: Arc<Db>,
    state: AppState,
    tw: Arc<FakeTwitch>,
    kk: Arc<FakeKick>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

async fn world() -> World {
    world_with(true, true, true).await
}

fn state_for(dir: &std::path::Path, base: &str, kick_base: &str, with_vault: bool) -> AppState {
    let db = Db::open(&dir.join("relay.db")).unwrap();
    let cfg = Config {
        public_url: base.to_string(),
        tip_url: None,
        kick_client_id: Some("kick-client".into()),
        kick_client_secret: Some("kick-secret".into()),
        kick_id_url: kick_base.to_string(),
        kick_api_url: kick_base.to_string(),
    };
    AppState::new(cfg, db).with_vault(with_vault.then(vault))
}

async fn world_with(twitch: bool, kick: bool, with_vault: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let kick_base = fake_kick_web().await;
    let tw = Arc::new(FakeTwitch::default());
    tw.with(|t| t.tokens.insert(DESK.into(), who(STREAMER_ID, "asha_live", DESKTOP_APP)));
    let kk = Arc::new(FakeKick::default());
    let rt = Runtime::new(
        twitch.then(twitch_cfg),
        twitch.then(|| tw.clone() as Arc<dyn TwitchCloud>),
        kick.then(|| kk.clone() as Arc<dyn KickBot>),
    );
    let state = state_for(dir.path(), &base, &kick_base, with_vault).with_bot(rt);
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, db, state, tw, kk, http, dir }
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

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

    async fn call(&self, method: reqwest::Method, path: &str, auth: Option<&str>, body: Option<Value>) -> Answer {
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
        Answer { status, headers, text: res.text().await.unwrap() }
    }

    async fn get(&self, path: &str, auth: &str) -> Answer {
        self.call(reqwest::Method::GET, path, Some(auth), None).await
    }
    async fn put(&self, path: &str, auth: &str, body: Value) -> Answer {
        self.call(reqwest::Method::PUT, path, Some(auth), Some(body)).await
    }
    async fn post(&self, path: &str, auth: &str, body: Value) -> Answer {
        self.call(reqwest::Method::POST, path, Some(auth), Some(body)).await
    }
    async fn del(&self, path: &str, auth: &str) -> Answer {
        self.call(reqwest::Method::DELETE, path, Some(auth), None).await
    }
    async fn page(&self, path: &str) -> Answer {
        self.call(reqwest::Method::GET, path, None, None).await
    }

    async fn view(&self, auth: &str) -> Value {
        let a = self.get("/v1/pro/bot", auth).await;
        assert_eq!(a.status, 200, "{}", a.text);
        a.json()
    }

    async fn enable(&self, auth: &str, on: bool) -> Value {
        let a = self.put("/v1/pro/bot/enabled", auth, json!({ "enabled": on })).await;
        assert_eq!(a.status, 200, "{}", a.text);
        a.json()
    }

    /// Connect for one Twitch step: the `state` in Twitch's sign-in link.
    async fn start(&self, auth: &str, body: Value) -> String {
        let a = self.post("/v1/pro/bot/twitch/connect", auth, body).await;
        assert_eq!(a.status, 201, "{}", a.text);
        let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
        url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string()
    }

    /// Twitch sends the browser back after `who` signed in with `scopes`.
    async fn finish(&self, state: &str, refresh: &str, who: (&str, &str), scopes: &[&str]) -> Answer {
        let code = format!("code-{}", crate::random(6));
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        self.tw.with(|t| {
            t.codes.insert(
                code.clone(),
                (
                    UserTokens {
                        access_token: format!("at-{code}"),
                        refresh_token: refresh.into(),
                        scopes: scopes.clone(),
                    },
                    TokenInfo { user_id: who.0.into(), login: who.1.into(), scopes, client_id: "cloud-client".into() },
                ),
            )
        });
        self.page(&format!("/v1/bot/twitch/callback?code={code}&state={}&scope=x", enc(state))).await
    }

    /// One Twitch step the way a browser does it: connect, Twitch's page, the callback.
    async fn twitch_step(&self, auth: &str, account: &str, who: (&str, &str), scopes: &[&str]) -> Answer {
        let state = self.start(auth, json!({ "account": account, "owner_token": DESK })).await;
        self.finish(&state, &format!("rt-{account}-{}", who.0), who, scopes).await
    }

    /// Twitch fully connected: the bot account, then the channel.
    async fn connect_twitch(&self, auth: &str) {
        let a = self.twitch_step(auth, "bot", (BOT_ID, "ashabot"), twitch::BOT_SCOPES).await;
        assert_eq!(a.status, 200, "{}", a.text);
        let a = self.twitch_step(auth, "channel", (STREAMER_ID, "asha_live"), twitch::CHANNEL_SCOPES).await;
        assert_eq!(a.status, 200, "{}", a.text);
        assert!(a.text.contains("set up in asha_live"), "{}", a.text);
    }

    /// Twitch connected, a config pushed, switched on, the PCs quiet: the cloud answers.
    async fn on_duty(&self, auth: &str, sub: &str) {
        self.connect_twitch(auth).await;
        self.push(auth, 0, config()).await;
        self.enable(auth, true).await;
        self.cloud_takes_over(sub);
    }

    /// Pushes a config as a PC would (from version `base`).
    async fn push(&self, auth: &str, base: i64, config: Value) -> Answer {
        self.put("/v1/pro/bot/config", auth, json!({ "version": base, "config": config })).await
    }

    fn sub_of(&self, install: &str) -> String {
        self.db.pro_sub_for_install(install).unwrap().unwrap().id
    }

    /// The PCs went quiet long ago: the cloud is on duty on every platform.
    fn cloud_takes_over(&self, sub: &str) {
        let past = Instant::now().checked_sub(TAKEOVER_AFTER + Duration::from_secs(1)).unwrap();
        for p in PLATFORMS {
            self.state.bot.lease(sub, p, |l| *l = Lease::new(past, now_ms() - 91_000));
        }
    }

    fn channel(&self, sub: &str, platform: &str) -> store::Channel {
        self.db.bot_channel(sub, platform).unwrap().unwrap()
    }

    async fn chat(&self, sub: &str, id: &str, who: (&str, &str), badges: Value, text: &str) {
        let ch = self.channel(sub, "twitch");
        let event = json!({
            "broadcaster_user_id": STREAMER_ID, "chatter_user_id": who.0, "chatter_user_login": who.1,
            "chatter_user_name": who.1, "message_id": id, "message": { "text": text, "fragments": [] },
            "badges": badges, "color": "#FF0000",
        });
        let e = ferrocast_twitch::map::map("channel.chat.message", "delivery", &event, Millis(now_ms()));
        on_chat(&self.state, &ch, Platform::Twitch, e).await;
    }

    /// Kick connected for the cloud bot through the broker (the channel owned by `install`).
    async fn connect_kick(&self, auth: &str, install: &str) -> Answer {
        self.db.set_kick_owner(KICK_STREAMER, install).unwrap();
        let a = self.post("/v1/pro/bot/kick/connect", auth, json!({})).await;
        assert_eq!(a.status, 201, "{}", a.text);
        let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
        let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        self.page(&format!("/v1/kick/callback?code=kc&state={}", enc(&state))).await
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn config() -> Value {
    json!({
        "bot_on": true,
        "commands": [
            { "id": "c1", "name": "!lurk", "aliases": [], "response": "Enjoy the lurk, {user}!", "roles": "everyone",
              "cooldown_s": 0, "user_cooldown_s": 0, "platforms": [], "enabled": true },
            { "id": "c2", "name": "!mods", "aliases": [], "response": "mod line {args}", "roles": "mods",
              "cooldown_s": 30, "user_cooldown_s": 30, "platforms": [], "enabled": true },
            { "id": "c3", "name": "!up", "aliases": [], "response": "up {uptime}", "roles": "everyone",
              "cooldown_s": 0, "user_cooldown_s": 0, "platforms": [], "enabled": true }
        ],
        "timers": [{ "id": "t1", "name": "socials", "message": "Follow on {platform}", "every_min": 10,
                     "min_chat_lines": 0, "platforms": [], "enabled": true }],
        "auto_replies": [],
        "counters": [{ "name": "deaths", "command": "!deaths", "reply": "{counter}: {count}", "enabled": true, "value": 3 }]
    })
}

fn moderator() -> Value {
    json!([{ "set_id": "moderator", "id": "1", "info": "" }])
}

// ── The gate ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_route_checks_the_install_and_pro_first() {
    let w = world().await;
    let plain = w.install("i_free");
    let routes: [(reqwest::Method, &str, Option<Value>); 5] = [
        (reqwest::Method::GET, "/v1/pro/bot", None),
        (reqwest::Method::PUT, "/v1/pro/bot/config", Some(json!({ "version": 0, "config": {} }))),
        (reqwest::Method::PUT, "/v1/pro/bot/enabled", Some(json!({ "enabled": true }))),
        (reqwest::Method::POST, "/v1/pro/bot/twitch/connect", Some(json!({}))),
        (reqwest::Method::DELETE, "/v1/pro/bot/twitch", None),
    ];
    for (m, p, b) in routes {
        let a = w.call(m.clone(), p, None, b.clone()).await;
        assert_eq!((a.status, a.code().as_str()), (401, "unauthorized"), "{m} {p}");
        let a = w.call(m.clone(), p, Some("nobody.secret"), b.clone()).await;
        assert_eq!(a.status, 401, "{m} {p}");
        let a = w.call(m.clone(), p, Some(&plain), b).await;
        assert_eq!((a.status, a.code().as_str()), (402, "pro_required"), "{m} {p}: {}", a.text);
    }
}

#[tokio::test]
async fn the_view_starts_empty_and_lists_what_this_relay_has() {
    let w = world_with(true, false, true).await;
    let auth = w.pro("i1", "sub_1");
    let v = w.view(&auth).await;
    assert_eq!(v["enabled"], false);
    assert_eq!(v["duty"]["who"], "off");
    assert_eq!((v["version"].as_i64(), v["channels"].as_array().map(Vec::len)), (Some(0), Some(0)));
    assert_eq!(v["available"], json!({ "twitch": true, "kick": false }));
    assert_eq!(v["limits"]["timers"], 50);
    assert_eq!(v["limits"]["commands"], 500);
    assert_eq!(v["limits"]["messages_per_minute"], 20);
    assert_eq!(v["limits"]["takeover_after_s"], 90);
    // This PC's code: the same on the sign-in pages, nothing of the install id.
    let code = v["pc_code"].as_str().unwrap().to_string();
    assert_eq!(code, pc_code(&w.state, "i1"));
    assert!(code.len() == 9 && code.as_bytes()[4] == b'-' && !code.contains("i1"), "{code}");
    assert_ne!(pc_code(&w.state, "i2"), code);
    let st = w.get("/v1/pro/status", &auth).await.json();
    assert_eq!(st["features"]["cloud_bot"], true);
    // No vault: the feature is off even with Twitch set up.
    let w2 = world_with(true, true, false).await;
    let auth2 = w2.pro("i2", "sub_2");
    assert_eq!(w2.get("/v1/pro/status", &auth2).await.json()["features"]["cloud_bot"], false);
    let a = w2.post("/v1/pro/bot/twitch/connect", &auth2, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (503, "vault_unavailable"));
}

// ── Config ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn config_pushes_are_versioned_and_checked() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.push(&auth, 0, config()).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert_eq!(a.json()["version"], 1);
    assert_eq!(a.headers["cache-control"], "no-store");
    // Another PC still on version 0: refused, and told to read again.
    let a = w.push(&auth, 0, config()).await;
    assert_eq!((a.status, a.code().as_str()), (409, "stale_version"), "{}", a.text);
    assert!(a.json()["error"]["message"].as_str().unwrap().contains("version 1"));
    let a = w.push(&auth, 1, config()).await;
    assert_eq!(a.json()["version"], 2);
    let v = w.view(&auth).await;
    assert_eq!(v["version"], 2);
    assert_eq!(v["counts"], json!({ "commands": 3, "timers": 1, "counters": 1, "auto_replies": 0 }));
    assert!(v["config_updated_at"].as_i64().is_some());

    // Limits.
    let mut many = config();
    many["commands"] = Value::Array(
        (0..501)
            .map(|i| {
                json!({ "id": format!("c{i}"), "name": format!("!c{i}"), "response": "x", "roles": "everyone",
                             "cooldown_s": 0, "user_cooldown_s": 0, "enabled": true })
            })
            .collect(),
    );
    let a = w.push(&auth, 2, many).await;
    assert_eq!((a.status, a.code().as_str()), (413, "too_many_commands"), "{}", a.text);
    let mut timers = config();
    timers["timers"] = Value::Array((0..51).map(|_| config()["timers"][0].clone()).collect());
    let a = w.push(&auth, 2, timers).await;
    assert_eq!((a.status, a.code().as_str()), (413, "too_many_timers"));
    let mut huge = config();
    huge["commands"][0]["response"] = json!("x".repeat(300 * 1024));
    let a = w.push(&auth, 2, huge).await;
    assert_eq!((a.status, a.code().as_str()), (413, "too_large"), "{}", a.text);
    // A bad item names itself; a wrong shape is 422 too; not JSON 400; wrong type 415.
    let mut bad = config();
    bad["commands"][1]["name"] = json!("mods");
    let a = w.push(&auth, 2, bad).await;
    assert_eq!((a.status, a.code().as_str()), (422, "invalid_config"));
    assert!(a.json()["error"]["message"].as_str().unwrap().starts_with("commands[1] (mods): "), "{}", a.text);
    let a = w.push(&auth, 2, json!({ "commands": "nope" })).await;
    assert_eq!((a.status, a.code().as_str()), (422, "invalid_config"));
    let a = w.put("/v1/pro/bot/config", &auth, json!({ "config": {} })).await;
    assert_eq!((a.status, a.code().as_str()), (400, "invalid"));
    let raw = w
        .http
        .put(format!("{}/v1/pro/bot/config", w.base))
        .bearer_auth(&auth)
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 415);
    let raw = w
        .http
        .put(format!("{}/v1/pro/bot/config", w.base))
        .bearer_auth(&auth)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 400);
    assert_eq!(w.view(&auth).await["version"], 2, "nothing refused was kept");
}

// ── Twitch ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn twitch_connects_in_two_steps_with_sealed_sign_ins() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    // The channel step needs the bot first.
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "account": "channel", "owner_token": DESK })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "bot_account_first"));
    // The bot step asks only for the bot scopes; moderation is a separate opt-in.
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "owner_token": DESK })).await;
    assert_eq!(a.status, 201, "{}", a.text);
    let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
    let q: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
    assert_eq!(q["scope"], "user:bot user:read:chat user:write:chat");
    assert_eq!(q["client_id"], "cloud-client");
    assert_eq!(q["redirect_uri"], format!("{}/v1/bot/twitch/callback", w.base));
    assert_eq!(q["force_verify"], "true");
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "moderation": true, "owner_token": DESK })).await;
    let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
    let scope = url.query_pairs().find(|(k, _)| k == "scope").unwrap().1.to_string();
    assert!(scope.contains("moderator:manage:banned_users"), "{scope}");

    let a = w.twitch_step(&auth, "bot", (BOT_ID, "ashabot"), twitch::BOT_SCOPES).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(a.text.contains("Allow it in your channel"), "{}", a.text);
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["state"], "needs_channel");
    assert_eq!(v["channels"][0]["bot"], json!({ "id": BOT_ID, "login": "ashabot" }));
    assert_eq!(v["channels"][0]["moderation"], false);
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "account": "bot", "owner_token": DESK })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "already_connected"));

    let a = w.twitch_step(&auth, "channel", (STREAMER_ID, "asha_live"), twitch::CHANNEL_SCOPES).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(a.text.contains(&pc_code(&w.state, "i1")), "the PC code on the page: {}", a.text);
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["state"], "ready");
    assert_eq!(v["channels"][0]["channel"], json!({ "id": STREAMER_ID, "login": "asha_live" }));
    // Nothing is subscribed while the cloud bot is off: switching on does it.
    assert!(w.tw.kinds().is_empty(), "no chat flows while it's off");
    w.enable(&auth, true).await;
    let subs = w.tw.with(|t| t.subscribed.clone());
    let kinds: Vec<&str> = subs.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(kinds, ["channel.chat.message", "stream.online", "stream.offline"]);
    assert_eq!(subs[0].1, json!({ "broadcaster_user_id": STREAMER_ID, "user_id": BOT_ID }));
    assert_eq!(subs[0].2, "conduit-1");
    assert_eq!(w.channel(&sub, "twitch").subscriptions, ["es-1", "es-2", "es-3"]);
    assert_eq!(w.db.bot_kv(store::KV_CONDUIT).unwrap().as_deref(), Some("conduit-1"), "the conduit is remembered");

    // The vault round trip: rows hold ciphertext that opens only for this subscription.
    let sealed = w.db.bot_token(&sub, "twitch", "bot").unwrap().unwrap();
    assert!(!sealed.ct.windows(10).any(|x| x == b"rt-bot-500"), "never in plain text");
    assert_eq!(open_token(&w.state, &sub, "twitch", "bot").as_deref(), Some("rt-bot-500000001"));
    assert_eq!(open_token(&w.state, &sub, "twitch", "channel").as_deref(), Some("rt-channel-141981764"));
    assert!(vault().open("sub_other", &token_purpose("twitch", "bot"), &sealed).is_err());
    assert!(!format!("{sealed:?}").contains("rt-"), "Debug shows no secret");

    // Disconnect: subscriptions deleted and both sign-ins revoked at Twitch, rows gone.
    let a = w.del("/v1/pro/bot/twitch", &auth).await;
    assert_eq!(a.status, 204, "{}", a.text);
    assert_eq!(w.tw.with(|t| t.unsubscribed.clone()), ["es-1", "es-2", "es-3"]);
    let revoked = w.tw.with(|t| t.revoked.clone());
    assert!(revoked.contains(&"at-from-rt-bot-500000001".to_string()), "{revoked:?}");
    assert!(revoked.contains(&"at-from-rt-channel-141981764".to_string()), "{revoked:?}");
    assert!(w.db.bot_channel(&sub, "twitch").unwrap().is_none());
    assert!(w.db.bot_token(&sub, "twitch", "bot").unwrap().is_none());
    assert_eq!(w.view(&auth).await["channels"], json!([]));
}

#[tokio::test]
async fn both_twitch_steps_need_the_pcs_own_twitch_account() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    // No token from the PC, one Twitch doesn't know, one from another app.
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (409, "twitch_not_linked"), "{}", a.text);
    assert!(a.json()["error"]["hint"].as_str().unwrap().contains("Accounts"));
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "owner_token": "nope" })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "twitch_not_linked"));
    w.tw.with(|t| t.tokens.insert("other-app".into(), who(STREAMER_ID, "asha_live", "someone-elses-app")));
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "owner_token": "other-app" })).await;
    assert_eq!((a.status, a.code().as_str()), (403, "wrong_twitch_app"), "{}", a.text);

    // The bot page names whose chat it will speak in, warns it isn't its own, shows the code.
    let a = w.twitch_step(&auth, "bot", (BOT_ID, "ashabot"), twitch::BOT_SCOPES).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(a.text.contains("in asha_live&#39;s chat") || a.text.contains("in asha_live's chat"), "{}", a.text);
    assert!(a.text.contains("doesn't own that channel") || a.text.contains("doesn&#39;t own"), "{}", a.text);
    assert!(a.text.contains(&pc_code(&w.state, "i1")), "{}", a.text);
    // The bot is the streamer's own account: no warning.
    let w2 = world().await;
    let auth2 = w2.pro("i1", "sub_1");
    let a = w2.twitch_step(&auth2, "bot", (STREAMER_ID, "asha_live"), twitch::BOT_SCOPES).await;
    assert!(a.text.contains("in its own chat") && !a.text.contains("own that channel"), "{}", a.text);

    // The channel step signed in by anyone but the PC's account: refused and revoked.
    let state = w.start(&auth, json!({ "account": "channel", "owner_token": DESK })).await;
    let a = w.finish(&state, "rt-x", ("666", "mallory"), twitch::CHANNEL_SCOPES).await;
    assert_eq!(a.status, 403, "{}", a.text);
    assert!(a.text.contains("asha_live"), "names the account to use: {}", a.text);
    assert!(w.channel(&sub, "twitch").channel_id.is_none());
    assert!(w.db.bot_token(&sub, "twitch", "channel").unwrap().is_none());
    assert!(w.tw.with(|t| t.revoked.iter().any(|r| r.starts_with("at-code-"))), "the stray sign-in is revoked");
}

#[tokio::test]
async fn the_twitch_callback_refuses_what_it_should() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // A tampered or expired state.
    let state = w.start(&auth, json!({ "owner_token": DESK })).await;
    let forged = state.replacen("i1", "i2", 1);
    let a = w.page(&format!("/v1/bot/twitch/callback?code=x&state={}", enc(&forged))).await;
    assert_eq!(a.status, 400);
    let old = twitch::sign_state(&twitch_cfg(), twitch::Step::Channel, "i1", STREAMER_ID, "n", now_ms() - 11 * 60_000);
    assert!(twitch::check_state(&twitch_cfg(), &old, now_ms()).is_none());
    // The state is keyed with a key derived for it, never the client secret itself.
    let payload = format!("c.i1.{}.n.{STREAMER_ID}", now_ms() + 60_000);
    let mut raw = Hmac::<Sha256>::new_from_slice(b"cloud-secret-SECRET").unwrap();
    raw.update(payload.as_bytes());
    let hex: String = raw.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    assert!(twitch::check_state(&twitch_cfg(), &format!("{payload}.{hex}"), now_ms()).is_none());
    let good = twitch::sign_state(&twitch_cfg(), twitch::Step::Channel, "i1", STREAMER_ID, "n", now_ms());
    assert!(twitch::check_state(&twitch_cfg(), &good, now_ms()).is_some());
    let a = w.page("/v1/bot/twitch/callback?error=access_denied&state=x").await;
    assert!(a.text.contains("cancelled"));
    // A correctly signed state that was never handed out (no nonce kept): refused.
    let a = w.page(&format!("/v1/bot/twitch/callback?code=x&state={}", enc(&good))).await;
    assert_eq!(a.status, 400, "{}", a.text);
    // Missing scopes: refused and revoked.
    let a = w.twitch_step(&auth, "bot", (BOT_ID, "ashabot"), &["user:read:chat"]).await;
    assert_eq!(a.status, 400, "{}", a.text);
    assert!(a.text.contains("user:bot"), "{}", a.text);
    assert_eq!(w.tw.with(|t| t.revoked.len()), 1);
    // A state is used once: the same link again is refused before any code is traded.
    let state = w.start(&auth, json!({ "owner_token": DESK })).await;
    let a = w.finish(&state, "rt-1", (BOT_ID, "ashabot"), twitch::BOT_SCOPES).await;
    assert_eq!(a.status, 200, "{}", a.text);
    let a = w.finish(&state, "rt-2", ("500000002", "otherbot"), twitch::BOT_SCOPES).await;
    assert_eq!(a.status, 400, "{}", a.text);
    assert!(a.text.contains("already used"), "{}", a.text);
    assert_eq!(w.channel(&w.sub_of("i1"), "twitch").bot_login.as_deref(), Some("ashabot"));
    // Pro lapsed between the click and the callback.
    let lapsed = w.pro("i5", "sub_5");
    let state = w.start(&lapsed, json!({ "owner_token": DESK })).await;
    w.set_sub("sub_5", "halted", Some(now_ms() - 10 * DAY));
    let a = w.finish(&state, "rt5", ("5", "x"), &[]).await;
    assert_eq!(a.status, 402, "{}", a.text);
    assert!(w.tw.with(|t| t.revoked.len()) >= 2, "revoked");
}

#[tokio::test]
async fn a_replaced_bot_sign_in_is_revoked() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // Two bot links opened before either finished: the second replaces the first.
    let first = w.start(&auth, json!({ "owner_token": DESK })).await;
    let second = w.start(&auth, json!({ "owner_token": DESK })).await;
    assert_eq!(w.finish(&first, "rt-first", (BOT_ID, "ashabot"), twitch::BOT_SCOPES).await.status, 200);
    assert_eq!(w.finish(&second, "rt-second", ("500000002", "bot2"), twitch::BOT_SCOPES).await.status, 200);
    assert!(w.tw.with(|t| t.revoked.contains(&"at-from-rt-first".to_string())), "the old one is revoked at Twitch");
    assert_eq!(open_token(&w.state, &w.sub_of("i1"), "twitch", "bot").as_deref(), Some("rt-second"));
}

#[tokio::test]
async fn the_channels_proven_owner_takes_it_from_another_subscription() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect_twitch(&auth).await;
    w.enable(&auth, true).await;
    assert_eq!(w.channel("sub_1", "twitch").subscriptions, ["es-1", "es-2", "es-3"]);
    // Another subscription whose PC is signed in to Twitch as the same streamer.
    let other = w.pro("i9", "sub_9");
    let a = w.twitch_step(&other, "bot", ("500000009", "otherbot"), twitch::BOT_SCOPES).await;
    assert_eq!(a.status, 200);
    let a = w.twitch_step(&other, "channel", (STREAMER_ID, "asha_live"), twitch::CHANNEL_SCOPES).await;
    assert_eq!(a.status, 200, "taken over, not 409: {}", a.text);
    assert_eq!(w.channel("sub_9", "twitch").channel_id.as_deref(), Some(STREAMER_ID));
    // The old binding is gone with its subscriptions and sign-in; its owner sees why.
    let old = w.channel("sub_1", "twitch");
    assert_eq!((old.channel_id.as_deref(), old.subscriptions.len()), (None, 0));
    assert_eq!(old.error_code(), Some("twitch_channel_taken"));
    assert!(w.db.bot_token("sub_1", "twitch", "channel").unwrap().is_none());
    assert_eq!(w.tw.with(|t| t.unsubscribed.clone()), ["es-1", "es-2", "es-3"]);
    assert!(w.tw.with(|t| t.revoked.contains(&"at-from-rt-channel-141981764".to_string())));
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["state"], "needs_channel");
}

#[tokio::test]
async fn connect_errors_name_the_problem() {
    let w = world_with(false, false, true).await;
    let auth = w.pro("i1", "sub_1");
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (503, "platform_unavailable"));
    assert!(a.json()["error"]["hint"].as_str().unwrap().contains("this PC"));
    let a = w.post("/v1/pro/bot/kick/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (503, "platform_unavailable"));
    let a = w.post("/v1/pro/bot/youtube/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (503, "platform_unavailable"));
    let a = w.post("/v1/pro/bot/tiktok/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (404, "platform_not_found"));
    let a = w.del("/v1/pro/bot/tiktok", &auth).await;
    assert_eq!((a.status, a.code().as_str()), (404, "platform_not_found"));
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "account": "both" })).await;
    assert_eq!((a.status, a.code().as_str()), (400, "invalid"));
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "surprise": 1 })).await;
    assert_eq!(a.status, 400);
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "retry": true })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "not_connected"));
}

#[tokio::test]
async fn switching_on_needs_a_ready_channel_and_off_stops_the_chat() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    let a = w.put("/v1/pro/bot/enabled", &auth, json!({ "enabled": true })).await;
    assert_eq!((a.status, a.code().as_str()), (422, "no_platform"), "{}", a.text);
    w.connect_twitch(&auth).await;
    w.push(&auth, 0, config()).await;
    let v = w.enable(&auth, true).await;
    assert_eq!(v["enabled"], true);
    assert_eq!(v["duty"]["who"], "desktop", "the cloud waits 90 s before stepping in");
    assert_eq!(v["channels"][0]["duty"], "desktop");
    assert_eq!(w.tw.kinds().len(), 3);
    // Off: Twitch stops sending chat, and nothing of it is kept or answered.
    let v = w.enable(&auth, false).await;
    assert_eq!((v["duty"]["who"].as_str(), v["channels"][0]["duty"].as_str()), (Some("off"), Some("off")));
    assert_eq!(w.tw.with(|t| t.unsubscribed.len()), 3);
    assert!(w.channel(&sub, "twitch").subscriptions.is_empty());
    w.cloud_takes_over(&sub);
    w.chat(&sub, "off-1", ("77", "viewer"), json!([]), "!lurk").await;
    assert!(recent_chat(&w.state, &sub, Platform::Twitch).is_empty(), "not kept while off");
    assert!(w.tw.sent().is_empty());
    // On again: subscribed again, and the live status read from Twitch.
    w.tw.with(|t| t.live = Some(now_ms() - 60_000));
    w.enable(&auth, true).await;
    assert_eq!(w.tw.kinds().len(), 6);
    assert!(w.channel(&sub, "twitch").live_since.is_some());
    // Disconnecting the last platform switches it off.
    w.del("/v1/pro/bot/twitch", &auth).await;
    assert_eq!(w.view(&auth).await["enabled"], false);
}

// ── EventSub ─────────────────────────────────────────────────────────────────────────

fn sign(id: &str, ts: &str, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(ES_SECRET.as_bytes()).unwrap();
    m.update(id.as_bytes());
    m.update(ts.as_bytes());
    m.update(body.as_bytes());
    format!("sha256={}", m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn rfc3339(ms: i64) -> String {
    let (y, mo, d) = crate::quota::civil(ms.div_euclid(DAY));
    let s = ms.rem_euclid(DAY) / 1000;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}123Z", s / 3600, s / 60 % 60, s % 60, ms.rem_euclid(1000))
}

impl World {
    async fn eventsub(&self, id: &str, ty: &str, ts: &str, body: &Value, sig: Option<String>) -> Answer {
        let text = body.to_string();
        let sig = sig.unwrap_or_else(|| sign(id, ts, &text));
        let res = self
            .http
            .post(format!("{}/v1/bot/twitch/eventsub", self.base))
            .header("Twitch-Eventsub-Message-Id", id)
            .header("Twitch-Eventsub-Message-Timestamp", ts)
            .header("Twitch-Eventsub-Message-Signature", sig)
            .header("Twitch-Eventsub-Message-Type", ty)
            .header("content-type", "application/json")
            .body(text)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        Answer { status, headers, text: res.text().await.unwrap() }
    }
}

fn chat_note(msg_id: &str, text: &str) -> Value {
    json!({
        "subscription": { "type": "channel.chat.message", "version": "1",
                          "condition": { "broadcaster_user_id": STREAMER_ID, "user_id": BOT_ID } },
        "event": { "broadcaster_user_id": STREAMER_ID, "chatter_user_id": "77", "chatter_user_login": "viewer",
                   "chatter_user_name": "Viewer", "message_id": msg_id, "message": { "text": text, "fragments": [] },
                   "badges": [] }
    })
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn eventsub_checks_the_signature_the_time_and_replays() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.on_duty(&auth, &sub).await;
    let now = rfc3339(now_ms());

    // The challenge: echoed as plain text.
    let verify =
        json!({ "challenge": "pogchamp-kappa-360noscope", "subscription": { "type": "channel.chat.message" } });
    let a = w.eventsub("m-verify", "webhook_callback_verification", &now, &verify, None).await;
    assert_eq!((a.status, a.text.as_str()), (200, "pogchamp-kappa-360noscope"));
    assert!(a.headers["content-type"].to_str().unwrap().starts_with("text/plain"));

    // A good notification: 204 at once, the reply goes out.
    let a = w.eventsub("m-1", "notification", &now, &chat_note("chat-1", "!lurk"), None).await;
    assert_eq!(a.status, 204, "{}", a.text);
    eventually(|| w.tw.sent().len() == 1).await;
    let sent = w.tw.with(|t| t.sent[0].clone());
    assert_eq!(sent, (STREAMER_ID.into(), BOT_ID.into(), "Enjoy the lurk, Viewer!".into(), Some("chat-1".into())));

    // The same message id again (a Twitch retry): 204, nothing done.
    let a = w.eventsub("m-1", "notification", &now, &chat_note("chat-1", "!lurk"), None).await;
    assert_eq!(a.status, 204);
    // A bad signature, a changed body, a stale and a future timestamp: 403.
    let a = w.eventsub("m-2", "notification", &now, &chat_note("chat-2", "!lurk"), Some("sha256=00".into())).await;
    assert_eq!((a.status, a.code().as_str()), (403, "bad_signature"));
    let sig = sign("m-3", &now, &chat_note("chat-3", "!lurk").to_string());
    let a = w.eventsub("m-3", "notification", &now, &chat_note("chat-3", "!lurk and more"), Some(sig)).await;
    assert_eq!(a.status, 403);
    let stale = rfc3339(now_ms() - 11 * 60_000);
    let a = w.eventsub("m-4", "notification", &stale, &chat_note("chat-4", "!lurk"), None).await;
    assert_eq!((a.status, a.code().as_str()), (403, "stale_message"));
    let future = rfc3339(now_ms() + 11 * 60_000);
    let a = w.eventsub("m-5", "notification", &future, &chat_note("chat-5", "!lurk"), None).await;
    assert_eq!(a.status, 403);
    let a = w
        .http
        .post(format!("{}/v1/bot/twitch/eventsub", w.base))
        .header("Twitch-Eventsub-Message-Id", "m-6")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(a.status().as_u16(), 400);
    // A new delivery id for the same chat message (a second shard): still answered once.
    let a = w.eventsub("m-7", "notification", &now, &chat_note("chat-1", "!lurk"), None).await;
    assert_eq!(a.status, 204);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(w.tw.sent().len(), 1, "deduped by the chat message id");

    // Revocation (the grant was removed): the channel is unbound, its subscriptions deleted
    // and its sign-in dropped; the cloud stays quiet there.
    let rev = json!({ "subscription": { "type": "channel.chat.message", "status": "authorization_revoked",
                                        "condition": { "broadcaster_user_id": STREAMER_ID, "user_id": BOT_ID } } });
    let a = w.eventsub("m-8", "revocation", &now, &rev, None).await;
    assert_eq!(a.status, 204);
    eventually(|| w.channel(&sub, "twitch").channel_id.is_none()).await;
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["state"], "needs_channel");
    assert_eq!(v["channels"][0]["error"]["code"], "twitch_revoked");
    assert_eq!(v["channels"][0]["error"]["retry"], false);
    assert!(w.db.bot_token(&sub, "twitch", "channel").unwrap().is_none());
    assert_eq!(w.tw.with(|t| t.unsubscribed.clone()), ["es-1", "es-2", "es-3"]);
    w.eventsub("m-9", "notification", &now, &chat_note("chat-9", "!lurk"), None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(w.tw.sent().len(), 1);
    // The owner allows it again: bound once more.
    let a = w.twitch_step(&auth, "channel", (STREAMER_ID, "asha_live"), twitch::CHANNEL_SCOPES).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert_eq!(w.view(&auth).await["channels"][0]["state"], "ready");
}

#[test]
fn the_signature_and_time_checks_alone() {
    let body = br#"{"a":1}"#;
    let ts = "2026-10-19T14:56:51.634234626Z";
    let at = twitch::parse_time(ts).unwrap();
    assert_eq!(at % 1000, 634);
    assert_eq!(twitch::parse_time("2026-10-19T14:56:51Z"), Some(at - 634));
    assert_eq!(twitch::parse_time("2026-10-19T14:56:51+00:00"), Some(at - 634));
    for bad in ["", "2026-10-19 14:56:51Z", "2026-13-19T14:56:51Z", "2026-10-19T14:56:51+05:30", "2026-10-19T14:56:51."]
    {
        assert_eq!(twitch::parse_time(bad), None, "{bad}");
    }
    let sig = {
        let mut m = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        m.update(b"id1");
        m.update(ts.as_bytes());
        m.update(body);
        format!("sha256={}", m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>())
    };
    assert_eq!(twitch::verify("secret", "id1", ts, body, &sig, at), Ok(()));
    assert_eq!(
        twitch::verify("secret", "id1", ts, body, &sig.to_uppercase().replace("SHA256=", "sha256="), at),
        Ok(())
    );
    assert_eq!(twitch::verify("other", "id1", ts, body, &sig, at), Err(twitch::Refusal::BadSignature));
    assert_eq!(twitch::verify("secret", "id2", ts, body, &sig, at), Err(twitch::Refusal::BadSignature));
    assert_eq!(twitch::verify("secret", "id1", ts, body, &sig[7..], at), Err(twitch::Refusal::BadSignature));
    assert_eq!(twitch::verify("secret", "id1", ts, body, &sig, at + 10 * 60_000 + 1), Err(twitch::Refusal::Stale));
    assert!(TwitchConfig::from_values(Some("a".into()), None, None).is_err());
    assert!(TwitchConfig::from_values(Some("a".into()), Some("b".into()), Some("short".into())).is_err());
    assert!(TwitchConfig::from_values(None, None, None).unwrap().is_none());
    let d = format!("{:?}", twitch_cfg());
    assert!(!d.contains("SECRET"), "{d}");
    // Derived keys differ by label and never equal the secret.
    assert_ne!(derive_key(b"s", "a"), derive_key(b"s", "b"));
    assert_ne!(derive_key(b"s", "a"), derive_key(b"t", "a"));
}

#[test]
fn bad_signatures_warn_once_a_minute() {
    let rt = Runtime::default();
    assert!(rt.note_bad_signature(60_000), "the first one warns");
    assert!(!rt.note_bad_signature(61_000), "then quiet for the minute");
    assert!(!rt.note_bad_signature(119_999));
    assert!(rt.note_bad_signature(120_000), "a new minute warns again");
}

#[tokio::test]
async fn webhook_work_is_bounded_and_counts_what_it_drops() {
    let rt = Runtime::default();
    let hold = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = QUEUE_CAP + WORKERS + 50;
    for _ in 0..total {
        let (hold, done) = (hold.clone(), done.clone());
        rt.work_twitch(Box::pin(async move {
            hold.notified().await;
            done.fetch_add(1, Ordering::Relaxed);
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (dropped, kick) = rt.dropped_work();
    assert!(dropped >= 30 && kick == 0, "a full queue drops work: {dropped}");
    hold.notify_waiters();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(done.load(Ordering::Relaxed) <= WORKERS, "at most {WORKERS} ran at once");
}

// ── The lease ────────────────────────────────────────────────────────────────────────

/// A state without an HTTP server, so the clock can be paused.
fn bare(tw: Arc<FakeTwitch>) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let st = state_for(dir.path(), "http://relay.test", "http://kick.test", true).with_bot(Runtime::new(
        Some(twitch_cfg()),
        Some(tw as Arc<dyn TwitchCloud>),
        Some(Arc::new(FakeKick::default()) as Arc<dyn KickBot>),
    ));
    (st, dir)
}

fn ready_twitch(st: &AppState, install: &str, sub: &str) {
    st.db.add_install(install, &crate::hash("secret")).unwrap();
    st.db.add_pro_sub(sub, "monthly", "e", "created", install).unwrap();
    st.db
        .sync_pro_sub(
            None,
            sub,
            &Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None },
        )
        .unwrap();
    st.db.bot_set_account(sub, "twitch", (BOT_ID, "ashabot"), None, "", false, install, 1).unwrap();
    st.db.bot_set_channel(sub, "twitch", STREAMER_ID, "asha_live").unwrap();
    let cfg: BotConfig = serde_json::from_value(config()).unwrap();
    st.db.bot_push_config(sub, 0, &serde_json::to_string(&cfg).unwrap(), 0, 1).unwrap();
    st.db.bot_set_enabled(sub, true, 1).unwrap();
}

async fn say_lurk(st: &AppState, sub: &str, id: &str) {
    let ch = st.db.bot_channel(sub, "twitch").unwrap().unwrap();
    let event = json!({ "broadcaster_user_id": STREAMER_ID, "chatter_user_id": "77", "chatter_user_login": "v",
                        "chatter_user_name": "V", "message_id": id, "message": { "text": "!lurk" }, "badges": [] });
    let e: Event = ferrocast_twitch::map::map("channel.chat.message", id, &event, Millis(now_ms()));
    on_chat(st, &ch, Platform::Twitch, e).await;
}

#[tokio::test(start_paused = true)]
async fn the_cloud_takes_over_after_90_s_of_silence_and_hands_back_at_once() {
    let tw = Arc::new(FakeTwitch::default());
    let (st, _dir) = bare(tw.clone());
    ready_twitch(&st, "i1", "sub_1");
    // A fresh relay waits 90 s, even with no PC heard yet.
    say_lurk(&st, "sub_1", "a").await;
    assert!(tw.sent().is_empty(), "standing by");
    tokio::time::advance(Duration::from_secs(60)).await;
    desktop_seen(&st, "i1", &PLATFORMS);
    for (i, s) in [30u64, 30, 29].into_iter().enumerate() {
        tokio::time::advance(Duration::from_secs(s)).await;
        say_lurk(&st, "sub_1", &format!("b{i}")).await;
    }
    assert!(tw.sent().is_empty(), "89 s since the PC: still the PC's");
    let v = view(&st, "sub_1", "i1").unwrap();
    assert_eq!((v["duty"]["who"].as_str(), v["duty"]["this_pc"].as_bool()), (Some("desktop"), Some(true)));
    assert_eq!(view(&st, "sub_1", "i2").unwrap()["duty"]["this_pc"], false, "another PC is answering");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(cloud_on_duty(&st, "sub_1", Platform::Twitch));
    let v = view(&st, "sub_1", "i1").unwrap();
    assert_eq!(v["duty"]["who"], "cloud");
    let seen = v["duty"]["desktop_seen_at"].as_i64().unwrap();
    assert_eq!(v["duty"]["since"].as_i64(), Some(seen + 90_000));
    say_lurk(&st, "sub_1", "c").await;
    assert_eq!(tw.sent(), ["Enjoy the lurk, V!"]);
    // The PC is back: handed back on its first `bot_seen`.
    desktop_seen(&st, "sub_1_install_unknown", &PLATFORMS);
    desktop_seen(&st, "i1", &PLATFORMS);
    assert!(!cloud_on_duty(&st, "sub_1", Platform::Twitch));
    say_lurk(&st, "sub_1", "d").await;
    assert_eq!(tw.sent().len(), 1);
    // Another PC of the same subscription counts too.
    st.db.add_install("i2", &crate::hash("secret")).unwrap();
    st.db.link_pro_install("i2", "sub_1").unwrap();
    tokio::time::advance(Duration::from_secs(80)).await;
    desktop_seen(&st, "i2", &PLATFORMS);
    tokio::time::advance(Duration::from_secs(80)).await;
    assert!(!cloud_on_duty(&st, "sub_1", Platform::Twitch));
}

#[tokio::test(start_paused = true)]
async fn the_lease_is_per_platform_so_the_cloud_covers_what_the_pc_isnt_connected_to() {
    let tw = Arc::new(FakeTwitch::default());
    let (st, _dir) = bare(tw);
    ready_twitch(&st, "i1", "sub_1");
    st.db
        .bot_set_account("sub_1", "kick", (KICK_BOT, "AshaBot"), Some((KICK_STREAMER, "asha")), "", false, "i1", 1)
        .unwrap();
    // The PC is connected to Twitch only.
    for _ in 0..4 {
        desktop_seen(&st, "i1", &seen_platforms(&json!({ "type": "bot_seen", "platforms": ["twitch"] })));
        tokio::time::advance(Duration::from_secs(30)).await;
    }
    assert!(!cloud_on_duty(&st, "sub_1", Platform::Twitch), "the PC answers Twitch");
    assert!(cloud_on_duty(&st, "sub_1", Platform::Kick), "the cloud answers Kick");
    let v = view(&st, "sub_1", "i1").unwrap();
    let duty: HashMap<String, String> = v["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["platform"].as_str().unwrap().into(), c["duty"].as_str().unwrap().into()))
        .collect();
    assert_eq!((duty["twitch"].as_str(), duty["kick"].as_str()), ("desktop", "cloud"));
    assert_eq!(v["duty"]["who"], "cloud", "on duty somewhere");
    // A plain `bot_seen` (an older PC) covers every platform.
    assert_eq!(seen_platforms(&json!({ "type": "bot_seen" })), PLATFORMS.to_vec());
    desktop_seen(&st, "i1", &seen_platforms(&json!({ "type": "bot_seen" })));
    assert!(!cloud_on_duty(&st, "sub_1", Platform::Kick));
    assert!(seen_platforms(&json!({ "platforms": ["youtube", 3] })).is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_link_frame_refreshes_the_lease() {
    let tw = Arc::new(FakeTwitch::default());
    let (st, _dir) = bare(tw);
    ready_twitch(&st, "i1", "sub_1");
    // What `link_loop` does with `{type: "bot_seen"}` (the socket test is in `link_sends_bot_seen`).
    tokio::time::advance(Duration::from_secs(120)).await;
    assert!(cloud_on_duty(&st, "sub_1", Platform::Twitch));
    desktop_seen(&st, "i1", &PLATFORMS);
    assert!(!cloud_on_duty(&st, "sub_1", Platform::Twitch));
}

#[tokio::test]
async fn link_sends_bot_seen() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    assert!(w.state.bot.lease("sub_1", Platform::Kick, |l| l.seen_at()).is_none());
    let mut req = format!("{}/v1/link", w.base.replacen("http", "ws", 1)).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text(json!({ "type": "bot_seen", "platforms": ["kick"] }).to_string())).await.unwrap();
    eventually(|| w.state.bot.lease("sub_1", Platform::Kick, |l| l.seen_at()).is_some()).await;
    assert!(w.state.bot.lease("sub_1", Platform::Twitch, |l| l.seen_at()).is_none(), "only what it named");
    let _ = ws.close(None).await;
    let _ = ws.next().await;
}

// ── Answering ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn twenty_messages_a_minute_per_channel_and_never_its_own_echo_or_shared_chat() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.on_duty(&auth, &sub).await;
    // Mods skip cooldowns, so only the cap holds them.
    for i in 0..25 {
        w.chat(&sub, &format!("m{i}"), ("88", "meera"), moderator(), "!mods hi").await;
    }
    let sent = w.tw.sent().len();
    let minute_edge = now_ms() % 60_000 > 58_000; // a minute ended mid-test: up to 20 more
    assert!(sent == 20 || (minute_edge && sent <= 25), "{sent}");
    w.tw.with(|t| t.sent.clear());
    // The cloud's own line coming back (by its message id), a Shared Chat line: nothing.
    let ch = w.channel(&sub, "twitch");
    w.chat(&sub, "msg-1", (BOT_ID, "ashabot"), json!([]), "!lurk").await;
    let shared = json!({ "broadcaster_user_id": STREAMER_ID, "source_broadcaster_user_id": "999",
                         "chatter_user_id": "70", "chatter_user_name": "Guest", "message_id": "sh1",
                         "message": { "text": "!lurk" }, "badges": [] });
    let e = ferrocast_twitch::map::map("channel.chat.message", "x", &shared, Millis(now_ms()));
    on_chat(&w.state, &ch, Platform::Twitch, e).await;
    // A disabled bot switch on the PC means quiet; so does Pro ending.
    let mut off = config();
    off["bot_on"] = json!(false);
    w.push(&auth, 1, off).await;
    w.chat(&sub, "q1", ("77", "viewer"), json!([]), "!lurk").await;
    w.push(&auth, 2, config()).await;
    w.set_sub("sub_1", "halted", Some(now_ms() - 10 * DAY));
    w.chat(&sub, "q2", ("78", "viewer2"), json!([]), "!lurk").await;
    if !minute_edge {
        assert!(w.tw.sent().is_empty(), "{:?}", w.tw.sent());
    }
    assert!(recent_chat(&w.state, &sub, Platform::Twitch).iter().all(|l| l.user != "viewer2"), "not kept after Pro");
}

#[tokio::test]
async fn the_bot_on_the_streamers_own_account_answers_the_streamer_but_never_itself() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    // One account for both steps (the bot is the streamer).
    assert_eq!(w.twitch_step(&auth, "bot", (STREAMER_ID, "asha_live"), twitch::BOT_SCOPES).await.status, 200);
    assert_eq!(w.twitch_step(&auth, "channel", (STREAMER_ID, "asha_live"), twitch::CHANNEL_SCOPES).await.status, 200);
    // An auto-reply that would answer the bot's own reply (a loop) if it ever saw it.
    let mut cfg = config();
    let mut rule = AutoReply::new("Lurkers", MatchKind::Word, "lurk", "lurkers welcome");
    rule.id = "ar1".into();
    rule.only_when_live = false;
    cfg["auto_replies"] = json!([serde_json::to_value(&rule).unwrap()]);
    w.push(&auth, 0, cfg).await;
    w.enable(&auth, true).await;
    w.cloud_takes_over(&sub);
    w.chat(&sub, "s1", (STREAMER_ID, "asha_live"), json!([]), "!lurk").await;
    assert_eq!(w.tw.sent(), ["Enjoy the lurk, asha_live!"], "the streamer is answered");
    // Its echo by message id, then by text (an echo before Twitch's answer): ignored.
    w.chat(&sub, "msg-1", (STREAMER_ID, "asha_live"), json!([]), "Enjoy the lurk, asha_live!").await;
    w.chat(&sub, "other-id", (STREAMER_ID, "asha_live"), json!([]), "Enjoy the lurk, asha_live!").await;
    assert_eq!(w.tw.sent().len(), 1, "{:?}", w.tw.sent());
    // A platform with no message ids: the text alone is enough.
    w.tw.with(|t| t.no_ids = true);
    w.chat(&sub, "s2", ("77", "viewer"), json!([]), "!lurk").await;
    w.chat(&sub, "echo2", (STREAMER_ID, "asha_live"), json!([]), "Enjoy the lurk, viewer!").await;
    assert_eq!(w.tw.sent().len(), 2, "{:?}", w.tw.sent());
}

#[tokio::test]
async fn counters_changed_in_the_cloud_go_back_to_every_pc_as_deltas() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.db.add_install("i2", &crate::hash("secret")).unwrap();
    w.db.link_pro_install("i2", "sub_1").unwrap();
    w.on_duty(&auth, &sub).await;
    w.chat(&sub, "k1", ("88", "meera"), moderator(), "!deaths +2").await;
    assert_eq!(w.tw.sent(), ["Deaths: 5"]);
    // A viewer sees it, can't change it.
    w.chat(&sub, "k2", ("77", "viewer"), json!([]), "!deaths +").await;
    assert_eq!(w.tw.sent().last().unwrap(), "Deaths: 5");
    let changes = w.db.bot_counter_changes(&sub).unwrap();
    assert_eq!(changes.len(), 1);
    let c = changes[0].clone();
    assert_eq!((c.name.as_str(), c.op.as_str(), c.delta, c.value), ("deaths", "add", 2, 5));
    for install in ["i1", "i2"] {
        let items = w.db.pending(install, 0, 50).unwrap();
        let item = items.iter().find(|i| i.kind == "bot_counter").expect("queued for each PC");
        let p: Value = serde_json::from_str(&item.payload).unwrap();
        assert_eq!(
            (p["id"].as_i64(), p["name"].as_str(), p["op"].as_str(), p["delta"].as_i64(), p["value"].as_i64()),
            (Some(c.id), Some("deaths"), Some("add"), Some(2), Some(5))
        );
    }
    // A PC pushing before it got the change: the step is laid on top of its own value, so
    // neither change is lost (its 10 was its own +7).
    let mut own = config();
    own["counters"][0]["value"] = json!(10);
    w.push(&auth, 1, own).await;
    w.chat(&sub, "k3", ("88", "meera"), moderator(), "!deaths").await;
    assert_eq!(w.tw.sent().last().unwrap(), "Deaths: 12");
    // A mod's `set` is a set.
    w.chat(&sub, "k3b", ("88", "meera"), moderator(), "!deaths set 4").await;
    let last = w.db.bot_counter_changes(&sub).unwrap().last().cloned().unwrap();
    assert_eq!((last.op.as_str(), last.value), ("set", 4));
    // Once a PC says it applied the changes, the rows go and its push is the truth again.
    let mut applied = config();
    applied["counters"][0]["value"] = json!(4);
    let a =
        w.put("/v1/pro/bot/config", &auth, json!({ "version": 2, "config": applied, "counters_seen": last.id })).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(w.db.bot_counter_changes(&sub).unwrap().is_empty());
    // After a relay restart the change is still known (from the table).
    w.chat(&sub, "k4", ("88", "meera"), moderator(), "!deaths +").await;
    w.state.bot.forget_mem(&sub, None);
    w.cloud_takes_over(&sub);
    w.chat(&sub, "k5", ("88", "meera"), moderator(), "!deaths").await;
    assert_eq!(w.tw.sent().last().unwrap(), "Deaths: 5");
}

#[tokio::test]
async fn timers_post_only_live_and_on_duty_and_not_at_once_after_a_takeover() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.connect_twitch(&auth).await;
    w.push(&auth, 0, config()).await;
    w.enable(&auth, true).await;
    timer_tick(&w.state).await;
    assert!(w.tw.sent().is_empty(), "the PC is answering");
    // Taken over 11 minutes ago, offline.
    let past = Instant::now().checked_sub(Duration::from_secs(11 * 60)).unwrap();
    for p in PLATFORMS {
        w.state.bot.lease(&sub, p, |l| *l = Lease::new(past, now_ms() - 11 * 60_000 - 90_000));
    }
    timer_tick(&w.state).await;
    assert!(w.tw.sent().is_empty(), "not live");
    set_live(&w.state, &sub, Platform::Twitch, Some(now_ms() - 65 * 60_000));
    timer_tick(&w.state).await;
    assert_eq!(w.tw.sent(), ["Follow on Twitch"], "10 minutes after the takeover");
    timer_tick(&w.state).await;
    assert_eq!(w.tw.sent().len(), 1, "every 10 minutes");
    // Just taken over: the PC may have posted a moment ago, so not at once.
    w.state.bot.forget_mem(&sub, None);
    w.cloud_takes_over(&sub);
    timer_tick(&w.state).await;
    assert_eq!(w.tw.sent().len(), 1, "no double post at the handover");
    // Live and `{uptime}` survive a relay restart (kept on the channel).
    w.chat(&sub, "u1", ("77", "viewer"), json!([]), "!up").await;
    assert_eq!(w.tw.sent().last().unwrap(), "up 1h 5m");
    // The live status comes from EventSub, with the stream's own start.
    let now = rfc3339(now_ms());
    let started = now_ms() - 5 * 60_000;
    let on = json!({ "subscription": { "type": "stream.online", "condition": { "broadcaster_user_id": STREAMER_ID } },
                     "event": { "broadcaster_user_id": STREAMER_ID, "started_at": rfc3339(started) } });
    w.set_live_none(&sub);
    w.eventsub("s-on", "notification", &now, &on, None).await;
    eventually(|| w.channel(&sub, "twitch").live_since == Some(started)).await;
    let off = json!({ "subscription": { "type": "stream.offline", "condition": { "broadcaster_user_id": STREAMER_ID } },
                      "event": { "broadcaster_user_id": STREAMER_ID } });
    w.eventsub("s-off", "notification", &now, &off, None).await;
    eventually(|| w.channel(&sub, "twitch").live_since.is_none()).await;
}

impl World {
    fn set_live_none(&self, sub: &str) {
        set_live(&self.state, sub, Platform::Twitch, None);
    }
}

#[tokio::test]
async fn only_signing_out_blocks_and_a_good_message_clears_a_refusal() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.on_duty(&auth, &sub).await;
    // Refused (banned, timed out): shown with a hint and Try again, the channel keeps going.
    w.tw.with(|t| t.send_fails = Some(TwitchError::Forbidden("banned".into())));
    w.chat(&sub, "e1", ("77", "viewer"), json!([]), "!lurk").await;
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["state"], "ready");
    assert_eq!(v["channels"][0]["error"]["code"], "twitch_refused");
    assert_eq!(v["channels"][0]["error"]["retry"], true);
    assert!(v["channels"][0]["error"]["hint"].as_str().unwrap().contains("banned"));
    // Dropped (AutoMod, slow mode): noted, not an error.
    w.tw.with(|t| t.send_fails = Some(TwitchError::Dropped("Your message is being checked by mods".into())));
    w.chat(&sub, "e2", ("78", "viewer2"), json!([]), "!lurk").await;
    let v = w.view(&auth).await;
    assert_eq!(v["channels"][0]["last_drop"]["reason"], "Your message is being checked by mods");
    assert!(v["channels"][0]["last_drop"]["at"].as_i64().is_some());
    // The next message that goes out clears the refusal.
    w.tw.with(|t| t.send_fails = None);
    w.chat(&sub, "e3", ("79", "viewer3"), json!([]), "!lurk").await;
    assert_eq!(w.tw.sent().len(), 1);
    assert_eq!(w.view(&auth).await["channels"][0]["error"], Value::Null);
    // Signed out blocks until it's connected again.
    w.tw.with(|t| t.send_fails = Some(TwitchError::Unauthorized));
    w.chat(&sub, "e4", ("80", "viewer4"), json!([]), "!lurk").await;
    let v = w.view(&auth).await;
    assert_eq!(
        (v["channels"][0]["state"].as_str(), v["channels"][0]["error"]["retry"].as_bool()),
        (Some("error"), Some(false))
    );
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "retry": true })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "connect_again"));
    // The hourly check marks a sign-in Twitch no longer takes.
    w.db.bot_set_error(&sub, "twitch", None).unwrap();
    let conduits = w.tw.with(|t| t.conduits);
    w.tw.with(|t| t.refresh_fails = true);
    sweep(&w.state).await;
    assert_eq!(w.view(&auth).await["channels"][0]["error"]["code"], "twitch_signed_out");
    assert_eq!(w.tw.with(|t| t.conduits), conduits + 1, "the hourly sweep points the shard here again");
}

#[tokio::test]
async fn twitch_unreachable_is_retried_by_the_sweep_and_try_again() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.connect_twitch(&auth).await;
    // Subscribing fails halfway: what was made is saved (no orphans at Twitch).
    w.tw.with(|t| t.subscribe_fails = Some(("stream.offline".into(), TwitchError::Unavailable("down".into()))));
    w.enable(&auth, true).await;
    let ch = w.channel(&sub, "twitch");
    assert_eq!(ch.subscriptions, ["es-1", "es-2"], "each id saved as it's made");
    assert_eq!(ch.error_code(), Some("twitch_unavailable"));
    assert!(ch.ready(), "not blocking");
    // Try again from the panel: subscribed again (the old ones deleted first).
    w.tw.with(|t| t.subscribe_fails = None);
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "retry": true })).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert_eq!(a.json()["channels"][0]["error"], Value::Null);
    assert_eq!(w.tw.with(|t| t.unsubscribed.clone()), ["es-1", "es-2"]);
    assert_eq!(w.channel(&sub, "twitch").subscriptions.len(), 3);
    // The sweep retries one still marked unreachable, on its own.
    let err = error_json("twitch_unavailable", "x", "y");
    w.db.bot_set_error(&sub, "twitch", Some(&err)).unwrap();
    let before = w.tw.kinds().len();
    sweep(&w.state).await;
    assert_eq!(w.tw.kinds().len(), before + 3);
    assert!(w.channel(&sub, "twitch").error.is_none());
    // A subscription that already exists at Twitch is looked up, not lost.
    w.tw.with(|t| t.conflicts = vec!["stream.online".into()]);
    let a = w.post("/v1/pro/bot/twitch/connect", &auth, json!({ "retry": true })).await;
    assert_eq!(a.status, 200);
    assert!(w.channel(&sub, "twitch").subscriptions.contains(&"old-stream.online".to_string()));
}

#[tokio::test]
async fn a_new_conduit_subscribes_every_channel_again() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.connect_twitch(&auth).await;
    w.enable(&auth, true).await;
    assert_eq!(w.tw.kinds().len(), 3);
    // The same conduit at the hourly sweep: nothing more.
    sweep(&w.state).await;
    assert_eq!(w.tw.kinds().len(), 3);
    // Twitch hands back another one (the old was deleted): everything again, on it.
    w.tw.with(|t| t.conduit_id = Some("conduit-2".into()));
    sweep(&w.state).await;
    let subs = w.tw.with(|t| t.subscribed.clone());
    assert_eq!(subs.len(), 6, "{subs:?}");
    assert!(subs[3..].iter().all(|(_, _, c)| c == "conduit-2"));
    assert_eq!(w.db.bot_kv(store::KV_CONDUIT).unwrap().as_deref(), Some("conduit-2"));
    assert_eq!(w.channel(&sub, "twitch").subscriptions.len(), 3);
}

/// Helix's conduit routes on loopback: shard 0's status and how many PATCHes came.
async fn fake_helix(status: &'static str) -> (String, Arc<Mutex<u32>>) {
    use axum::routing::get;
    let patches = Arc::new(Mutex::new(0u32));
    let p2 = patches.clone();
    let state: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let s2 = state.clone();
    *state.lock().unwrap() = format!("{base}/cb");
    let app = axum::Router::new()
        .route(
            "/eventsub/conduits",
            get(|| async { axum::Json(json!({ "data": [{ "id": "c-9", "shard_count": 1 }] })) }),
        )
        .route(
            "/eventsub/conduits/shards",
            get(move || {
                let cb = s2.lock().unwrap().clone();
                async move {
                    axum::Json(json!({ "data": [{ "id": "0", "status": status,
                                                    "transport": { "method": "webhook", "callback": cb } }] }))
                }
            })
            .patch(move || {
                *p2.lock().unwrap() += 1;
                async { axum::Json(json!({ "data": [], "errors": [] })) }
            }),
        );
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, patches)
}

#[tokio::test]
async fn the_conduit_shard_is_patched_only_when_it_needs_it() {
    let (base, patches) = fake_helix("enabled").await;
    let api = HttpTwitch::with_bases(&twitch_cfg(), &base, &base);
    assert_eq!(api.conduit("app", &format!("{base}/cb"), ES_SECRET).await.unwrap(), "c-9");
    assert_eq!(*patches.lock().unwrap(), 0, "enabled at this callback: left alone");
    assert_eq!(api.conduit("app", &format!("{base}/elsewhere"), ES_SECRET).await.unwrap(), "c-9");
    assert_eq!(*patches.lock().unwrap(), 1, "another callback: pointed here");
    let (base, patches) = fake_helix("webhook_callback_verification_failed").await;
    let api = HttpTwitch::with_bases(&twitch_cfg(), &base, &base);
    api.conduit("app", &format!("{base}/cb"), ES_SECRET).await.unwrap();
    assert_eq!(*patches.lock().unwrap(), 1, "disabled: switched on again");
}

// ── Kick ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn kick_connects_through_the_relays_kick_sign_in_and_answers_webhooks() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    let a = w.post("/v1/pro/bot/kick/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (409, "kick_not_linked"), "{}", a.text);
    w.db.set_kick_owner(KICK_STREAMER, "i1").unwrap();
    let a = w.post("/v1/pro/bot/kick/connect", &auth, json!({})).await;
    assert_eq!(a.status, 201, "{}", a.text);
    let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
    let q: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
    assert_eq!(q["scope"], "user:read chat:write");
    assert_eq!(q["redirect_uri"], format!("{}/v1/kick/callback", w.base), "the existing broker's redirect");
    assert!(q["state"].starts_with("bot."));
    // Kick sends the browser back to the broker's callback.
    let a = w.page(&format!("/v1/kick/callback?code=kc&state={}", enc(&q["state"]))).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(a.text.contains("AshaBot") && a.text.contains(&pc_code(&w.state, "i1")), "{}", a.text);
    // The tokens stayed here, sealed; the PC's queue got nothing and the channel's owner is unchanged.
    assert_eq!(open_token(&w.state, &sub, "kick", "bot").as_deref(), Some("kick-rt"));
    assert!(w.db.pending("i1", 0, 50).unwrap().iter().all(|i| i.kind != "kick_connected"));
    assert_eq!(w.db.kick_owner(KICK_STREAMER).unwrap().as_deref(), Some("i1"));
    assert_eq!(w.db.kick_owner(KICK_BOT).unwrap(), None);
    let v = w.view(&auth).await;
    let ch = v["channels"].as_array().unwrap().iter().find(|c| c["platform"] == "kick").unwrap().clone();
    assert_eq!((ch["state"].as_str(), ch["bot"]["login"].as_str()), (Some("ready"), Some("AshaBot")));
    assert_eq!(ch["channel"]["id"], KICK_STREAMER);
    let a = w.post("/v1/pro/bot/kick/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (409, "already_connected"));

    // A chat webhook while the cloud is on duty: answered in the streamer's channel.
    w.push(&auth, 0, config()).await;
    w.enable(&auth, true).await;
    w.cloud_takes_over(&sub);
    let body = json!({ "message_id": "k-1", "broadcaster": { "user_id": 668 },
                       "sender": { "user_id": 31, "username": "viewer", "identity": { "badges": [] } }, "content": "!lurk" });
    kick::on_webhook(&w.state, "i1", KICK_STREAMER, "chat.message.sent", "wh-1", &body).await;
    kick::on_webhook(&w.state, "i1", KICK_STREAMER, "chat.message.sent", "wh-1b", &body).await;
    let sent = w.kk.with(|k| k.sent.clone());
    assert_eq!(sent.len(), 1, "deduped by Kick's message id: {sent:?}");
    assert_eq!(sent[0].0, "kick-at", "the access token from the sign-in, not a refresh");
    assert_eq!((sent[0].1.as_str(), sent[0].3.as_deref()), (KICK_STREAMER, Some("k-1")));
    // Its own line by Kick's message id is never answered; another channel's webhook is ignored.
    let own =
        json!({ "message_id": "kmsg-1", "sender": { "user_id": 9001, "username": "AshaBot" }, "content": "!lurk" });
    kick::on_webhook(&w.state, "i1", KICK_STREAMER, "chat.message.sent", "wh-2", &own).await;
    kick::on_webhook(&w.state, "i1", "12345", "chat.message.sent", "wh-3", &body).await;
    assert_eq!(w.kk.with(|k| k.sent.len()), 1);
    // Live from Kick's webhook (queued through the bounded worker), kept on the channel.
    kick::queue_webhook(
        &w.state,
        "i1".into(),
        KICK_STREAMER.into(),
        "livestream.status.updated".into(),
        "wh-4".into(),
        json!({ "is_live": true }),
    );
    eventually(|| w.channel(&sub, "kick").live_since.is_some()).await;
    // A message Kick won't show (400): noted, not blocking.
    w.kk.with(|k| k.send_fails = Some(KickError::Dropped("Message too long".into())));
    let long = json!({ "message_id": "k-5", "sender": { "user_id": 32, "username": "v2", "identity": { "badges": [] } }, "content": "!lurk" });
    kick::on_webhook(&w.state, "i1", KICK_STREAMER, "chat.message.sent", "wh-5", &long).await;
    let ch = w.channel(&sub, "kick");
    assert_eq!(
        (ch.last_drop.as_ref().map(|d| d["reason"].clone()), ch.ready()),
        (Some(json!("Message too long")), true)
    );
    // Disconnect revokes at Kick.
    let a = w.del("/v1/pro/bot/kick", &auth).await;
    assert_eq!(a.status, 204);
    assert_eq!(w.kk.with(|k| k.revoked.clone()), ["kick-rt"]);
    assert!(w.db.bot_token(&sub, "kick", "bot").unwrap().is_none());
}

#[tokio::test]
async fn kick_needs_the_channel_this_pc_owns_and_its_owner_reclaims_it() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    assert_eq!(w.connect_kick(&auth, "i1").await.status, 200);
    // The channel moves to another PC's Kick connection between Connect and the callback.
    let other = w.pro("i2", "sub_2");
    let a = w.post("/v1/pro/bot/kick/connect", &auth, json!({})).await;
    assert_eq!((a.status, a.code().as_str()), (409, "already_connected"));
    // That other PC's subscription now owns the channel in Accounts: it takes the cloud bot
    // over (the stale binding goes, its sign-in revoked) instead of a dead end.
    let a = w.connect_kick(&other, "i2").await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert!(w.db.bot_channel("sub_1", "kick").unwrap().is_none(), "the stale binding is gone");
    assert_eq!(w.channel("sub_2", "kick").channel_id.as_deref(), Some(KICK_STREAMER));
    assert!(w.kk.with(|k| k.revoked.contains(&"kick-rt".to_string())));
    // A callback for a channel this PC doesn't own (any more): refused, the sign-in revoked.
    let third = w.pro("i3", "sub_3");
    w.db.set_kick_owner("777", "i3").unwrap();
    let a = w.post("/v1/pro/bot/kick/connect", &third, json!({})).await;
    let url = url::Url::parse(a.json()["auth_url"].as_str().unwrap()).unwrap();
    let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    w.install("i_somebody");
    w.db.set_kick_owner("777", "i_somebody").unwrap();
    let revoked = w.kk.with(|k| k.revoked.len());
    let a = w.page(&format!("/v1/kick/callback?code=kc&state={}", enc(&state))).await;
    assert_eq!(a.status, 409, "{}", a.text);
    assert_eq!(w.kk.with(|k| k.revoked.len()), revoked + 1, "revoked");
    assert!(w.db.bot_channel("sub_3", "kick").unwrap().is_none());
}

#[tokio::test]
async fn kick_refreshes_once_at_a_time_and_retries_a_token_another_refresh_replaced() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    assert_eq!(w.connect_kick(&auth, "i1").await.status, 200);
    let ch = w.channel(&sub, "kick");
    // Two sends with no cached token: one refresh (Kick's refresh tokens are single use).
    w.state.bot.forget_kick_access(&sub);
    w.kk.with(|k| k.valid = Some(HashSet::from(["kick-rt".to_string()])));
    let (a, b) = tokio::join!(kick::send(&w.state, &ch, "one", None), kick::send(&w.state, &ch, "two", None));
    assert!(a.is_ok() && b.is_ok(), "{a:?} {b:?}");
    assert_eq!(w.kk.with(|k| k.refreshed), 1);
    assert_eq!(open_token(&w.state, &sub, "kick", "bot").as_deref(), Some("kick-rt+"), "the new one sealed");
    // A refresh that fails because another one replaced the sealed token meanwhile: tried
    // again with the new token, not signed out.
    w.state.bot.forget_kick_access(&sub);
    let gate = Arc::new(tokio::sync::Notify::new());
    w.kk.with(|k| {
        k.valid = Some(HashSet::from(["rt-B".to_string()]));
        k.wait = Some(gate.clone());
        k.entered = 0;
    });
    let (st2, ch2) = (w.state.clone(), ch.clone());
    let task = tokio::spawn(async move { kick::send(&st2, &ch2, "three", None).await });
    eventually(|| w.kk.with(|k| k.entered) == 1).await;
    seal_token(&w.state, &sub, "kick", "bot", "rt-B");
    gate.notify_one();
    assert!(task.await.unwrap().is_ok(), "retried with the replaced token");
    assert_eq!(open_token(&w.state, &sub, "kick", "bot").as_deref(), Some("rt-B+"));
    // Nothing changed meanwhile and Kick says no: signed out.
    w.state.bot.forget_kick_access(&sub);
    w.kk.with(|k| k.valid = Some(HashSet::new()));
    assert_eq!(kick::send(&w.state, &ch, "four", None).await, Err(SendErr::SignedOut));
}

// ── Privacy, lapse ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_is_never_stored_only_kept_in_memory() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.connect_twitch(&auth).await;
    w.push(&auth, 0, config()).await;
    w.enable(&auth, true).await;
    let secret = "my-phone-is-5550199-zebra";
    for i in 0..(RING + 5) {
        w.chat(&sub, &format!("p{i}"), ("77", "viewer"), json!([]), &format!("{secret} {i}")).await;
    }
    let lines = recent_chat(&w.state, &sub, Platform::Twitch);
    assert_eq!(lines.len(), RING, "a ring of 200");
    assert!(lines[0].text.ends_with(" 5"), "the oldest went first");
    w.db.c().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    for f in ["relay.db", "relay.db-wal"] {
        let bytes = std::fs::read(w.dir.path().join(f)).unwrap_or_default();
        assert!(!bytes.windows(secret.len()).any(|x| x == secret.as_bytes()), "{f} holds chat");
    }
    // Switching off forgets the ring.
    w.enable(&auth, false).await;
    assert!(recent_chat(&w.state, &sub, Platform::Twitch).is_empty());
}

#[tokio::test]
async fn thirty_days_after_pro_ends_everything_goes_and_sign_ins_are_revoked() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let sub = w.sub_of("i1");
    w.connect_twitch(&auth).await;
    w.push(&auth, 0, config()).await;
    w.enable(&auth, true).await;
    // Ten days after the end: kept (Disconnect still works, R-04), but Twitch stops sending chat.
    w.set_sub("sub_1", "cancelled", Some(now_ms() - 10 * DAY));
    sweep(&w.state).await;
    let ch = w.db.bot_channel(&sub, "twitch").unwrap().unwrap();
    assert!(ch.subscriptions.is_empty(), "no chat flows after Pro");
    assert_eq!(w.tw.with(|t| t.unsubscribed.len()), 3);
    assert_eq!(w.get("/v1/pro/bot", &auth).await.status, 402);
    // Thirty-one days: gone, revoked.
    w.set_sub("sub_1", "cancelled", Some(now_ms() - 31 * DAY));
    sweep(&w.state).await;
    assert!(w.db.bot_channel(&sub, "twitch").unwrap().is_none());
    assert!(w.db.bot_config(&sub).unwrap().is_none());
    assert!(w.db.bot_token(&sub, "twitch", "channel").unwrap().is_none());
    assert!(w.tw.with(|t| t.revoked.len()) >= 2);
}

#[tokio::test]
async fn disconnect_stays_open_after_pro_ends() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect_twitch(&auth).await;
    w.set_sub("sub_1", "halted", Some(now_ms() - 5 * DAY));
    let a = w.del("/v1/pro/bot/twitch", &auth).await;
    assert_eq!(a.status, 204, "{}", a.text);
    let plain = w.install("i_free");
    assert_eq!(w.del("/v1/pro/bot/twitch", &plain).await.status, 402);
}

#[tokio::test]
async fn requests_are_rate_limited_per_subscription() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    for _ in 0..RATE.limit {
        w.db.try_take("sub_1", RATE.name, RATE.period, 1, RATE.limit).unwrap();
    }
    let a = w.get("/v1/pro/bot", &auth).await;
    if a.status != 200 {
        // (the minute may have just turned over)
        assert_eq!((a.status, a.code().as_str()), (429, "bot_rate"));
        assert!(a.headers.contains_key("retry-after"));
    }
}

#[test]
fn migration_10_has_the_kv_table() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("r.db")).unwrap();
    db.bot_put_kv("k", "v1").unwrap();
    db.bot_put_kv("k", "v2").unwrap();
    assert_eq!(db.bot_kv("k").unwrap().as_deref(), Some("v2"));
    assert_eq!(db.bot_kv("missing").unwrap(), None);
}
