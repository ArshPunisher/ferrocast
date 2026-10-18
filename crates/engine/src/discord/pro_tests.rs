//! The Ferrocast bot on the desktop: which events become perk events (pure), then go-live and
//! perk uploads against a fake relay and a fake webhook: the bot posts and the webhook stays
//! quiet, the webhook steps in when the bot can't, the perk queue waits, drops and survives a
//! restart.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use ferrocast_core::event::{Actor, Event, EventKind, Millis, Origin, Platform, Tier};
use ferrocast_core::money::Money;
use ferrocast_store::repo;
use serde_json::{Value, json};

use super::pro::{LiveRun, Queued, SAVE_KEY, Saved, perk_event};
use crate::{Engine, PlatformApps, now};

fn actor(platform: Platform, id: Option<&str>) -> Actor {
    let mut a = Actor::named(platform, "Asha");
    a.id = id.map(String::from);
    a
}

fn ev(platform: Platform, kind: EventKind) -> Event {
    Event::live(platform, Some(format!("s{}", n())), Millis(now().0), kind)
}

fn n() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn sub(platform: Platform, id: Option<&str>, tier: Tier) -> Event {
    ev(
        platform,
        EventKind::Subscription { actor: actor(platform, id), tier, months: 1, message: None, gifted_by: None },
    )
}

// ── Which events earn perks ──────────────────────────────────────────────────────────

#[test]
fn subs_memberships_and_paid_messages_become_perk_events() {
    let t = perk_event(&sub(Platform::Twitch, Some("141981764"), Tier::Tier2)).unwrap();
    assert_eq!(
        (t["platform"].as_str(), t["kind"].as_str(), t["tier"].as_str()),
        (Some("twitch"), Some("sub"), Some("2"))
    );
    assert_eq!(t["platform_user_id"], "141981764");
    let p = perk_event(&sub(Platform::Twitch, Some("1"), Tier::Prime)).unwrap();
    assert_eq!(p["tier"], "prime");
    let k = perk_event(&sub(Platform::Kick, Some("77"), Tier::Tier1)).unwrap();
    assert_eq!((k["platform"].as_str(), k["kind"].as_str()), (Some("kick"), Some("sub")));
    let y = perk_event(&sub(Platform::Youtube, Some("UC1"), Tier::Level { id: "gold".into(), name: "Gold".into() }))
        .unwrap();
    assert_eq!((y["kind"].as_str(), y["tier"].as_str()), (Some("member"), Some("Gold")));
    let paid = ev(
        Platform::Youtube,
        EventKind::PaidMessage {
            actor: actor(Platform::Youtube, Some("UC1")),
            money: Money::new(19_900, "INR"),
            content: None,
            sticker_url: None,
        },
    );
    let m = perk_event(&paid).unwrap();
    assert_eq!(
        (m["kind"].as_str(), m["amount_minor"].as_i64(), m["currency"].as_str()),
        (Some("tip"), Some(19_900), Some("INR"))
    );
    // No platform account, a tip-page tip, a test alert, a follow: nothing.
    assert!(perk_event(&sub(Platform::Twitch, None, Tier::Tier1)).is_none());
    assert!(perk_event(&sub(Platform::Tips, Some("x"), Tier::Tier1)).is_none());
    let mut test = sub(Platform::Twitch, Some("1"), Tier::Tier1);
    test.origin = Origin::Test;
    assert!(perk_event(&test).is_none());
    assert!(
        perk_event(&ev(Platform::Twitch, EventKind::Follow { actor: actor(Platform::Twitch, Some("1")) })).is_none()
    );
}

// ── Against a fake relay ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Relay {
    /// (path, body) of every Discord call.
    calls: Vec<(String, Value)>,
    /// What `/live` answers: status and body.
    live: Option<(u16, Value)>,
    /// What `/perk-events` answers.
    perks: Option<u16>,
    /// What `/perk-events` answers with 200 (default: one granted).
    perks_body: Option<Value>,
    /// `/perk-events` waits this long before answering.
    perks_delay_ms: u64,
    /// What `/live` answers for a stream end (`live: false`); `live` otherwise.
    ended: Option<(u16, Value)>,
    view: Value,
}

