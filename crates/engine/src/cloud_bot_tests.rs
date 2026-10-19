//! The PC's side of the cloud bot: when to push (debounce, values once a minute, backoff,
//! refused configs), then against a fake relay: pushes with the version, a stale version read
//! and pushed again once, a refused config held until it changes, counter changes from the
//! cloud applied once (as deltas) and reported back, `bot_seen` on the link naming this PC's
//! chat platforms, and Connect sending this PC's own Twitch token.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use ferrocast_core::chatbot::CounterDef;
use ferrocast_core::command::{Access, Command};
use ferrocast_core::event::Platform;
use ferrocast_store::repo;
use serde_json::{Value, json};

use super::*;
use crate::{Engine, PlatformApps};

// ── When to push ─────────────────────────────────────────────────────────────────────

#[test]
fn a_change_waits_until_it_stands_still_for_2_s() {
    let mut p = Pusher::default();
    assert!(!p.due("a", "v", 0), "just seen");
    assert!(!p.due("a", "v", 1_999));
    assert!(p.due("a", "v", 2_000));
    p.pushed("a", "v", 2_000);
    assert!(!p.due("a", "v", 10_000), "nothing changed");
    // Typing: each change starts the 2 s again.
    assert!(!p.due("b", "v", 11_000));
    assert!(!p.due("c", "v", 12_500));
    assert!(!p.due("c", "v", 14_000));
    assert!(p.due("c", "v", 14_500));
    // A restart knows what it pushed last.
    let mut q = Pusher::with_pushed(Some("c".into()));
    assert!(!q.due("c", "v", 0));
}

#[test]
fn counter_values_alone_go_up_at_most_once_a_minute() {
    let mut p = Pusher::default();
    p.due("a", "1", 0);
    p.pushed("a", "1", 2_000);
    assert!(!p.due("a", "2", 30_000), "a counting stream doesn't push every second");
    assert!(p.due("a", "2", 62_000));
    p.pushed("a", "2", 62_000);
    assert!(!p.due("a", "2", 200_000), "unchanged");
}

#[test]
fn failures_back_off_from_2_s_to_5_min_and_honour_retry_after() {
    let mut p = Pusher::default();
    p.due("a", "v", 0);
    let waits: Vec<i64> = (0..10).map(|_| p.failed(0, None)).collect();
    assert_eq!(waits[..5], [2_000, 4_000, 8_000, 16_000, 32_000]);
    assert_eq!(*waits.last().unwrap(), BACKOFF_MAX_MS);
    assert!(!p.due("a", "v", BACKOFF_MAX_MS - 1));
    assert!(p.due("a", "v", BACKOFF_MAX_MS));
    let mut q = Pusher::default();
    assert_eq!(q.failed(0, Some(45)), 45_000, "the relay's Retry-After");
    assert_eq!(q.failed(0, Some(99_999)), BACKOFF_MAX_MS, "never more than 5 minutes");
    q.pushed("a", "v", 0);
    assert_eq!(q.failed(0, None), 2_000, "a success starts the backoff over");
}

#[test]
fn a_refused_config_waits_for_a_change() {
    let mut p = Pusher::default();
    p.due("a", "v", 0);
    assert!(p.due("a", "v", 2_000));
    p.refused("a", "v");
    assert!(!p.due("a", "v", 100_000));
    assert!(!p.due("b", "v", 100_000));
    assert!(p.due("b", "v", 102_000), "fixed: it goes up");
}

// ── Against a fake relay ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Relay {
    version: i64,
    pushes: Vec<Value>,
    /// Answers for the next pushes (status, body), else 200.
    answers: Vec<(u16, Value)>,
    enabled: bool,
    link_frames: Vec<Value>,
    /// Items to hand the PC on its link.
    items: Vec<Value>,
    acks: Vec<i64>,
    /// Bodies of `POST /v1/pro/bot/{platform}/connect`.
    connects: Vec<Value>,
}

type Shared = Arc<Mutex<Relay>>;

