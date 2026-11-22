//! Fast 8-hour stream soak test across core state machines and store retention (no sleeps).
//!
//! Steps simulated stream time from `t = 0` to `t = 8 h` (`28_800_000 ms`) while feeding high-rate
//! chat, commands, auto-replies, chatbot modules, loyalty activity, alerts, live-watch device
//! churn, and ephemeral TTS media files. Asserts every bounded collection stays within its cap
//! and deleted/expired entries return to 0.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ferrocast_core::alert::RenderedAlert;
use ferrocast_core::autoreply::{AutoReply, Cooldowns as AutoReplyCooldowns, Match, MatchKind, RuleSet};
use ferrocast_core::chatbot::KeyedCooldowns;
use ferrocast_core::command::{self, Access, Command, Cooldowns as CmdCooldowns};
use ferrocast_core::dedupe::Deduper;
use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
use ferrocast_core::loyalty::{Activity, LoyaltyConfig};
use ferrocast_core::queue::{AckPhase, AlertQueue, QueueTuning};
use ferrocast_core::show::{LiveWatch, LiveWatchConfig, WatchSample};
use ferrocast_core::template::Vars;
use ferrocast_store::{Store, repo};

const EIGHT_HOURS_MS: i64 = 8 * 3_600_000;

fn actor(i: usize) -> Actor {
    let name = format!("viewer_{i}");
    let mut a = Actor::named(Platform::Twitch, &name);
    a.id = Some(format!("id_{i}"));
    a
}

fn chat_event(i: usize, at: Millis) -> Event {
    Event::live(
        Platform::Twitch,
        Some(format!("msg-{i}")),
        at,
        EventKind::ChatMessage { actor: actor(i), content: Content::plain("hello"), reply_to: None, is_action: false },
    )
}

fn rendered_alert(id: usize) -> RenderedAlert {
    RenderedAlert {
        alert_id: format!("alert-{id}"),
        event_id: format!("ev-{id}"),
        rule_id: "r1".into(),
        variation_id: "v1".into(),
        kind: "subscription".into(),
        platform: Platform::Twitch,
        title: "Sub".into(),
        body: "Thanks!".into(),
        image: None,
        sound: None,
        volume: 100,
        duration_ms: 3_000,
        layout: "above".into(),
        css: String::new(),
        tts_text: None,
        tts_audio: None,
        test: false,
        anim: None,
        custom: None,
        design: None,
    }
}