type Shared = Arc<Mutex<Relay>>;

async fn fake_relay() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn installs() -> axum::Json<Value> {
        axum::Json(json!({ "install_id": "i1", "install_secret": "s1" }))
    }
    async fn live(State(f): State<Shared>, axum::Json(body): axum::Json<Value>) -> (StatusCode, axum::Json<Value>) {
        let mut f = f.lock().unwrap();
        let end = body["live"] == false;
        f.calls.push(("/live".into(), body));
        let answer = if end { f.ended.clone().or_else(|| f.live.clone()) } else { f.live.clone() };
        let (s, v) = answer.unwrap_or((200, json!({ "posted": true, "edited": false, "reason": null })));
        (StatusCode::from_u16(s).unwrap(), axum::Json(v))
    }
    async fn perks(State(f): State<Shared>, axum::Json(body): axum::Json<Value>) -> (StatusCode, axum::Json<Value>) {
        let (s, v, delay) = {
            let mut f = f.lock().unwrap();
            f.calls.push(("/perk-events".into(), body));
            let s = f.perks.unwrap_or(200);
            let v = if s == 200 {
                f.perks_body.clone().unwrap_or(json!({ "events": 1, "granted": 1 }))
            } else {
                json!({ "error": { "code": "missing_permission", "message": "no", "hint": "Move Ferrocast's role above the perk roles in Server Settings › Roles." } })
            };
            (s, v, f.perks_delay_ms)
        };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        (StatusCode::from_u16(s).unwrap(), axum::Json(v))
    }
    async fn view(State(f): State<Shared>) -> axum::Json<Value> {
        axum::Json(f.lock().unwrap().view.clone())
    }
    let app = axum::Router::new()
        .route("/v1/installs", post(installs))
        .route("/v1/pro/discord", get(view).delete(|| async { StatusCode::NO_CONTENT }))
        .route("/v1/pro/discord/live", post(live))
        .route("/v1/pro/discord/perk-events", post(perks))
        .with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, shared)
}

async fn fake_webhook() -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/api/webhooks/{id}/{token}",
        post(move |axum::Json(v): axum::Json<Value>| {
            let s = s2.clone();
            async move {
                s.lock().unwrap().push(v);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    (format!("http://{addr}/api/webhooks/123456789012345678/tok"), seen)
}

async fn engine(relay: &str) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay.into()), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    (e, dir)
}

