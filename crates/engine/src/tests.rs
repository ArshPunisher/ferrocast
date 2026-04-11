//! Engine tests with a fake platform that records every action.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError};
use ferrocast_core::command::{Access, Command, Timer};
use ferrocast_core::event::*;
use ferrocast_core::filter::{Action as FAction, BlockedTerm};
use ferrocast_core::goal::{Goal, GoalKind};
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
        Arc::new(FakePlatform {
            platform: Some(p),
            ..Default::default()
        })
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
    async fn send_chat(
        &self,
        text: &str,
        reply_to: Option<&str>,
    ) -> ferrocast_connector::Result<()> {
        self.push(format!("say {text} reply={}", reply_to.unwrap_or("-")))
    }
    async fn delete_message(&self, id: &str) -> ferrocast_connector::Result<()> {
        self.push(format!("delete {id}"))
    }
    async fn timeout(
        &self,
        user: &str,
        s: u32,
        _r: Option<&str>,
    ) -> ferrocast_connector::Result<()> {
        self.push(format!("timeout {user} {s}"))
    }
    async fn ban(&self, user: &str, _r: Option<&str>) -> ferrocast_connector::Result<()> {
        if self.fail_bans {
            return Err(ConnectorError::Refused {
                platform: self.platform(),
                message: "no".into(),
            });
        }
        self.push(format!("ban {user}"))
    }
    async fn unban(&self, user: &str) -> ferrocast_connector::Result<()> {
        self.push(format!("unban {user}"))
    }
    async fn set_mode(&self, c: ModeChange) -> ferrocast_connector::Result<()> {
        self.push(format!("mode {:?} {}", c.mode, c.enabled))
    }
}

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Engine::start(dir.path()).await.unwrap(), dir)
}

fn chat(p: Platform, id: &str, user: &str, text: &str) -> Event {
    let mut a = Actor::named(p, user);
    a.id = Some(format!("{user}-id"));
    Event::live(
        p,
        Some(id.into()),
        Millis(0),
        EventKind::ChatMessage {
            actor: a,
            content: Content::plain(text),
            reply_to: None,
            is_action: false,
        },
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

async fn next_overlay_frame(
    rx: &mut tokio::sync::broadcast::Receiver<frames::OverlayFrame>,
    kind: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out")
            .expect("closed");
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
    f.terms.push(BlockedTerm {
        term: "badword".into(),
        action: FAction::Delete,
    });
    e.save_filter(f).await.unwrap();
    let mut rx = e.subscribe_overlays();
    e.ingest(chat(Platform::Twitch, "m-bad", "troll", "b.a.d.w.o.r.d"))
        .await;
    e.ingest(chat(Platform::Twitch, "m-ok", "fan", "great stream"))
        .await;
    let shown = next_overlay_frame(&mut rx, "event").await;
    assert_eq!(
        shown["data"]["source_id"], "m-ok",
        "the filtered message never reached overlays"
    );
    eventually(|| tw.log().contains(&"delete m-bad".to_string())).await;
    let o = e
        .create_overlay("Main", ferrocast_core::overlay::Preset::ChatOnly)
        .await
        .unwrap();
    let ov = e.overlay(&o.id).await.unwrap();
    let snap = e.overlay_snapshot(&ov).await.unwrap();
    let ids: Vec<_> = snap["state"]["chat"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["source_id"].clone())
        .collect();
    assert_eq!(ids, vec![Value::from("m-ok")]);
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
    e.ingest(chat(Platform::Kick, "k1", "Sam", "!discord"))
        .await;
    eventually(|| {
        kick.log()
            .contains(&"say Join us, Sam! reply=k1".to_string())
    })
    .await;
}

#[tokio::test]
async fn ban_everywhere_uses_linked_accounts_and_reports_failures() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    let kick = Arc::new(FakePlatform {
        platform: Some(Platform::Kick),
        fail_bans: true,
        ..Default::default()
    });
    e.register_connector(tw.clone());
    e.register_connector(kick.clone());
    e.ingest(chat(Platform::Twitch, "t1", "raider", "hi")).await;
    e.ingest(chat(Platform::Kick, "k1", "raider_k", "hi")).await;
    let store = e.store();
    tokio::task::spawn_blocking(move || {
        ferrocast_store::repo::viewers::link(
            &store,
            Platform::Kick,
            "raider_k-id",
            Platform::Twitch,
            "raider-id",
        )
    })
    .await
    .unwrap()
    .unwrap();
    let out = e
        .ban(
            Platform::Twitch,
            "raider-id",
            "raider",
            Some("hate raid".into()),
            true,
        )
        .await
        .unwrap();
    assert_eq!(out.ok, vec![Platform::Twitch]);
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].platform, Platform::Kick);
    assert!(tw.log().contains(&"ban raider-id".to_string()));
    assert_eq!(e.bans().entries.len(), 1);
    // The banned account chats again on Twitch: banned on sight and hidden.
    e.ingest(chat(Platform::Twitch, "t2", "raider", "back"))
        .await;
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
    let mut pending = tip(PaymentStatus::Pending);
    pending.source_id = Some("pay_9-pending".into());
    e.ingest(pending).await;
    assert!(e.queue().await.playing.is_none());
    assert_eq!(e.goals()[0].current, 0);
    e.ingest(tip(PaymentStatus::Captured)).await;
    eventually(|| e.goals()[0].current == 50_000).await;
    assert!(e.queue().await.playing.is_some());
    e.ingest(Event::live(
        Platform::Tips,
        Some("refund_1".into()),
        Millis(0),
        EventKind::Reversal {
            original_source_id: "pay_9".into(),
            reason: ReversalReason::Chargeback,
        },
    ))
    .await;
    assert_eq!(e.goals()[0].current, 0);
}

