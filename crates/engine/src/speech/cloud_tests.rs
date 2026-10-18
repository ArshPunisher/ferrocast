//! Ferrocast voices against the real relay (`ferrocast_relay::router` with its `FakeTts`):
//! only lines the local rules let through are sent, the audio lands in the media library,
//! and a lapsed Pro, a refusal or a slow relay falls back to the system voice with one notice.

use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::event::{Actor, Event, EventKind, Millis, PaymentStatus, Platform, TipProvider};
use ferrocast_core::money::Money;
use ferrocast_relay::billing::store::Synced;
use ferrocast_relay::tts::{FakeTts, TtsError, TtsProvider};
use ferrocast_store::repo;
use serde_json::{Value, json};

use super::*;
use crate::{Engine, EngineError, PlatformApps, now};

const HINDI: &str = "google:hi-IN-Neural2-A";

struct World {
    e: Engine,
    fake: Arc<FakeTts>,
    relay_db: Arc<ferrocast_relay::Db>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// A relay with `FakeTts`, and a PC whose install has Pro there and in its remembered status.
async fn world() -> World {
    let rdir = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let fake = Arc::new(FakeTts::new());
    let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
        .with_tts(Some(fake.clone() as Arc<dyn TtsProvider>), 800_000);
    let relay_db = state.db.clone();
    let app = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });

    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(base), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    let auth = e.relay_auth().await.unwrap();
    let install = auth.split('.').next().unwrap().to_string();
    relay_db.add_pro_sub("sub_1", "monthly", "e", "created", &install).unwrap();
    let s = Synced {
        status: "active".into(),
        current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
        cancel_at_end: None,
    };
    relay_db.sync_pro_sub(None, "sub_1", &s).unwrap();
    remember(&e, true, true).await;
    let mut tts = e.tts_policy();
    tts.enabled = true;
    e.save_tts(tts).await.unwrap();
    World { e, fake, relay_db, _dirs: (rdir, dir) }
}

/// The Pro status the PC remembers (no network): Pro or not, Ferrocast voices on the relay or not.
async fn remember(e: &Engine, pro: bool, tts: bool) {
    let n = now().0;
    let status = json!({
        "pro": pro, "status": if pro { "active" } else { "halted" }, "plan": "monthly", "subscription_id": "sub_1",
        "current_end": n + 30 * 86_400_000, "access_until": n + 33 * 86_400_000, "checked_at": n,
        "features": { "tts": tts }, "quotas": {}
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
}

fn tip(id: &str, minor: i64, message: &str) -> Event {
    Event::live(
        Platform::Tips,
        Some(id.into()),
        Millis(0),
        EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: Money::new(minor, "INR"),
            message: Some(message.into()),
            provider: TipProvider::Razorpay,
            payment_ref: id.into(),
            status: PaymentStatus::Captured,
        },
    )
}

/// The waiting alert's speech, once it's attached.
async fn attached_audio(e: &Engine) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let q = e.queue().await;
        if let Some(w) = q.waiting.first()
            && let Some(id) = w.alert.tts_audio.clone()
        {
            let store = e.store();
            let found = tokio::task::spawn_blocking(move || repo::media::get(&store, &id)).await.unwrap().unwrap();
            return (found.unwrap().0.mime, w.alert.tts_text.clone().unwrap_or_default());
        }
        assert!(tokio::time::Instant::now() < deadline, "no speech attached");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn notices(rx: &mut tokio::sync::broadcast::Receiver<crate::frames::LiveFrame>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "notice" && v["data"]["key"] == NOTICE_KEY {
            out.push(v["data"].clone());
        }
    }
    out
}

#[tokio::test]
async fn cloud_voices_are_listed_only_with_pro_and_the_feature() {
    let w = world().await;
    let list = w.e.tts_voices().await;
    let cloud: Vec<&Value> = list.iter().filter(|v| v["pro"] == true).collect();
    assert_eq!(cloud.len(), 10);
    assert_eq!(cloud[0]["id"], HINDI);
    assert_eq!(cloud[0]["language"], "hi-IN");
    assert!(list.iter().filter(|v| v["pro"] == false).all(|v| !v["id"].as_str().unwrap().starts_with(CLOUD_PREFIX)));
    remember(&w.e, true, false).await;
    assert!(w.e.tts_voices().await.iter().all(|v| v["pro"] == false), "an older relay without the feature");
    remember(&w.e, false, true).await;
    assert!(w.e.tts_voices().await.iter().all(|v| v["pro"] == false), "Pro lapsed");
}