async fn remember_pro(e: &Engine, discord: bool) {
    let now = now().0;
    let status = json!({
        "pro": true, "status": "active", "plan": "monthly", "subscription_id": "sub_1",
        "current_end": now + 30 * 86_400_000, "access_until": now + 33 * 86_400_000, "checked_at": now,
        "features": { "discord": discord }, "quotas": {}
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
    e.discord_pro_round().await;
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

async fn twitch_account(e: &Engine) {
    let a = repo::settings::Account {
        platform: "twitch".into(),
        user_id: "141981764".into(),
        login: "Asha_Live".into(),
        display_name: "Asha".into(),
        avatar_url: None,
        scopes: vec![],
        connected_at: 1,
    };
    e.db(move |s| repo::settings::save_account(s, &a)).await.unwrap();
}

fn calls(f: &Shared, path: &str) -> Vec<Value> {
    f.lock().unwrap().calls.iter().filter(|(p, _)| p == path).map(|(_, b)| b.clone()).collect()
}

#[tokio::test]
async fn with_the_bot_posting_the_free_webhook_stays_quiet() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    let (hook, posts) = fake_webhook().await;
    e.set_discord_webhook_for_tests(&hook);
    twitch_account(&e).await;
    remember_pro(&e, true).await;
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: Some("Ranked".into()) })).await;
    eventually(|| !calls(&fake, "/live").is_empty()).await;
    let c = calls(&fake, "/live")[0].clone();
    assert_eq!(
        (c["platform"].as_str(), c["live"].as_bool(), c["title"].as_str()),
        (Some("twitch"), Some(true), Some("Ranked"))
    );
    assert_eq!(c["name"], "Asha");
    assert_eq!(c["url"], "https://www.twitch.tv/Asha_Live");
    assert_eq!(c["thumbnail_url"], "https://static-cdn.jtvnw.net/previews-ttv/live_user_asha_live-1280x720.jpg");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(posts.lock().unwrap().is_empty(), "the webhook didn't post: the bot did");
    eventually(|| e.discord_pro().saved.go_live).await;
    assert!(e.discord().await.unwrap().pro_go_live, "the free section says the bot posts");
    // The same platform again within 30 minutes: not even the relay is asked.
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls(&fake, "/live").len(), 1);
    // Viewers, then the end: the relay edits its post with the peak.
    e.ingest(ev(Platform::Twitch, EventKind::ViewerCount { count: 40 })).await;
    e.ingest(ev(Platform::Twitch, EventKind::ViewerCount { count: 132 })).await;
    e.ingest(ev(Platform::Twitch, EventKind::ViewerCount { count: 90 })).await;
    e.ingest(ev(Platform::Twitch, EventKind::StreamOffline)).await;
    eventually(|| calls(&fake, "/live").len() == 2).await;
    let end = calls(&fake, "/live")[1].clone();
    assert_eq!((end["live"].as_bool(), end["peak_viewers"].as_u64()), (Some(false), Some(132)));
    assert_eq!(end["started_at"], c["started_at"]);
}

#[tokio::test]
async fn when_the_bot_cant_post_the_webhook_does() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    let (hook, posts) = fake_webhook().await;
    e.set_discord_webhook_for_tests(&hook);
    remember_pro(&e, true).await;
    // The bot's go-live is off on the relay: the webhook posts, and the flag is remembered.
    fake.lock().unwrap().live = Some((200, json!({ "posted": false, "edited": false, "reason": "off" })));
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    eventually(|| posts.lock().unwrap().len() == 1).await;
    assert!(!e.discord().await.unwrap().pro_go_live);
    // The relay is down (502): the webhook again.
    fake.lock().unwrap().live = Some((502, json!({ "error": { "code": "discord_unavailable", "message": "x" } })));
    e.ingest(ev(Platform::Kick, EventKind::StreamOnline { title: None })).await;
    eventually(|| posts.lock().unwrap().len() == 2).await;
    // Ferrocast can't post in the channel (403): the webhook, and a notice to fix the role.
    let mut live = e.subscribe_live();
    fake.lock().unwrap().live = Some((
        403,
        json!({ "error": { "code": "missing_permission", "message": "Discord says Ferrocast is missing a permission.", "hint": "Move Ferrocast's role above the perk roles in Server Settings › Roles." } }),
    ));
    e.ingest(ev(Platform::Youtube, EventKind::StreamOnline { title: None })).await;
    eventually(|| posts.lock().unwrap().len() == 3).await;
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let f = live.recv().await.unwrap();
            if f.text.contains("discord-pro") {
                return f.text.to_string();
            }
        }
    })
    .await
    .unwrap();
    assert!(frame.contains("missing a permission"), "{frame}");
    // Without Pro the relay is never asked.
    let (relay2, fake2) = fake_relay().await;
    let (e2, _d2) = engine(&relay2).await;
    let (hook2, posts2) = fake_webhook().await;
    e2.set_discord_webhook_for_tests(&hook2);
    remember_pro(&e2, false).await;
    e2.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    eventually(|| posts2.lock().unwrap().len() == 1).await;
    e2.ingest(sub(Platform::Twitch, Some("1"), Tier::Tier1)).await;
    e2.discord_pro_round().await;
    assert!(fake2.lock().unwrap().calls.is_empty());
    assert!(e2.discord_pro().saved.queue.is_empty());
}