async fn fake_relay() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn installs() -> axum::Json<Value> {
        axum::Json(json!({ "install_id": "i1", "install_secret": "s1" }))
    }
    async fn view(State(f): State<Shared>) -> axum::Json<Value> {
        let f = f.lock().unwrap();
        axum::Json(json!({ "enabled": f.enabled, "version": f.version, "duty": { "who": "desktop" }, "channels": [],
                           "available": { "twitch": true, "kick": false } }))
    }
    async fn config(State(f): State<Shared>, axum::Json(b): axum::Json<Value>) -> (StatusCode, axum::Json<Value>) {
        let mut f = f.lock().unwrap();
        f.pushes.push(b.clone());
        if !f.answers.is_empty() {
            let (s, v) = f.answers.remove(0);
            if s != 200 {
                return (StatusCode::from_u16(s).unwrap(), axum::Json(v));
            }
        }
        if b["version"].as_i64() != Some(f.version) {
            let e = json!({ "error": { "code": "stale_version", "message": "changed on another PC", "hint": "" } });
            return (StatusCode::CONFLICT, axum::Json(e));
        }
        f.version += 1;
        (StatusCode::OK, axum::Json(json!({ "version": f.version, "updated_at": 1 })))
    }
    async fn enabled(State(f): State<Shared>, axum::Json(b): axum::Json<Value>) -> axum::response::Response {
        let mut f = f.lock().unwrap();
        if b["enabled"] == true && f.version == 0 {
            let e = json!({ "error": { "code": "no_platform", "message": "Connect first.", "hint": "h" } });
            return (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(e)).into_response();
        }
        f.enabled = b["enabled"].as_bool().unwrap_or(false);
        axum::Json(json!({ "enabled": f.enabled, "version": f.version, "channels": [] })).into_response()
    }
    async fn connect(State(f): State<Shared>, axum::Json(b): axum::Json<Value>) -> axum::response::Response {
        let mut f = f.lock().unwrap();
        f.connects.push(b.clone());
        if b["retry"] == true {
            return axum::Json(json!({ "enabled": f.enabled, "version": f.version, "channels": [] })).into_response();
        }
        (
            StatusCode::CREATED,
            axum::Json(json!({ "auth_url": "https://id.twitch.tv/oauth2/authorize?x", "expires_at": 1 })),
        )
            .into_response()
    }
    async fn link(State(f): State<Shared>, ws: WebSocketUpgrade) -> axum::response::Response {
        ws.on_upgrade(move |s| link_loop(f, s))
    }
    async fn link_loop(f: Shared, mut s: WebSocket) {
        let items: Vec<Value> = std::mem::take(&mut f.lock().unwrap().items);
        for (i, it) in items.into_iter().enumerate() {
            let frame = json!({ "type": "item", "id": i + 1, "kind": it["kind"], "payload": it["payload"] });
            if s.send(Message::Text(frame.to_string().into())).await.is_err() {
                return;
            }
        }
        while let Some(Ok(m)) = s.recv().await {
            if let Message::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                let mut g = f.lock().unwrap();
                if v["type"] == "ack" {
                    g.acks.push(v["id"].as_i64().unwrap_or(0));
                } else {
                    g.link_frames.push(v);
                }
            }
        }
    }
    let app = axum::Router::new()
        .route("/v1/installs", post(installs))
        .route("/v1/pro/bot", get(view))
        .route("/v1/pro/bot/config", put(config))
        .route("/v1/pro/bot/enabled", put(enabled))
        .route("/v1/pro/bot/{platform}/connect", post(connect))
        .route("/v1/link", get(link))
        .with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, shared)
}

async fn engine(relay: &str) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay.into()), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    (e, dir)
}

