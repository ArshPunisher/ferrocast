//! Engine tests with a fake platform that records every action.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError};
use ferrocast_core::command::{Access, Command, Timer};
use ferrocast_core::event::*;
use ferrocast_core::goal::{Goal, GoalKind};
use ferrocast_core::loyalty::MAX_ADJUST;
use ferrocast_core::moderation::{Capabilities, ModeChange};
use ferrocast_core::money::Money;
use ferrocast_core::queue::AckPhase;
use serde_json::Value;

use super::*;

#[derive(Debug, Default)]
struct FakePlatform {
    platform: Option<Platform>,
    log: Mutex<Vec<String>>,
    fail_bans: bool,
}

impl FakePlatform {
    fn new(p: Platform) -> Arc<Self> {
        Arc::new(FakePlatform { platform: Some(p), ..Default::default() })
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn push(&self, s: String) -> ferrocast_connector::Result<()> {
        self.log.lock().unwrap().push(s);
        Ok(())
    }
}

#[async_trait]
impl Connector for FakePlatform {
    fn platform(&self) -> Platform {
        self.platform.unwrap_or(Platform::Twitch)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            modes: vec![ChatMode::Slow, ChatMode::FollowersOnly],
            can_ban: true,
            can_timeout: true,
            can_delete: true,
        }
    }
    async fn send_chat(&self, text: &str, reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
        self.push(format!("say {text} reply={}", reply_to.unwrap_or("-")))
    }
    async fn delete_message(&self, id: &str) -> ferrocast_connector::Result<()> {
        self.push(format!("delete {id}"))
    }
    async fn timeout(&self, user: &str, s: u32, _r: Option<&str>) -> ferrocast_connector::Result<()> {
        self.push(format!("timeout {user} {s}"))
    }
    async fn ban(&self, user: &str, _r: Option<&str>) -> ferrocast_connector::Result<()> {
        if self.fail_bans {
            return Err(ConnectorError::Refused { platform: self.platform(), message: "no".into() });
        }
        self.push(format!("ban {user}"))
    }
    async fn unban(&self, user: &str) -> ferrocast_connector::Result<()> {
        self.push(format!("unban {user}"))
    }
    async fn set_mode(&self, c: ModeChange) -> ferrocast_connector::Result<()> {
        self.push(format!("mode {:?} {}", c.mode, c.enabled))
    }
    async fn last_game(&self, login: &str) -> ferrocast_connector::Result<Option<String>> {
        self.push(format!("lookup {login}"))?;
        Ok((login == "friend").then(|| "Hades II".to_string()))
    }
}

fn mem() -> Arc<dyn ferrocast_secrets::Secrets> {
    Arc::new(ferrocast_secrets::MemorySecrets::default())
}

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Engine::start_with_secrets(dir.path(), mem()).await.unwrap(), dir)
}

fn chat(p: Platform, id: &str, user: &str, text: &str) -> Event {
    let mut a = Actor::named(p, user);
    a.id = Some(format!("{user}-id"));
    Event::live(
        p,
        Some(id.into()),
        Millis(0),
        EventKind::ChatMessage { actor: a, content: Content::plain(text), reply_to: None, is_action: false },
    )
}

/// Waits until `f` is true or fails after 3 s (background tasks run on their own time).
async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

async fn next_overlay_frame(rx: &mut tokio::sync::broadcast::Receiver<frames::OverlayFrame>, kind: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, rx.recv()).await.expect("timed out").expect("closed");
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == kind {
            return v;
        }
    }
}

#[tokio::test]
async fn a_test_alert_reaches_overlays_and_the_ack_advances_the_queue() {
    let (e, _d) = engine().await;
    let mut rx = e.subscribe_overlays();
    e.test("follow", None).await.unwrap();
    e.test("raid", None).await.unwrap();
    let first = next_overlay_frame(&mut rx, "alert").await;
    assert_eq!(first["data"]["title"], "NovaRiver followed!");
    assert_eq!(first["data"]["test"], true);
    let id = first["data"]["alert_id"].as_str().unwrap().to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while e.queue().await.waiting.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(e.queue().await.waiting.len(), 1, "the raid waits");
    e.ack(&id, AckPhase::Finished).await;
    let second = next_overlay_frame(&mut rx, "alert").await;
    assert_eq!(second["data"]["kind"], "raid");
}

#[tokio::test]
async fn duplicate_platform_events_are_dropped() {
    let (e, _d) = engine().await;
    let mut live = e.subscribe_live();
    let ev = chat(Platform::Twitch, "m-1", "viewer", "hello");
    e.ingest(ev.clone()).await;
    e.ingest(ev).await;
    let mut events = 0;
    while let Ok(f) = live.try_recv() {
        if f.text.contains("\"type\":\"event\"") {
            events += 1;
        }
    }
    assert_eq!(events, 1);
    assert_eq!(e.events(None, 50, vec![]).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_filter_deletes_hides_and_tells_the_platform() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let mut f = e.filter();
    f.words.list.push("badword".into());
    e.save_filter(f).await.unwrap();
    let mut rx = e.subscribe_overlays();
    e.ingest(chat(Platform::Twitch, "m-bad", "troll", "b.a.d.w.o.r.d")).await;
    e.ingest(chat(Platform::Twitch, "m-ok", "fan", "great stream")).await;
    let shown = next_overlay_frame(&mut rx, "event").await;
    assert_eq!(shown["data"]["source_id"], "m-ok", "the filtered message never reached overlays");
    eventually(|| tw.log().contains(&"delete m-bad".to_string())).await;
    let o = e.create_overlay("Main", ferrocast_core::overlay::Preset::ChatOnly).await.unwrap();
    let ov = e.overlay(&o.id).await.unwrap();
    let snap = e.overlay_snapshot(&ov).await.unwrap();
    let ids: Vec<_> = snap["state"]["chat"].as_array().unwrap().iter().map(|c| c["source_id"].clone()).collect();
    assert_eq!(ids, vec![Value::from("m-ok")]);
}

fn first_time(mut e: Event) -> Event {
    if let EventKind::ChatMessage { actor, .. } = &mut e.kind {
        actor.roles = Roles::FIRST_TIME.union(Roles::SUBSCRIBER);
    }
    e
}

#[tokio::test]
async fn held_messages_wait_then_are_released_or_dropped_once() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let mut live = e.subscribe_live();
    let mut overlays = e.subscribe_overlays();
    e.ingest(first_time(chat(Platform::Twitch, "m1", "newbie", "hey check mysite.dev"))).await;
    e.ingest(first_time(chat(Platform::Twitch, "m2", "other", "and mine at spam.xyz"))).await;
    // The dashboard sees the message, then the hold mark.
    let mut kinds = Vec::new();
    while let Ok(f) = live.try_recv() {
        kinds.push(serde_json::from_str::<Value>(&f.text).unwrap()["type"].as_str().unwrap_or("").to_string());
    }
    let pos = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(pos("event") < pos("held"), "{kinds:?}");
    assert!(overlays.try_recv().is_err(), "held chat never reaches overlays");
    let held = e.held().await.unwrap();
    assert_eq!(held.len(), 2);
    e.release_held(&held[0].id).await.unwrap();
    let shown = next_overlay_frame(&mut overlays, "event").await;
    assert_eq!(shown["data"]["source_id"], "m1", "a released message shows in chat overlays");
    assert!(matches!(e.release_held(&held[0].id).await, Err(EngineError::Missing { code: "held_not_found", .. })));
    e.drop_held(&held[1].id).await.unwrap();
    eventually(|| tw.log().contains(&"delete m2".to_string())).await;
    assert!(e.held().await.unwrap().is_empty());
}

