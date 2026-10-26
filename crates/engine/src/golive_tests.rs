//! Go live everywhere in the engine: OBS setup and putting it back, a platform destination
//! that can't start saying why, and destinations started and stopped with the stream.

use std::sync::Arc;

use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, start as start_obs};
use serde_json::json;

use crate::golive::GoLiveConfig;
use crate::{Engine, PasswordChange};

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    e.start_golive(0).await;
    (e, dir)
}

#[tokio::test]
async fn obs_is_pointed_here_and_put_back_exactly() {
    let (e, _dir) = engine().await;
    let st = FakeObs::studio();
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let before = st.lock().unwrap().service.clone();
    let v = e.golive_point_obs().await.unwrap();
    assert!(v.connected && v.points_here && !v.streaming);
    assert_eq!(v.before.as_deref(), Some("Twitch"));
    let (server, key) = e.golive_ingest().await.unwrap();
    {
        let s = st.lock().unwrap();
        assert_eq!(s.service_kind, "rtmp_custom");
        assert_eq!(s.service["server"], server.as_str());
        assert_eq!(s.service["key"], key.as_str());
    }
    // Pointing again keeps the first saved setting (not our own).
    e.golive_point_obs().await.unwrap();
    // Never while OBS streams.
    st.lock().unwrap().streaming = true;
    let err = e.golive_restore_obs().await.unwrap_err().to_string();
    assert!(err.contains("streaming"), "{err}");
    st.lock().unwrap().streaming = false;
    let v = e.golive_restore_obs().await.unwrap();
    assert!(!v.points_here && v.before.is_none());
    let s = st.lock().unwrap();
    assert_eq!((s.service_kind.as_str(), &s.service), ("rtmp_common", &before));
    assert_eq!(s.service, json!({ "service": "Twitch", "server": "auto", "key": "live_123_OldKey", "bwtest": false }));
}