#[test]
fn eight_hour_stream_soak_keeps_all_collections_bounded() {
    let base_ms = 1_793_300_000_000i64;

    // 1. Deduper: 60,000 events spread across 8 hours (15-min window, 50k cap).
    let mut deduper = Deduper::new(900_000, 50_000);
    for i in 0..60_000usize {
        let t = Millis(base_ms + (i as i64 * EIGHT_HOURS_MS) / 60_000);
        let ev = chat_event(i, t);
        assert!(deduper.first_time(&ev, t));
        assert!(deduper.len() <= 50_000, "deduper exceeded cap: {}", deduper.len());
    }

    // 2. Command cooldowns: 40,000 unique viewers across 8 hours with active + deleted commands.
    let cmd = Command {
        id: "so".into(),
        name: "!so".into(),
        aliases: vec![],
        response: "Hi {user}".into(),
        access: Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 3600,
        platforms: vec![],
        enabled: true,
    };
    let mut cmd_cd = CmdCooldowns::default();
    let empty_vars = Vars::new();
    for i in 0..40_000usize {
        let t = Millis(base_ms + (i as i64 * EIGHT_HOURS_MS) / 40_000);
        let a = actor(i);
        let _ = command::run(std::slice::from_ref(&cmd), &mut cmd_cd, Platform::Twitch, &a, "!so", &empty_vars, t);
        assert!(cmd_cd.len().1 <= 20_000, "command per_user exceeded cap: {}", cmd_cd.len().1);
    }

    // 3. Auto-reply cooldowns: 40,000 unique viewers within long window.
    let ar = AutoReply {
        id: "ar1".into(),
        name: "Hello".into(),
        enabled: true,
        matcher: Match { kind: MatchKind::Contains, text: "hello".into(), case_sensitive: false },
        reply: "Hey {user}".into(),
        cooldown_s: 5,
        user_cooldown_s: 3600,
        platforms: vec![],
        min_role: Access::Everyone,
        only_when_live: false,
    };
    let (set, bad) = RuleSet::compile(std::slice::from_ref(&ar));
    assert!(bad.is_empty() && !set.is_empty());
    let mut ar_cd = AutoReplyCooldowns::default();
    for i in 0..40_000usize {
        let t = Millis(base_ms + (i as i64 * EIGHT_HOURS_MS) / 40_000);
        ar_cd.record(&ar, Platform::Twitch, &format!("u-{i}"), t);
        assert!(ar_cd.len().1 <= 20_000, "autoreply per_user exceeded cap: {}", ar_cd.len().1);
    }

    // 4. Chatbot KeyedCooldowns: 40,000 unique keys across 8 hours.
    let mut keyed = KeyedCooldowns::default();
    for i in 0..40_000usize {
        let t_ms = base_ms + (i as i64 * EIGHT_HOURS_MS) / 40_000;
        assert!(keyed.take(&format!("!deaths:u-{i}"), 3600, t_ms));
        assert!(keyed.len() <= 20_000, "keyed cooldowns exceeded cap: {}", keyed.len());
    }

    // 5. AlertQueue: 5,000 alerts across 8 hours (mix of normal finishes and burst folding).
    let mut q = AlertQueue::new(QueueTuning::default());
    for i in 0..5_000usize {
        let t = Millis(base_ms + (i as i64 * EIGHT_HOURS_MS) / 5_000);
        let a = rendered_alert(i);
        let aid = a.alert_id.clone();
        let _ = q.enqueue(a, 0, t);
        assert!(q.len() <= q.tuning.max_waiting + 1, "alert queue exceeded cap: {}", q.len());
        if i % 3 == 0 {
            let _ = q.ack(&aid, AckPhase::Finished, Millis(t.0 + 3_000));
        } else {
            let _ = q.tick(Millis(t.0 + 10_000));
        }
    }
    let _ = q.tick(Millis(base_ms + EIGHT_HOURS_MS + 600_000));
    assert!(q.len() <= 1, "queue drained cleanly after stream end: {}", q.len());

    // 6. Loyalty activity: 96 five-minute intervals over 8 hours, 1,000 viewers/interval.
    let loyalty_cfg = LoyaltyConfig { enabled: true, interval_min: 5, ..LoyaltyConfig::default() };
    let mut activity = Activity::default();
    // Anchor the interval clock at stream start.
    assert!(activity.due(&loyalty_cfg, true, Millis(base_ms)).is_empty());
    for interval in 1..=96usize {
        for v in 0..1_000usize {
            let a = actor(interval * 1_000 + v);
            activity.saw(&a);
        }
        assert!(activity.len() <= 50_000);
        let due_at = Millis(base_ms + interval as i64 * 5 * 60_000);
        let drained = activity.due(&loyalty_cfg, true, due_at);
        assert_eq!(drained.len(), 1_000);
        assert_eq!(activity.len(), 0);
    }

    // 7. LiveWatch: 14,400 samples (every 2 s for 8 hours) with device/destination churn.
    let cfg = LiveWatchConfig::default();
    let mut watch = LiveWatch::default();
    for tick in 0..14_400i64 {
        let epoch = tick / 600; // Rotate names every 20 minutes to simulate replugs/restarts.
        let s = WatchSample {
            at: base_ms + tick * 2_000,
            mics: vec![(format!("Mic-{epoch}"), -25.0)],
            cameras: vec![(format!("Cam-{epoch}"), false, tick as u64)],
            destinations: vec![(format!("Dest-{epoch}"), true)],
            ..Default::default()
        };
        let _ = watch.tick(&cfg, &s);
        let (h, c, d, sn) = watch.tracked_counts();
        assert!(h <= 1 && c <= 1 && d <= 1 && sn <= 1, "live watch leaked entries: ({h}, {c}, {d}, {sn})");
    }
    let _ = watch.tick(&cfg, &WatchSample { at: base_ms + EIGHT_HOURS_MS + 2_000, ..Default::default() });
    assert_eq!(watch.tracked_counts(), (0, 0, 0, 0));

    // 8. Store TTS retention: 200 speech files over 8 hours prune cleanly while keeping user uploads.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let user_sound = repo::media::save(&store, "fanfare.wav", b"RIFF\0\0\0\0WAVEfmt ").unwrap();
    for _ in 0..200 {
        let _ = repo::media::save_speech(&store, "speech.wav", b"RIFF\0\0\0\0WAVEfmt ").unwrap();
    }
    let old = user_sound.created_at - 7_200_000;
    store
        .write(|tx| {
            tx.execute("UPDATE media SET created_at = ?1", [old])?;
            Ok(())
        })
        .unwrap();
    let removed = repo::media::prune_speech(&store, user_sound.created_at - 3_600_000, &[]).unwrap();
    assert_eq!(removed, 200);
    let left = repo::media::list(&store).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, user_sound.id);
}