#[tokio::test]
async fn the_dashboard_view_tells_the_free_section_the_bot_posts() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().view = json!({ "connected": true, "channel_id": "900000000000000010", "go_live": { "enabled": true, "message": null } });
    e.discord_bot().await.unwrap();
    assert!(e.discord().await.unwrap().pro_go_live);
    fake.lock().unwrap().view =
        json!({ "connected": true, "channel_id": null, "go_live": { "enabled": true, "message": null } });
    e.discord_bot().await.unwrap();
    assert!(!e.discord().await.unwrap().pro_go_live, "no channel: the webhook still posts");
}

#[tokio::test]
async fn perk_events_queue_offline_and_go_up_in_batches() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().perks = Some(503);
    for i in 0..150 {
        e.ingest(sub(Platform::Twitch, Some(&format!("{}", 1000 + i)), Tier::Tier1)).await;
    }
    e.ingest(ev(Platform::Twitch, EventKind::Follow { actor: actor(Platform::Twitch, Some("9")) })).await;
    assert_eq!(e.discord_pro().saved.queue.len(), 150, "follows earn nothing");
    e.discord_pro_round().await;
    assert_eq!(calls(&fake, "/perk-events").len(), 1, "the first batch failed, so the rest waited");
    assert_eq!(e.discord_pro().saved.queue.len(), 150);
    assert!(e.discord_pro().retry_at > now().0, "backing off");
    // Saved: a restart keeps it.
    let saved = e.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.unwrap().unwrap();
    assert_eq!(saved.queue.len(), 150);
    e.discord_pro().saved = Saved::default();
    e.load_discord_pro().await;
    assert_eq!(e.discord_pro().saved.queue.len(), 150);
    // Back up: two batches, 100 then 50, in order.
    fake.lock().unwrap().perks = None;
    e.discord_pro().retry_at = 0;
    e.discord_pro_round().await;
    let sent = calls(&fake, "/perk-events");
    assert_eq!(sent.len(), 3);
    assert_eq!((sent[1].as_array().unwrap().len(), sent[2].as_array().unwrap().len()), (100, 50));
    assert_eq!(sent[1][0]["platform_user_id"], "1000");
    assert!(e.discord_pro().saved.queue.is_empty());
    // Refused for good (422): dropped. Ferrocast's role too low (403): kept, retried slowly.
    fake.lock().unwrap().perks = Some(422);
    e.ingest(sub(Platform::Kick, Some("5"), Tier::Tier1)).await;
    e.discord_pro_round().await;
    assert!(e.discord_pro().saved.queue.is_empty());
    fake.lock().unwrap().perks = Some(403);
    e.ingest(sub(Platform::Kick, Some("6"), Tier::Tier1)).await;
    e.discord_pro_round().await;
    assert_eq!(e.discord_pro().saved.queue.len(), 1);
    assert!(e.discord_pro().retry_at - now().0 > 10 * 60_000, "a long wait until the role is fixed");
    // Events older than 30 days are dropped without asking.
    e.discord_pro().saved.queue[0].event["at"] = json!(now().0 - 31 * 86_400_000);
    e.discord_pro_round().await;
    assert!(e.discord_pro().saved.queue.is_empty());
}

#[tokio::test]
async fn disconnecting_clears_the_queue_and_the_flag() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().perks = Some(503);
    e.ingest(sub(Platform::Twitch, Some("1"), Tier::Tier1)).await;
    e.discord_pro().saved.go_live = true;
    e.discord_bot_delete("").await.unwrap();
    assert!(e.discord_pro().saved.queue.is_empty() && !e.discord_pro().saved.go_live);
    let saved = e.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.unwrap().unwrap();
    assert!(saved.queue.is_empty(), "the store is emptied too");
}