async fn remember_pro(e: &Engine, cloud_bot: bool) {
    let now = now().0;
    let status = json!({
        "pro": true, "status": "active", "plan": "monthly", "subscription_id": "sub_1",
        "current_end": now + 30 * 86_400_000, "access_until": now + 33 * 86_400_000, "checked_at": now,
        "features": { "cloud_bot": cloud_bot }, "quotas": {}
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
}

fn cmd(name: &str) -> Command {
    Command {
        id: String::new(),
        name: name.into(),
        aliases: vec![],
        response: "hi {user}".into(),
        access: Access::Everyone,
        cooldown_s: 5,
        user_cooldown_s: 5,
        platforms: vec![],
        enabled: true,
    }
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

#[tokio::test]
async fn the_config_goes_up_with_its_version_and_a_stale_one_is_read_again() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    e.save_command(cmd("!lurk")).await.unwrap();
    e.create_counter(CounterDef::new("deaths"), 3).await.unwrap();
    let (config, defs, values) = snapshot(&e);
    assert_eq!(config["commands"][0]["name"], "!lurk");
    assert_eq!(
        config["counters"][0],
        json!({ "name": "deaths", "command": "!deaths", "reply": "{counter}: {count}",
                                               "enabled": true, "value": 3 })
    );
    // The values aren't in the definitions' hash.
    e.change_counter("deaths", ferrocast_core::chatbot::CounterOp::Add(1)).await.unwrap();
    let (_, defs2, values2) = snapshot(&e);
    assert_eq!(defs, defs2);
    assert_ne!(values, values2);

    e.cloud_bot_push(config.clone(), &defs, &values).await;
    assert_eq!(fake.lock().unwrap().pushes[0]["version"], 0);
    assert_eq!(e.cloud_bot_saved().version, 1);
    assert_eq!(e.cloud_bot_saved().pushed_hash.as_deref(), Some(defs.as_str()));
    // Another PC pushed meanwhile (the relay is at 5): read and pushed again, once.
    fake.lock().unwrap().version = 5;
    e.cloud_bot_push(config.clone(), &defs, &values).await;
    {
        let f = fake.lock().unwrap();
        let versions: Vec<i64> = f.pushes.iter().map(|p| p["version"].as_i64().unwrap()).collect();
        assert_eq!(versions, [0, 1, 5]);
    }
    assert_eq!(e.cloud_bot_saved().version, 6);
    assert!(e.cloud_bot_saved().error.is_none());
    // Still stale after reading (a third PC in between): one retry only, then the error.
    fake.lock().unwrap().answers = vec![
        (409, json!({ "error": { "code": "stale_version", "message": "m", "hint": "" } })),
        (409, json!({ "error": { "code": "stale_version", "message": "m", "hint": "" } })),
    ];
    e.cloud_bot_push(config, &defs, &values).await;
    assert_eq!(fake.lock().unwrap().pushes.len(), 5);
    assert_eq!(e.cloud_bot_saved().error.unwrap()["code"], "stale_version");
    // The state survives a restart of the engine's memory.
    e.load_cloud_bot().await;
    assert_eq!(e.cloud_bot_saved().version, 6);
}

#[tokio::test]
async fn a_refused_config_shows_its_error_and_isnt_sent_again_until_it_changes() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().answers = vec![(
        413,
        json!({ "error": { "code": "too_many_timers", "message": "Up to 50 timers can run in the cloud.", "hint": "h" } }),
    )];
    fake.lock().unwrap().enabled = true;
    e.cloud_bot_set_enabled_for_tests(true);
    // The background round pushes after the 2 s debounce.
    eventually(|| !fake.lock().unwrap().pushes.is_empty()).await;
    eventually(|| e.cloud_bot_saved().error.is_some()).await;
    let err = e.cloud_bot_saved().error.unwrap();
    assert_eq!((err["code"].as_str(), err["hint"].as_str()), (Some("too_many_timers"), Some("h")));
    let v = e.cloud_bot_view().await.unwrap();
    assert_eq!(v["sync"]["state"], "error");
    assert_eq!(v["sync"]["error"]["code"], "too_many_timers");
    assert_eq!(v["pro_available"], true);
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    assert_eq!(fake.lock().unwrap().pushes.len(), 1, "the same config isn't sent again");
    // Fixed on the PC: it goes up.
    e.save_command(cmd("!new")).await.unwrap();
    eventually(|| fake.lock().unwrap().pushes.len() == 2).await;
    eventually(|| e.cloud_bot_saved().error.is_none()).await;
    assert_eq!(e.cloud_bot_view().await.unwrap()["sync"]["state"], "synced");
    // Switched off on the relay (another PC, a phone): this PC follows and stops pushing.
    fake.lock().unwrap().enabled = false;
    assert_eq!(e.cloud_bot_view().await.unwrap()["sync"]["state"], "off");
    assert!(!e.cloud_bot_saved().enabled);
}

#[tokio::test]
async fn switching_on_pushes_first_and_a_refusal_switches_it_off_here_too() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().answers = vec![(503, json!({ "error": { "code": "storage", "message": "m", "hint": "" } }))];
    // The push failed, so the relay refuses switching on (the fake wants a push first).
    let err = e.cloud_bot_enable(true).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 422, code: "no_platform", .. }), "{err:?}");
    assert!(!e.cloud_bot_saved().enabled);
    let v = e.cloud_bot_enable(true).await.unwrap();
    assert_eq!(v["enabled"], true);
    assert_eq!(v["sync"]["state"], "synced");
    assert!(e.cloud_bot_saved().enabled);
    let v = e.cloud_bot_enable(false).await.unwrap();
    assert_eq!((v["enabled"].as_bool(), v["sync"]["state"].as_str()), (Some(false), Some("off")));
    // Without Pro's feature nothing is pushed by the round.
    remember_pro(&e, false).await;
    e.cloud_bot_set_enabled_for_tests(true);
    let before = fake.lock().unwrap().pushes.len();
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(fake.lock().unwrap().pushes.len(), before);
    assert!(!e.cloud_bot_on().await);
}