#[tokio::test]
async fn panic_mode_holds_first_time_chatters() {
    let (e, _d) = engine().await;
    e.register_connector(FakePlatform::new(Platform::Twitch));
    e.ingest(first_time(chat(Platform::Twitch, "a1", "early", "hello!"))).await;
    assert!(e.held().await.unwrap().is_empty(), "outside panic mode a plain hello is fine");
    e.panic(true).await.unwrap();
    e.ingest(first_time(chat(Platform::Twitch, "a2", "raider", "hello!"))).await;
    e.ingest(chat(Platform::Twitch, "a3", "regular", "hello!")).await;
    let held: Vec<_> = e.held().await.unwrap().into_iter().map(|e| e.source_id.unwrap_or_default()).collect();
    assert_eq!(held, ["a2"], "only first-time chatters wait");
    let mut cfg = e.panic_state().1;
    cfg.hold_new_accounts = false;
    e.save_panic(cfg).await.unwrap();
    e.ingest(first_time(chat(Platform::Twitch, "a4", "raider2", "hello!"))).await;
    assert_eq!(e.held().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_platform_that_needs_signing_in_shows_one_reconnect_notice() {
    let (e, _d) = engine().await;
    let mut live = e.subscribe_live();
    let needs = || Event::live(Platform::Kick, None, Millis(0), EventKind::Connector(ConnectorStatus::SignInNeeded));
    e.ingest(needs()).await;
    e.ingest(needs()).await;
    let mut notices = Vec::new();
    while let Ok(f) = live.try_recv() {
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "notice" {
            notices.push(v);
        }
    }
    assert_eq!(notices.len(), 1, "one banner, not one per retry");
    assert_eq!(notices[0]["data"]["action"]["href"], "#/settings/accounts");
    assert!(notices[0]["data"]["message"].as_str().unwrap().starts_with("Kick"));
}

#[tokio::test]
async fn session_numbers_follow_tips_refunds_bits_and_viewers() {
    let (e, _d) = engine().await;
    let live = |p: Platform, kind: EventKind| Event::live(p, None, Millis(0), kind);
    e.ingest(live(Platform::Twitch, EventKind::StreamOnline { title: None })).await;
    e.ingest(live(Platform::Youtube, EventKind::StreamOnline { title: None })).await;
    let tip = serde_json::json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_s",
        "amount_minor": 50_000, "digits": 2, "currency": "INR", "name": "Asha" });
    e.ingest(crate::relay::tip_event(&tip).unwrap()).await;
    let mut bits = ferrocast_core::sample::sample("platform_gift", Platform::Twitch, Millis(0)).unwrap();
    bits.source_id = Some("cheer-1".into());
    bits.origin = Origin::Live;
    e.ingest(bits).await;
    e.ingest(live(Platform::Twitch, EventKind::ViewerCount { count: 100 })).await;
    e.ingest(live(Platform::Youtube, EventKind::ViewerCount { count: 50 })).await;
    let (s, _) = e.session_view();
    assert_eq!(s.tips, vec![Money::new(50_000, "INR")]);
    assert_eq!((s.bits, s.peak_viewers), (500, 150));
    // YouTube ends; its last count no longer adds up.
    e.ingest(live(Platform::Youtube, EventKind::StreamOffline)).await;
    e.ingest(live(Platform::Twitch, EventKind::ViewerCount { count: 120 })).await;
    assert_eq!(e.session_view().0.peak_viewers, 150);
    let refund = serde_json::json!({ "provider": "razorpay", "status": "refunded", "payment_id": "pay_s" });
    e.ingest(crate::relay::tip_event(&refund).unwrap()).await;
    assert_eq!(e.session_view().0.tips, vec![Money::new(0, "INR")], "refunds come off this stream's total");
    let tip2 = serde_json::json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_t",
        "amount_minor": 20_000, "digits": 2, "currency": "INR", "name": "Ravi" });
    e.ingest(crate::relay::tip_event(&tip2).unwrap()).await;
    for status in ["refunded", "disputed"] {
        let r = serde_json::json!({ "provider": "razorpay", "status": status, "payment_id": "pay_t" });
        e.ingest(crate::relay::tip_event(&r).unwrap()).await;
    }
    let tip3 = serde_json::json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_u",
        "amount_minor": 30_000, "digits": 2, "currency": "INR", "name": "Meera" });
    e.ingest(crate::relay::tip_event(&tip3).unwrap()).await;
    assert_eq!(e.session_view().0.tips, vec![Money::new(30_000, "INR")], "a refund and a dispute take it back once");
}

#[tokio::test]
async fn held_marks_are_replayed_with_their_messages() {
    let (e, _d) = engine().await;
    e.register_connector(FakePlatform::new(Platform::Twitch));
    let before = e.current_seq();
    e.ingest(first_time(chat(Platform::Twitch, "h1", "newbie", "visit spam.xyz"))).await;
    let (frames, gap) = e.events_since(before);
    assert!(!gap);
    let types: Vec<String> = frames
        .iter()
        .map(|f| serde_json::from_str::<Value>(&f.text).unwrap()["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(types, ["event", "held", "chatter"], "a first message's mark is replayed with it too");
    assert!(e.current_seq() > 1_000_000_000_000, "seqs start from the clock, so they grow across restarts");
}

#[tokio::test]
async fn a_restore_keeps_panic_mode_on() {
    let (e, _d) = engine().await;
    e.register_connector(FakePlatform::new(Platform::Twitch));
    e.panic(true).await.unwrap();
    e.reload().await.unwrap();
    assert!(e.panic_state().0, "importing a pack mid-raid doesn't quietly turn panic off");
}

#[tokio::test]
async fn commands_reply_on_the_source_platform() {
    let (e, _d) = engine().await;
    let kick = FakePlatform::new(Platform::Kick);
    e.register_connector(kick.clone());
    e.save_command(Command {
        id: "dc".into(),
        name: "!discord".into(),
        aliases: vec![],
        response: "Join us, {user}!".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();
    e.ingest(chat(Platform::Kick, "k1", "Sam", "!discord")).await;
    eventually(|| kick.log().contains(&"say Join us, Sam! reply=k1".to_string())).await;
}

#[tokio::test]
async fn ban_everywhere_uses_linked_accounts_and_reports_failures() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    let kick = Arc::new(FakePlatform { platform: Some(Platform::Kick), fail_bans: true, ..Default::default() });
    e.register_connector(tw.clone());
    e.register_connector(kick.clone());
    e.ingest(chat(Platform::Twitch, "t1", "raider", "hi")).await;
    e.ingest(chat(Platform::Kick, "k1", "raider_k", "hi")).await;
    let store = e.store();
    tokio::task::spawn_blocking(move || {
        ferrocast_store::repo::viewers::link(&store, Platform::Kick, "raider_k-id", Platform::Twitch, "raider-id")
    })
    .await
    .unwrap()
    .unwrap();
    let out =
        e.ban(Platform::Twitch, "raider-id", "raider", Some("Raider"), Some("hate raid".into()), true).await.unwrap();
    assert_eq!(out.ok, vec![Platform::Twitch]);
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].platform, Platform::Kick);
    assert!(tw.log().contains(&"ban raider-id".to_string()));
    assert_eq!(e.bans().entries.len(), 1);
    // The banned account chats again on Twitch: banned on sight and hidden.
    e.ingest(chat(Platform::Twitch, "t2", "raider", "back")).await;
    eventually(|| tw.log().iter().filter(|l| *l == "ban raider-id").count() == 2).await;
}

#[tokio::test]
async fn tips_alert_only_when_captured_and_refunds_take_goals_back() {
    let (e, _d) = engine().await;
    e.save_goal(Goal {
        id: "mic".into(),
        title: "New mic".into(),
        kind: GoalKind::Tips,
        target: 1_000_000,
        current: 0,
        currency: Some("INR".into()),
        starts_at: Millis(0),
        ends_at: None,
    })
    .await
    .unwrap();
    let tip = |status| {
        Event::live(
            Platform::Tips,
            Some("pay_9".into()),
            Millis(0),
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Asha"),
                money: Money::new(50_000, "INR"),
                message: None,
                provider: TipProvider::Razorpay,
                payment_ref: "pay_9".into(),
                status,
            },
        )
    };
    // The provider sends the same payment id twice: pending, then captured.
    e.ingest(tip(PaymentStatus::Pending)).await;
    assert!(e.queue().await.playing.is_none());
    assert_eq!(e.goals()[0].current, 0);
    e.ingest(tip(PaymentStatus::Captured)).await;
    eventually(|| e.goals()[0].current == 50_000).await;
    assert!(e.queue().await.playing.is_some());
    e.ingest(Event::live(
        Platform::Tips,
        Some("refund_1".into()),
        Millis(0),
        EventKind::Reversal { original_source_id: "pay_9".into(), reason: ReversalReason::Chargeback },
    ))
    .await;
    assert_eq!(e.goals()[0].current, 0);
}