// ── Review fixes: stream ends, "recent", perk batches ────────────────────────────────

#[tokio::test]
async fn a_stream_end_is_queued_and_retried_until_the_relay_has_it() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    twitch_account(&e).await;
    remember_pro(&e, true).await;
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    eventually(|| e.discord_pro().saved.live.contains_key("twitch")).await;
    let started = calls(&fake, "/live")[0]["started_at"].as_i64().unwrap();
    // The relay is down when the stream ends: the end waits, saved (a restart keeps it).
    fake.lock().unwrap().ended = Some((502, json!({ "error": { "code": "discord_unavailable", "message": "x" } })));
    e.ingest(ev(Platform::Twitch, EventKind::StreamOffline)).await;
    eventually(|| calls(&fake, "/live").len() == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(e.discord_pro().saved.ends.len(), 1);
    let saved = e.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.unwrap().unwrap();
    assert_eq!(saved.ends.len(), 1, "saved for after a restart");
    // Back: the next round delivers it, with when it really ended.
    fake.lock().unwrap().ended = None;
    e.discord_pro_round().await;
    let ends: Vec<Value> = calls(&fake, "/live").into_iter().filter(|c| c["live"] == false).collect();
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[1]["started_at"], started);
    assert!(ends[1]["ended_at"].as_i64().unwrap() <= now().0);
    assert!(e.discord_pro().saved.ends.is_empty());
    // Refused for good (no post to edit any more): dropped, not retried forever.
    e.discord_pro().saved.live.insert("kick".into(), LiveRun { started_at: now().0, peak: 0, seen_at: now().0 });
    fake.lock().unwrap().ended = Some((404, json!({ "error": { "code": "not_connected", "message": "x" } })));
    e.ingest(ev(Platform::Kick, EventKind::StreamOffline)).await;
    eventually(|| e.discord_pro().saved.ends.is_empty()).await;
}

#[tokio::test]
async fn a_restart_mid_stream_is_the_same_stream_and_a_missed_end_is_closed_first() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    let (hook, posts) = fake_webhook().await;
    e.set_discord_webhook_for_tests(&hook);
    remember_pro(&e, true).await;
    // Reported live before a restart, viewers counted a minute ago.
    let started = now().0 - 2 * 3_600_000;
    e.discord_pro()
        .saved
        .live
        .insert("twitch".into(), LiveRun { started_at: started, peak: 50, seen_at: now().0 - 60_000 });
    fake.lock().unwrap().live = Some((200, json!({ "posted": false, "edited": false, "reason": "recent" })));
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    eventually(|| calls(&fake, "/live").len() == 1).await;
    assert_eq!(calls(&fake, "/live")[0]["started_at"], started, "the same stream's start");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(posts.lock().unwrap().is_empty(), "the bot's post is up: the webhook stays quiet");
    // Kick: last seen hours ago and no end ever came. The old one is closed, then the new one.
    let old = now().0 - 5 * 3_600_000;
    e.discord_pro().saved.live.insert("kick".into(), LiveRun { started_at: old, peak: 9, seen_at: old + 3_600_000 });
    fake.lock().unwrap().live = None;
    e.ingest(ev(Platform::Kick, EventKind::StreamOnline { title: None })).await;
    eventually(|| calls(&fake, "/live").len() == 3).await;
    let c = calls(&fake, "/live");
    assert_eq!((c[1]["live"].as_bool(), c[1]["started_at"].as_i64()), (Some(false), Some(old)));
    assert_eq!((c[1]["ended_at"].as_i64(), c[1]["peak_viewers"].as_u64()), (Some(old + 3_600_000), Some(9)));
    assert_eq!(c[2]["live"], true);
    assert!(c[2]["started_at"].as_i64().unwrap() > old + 3_600_000, "a new stream");
}