#[tokio::test]
async fn counters_changed_in_the_cloud_are_applied_once_and_reported_back() {
    let (relay, fake) = fake_relay().await;
    // The cloud's mod did `!deaths +2` (3 → 5 there) while this PC counted to 10 itself.
    fake.lock().unwrap().items = vec![
        json!({ "kind": "bot_counter", "payload": { "id": 7, "name": "deaths", "op": "add", "delta": 2, "value": 5, "at": 1 } }),
        json!({ "kind": "bot_counter", "payload": { "id": 7, "name": "deaths", "op": "add", "delta": 2, "value": 5, "at": 1 } }),
        json!({ "kind": "bot_counter", "payload": { "id": 8, "name": "gone", "op": "set", "value": 1, "at": 2 } }),
    ];
    fake.lock().unwrap().enabled = true;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    e.create_counter(CounterDef::new("deaths"), 3).await.unwrap();
    e.change_counter("deaths", ferrocast_core::chatbot::CounterOp::Set(10)).await.unwrap();
    e.cloud_bot_set_enabled_for_tests(true);
    // The round opens the link; the items arrive and are acked.
    eventually(|| fake.lock().unwrap().acks.len() == 3).await;
    assert_eq!(
        e.counter_view("deaths").unwrap().value,
        12,
        "both changes kept (not overwritten with the cloud's 5), and a resent one isn't applied twice"
    );
    assert_eq!(e.cloud_bot_saved().counters_seen, 8, "a deleted counter's change still counts as seen");
    // The next push reports it, with the value.
    let (config, defs, values) = snapshot(&e);
    e.cloud_bot_push(config, &defs, &values).await;
    let last = fake.lock().unwrap().pushes.last().cloned().unwrap();
    assert_eq!(last["counters_seen"], 8);
    assert_eq!(last["config"]["counters"][0]["value"], 12);
    // A `set` sets; an item from an older relay (no `op`) sets too.
    e.apply_cloud_counter(&json!({ "id": 9, "name": "deaths", "op": "set", "value": 4, "at": 3 })).await;
    assert_eq!(e.counter_view("deaths").unwrap().value, 4);
    e.apply_cloud_counter(&json!({ "id": 10, "name": "deaths", "op": "add", "delta": -1, "value": 999, "at": 4 }))
        .await;
    assert_eq!(e.counter_view("deaths").unwrap().value, 3);
    e.apply_cloud_counter(&json!({ "id": 11, "name": "deaths", "value": 30, "at": 5 })).await;
    assert_eq!(e.counter_view("deaths").unwrap().value, 30);
}

#[tokio::test]
async fn bans_made_from_a_phone_while_the_pc_was_off_join_the_ban_list_once() {
    let (relay, fake) = fake_relay().await;
    let ban = json!({ "kind": "cloud_moderation", "payload": {
        "platform": "twitch", "action": "ban", "target_id": "77", "target_login": "viewer77",
        "target_name": "Viewer 77", "duration": null, "reason": "spam", "by_device": "Meera",
        "by_role": "mod", "at": 1_700_000_000_000_i64 } });
    fake.lock().unwrap().items = vec![
        ban.clone(),
        // Resent before the ack landed: not listed twice.
        ban,
        json!({ "kind": "cloud_moderation", "payload": {
            "platform": "kick", "action": "ban", "target_id": "4242", "by_device": "My phone",
            "by_role": "owner", "at": 1_700_000_000_001_i64 } }),
        json!({ "kind": "cloud_moderation", "payload": {
            "platform": "twitch", "action": "timeout", "target_id": "78", "duration": 600,
            "by_device": "Meera", "by_role": "mod", "at": 1 } }),
        json!({ "kind": "cloud_moderation", "payload": { "platform": "youtube", "action": "ban", "target_id": "x" } }),
    ];
    fake.lock().unwrap().enabled = true;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    e.cloud_bot_set_enabled_for_tests(true);
    eventually(|| fake.lock().unwrap().acks.len() == 5).await;
    let bans = e.bans().entries;
    assert_eq!(bans.len(), 2, "{bans:?}");
    let tw = bans.iter().find(|b| b.platform == Platform::Twitch).unwrap();
    assert_eq!(
        (tw.user_id.as_deref(), tw.login.as_str(), tw.display_name.as_str(), tw.reason.as_deref()),
        (Some("77"), "viewer77", "Viewer 77", Some("spam"))
    );
    assert_eq!((tw.by.as_deref(), tw.offline, tw.at.0), (Some("Meera"), true, 1_700_000_000_000));
    let kk = bans.iter().find(|b| b.platform == Platform::Kick).unwrap();
    assert_eq!((kk.login.as_str(), kk.by.as_deref(), kk.offline), ("4242", None, true), "the streamer's own phone");
    // Kept like any ban: on disk, and Unban takes it off (no Twitch connected here to ask).
    e.unban(Platform::Twitch, "77").await.unwrap();
    assert_eq!(e.bans().entries.len(), 1);
}