#[tokio::test]
async fn a_tip_whose_refund_arrived_first_never_alerts_or_counts() {
    let (e, _d) = engine().await;
    e.save_goal(Goal {
        id: "mic".into(),
        title: "New mic".into(),
        kind: GoalKind::Tips,
        target: 1_000_000,
        current: 0,
        currency: Some("INR".into()),
        starts_at: Millis(0),
        ends_at: None,
    })
    .await
    .unwrap();
    // Relay items, in the wrong order: the refund, then the capture, then a replay of it.
    let refund = serde_json::json!({ "provider": "razorpay", "status": "refunded", "payment_id": "pay_late" });
    let capture = serde_json::json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_late",
        "amount_minor": 50_000, "digits": 2, "currency": "INR", "name": "Asha" });
    e.ingest(crate::relay::tip_event(&refund).unwrap()).await;
    e.ingest(crate::relay::tip_event(&capture).unwrap()).await;
    e.ingest(crate::relay::tip_event(&capture).unwrap()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(e.goals()[0].current, 0);
    assert!(e.queue().await.playing.is_none(), "no alert for money that was given back");
    // A normal tip in another currency still counts only in its own currency's goal.
    let usd = serde_json::json!({ "provider": "stripe", "status": "captured", "payment_id": "pi_2",
        "amount_minor": 500, "digits": 2, "currency": "USD", "name": "Sam" });
    e.ingest(crate::relay::tip_event(&usd).unwrap()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(e.queue().await.playing.is_some(), "a normal tip still alerts");
    assert_eq!(e.goals()[0].current, 0, "dollars never land in a rupee goal");
}

#[tokio::test]
async fn panic_mode_switches_modes_and_pauses_alerts() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let mut live = e.subscribe_live();
    let result = e.panic(true).await.unwrap();
    assert_eq!(result.applied.len(), 1);
    assert_eq!(result.applied[0].modes, vec!["followers_only".to_string(), "slow".to_string()]);
    let mut kinds = Vec::new();
    while let Ok(f) = live.try_recv() {
        let v = serde_json::from_str::<Value>(&f.text).unwrap();
        if v["type"] == "notice" {
            assert_eq!(v["data"]["key"], "panic", "panic notices replace each other on the dashboard");
        }
        kinds.push(v["type"].as_str().unwrap_or("").to_string());
    }
    assert!(kinds.contains(&"panic".to_string()) && kinds.contains(&"notice".to_string()), "{kinds:?}");
    assert!(e.queue().await.paused);
    assert!(tw.log().iter().any(|l| l.starts_with("mode FollowersOnly true")));
    e.panic(false).await.unwrap();
    assert!(!e.queue().await.paused);
    assert!(tw.log().iter().any(|l| l.starts_with("mode Slow false")));
}

#[tokio::test]
async fn the_queue_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
        e.queue_pause().await;
        e.test("tip", None).await.unwrap();
        e.test("raid", None).await.unwrap();
        eventually(|| true).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while e.queue().await.waiting.len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
    let q = e.queue().await;
    assert!(q.paused);
    assert_eq!(q.waiting.len(), 2);
}

#[tokio::test]
async fn timers_post_only_while_live_and_after_chat() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.save_timer(Timer {
        id: "soc".into(),
        name: "socials".into(),
        message: "Follow on {platform}".into(),
        every_min: 10,
        min_chat_lines: 2,
        platforms: vec![Platform::Twitch],
        enabled: true,
    })
    .await
    .unwrap();
    let mut last = Default::default();
    assert!(timers::tick(&e, &mut last, Millis(0)).is_empty(), "not live");
    e.ingest(Event::live(Platform::Twitch, Some("on".into()), Millis(0), EventKind::StreamOnline { title: None }))
        .await;
    assert!(timers::tick(&e, &mut last, Millis(0)).is_empty(), "no chat yet");
    e.ingest(chat(Platform::Twitch, "c1", "a", "hi")).await;
    e.ingest(chat(Platform::Twitch, "c2", "b", "hey")).await;
    assert_eq!(timers::tick(&e, &mut last, Millis(0)).len(), 1);
    assert!(timers::tick(&e, &mut last, Millis(60_000)).is_empty(), "too soon");
    eventually(|| tw.log().contains(&"say Follow on Twitch reply=-".to_string())).await;
}

#[tokio::test]
async fn backfill_goes_to_the_feed_with_a_reason() {
    let (e, _d) = engine().await;
    let mut ev = Event::live(
        Platform::Twitch,
        Some("f-old".into()),
        Millis(0),
        EventKind::Follow { actor: Actor::named(Platform::Twitch, "late") },
    );
    ev.origin = Origin::Backfill;
    e.ingest(ev).await;
    let logged = e.events(None, 5, vec![]).await.unwrap();
    let why = e.why(&logged[0].id).unwrap();
    assert!(!why.alerted);
    assert!(why.reasons[0].contains("reconnect"));
    assert!(e.queue().await.playing.is_none());
}

#[tokio::test]
async fn moderated_chat_stays_hidden_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
        e.register_connector(FakePlatform::new(Platform::Twitch));
        let mut f = e.filter();
        f.words.list.push("badword".into());
        e.save_filter(f).await.unwrap();
        e.ingest(chat(Platform::Twitch, "bad-1", "troll", "badword")).await;
        e.ingest(chat(Platform::Twitch, "raid-1", "raider", "spam spam")).await;
        let mut target = Actor::named(Platform::Twitch, "raider");
        target.id = Some("raider-id".into());
        e.ingest(Event::live(
            Platform::Twitch,
            Some("ban-1".into()),
            Millis(0),
            EventKind::Ban { target, reason: None, until: None },
        ))
        .await;
        e.ingest(chat(Platform::Twitch, "ok-1", "fan", "hello")).await;
        let store = e.store();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let s = store.clone();
            let visible = tokio::task::spawn_blocking(move || {
                ferrocast_store::repo::events::recent_visible(&s, None, 50, &["chat_message".to_string()])
                    .unwrap()
                    .len()
            })
            .await
            .unwrap();
            if visible == 1 || tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
    let o = e.create_overlay("Main", ferrocast_core::overlay::Preset::ChatOnly).await.unwrap();
    let snap = e.overlay_snapshot(&e.overlay(&o.id).await.unwrap()).await.unwrap();
    let ids: Vec<_> = snap["state"]["chat"].as_array().unwrap().iter().map(|c| c["source_id"].clone()).collect();
    assert_eq!(ids, vec![Value::from("ok-1")], "filtered and banned chat never comes back");
}

#[tokio::test]
async fn a_deleted_message_is_not_read_aloud_by_a_queued_alert() {
    let (e, _d) = engine().await;
    let mut tts = e.tts_policy();
    tts.enabled = true;
    e.save_tts(tts).await.unwrap();
    e.queue_pause().await;
    let sc = |id: &str| {
        Event::live(
            Platform::Youtube,
            Some(id.into()),
            Millis(0),
            EventKind::PaidMessage {
                actor: Actor::named(Platform::Youtube, "Ravi"),
                money: Money::new(50_000, "INR"),
                content: Some(Content::plain("read this out")),
                sticker_url: None,
            },
        )
    };
    e.ingest(sc("sc-1")).await;
    let q = e.queue().await;
    assert!(q.waiting[0].alert.tts_text.is_some());
    e.ingest(Event::live(
        Platform::Youtube,
        Some("del-1".into()),
        Millis(0),
        EventKind::MessageDeleted { message_id: "sc-1".into() },
    ))
    .await;
    assert!(e.queue().await.waiting[0].alert.tts_text.is_none());
}

#[tokio::test]
async fn only_alerts_can_be_replayed() {
    let (e, _d) = engine().await;
    e.ingest(Event::live(Platform::Twitch, Some("on".into()), Millis(0), EventKind::StreamOnline { title: None }))
        .await;
    let ev = e.events(None, 5, vec![]).await.unwrap();
    assert!(matches!(e.replay(&ev[0].id).await, Err(EngineError::Invalid(_))));
    assert!(matches!(e.replay("missing").await, Err(EngineError::NotFound(_))));
}