#[tokio::test]
async fn a_platform_that_is_not_connected_says_why_when_the_stream_starts() {
    use ferrocast_testkit::rtmp::Obs;
    let (e, _dir) = engine().await;
    let order: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "twitch")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    let mut why = None;
    for _ in 0..50 {
        let v = e.golive_view().await;
        if let Some(b) = v.destinations.iter().find(|d| d.id == "twitch").and_then(|d| d.blocked.clone()) {
            why = Some(b);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(why.as_deref(), Some("Connect Twitch in Settings, Accounts first."));
}

/// Upload protection pausing a destination: it stops, says why, isn't restarted by the next
/// start/stop pass, and comes back on "Resume now" or when protection is turned off.
#[tokio::test]
async fn a_paused_destination_stays_off_until_resumed() {
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Got, Obs, platform};
    use tokio::time::{Duration, timeout};
    // The next message matching `want`, skipping media in between.
    async fn until(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Got>, want: impl Fn(&Got) -> bool) -> Got {
        loop {
            let g = timeout(Duration::from_secs(5), rx.recv()).await.expect("in time").expect("open");
            if want(&g) {
                return g;
            }
        }
    }
    let publish = |g: &Got| matches!(g, Got::Publish(_));
    let closed = |g: &Got| *g == Got::Closed;
    let (e, _dir) = engine().await;
    let (port_a, mut got_a) = platform("key-a").await;
    let (port_b, mut got_b) = platform("key-b").await;
    let a = e
        .add_golive_destination("Main", &format!("rtmp://127.0.0.1:{port_a}/app"), "key-a", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let b = e
        .add_golive_destination(
            "Backup",
            &format!("rtmp://127.0.0.1:{port_b}/app"),
            "key-b",
            crate::golive::Canvas::Main,
        )
        .await
        .unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    assert_eq!(until(&mut got_a, publish).await, Got::Publish("key-a".into()));
    assert_eq!(until(&mut got_b, publish).await, Got::Publish("key-b".into()));
    let state = |v: &crate::golive::GoLiveView, id: &str| {
        v.destinations.iter().find(|d| d.id == id).and_then(|d| d.health.clone()).map(|h| h.state)
    };

    e.pause(b.id.clone(), "Main".into());
    until(&mut got_b, closed).await;
    let v = e.golive_view().await;
    match state(&v, &b.id) {
        Some(PushState::Paused { reason, .. }) => assert!(reason.contains("keep Main smooth"), "{reason}"),
        other => panic!("not paused: {other:?}"),
    }
    // A start/stop pass (any order change) leaves it paused.
    e.reconcile().await;
    assert!(matches!(state(&e.golive_view().await, &b.id), Some(PushState::Paused { .. })));
    assert!(timeout(Duration::from_millis(300), got_b.recv()).await.is_err(), "nothing reconnected");

    // "Resume now" starts it again; resuming twice says there's nothing to resume.
    e.golive_resume(&b.id).await.unwrap();
    assert_eq!(until(&mut got_b, publish).await, Got::Publish("key-b".into()));
    let err = e.golive_resume(&b.id).await.unwrap_err().to_string();
    assert!(err.contains("isn't paused"), "{err}");

    // Turning protection off brings back whatever is paused, and stays saved.
    e.pause(a.id.clone(), "Backup".into());
    until(&mut got_a, closed).await;
    e.set_golive_protect(false).await.unwrap();
    assert_eq!(until(&mut got_a, publish).await, Got::Publish("key-a".into()));
    assert!(!e.golive_view().await.protect);
    assert!(!e.golive_config().protect);
}

/// OBS's vertical canvas on the second key: only vertical destinations get it, and they stop
/// when it stops, while the normal stream's destinations follow the normal stream.
#[tokio::test]
async fn the_vertical_canvas_goes_only_to_vertical_destinations() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::{Got, Obs, next, platform};
    let (e, _dir) = engine().await;
    let (port_v, mut got_v) = platform("vert-key").await;
    let (port_m, mut got_m) = platform("main-key").await;
    e.add_golive_destination("Shorts box", &format!("rtmp://127.0.0.1:{port_v}/app"), "vert-key", Canvas::Vertical)
        .await
        .unwrap();
    e.add_golive_destination("Backup", &format!("rtmp://127.0.0.1:{port_m}/app"), "main-key", Canvas::Main)
        .await
        .unwrap();
    let (server, vkey) = e.golive_vertical_ingest().await.unwrap();
    let (_, key) = e.golive_ingest().await.unwrap();
    assert_ne!(vkey, key);
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let vertical = Obs::publish(port, "live", &vkey).await.unwrap();
    assert_eq!(next(&mut got_v).await, Got::Publish("vert-key".into()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), got_m.recv()).await.is_err(),
        "the normal destination waits for the normal stream"
    );
    let v = e.golive_view().await;
    assert!(v.vertical_live && !v.live);
    // The normal stream starts too: its destination joins; the vertical stops alone.
    let _main = Obs::publish(port, "live", &key).await.unwrap();
    assert_eq!(next(&mut got_m).await, Got::Publish("main-key".into()));
    drop(vertical);
    loop {
        let g = tokio::time::timeout(std::time::Duration::from_secs(5), got_v.recv()).await.unwrap().unwrap();
        if g == Got::Closed {
            break;
        }
    }
    assert!(e.golive_view().await.live);
}

