//! Click-order race integration tests for the locked broadcast state machine (M9).
//!
//! Covers every race called out in M9-fix:
//! 1. Stop Studio / End stream during `Preparing` (slow `stream_start`)
//! 2. Go live during a countdown + concurrent `skip_countdown` + `cancel_countdown` vs `skip_countdown`
//! 3. Stop/Start Studio mid-show + delayed stop vs a plain stream started without a Show + End now
//! 4. A platform failing during Show start (Kick multistream refusal -> Failed -> Studio stops LIVE & Show ends)
//! 5. Scene collection / profile switch while live, recording, or in an active Show

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrocast_connector::{Connector, Ingest};
use ferrocast_core::event::Platform;
use ferrocast_core::moderation::{Capabilities, ModeChange};
use ferrocast_engine::golive::{DestAdvanced, GoLiveConfig};
use ferrocast_engine::studio::show::{Phase, ShowStartOpts};
use ferrocast_engine::studio_host::{PreviewRect, ShelfAct, StudioAct, StudioHost, StudioSettings, StudioStatus};
use ferrocast_engine::{Engine, EngineError};
use ferrocast_golive::push::{DEFAULT_MAX_ATTEMPTS, PushState};
use ferrocast_testkit::rtmp::{Obs, platform_dropping};
use serde_json::{Value, json};
use tokio::sync::Notify;

#[derive(Debug)]
struct Barrier {
    entered: Notify,
    release: Notify,
    armed: AtomicBool,
}