#[tokio::test]
async fn connecting_twitch_signs_in_runs_eventsub_and_answers_commands() {
    use ferrocast_testkit::twitch::{fake, notification, push};
    let (st, (id, api, eventsub)) = fake().await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = mem();
    let apps = PlatformApps {
        twitch_client_id: Some("client".into()),
        twitch_endpoints: ferrocast_twitch::Endpoints { id, api, eventsub },
        ..PlatformApps::default()
    };
    let e = Engine::start_with(dir.path(), secrets.clone(), apps.clone()).await.unwrap();
    let ConnectFlow::DeviceCode { user_code, .. } = e.connect_account(Platform::Twitch).await.unwrap() else {
        panic!("Twitch uses a device code")
    };
    assert_eq!(user_code, "ABCD-EFGH");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while st.lock().unwrap().sessions == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(secrets.get("twitch-tokens").unwrap().is_some(), "tokens went to the secret store");
    while st.lock().unwrap().subs.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    e.save_command(Command {
        id: "hi".into(),
        name: "!hi".into(),
        aliases: vec![],
        response: "hey {user}".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();
    push(
        &st,
        "session-1",
        notification(
            "channel.chat.message",
            "d1",
            serde_json::json!({
        "broadcaster_user_id": "111", "chatter_user_id": "5", "chatter_user_login": "viewer", "chatter_user_name": "Viewer",
        "message_id": "chat-1", "message": { "text": "!hi", "fragments": [{ "type": "text", "text": "!hi" }] }, "badges": [] }),
        ),
    );
    while st.lock().unwrap().chats.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(st.lock().unwrap().chats[0]["message"], "hey Viewer");
    assert_eq!(st.lock().unwrap().chats[0]["reply_parent_message_id"], "chat-1");
    assert_eq!(e.events(None, 10, vec!["chat_message".into()]).await.unwrap().len(), 1);
    // A restart picks the account up again by itself.
    drop(e);
    let e2 = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    while st.lock().unwrap().sessions < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(st.lock().unwrap().sessions >= 2, "resumed on start");
    e2.disconnect_account(Platform::Twitch).await.unwrap();
    assert!(secrets.get("twitch-tokens").unwrap().is_none(), "signing out forgets the tokens");
}

#[tokio::test]
async fn connecting_without_a_twitch_app_says_so() {
    let (e, _d) = engine().await;
    assert!(
        matches!(e.connect_account(Platform::Twitch).await, Err(EngineError::Unavailable(m)) if m.contains("no Twitch app"))
    );
}

#[tokio::test]
async fn backfilled_and_shared_chat_never_trigger_the_bot() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.save_command(Command {
        id: "hi".into(),
        name: "!hi".into(),
        aliases: vec![],
        response: "hey".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();
    let mut old = chat(Platform::Twitch, "h1", "a", "!hi");
    old.origin = Origin::Backfill;
    e.ingest(old).await;
    let mut shared = chat(Platform::Twitch, "s1", "b", "!hi");
    shared.channel = Some(ChannelRef { platform: Platform::Twitch, id: "777".into(), name: "Partner".into() });
    e.ingest(shared).await;
    e.ingest(chat(Platform::Twitch, "l1", "c", "!hi")).await;
    eventually(|| !tw.log().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tw.log(), vec!["say hey reply=l1".to_string()]);
}

#[tokio::test]
async fn kick_connects_through_the_relay_and_its_chat_gets_answers() {
    use base64::Engine as _;
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    use rsa::signature::{SignatureEncoding, Signer};

    let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 1024).unwrap();
    let pem = key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
    let recorded: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let rec = recorded.clone();
    let kick_app = axum::Router::new()
        .route(
            "/public/v1/public-key",
            axum::routing::get(move || {
                let pem = pem.clone();
                async move { axum::Json(serde_json::json!({ "data": { "public_key": pem } })) }
            }),
        )
        .route(
            "/oauth/token",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({ "access_token": "ka", "refresh_token": "kr", "expires_in": 7200 }))
            }),
        )
        .route(
            "/public/v1/users",
            axum::routing::get(|| async {
                axum::Json(
                    serde_json::json!({ "data": [{ "user_id": 4242, "name": "kicker", "profile_picture": "" }] }),
                )
            }),
        )
        .fallback(move |uri: axum::http::Uri, body: String| {
            let rec = rec.clone();
            async move {
                rec.lock().unwrap().push((uri.path().to_string(), serde_json::from_str(&body).unwrap_or(Value::Null)));
                axum::Json(serde_json::json!({ "data": {}, "message": "OK" }))
            }
        });
    let kl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let kick = format!("http://{}", kl.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(kl, kick_app).await });

    let rdir = tempfile::tempdir().unwrap();
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("http://{}", rl.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: relay.clone(),
        kick_client_id: Some("cid".into()),
        kick_client_secret: Some("cs".into()),
        kick_id_url: kick.clone(),
        kick_api_url: kick.clone(),
    };
    let rapp = ferrocast_relay::router(ferrocast_relay::AppState::new(
        cfg,
        ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap(),
    ))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(rl, rapp).await });

    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay.clone()), kick_api: kick.clone(), ..PlatformApps::default() };
    let e = Engine::start_with(dir.path(), mem(), apps).await.unwrap();
    e.save_command(Command {
        id: "hi".into(),
        name: "!hi".into(),
        aliases: vec![],
        response: "hey {user}".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();

    let ConnectFlow::Browser { url } = e.connect_account(Platform::Kick).await.unwrap() else {
        panic!("Kick uses the browser")
    };
    let state = url::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    // Kick redirects the streamer back to the relay.
    assert_eq!(reqwest::get(format!("{relay}/v1/kick/callback?code=c&state={state}")).await.unwrap().status(), 200);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !recorded.lock().unwrap().iter().any(|(p, _)| p == "/public/v1/events/subscriptions")
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sub = recorded
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/public/v1/events/subscriptions")
        .cloned()
        .expect("subscribed");
    assert_eq!(sub.1["broadcaster_user_id"], 4242);

    let body = serde_json::json!({
        "message_id": "kmsg-1", "broadcaster": { "user_id": 4242 },
        "sender": { "user_id": 77, "username": "viewer", "channel_slug": "viewer" }, "content": "!hi",
    })
    .to_string();
    let ts = {
        // the relay checks the timestamp; send "now" in RFC 3339
        let secs = crate::now().0 / 1000;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
    };
    let sig = SigningKey::<rsa::sha2::Sha256>::new(key).sign(format!("evt-1.{ts}.{body}").as_bytes());
    let status = reqwest::Client::new()
        .post(format!("{relay}/v1/kick/webhook"))
        .header("Kick-Event-Message-Id", "evt-1")
        .header("Kick-Event-Message-Timestamp", &ts)
        .header("Kick-Event-Signature", base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
        .header("Kick-Event-Type", "chat.message.sent")
        .header("Kick-Event-Version", "1")
        .body(body)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
    while !recorded.lock().unwrap().iter().any(|(p, _)| p == "/public/v1/chat")
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reply = recorded
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/public/v1/chat")
        .cloned()
        .expect("the bot answered on Kick");
    assert_eq!(reply.1["content"], "hey viewer");
    assert_eq!(reply.1["reply_to_message_id"], "kmsg-1");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_tip_with_tts_gets_real_speech_attached_before_it_plays() {
    let (e, _d) = engine().await;
    let mut tts = e.tts_policy();
    tts.enabled = true;
    e.save_tts(tts).await.unwrap();
    e.queue_pause().await;
    e.ingest(Event::live(
        Platform::Tips,
        Some("pay_tts".into()),
        Millis(0),
        EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: Money::new(50_000, "INR"),
            message: Some("great stream".into()),
            provider: TipProvider::Razorpay,
            payment_ref: "pay_tts".into(),
            status: PaymentStatus::Captured,
        },
    ))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let q = e.queue().await;
        if let Some(id) = q.waiting.first().and_then(|w| w.alert.tts_audio.clone()) {
            let store = e.store();
            let found = tokio::task::spawn_blocking(move || ferrocast_store::repo::media::get(&store, &id))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.unwrap().0.mime, "audio/wav");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no speech attached");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_forgotten_relay_install_registers_again() {
    let rdir = tempfile::tempdir().unwrap();
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("http://{}", rl.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: relay.clone(),
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

    let secrets = mem();
    // An install the relay has since removed (or never had).
    secrets.set(relay::INSTALL, "gone.gone").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay), ..PlatformApps::default() };
    let e = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    e.ensure_relay_link();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let now = secrets.get(relay::INSTALL).unwrap();
        if now.as_deref().is_some_and(|a| a != "gone.gone") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the install was never replaced");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------- loyalty points

async fn go_live(e: &Engine, p: Platform) {
    e.ingest(Event::live(p, Some(format!("online-{p:?}")), Millis(0), EventKind::StreamOnline { title: None })).await;
}

fn as_mod(mut ev: Event) -> Event {
    if let EventKind::ChatMessage { actor, .. } = &mut ev.kind {
        actor.roles = Roles::MODERATOR;
    }
    ev
}

#[tokio::test]
async fn chatters_earn_points_while_live_and_linked_accounts_earn_once() {
    let (e, _d) = engine().await;
    e.ingest(chat(Platform::Twitch, "pre", "sam", "before the stream")).await;
    assert_eq!(e.pay_watch_time(Millis(0)).await, 0, "offline chat earns nothing");
    go_live(&e, Platform::Twitch).await;
    go_live(&e, Platform::Kick).await;
    e.ingest(chat(Platform::Twitch, "c1", "sam", "hi")).await;
    e.ingest(chat(Platform::Kick, "c2", "sam_k", "hi from kick")).await;
    e.ingest(chat(Platform::Twitch, "c3", "ana", "hello")).await;
    let store = e.store();
    repo::viewers::link(&store, Platform::Kick, "sam_k-id", Platform::Twitch, "sam-id").unwrap();
    assert_eq!(e.pay_watch_time(Millis(1_000)).await, 0, "the interval clock starts");
    assert_eq!(e.pay_watch_time(Millis(301_000)).await, 2, "Sam (two accounts) and Ana, once each");
    assert_eq!(e.points(Platform::Kick, "sam_k-id").await.unwrap(), 10);
    assert_eq!(e.points(Platform::Twitch, "ana-id").await.unwrap(), 10);
    assert_eq!(e.pay_watch_time(Millis(601_000)).await, 0, "nobody chatted since");
}

#[tokio::test]
async fn support_bonuses_pay_live_events_once() {
    let (e, _d) = engine().await;
    let mut fan = Actor::named(Platform::Twitch, "Fan");
    fan.id = Some("fan-id".into());
    let follow = Event::live(Platform::Twitch, Some("f-1".into()), Millis(0), EventKind::Follow { actor: fan.clone() });
    e.ingest(follow.clone()).await;
    e.ingest(follow).await; // a redelivery
    assert_eq!(e.points(Platform::Twitch, "fan-id").await.unwrap(), 50);
    e.test("follow", None).await.unwrap();
    e.ingest(Event::live(Platform::Twitch, Some("r-1".into()), Millis(0), EventKind::Raid { from: fan, viewers: 12 }))
        .await;
    assert_eq!(e.points(Platform::Twitch, "fan-id").await.unwrap(), 150, "follow + raid; the test paid nothing");
}

#[tokio::test]
async fn points_commands_answer_mods_and_viewers() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.ingest(chat(Platform::Twitch, "m1", "Sam", "hello")).await;
    e.ingest(chat(Platform::Twitch, "m2", "Sam", "!points")).await;
    eventually(|| tw.log().contains(&"say @Sam, you have 0 points. reply=m2".to_string())).await;
    e.ingest(chat(Platform::Twitch, "m3", "Sam", "!addpoints sam 500")).await;
    e.ingest(as_mod(chat(Platform::Twitch, "m4", "Mo", "!addpoints @SAM 1,000"))).await;
    eventually(|| tw.log().iter().any(|l| l.starts_with("say Gave 1,000 points to Sam. They have 1,000 points now.")))
        .await;
    assert_eq!(e.points(Platform::Twitch, "Sam-id").await.unwrap(), 1_000, "only the mod's command counted");
    e.ingest(as_mod(chat(Platform::Twitch, "m5", "Mo", "!removepoints sam 5000"))).await;
    eventually(|| tw.log().iter().any(|l| l.starts_with("say Took 5,000 points from Sam. They have 0 points now.")))
        .await;
    e.ingest(as_mod(chat(Platform::Twitch, "m6", "Mo", "!addpoints ghost 5"))).await;
    eventually(|| tw.log().iter().any(|l| l.starts_with("say I haven't seen @ghost in chat yet."))).await;
    e.ingest(as_mod(chat(Platform::Twitch, "m7", "Mo", "!addpoints sam -5"))).await;
    eventually(|| tw.log().iter().any(|l| l.starts_with("say Use !addpoints @name 100"))).await;
    // A command the streamer made with the same name wins.
    e.save_command(Command {
        id: "p".into(),
        name: "!points".into(),
        aliases: vec![],
        response: "Points are coming soon!".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();
    e.ingest(chat(Platform::Twitch, "m8", "Ana", "!points")).await;
    eventually(|| tw.log().iter().any(|l| l.starts_with("say Points are coming soon!"))).await;
}

#[tokio::test]
async fn dashboard_adjustments_refuse_bad_amounts() {
    let (e, _d) = engine().await;
    e.ingest(chat(Platform::Youtube, "y1", "Lee", "hi")).await;
    assert_eq!(e.adjust_points(Platform::Youtube, "Lee-id", 75).await.unwrap(), 75);
    for bad in [0, MAX_ADJUST + 1, -MAX_ADJUST - 1, i64::MIN] {
        let err = e.adjust_points(Platform::Youtube, "Lee-id", bad).await.unwrap_err();
        assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with("delta:")), "{bad}: {err}");
    }
    let err = e.adjust_points(Platform::Youtube, "Lee-id", -76).await.unwrap_err();
    assert!(matches!(&err, EngineError::Invalid(m) if m.contains("at most 75")), "{err}");
    assert!(matches!(
        e.adjust_points(Platform::Kick, "nobody", 5).await,
        Err(EngineError::Missing { code: "viewer_not_found", .. })
    ));
    let patch = serde_json::json!({ "name": "Coins", "per_interval": "ten" });
    let err = e.patch_loyalty(patch.as_object().unwrap()).await.unwrap_err();
    assert_eq!(err.to_string(), "per_interval: must be a whole number of 0 or more.");
    let patch = serde_json::json!({ "name": "Coins", "interval_min": 0 });
    assert!(e.patch_loyalty(patch.as_object().unwrap()).await.unwrap_err().to_string().starts_with("interval_min:"));
    let patch = serde_json::json!({ "name": "  Coins " });
    assert_eq!(e.patch_loyalty(patch.as_object().unwrap()).await.unwrap().name, "Coins");
    assert_eq!(e.leaderboard(10).await.unwrap()[0].points, 75);
}

// ---------------------------------------------------------------- polls

fn poll_spec(opts: &[&str]) -> ferrocast_core::poll::PollSpec {
    ferrocast_core::poll::PollSpec {
        question: "Which map next?".into(),
        options: opts.iter().map(|s| s.to_string()).collect(),
        duration_s: 60,
    }
}

#[tokio::test]
async fn polls_count_one_vote_per_person_and_post_the_result_once() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    let kick = FakePlatform::new(Platform::Kick);
    e.register_connector(tw.clone());
    e.register_connector(kick.clone());
    // Sam has a Twitch and a Kick account, linked.
    e.ingest(chat(Platform::Twitch, "s1", "sam", "hi")).await;
    e.ingest(chat(Platform::Kick, "s2", "sam_k", "hi")).await;
    repo::viewers::link(&e.store(), Platform::Kick, "sam_k-id", Platform::Twitch, "sam-id").unwrap();
    let mut live = e.subscribe_live();
    let p = e.start_poll(poll_spec(&["Ascent", "Bind", "Haven"])).await.unwrap();
    assert_eq!(p.options.len(), 3);
    eventually(|| tw.log().iter().any(|l| l.starts_with("say Poll: Which map next? Type !vote 1 for Ascent"))).await;
    eventually(|| kick.log().iter().any(|l| l.starts_with("say Poll: Which map next?"))).await;
    assert!(matches!(e.start_poll(poll_spec(&["a", "b"])).await, Err(EngineError::Conflict(_))), "one at a time");
    e.ingest(chat(Platform::Twitch, "v1", "sam", "!vote 1")).await;
    e.ingest(chat(Platform::Kick, "v2", "sam_k", "!vote 2")).await; // same person: changes the vote
    e.ingest(chat(Platform::Twitch, "v3", "ana", "!vote 2")).await;
    e.ingest(chat(Platform::Twitch, "v4", "lee", "!vote 9")).await; // no such answer
    e.ingest(chat(Platform::Twitch, "v5", "kim", "!vote two")).await;
    let mut shared = chat(Platform::Twitch, "v6", "guest", "!vote 3");
    shared.channel = Some(ChannelRef { platform: Platform::Twitch, id: "partner".into(), name: "partner".into() });
    e.ingest(shared).await; // Shared Chat from another channel doesn't vote here
    let v = e.poll().unwrap();
    assert_eq!(v.options.iter().map(|o| o.votes).collect::<Vec<_>>(), vec![0, 2, 0]);
    assert_eq!(v.leaders, vec![1]);
    // The timer and the button race: exactly one of them closes it and posts the result.
    let (a, b) = tokio::join!(e.end_poll(), e.finish_poll(Some(&p.id)));
    assert!(a.is_ok() != b.is_ok(), "exactly one wins: {a:?} {b:?}");
    assert!(matches!(e.end_poll().await, Err(EngineError::Conflict(_))));
    eventually(|| {
        tw.log().iter().any(|l| l.starts_with("say Poll closed: Which map next? Winner: Bind with 2 votes (100%)."))
    })
    .await;
    let results = tw.log().iter().filter(|l| l.contains("Poll closed")).count();
    assert_eq!(results, 1, "posted once");
    e.ingest(chat(Platform::Twitch, "v7", "zoe", "!vote 1")).await;
    assert_eq!(e.poll().unwrap().total, 2, "votes after the end don't count");
    let mut saw_poll_frame = false;
    while let Ok(f) = live.try_recv() {
        saw_poll_frame |= f.text.contains("\"type\":\"poll\"");
    }
    assert!(saw_poll_frame, "dashboards get live results");
    e.clear_poll().unwrap();
    assert!(e.poll().is_none());
    assert!(matches!(e.clear_poll(), Err(EngineError::Missing { code: "poll_not_found", .. })));
    assert!(matches!(e.end_poll().await, Err(EngineError::Missing { code: "poll_not_found", .. })));
    assert!(e.start_poll(poll_spec(&["only one"])).await.is_err());
}