#[tokio::test]
async fn recent_from_a_stream_this_desktop_didnt_report_lets_the_webhook_post() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    let (hook, posts) = fake_webhook().await;
    e.set_discord_webhook_for_tests(&hook);
    remember_pro(&e, true).await;
    fake.lock().unwrap().live = Some((200, json!({ "posted": false, "edited": false, "reason": "recent" })));
    e.ingest(ev(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    eventually(|| posts.lock().unwrap().len() == 1).await;
    assert!(!e.discord_pro().saved.live.contains_key("twitch"));
}

#[tokio::test]
async fn events_queued_or_dropped_while_a_batch_is_in_flight_are_never_lost() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    let ev = |i: u64| json!({ "platform": "twitch", "platform_user_id": i.to_string(), "kind": "sub", "at": now().0 });
    {
        let mut d = e.discord_pro();
        d.saved.queue = (0..1000).map(|i| Queued { seq: i, event: ev(i) }).collect();
        d.saved.next_seq = 1000;
    }
    fake.lock().unwrap().perks_delay_ms = 300;
    let round = tokio::spawn({
        let e = e.clone();
        async move { e.discord_pro_round().await }
    });
    eventually(|| calls(&fake, "/perk-events").len() == 1).await;
    // While the first 100 are in flight: 50 more arrive, so the 50 oldest (sent) fall off the
    // full queue; the next upload fails.
    {
        let mut d = e.discord_pro();
        for i in 1000..1050 {
            d.saved.queue.push_back(Queued { seq: i, event: ev(i) });
            d.saved.queue.pop_front();
        }
        d.saved.next_seq = 1050;
    }
    fake.lock().unwrap().perks = Some(503);
    round.await.unwrap();
    let d = e.discord_pro();
    assert_eq!(d.saved.queue.len(), 950, "only the 100 sent left the queue");
    assert_eq!(d.saved.queue[0].event["platform_user_id"], "100", "the first unsent is still there");
    assert_eq!(d.saved.queue.back().unwrap().seq, 1049);
}

#[tokio::test]
async fn a_refused_batch_is_retried_one_by_one_and_blocked_roles_warn_once() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    fake.lock().unwrap().perks = Some(503);
    for i in 0..3 {
        e.ingest(sub(Platform::Twitch, Some(&format!("{}", 10 + i)), Tier::Tier1)).await;
    }
    fake.lock().unwrap().perks = Some(422);
    e.discord_pro_round().await;
    let sent = calls(&fake, "/perk-events");
    assert_eq!(
        sent.iter().map(|b| b.as_array().unwrap().len()).collect::<Vec<_>>(),
        vec![3, 1, 1, 1],
        "the batch, then each event alone"
    );
    assert!(e.discord_pro().saved.queue.is_empty());
    // 200 with roles blocked: taken (not resent), and one notice.
    let mut live = e.subscribe_live();
    {
        let mut f = fake.lock().unwrap();
        f.perks = None;
        f.perks_body = Some(
            json!({ "events": 1, "granted": 0, "blocked": 1, "issue": { "rule_id": "dr_1", "code": "missing_permission" } }),
        );
    }
    e.ingest(sub(Platform::Twitch, Some("20"), Tier::Tier1)).await;
    e.discord_pro_round().await;
    assert!(e.discord_pro().saved.queue.is_empty());
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let f = live.recv().await.unwrap();
            if f.text.contains("discord-pro") {
                return f.text.to_string();
            }
        }
    })
    .await
    .unwrap();
    assert!(frame.contains("Server Settings"), "{frame}");
}

#[tokio::test]
async fn the_bots_view_turns_the_free_sections_line_on_at_once() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    // The remembered Pro status is from before the bot existed (no discord feature yet).
    remember_pro(&e, false).await;
    fake.lock().unwrap().view = json!({ "connected": true, "channel_id": "900000000000000010", "go_live": { "enabled": true, "message": null } });
    e.discord_bot().await.unwrap();
    assert!(e.discord().await.unwrap().pro_go_live);
}