impl Barrier {
    fn new() -> Self {
        Self { entered: Notify::new(), release: Notify::new(), armed: AtomicBool::new(false) }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_if_armed(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

#[derive(Debug)]
struct RaceStudio {
    running: AtomicBool,
    streaming: AtomicBool,
    recording: AtomicBool,
    current: Mutex<String>,
    stream_start_barrier: Barrier,
    switch_live_barrier: Barrier,
    shelf_switches: AtomicUsize,
}

impl Default for RaceStudio {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(true),
            streaming: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            current: Mutex::new("Gameplay".into()),
            stream_start_barrier: Barrier::new(),
            switch_live_barrier: Barrier::new(),
            shelf_switches: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl StudioHost for RaceStudio {
    async fn status(&self) -> StudioStatus {
        StudioStatus { available: true, running: self.running.load(Ordering::SeqCst), ..Default::default() }
    }
    async fn start(&self) -> Result<StudioStatus, String> {
        self.running.store(true, Ordering::SeqCst);
        Ok(self.status().await)
    }
    async fn stop(&self) -> Result<StudioStatus, String> {
        self.running.store(false, Ordering::SeqCst);
        self.streaming.store(false, Ordering::SeqCst);
        self.recording.store(false, Ordering::SeqCst);
        Ok(self.status().await)
    }
    async fn kinds(&self) -> Result<Value, String> {
        Ok(json!({ "inputs": [] }))
    }
    async fn options(&self, _kind: String) -> Result<Value, String> {
        Ok(json!([]))
    }
    async fn preview(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    async fn defaults(&self, _kind: String) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn act(&self, act: StudioAct) -> Result<Value, String> {
        match act {
            StudioAct::Scenes => Ok(json!({
                "current": self.current.lock().unwrap().clone(),
                "scenes": [
                    { "name": "Starting", "items": 1 },
                    { "name": "Gameplay", "items": 1 },
                    { "name": "Ending", "items": 1 },
                ]
            })),
            StudioAct::Items { .. } => {
                Ok(json!([{ "id": 1, "name": "Cam", "kind": "av_capture_input", "visible": true }]))
            }
            StudioAct::SetCurrent { scene } => {
                if scene == "Gameplay" {
                    self.switch_live_barrier.wait_if_armed().await;
                }
                *self.current.lock().unwrap() = scene;
                Ok(Value::Null)
            }
            StudioAct::Audio { .. } => Ok(json!([{ "name": "Mic", "muted": false }])),
            StudioAct::SetAudio { .. } => Ok(Value::Null),
            other => Err(format!("not faked: {other:?}")),
        }
    }
    async fn settings(&self) -> StudioSettings {
        StudioSettings::default()
    }
    async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn stats(&self) -> Result<Value, String> {
        Ok(json!({
            "disk_free_mb": 500_000.0,
            "stream": { "active": self.streaming.load(Ordering::SeqCst), "reconnecting": false },
            "record": { "active": self.recording.load(Ordering::SeqCst) },
            "replay": { "active": false },
            "virtualcam": { "active": false },
            "ndi": { "active": false },
            "source_records": [],
        }))
    }
    async fn record(&self, on: bool) -> Result<Value, String> {
        self.recording.store(on, Ordering::SeqCst);
        Ok(json!({ "active": on }))
    }
    async fn record_pause(&self, _on: bool) -> Result<(), String> {
        Ok(())
    }
    async fn monitoring(&self) -> Result<Value, String> {
        Ok(json!({ "devices": [] }))
    }
    async fn stream_start(
        &self,
        _server: String,
        _key: String,
        _tracks: Vec<(u32, u32, u32)>,
    ) -> Result<Value, String> {
        self.stream_start_barrier.wait_if_armed().await;
        if !self.running.load(Ordering::SeqCst) {
            return Err("Start the Studio first.".into());
        }
        self.streaming.store(true, Ordering::SeqCst);
        Ok(json!({ "active": true }))
    }
    async fn stream_stop(&self) -> Result<(), String> {
        self.streaming.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn stream_stats(&self) -> Result<Value, String> {
        if self.streaming.load(Ordering::SeqCst) {
            Ok(json!({ "active": true, "reconnecting": false }))
        } else {
            Ok(Value::Null)
        }
    }
    async fn obs_collections(&self) -> Result<Value, String> {
        Ok(json!([{ "id": "obs_one", "name": "OBS One" }]))
    }
    async fn import_obs(&self, _id: String) -> Result<Value, String> {
        Ok(json!({ "id": "obs_one" }))
    }
    async fn shelf(&self, _kind: String, act: ShelfAct) -> Result<Value, String> {
        match act {
            ShelfAct::List => Ok(
                json!({ "current": "default", "list": [{ "id": "default", "name": "Default" }, { "id": "alt", "name": "Alt" }] }),
            ),
            ShelfAct::Export { .. } => Ok(json!({ "name": "Default", "sources": [] })),
            ShelfAct::Switch { id } => {
                self.shelf_switches.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "id": id }))
            }
            ShelfAct::New { name, .. } => Ok(json!({ "id": name })),
            ShelfAct::Import { name, .. } => Ok(json!({ "id": name })),
            ShelfAct::Rename { .. } | ShelfAct::Remove { .. } => Ok(Value::Null),
        }
    }
}

#[derive(Debug)]
struct ChatRecorder {
    platform: Platform,
    said: Mutex<Vec<String>>,
    ingest_url: Option<String>,
}

impl ChatRecorder {
    fn twitch() -> Arc<Self> {
        Arc::new(Self { platform: Platform::Twitch, said: Mutex::new(Vec::new()), ingest_url: None })
    }

    fn kick(url: String) -> Arc<Self> {
        Arc::new(Self { platform: Platform::Kick, said: Mutex::new(Vec::new()), ingest_url: Some(url) })
    }

    fn said(&self) -> Vec<String> {
        self.said.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connector for ChatRecorder {
    fn platform(&self) -> Platform {
        self.platform
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: false, can_timeout: false, can_delete: false }
    }
    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
        self.said.lock().unwrap().push(text.to_string());
        Ok(())
    }
    async fn delete_message(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn timeout(&self, _: &str, _: u32, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ban(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn unban(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn set_mode(&self, _: ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ingest(&self) -> ferrocast_connector::Result<Ingest> {
        match &self.ingest_url {
            Some(url) => Ok(Ingest { url: url.clone(), key: "kick-k".into() }),
            None => Err(ferrocast_connector::ConnectorError::NotSupported { platform: self.platform, what: "ingest" }),
        }
    }
}

async fn setup() -> (Engine, Arc<RaceStudio>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    e.start_golive(0).await;
    let studio = Arc::new(RaceStudio::default());
    e.set_studio_host(studio.clone());
    (e, studio, dir)
}

#[tokio::test]
async fn stop_during_starting_cancels_show_and_leaves_no_orphan_stream() {
    let (e, studio, _dir) = setup().await;
    e.patch_show_config(
        json!({
            "starting_scene": "Starting", "live_scene": "Gameplay", "countdown_s": 10,
            "start_stream": true, "stop_stream": true, "stop_delay_s": 0,
            "discord": false, "alerts_on": false, "bot_on": false, "recap_message": null,
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();

    // 1. Stop Studio while `start_show` is blocked inside `stream_start`.
    studio.stream_start_barrier.arm();
    let e_start = e.clone();
    let start_task = tokio::spawn(async move { e_start.start_show(true).await });
    studio.stream_start_barrier.entered.notified().await;
    assert!(e.is_show_active(), "Preparing is active while stream_start runs");

    // Double start while Preparing is rejected immediately with 409 Conflict.
    assert!(matches!(e.start_show(true).await, Err(EngineError::Conflict(_))));

    let e_stop = e.clone();
    let stop_task = tokio::spawn(async move { e_stop.studio_stop().await });
    // Let `studio_stop` register its cancellation before releasing `stream_start`.
    tokio::time::sleep(Duration::from_millis(25)).await;
    studio.stream_start_barrier.release.notify_one();

    let start_res = start_task.await.unwrap();
    stop_task.await.unwrap().unwrap();
    assert!(matches!(start_res, Err(EngineError::Conflict(_))), "cancelled start must not succeed: {start_res:?}");
    assert!(!e.is_show_active(), "Show is not stuck active after Stop during Starting");
    assert!(!studio.streaming.load(Ordering::SeqCst), "stream is off after Stop Studio");

    // Restart Studio and verify a fresh Show starts cleanly.
    studio.running.store(true, Ordering::SeqCst);

    // 2. End live while `start_show` is blocked inside `stream_start`.
    studio.stream_start_barrier.arm();
    let e_start = e.clone();
    let start_task =
        tokio::spawn(async move { e_start.start_show_with(ShowStartOpts { force: true, stream: Some(true) }).await });
    studio.stream_start_barrier.entered.notified().await;

    let e_end = e.clone();
    let end_task = tokio::spawn(async move { e_end.end_live().await });
    tokio::time::sleep(Duration::from_millis(25)).await;
    studio.stream_start_barrier.release.notify_one();

    let start_res = start_task.await.unwrap();
    end_task.await.unwrap().unwrap();
    assert!(matches!(start_res, Err(EngineError::Conflict(_))), "end_live cancelled Preparing: {start_res:?}");
    assert!(!e.is_show_active());
    assert!(!studio.streaming.load(Ordering::SeqCst), "no orphaned live stream after end_live during Starting");
}

#[tokio::test]
async fn go_live_during_countdown_and_cancel_vs_skip_race() {
    let (e, studio, _dir) = setup().await;
    let chat = ChatRecorder::twitch();
    e.register_connector(chat.clone());
    e.patch_show_config(
        json!({
            "starting_scene": "Starting", "live_scene": "Gameplay", "countdown_s": 30,
            "start_stream": false, "stop_stream": false, "stop_delay_s": 0,
            "discord": false, "alerts_on": false, "bot_on": false, "recap_message": null,
            "at_zero": { "switch": true, "chat": "We are live!", "marker": false }
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();

    // 1. Concurrent skip_countdown calls: only one wins, and chat is posted once (no double post).
    let run = e.start_show(true).await.unwrap();
    assert_eq!(run.phase, Phase::Countdown);
    assert!(matches!(e.start_show(true).await, Err(EngineError::Conflict(_))), "double start refused during countdown");

    studio.switch_live_barrier.arm();
    let e1 = e.clone();
    let t1 = tokio::spawn(async move { e1.skip_countdown().await });
    studio.switch_live_barrier.entered.notified().await;
    let second = e.skip_countdown().await;
    assert!(
        matches!(second, Err(EngineError::Missing { code: "no_countdown", .. })),
        "second skip rejected: {second:?}"
    );
    studio.switch_live_barrier.release.notify_one();
    let live = t1.await.unwrap().unwrap();
    assert_eq!(live.phase, Phase::Live);
    assert_eq!(chat.said().iter().filter(|m| m.as_str() == "We are live!").count(), 1, "zero double chat posts");
    e.end_show().await.unwrap();

    // 2. cancel_countdown racing an in-flight skip_countdown: countdown stays cancelled (Starting),
    //    never resurrected to Live, and chat is NOT posted.
    chat.said.lock().unwrap().clear();
    e.start_show(true).await.unwrap();
    studio.switch_live_barrier.arm();
    let e_skip = e.clone();
    let skip_task = tokio::spawn(async move { e_skip.skip_countdown().await });
    studio.switch_live_barrier.entered.notified().await;

    let cancelled = e.cancel_countdown().unwrap();
    assert_eq!(cancelled.phase, Phase::Starting);
    studio.switch_live_barrier.release.notify_one();
    let _ = skip_task.await.unwrap();

    assert_eq!(e.show_run().unwrap().phase, Phase::Starting, "cancelled countdown must never be resurrected to Live");
    assert!(chat.said().is_empty(), "at_zero chat must not post after cancel: {:?}", chat.said());
    e.end_show().await.unwrap();

    // 3. Unified go_live() during a countdown advances cleanly to Live.
    e.start_show(true).await.unwrap();
    assert_eq!(e.show_run().unwrap().phase, Phase::Countdown);
    e.go_live().await.unwrap();
    assert_eq!(e.show_run().unwrap().phase, Phase::Live);
}

#[tokio::test]
async fn stop_start_studio_mid_show_and_delayed_stop_vs_plain_stream() {
    let (e, studio, _dir) = setup().await;
    let chat = ChatRecorder::twitch();
    e.register_connector(chat.clone());
    e.patch_show_config(
        json!({
            "starting_scene": "Starting", "live_scene": "Gameplay", "countdown_s": 30,
            "start_stream": true, "stop_stream": true, "stop_delay_s": 1,
            "discord": false, "alerts_on": false, "bot_on": false,
            "recap_message": "Thanks everyone!"
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();

    // 1. Stop Studio mid-show cancels countdown so restarting Studio lets a new Show start.
    let run = e.start_show(true).await.unwrap();
    assert_eq!(run.phase, Phase::Countdown);
    e.studio_stop().await.unwrap();
    assert_eq!(e.show_run().unwrap().phase, Phase::Ended);
    assert!(!e.is_show_active());
    studio.running.store(true, Ordering::SeqCst);
    let second = e.start_show(true).await.unwrap();
    assert_eq!(second.phase, Phase::Countdown, "new Show starts cleanly after Stop/Start Studio");

    // 2. Delayed stop from an ended Show must NEVER kill a plain stream started without a Show.
    let ended = e.end_show().await.unwrap();
    assert_eq!(ended.phase, Phase::Ended);
    assert!(ended.steps.iter().any(|s| s.id == "stop_stream" && s.status == "waiting"));
    // User starts a plain stream (no Show) before the 1 s stop delay fires.
    e.studio_stream(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(
        studio.streaming.load(Ordering::SeqCst),
        "delayed stop from earlier Show must not kill a plain stream started without a Show"
    );
    assert!(
        e.show_run().unwrap().steps.iter().any(|s| s.id == "stop_stream" && s.status == "skipped"),
        "pending stop_stream step marked skipped when new stream started"
    );
    e.studio_stream(false).await.unwrap();

    // 3. End now (`end_show_with(true)` / second `end_live()`) brings a waiting stop forward
    //    immediately without double-posting recap chat.
    chat.said.lock().unwrap().clear();
    e.patch_show_config(json!({ "stop_delay_s": 60 }).as_object().unwrap()).await.unwrap();
    e.start_show(true).await.unwrap();
    e.end_show().await.unwrap();
    assert!(studio.streaming.load(Ordering::SeqCst), "still streaming during 60 s stop delay");
    assert_eq!(chat.said().len(), 1);
    e.end_live().await.unwrap();
    assert!(!studio.streaming.load(Ordering::SeqCst), "second end_live stopped immediately");
    assert_eq!(chat.said().len(), 1, "bringing stop forward did not double-post recap");
}

#[tokio::test]
async fn platform_failing_during_start_gives_up_and_stops_studio_live() {
    let (e, studio, _dir) = setup().await;
    let (kport, _kgot) = platform_dropping("kick-k", Duration::from_millis(40)).await;
    e.register_connector(ChatRecorder::kick(format!("rtmp://127.0.0.1:{kport}/app")));

    let order: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let mut adv = e.golive_config().advanced;
    adv.per.insert("kick".into(), DestAdvanced { first_retry_s: 1, max_retry_s: 1, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();

    e.patch_show_config(
        json!({
            "starting_scene": "Starting", "live_scene": "Gameplay", "countdown_s": 30,
            "start_stream": true, "stop_stream": true, "stop_delay_s": 0,
            "discord": false, "alerts_on": false, "bot_on": false, "recap_message": null,
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();

    let run = e.start_show(true).await.unwrap();
    assert_eq!(run.phase, Phase::Countdown);
    assert!(studio.streaming.load(Ordering::SeqCst));

    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();

    let mut reached_failed = None;
    for _ in 0..100 {
        let v = e.golive_view().await;
        if let Some(PushState::Failed { .. }) =
            v.destinations.iter().find(|d| d.id == "kick").and_then(|d| d.health.as_ref()).map(|h| &h.state)
            && !studio.streaming.load(Ordering::SeqCst)
            && e.show_run().is_some_and(|r| r.phase == Phase::Ended)
        {
            reached_failed =
                v.destinations.into_iter().find(|d| d.id == "kick").and_then(|d| d.health).map(|h| h.state);
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    match reached_failed {
        Some(PushState::Failed { attempts, reason, detail, refused }) => {
            assert_eq!(attempts, DEFAULT_MAX_ATTEMPTS);
            assert!(refused);
            assert!(reason.contains("Kick refused"), "{reason}");
            assert!(detail.as_deref().is_some_and(|d| d.contains("Allow multistreaming")), "{detail:?}");
        }
        other => panic!("Kick should have given up, stopped Studio LIVE, and ended the Show: {other:?}"),
    }
    assert!(
        !e.studio_stream_stats().await.unwrap()["active"].as_bool().unwrap_or(false),
        "Studio stops showing LIVE once all enabled destinations fail"
    );
    assert!(!e.is_show_active(), "Show ended cleanly when all destinations failed");
}

#[tokio::test]
async fn collection_switch_refused_while_live_recording_or_in_show() {
    let (e, studio, _dir) = setup().await;

    // 1. Streaming: Switch, New, Import, and import_obs all refused 409 studio_busy;
    //    List and Export remain allowed.
    e.studio_stream(true).await.unwrap();
    for act in [
        ShelfAct::Switch { id: "alt".into() },
        ShelfAct::New { name: "Fresh".into(), copy: false },
        ShelfAct::Import { name: "Imp".into(), doc: json!({ "sources": [] }) },
    ] {
        let err = e.studio_shelf("collections", act.clone()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "expected 409 studio_busy for {act:?}, got {err:?}"
        );
    }
    assert!(matches!(
        e.studio_import_obs("obs_one").await,
        Err(EngineError::Refused { status: 409, code: "studio_busy", .. })
    ));
    assert!(e.studio_shelf("collections", ShelfAct::List).await.is_ok());
    assert!(e.studio_shelf("collections", ShelfAct::Export { id: "default".into() }).await.is_ok());
    assert_eq!(studio.shelf_switches.load(Ordering::SeqCst), 0);
    e.studio_stream(false).await.unwrap();

    // 2. Recording: Switch refused 409 studio_busy.
    studio.recording.store(true, Ordering::SeqCst);
    assert!(matches!(
        e.studio_shelf("collections", ShelfAct::Switch { id: "alt".into() }).await,
        Err(EngineError::Refused { status: 409, code: "studio_busy", .. })
    ));
    studio.recording.store(false, Ordering::SeqCst);

    // 3. Active Show (Countdown, even without streaming): Switch refused 409 studio_busy.
    e.patch_show_config(
        json!({
            "starting_scene": "Starting", "live_scene": "Gameplay", "countdown_s": 30,
            "start_stream": false, "stop_stream": false, "discord": false,
            "alerts_on": false, "bot_on": false, "recap_message": null,
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();
    e.start_show(true).await.unwrap();
    let err = e.studio_shelf("collections", ShelfAct::Switch { id: "alt".into() }).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 409, code: "studio_busy", ref message, .. } if message.contains("Stop the show")),
        "{err:?}"
    );
    e.end_show().await.unwrap();

    // 4. Idle: Switch now succeeds.
    e.studio_shelf("collections", ShelfAct::Switch { id: "alt".into() }).await.unwrap();
    assert_eq!(studio.shelf_switches.load(Ordering::SeqCst), 1);
}