#[tokio::test]
async fn a_poll_without_any_chat_connected_says_so() {
    let (e, _d) = engine().await;
    let mut live = e.subscribe_live();
    e.start_poll(poll_spec(&["a", "b"])).await.unwrap();
    let mut notice = None;
    while let Ok(f) = live.try_recv() {
        if f.text.contains("\"type\":\"notice\"") {
            notice = Some(f.text.to_string());
        }
    }
    assert!(notice.unwrap().contains("No chat is connected"));
}

// ---------------------------------------------------------------- giveaways

fn giveaway_spec(cost: i64, subs: bool) -> ferrocast_core::giveaway::GiveawaySpec {
    ferrocast_core::giveaway::GiveawaySpec {
        prize: "a game key".into(),
        keyword: "!join".into(),
        cost,
        subscriber_bonus: subs,
    }
}

fn with_roles(mut ev: Event, roles: Roles) -> Event {
    if let EventKind::ChatMessage { actor, .. } = &mut ev.kind {
        actor.roles = roles;
    }
    ev
}

/// Lets the giveaway's entry handling (it waits on a lock) finish.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn giveaways_take_one_paid_entry_per_person_and_never_the_streamer_or_bots() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    for (id, user) in [("a1", "ana"), ("b1", "bo"), ("k1", "ana_k"), ("s1", "sub")] {
        let p = if user == "ana_k" { Platform::Kick } else { Platform::Twitch };
        e.ingest(chat(p, id, user, "hi")).await;
    }
    repo::viewers::link(&e.store(), Platform::Kick, "ana_k-id", Platform::Twitch, "ana-id").unwrap();
    e.adjust_points(Platform::Twitch, "ana-id", 100).await.unwrap();
    e.adjust_points(Platform::Twitch, "sub-id", 100).await.unwrap();
    e.adjust_points(Platform::Twitch, "bo-id", 10).await.unwrap();
    assert!(
        matches!(e.start_giveaway(giveaway_spec(-5, true)).await, Err(EngineError::Invalid(m)) if m.starts_with("cost:"))
    );
    e.start_giveaway(giveaway_spec(50, true)).await.unwrap();
    eventually(|| {
        tw.log().iter().any(|l| l.starts_with("say Giveaway: a game key! Type !join to enter. It costs 50 points."))
    })
    .await;
    assert!(matches!(e.start_giveaway(giveaway_spec(0, false)).await, Err(EngineError::Conflict(_))));
    e.ingest(chat(Platform::Twitch, "j1", "ana", "!join")).await;
    e.ingest(chat(Platform::Twitch, "j2", "ana", "!JOIN")).await; // again: no second fee
    e.ingest(chat(Platform::Kick, "j3", "ana_k", "!join")).await; // same person on Kick
    e.ingest(with_roles(chat(Platform::Twitch, "j4", "sub", "!join"), Roles::SUBSCRIBER)).await;
    e.ingest(chat(Platform::Twitch, "j5", "bo", "!join")).await; // can't afford it
    e.ingest(chat(Platform::Twitch, "j6", "bo", "!join")).await; // told once only
    e.ingest(with_roles(chat(Platform::Twitch, "j7", "me", "!join"), Roles::BROADCASTER)).await;
    e.ingest(chat(Platform::Twitch, "j8", "Nightbot", "!join")).await;
    settle().await;
    let g = e.giveaway().await.unwrap();
    assert_eq!((g.entries, g.tickets), (2, 3), "Ana once, the sub twice");
    assert_eq!(e.points(Platform::Twitch, "ana-id").await.unwrap(), 50, "paid once");
    assert_eq!(e.points(Platform::Twitch, "bo-id").await.unwrap(), 10);
    eventually(|| {
        tw.log().iter().filter(|l| l.contains("entering costs 50 points and you have 10 points")).count() == 1
    })
    .await;
    // Cancelling gives every fee back.
    assert_eq!(e.cancel_giveaway().await.unwrap(), 2);
    assert_eq!(e.points(Platform::Twitch, "ana-id").await.unwrap(), 100);
    assert_eq!(e.points(Platform::Twitch, "sub-id").await.unwrap(), 100);
    assert!(e.giveaway().await.is_none());
    assert!(matches!(e.cancel_giveaway().await, Err(EngineError::Missing { code: "giveaway_not_found", .. })));
}