#[tokio::test]
async fn picking_a_voice_needs_pro_and_a_listed_voice() {
    let w = world().await;
    let v = w.e.tts_cloud().await;
    assert!(v.available && v.voice.is_none() && !v.resting);
    assert_eq!(v.voices.len(), 10);
    assert_eq!(v.usage, Some(CloudUsage { used: 0, limit: 20_000, reset_at: v.usage.unwrap().reset_at }));
    let err = w.e.set_tts_cloud(Some("google:xx-XX-Nope".into())).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 404, code: "voice_not_found", .. }), "{err:?}");
    let v = w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    assert_eq!(v.voice.as_deref(), Some(HINDI));
    remember(&w.e, false, true).await;
    let err = w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 402, code: "pro_required", .. }), "{err:?}");
    let v = w.e.set_tts_cloud(None).await.unwrap();
    assert!(v.voice.is_none() && !v.available, "going back to the system voice always works");
    assert!(w.fake.calls().is_empty(), "nothing was spoken");
}

#[tokio::test]
async fn only_text_the_local_rules_approved_is_sent() {
    let w = world().await;
    w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    let mut tts = w.e.tts_policy();
    tts.hold_for_approval = true;
    w.e.save_tts(tts).await.unwrap();
    w.e.queue_pause().await;
    // Under the minimum (₹100 by default): never read, never sent.
    w.e.ingest(tip("pay_small", 5_000, "too small to read")).await;
    // Held for approval: not sent until a mod approves it.
    w.e.ingest(tip("pay_held", 50_000, "great stream")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(w.fake.calls().is_empty(), "nothing is sent before approval: {:?}", w.fake.calls());
    let pending = w.e.tts_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    let held_text = pending[0]["text"].as_str().unwrap().to_string();
    let mut overlays = w.e.subscribe_overlays();
    w.e.decide_tts(pending[0]["id"].as_str().unwrap(), true).await.unwrap();
    assert_eq!(w.fake.calls(), [(held_text.clone(), HINDI.to_string())], "exactly the approved line");
    assert!(held_text.contains("great stream"));
    let frame = loop {
        let f = overlays.recv().await.unwrap();
        if f.text.contains("\"type\":\"tts\"") {
            break serde_json::from_str::<Value>(&f.text).unwrap();
        }
    };
    let id = frame["data"]["audio"].as_str().expect("audio attached").to_string();
    let store = w.e.store();
    let m = tokio::task::spawn_blocking(move || repo::media::get(&store, &id)).await.unwrap().unwrap().unwrap();
    assert_eq!(m.0.mime, "audio/ogg");

    // A rejected one is never sent.
    w.e.ingest(tip("pay_rejected", 50_000, "please do not read")).await;
    let pending = w.e.tts_pending().await.unwrap();
    w.e.decide_tts(pending[0]["id"].as_str().unwrap(), false).await.unwrap();
    assert_eq!(w.fake.calls().len(), 1);
}

#[tokio::test]
async fn an_alerts_line_is_read_by_the_cloud_voice_and_saved_like_the_wav() {
    let w = world().await;
    w.e.set_tts_cloud(Some("google:en-IN-Neural2-A".into())).await.unwrap();
    w.e.queue_pause().await;
    w.e.ingest(tip("pay_1", 50_000, "love the stream")).await;
    let (mime, text) = attached_audio(&w.e).await;
    assert_eq!(mime, "audio/ogg");
    assert_eq!(w.fake.calls(), [(text, "google:en-IN-Neural2-A".to_string())]);
    let used = w.relay_db.quota_used("sub_1", "tts_chars", ferrocast_relay::quota::Period::Month).unwrap();
    assert_eq!(used, w.fake.calls()[0].0.chars().count() as i64);
    assert_eq!(w.e.tts_cloud().await.usage.unwrap().used, used, "usage follows the relay");
}

#[tokio::test]
async fn a_refusal_falls_back_to_the_system_voice_with_one_notice() {
    let w = world().await;
    w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    w.fake.fail_with(Some(TtsError::Unavailable("down".into())));
    let mut live = w.e.subscribe_live();
    let a = w.e.speak_line("first line", None).await;
    let b = w.e.speak_line("second line", None).await;
    assert_eq!(w.fake.calls().len(), 2, "each line still tries the Ferrocast voice first");
    let n = notices(&mut live);
    assert_eq!(n.len(), 1, "one notice, not one per line: {n:?}");
    assert_eq!(n[0]["level"], "warn");
    assert!(n[0]["message"].as_str().unwrap().contains("can't answer right now"), "{n:?}");
    assert!(n[0]["message"].as_str().unwrap().contains("Reading with this PC"), "{n:?}");
    assert_eq!(n[0]["action"]["href"], "#/settings/tts");
    assert!(w.e.tts_cloud().await.fallback.unwrap().contains("can't answer"));
    if cfg!(target_os = "macos") {
        let a = a.expect("the system voice read it");
        assert!(!a.cloud && a.notice.is_some());
        assert!(b.is_some());
    }
    // Working again: the next failure tells the streamer again.
    w.fake.fail_with(None);
    assert!(w.e.speak_line("third line", None).await.unwrap().cloud);
    assert!(w.e.tts_cloud().await.fallback.is_none());
    w.fake.fail_with(Some(TtsError::Quota));
    w.e.speak_line("fourth line", None).await;
    assert_eq!(notices(&mut live).len(), 1);
}

#[tokio::test]
async fn a_lapsed_pro_or_a_long_line_never_reaches_the_relay() {
    let w = world().await;
    w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    let mut live = w.e.subscribe_live();
    w.e.speak_line(&"a".repeat(CLOUD_MAX_CHARS + 1), None).await;
    let n = notices(&mut live);
    assert!(n[0]["message"].as_str().unwrap().contains("300 characters"), "{n:?}");
    remember(&w.e, false, true).await;
    w.e.speak_line("hello", None).await;
    assert!(w.fake.calls().is_empty());
    assert!(w.e.tts_cloud().await.fallback.unwrap().contains("part of Pro"));
}

#[tokio::test]
async fn the_month_used_up_falls_back_with_the_relays_reason() {
    let w = world().await;
    w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    w.relay_db.try_take("sub_1", "tts_chars", ferrocast_relay::quota::Period::Month, 20_000, 20_000).unwrap();
    let mut live = w.e.subscribe_live();
    w.e.speak_line("hello", None).await;
    assert!(w.fake.calls().is_empty(), "the relay refused before the provider");
    let n = notices(&mut live);
    assert!(n[0]["message"].as_str().unwrap().contains("this month's Ferrocast voice characters"), "{n:?}");
}

#[tokio::test]
async fn a_slow_voice_never_holds_a_line_past_eight_seconds() {
    let w = world().await;
    w.e.set_tts_cloud(Some(HINDI.into())).await.unwrap();
    w.fake.slow(Some(Duration::from_secs(30)));
    let mut live = w.e.subscribe_live();
    let started = tokio::time::Instant::now();
    let e = w.e.clone();
    let speak = tokio::spawn(async move { e.speak_line("hello", None).await });
    // Timed to the notice, not to the end of the line: the system voice that takes over can be
    // slow on a CI runner, and that isn't the wait this test is about.
    let notice = tokio::time::timeout(CLOUD_TIMEOUT + Duration::from_secs(5), async {
        loop {
            let f = live.recv().await.unwrap();
            let v: Value = serde_json::from_str(&f.text).unwrap();
            if v["type"] == "notice" && v["data"]["key"] == NOTICE_KEY {
                return v["data"].clone();
            }
        }
    })
    .await
    .expect("a notice");
    let took = started.elapsed();
    // The relay gives up on the provider at 7 s (and says so); the PC at 8 s whatever happens.
    assert!(took >= Duration::from_secs(6) && took < CLOUD_TIMEOUT + Duration::from_millis(1500), "{took:?}");
    tokio::time::timeout(Duration::from_secs(30), speak).await.expect("the line ends").unwrap();
    let m = notice["message"].as_str().unwrap();
    assert!(m.contains("can't answer right now") || m.contains("didn't answer in time"), "{notice:?}");
    let used = w.relay_db.quota_used("sub_1", "tts_chars", ferrocast_relay::quota::Period::Month).unwrap();
    assert_eq!(used, 0, "the characters came back");
}

#[tokio::test]
async fn a_preview_goes_the_same_way_and_is_made_once() {
    let w = world().await;
    let p = w.e.tts_preview(Some(HINDI.into())).await.unwrap();
    assert!(p.cloud && p.notice.is_none());
    assert_eq!(p.url, format!("/api/v1/media/{}", p.media_id));
    assert_eq!(w.fake.calls(), [(sample_line("hi-IN").to_string(), HINDI.to_string())]);
    let again = w.e.tts_preview(Some(HINDI.into())).await.unwrap();
    assert_eq!(again.media_id, p.media_id);
    assert_eq!(w.fake.calls().len(), 1, "the second preview spends nothing");
    w.e.tts_preview(Some("google:en-IN-Neural2-B".into())).await.unwrap();
    assert_eq!(w.fake.calls()[1].0, sample_line("en-IN"));
    let err = w.e.tts_preview(Some("google:xx-XX-Nope".into())).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 404, .. }), "{err:?}");
}