#[tokio::test]
async fn panic_mode_switches_modes_and_pauses_alerts() {
    let (e, _d) = engine().await;
    let tw = FakePlatform::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let plan = e.panic(true).await.unwrap();
    assert_eq!(plan.platforms.len(), 1);
    assert!(e.queue().await.paused);
    assert!(
        tw.log()
            .iter()
            .any(|l| l.starts_with("mode FollowersOnly true"))
    );
    e.panic(false).await.unwrap();
    assert!(!e.queue().await.paused);
    assert!(tw.log().iter().any(|l| l.starts_with("mode Slow false")));
}

#[tokio::test]
async fn the_queue_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let e = Engine::start(dir.path()).await.unwrap();
        e.queue_pause().await;
        e.test("tip", None).await.unwrap();
        e.test("raid", None).await.unwrap();
        eventually(|| true).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while e.queue().await.waiting.len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let e = Engine::start(dir.path()).await.unwrap();
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
    assert!(
        timers::tick(&e, &mut last, Millis(0)).is_empty(),
        "not live"
    );
    e.ingest(Event::live(
        Platform::Twitch,
        Some("on".into()),
        Millis(0),
        EventKind::StreamOnline { title: None },
    ))
    .await;
    assert!(
        timers::tick(&e, &mut last, Millis(0)).is_empty(),
        "no chat yet"
    );
    e.ingest(chat(Platform::Twitch, "c1", "a", "hi")).await;
    e.ingest(chat(Platform::Twitch, "c2", "b", "hey")).await;
    assert_eq!(timers::tick(&e, &mut last, Millis(0)).len(), 1);
    assert!(
        timers::tick(&e, &mut last, Millis(60_000)).is_empty(),
        "too soon"
    );
    eventually(|| {
        tw.log()
            .contains(&"say Follow on Twitch reply=-".to_string())
    })
    .await;
}

#[tokio::test]
async fn backfill_goes_to_the_feed_with_a_reason() {
    let (e, _d) = engine().await;
    let mut ev = Event::live(
        Platform::Twitch,
        Some("f-old".into()),
        Millis(0),
        EventKind::Follow {
            actor: Actor::named(Platform::Twitch, "late"),
        },
    );
    ev.origin = Origin::Backfill;
    e.ingest(ev).await;
    let logged = e.events(None, 5, vec![]).await.unwrap();
    let why = e.why(&logged[0].id).unwrap();
    assert!(!why.alerted);
    assert!(why.reasons[0].contains("reconnect"));
    assert!(e.queue().await.playing.is_none());
}