#[tokio::test]
async fn draws_are_serialised_redraws_skip_winners_and_entries_close() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    assert!(matches!(e.draw_giveaway(false).await, Err(EngineError::Missing { .. })));
    e.start_giveaway(giveaway_spec(0, false)).await.unwrap();
    assert!(matches!(e.draw_giveaway(false).await, Err(EngineError::Unavailable(_))), "nobody entered");
    assert!(matches!(e.draw_giveaway(true).await, Err(EngineError::Conflict(_))), "redraw before draw");
    e.ingest(chat(Platform::Twitch, "j1", "ana", "!join")).await;
    e.ingest(chat(Platform::Twitch, "j2", "bo", "!join")).await;
    settle().await;
    let (a, b) = tokio::join!(e.draw_giveaway(false), e.draw_giveaway(false));
    assert!(a.is_ok() != b.is_ok(), "a double click draws once: {a:?} {b:?}");
    let first = a.or(b).unwrap();
    assert!(!first.open, "drawing closes entries");
    e.ingest(chat(Platform::Twitch, "j3", "late", "!join")).await;
    settle().await;
    assert_eq!(e.giveaway().await.unwrap().entries, 2, "no entries after the draw");
    let winner = first.winners[0].display_name.clone();
    eventually(|| tw.log().iter().any(|l| l == &format!("say Congratulations @{winner}, you won a game key! reply=-")))
        .await;
    let second = e.draw_giveaway(true).await.unwrap();
    assert_ne!(second.winners[0].user_id, second.winners[1].user_id);
    assert!(matches!(e.draw_giveaway(true).await, Err(EngineError::Unavailable(m)) if m.contains("already won")));
    assert!(matches!(e.close_giveaway().await, Err(EngineError::Conflict(_))));
    let ended = e.end_giveaway().await.unwrap();
    assert!(ended.ended);
    assert!(matches!(e.end_giveaway().await, Err(EngineError::Conflict(_))));
    assert!(matches!(e.draw_giveaway(true).await, Err(EngineError::Conflict(_))));
    e.start_giveaway(giveaway_spec(0, false)).await.unwrap();
}

#[tokio::test]
async fn fees_from_a_giveaway_that_never_finished_come_back_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
        e.ingest(chat(Platform::Twitch, "c1", "ana", "hi")).await;
        e.adjust_points(Platform::Twitch, "ana-id", 30).await.unwrap();
        e.start_giveaway(giveaway_spec(20, false)).await.unwrap();
        e.ingest(chat(Platform::Twitch, "j1", "ana", "!join")).await;
        settle().await;
        assert_eq!(e.points(Platform::Twitch, "ana-id").await.unwrap(), 10);
    }
    let e = Engine::start_with_secrets(dir.path(), mem()).await.unwrap();
    let mut back = false;
    for _ in 0..100 {
        if e.points(Platform::Twitch, "ana-id").await.unwrap() == 30 {
            back = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(back, "the entry fee was refunded at start");
}

// ---------------------------------------------------------------- first-time chatters

fn chatter_frames(rx: &mut tokio::sync::broadcast::Receiver<frames::LiveFrame>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "chatter" {
            out.push(v["data"].clone());
        }
    }
    out
}