/// Cut from the stream (A2.4): with ffmpeg here, the normal stream becomes an upright one that
/// reaches the vertical destinations; a bad layout is refused with the field named.
#[tokio::test]
async fn the_vertical_picture_is_cut_from_the_normal_stream() {
    use crate::golive::{Canvas, CutBox, CutConfig, VerticalSource, find_ffmpeg};
    use ferrocast_testkit::rtmp::{Got, next, platform};
    let Some(ff) = find_ffmpeg() else { return };
    let (e, _dir) = engine().await;
    let bad = CutConfig { game: CutBox { x: 0.6, y: 0.0, w: 0.6, h: 1.0 }, ..CutConfig::default() };
    assert!(e.set_vertical_cut(bad).await.unwrap_err().to_string().contains("game:"));
    e.set_vertical_cut(CutConfig { kbps: 2000, ..CutConfig::default() }).await.unwrap();
    e.set_vertical_source(VerticalSource::Cut).await.unwrap();
    let (port_v, mut got_v) = platform("cut-key").await;
    e.add_golive_destination("Shorts box", &format!("rtmp://127.0.0.1:{port_v}/app"), "cut-key", Canvas::Vertical)
        .await
        .unwrap();
    assert!(e.golive_view().await.cut.state.is_some(), "the cutter waits for the stream");
    let (server, key) = e.golive_ingest().await.unwrap();
    let mut obs = tokio::process::Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
        .args(["-t", "20", "-c:v", "libx264", "-preset", "ultrafast", "-g", "60", "-f", "flv"])
        .arg(format!("{server}/{key}"))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // ffmpeg looks at the input for a second before the vertical stream starts.
    for _ in 0..40 {
        if e.golive_view().await.vertical_live {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(next(&mut got_v).await, Got::Publish("cut-key".into()));
    let mut vertical = false;
    for _ in 0..60 {
        let v = e.golive_view().await;
        if v.vertical_live && v.vertical_stats.height == Some(1920) {
            vertical = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(vertical, "the cut stream is 1080x1920");
    // Back to OBS's canvas: the cutter stops and the vertical stream ends.
    e.set_vertical_source(VerticalSource::Canvas).await.unwrap();
    assert!(e.golive_view().await.cut.state.is_none());
    let _ = obs.kill().await;
}

/// The cut editor's picture: what's on air, at the size OBS streams.
#[tokio::test]
async fn the_cut_editor_gets_a_picture_of_what_is_on_air() {
    let (e, _dir) = engine().await;
    let port = start_obs(FakeObs::studio()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let v = e.golive_picture().await.unwrap();
    assert!(v["image"].as_str().unwrap().starts_with("data:image/jpg;base64,"));
    assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(1920), Some(1080)), "the streamed size");
}

/// Advanced mode (A2.5): settings are checked field by field and saved.
#[tokio::test]
async fn advanced_settings_are_checked_and_saved() {
    use crate::golive::{Advanced, Canvas, DestAdvanced};
    let (e, _dir) = engine().await;
    let c = e.add_golive_destination("Backup", "rtmp://127.0.0.1:1/app", "k", Canvas::Main).await.unwrap();
    let mut a = Advanced::default();
    a.rules.slow_pct = 99;
    assert!(e.set_golive_advanced(a.clone()).await.unwrap_err().to_string().contains("rules.slow_pct"));
    a.rules.slow_pct = 60;
    a.rules.first_to_pause = Some("nope".into());
    assert!(e.set_golive_advanced(a.clone()).await.unwrap_err().to_string().contains("first_to_pause"));
    a.rules.first_to_pause = Some("kick".into());
    a.per.insert(c.id.clone(), DestAdvanced { server: Some("rtmp://x.example.com/live".into()), ..Default::default() });
    assert!(e.set_golive_advanced(a.clone()).await.unwrap_err().to_string().contains("already has its address"));
    a.per.clear();
    a.per.insert("twitch".into(), DestAdvanced { server: Some("https://nope".into()), ..Default::default() });
    assert!(e.set_golive_advanced(a.clone()).await.unwrap_err().to_string().contains("per.twitch.server"));
    a.per.insert(
        "twitch".into(),
        DestAdvanced { server: Some("rtmps://live-fra.twitch.tv/app".into()), first_retry_s: 2, max_retry_s: 60 },
    );
    a.keep_free_pct = 30;
    e.set_golive_advanced(a.clone()).await.unwrap();
    let v = e.golive_view().await;
    assert_eq!(v.advanced, a);
    assert_eq!(e.golive_config().advanced.keep_free_pct, 30);
    let mut bad = a.clone();
    bad.per.insert("twitch".into(), DestAdvanced { first_retry_s: 40, ..Default::default() });
    assert!(e.set_golive_advanced(bad).await.unwrap_err().to_string().contains("first_retry_s"));
}

/// "Make a new key": the old key stops working, the new one works, and OBS is pointed at it.
#[tokio::test]
async fn a_new_key_replaces_the_old_one_and_obs_follows() {
    use ferrocast_testkit::rtmp::Obs;
    let (e, _dir) = engine().await;
    let st = FakeObs::studio();
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    e.golive_point_obs().await.unwrap();
    let (server, old) = e.golive_ingest().await.unwrap();
    e.golive_new_key().await.unwrap();
    let (server2, new) = e.golive_ingest().await.unwrap();
    assert_eq!(server, server2, "same address");
    assert_ne!(old, new);
    assert_eq!(st.lock().unwrap().service["key"], new.as_str(), "OBS follows");
    let p: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    assert!(Obs::publish(p, "live", &old).await.is_err(), "the old key is refused");
    let _ok = Obs::publish(p, "live", &new).await.unwrap();
    // Not while a stream comes in.
    let err = e.golive_new_key().await.unwrap_err().to_string();
    assert!(err.contains("right now"), "{err}");
}

/// Aitum Vertical: pointed at Ferrocast's vertical key with its own setting kept, never while
/// it streams, and put back exactly.
#[tokio::test]
async fn aitum_vertical_is_pointed_here_and_put_back() {
    use ferrocast_testkit::obs::FakeVertical;
    let (e, dir) = engine().await;
    let st = FakeObs::studio();
    st.lock().unwrap().vertical = Some(FakeVertical {
        streaming: false,
        server: "rtmps://a.rtmps.youtube.com/live2".into(),
        key: "yt-vertical".into(),
    });
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let ws = dir.path().join("obs-studio/plugin_config/obs-websocket/config.json");
    let vdir = dir.path().join("obs-studio/plugin_config/vertical-canvas");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::write(
        vdir.join("config.json"),
        serde_json::to_vec(&json!({ "canvas": [{ "width": 1080, "height": 1920, "stream_outputs": [
            { "name": "YouTube", "stream_server": "rtmps://a.rtmps.youtube.com/live2", "stream_key": "yt-vertical" }] }] }))
        .unwrap(),
    )
    .unwrap();
    e.set_obs_detect_paths(vec![ws]);
    let v = e.golive_view().await.obs.vertical.expect("Aitum Vertical is there");
    assert!(!v.points_here && v.before.is_none());
    // Not while it streams.
    st.lock().unwrap().vertical.as_mut().unwrap().streaming = true;
    assert!(e.golive_point_vertical().await.unwrap_err().to_string().contains("streaming"));
    st.lock().unwrap().vertical.as_mut().unwrap().streaming = false;
    let v = e.golive_point_vertical().await.unwrap().unwrap();
    assert!(v.points_here);
    assert_eq!(v.before.as_deref(), Some("a.rtmps.youtube.com"));
    let (server, vkey) = e.golive_vertical_ingest().await.unwrap();
    {
        let s = st.lock().unwrap();
        let fv = s.vertical.as_ref().unwrap();
        assert_eq!((fv.server.as_str(), fv.key.as_str()), (server.as_str(), vkey.as_str()));
    }
    // Pointing again keeps the first saved setting.
    e.golive_point_vertical().await.unwrap();
    let v = e.golive_restore_vertical().await.unwrap().unwrap();
    assert!(!v.points_here);
    {
        let s = st.lock().unwrap();
        let fv = s.vertical.as_ref().unwrap();
        assert_eq!((fv.server.as_str(), fv.key.as_str()), ("rtmps://a.rtmps.youtube.com/live2", "yt-vertical"));
    }
    assert!(e.golive_restore_vertical().await.unwrap_err().to_string().contains("no earlier"));
    // No stream output in Aitum Vertical yet: said, not pretended.
    std::fs::remove_file(vdir.join("config.json")).unwrap();
    assert!(e.golive_point_vertical().await.unwrap_err().to_string().contains("no stream output"));
    // Without the plugin: said plainly.
    st.lock().unwrap().vertical = None;
    assert!(e.golive_view().await.obs.vertical.is_none());
    assert!(e.golive_point_vertical().await.unwrap_err().to_string().contains("Aitum Vertical"));
}

/// Aitum Multistream in OBS: its outputs, where each goes (from its settings file, never the
/// key) and which are sending now; "on purpose" is saved.
#[tokio::test]
async fn aitum_multistream_outputs_are_found_and_named() {
    use crate::golive::aitum_platform;
    use ferrocast_golive::probe::Service;
    let (e, dir) = engine().await;
    let st = FakeObs::studio();
    st.lock().unwrap().aitum =
        Some(vec![("Twitch Output".into(), true), ("Kick".into(), false), ("Backup".into(), false)]);
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    let ws = dir.path().join("obs-studio/plugin_config/obs-websocket/config.json");
    let aitum = dir.path().join("obs-studio/plugin_config/aitum-multistream");
    std::fs::create_dir_all(&aitum).unwrap();
    std::fs::write(
        aitum.join("config.json"),
        serde_json::to_vec(&json!({ "profiles": [{ "name": "Untitled", "outputs": [
            { "name": "Twitch Output", "stream_server": "rtmps://aps30.contribute.live-video.net/app", "stream_key": "live_x" },
            { "name": "Kick", "stream_server": "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app/" },
            { "name": "Backup", "stream_server": "rtmp://backup.example.com/live" }
        ] }] }))
        .unwrap(),
    )
    .unwrap();
    e.set_obs_detect_paths(vec![ws]);
    let v = e.golive_view().await;
    let out = v.obs.aitum.expect("the plugin is there");
    let got: Vec<(&str, Option<Service>, bool)> = out.iter().map(|o| (o.name.as_str(), o.platform, o.live)).collect();
    assert_eq!(
        got,
        vec![
            ("Twitch Output", Some(Service::Twitch), true),
            ("Kick", Some(Service::Kick), false),
            ("Backup", None, false)
        ]
    );
    assert!(!v.aitum_ok);
    e.set_golive_aitum_ok(true).await.unwrap();
    assert!(e.golive_view().await.aitum_ok);
    // 1.0.8 doesn't answer: the names come from its settings file, live from OBS's outputs.
    st.lock().unwrap().aitum_old = true;
    let old = e.golive_view().await.obs.aitum.expect("found from the settings file");
    assert_eq!(
        old.iter().map(|o| (o.name.as_str(), o.live)).collect::<Vec<_>>(),
        [("Twitch Output", true), ("Kick", false), ("Backup", false)]
    );
    st.lock().unwrap().aitum_old = false;
    // Without the plugin: nothing to say.
    st.lock().unwrap().aitum = None;
    std::fs::remove_file(aitum.join("config.json")).unwrap();
    assert_eq!(e.golive_view().await.obs.aitum, None);
    // Names decide when the server doesn't.
    assert_eq!(aitum_platform(Some("a.rtmps.youtube.com"), "x"), Some(Service::Youtube));
    assert_eq!(aitum_platform(None, "My YouTube"), Some(Service::Youtube));
    assert_eq!(aitum_platform(Some("ingest.global-contribute.live-video.net"), "x"), Some(Service::Twitch));
    assert_eq!(aitum_platform(Some("restream.example.com"), "Backup"), None);
}

/// The relay builds the app's Kick sign-in link: its scope list must be the app's own.
#[test]
fn the_relays_kick_scopes_are_the_apps() {
    assert_eq!(ferrocast_relay::KICK_SCOPES, ferrocast_kick::SCOPES);
}

/// A Studio that's streaming and sends to direct servers (SRT, RIST, WebRTC) on request.
#[derive(Debug, Default)]
struct DirectStudio {
    started: std::sync::Mutex<Vec<(String, String, String)>>,
    stopped: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl crate::studio_host::StudioHost for DirectStudio {
    async fn status(&self) -> crate::studio_host::StudioStatus {
        Default::default()
    }
    async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
        Err("n/a".into())
    }
    async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
        Err("n/a".into())
    }
    async fn kinds(&self) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn options(&self, _kind: String) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn preview(&self, _rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    async fn defaults(&self, _kind: String) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn act(&self, _act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn settings(&self) -> crate::studio_host::StudioSettings {
        Default::default()
    }
    async fn set_settings(&self, _s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn stats(&self) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn record(&self, _on: bool) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn record_pause(&self, _on: bool) -> Result<(), String> {
        Ok(())
    }
    async fn monitoring(&self) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn stream_start(&self, _server: String, _key: String) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    async fn stream_stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stream_stats(&self) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    async fn obs_collections(&self) -> Result<serde_json::Value, String> {
        Ok(json!([]))
    }
    async fn import_obs(&self, _id: String) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn direct_start(&self, id: String, url: String, key: String) -> Result<(), String> {
        self.started.lock().unwrap().push((id, url, key));
        Ok(())
    }
    async fn direct_stop(&self, id: String) -> Result<(), String> {
        self.stopped.lock().unwrap().push(id);
        Ok(())
    }
    async fn direct_stats(&self, id: String) -> Result<serde_json::Value, String> {
        let on = self.started.lock().unwrap().iter().any(|s| s.0 == id) && !self.stopped.lock().unwrap().contains(&id);
        Ok(if on {
            json!({ "active": true, "reconnecting": false, "bytes": 250_000, "dropped": 0 })
        } else {
            json!(null)
        })
    }
}

#[tokio::test]
async fn srt_rist_and_webrtc_servers_go_straight_from_the_studio() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::Obs;
    let (e, _dir) = engine().await;
    // Their key may be empty; they send the main picture only; a whole address is needed.
    let srt =
        e.add_golive_destination("Studio SRT", "srt://192.0.2.7:9000?latency=200", "", Canvas::Main).await.unwrap();
    assert_eq!(srt.url.as_deref(), Some("srt://192.0.2.7:9000?latency=200"));
    let whip = e
        .add_golive_destination("Cam WHIP", "https://whip.example.com/live/whip", "token-1", Canvas::Main)
        .await
        .unwrap();
    assert!(e.add_golive_destination("V", "srt://192.0.2.7:9000", "", Canvas::Vertical).await.is_err());
    assert!(e.add_golive_destination("Bad", "srt:// spaced host", "", Canvas::Main).await.is_err());
    assert!(e.add_golive_destination("Who", "https://me:pw@whip.example.com/w", "", Canvas::Main).await.is_err());
    assert!(
        e.add_golive_destination("Plain", "http://whip.example.com/w", "t", Canvas::Main).await.is_err(),
        "WebRTC needs https off this PC"
    );
    assert_eq!(crate::golive::direct_kind("http://127.0.0.1:8889/w/whip"), Some("whip"));
    assert_eq!(crate::golive::direct_kind("http://localhost.evil.com/w"), None);
    assert!(
        e.add_golive_destination("Empty", "rtmp://192.0.2.7/app", "", Canvas::Main).await.is_err(),
        "RTMP keeps needing a key"
    );
    let order: Vec<(String, bool)> =
        e.golive_view().await.destinations.iter().map(|d| (d.id.clone(), d.id == srt.id || d.id == whip.id)).collect();
    e.set_golive_destinations(order).await.unwrap();
    let v = e.golive_view().await;
    let row = v.destinations.iter().find(|d| d.id == srt.id).unwrap();
    assert_eq!((row.direct, row.account.as_deref()), (Some("srt"), Some("192.0.2.7:9000")));

    // Live through OBS (not the Studio): they say why they can't start.
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let obs = Obs::publish(port, "live", &key).await.unwrap();
    let mut why = None;
    for _ in 0..100 {
        why = e.golive_view().await.destinations.iter().find(|d| d.id == srt.id).and_then(|d| d.blocked.clone());
        if why.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(why.unwrap().contains("Ferrocast picture"));
    drop(obs);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // With the Studio streaming, each is started there with its key and shows as live.
    let studio = Arc::new(DirectStudio::default());
    e.set_studio_host(studio.clone());
    let obs = Obs::publish(port, "live", &key).await.unwrap();
    let mut live = false;
    for _ in 0..200 {
        let v = e.golive_view().await;
        live = [&srt.id, &whip.id].iter().all(|id| {
            v.destinations
                .iter()
                .find(|d| &d.id == *id)
                .and_then(|d| d.health.as_ref())
                .is_some_and(|h| h.state == ferrocast_golive::PushState::Live)
        });
        if live {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(live, "both show as live");
    let started = studio.started.lock().unwrap().clone();
    assert!(started.contains(&(whip.id.clone(), "https://whip.example.com/live/whip".into(), "token-1".into())));
    assert!(started.iter().any(|s| s.0 == srt.id && s.2.is_empty()));
    // The stream ends: the Studio stops sending to them.
    drop(obs);
    for _ in 0..200 {
        if studio.stopped.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(studio.stopped.lock().unwrap().len(), 2);
}