#[tokio::test]
async fn connect_sends_this_pcs_own_twitch_token_and_never_one_from_the_request() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    // No Twitch connected here: nothing to prove with (the relay says `twitch_not_linked`),
    // and a token in the request is dropped.
    let v = e
        .cloud_bot_connect("twitch", json!({ "account": "channel", "owner_token": "forged", "retry": true }))
        .await
        .unwrap();
    assert!(v["auth_url"].as_str().unwrap().starts_with("https://"));
    let sent = fake.lock().unwrap().connects.last().cloned().unwrap();
    assert_eq!(sent, json!({ "account": "channel" }), "{sent}");
    // Twitch connected: its current token goes along (no network: it's fresh).
    let tokens = ferrocast_twitch::auth::Tokens {
        access_token: "desk-at".into(),
        refresh_token: "desk-rt".into(),
        expires_at: now().0 + 3_600_000,
        scopes: vec![],
    };
    let auth = ferrocast_twitch::Auth::new(
        reqwest::Client::new(),
        ferrocast_twitch::Endpoints::default(),
        "desktop-client".into(),
        tokens,
        Arc::new(|_| Ok(())),
    );
    e.cloud_bot_set_twitch(Some(Arc::new(auth)));
    e.cloud_bot_connect("twitch", json!({ "account": "bot" })).await.unwrap();
    let sent = fake.lock().unwrap().connects.last().cloned().unwrap();
    assert_eq!((sent["account"].as_str(), sent["owner_token"].as_str()), (Some("bot"), Some("desk-at")));
    // Kick never gets it.
    e.cloud_bot_connect("kick", json!({})).await.unwrap();
    assert!(fake.lock().unwrap().connects.last().unwrap().get("owner_token").is_none());
    // Try again: the relay's view with this PC's side.
    let v = e.cloud_bot_retry("twitch").await.unwrap();
    assert_eq!(fake.lock().unwrap().connects.last().unwrap()["retry"], true);
    assert!(v["sync"].is_object());
    assert!(matches!(e.cloud_bot_retry("tiktok").await, Err(EngineError::Refused { status: 404, .. })));
}

#[tokio::test]
async fn the_link_says_bot_seen_while_the_cloud_bot_is_on() {
    let (relay, fake) = fake_relay().await;
    // Switched on from another PC: this one never opened the panel, it follows the relay.
    fake.lock().unwrap().enabled = true;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    eventually(|| e.cloud_bot_saved().enabled).await;
    // The first beat goes out as soon as the link is up, naming this PC's chat platforms.
    eventually(|| fake.lock().unwrap().link_frames.iter().any(|f| f["type"] == "bot_seen")).await;
    let beat = fake.lock().unwrap().link_frames.iter().find(|f| f["type"] == "bot_seen").cloned().unwrap();
    assert_eq!(beat["platforms"], json!([]), "no chat platform connected here: the cloud covers all");
    assert_eq!(HEARTBEAT, Duration::from_secs(30));
    // Connected (or reconnecting for a moment) counts; signed out or off doesn't.
    {
        let mut st = e.state_mut();
        for (p, state) in [("twitch", "connected"), ("kick", "sign_in_needed"), ("youtube", "connected")] {
            let mut v = crate::state::ConnectorView::off();
            v.state = state.into();
            st.connectors.insert(p.into(), v);
        }
    }
    assert_eq!(e.cloud_bot_platforms(), ["twitch"]);
    e.state_mut().connectors.get_mut("kick").unwrap().state = "reconnecting".into();
    assert_eq!(e.cloud_bot_platforms(), ["twitch", "kick"]);
}