#[tokio::test]
async fn first_messages_are_marked_and_welcomed_only_when_turned_on() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let mut live = e.subscribe_live();
    e.ingest(chat(Platform::Twitch, "m1", "newbie", "hello!")).await;
    e.ingest(chat(Platform::Twitch, "m2", "newbie", "second")).await;
    let marks = chatter_frames(&mut live);
    assert_eq!(marks.len(), 1, "only the first message is marked: {marks:?}");
    assert_eq!(marks[0]["kind"], "first");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!tw.log().iter().any(|l| l.contains("Welcome")), "the welcome is off by default");
    let patch = serde_json::json!({ "enabled": true, "template": "Hi {user}, welcome!" });
    e.patch_welcome(patch.as_object().unwrap()).await.unwrap();
    e.ingest(chat(Platform::Twitch, "m3", "fresh", "hey")).await;
    e.ingest(chat(Platform::Twitch, "m4", "fresh2", "hey")).await; // within 10 s: no second welcome
    eventually(|| tw.log().contains(&"say Hi fresh, welcome! reply=m3".to_string())).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!tw.log().iter().any(|l| l.contains("fresh2")), "welcomes are spaced out");
    // A first message the filter holds gets its dashboard mark, but no public welcome.
    let mut f = e.filter();
    f.words.list.push("spamword".into());
    f.words.action = ferrocast_core::filter::RuleAction::Hold;
    e.save_filter(f).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10_100)).await; // past the welcome gap
    e.ingest(chat(Platform::Twitch, "m5", "spammer", "spamword here")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!tw.log().iter().any(|l| l.contains("spammer")), "held messages aren't welcomed");
    let bad = serde_json::json!({ "template": "{oops" });
    assert!(e.patch_welcome(bad.as_object().unwrap()).await.unwrap_err().to_string().starts_with("template:"));
    let bad = serde_json::json!({ "returning_days": 3 });
    assert!(e.patch_welcome(bad.as_object().unwrap()).await.unwrap_err().to_string().starts_with("returning_days:"));
}

#[tokio::test]
async fn people_back_after_a_month_get_a_returning_mark() {
    let (e, _d) = engine().await;
    e.ingest(chat(Platform::Kick, "m1", "regular", "hi")).await;
    let long_ago = now().0 - 45 * 86_400_000;
    e.store().write(|tx| Ok(tx.execute("UPDATE viewer_identities SET last_message_at = ?1", [long_ago])?)).unwrap();
    let mut live = e.subscribe_live();
    e.ingest(chat(Platform::Kick, "m2", "regular", "I'm back")).await;
    let marks = chatter_frames(&mut live);
    assert_eq!(marks.len(), 1);
    assert_eq!((marks[0]["kind"].as_str(), marks[0]["days"].as_u64()), (Some("returning"), Some(45)));
}

// ---------------------------------------------------------------- recap

#[tokio::test]
async fn the_recap_lists_this_streams_supporters_and_chatters() {
    let (e, _d) = engine().await;
    let empty = e.recap(None).await.unwrap();
    assert!(empty.started_at.is_none() && empty.thanks.is_empty(), "no stream yet");
    go_live(&e, Platform::Twitch).await;
    let mut fan = Actor::named(Platform::Twitch, "Fan");
    fan.id = Some("fan".into());
    e.ingest(Event::live(Platform::Twitch, Some("f".into()), Millis(0), EventKind::Follow { actor: fan.clone() }))
        .await;
    e.ingest(Event::live(
        Platform::Twitch,
        Some("g".into()),
        Millis(0),
        EventKind::GiftBatch { gifter: fan.clone(), tier: Tier::Tier1, count: 3 },
    ))
    .await;
    for i in 0..3 {
        e.ingest(chat(Platform::Twitch, &format!("c{i}"), "talker", "hey")).await;
    }
    e.ingest(chat(Platform::Kick, "k", "quiet", "hi")).await;
    e.test("tip", None).await.unwrap(); // tests never count
    e.ingest(Event::live(Platform::Twitch, Some("v".into()), Millis(0), EventKind::ViewerCount { count: 80 })).await;
    let r = e.recap(None).await.unwrap();
    assert!(r.live && r.started_at.is_some() && r.ended_at.is_none());
    assert_eq!((r.totals.follows, r.totals.subs, r.totals.messages, r.totals.peak_viewers), (1, 3, 4, 80));
    assert!(r.totals.tips.is_empty());
    assert_eq!(r.subscribers[0].gifts, 3);
    assert_eq!(r.top_chatters[0].name, "talker");
    assert_eq!(r.top_chatters[0].messages, 3);
    assert_eq!(r.thanks, vec!["Fan"]);
    e.ingest(Event::live(Platform::Twitch, Some("off".into()), Millis(0), EventKind::StreamOffline)).await;
    let after = e.recap(None).await.unwrap();
    assert!(!after.live && after.ended_at.is_some(), "still there after the stream");
    let custom = e.recap(Some(0)).await.unwrap();
    assert_eq!(custom.totals.peak_viewers, 0, "a custom window counts from the log only");
    assert_eq!(custom.totals.follows, 1);
}

// ---------------------------------------------------------------- Discord

/// A fake Discord webhook that answers with `status` and records what it was sent.
async fn fake_discord(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<Value>>>) {
    use axum::routing::post;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/api/webhooks/{id}/{token}",
        post(move |axum::Json(v): axum::Json<Value>| {
            let s = s2.clone();
            async move {
                s.lock().unwrap().push(v);
                (axum::http::StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], body)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    (format!("http://{addr}/api/webhooks/123456789012345678/tok"), seen)
}

#[tokio::test]
async fn going_live_posts_to_discord_once_per_platform() {
    let (e, _d) = engine().await;
    let (url, seen) = fake_discord(204, "").await;
    e.set_discord_webhook_for_tests(&url);
    let patch = serde_json::json!({ "template": "{name} is live on {platform}: {title}", "mention_everyone": true });
    e.save_discord(patch.as_object().unwrap()).await.unwrap();
    e.ingest(Event::live(
        Platform::Twitch,
        Some("on1".into()),
        Millis(0),
        EventKind::StreamOnline { title: Some("Ranked".into()) },
    ))
    .await;
    e.ingest(Event::live(Platform::Twitch, Some("on2".into()), Millis(0), EventKind::StreamOnline { title: None }))
        .await;
    e.ingest(Event::live(Platform::Kick, Some("on3".into()), Millis(0), EventKind::StreamOnline { title: None })).await;
    eventually(|| seen.lock().unwrap().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let posts = seen.lock().unwrap().clone();
    assert_eq!(posts.len(), 2, "Twitch once (the reconnect isn't news), Kick once");
    assert_eq!(posts[0]["content"], "The stream is live on Twitch: Ranked");
    assert_eq!(posts[0]["allowed_mentions"]["parse"], serde_json::json!(["everyone"]));
    let view = e.discord().await.unwrap();
    assert!(view.has_webhook);
    assert!(!serde_json::to_string(&view).unwrap().contains("tok"), "the URL never comes back");
}

#[tokio::test]
async fn discord_errors_become_plain_messages() {
    let (e, _d) = engine().await;
    assert!(
        matches!(e.test_discord().await, Err(DiscordFailure { error: EngineError::Unavailable(m), .. }) if m.contains("webhook URL first"))
    );
    let (gone, _) = fake_discord(404, r#"{"message":"Unknown Webhook","code":10015}"#).await;
    e.set_discord_webhook_for_tests(&gone);
    let f = e.test_discord().await.unwrap_err();
    assert!(matches!(&f.error, EngineError::Unavailable(m) if m.contains("was deleted")), "{:?}", f.error);
    let (slow, _) =
        fake_discord(429, r#"{"message":"You are being rate limited.","retry_after":2.4,"global":false}"#).await;
    e.set_discord_webhook_for_tests(&slow);
    let f = e.test_discord().await.unwrap_err();
    assert!(matches!(&f.error, EngineError::TooMany(m) if m.contains("3 seconds")), "{:?}", f.error);
    assert_eq!(f.retry_after, Some(3));
    let (denied, _) = fake_discord(401, r#"{"message":"Invalid Webhook Token"}"#).await;
    e.set_discord_webhook_for_tests(&denied);
    assert!(matches!(e.test_discord().await.unwrap_err().error, EngineError::Unavailable(m) if m.contains("refused")));
    e.set_discord_webhook_for_tests("http://127.0.0.1:1/api/webhooks/1/x");
    assert!(
        matches!(e.test_discord().await.unwrap_err().error, EngineError::Unavailable(m) if m.contains("Couldn't reach Discord"))
    );
    let (ok, seen) = fake_discord(204, "").await;
    e.set_discord_webhook_for_tests(&ok);
    e.test_discord().await.unwrap();
    assert!(seen.lock().unwrap()[0]["content"].as_str().unwrap().starts_with("Test from Ferrocast"));
    assert_eq!(seen.lock().unwrap()[0]["allowed_mentions"]["parse"], serde_json::json!([]), "tests never ping");
}

// ---------------------------------------------------------------- OBS auto-detect

#[tokio::test]
async fn obs_settings_are_found_and_used_without_typing() {
    let (e, d) = engine().await;
    let cfg = d.path().join("obs-config.json");
    std::fs::write(
        &cfg,
        br#"{"auth_required":true,"server_enabled":true,"server_password":"pw-from-obs","server_port":4466}"#,
    )
    .unwrap();
    let (view, _) = e.obs_detect_in(vec![cfg.clone()]).await.unwrap();
    assert!(view.found && view.has_password && view.reason.is_none());
    assert_eq!(view.port, Some(4466));
    assert!(!serde_json::to_string(&view).unwrap().contains("pw-from-obs"), "the password never leaves the engine");
    let used = e.obs_use_detected_in(vec![cfg]).await.unwrap();
    assert_eq!((used.host.as_str(), used.port, used.has_password), ("127.0.0.1", 4466, true));
    let missing = e.obs_use_detected_in(vec![d.path().join("none.json")]).await.unwrap_err();
    assert!(matches!(missing, EngineError::Unavailable(m) if m.contains("aren't on this computer")));
    let junk = d.path().join("junk.json");
    std::fs::write(&junk, b"{oops").unwrap();
    let (view, _) = e.obs_detect_in(vec![junk.clone()]).await.unwrap();
    assert!(!view.found && view.reason.unwrap().contains("couldn't be read"));
    assert!(matches!(e.obs_use_detected_in(vec![junk]).await, Err(EngineError::Unavailable(_))));
}

/// A tiny obs-websocket v5 server (no password): enough for "connected" and the scene.
async fn fake_obs() -> u16 {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                let hello = serde_json::json!({ "op": 0, "d": { "obsWebSocketVersion": "5.5.0", "rpcVersion": 1 } });
                let _ = ws.send(Message::text(hello.to_string())).await;
                let _ = ws.next().await;
                let _ = ws.send(Message::text(r#"{"op":2,"d":{"negotiatedRpcVersion":1}}"#)).await;
                while let Some(Ok(msg)) = ws.next().await {
                    let Ok(req) = serde_json::from_str::<Value>(msg.to_text().unwrap_or("")) else { continue };
                    let d = &req["d"];
                    let data = match d["requestType"].as_str() {
                        Some("GetVersion") => {
                            serde_json::json!({ "obsVersion": "32.0.1", "obsWebSocketVersion": "5.5.0",
                            "rpcVersion": 1, "availableRequests": [], "supportedImageFormats": [], "platform": "macos",
                            "platformDescription": "macOS" })
                        }
                        _ => {
                            serde_json::json!({ "sceneName": "Gaming", "sceneUuid": "11111111-1111-4111-8111-111111111111",
                            "currentProgramSceneName": "Gaming",
                            "currentProgramSceneUuid": "11111111-1111-4111-8111-111111111111" })
                        }
                    };
                    let resp = serde_json::json!({ "op": 7, "d": { "requestType": d["requestType"], "requestId": d["requestId"],
                        "requestStatus": { "result": true, "code": 100 }, "responseData": data } });
                    if ws.send(Message::text(resp.to_string())).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

async fn obs_state_becomes(e: &Engine, want: &str) -> crate::state::ConnectorView {
    for _ in 0..100 {
        let v = e.state().connectors.get("obs").cloned().unwrap();
        if v.state == want {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("OBS never became {want}: {:?}", e.state().connectors.get("obs"));
}

#[tokio::test]
async fn obs_connects_by_itself_once_its_websocket_is_switched_on() {
    let (e, d) = engine().await;
    let cfg = d.path().join("obs-config.json");
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    std::fs::write(&cfg, format!(r#"{{"server_enabled":false,"server_port":{closed}}}"#)).unwrap();
    e.set_obs_detect_paths(vec![cfg.clone()]);
    let mut live = e.subscribe_live();
    e.watch_obs();
    let off = obs_state_becomes(&e, "disconnected").await;
    assert!(off.detail.unwrap().contains("switched off"), "says how to fix it");

    // The streamer ticks "Enable WebSocket server" in OBS: no click in Ferrocast.
    let port = fake_obs().await;
    std::fs::write(&cfg, format!(r#"{{"server_enabled":true,"server_port":{port}}}"#)).unwrap();
    obs_state_becomes(&e, "connected").await;
    let mut saw_status = false;
    while let Ok(f) = live.try_recv() {
        saw_status |= f.text.contains(r#""type":"status""#) && f.text.contains(r#""state":"connected""#);
    }
    assert!(saw_status, "dashboards hear about it");
    let view = e.obs().await.unwrap();
    assert_eq!((view.state.as_str(), view.port, view.auto), ("connected", port, true));
    assert!(e.obs_detect().await.unwrap().in_use, "nothing to offer");

    // Pressing Connect without changing anything keeps following OBS.
    let same =
        e.save_obs(ferrocast_obs::ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    assert!(same.auto);

    // Changing only the address keeps the password read from OBS (now saved as typed).
    std::fs::write(
        &cfg,
        format!(r#"{{"server_enabled":true,"server_port":{port},"auth_required":true,"server_password":"from-obs"}}"#),
    )
    .unwrap();
    let moved =
        e.save_obs(ferrocast_obs::ObsConfig { host: "localhost".into(), port }, PasswordChange::Keep).await.unwrap();
    assert!(!moved.auto && moved.has_password);
    assert_eq!(e.inner.secrets.get("obs-password").unwrap().as_deref(), Some("from-obs"));
    std::fs::write(&cfg, format!(r#"{{"server_enabled":true,"server_port":{port}}}"#)).unwrap();

    // Typed details are never replaced by detected ones...
    let typed = e
        .save_obs(
            ferrocast_obs::ObsConfig { host: "127.0.0.1".into(), port: closed },
            PasswordChange::Set("typed".into()),
        )
        .await
        .unwrap();
    assert_eq!((typed.state.as_str(), typed.port, typed.auto), ("disconnected", closed, false));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(e.state().connectors.get("obs").unwrap().state, "disconnected", "still the streamer's details");
    let offer = e.obs_detect().await.unwrap();
    assert!(offer.found && !offer.in_use, "so \"Use these\" is offered");

    // ...until they choose "Use these".
    let used = e.obs_use_detected().await.unwrap();
    assert_eq!((used.state.as_str(), used.port, used.auto, used.has_password), ("connected", port, true, false));
    obs_state_becomes(&e, "connected").await;
}

// ---------------------------------------------------------------- starter commands

#[tokio::test]
async fn the_starter_pack_skips_names_in_use_and_shoutouts_name_the_game() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.save_command(Command {
        id: "mine".into(),
        name: "!hi".into(),
        aliases: vec!["!LURK".into()],
        response: "my own lurk".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    })
    .await
    .unwrap();
    let r = e.add_starter_commands().await.unwrap();
    assert_eq!(r.added.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["!socials", "!uptime", "!so"]);
    assert_eq!(r.skipped, vec!["!lurk"], "an alias counts as taken");
    let again = e.add_starter_commands().await.unwrap();
    assert!(again.added.is_empty() && again.skipped.len() == 4, "a second click adds nothing");
    e.ingest(as_mod(chat(Platform::Twitch, "s1", "Mo", "!so @friend"))).await;
    eventually(|| {
        tw.log().iter().any(|l| {
            l == "say Go show friend some love and give them a follow! They were last playing Hades II. reply=s1"
        })
    })
    .await;
    e.ingest(as_mod(chat(Platform::Twitch, "s2", "Mo", "!so someone"))).await;
    eventually(|| tw.log().iter().any(|l| l == "say Go show someone some love and give them a follow! reply=s2")).await;
    e.ingest(chat(Platform::Twitch, "s3", "viewer", "!so friend")).await; // mods only
    e.ingest(chat(Platform::Twitch, "u1", "viewer", "!uptime")).await;
    eventually(|| tw.log().iter().any(|l| l == "say We're not live right now. reply=u1")).await;
    assert!(!tw.log().iter().any(|l| l.ends_with("reply=s3")));
    assert_eq!(
        tw.log().iter().filter(|l| *l == "lookup friend").count(),
        1,
        "a viewer's !so asks the platform nothing"
    );
}
