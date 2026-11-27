//! Go live everywhere in the engine: OBS setup and putting it back, a platform destination
//! that can't start saying why, and destinations started and stopped with the stream.

use std::sync::Arc;

use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, start as start_obs};
use serde_json::json;

use crate::golive::GoLiveConfig;
use crate::{Engine, EngineError, PasswordChange};

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    e.start_golive(0).await;
    (e, dir)
}

/// Puts a stand-in FFmpeg where `e` looks for its fetched one, so an x264 host's stream start
/// isn't refused with "ffmpeg_downloading" on a machine without FFmpeg (CI). The fake hosts never
/// run it.
fn pretend_ffmpeg(e: &Engine) {
    let data = e.golive_data_dir().expect("the test engine has a data folder");
    let p = crate::ffmpeg_get::tools_path(&data);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"").unwrap();
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

/// Counts the video each destination's server gets, while the stream keeps coming in.
fn count_video(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ferrocast_testkit::rtmp::Got>,
) -> Arc<std::sync::atomic::AtomicUsize> {
    use ferrocast_testkit::rtmp::Got;
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = n.clone();
    tokio::spawn(async move {
        while let Some(g) = rx.recv().await {
            if matches!(g, Got::Video(..)) {
                c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    n
}

/// Starts a stream into Go live that keeps sending (a keyframe every 4th packet).
async fn stream_in(e: &Engine) -> tokio::task::JoinHandle<()> {
    use ferrocast_testkit::rtmp::Obs;
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let mut obs = Obs::publish(port, "live", &key).await.unwrap();
    tokio::spawn(async move {
        let mut ts = 0u32;
        loop {
            let data: &'static [u8] =
                if (ts / 40).is_multiple_of(4) { &[0x17, 0x01, 0, 0, 0, 0xAA] } else { &[0x27, 0x01, 0, 0, 0, 0xBB] };
            obs.video(ts, data).await;
            ts += 40;
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
    })
}

async fn eventually_async<F: std::future::Future<Output = bool>>(mut f: impl FnMut() -> F) {
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("it never came true");
}

/// Adding a destination while live starts only that output, removing one stops only that one:
/// the other keeps receiving frames, and the session record says who was on and when.
#[tokio::test]
async fn a_destination_added_or_removed_mid_stream_leaves_the_others_sending() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::{Got, platform};
    use std::sync::atomic::Ordering;
    use tokio::time::{Duration, timeout};
    let (e, _dir) = engine().await;
    let (port_a, got_a) = platform("key-a").await;
    let (port_b, mut got_b) = platform("key-b").await;
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{port_a}/app"), "key-a", Canvas::Main).await.unwrap();
    let b =
        e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{port_b}/app"), "key-b", Canvas::Main).await.unwrap();
    let only_a = |b_on: bool| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), d.id == a.id || (b_on && d.id == b.id))).collect()
    };
    e.set_golive_destinations(only_a(false)).await.unwrap();
    let a_frames = count_video(got_a);
    let _feed = stream_in(&e).await;
    eventually_async(|| async { a_frames.load(Ordering::Relaxed) > 5 }).await;
    assert!(timeout(Duration::from_millis(300), got_b.recv()).await.is_err(), "B is off: nothing reaches it");

    // Add B while live: only B starts, and A's counter never stops.
    let before = a_frames.load(Ordering::Relaxed);
    e.set_golive_destinations(only_a(true)).await.unwrap();
    loop {
        let g = timeout(Duration::from_secs(8), got_b.recv()).await.expect("B starts").expect("open");
        if g == Got::Publish("key-b".into()) {
            break;
        }
    }
    eventually_async(|| async { a_frames.load(Ordering::Relaxed) > before + 5 }).await;

    // Remove B: B closes cleanly, A keeps going.
    e.set_golive_destinations(only_a(false)).await.unwrap();
    loop {
        let g = timeout(Duration::from_secs(8), got_b.recv()).await.expect("B closes").expect("open");
        if g == Got::Closed {
            break;
        }
    }
    let mid = a_frames.load(Ordering::Relaxed);
    eventually_async(|| async { a_frames.load(Ordering::Relaxed) > mid + 5 }).await;
    let v = e.golive_view().await;
    assert!(v.live, "the stream itself never stopped");
    // The session record: A on, B on, B off, in that order.
    let log = e.golive().dest_log.lock().unwrap().clone();
    let seq: Vec<(String, bool)> = log.iter().map(|ev| (ev.name.clone(), ev.on)).collect();
    assert_eq!(seq, [("A".to_string(), true), ("B".to_string(), true), ("B".to_string(), false)], "{seq:?}");
    assert!(log.windows(2).all(|w| w[0].at.0 <= w[1].at.0));
}

/// Two toggles at the same moment both land: the saves are one at a time and the outputs
/// follow the final list.
#[tokio::test]
async fn two_toggles_at_once_both_land() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let (pc, _gc) = platform("key-c").await;
    let mk = |n: &'static str, p: u16, k: &'static str| {
        let e = e.clone();
        async move { e.add_golive_destination(n, &format!("rtmp://127.0.0.1:{p}/app"), k, Canvas::Main).await.unwrap() }
    };
    let (a, b, c) = (mk("A", pa, "key-a").await, mk("B", pb, "key-b").await, mk("C", pc, "key-c").await);
    let list = |on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(list(&[a.id.as_str()])).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    // B is ticked while C is ticked: each call lists everything once, as the dashboard sends it.
    let (r1, r2) = tokio::join!(
        e.set_golive_destinations(list(&[a.id.as_str(), b.id.as_str()])),
        e.set_golive_destinations(list(&[a.id.as_str(), c.id.as_str()]))
    );
    r1.unwrap();
    r2.unwrap();
    let enabled: Vec<String> =
        e.golive_config().destinations.iter().filter(|d| d.enabled).map(|d| d.id.clone()).collect();
    // Whichever saved last decides; the outputs agree with it, and nothing is half-started.
    eventually_async(|| async {
        let running = e.golive_running();
        running == enabled.iter().cloned().collect()
    })
    .await;
}

/// Adding or removing a destination while live must never reshape the Studio's encoder: every
/// other output, and a "Same as stream" recording, share it. No cap is applied or lifted.
#[tokio::test]
async fn adding_or_removing_a_destination_while_live_never_reshapes_the_encoder() {
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(studio.clone());
    let order = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    push_main(&e, 20_000);
    let _keep = keep_streaming(e.golive().main_hub(), 20_000, 4_000);
    e.set_studio_live(true);
    {
        let group_ops = e.golive().group_ops.lock().await;
        e.sync_stream_cap_locked(&group_ops).await;
    }
    let before = studio.caps.lock().unwrap().len();
    // Kick (8,000 kbps limit) fits the 1,600 kbps stream: it joins without touching the encoder.
    e.set_golive_destinations(order(&e, &["youtube", "kick"])).await.unwrap();
    // And taking it away again doesn't lift or change anything either.
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    // Whatever else ticked in between, the cap the stream runs with never changed.
    let caps = studio.caps.lock().unwrap().clone();
    assert!(caps[before..].iter().all(|c| *c == caps[before - 1]), "the encoder's cap changed while live: {caps:?}");
}

/// A destination the running encode can't serve is refused with a plain reason and nothing
/// changes; one that already fits joins.
#[tokio::test]
async fn a_destination_the_running_encode_cannot_serve_is_refused() {
    use crate::golive::Canvas;
    let (e, _dir) = engine().await;
    let order = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    // A 14,000 kbps stream is on air to YouTube: Twitch takes 6,000 at most.
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    push_main(&e, 175_000);
    let _keep = keep_streaming(e.golive().main_hub(), 175_000, 4_000);
    let err =
        e.set_golive_destinations_checked(order(&e, &["youtube", "twitch"]), Default::default()).await.unwrap_err();
    match err {
        EngineError::Refused { code, message, .. } => {
            assert_eq!(code, "golive_needs_new_encode");
            assert!(message.contains("Twitch") && message.contains("6000"), "{message}");
        }
        other => panic!("not refused: {other:?}"),
    }
    assert!(!e.golive_config().destinations.iter().any(|d| d.id == "twitch" && d.enabled), "nothing changed");
    // A vertical destination while only the main stream runs.
    let v = e.add_golive_destination("Shorts", "rtmp://127.0.0.1:9/app", "k", Canvas::Vertical).await.unwrap();
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    let err = e
        .set_golive_destinations_checked(order(&e, &["youtube", v.id.as_str()]), Default::default())
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused { code: "golive_needs_new_encode", .. }), "{err:?}");
}

/// A compliant 6,000 kbps video stream with its sound (6,160 kbps measured) may take Twitch: the
/// sound and the packaging are not the video, and only the destination joining is judged.
#[tokio::test]
async fn a_compliant_stream_can_take_twitch_and_the_others_are_not_judged() {
    let (e, _dir) = engine().await;
    let order = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    push_main(&e, 77_000);
    let _keep = keep_streaming(e.golive().main_hub(), 77_000, 4_000);
    eventually_async(|| async { e.golive().main_hub().stats().kbps > 5_000 }).await;
    e.set_golive_destinations_checked(order(&e, &["youtube", "twitch"]), Default::default()).await.unwrap();
    // Kick as well: it only has to fit itself (8,000), not be judged against Twitch's 6,000.
    e.set_golive_destinations_checked(order(&e, &["youtube", "twitch", "kick"]), Default::default()).await.unwrap();
}

/// While a stream runs a server added by hand is added unticked (ticking it is the guarded
/// step), and removing the last one sending asks first.
#[tokio::test]
async fn a_server_added_while_live_waits_to_be_ticked_and_the_last_one_cannot_be_removed() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    assert!(a.enabled, "added with no stream running: ticked, as always");
    let list = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(list(&e, &[a.id.as_str()])).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "key-b", Canvas::Main).await.unwrap();
    assert!(!b.enabled, "added while live: not ticked");
    assert!(!e.golive_running().contains(&b.id));
    let err = e.remove_golive_destination(&a.id).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_last_destination", .. }), "{err:?}");
    e.remove_golive_destination(&b.id).await.unwrap();
}

/// Removing the last destination, and adding one the measured upload can't carry, ask first;
/// confirming goes ahead and the stream itself keeps going.
#[tokio::test]
async fn the_last_destination_and_a_too_big_upload_ask_first() {
    use crate::golive::{Canvas, DestinationConfirm};
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "key-b", Canvas::Main).await.unwrap();
    let list = |on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(list(&[a.id.as_str()])).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    // Upload: 2 Mbps measured; one 6,000 kbps output is already more than it carries.
    e.seed_upload_for_test(2_000);
    let err = e
        .set_golive_destinations_checked(list(&[a.id.as_str(), b.id.as_str()]), DestinationConfirm::default())
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_upload_exceeded", .. }), "{err:?}");
    assert!(!e.golive_running().contains(&b.id));
    e.set_golive_destinations_checked(
        list(&[a.id.as_str(), b.id.as_str()]),
        DestinationConfirm { upload: true, last: false },
    )
    .await
    .unwrap();
    eventually_async(|| async { e.golive_running().contains(&b.id) }).await;
    // The view's upload line already counts both.
    let v = e.golive_view().await;
    assert_eq!(v.upload.as_ref().map(|u| u.rows.len()), Some(2));
    // Taking both away: the first removal is fine, the last asks.
    e.set_golive_destinations_checked(list(&[a.id.as_str()]), DestinationConfirm::default()).await.unwrap();
    let err = e.set_golive_destinations_checked(list(&[]), DestinationConfirm::default()).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_last_destination", .. }), "{err:?}");
    assert!(e.golive_running().contains(&a.id), "still sending until confirmed");
    e.set_golive_destinations_checked(list(&[]), DestinationConfirm { upload: false, last: true }).await.unwrap();
    eventually_async(|| async { e.golive_running().is_empty() }).await;
    assert!(e.golive_view().await.live, "the stream itself keeps going with nobody to send to");
}

/// A destination that gave up shows as failed and Retry starts it again, alone.
#[tokio::test]
async fn a_failed_destination_is_retried_on_its_own() {
    use crate::golive::Canvas;
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};
    let (e, _dir) = engine().await;
    let (pa, mut ga) = platform("key-a").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    e.seed_failed_for_test(&a.id);
    let v = e.golive_view().await;
    let state = v.destinations.iter().find(|d| d.id == a.id).and_then(|d| d.health.clone()).map(|h| h.state);
    assert!(matches!(state, Some(PushState::Failed { .. })), "{state:?}");
    e.golive_resume(&a.id).await.unwrap();
    loop {
        let g = timeout(Duration::from_secs(8), ga.recv()).await.expect("it starts again").expect("open");
        if matches!(g, Got::Publish(_)) {
            break;
        }
    }
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
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
        DestAdvanced {
            server: Some("rtmps://live-fra.twitch.tv/app".into()),
            first_retry_s: 2,
            max_retry_s: 60,
            backup: None,
            simulcast: false,
        },
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
    settings: std::sync::Mutex<crate::studio_host::StudioSettings>,
    caps: std::sync::Mutex<Vec<Option<crate::studio_host::StreamCap>>>,
    /// Set: `set_stream_cap` refuses with this (the encoder is shared with a recording).
    cap_refused: std::sync::Mutex<Option<String>>,
    streaming: std::sync::atomic::AtomicBool,
    budget_level: std::sync::Mutex<Option<String>>,
    budget_light_checks: std::sync::atomic::AtomicUsize,
    light_starts: std::sync::atomic::AtomicUsize,
    light_stops: std::sync::atomic::AtomicUsize,
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
        self.settings.lock().unwrap().clone()
    }
    async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
        *self.settings.lock().unwrap() = s;
        Ok(None)
    }
    async fn set_stream_cap(&self, cap: Option<crate::studio_host::StreamCap>) -> Result<(), String> {
        self.caps.lock().unwrap().push(cap);
        self.cap_refused.lock().unwrap().clone().map_or(Ok(()), Err)
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
    async fn encode_budget(
        &self,
        _draft: Option<serde_json::Value>,
        _enhanced: bool,
        light: bool,
    ) -> Result<serde_json::Value, String> {
        if light {
            self.budget_light_checks.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        Ok(match self.budget_level.lock().unwrap().as_deref() {
            Some(lvl) => json!({ "level": lvl }),
            None => json!({ "budget_mpx": null, "level": "unknown" }),
        })
    }
    async fn light_stream_start(&self, _server: String, _key: String) -> Result<(), String> {
        self.light_starts.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
    async fn light_stream_stop(&self) -> Result<(), String> {
        self.light_stops.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
    async fn stream_start(
        &self,
        _server: String,
        _key: String,
        _tracks: Vec<(u32, u32, u32)>,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    async fn stream_stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stream_stats(&self) -> Result<serde_json::Value, String> {
        if self.streaming.load(std::sync::atomic::Ordering::Acquire) {
            Ok(json!({ "active": true }))
        } else {
            Ok(serde_json::Value::Null)
        }
    }
    async fn obs_collections(&self) -> Result<serde_json::Value, String> {
        Ok(json!([]))
    }
    async fn import_obs(&self, _id: String) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn direct_start(&self, id: String, url: String, key: String, _simulcast: bool) -> Result<(), String> {
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
    assert!(why.unwrap().contains("go out from Studio"));
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

#[tokio::test]
async fn test_connection_tries_the_server_and_says_how_it_went() {
    use crate::EngineError;
    use crate::golive::Canvas;
    let (e, _dir) = engine().await;
    // A plain RTMP server here: reached and shaken hands with, key saved but not needed.
    let (port, _got) = ferrocast_testkit::rtmp::platform("k").await;
    let d =
        e.add_golive_destination("Mine", &format!("rtmp://127.0.0.1:{port}/live"), "k", Canvas::Main).await.unwrap();
    let r = e.golive_test(&d.id).await.unwrap();
    assert!(r.ok && r.message.contains("reachable"), "{r:?}");
    // Nothing there: said, not an error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let r = e.golive_test_address(&format!("rtmp://127.0.0.1:{closed}/live"), "").await.unwrap();
    assert!(!r.ok && r.message.contains("Couldn't reach"), "{r:?}");
    // Addresses that can't work, and unknown destinations.
    let r = e.golive_test_address("srt://:9000", "").await.unwrap();
    assert!(!r.ok && r.message.contains("host"), "{r:?}");
    assert!(e.golive_test_address(" ", "").await.is_err());
    assert!(matches!(e.golive_test("nope").await, Err(EngineError::NotFound(_))));
}

/// A destination that can't send hands over to its backup, which waited on standby, and takes
/// back once it's sending again.
#[tokio::test]
async fn a_backup_sends_while_its_destination_cannot() {
    use crate::golive::{Canvas, DestAdvanced};
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Got, Obs, platform, platform_on};
    use tokio::time::{Duration, timeout};
    async fn until(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Got>, want: impl Fn(&Got) -> bool) -> Got {
        loop {
            let g = timeout(Duration::from_secs(8), rx.recv()).await.expect("in time").expect("open");
            if want(&g) {
                return g;
            }
        }
    }
    let publish = |g: &Got| matches!(g, Got::Publish(_));
    let (e, _dir) = engine().await;
    // The main one's server is down (a free port, nothing on it); the backup's is up.
    let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (port_b, mut got_b) = platform("key-b").await;
    let main =
        e.add_golive_destination("Main", &format!("rtmp://127.0.0.1:{down}/app"), "key-a", Canvas::Main).await.unwrap();
    let backup = e
        .add_golive_destination("Backup", &format!("rtmp://127.0.0.1:{port_b}/app"), "key-b", Canvas::Main)
        .await
        .unwrap();
    let mut adv = e.golive_config().advanced;
    adv.per.insert(main.id.clone(), DestAdvanced { backup: Some(backup.id.clone()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    // On standby: ticked or not, it waits.
    let v = e.golive_view().await;
    assert_eq!(
        v.destinations.iter().find(|d| d.id == backup.id).and_then(|d| d.standby_for.clone()),
        Some("Main".into())
    );

    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    assert!(timeout(Duration::from_millis(500), got_b.recv()).await.is_err(), "the backup waits at first");
    // The main one keeps failing: the backup takes over.
    assert_eq!(until(&mut got_b, publish).await, Got::Publish("key-b".into()));
    let v = e.golive_view().await;
    assert_eq!(
        v.destinations.iter().find(|d| d.id == backup.id).and_then(|d| d.backup_for.clone()),
        Some("Main".into())
    );

    // Its server comes back: once it's sent long enough, the backup stops.
    let (_, mut got_a) = platform_on(down, "key-a").await;
    assert_eq!(until(&mut got_a, publish).await, Got::Publish("key-a".into()));
    until(&mut got_b, |g| *g == Got::Closed).await;
    let v = e.golive_view().await;
    let main_state = v.destinations.iter().find(|d| d.id == main.id).and_then(|d| d.health.clone()).map(|h| h.state);
    assert_eq!(main_state, Some(PushState::Live));

    // A backup that can't be: itself, or one already backing up another.
    let mut adv = e.golive_config().advanced;
    adv.per.insert(main.id.clone(), DestAdvanced { backup: Some(main.id.clone()), ..Default::default() });
    assert!(e.set_golive_advanced(adv).await.is_err());
    // Removing the backup clears it from the one it backed up.
    e.remove_golive_destination(&backup.id).await.unwrap();
    assert_eq!(e.golive_config().advanced.per.get(&main.id).and_then(|a| a.backup.clone()), None);
}

#[tokio::test]
async fn simulcast_is_for_webrtc_servers_only() {
    use crate::golive::{Canvas, DestAdvanced};
    let (e, _dir) = engine().await;
    let rtmp = e.add_golive_destination("Mine", "rtmp://127.0.0.1:1935/app", "k", Canvas::Main).await.unwrap();
    let whip = e.add_golive_destination("Web", "https://whip.example/x", "", Canvas::Main).await.unwrap();
    let mut adv = e.golive_config().advanced;
    adv.per.insert(rtmp.id.clone(), DestAdvanced { simulcast: true, ..Default::default() });
    let err = e.set_golive_advanced(adv).await.unwrap_err().to_string();
    assert!(err.contains("simulcast"), "{err}");
    let mut adv = e.golive_config().advanced;
    adv.per.insert(whip.id.clone(), DestAdvanced { simulcast: true, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    let d = e.golive_config().destinations.into_iter().find(|d| d.id == whip.id).unwrap();
    assert!(e.simulcast(&d));
}

/// The pre-flight tries each ticked destination: one whose server isn't there fails, saying so.
#[tokio::test]
async fn the_preflight_tries_ticked_destinations() {
    use crate::golive::Canvas;
    let (e, _dir) = engine().await;
    let (up, _got) = ferrocast_testkit::rtmp::platform("k").await;
    let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ok = e.add_golive_destination("Up", &format!("rtmp://127.0.0.1:{up}/app"), "k", Canvas::Main).await.unwrap();
    let bad =
        e.add_golive_destination("Down", &format!("rtmp://127.0.0.1:{down}/app"), "k", Canvas::Main).await.unwrap();
    let checks = e.golive_preflight().await;
    let of = |id: &str| checks.iter().find(|c| c.id == format!("dest:{id}")).cloned().unwrap();
    assert_eq!(of(&ok.id).status, ferrocast_core::show::CheckStatus::Ok);
    let b = of(&bad.id);
    assert_eq!(b.status, ferrocast_core::show::CheckStatus::Fail);
    assert!(b.detail.contains("Couldn't reach"), "{b:?}");
}

/// Twitch Enhanced Broadcasting's switch: Twitch's main picture only, kept in the config; with
/// it on but Twitch not connected, the stream goes out as usual and says why.
#[tokio::test]
async fn enhanced_broadcasting_is_twitchs_and_says_why_when_it_cant() {
    let (e, _dir) = engine().await;
    let cfg = e.set_golive_enhanced("twitch", true).await.unwrap();
    assert!(cfg.destinations.iter().any(|d| d.id == "twitch" && d.enhanced));
    let err = e.set_golive_enhanced("youtube", true).await.unwrap_err().to_string();
    assert!(err.contains("Twitch"), "{err}");
    // Not ticked: nothing asked of Twitch, no note.
    assert_eq!(e.enhanced_tracks((1920, 1080), 60, false).await, (Vec::new(), None));
    let order: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "twitch")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let (tracks, note) = e.enhanced_tracks((1920, 1080), 60, false).await;
    assert!(tracks.is_empty());
    assert!(note.is_some_and(|n| n.contains("connect Twitch")), "a note says why");
    // Off again: kept off.
    let cfg = e.set_golive_enhanced("twitch", false).await.unwrap();
    assert!(cfg.destinations.iter().all(|d| !d.enhanced));
}

/// M10-01: Test connection, Pre-flight and Upload check never POST or PUT to YouTube (no silent
/// PUBLIC broadcasts), and a 403 on broadcast creation during real Go Live blocks the destination
/// with a clear one-line reason instead of streaming into nothing.
#[tokio::test]
async fn youtube_test_preflight_and_upload_check_make_zero_writes_and_403_blocks_golive() {
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Obs, platform};
    use std::sync::Mutex;

    let (rtmp_port, mut rtmp_got) = platform("yt-key").await;
    let writes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let writes_b = writes.clone();
    let writes_put = writes.clone();
    let writes_s = writes.clone();
    let writes_bind = writes.clone();
    let rtmp_url = format!("rtmp://127.0.0.1:{rtmp_port}/live2");
    let app = axum::Router::new()
        .route(
            "/api/liveStreams",
            axum::routing::get({
                let rtmp_url = rtmp_url.clone();
                move || {
                    let rtmp_url = rtmp_url.clone();
                    async move {
                        axum::Json(json!({ "items": [{
                            "id": "s1",
                            "snippet": { "title": "Default", "isDefaultStream": true },
                            "cdn": {
                                "resolution": "variable",
                                "frameRate": "variable",
                                "ingestionType": "rtmp",
                                "ingestionInfo": { "streamName": "yt-key", "ingestionAddress": rtmp_url }
                            }
                        }] }))
                    }
                }
            })
            .post(move || {
                let w = writes_s.clone();
                async move {
                    w.lock().unwrap().push("POST /liveStreams".into());
                    (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({ "error": { "message": "forbidden" } })))
                }
            }),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(|| async { axum::Json(json!({ "items": [] })) })
                .post(move || {
                    let w = writes_b.clone();
                    async move {
                        w.lock().unwrap().push("POST /liveBroadcasts".into());
                        (
                            axum::http::StatusCode::FORBIDDEN,
                            axum::Json(json!({
                                "error": {
                                    "message": "Live streaming is not enabled on this channel.",
                                    "errors": [{ "reason": "liveStreamingNotEnabled" }]
                                }
                            })),
                        )
                    }
                })
                .put(move || {
                    let w = writes_put.clone();
                    async move {
                        w.lock().unwrap().push("PUT /liveBroadcasts".into());
                        (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({ "error": { "message": "forbidden" } })))
                    }
                }),
        )
        .route(
            "/api/liveBroadcasts/bind",
            axum::routing::post(move || {
                let w = writes_bind.clone();
                async move {
                    w.lock().unwrap().push("POST /liveBroadcasts/bind".into());
                    (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({ "error": { "message": "forbidden" } })))
                }
            }),
        );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, app).await });

    let (e, _dir) = engine().await;
    let ep = ferrocast_youtube::Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: "http://127.0.0.1:9".into(),
    };
    let tokens = ferrocast_youtube::Tokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: crate::now().0 + 3_600_000,
    };
    let api = Arc::new(ferrocast_youtube::Api::new(
        ferrocast_youtube::http_client(),
        ep,
        ferrocast_youtube::AppCredentials { client_id: "c".into(), client_secret: "s".into() },
        tokens,
        Arc::new(|_| Ok(())),
    ));
    e.register_connector(Arc::new(ferrocast_youtube::YoutubeConnector { api, live: Arc::default() }));

    // Enable both YouTube main and vertical.
    let order: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == crate::golive::YOUTUBE_VERTICAL))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    // 1. Test connection (main + vertical): succeeds with ZERO writes to YouTube.
    let r_main = e.golive_test("youtube").await.unwrap();
    assert!(r_main.ok, "{r_main:?}");
    let r_vert = e.golive_test(crate::golive::YOUTUBE_VERTICAL).await.unwrap();
    assert!(r_vert.ok, "{r_vert:?}");
    assert!(writes.lock().unwrap().is_empty(), "golive_test wrote to YouTube: {:?}", writes.lock().unwrap());

    // 2. Pre-flight: passes for both with ZERO writes to YouTube.
    let checks = e.golive_preflight().await;
    assert_eq!(checks.iter().find(|c| c.id == "dest:youtube").unwrap().status, ferrocast_core::show::CheckStatus::Ok);
    assert_eq!(
        checks.iter().find(|c| c.id == format!("dest:{}", crate::golive::YOUTUBE_VERTICAL)).unwrap().status,
        ferrocast_core::show::CheckStatus::Ok
    );
    assert!(writes.lock().unwrap().is_empty(), "golive_preflight wrote to YouTube: {:?}", writes.lock().unwrap());

    // 3. Upload check (with an offline local Twitch connector so no external network is touched):
    // still makes ZERO writes to YouTube.
    #[derive(Debug)]
    struct LocalTwitch(String);
    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for LocalTwitch {
        fn platform(&self) -> Platform {
            Platform::Twitch
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            Default::default()
        }
        async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
        async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            Ok(ferrocast_connector::Ingest { url: self.0.clone(), key: "tw-k".into(), backup: None, note: None })
        }
    }
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    e.register_connector(Arc::new(LocalTwitch(format!("rtmp://127.0.0.1:{closed}/app"))));
    let order_with_tw: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == crate::golive::YOUTUBE_VERTICAL || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(order_with_tw).await.unwrap();
    // Fails fast on closed local Twitch port, after probing YouTube read-only.
    let _ = e.golive_upload_check().await;
    assert!(writes.lock().unwrap().is_empty(), "golive_upload_check wrote to YouTube: {:?}", writes.lock().unwrap());

    // 4. Actual Go Live: POST /liveBroadcasts returns 403 -> destination is blocked with a clear
    // one-line reason and never spawns a forwarder or publishes RTMP into nothing.
    let order_yt: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(order_yt).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    let mut blocked = None;
    for _ in 0..100 {
        blocked = e.golive_view().await.destinations.iter().find(|d| d.id == "youtube").and_then(|d| d.blocked.clone());
        if blocked.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let reason = blocked.expect("403 on make_broadcast must block go-live");
    assert!(!reason.contains('\n'), "reason must be one line: {reason:?}");
    assert!(reason.contains("Live streaming is not enabled"), "{reason}");
    let yt_view = e.golive_view().await.destinations.into_iter().find(|d| d.id == "youtube").unwrap();
    assert!(yt_view.health.is_none(), "blocked destination must not start a forwarder: {yt_view:?}");
    let published = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        while let Some(g) = rtmp_got.recv().await {
            if matches!(g, ferrocast_testkit::rtmp::Got::Publish(_)) {
                return Some(g);
            }
        }
        None
    })
    .await;
    assert!(published.is_err(), "must never publish RTMP when broadcast creation failed: {published:?}");
}

/// M9-04 / Contract C-1: a platform that keeps refusing (or Kick accepting then closing) gives up
/// into `PushState::Failed` with a clear reason, stops retrying, and when it was the only enabled
/// destination the Studio stops showing LIVE while keeping the failed state visible.
#[tokio::test]
async fn destination_give_up_marks_failed_and_stops_studio_live() {
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Obs, platform, platform_dropping};

    #[derive(Debug, Default)]
    struct LiveHost {
        streaming: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for LiveHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            crate::studio_host::StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.streaming.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::studio_host::StudioStatus { available: true, running: false, ..Default::default() })
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            Ok(json!({ "inputs": [] }))
        }
        async fn options(&self, _: String) -> Result<serde_json::Value, String> {
            Ok(json!([]))
        }
        async fn preview(&self, _: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _: String) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            Default::default()
        }
        async fn set_settings(&self, _: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            let on = self.streaming.load(std::sync::atomic::Ordering::SeqCst);
            Ok(
                json!({ "stream": if on { json!({ "active": true, "reconnecting": false, "error": null }) } else { serde_json::Value::Null } }),
            )
        }
        async fn record(&self, _: bool) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
        async fn record_pause(&self, _: bool) -> Result<(), String> {
            Ok(())
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            Ok(json!({ "devices": [] }))
        }
        async fn act(&self, _: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        async fn stream_start(
            &self,
            _: String,
            _: String,
            _: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.streaming.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "active": true, "error": null }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.streaming.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            let on = self.streaming.load(std::sync::atomic::Ordering::SeqCst);
            Ok(if on { json!({ "active": true, "error": null }) } else { serde_json::Value::Null })
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            Ok(json!([]))
        }
        async fn import_obs(&self, _: String) -> Result<serde_json::Value, String> {
            Ok(json!({}))
        }
    }

    let (e, _dir) = engine().await;
    let host = Arc::new(LiveHost::default());
    e.set_studio_host(host.clone());

    // 1. Kick alone closing a compliant stream after 40 ms: gives up without misdiagnosing
    //    multistreaming, and stops the Studio stream so the Studio stops showing LIVE.
    let (kport, _kgot) = platform_dropping("kick-k", std::time::Duration::from_millis(40)).await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{kport}/app"), "kick-k".into())));
    let order: Vec<(String, bool)> =
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let mut adv = e.golive_config().advanced;
    adv.per
        .insert("kick".into(), crate::golive::DestAdvanced { first_retry_s: 1, max_retry_s: 1, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();

    e.studio_stream(true).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();

    // Wait for Kick to hit PushState::Failed and Studio to stop showing LIVE.
    let mut failed_state = None;
    for _ in 0..100 {
        let v = e.golive_view().await;
        if let Some(PushState::Failed { .. }) =
            v.destinations.iter().find(|d| d.id == "kick").and_then(|d| d.health.as_ref()).map(|h| &h.state)
            && !host.streaming.load(std::sync::atomic::Ordering::SeqCst)
        {
            failed_state = v.destinations.into_iter().find(|d| d.id == "kick").and_then(|d| d.health).map(|h| h.state);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    match failed_state {
        Some(PushState::Failed { attempts, reason, detail, refused }) => {
            assert_eq!(attempts, ferrocast_golive::push::DEFAULT_MAX_ATTEMPTS);
            assert!(refused);
            assert_eq!(reason, "Kick closed the stream.");
            let d = detail.as_deref().unwrap_or("");
            assert!(!d.contains("Allow multistreaming"), "{d}");
            assert!(d.contains("stream key"), "{d}");
        }
        other => panic!("Kick should have reached Failed and stopped Studio stream: {other:?}"),
    }
    assert!(
        !e.studio_stream_stats().await.unwrap()["active"].as_bool().unwrap_or(false),
        "Studio must not show LIVE after every enabled destination failed"
    );
    drop(_obs);

    // 2. Two destinations (good custom + bad-key custom): bad-key reaches Failed while good
    //    stays Live and Studio stays LIVE.
    let (gport, _ggot) = platform("good-key").await;
    let (bport, _bgot) = platform("right-key").await;
    let good = e
        .add_golive_destination(
            "Good",
            &format!("rtmp://127.0.0.1:{gport}/app"),
            "good-key",
            crate::golive::Canvas::Main,
        )
        .await
        .unwrap();
    let bad = e
        .add_golive_destination(
            "Bad",
            &format!("rtmp://127.0.0.1:{bport}/app"),
            "wrong-key",
            crate::golive::Canvas::Main,
        )
        .await
        .unwrap();
    let mut order2: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == good.id || d.id == bad.id)).collect();
    order2.sort_by_key(|(id, _)| if *id == good.id { 0 } else { 1 });
    e.set_golive_destinations(order2).await.unwrap();
    let mut adv2 = e.golive_config().advanced;
    adv2.per
        .insert(bad.id.clone(), crate::golive::DestAdvanced { first_retry_s: 1, max_retry_s: 1, ..Default::default() });
    e.set_golive_advanced(adv2).await.unwrap();

    e.studio_stream(true).await.unwrap();
    let _obs2 = Obs::publish(port, "live", &key).await.unwrap();
    let mut saw_bad_failed = false;
    for _ in 0..80 {
        let v = e.golive_view().await;
        let bad_h = v.destinations.iter().find(|d| d.id == bad.id).and_then(|d| d.health.as_ref());
        let good_h = v.destinations.iter().find(|d| d.id == good.id).and_then(|d| d.health.as_ref());
        if matches!(bad_h.map(|h| &h.state), Some(PushState::Failed { refused: true, .. }))
            && matches!(good_h.map(|h| &h.state), Some(PushState::Live))
        {
            saw_bad_failed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saw_bad_failed, "bad destination should fail while good stays live");
    assert!(
        e.studio_stream_stats().await.unwrap()["active"].as_bool() == Some(true),
        "Studio must stay LIVE while at least one destination is still live"
    );
}

/// S11: the privacy the streamer chose reaches every YouTube broadcast Ferrocast makes. The Api
/// the engine builds reads the setting when it makes one, so a change applies at once.
#[tokio::test]
async fn the_youtube_privacy_setting_reaches_the_broadcasts_ferrocast_makes() {
    use std::sync::Mutex;

    // Default, and what it must accept and refuse.
    let (e, _dir) = engine().await;
    assert_eq!(e.settings().youtube_privacy, "public", "as before unless chosen");
    let bad = e.patch_settings(json!({ "youtube_privacy": "friends" }).as_object().unwrap()).await.unwrap_err();
    assert!(
        bad.to_string().contains("youtube_privacy: use one of public, unlisted, private."),
        "names the field and the choices: {bad}"
    );
    assert_eq!(e.settings().youtube_privacy, "public", "a refused value changes nothing");
    let bad = e.patch_settings(json!({ "youtube_privacy": 3 }).as_object().unwrap()).await.unwrap_err();
    assert!(bad.to_string().contains("youtube_privacy: must be text."), "{bad}");

    // A fake YouTube that only records the privacy of each broadcast made.
    let made: Arc<Mutex<Vec<String>>> = Arc::default();
    let rec = made.clone();
    let app = axum::Router::new()
        .route(
            "/api/liveStreams",
            axum::routing::get(|| async {
                axum::Json(json!({ "items": [{
                    "id": "s1", "snippet": { "title": "Default", "isDefaultStream": true },
                    "cdn": { "resolution": "variable", "frameRate": "variable", "ingestionType": "rtmp",
                             "ingestionInfo": { "streamName": "k", "ingestionAddress": "rtmp://127.0.0.1:9/live2" } }
                }] }))
            }),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(|| async { axum::Json(json!({ "items": [] })) }).post(
                move |axum::Json(b): axum::Json<serde_json::Value>| {
                    let rec = rec.clone();
                    async move {
                        rec.lock().unwrap().push(b["status"]["privacyStatus"].as_str().unwrap_or("").to_string());
                        axum::Json(json!({ "id": "made-1" }))
                    }
                },
            ),
        )
        .route("/api/liveBroadcasts/bind", axum::routing::post(|| async { axum::Json(json!({ "id": "made-1" })) }))
        .route(
            "/api/videos",
            axum::routing::get(|| async { axum::Json(json!({ "items": [{ "snippet": {} }] })) })
                .put(|| async { axum::Json(json!({})) }),
        );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, app).await });
    let dir = tempfile::tempdir().unwrap();
    let apps = crate::PlatformApps {
        youtube: Some(ferrocast_youtube::AppCredentials { client_id: "c".into(), client_secret: "s".into() }),
        youtube_endpoints: ferrocast_youtube::Endpoints {
            authorize: format!("{base}/auth"),
            token: format!("{base}/token"),
            api: format!("{base}/api"),
            grpc: "http://127.0.0.1:9".into(),
        },
        ..Default::default()
    };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    let tokens = ferrocast_youtube::Tokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: crate::now().0 + 3_600_000,
    };
    let info = ferrocast_core::stream_info::StreamInfo { title: Some("Ranked".into()), ..Default::default() };
    // Each Api is built first and the setting changed after: it is read when the broadcast is made.
    for chosen in ["public", "unlisted", "private"] {
        let api = e.youtube_api(tokens.clone()).unwrap();
        e.patch_settings(json!({ "youtube_privacy": chosen }).as_object().unwrap()).await.unwrap();
        api.update_broadcast_info(&info).await.unwrap();
    }
    assert_eq!(*made.lock().unwrap(), ["public", "unlisted", "private"]);

    // A saved value that is none of the three (the file was edited by hand; validation only runs
    // on a save from the dashboard) never publishes: unlisted.
    let mut odd = e.settings();
    odd.youtube_privacy = "friends".into();
    e.save_settings(odd).await.unwrap();
    e.youtube_api(tokens).unwrap().update_broadcast_info(&info).await.unwrap();
    assert_eq!(made.lock().unwrap().last().map(String::as_str), Some("unlisted"));
}

#[tokio::test]
async fn light_hub_destination_checks_light_hub_rules_and_eb_is_exempt() {
    let (e, _dir) = engine().await;
    let g = e.golive();
    let order: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "twitch" || d.id == "youtube"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();
    e.set_golive_light("twitch", true).await.unwrap();

    // Main hub runs at 12,000 kbps H.264 2 s (for YouTube); light hub runs at 3,000 kbps H.264 2 s.
    let push_stream = |hub: &ferrocast_golive::Hub, frame_bytes: usize| {
        hub.start();
        hub.video(0, (&[0x17u8, 0x00, 1, 2][..]).into());
        for i in 0..=40u32 {
            let ts = i * 100;
            let mut d = vec![if i % 20 == 0 { 0x17 } else { 0x27 }, 0x01];
            d.resize(frame_bytes, 0);
            hub.video(ts, d.into());
        }
    };
    let main_hub = g.main_hub();
    let light_hub = g.light_hub();
    push_stream(&main_hub, 150_000); // ~12,000 kbps
    push_stream(&light_hub, 37_500); // ~3,000 kbps
    let feeds = [keep_streaming(main_hub, 150_000, 4_000), keep_streaming(light_hub, 37_500, 4_000)];

    let v = e.golive_view().await;
    let twitch = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(twitch.rules.is_empty(), "light-hub Twitch checked against lhub (3 Mbps): {:?}", twitch.rules);
    assert!(twitch.issues.is_empty(), "{:?}", twitch.issues);

    // Move Twitch off light onto main hub with Enhanced Broadcasting enabled -> still exempt from 6,000 kbps rule.
    // Off light while live puts it on the full 12,000 kbps stream: refused, as a new destination is.
    let err = e.set_golive_light("twitch", false).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. }), "{err:?}");
    // (The rest is about what the view says, so the move is made in the saved settings directly.)
    let mut cfg = e.golive_config();
    let twitch_cfg = cfg.destinations.iter_mut().find(|d| d.id == "twitch").unwrap();
    twitch_cfg.light = false;
    twitch_cfg.quality = crate::golive::DestinationQuality::SameAsMain;
    e.save_golive(cfg).await.unwrap();
    e.set_golive_enhanced("twitch", true).await.unwrap();
    let v = e.golive_view().await;
    let twitch = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(twitch.rules.is_empty(), "Twitch EB must be exempt from legacy 6,000 limit: {:?}", twitch.rules);
    assert!(twitch.issues.is_empty(), "{:?}", twitch.issues);

    // Turning EB off exposes C-2 structured rule data against main hub's 12,000 kbps.
    e.set_golive_enhanced("twitch", false).await.unwrap();
    let v = e.golive_view().await;
    let twitch = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert_eq!(twitch.issues.len(), 1);
    assert_eq!(twitch.issues[0].code, ferrocast_golive::probe::RuleCode::Bitrate);
    assert_eq!(twitch.issues[0].fix_kbps, Some(5_500));
    assert!(!twitch.rules.is_empty());
    feeds.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test]
async fn studio_stream_caps_twitch_and_kick_exposes_c2_and_honors_ignore_limits() {
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        video_kbps: 14_000,
        vbr: true,
        keyframe_s: 4,
        video_encoder: Some("com.apple.videotoolbox.videoencoder.ave.hevc".into()),
        ..Default::default()
    };
    e.set_studio_host(studio.clone());
    // Signed in (a destination that can't start sets no limit).
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Kick)));

    let enable_only = |target: &str| {
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == target)).collect::<Vec<_>>()
    };

    // Twitch enabled: Studio gets 6,000 kbps H.264 2 s cap; DestinationView shows calm C-2 chip.
    e.set_golive_destinations(enable_only("twitch")).await.unwrap();
    let v = e.golive_view().await;
    let twitch = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    let cap_view = twitch.capped.as_ref().expect("twitch C-2 capped chip");
    assert_eq!(cap_view.kbps, 6_000);
    assert_eq!(cap_view.summary, "Capped at 6,000 kbps for Twitch");
    assert!(!cap_view.separate_encode);

    e.studio_stream(true).await.unwrap();
    let last_cap = studio.caps.lock().unwrap().last().copied().flatten();
    assert_eq!(
        last_cap,
        Some(crate::studio_host::StreamCap {
            max_kbps: Some(6_000),
            h264_only: true,
            keyframe_secs: 2,
            no_bframes: false
        })
    );

    // While Studio holds the live stream, small VT accounting above 6,000 produces zero nagging warnings.
    let hub = e.golive().main_hub();
    hub.start();
    hub.video(0, (&[0x17u8, 0x00, 1, 2][..]).into());
    for i in 0..=40u32 {
        let ts = i * 100;
        let mut d = vec![if i % 20 == 0 { 0x17 } else { 0x27 }, 0x01];
        d.resize(82_000, 0); // ~6,560 kbps FLV total before VT CappedCbr tightening
        hub.video(ts, d.into());
    }
    let v = e.golive_view().await;
    let twitch = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(twitch.capped.is_some());
    assert!(twitch.issues.is_empty(), "capped Studio stream must not nag mid-stream: {:?}", twitch.issues);

    // Switching to Kick while live leaves the running encode alone: the cap stays at Twitch's
    // 6,000 kbps (it is applied again when the next stream starts).
    e.set_golive_destinations(enable_only("kick")).await.unwrap();
    let last_cap = studio.caps.lock().unwrap().last().copied().flatten();
    assert_eq!(
        last_cap,
        Some(crate::studio_host::StreamCap {
            max_kbps: Some(6_000),
            h264_only: true,
            keyframe_secs: 2,
            no_bframes: false
        })
    );
    let v = e.golive_view().await;
    let kick = v.destinations.iter().find(|d| d.id == "kick").unwrap();
    let kick_cap = kick.capped.as_ref().expect("running encode stays capped at 6,000 kbps");
    assert_eq!(kick_cap.kbps, 6_000, "{kick_cap:?}");
    assert!(
        kick_cap.summary.contains("6,000 kbps") && !kick_cap.summary.contains("Kick"),
        "must describe the applied 6,000 kbps cap rather than Kick's 8,000: {:?}",
        kick_cap
    );

    // Power-user override (C-6) disables automatic cap and removes the chip.
    let mut adv = e.golive_config().advanced;
    adv.ignore_service_limits = true;
    e.set_golive_advanced(adv).await.unwrap();
    assert_eq!(studio.caps.lock().unwrap().last().copied().flatten(), None);
    let v = e.golive_view().await;
    let kick = v.destinations.iter().find(|d| d.id == "kick").unwrap();
    assert_eq!(kick.capped, None);
    e.studio_stream(false).await.unwrap();
}

// What `SignedIn` was last told about the keys of the vertical canvas.
// (Per thread: each test runs on its own, so tests that resolve in parallel don't overwrite it.)
thread_local! {
    static VERTICAL_KEYS_TOLD: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A platform that is signed in and gives a local address and a key (so its destination can start).
#[derive(Debug)]
struct SignedIn(ferrocast_core::event::Platform);
#[async_trait::async_trait]
impl ferrocast_connector::Connector for SignedIn {
    fn platform(&self) -> ferrocast_core::event::Platform {
        self.0
    }
    fn set_vertical_keys(&self, keys: Vec<String>) {
        VERTICAL_KEYS_TOLD.with(|t| *t.borrow_mut() = keys);
    }
    fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
        Default::default()
    }
    async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
    async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
        // YouTube gives a backup server beside its own; the others don't.
        let backup =
            (self.0 == ferrocast_core::event::Platform::Youtube).then(|| "rtmp://127.0.0.1:10/app".to_string());
        // YouTube says which key it used; the others don't.
        let note = (self.0 == ferrocast_core::event::Platform::Youtube).then(|| "Ferrocast · variable".to_string());
        Ok(ferrocast_connector::Ingest { url: "rtmp://127.0.0.1:9/app".into(), key: "k".into(), backup, note })
    }
}

fn twitch_studio_settings() -> crate::studio_host::StudioSettings {
    crate::studio_host::StudioSettings { video_kbps: 14_000, vbr: true, keyframe_s: 4, ..Default::default() }
}

/// After a burst (frames every 100 ms of stream, a keyframe every 2 s, up to `ts`), the same
/// stream keeps arriving on `hub` at its own pace, as a real one does, until aborted. The
/// measured rate reads 0 three seconds after the last packet, and on Windows each view takes
/// about 2 s (it dials OBS, and a refused connect takes that long there): a burst pushed once had
/// gone stale by the second view, and the rules it was meant to trip said nothing.
fn keep_streaming(hub: Arc<ferrocast_golive::Hub>, frame_bytes: usize, mut ts: u32) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            ts += 100;
            let mut d = vec![if (ts / 100).is_multiple_of(20) { 0x17 } else { 0x27 }, 0x01];
            d.resize(frame_bytes, 0);
            hub.video(ts, d.into());
        }
    })
}

/// A stream of `kbps` on the main hub, as the Studio's would arrive.
fn push_main(e: &crate::Engine, frame_bytes: usize) {
    let hub = e.golive().main_hub();
    hub.start();
    hub.video(0, (&[0x17u8, 0x00, 1, 2][..]).into());
    for i in 0..=40u32 {
        let mut d = vec![if i % 20 == 0 { 0x17 } else { 0x27 }, 0x01];
        d.resize(frame_bytes, 0);
        hub.video(i * 100, d.into());
    }
}

/// The cap is for the streams that go out: a destination that can't start (Twitch with nobody
/// signed in) sets no limit, so a YouTube stream was held to 6,000 kbps by a switch that did
/// nothing.
#[tokio::test]
async fn a_destination_that_cannot_start_sets_no_limit() {
    use ferrocast_core::event::Platform;
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(studio.clone());
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(Platform::Youtube)));
    let both: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "twitch" || d.id == "youtube"))
        .collect();
    e.set_golive_destinations(both).await.unwrap();
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000), "Twitch connected and ticked");
    // Runtime block (e.g. token revoked at Go Live): the stream is YouTube's alone.
    e.golive().blocked.lock().unwrap().insert("twitch".into(), "Twitch needs sign-in.".into());
    assert_eq!(e.studio_limit(), None);
    {
        let group_ops = e.golive().group_ops.lock().await;
        e.sync_stream_cap_locked(&group_ops).await;
    }
    assert_eq!(studio.caps.lock().unwrap().last().copied().flatten(), None, "lifted for the stream on air");
    // Unblocked: the limit is back.
    e.golive().blocked.lock().unwrap().clear();
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000));
}

#[tokio::test]
async fn disconnected_twitch_beside_youtube_does_not_cap_live_youtube_stream() {
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::platform;

    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        video_kbps: 12_000,
        vbr: true,
        keyframe_s: 4,
        bframes: true,
        ..Default::default()
    };
    e.set_studio_host(studio.clone());
    // YouTube is connected; Twitch is NOT connected. Both are ticked.
    let (yt_port, _yt_got) = platform("k").await;
    #[derive(Debug)]
    struct LocalYt(String);
    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for LocalYt {
        fn platform(&self) -> Platform {
            Platform::Youtube
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            Default::default()
        }
        async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
        async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            Ok(ferrocast_connector::Ingest { url: self.0.clone(), key: "k".into(), backup: None, note: None })
        }
    }
    e.register_connector(Arc::new(LocalYt(format!("rtmp://127.0.0.1:{yt_port}/app"))));

    let both: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "twitch" || d.id == "youtube"))
        .collect();
    e.set_golive_destinations(both).await.unwrap();

    // Before streaming starts, disconnected Twitch must NOT impose a 6,000 kbps cap.
    assert_eq!(e.studio_limit(), None, "disconnected Twitch must not set studio_limit");
    assert!(
        studio.caps.lock().unwrap().iter().all(|c| c.is_none()),
        "no cap must be pushed before stream start: {:?}",
        *studio.caps.lock().unwrap()
    );

    // Start Studio stream and bring up the real hub/ingest running state.
    e.studio_stream(true).await.unwrap();
    assert!(
        studio.caps.lock().unwrap().iter().all(|c| c.is_none()),
        "studio_stream(true) must not apply a 6,000 kbps cap: {:?}",
        *studio.caps.lock().unwrap()
    );
    let _feed = stream_in(&e).await;
    eventually_async(|| async {
        let v = e.golive_view().await;
        v.live
            && v.destinations.iter().any(|d| d.id == "twitch" && d.blocked.is_some())
            && e.golive_running().contains("youtube")
    })
    .await;

    // Both before and after reconcile(), no 6,000 kbps cap was ever applied,
    // YouTube has no capped badge, and Twitch reports blocked.
    assert_eq!(e.studio_limit(), None);
    assert_eq!(e.applied_cap(), None);
    assert!(
        studio.caps.lock().unwrap().iter().all(|c| c.is_none()),
        "live YouTube stream must never be capped at 6,000 kbps: {:?}",
        *studio.caps.lock().unwrap()
    );
    let v = e.golive_view().await;
    let yt = v.destinations.iter().find(|d| d.id == "youtube").unwrap();
    let tw = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert_eq!(yt.capped, None);
    assert!(tw.blocked.is_some());
    assert_eq!(tw.capped, None);

    _feed.abort();
    e.studio_stream(false).await.unwrap();
    eventually_async(|| async {
        let v = e.golive_view().await;
        !v.live && v.destinations.iter().all(|d| d.blocked.is_none())
    })
    .await;

    // Custom RTMP missing its stream key in secrets cannot start, and custom WHIP without a
    // valid host must not disable B-frames, whereas a valid WHIP does.
    let no_key = e
        .add_golive_destination("NoKey", "rtmp://127.0.0.1:9/app", "tmp-k", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let _ = e.inner.secrets.delete(&format!("{}{}", crate::golive::CUSTOM_KEY_PREFIX, no_key.id));
    let mut cfg = e.golive_config();
    cfg.destinations.push(crate::golive::DestinationConfig {
        id: "c_bad_whip".into(),
        kind: ferrocast_golive::probe::Service::Custom,
        name: Some("Bad WHIP".into()),
        url: Some("https://".into()),
        enabled: true,
        canvas: crate::golive::Canvas::Main,
        light: false,
        enhanced: false,
        brand: None,
        quality: crate::golive::DestinationQuality::SameAsMain,
    });
    e.save_golive(cfg).await.unwrap();
    let empty_blocked = std::collections::HashMap::new();
    assert!(!e.can_start_destination(&no_key, &empty_blocked), "custom RTMP without key cannot start");
    assert!(!e.studio_no_bframes(), "WHIP without a valid host must not disable B-frames");
    let _good_whip = e
        .add_golive_destination("Good WHIP", "https://whip.example.com/whip", "", crate::golive::Canvas::Main)
        .await
        .unwrap();
    assert!(e.studio_no_bframes(), "valid WHIP disables B-frames");

    // Once Twitch connects, studio_limit returns 6,000 kbps unless ignore_service_limits is set.
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000));
    let mut adv = e.golive_config().advanced;
    adv.ignore_service_limits = true;
    e.set_golive_advanced(adv).await.unwrap();
    assert_eq!(e.studio_limit(), None, "ignore_service_limits keeps studio_limit disabled");
}

#[tokio::test]
async fn live_view_reflects_applied_cap_after_midstream_destination_switch_and_standby() {
    use crate::golive::DestAdvanced;
    use ferrocast_core::event::Platform;

    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() =
        crate::studio_host::StudioSettings { video_kbps: 12_000, vbr: true, keyframe_s: 4, ..Default::default() };
    e.set_studio_host(studio.clone());
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(Platform::Kick)));

    let enable = |ids: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), ids.contains(&d.id.as_str()))).collect()
    };

    // 1. standby_for: Twitch configured as backup for enabled YouTube must NOT set studio_limit,
    //    even if Twitch itself is enabled, until Twitch actually carries the stream.
    let mut adv = e.golive_config().advanced;
    adv.per.insert("youtube".into(), DestAdvanced { backup: Some("twitch".into()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    e.set_golive_destinations(enable(&["youtube", "twitch"])).await.unwrap();
    let v = e.golive_view().await;
    let tw = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert_eq!(tw.standby_for.as_deref(), Some("YouTube"));
    assert_eq!(e.studio_limit(), None, "standby Twitch must not cap YouTube");
    assert_eq!(tw.capped, None, "standby Twitch must not display a cap badge");

    // Once primary YouTube fails over so Twitch carries the stream, Twitch counts in studio_limit.
    e.seed_backup_failing_for_test("youtube", true);
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000));
    e.seed_backup_failing_for_test("youtube", false);

    // Clear backup for the mid-stream badge checks below.
    let mut adv = e.golive_config().advanced;
    adv.per.clear();
    e.set_golive_advanced(adv).await.unwrap();

    // 2. Start live Studio stream at 12,000 kbps with Twitch + YouTube -> applied_cap = 6,000 kbps.
    e.set_golive_destinations(enable(&["twitch", "youtube"])).await.unwrap();
    e.studio_stream(true).await.unwrap();
    e.golive().main_hub().start();
    assert_eq!(e.applied_cap().and_then(|c| c.max_kbps), Some(6_000));

    // (a) Twitch + YouTube -> YouTube only mid-stream: applied_cap stays 6,000 kbps and
    // golive_view() still informs the user that the running stream stays held at 6,000 kbps.
    e.set_golive_destinations(enable(&["youtube"])).await.unwrap();
    assert_eq!(e.applied_cap().and_then(|c| c.max_kbps), Some(6_000));
    let v = e.golive_view().await;
    let yt = v.destinations.iter().find(|d| d.id == "youtube").unwrap();
    let yt_cap = yt.capped.as_ref().expect("YouTube must explain that running stream stays at 6,000 kbps");
    assert_eq!(yt_cap.kbps, 6_000);
    assert!(yt_cap.summary.contains("6,000 kbps"), "{yt_cap:?}");
    assert!(yt_cap.summary.contains("next stream") || yt_cap.detail.contains("next stream"), "{yt_cap:?}");

    // (b) Switch to Kick mid-stream: applied_cap stays 6,000 kbps and Kick must NOT claim 8,000 kbps.
    e.set_golive_destinations(enable(&["kick"])).await.unwrap();
    assert_eq!(e.applied_cap().and_then(|c| c.max_kbps), Some(6_000));
    let v = e.golive_view().await;
    let kick = v.destinations.iter().find(|d| d.id == "kick").unwrap();
    let kick_cap = kick.capped.as_ref().expect("Kick must show the actual 6,000 kbps running cap");
    assert_eq!(kick_cap.kbps, 6_000);
    assert!(!kick_cap.summary.contains("8,000") && !kick_cap.summary.contains("Kick"), "{kick_cap:?}");

    // (c) Auto-light (lighter_for_route: true) suppresses the main-stream cap badge.
    e.seed_light_route_for_test("kick");
    e.golive().light_hub().start();
    let v = e.golive_view().await;
    let kick = v.destinations.iter().find(|d| d.id == "kick").unwrap();
    assert!(kick.lighter_for_route, "Kick should report lighter_for_route");
    assert_eq!(kick.capped, None, "auto-light destination must not display a main-stream cap badge");

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn uncapped_primary_failover_to_incompatible_standby_twitch_is_blocked_without_pushing() {
    use crate::golive::DestAdvanced;
    use ferrocast_core::event::Platform;

    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() =
        crate::studio_host::StudioSettings { video_kbps: 12_000, vbr: true, keyframe_s: 4, ..Default::default() };
    e.set_studio_host(studio);
    e.register_connector(Arc::new(SignedIn(Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));

    let mut adv = e.golive_config().advanced;
    adv.per.insert("youtube".into(), DestAdvanced { backup: Some("twitch".into()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    let both: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(both).await.unwrap();

    // Start uncapped YouTube at 12,000 kbps while Twitch waits on standby.
    e.studio_stream(true).await.unwrap();
    e.golive().main_hub().start();
    e.reconcile().await;
    assert_eq!(e.applied_cap(), None);
    assert!(e.golive_running().contains("youtube"));
    assert!(!e.golive_running().contains("twitch"));

    // Live failover to Twitch must NOT start Twitch on the incompatible 12,000 kbps encode.
    e.seed_backup_failing_for_test("youtube", true);
    e.reconcile().await;
    assert!(
        !e.golive_running().contains("twitch"),
        "standby Twitch must not start on an incompatible 12,000 kbps encode"
    );
    let v = e.golive_view().await;
    let tw = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    let blocked = tw.blocked.as_deref().unwrap_or("");
    assert!(blocked.contains("6000") && blocked.contains("12000"), "unexpected block reason: {tw:?}");

    // When primary YouTube recovers, Twitch leaves want and its block clears.
    e.seed_backup_failing_for_test("youtube", false);
    e.reconcile().await;
    let v = e.golive_view().await;
    let tw = v.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert_eq!(tw.blocked, None);

    e.studio_stream(false).await.unwrap();
}

/// A cap the Studio couldn't put on the stream on air (its encoder is shared with a recording)
/// isn't claimed: no badge says the stream is held, and the bitrate warning isn't held back, so
/// the 7,011 kbps one comes back instead of silence. The codec rule still speaks when capped.
#[tokio::test]
async fn a_cap_that_could_not_be_applied_is_not_claimed_and_hides_nothing() {
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(studio.clone());
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    let only_twitch = || {
        GoLiveConfig::default().destinations.into_iter().map(|d| (d.id.clone(), d.id == "twitch")).collect::<Vec<_>>()
    };
    e.set_golive_destinations(only_twitch()).await.unwrap();
    // The Studio can't apply it to the stream it starts (its encoder is shared with a recording).
    *studio.cap_refused.lock().unwrap() = Some("the stream shares its encoder with the recording".into());
    e.studio_stream(true).await.unwrap();
    // 7,011 kbps arrives, and goes on arriving.
    push_main(&e, 90_000);
    let feed = keep_streaming(e.golive().main_hub(), 90_000, 4_000);
    let twitch = |v: &crate::golive::GoLiveView| v.destinations.iter().find(|d| d.id == "twitch").cloned().unwrap();
    let v = e.golive_view().await;
    assert_eq!(twitch(&v).capped, None, "not claimed");
    assert_eq!(twitch(&v).issues.len(), 1, "{:?}", twitch(&v).issues);
    assert_eq!(twitch(&v).issues[0].code, ferrocast_golive::probe::RuleCode::Bitrate);
    // Applied again (the recording stopped): held, as before.
    *studio.cap_refused.lock().unwrap() = None;
    {
        let group_ops = e.golive().group_ops.lock().await;
        e.sync_stream_cap_locked(&group_ops).await;
    }
    let v = e.golive_view().await;
    assert!(twitch(&v).capped.is_some() && twitch(&v).issues.is_empty());
    feed.abort();
    e.studio_stream(false).await.unwrap();
}

/// YouTube's backup server reaches the forwarder as a second target for the same key; a server the
/// person typed in themselves, and the platforms that have none, get none.
#[tokio::test]
async fn a_backup_server_reaches_the_destination_for_youtube_only() {
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    let cfg = e.golive_config();
    let dest = |id: &str| cfg.destinations.iter().find(|d| d.id == id).cloned().unwrap();
    let yt = e.resolve(&dest("youtube")).await.unwrap();
    assert_eq!(yt.backup.as_ref().map(|t| t.port), Some(10), "{:?}", yt.backup);
    assert_eq!(yt.target.port, 9);
    assert_eq!(yt.key, "k", "the same key");
    assert!(e.resolve(&dest("twitch")).await.unwrap().backup.is_none());
    // Its own address chosen for YouTube: no backup beside it.
    let mut adv = e.golive_config().advanced;
    adv.per.insert(
        "youtube".into(),
        crate::golive::DestAdvanced { server: Some("rtmp://127.0.0.1:11/app".into()), ..Default::default() },
    );
    e.set_golive_advanced(adv).await.unwrap();
    let own = e.resolve(&dest("youtube")).await.unwrap();
    assert!(own.backup.is_none() && own.target.port == 11);
}

/// Which key YouTube's stream went to ("Ferrocast · variable") shows in its details, once it has
/// started; Twitch, which says nothing, shows none. The key itself never does.
#[tokio::test]
async fn the_key_a_youtube_destination_sent_to_is_named_in_its_view() {
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    let cfg = e.golive_config();
    let dest = |id: &str| cfg.destinations.iter().find(|d| d.id == id).cloned().unwrap();
    let note =
        |v: &crate::golive::GoLiveView, id: &str| v.destinations.iter().find(|d| d.id == id).unwrap().key_note.clone();
    assert_eq!(note(&e.golive_view().await, "youtube"), None, "nothing before it starts");
    e.resolve(&dest("youtube")).await.unwrap();
    e.resolve(&dest("twitch")).await.unwrap();
    let v = e.golive_view().await;
    assert_eq!(note(&v, "youtube").as_deref(), Some("Ferrocast · variable"));
    assert_eq!(note(&v, "twitch"), None);
    assert!(!serde_json::to_string(&v).unwrap().contains("\"k\""), "never the key");
}

/// The keys of the custom servers on the vertical canvas (and only those) are told to the platform
/// before the normal stream's key is asked for, so a YouTube key set by hand for vertical video is
/// never given to the normal stream.
#[tokio::test]
async fn the_keys_set_for_the_vertical_canvas_are_told_to_the_platform_before_its_key_is_asked() {
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    e.add_golive_destination("Shorts box", "rtmp://127.0.0.1:9/app", "vert-key", crate::golive::Canvas::Vertical)
        .await
        .unwrap();
    e.add_golive_destination("Main box", "rtmp://127.0.0.1:9/app", "main-key", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let cfg = e.golive_config();
    let yt = cfg.destinations.iter().find(|d| d.id == "youtube").cloned().unwrap();
    e.resolve(&yt).await.unwrap();
    assert_eq!(VERTICAL_KEYS_TOLD.with(|t| t.borrow().clone()), vec!["vert-key".to_string()]);
}

/// The Studio's Advanced reconnect settings reach every Go live destination (they didn't: a
/// platform always got six tries a second apart): off means no retry, tries and the first wait are
/// the Studio's, a destination's own changed first wait wins, and OBS-only streaming keeps the defaults.
#[tokio::test]
async fn the_studios_reconnect_settings_govern_the_go_live_destinations() {
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    let cfg = e.golive_config();
    let yt = cfg.destinations.iter().find(|d| d.id == "youtube").cloned().unwrap();
    // No Studio: Go live's own defaults.
    let plain = e.resolve(&yt).await.unwrap().retry;
    assert_eq!(plain, ferrocast_golive::push::Retry::default());
    let studio = Arc::new(DirectStudio::default());
    e.set_studio_host(studio.clone());
    // Reconnecting on, 2 tries, every 3 s.
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        reconnect: true,
        reconnect_tries: 2,
        reconnect_every_s: 3,
        ..Default::default()
    };
    let r = e.resolve(&yt).await.unwrap().retry;
    assert_eq!((r.max_attempts, r.first.as_secs()), (2, 3));
    // A first wait set on the destination itself still wins.
    let mut adv = e.golive_config().advanced;
    adv.per.insert("youtube".into(), crate::golive::DestAdvanced { first_retry_s: 7, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    let r = e.resolve(&yt).await.unwrap().retry;
    assert_eq!((r.max_attempts, r.first.as_secs()), (2, 7));
    // Off: no retry at all.
    studio.settings.lock().unwrap().reconnect = false;
    assert!(e.resolve(&yt).await.unwrap().retry.is_never());
}

/// The IP version and the pinned connection of Go live's Advanced settings reach every RTMP
/// destination (and its backup server), and a nonsense connection is refused with the field's name.
#[tokio::test]
async fn the_ip_version_and_connection_chosen_reach_the_destinations() {
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    let cfg = e.golive_config();
    let yt = cfg.destinations.iter().find(|d| d.id == "youtube").cloned().unwrap();
    let plain = e.resolve(&yt).await.unwrap();
    assert_eq!((plain.target.family, plain.target.bind.clone()), (ferrocast_golive::IpFamily::Auto, None));
    let mut adv = e.golive_config().advanced;
    adv.ip_family = ferrocast_golive::IpFamily::V4;
    adv.bind = Some("en7".into());
    e.set_golive_advanced(adv).await.unwrap();
    let d = e.resolve(&yt).await.unwrap();
    assert_eq!((d.target.family, d.target.bind.as_deref()), (ferrocast_golive::IpFamily::V4, Some("en7")));
    let backup = d.backup.expect("YouTube's backup server");
    assert_eq!((backup.family, backup.bind.as_deref()), (ferrocast_golive::IpFamily::V4, Some("en7")));
    let mut bad = e.golive_config().advanced;
    bad.bind = Some("  ".into());
    assert!(e.set_golive_advanced(bad).await.unwrap_err().to_string().contains("bind"));
}

#[tokio::test]
async fn whip_disables_bframes_even_with_ignore_service_limits_and_shows_capped_notice() {
    use crate::golive::Canvas;
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings { bframes: true, ..Default::default() };
    e.set_studio_host(studio.clone());
    let whip = e.add_golive_destination("WebRTC", "https://whip.example.com/whip", "tok", Canvas::Main).await.unwrap();

    // Even when ignore_service_limits is true, WHIP cannot carry B-frames so Studio must be told.
    let mut adv = e.golive_config().advanced;
    adv.ignore_service_limits = true;
    e.set_golive_advanced(adv).await.unwrap();

    assert_eq!(
        studio.caps.lock().unwrap().last().copied().flatten(),
        Some(crate::studio_host::StreamCap { max_kbps: None, h264_only: false, keyframe_secs: 0, no_bframes: true })
    );
    let v = e.golive_view().await;
    let row = v.destinations.iter().find(|d| d.id == whip.id).unwrap();
    let cap = row.capped.as_ref().expect("WHIP with b_frames on must explain why B-frames are off");
    assert!(cap.summary.to_ascii_lowercase().contains("b-frames"), "{}", cap.summary);
}

#[test]
fn only_a_recent_push_that_some_platform_took_is_given_to_a_new_destination() {
    use ferrocast_core::stream_info::{HistoryEntry, PlatformResult, StreamInfo};
    let ok = |ok: bool| PlatformResult {
        platform: ferrocast_core::event::Platform::Twitch,
        ok,
        applied: vec![],
        skipped: vec![],
        error: None,
        waiting: false,
        category: None,
    };
    let entry = |at: i64, ok_: bool| HistoryEntry { info: StreamInfo::default(), at, results: vec![ok(ok_)] };
    let now = 100 * 3_600_000;
    assert!(crate::golive::recent_push(&entry(now - 3_600_000, true), now));
    assert!(!crate::golive::recent_push(&entry(now - 24 * 3_600_000, true), now), "a day-old title");
    assert!(!crate::golive::recent_push(&entry(now - 60_000, false), now), "a push nothing took");
}

/// The last destination is judged per live picture: a ticked vertical destination (whose picture
/// isn't on air) doesn't make removing the last main one free. And with nothing sending, a
/// platform saying the stream ended doesn't close the session record while the hub runs.
#[tokio::test]
async fn the_last_destination_is_per_live_canvas_and_the_record_stays_open() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let v = e.add_golive_destination("V", "rtmp://127.0.0.1:9/app", "key-v", Canvas::Vertical).await.unwrap();
    let list = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(list(&e, &[a.id.as_str(), v.id.as_str()])).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    let err = e.set_golive_destinations_checked(list(&e, &[v.id.as_str()]), Default::default()).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_last_destination", .. }), "{err:?}");
    e.set_golive_destinations_checked(
        list(&e, &[v.id.as_str()]),
        crate::golive::DestinationConfirm { upload: false, last: true },
    )
    .await
    .unwrap();
    // Nothing sends now, the stream goes on: the session record is told to close with the hub.
    assert!(e.golive_stream_running());
    assert!(e.golive_senders().is_empty());
}

/// Taking the one healthy destination away while a sibling has failed is the last one too: with
/// only the failed one left the stream would be ended for the failure.
#[tokio::test]
async fn unticking_the_only_healthy_destination_beside_a_failed_one_asks() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "key-b", Canvas::Main).await.unwrap();
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) && e.golive_running().contains(&b.id) }).await;
    e.seed_failed_for_test(&b.id);
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), d.id == b.id)).collect();
    let err = e.set_golive_destinations_checked(order, Default::default()).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_last_destination", .. }), "{err:?}");
}

/// A backup waiting for its ticked main destination isn't billed as a second sender, and a
/// problem a check found hours ago doesn't zero a destination on a stream that is running.
#[tokio::test]
async fn the_upload_estimate_bills_what_really_sends() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "key-b", Canvas::Main).await.unwrap();
    e.seed_upload_for_test(30_000);
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    let mut cfg = e.golive_config();
    cfg.advanced.per.entry(a.id.clone()).or_default().backup = Some(b.id.clone());
    for d in &mut cfg.destinations {
        d.enabled = d.id == a.id || d.id == b.id;
    }
    let check = e.upload_now_for_test(&cfg).expect("an estimate");
    assert_eq!(check.rows.len(), 1, "the standby backup is not billed beside its main: {:?}", check.rows);
}

/// The cap the running encode started with stays for its whole life: removing the platform that
/// set it (and any ordinary reconcile after) doesn't raise it, and a compatible destination
/// is judged against what is really on air (6,000 kbps), not the configured 14,000.
#[tokio::test]
async fn the_running_encode_keeps_the_cap_it_started_with() {
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(studio.clone());
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    let order = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    e.set_golive_destinations(order(&e, &["twitch", "youtube"])).await.unwrap();
    e.studio_stream(true).await.unwrap();
    let started = studio.caps.lock().unwrap().last().copied().flatten();
    assert_eq!(started.and_then(|c| c.max_kbps), Some(6_000));
    push_main(&e, 20_000);
    let _keep = keep_streaming(e.golive().main_hub(), 20_000, 4_000);
    // Twitch leaves: the encoder keeps its cap, through this and an ordinary reconcile.
    e.set_golive_destinations(order(&e, &["youtube"])).await.unwrap();
    e.reconcile().await;
    let caps = studio.caps.lock().unwrap().clone();
    assert!(
        caps.iter().rev().take(3).all(|c| c.and_then(|c| c.max_kbps) == Some(6_000)),
        "the cap was raised while live: {caps:?}"
    );
    // Kick (8,000) fits a stream that is really 6,000: not refused for the configured 14,000.
    e.set_golive_destinations(order(&e, &["youtube", "kick"])).await.unwrap();
}

/// Deleting the only sender of a live picture asks, even when another picture has a ticked
/// destination; deleting one of two senders is free and its stop goes in the session log.
#[tokio::test]
async fn deleting_a_sender_follows_the_canvas_rule_and_its_stop_is_logged() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;
    let (e, _dir) = engine().await;
    let (pa, _ga) = platform("key-a").await;
    let (pb, _gb) = platform("key-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "key-a", Canvas::Main).await.unwrap();
    let v = e.add_golive_destination("V", "rtmp://127.0.0.1:9/app", "key-v", Canvas::Vertical).await.unwrap();
    assert!(v.enabled);
    let _feed = stream_in(&e).await;
    eventually_async(|| async { e.golive_running().contains(&a.id) }).await;
    let err = e.remove_golive_destination(&a.id).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "golive_last_destination", .. }), "{err:?}");
    // A second main sender: now A can go, and its stop is in the log under its name.
    let list = |e: &Engine, on: &[&str]| -> Vec<(String, bool)> {
        e.golive_config().destinations.iter().map(|d| (d.id.clone(), on.contains(&d.id.as_str()))).collect()
    };
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "key-b", Canvas::Main).await.unwrap();
    e.set_golive_destinations_checked(
        list(&e, &[a.id.as_str(), b.id.as_str(), v.id.as_str()]),
        crate::golive::DestinationConfirm { upload: true, last: true },
    )
    .await
    .unwrap();
    eventually_async(|| async { e.golive_running().contains(&b.id) }).await;
    e.remove_golive_destination(&a.id).await.unwrap();
    let log = e.golive().dest_log.lock().unwrap().clone();
    assert!(log.iter().any(|ev| ev.id == a.id && ev.name == "A" && !ev.on), "{log:?}");
}

/// X, Instagram and Facebook have no key Ferrocast can create for the streamer (X and Instagram
/// only hand keys out in their own apps): the streamer pastes the server and key from there. The
/// key goes in the secret store like any custom server's, never in the saved settings.
#[tokio::test]
async fn a_pasted_key_for_x_instagram_or_facebook_is_a_branded_custom_server() {
    let (e, _dir) = engine().await;
    for (brand, label) in [("x", "X"), ("instagram", "Instagram"), ("facebook", "Facebook")] {
        let d = e
            .add_golive_destination_branded(
                label,
                "rtmps://live.example:443/rtmp/",
                "SECRET-KEY-VALUE",
                crate::golive::Canvas::Main,
                Some(brand),
            )
            .await
            .unwrap();
        assert_eq!(d.brand.as_deref(), Some(brand));
        assert_eq!(d.kind, ferrocast_golive::probe::Service::Custom);
        // The key is not in the saved destination.
        assert!(!serde_json::to_string(&d).unwrap().contains("SECRET-KEY-VALUE"));
        assert!(e.golive_config().destinations.iter().any(|x| x.id == d.id && x.brand.as_deref() == Some(brand)));
    }
    // A brand that isn't one of those is refused with the list.
    let err = e
        .add_golive_destination_branded("Z", "rtmp://127.0.0.1:9/app", "k", crate::golive::Canvas::Main, Some("tiktok"))
        .await
        .unwrap_err();
    assert!(matches!(&err, EngineError::Invalid(m) if m.contains("brand")), "{err:?}");
    // No brand stays a plain custom server.
    let plain =
        e.add_golive_destination("Mine", "rtmp://127.0.0.1:9/app", "k", crate::golive::Canvas::Main).await.unwrap();
    assert_eq!(plain.brand, None);
    // The dashboard's view carries the brand (so it can name and mark the server), and leaves the
    // field out when there is none.
    let view = serde_json::to_value(e.golive_view().await).unwrap();
    let rows = view["destinations"].as_array().unwrap();
    let brand_of = |id: &str| rows.iter().find(|r| r["id"] == id).map(|r| r.get("brand").cloned());
    assert_eq!(brand_of(&plain.id), Some(None), "no brand, no field");
    assert!(rows.iter().any(|r| r["brand"] == "facebook"), "{rows:?}");
    assert!(rows.iter().any(|r| r["brand"] == "instagram"));
}

#[tokio::test]
async fn v37_golive_config_fixture_loads_preserves_single_main_encode_and_ignores_unknown_quality_on_rollback() {
    use ferrocast_golive::probe::Service;

    use crate::golive::{Canvas, DestinationQuality, GROUP_LIGHT, GROUP_MAIN, PcBudget};

    // Exact Ferrocast 3.7 GoLiveConfig JSON shape (no `quality` field on any destination).
    let v37_json = json!({
        "destinations": [
            { "id": "twitch", "kind": "twitch", "enabled": true, "light": true },
            { "id": "youtube", "kind": "youtube", "enabled": true },
            { "id": "kick", "kind": "kick", "enabled": true },
            { "id": "youtube_vertical", "kind": "youtube", "enabled": false, "canvas": "vertical" },
            { "id": "c_legacy", "kind": "custom", "name": "BackupRTMP", "url": "rtmp://127.0.0.1:9/app", "enabled": false }
        ],
        "protect": true,
        "auto_quality": true,
        "aitum_ok": false
    });
    let mut cfg: GoLiveConfig = serde_json::from_value(v37_json).unwrap();
    assert!(
        cfg.destinations.iter().all(|d| d.quality == DestinationQuality::SameAsMain),
        "v3.7 destinations missing `quality` must default to SameAsMain"
    );

    // Default `SameAsMain` is omitted on serialization (zero wire change unless custom quality is chosen).
    let serialized = serde_json::to_value(&cfg).unwrap();
    let arr = serialized["destinations"].as_array().unwrap();
    assert!(arr.iter().all(|d| d.get("quality").is_none()), "default SameAsMain must omit `quality` field: {arr:?}");

    // Legacy `light: true` in v3.7 config resolves through planner bridge to `GROUP_LIGHT`.
    let (e, _dir) = engine().await;
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    };
    e.set_studio_host(studio);
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    });
    e.save_golive(cfg.clone()).await.unwrap();
    let plan_with_light = e.plan_golive_encodes(PcBudget::unknown());
    assert_eq!(plan_with_light.destinations["twitch"].group_id, GROUP_LIGHT);

    // Without `light: true`, a 3.7 1080p60 config preserves today's single `main` encode (6,000 kbps Studio cap),
    // collapses to one `main` group under a 1-session budget, and separates cleanly when budget permits.
    cfg.destinations.iter_mut().find(|d| d.id == "twitch").unwrap().light = false;
    e.save_golive(cfg).await.unwrap();
    let studio_cap = e.studio_limit();
    assert_eq!(studio_cap.map(|l| l.max_kbps), Some(6_000));
    e.studio_stream(true).await.unwrap();
    assert_eq!(e.applied_cap().and_then(|c| c.max_kbps), Some(6_000));
    e.studio_stream(false).await.unwrap();

    let tight =
        e.plan_golive_encodes(PcBudget::with_limits(crate::golive::BudgetSource::CpuDefault, Some(200.0), Some(1)));
    assert_eq!(tight.groups.len(), 1, "tight 1-session budget keeps 3.7 single main encode: {:?}", tight.groups);
    assert_eq!(tight.groups[0].id, GROUP_MAIN);
    assert_eq!(tight.groups[0].key.kbps, 6_000);
    assert!(tight.destinations.values().all(|d| d.group_id == GROUP_MAIN));

    let unconstrained = e.plan_golive_encodes(PcBudget::apple_silicon(2));
    assert_eq!(unconstrained.groups.len(), 2, "unconstrained 3.8 splits YouTube 12M and Twitch 6M");

    // Rollback test: a simulated v3.7 reader (without `quality` field) cleanly ignores `quality` in JSON.
    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code)]
    struct V37DestinationConfig {
        id: String,
        kind: Service,
        enabled: bool,
        #[serde(default)]
        canvas: Canvas,
        #[serde(default)]
        light: bool,
    }
    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code)]
    struct V37GoLiveConfig {
        destinations: Vec<V37DestinationConfig>,
    }
    e.set_golive_quality(
        "kick",
        DestinationQuality::Custom(crate::golive::CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 4_500,
            codec: Some(crate::golive::VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    let saved_json = serde_json::to_value(e.golive_config()).unwrap();
    assert!(
        saved_json["destinations"].as_array().unwrap().iter().any(|d| d["id"] == "kick" && d.get("quality").is_some()),
        "custom quality must serialize into golive config"
    );
    let rollback: V37GoLiveConfig = serde_json::from_value(saved_json).unwrap();
    assert_eq!(rollback.destinations.len(), e.golive_config().destinations.len());
}

#[tokio::test]
async fn legacy_light_bridge_and_per_destination_quality_persist_and_validate() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, PcBudget, VideoCodec};

    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    };
    e.set_studio_host(studio);
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Kick)));

    // Turning legacy `light` on populates `light: true` + `Custom(720p, <=3000 kbps, H264)`.
    let cfg = e.set_golive_light("twitch", true).await.unwrap();
    let twitch = cfg.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(twitch.light);
    assert_eq!(
        twitch.quality,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 60,
            kbps: 3_000,
            codec: Some(VideoCodec::H264),
        })
    );

    // Re-deserializing preserves the exact quality.
    let raw = serde_json::to_string(&cfg).unwrap();
    let reloaded: GoLiveConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(reloaded.destinations.iter().find(|d| d.id == "twitch").unwrap().quality, twitch.quality);

    // Turning `light` off clears both `light` and resets `quality` back to `SameAsMain`.
    let cfg = e.set_golive_light("twitch", false).await.unwrap();
    let twitch = cfg.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(!twitch.light);
    assert_eq!(twitch.quality, DestinationQuality::SameAsMain);

    // Explicit `set_golive_quality` writes Custom, clears `light`, and produces a dedicated encode group.
    e.set_golive_light("kick", true).await.unwrap();
    let cfg = e
        .set_golive_quality(
            "kick",
            DestinationQuality::Custom(CustomQuality {
                width: 1280,
                height: 720,
                fps: 30,
                kbps: 4_500,
                codec: Some(VideoCodec::H264),
            }),
        )
        .await
        .unwrap();
    let kick = cfg.destinations.iter().find(|d| d.id == "kick").unwrap();
    assert!(!kick.light);
    assert!(matches!(kick.quality, DestinationQuality::Custom(_)));
    let order: Vec<(String, bool)> = cfg.destinations.iter().map(|d| (d.id.clone(), d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let plan = e.plan_golive_encodes(PcBudget::unknown());
    assert_ne!(plan.destinations["kick"].group_id, crate::golive::GROUP_MAIN);

    // Validation rejections: resolution > canvas, odd dimensions, disallowed codec, direct/EB.
    let err = e
        .set_golive_quality(
            "youtube",
            DestinationQuality::Custom(CustomQuality { width: 2560, height: 1440, fps: 60, kbps: 9_000, codec: None }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    let err = e
        .set_golive_quality(
            "twitch",
            DestinationQuality::Custom(CustomQuality {
                width: 1280,
                height: 720,
                fps: 60,
                kbps: 9_000,
                codec: Some(VideoCodec::Hevc),
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Refused { status: 422, code: "golive_quality_locked" | "platform_limit_exceeded", .. }
        ),
        "{err:?}"
    );

    let whip = e.add_golive_destination("WHIP", "https://whip.example.com/whip", "", Canvas::Main).await.unwrap();
    let err = e
        .set_golive_quality(
            &whip.id,
            DestinationQuality::Custom(CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_000, codec: None }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }), "{err:?}");

    e.set_golive_enhanced("twitch", true).await.unwrap();
    let err = e
        .set_golive_quality(
            "twitch",
            DestinationQuality::Custom(CustomQuality { width: 1280, height: 720, fps: 60, kbps: 5_000, codec: None }),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }), "{err:?}");
}

#[tokio::test]
async fn dynamic_hub_registry_routes_custom_group_checks_budget_and_promotes_back_to_assigned_group() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::timeout;

    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_LIGHT, GROUP_MAIN, PcBudget, VideoCodec};

    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    };
    studio.streaming.store(true, Ordering::Release);
    e.set_studio_host(studio.clone());

    let (port_main, mut got_main) = platform("main-k").await;
    let (port_cust, mut got_cust) = platform("cust-k").await;
    let (port_low, _got_low) = platform("low-k").await;

    let d_main = e
        .add_golive_destination("MainOut", &format!("rtmp://127.0.0.1:{port_main}/app"), "main-k", Canvas::Main)
        .await
        .unwrap();
    let d_cust = e
        .add_golive_destination("CustOut", &format!("rtmp://127.0.0.1:{port_cust}/app"), "cust-k", Canvas::Main)
        .await
        .unwrap();
    let d_low = e
        .add_golive_destination("LowOut", &format!("rtmp://127.0.0.1:{port_low}/app"), "low-k", Canvas::Main)
        .await
        .unwrap();

    e.set_golive_quality(
        &d_cust.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 60,
            kbps: 4_500,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    // Already lighter than the 3,000 kbps fallback: never downgraded onto `light`.
    e.set_golive_quality(
        &d_low.id,
        DestinationQuality::Custom(CustomQuality {
            width: 852,
            height: 480,
            fps: 30,
            kbps: 1_500,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();

    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| {
            let on = d.id == d_main.id || d.id == d_cust.id || d.id == d_low.id;
            (d.id, on)
        })
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    let plan = e.plan_golive_encodes(PcBudget::unknown());
    let cust_group = plan.destinations[&d_cust.id].group_id.clone();
    let low_group = plan.destinations[&d_low.id].group_id.clone();
    assert_ne!(cust_group, GROUP_MAIN);
    assert_ne!(cust_group, low_group);

    let g = e.golive();
    let main_hub = g.main_hub();
    let cust_hub = g.ensure_hub(&cust_group, Canvas::Main);
    let low_hub = g.ensure_hub(&low_group, Canvas::Main);

    let start_hub = |hub: &ferrocast_golive::Hub, frame_bytes: usize| {
        hub.start();
        hub.video(0, (&[0x17u8, 0x00, 1, 2][..]).into());
        for i in 0..=40u32 {
            let mut d = vec![if i % 20 == 0 { 0x17 } else { 0x27 }, 0x01];
            d.resize(frame_bytes, 0);
            hub.video(i * 100, d.into());
        }
    };
    start_hub(&main_hub, 150_000); // ~12,000 kbps
    start_hub(&cust_hub, 56_250); // ~4,500 kbps
    start_hub(&low_hub, 18_750); // ~1,500 kbps
    let feeds = [
        keep_streaming(main_hub, 150_000, 4_000),
        keep_streaming(cust_hub, 56_250, 4_000),
        keep_streaming(low_hub, 18_750, 4_000),
    ];

    e.reconcile().await;
    assert_eq!(e.golive_running_group(&d_main.id).as_deref(), Some(GROUP_MAIN));
    assert_eq!(e.golive_running_group(&d_cust.id).as_deref(), Some(cust_group.as_str()));
    assert_eq!(e.golive_running_group(&d_low.id).as_deref(), Some(low_group.as_str()));

    let mut saw_cust_publish = false;
    for _ in 0..10 {
        if let Ok(Some(msg)) = timeout(Duration::from_secs(2), got_cust.recv()).await
            && matches!(msg, Got::Publish(ref k) if k == "cust-k")
        {
            saw_cust_publish = true;
            break;
        }
    }
    assert!(saw_cust_publish, "dynamic hub must feed its assigned custom destination");
    let mut saw_main_publish = false;
    for _ in 0..10 {
        if let Ok(Some(msg)) = timeout(Duration::from_secs(2), got_main.recv()).await
            && matches!(msg, Got::Publish(ref k) if k == "main-k")
        {
            saw_main_publish = true;
            break;
        }
    }
    assert!(saw_main_publish, "main hub must keep serving its destination");

    // 1. Healthy throughput on `d_cust` matching its own group's ~4,500 kbps (NOT main's 12,000 kbps)
    // must NOT trigger a downgrade to `light`.
    let _main_tx = e.replace_running_health_for_test(&d_main.id, GROUP_MAIN, 12_000, 60_000);
    let cust_tx = e.replace_running_health_for_test(&d_cust.id, &cust_group, 4_500, 60_000);
    let _low_tx = e.replace_running_health_for_test(&d_low.id, &low_group, 200, 60_000);
    let t0 = Instant::now();
    for s in 0..10u64 {
        e.quality_tick_at(t0 + Duration::from_secs(s)).await;
    }
    let updated_cfg = e.golive_config();
    assert!(!e.wants_light_now(updated_cfg.destinations.iter().find(|d| d.id == d_cust.id).unwrap()));
    assert!(
        !e.wants_light_now(updated_cfg.destinations.iter().find(|d| d.id == d_low.id).unwrap()),
        "already-1500 kbps destination must never be moved to 3000 kbps light fallback"
    );
    assert_eq!(studio.light_starts.load(Ordering::Acquire), 0);

    // 2. Slow route on `d_cust` while encode budget is `over` keeps `light` off cleanly.
    *studio.budget_level.lock().unwrap() = Some("over".into());
    cust_tx.send_modify(|h| h.kbps = 500);
    for s in 10..20u64 {
        e.quality_tick_at(t0 + Duration::from_secs(s)).await;
    }
    assert!(studio.budget_light_checks.load(Ordering::Acquire) >= 1);
    assert_eq!(studio.light_starts.load(Ordering::Acquire), 0, "over budget must block on-demand light start");
    assert!(!e.wants_light_now(e.golive_config().destinations.iter().find(|d| d.id == d_cust.id).unwrap()));

    // 3. With budget `ok`, slow `d_cust` moves to `light` and starts light stream.
    *studio.budget_level.lock().unwrap() = Some("ok".into());
    for s in 20..30u64 {
        e.quality_tick_at(t0 + Duration::from_secs(s)).await;
    }
    assert!(studio.light_starts.load(Ordering::Acquire) >= 1, "ok budget should allow light stream start");
    assert!(e.wants_light_now(e.golive_config().destinations.iter().find(|d| d.id == d_cust.id).unwrap()));
    let light_hub = g.light_hub();
    start_hub(&light_hub, 37_500);
    e.reconcile().await;
    assert_eq!(e.golive_running_group(&d_cust.id).as_deref(), Some(GROUP_LIGHT));

    // 4. After LIGHT_FIRST cooldown expires, `quality_tick` promotes `d_cust` back to its own `cust_group`, not `main`.
    e.quality_tick_at(t0 + Duration::from_secs(200)).await;
    light_hub.end();
    e.reconcile().await;
    assert!(!e.wants_light_now(e.golive_config().destinations.iter().find(|d| d.id == d_cust.id).unwrap()));
    assert_eq!(
        e.golive_running_group(&d_cust.id).as_deref(),
        Some(cust_group.as_str()),
        "recovered destination must return to its assigned encode group, not blindly main"
    );

    feeds.iter().for_each(tokio::task::JoinHandle::abort);
}

#[tokio::test]
async fn save_golive_and_view_with_backup_do_not_deadlock() {
    use crate::golive::{Canvas, DestAdvanced};
    let (e, _dir) = engine().await;
    let main = e.add_golive_destination("Primary", "rtmp://127.0.0.1:1935/app", "k1", Canvas::Main).await.unwrap();
    let backup = e.add_golive_destination("Fallback", "rtmp://127.0.0.1:1935/app", "k2", Canvas::Main).await.unwrap();
    let mut cfg = e.golive_config();
    cfg.advanced.per.insert(main.id.clone(), DestAdvanced { backup: Some(backup.id.clone()), ..Default::default() });
    e.save_golive(cfg).await.unwrap();
    let view = e.golive_view().await;
    assert_eq!(
        view.destinations.iter().find(|d| d.id == backup.id).and_then(|d| d.standby_for.as_deref()),
        Some("Primary")
    );
}

#[derive(Debug)]
struct LocalPlatform(ferrocast_core::event::Platform, String, String);

#[async_trait::async_trait]
impl ferrocast_connector::Connector for LocalPlatform {
    fn platform(&self) -> ferrocast_core::event::Platform {
        self.0
    }
    fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
        Default::default()
    }
    async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
    async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
        Ok(ferrocast_connector::Ingest { url: self.1.clone(), key: self.2.clone(), backup: None, note: None })
    }
}

#[derive(Debug)]
struct LocalKick(String, String);

#[async_trait::async_trait]
impl ferrocast_connector::Connector for LocalKick {
    fn platform(&self) -> ferrocast_core::event::Platform {
        ferrocast_core::event::Platform::Kick
    }
    fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
        Default::default()
    }
    async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
    async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
        Ok(ferrocast_connector::Ingest { url: self.0.clone(), key: self.1.clone(), backup: None, note: None })
    }
}

type StartedGroupEntry = (crate::studio_host::EncodeGroupSpec, Arc<ferrocast_golive::Hub>, tokio::task::JoinHandle<()>);

#[derive(Debug)]
struct MultiGroupHost {
    settings: std::sync::Mutex<crate::studio_host::StudioSettings>,
    budget_info: std::sync::Mutex<Option<crate::studio_host::HostBudgetInfo>>,
    started: std::sync::Mutex<std::collections::HashMap<String, StartedGroupEntry>>,
    start_counts: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    start_attempts: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    stopped_groups: std::sync::Mutex<Vec<String>>,
    aborted_groups: std::sync::Mutex<Vec<String>>,
    legacy_stream_starts: std::sync::atomic::AtomicUsize,
    blocked_idr_groups: std::sync::Mutex<std::collections::HashSet<String>>,
    header_only_groups: std::sync::Mutex<std::collections::HashSet<String>>,
    delay_idr_only_groups: std::sync::Mutex<std::collections::HashSet<String>>,
    fail_groups: std::sync::Mutex<std::collections::HashSet<String>>,
    idr_gate: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>,
    defer_reshape: std::sync::Mutex<Option<String>>,
}

impl Default for MultiGroupHost {
    fn default() -> Self {
        Self {
            settings: std::sync::Mutex::new(Default::default()),
            budget_info: std::sync::Mutex::new(None),
            started: std::sync::Mutex::new(Default::default()),
            start_counts: std::sync::Mutex::new(Default::default()),
            start_attempts: std::sync::Mutex::new(Default::default()),
            stopped_groups: std::sync::Mutex::new(Vec::new()),
            aborted_groups: std::sync::Mutex::new(Vec::new()),
            legacy_stream_starts: std::sync::atomic::AtomicUsize::new(0),
            blocked_idr_groups: std::sync::Mutex::new(Default::default()),
            header_only_groups: std::sync::Mutex::new(Default::default()),
            delay_idr_only_groups: std::sync::Mutex::new(Default::default()),
            fail_groups: std::sync::Mutex::new(Default::default()),
            idr_gate: std::sync::Mutex::new(Default::default()),
            defer_reshape: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl crate::studio_host::StudioHost for MultiGroupHost {
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
        self.settings.lock().unwrap().clone()
    }
    async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
        let deferred = self.defer_reshape.lock().unwrap().clone();
        let was = {
            let mut slot = self.settings.lock().unwrap();
            let old = slot.clone();
            *slot = s.clone();
            old
        };
        if deferred.is_some() {
            return Ok(deferred);
        }
        if let Some((spec, _, _)) = self.started.lock().unwrap().get_mut(crate::golive::GROUP_MAIN) {
            if spec.key.width == was.width && spec.key.height == was.height {
                spec.key.width = s.width;
                spec.key.height = s.height;
            }
            if spec.key.fps == was.fps || s.fps < spec.key.fps {
                spec.key.fps = s.fps;
                spec.key.fps_ntsc = s.fps_ntsc;
            }
            spec.key.kbps = if spec.key.kbps < was.video_kbps { spec.key.kbps.min(s.video_kbps) } else { s.video_kbps };
        }
        Ok(None)
    }
    async fn stats(&self) -> Result<serde_json::Value, String> {
        Ok(json!({}))
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
    async fn stream_start(
        &self,
        _server: String,
        _key: String,
        _t: Vec<(u32, u32, u32)>,
    ) -> Result<serde_json::Value, String> {
        self.legacy_stream_starts.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(json!({ "active": true }))
    }
    async fn stream_stop(&self) -> Result<(), String> {
        let keys: Vec<String> = self.started.lock().unwrap().keys().cloned().collect();
        for k in keys {
            let _ = self.stop_encode_group(&k).await;
        }
        Ok(())
    }
    async fn stream_stats(&self) -> Result<serde_json::Value, String> {
        let active = !self.started.lock().unwrap().is_empty();
        if active { Ok(json!({ "active": true, "dropped": 0 })) } else { Ok(serde_json::Value::Null) }
    }
    async fn obs_collections(&self) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    async fn import_obs(&self, _id: String) -> Result<serde_json::Value, String> {
        Err("n/a".into())
    }
    fn supports_encode_groups(&self) -> bool {
        true
    }
    async fn start_encode_group(
        &self,
        spec: crate::studio_host::EncodeGroupSpec,
        hub: Arc<ferrocast_golive::Hub>,
    ) -> Result<bool, String> {
        *self.start_attempts.lock().unwrap().entry(spec.id.clone()).or_insert(0) += 1;
        if spec.key.canvas == crate::golive::Canvas::Vertical
            && self.budget_info.lock().unwrap().as_ref().is_some_and(|b| !b.vertical_on)
        {
            return Ok(false);
        }
        if self.fail_groups.lock().unwrap().contains(&spec.id) {
            return Err(format!("forced failure starting {}", spec.id));
        }
        *self.start_counts.lock().unwrap().entry(spec.id.clone()).or_insert(0) += 1;
        if let Some((_, _, old_pump)) = self.started.lock().unwrap().remove(&spec.id) {
            old_pump.abort();
        }
        let delay_only = self.delay_idr_only_groups.lock().unwrap().contains(&spec.id);
        let header_only = self.header_only_groups.lock().unwrap().contains(&spec.id);
        if !delay_only {
            hub.start();
            let mut meta = ferrocast_golive::hub::StreamMetadata::new();
            meta.video_width = Some(spec.key.width);
            meta.video_height = Some(spec.key.height);
            meta.video_frame_rate = Some(spec.key.fps as f32);
            meta.video_bitrate_kbps = Some(spec.key.kbps);
            meta.video_codec_id = Some(if spec.key.codec == crate::golive::VideoCodec::Hevc { 12 } else { 7 });
            meta.audio_codec_id = Some(10);
            hub.metadata(meta);
            let hdr: Vec<u8> = if spec.key.codec == crate::golive::VideoCodec::Hevc {
                vec![0x1Cu8, 0x00, 0, 0, 0]
            } else {
                ferrocast_golive::flv::minimal_avc_header(spec.key.width, spec.key.height)
            };
            hub.video(0, hdr.into());
            hub.audio(0, (&[0xAFu8, 0x00, 0x12, 0x10][..]).into());
        }

        let emit_idr_now = !self.blocked_idr_groups.lock().unwrap().contains(&spec.id);
        let gate = self.idr_gate.lock().unwrap().get(&spec.id).cloned();
        let hub_clone = hub.clone();
        let pump = tokio::spawn(async move {
            if let Some(notify) = gate {
                notify.notified().await;
            }
            if !emit_idr_now || header_only {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
            if delay_only {
                hub_clone.note_encoded_idr();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
            let mut ts = 0u32;
            let mut frame = 0u32;
            loop {
                let tag = if frame.is_multiple_of(10) {
                    vec![0x17u8, 0x01, 0x00, 0x00, 0x01]
                } else {
                    vec![0x27u8, 0x01, 0x00, 0x00, 0x01]
                };
                hub_clone.video(ts, tag.into());
                hub_clone.audio(ts, (&[0xAFu8, 0x01, 0x22][..]).into());
                ts = ts.saturating_add(33);
                frame = frame.wrapping_add(1);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        tokio::task::yield_now().await;
        self.started.lock().unwrap().insert(spec.id.clone(), (spec, hub, pump));
        Ok(true)
    }
    async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
        self.started.lock().unwrap().get(id).map(|(spec, _, _)| spec.key.clone())
    }
    async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
        if let Some((_, hub, pump)) = self.started.lock().unwrap().remove(id) {
            pump.abort();
            hub.end();
        }
        self.stopped_groups.lock().unwrap().push(id.to_string());
        Ok(())
    }
    async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
        self.aborted_groups.lock().unwrap().push(id.to_string());
        self.stop_encode_group(id).await
    }
    async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
        let map: serde_json::Map<String, serde_json::Value> = self
            .started
            .lock()
            .unwrap()
            .keys()
            .map(|id| (id.clone(), json!({ "active": true, "dropped": 0 })))
            .collect();
        Ok((!map.is_empty()).then_some(serde_json::Value::Object(map)))
    }
    async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
        self.budget_info.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn multi_encode_studio_start_and_live_single_dest_switch_keeps_other_destinations_live() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    async fn wait_pub(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Got>, want: &str) {
        loop {
            match timeout(Duration::from_secs(3), rx.recv()).await.expect("rtmp timely").expect("open") {
                Got::Publish(k) if k == want => return,
                Got::Closed => panic!("unexpected close before publish for {want}"),
                _ => {}
            }
        }
    }

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port_yt, mut got_yt) = platform("yt-k").await;
    let (port_kick, mut got_kick) = platform("kick-k").await;
    let yt =
        e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-k", Canvas::Main).await.unwrap();
    let kick = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-k", Canvas::Main)
        .await
        .unwrap();

    e.studio_stream(true).await.unwrap();
    assert_eq!(host.legacy_stream_starts.load(std::sync::atomic::Ordering::Acquire), 0);
    assert!(host.started.lock().unwrap().contains_key(GROUP_MAIN));

    wait_pub(&mut got_yt, "yt-k").await;
    wait_pub(&mut got_kick, "kick-k").await;

    let initial_kick_group = e.golive_running_group(&kick.id).unwrap();
    let custom_720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 4_500, codec: Some(VideoCodec::H264) };
    let target_gid = {
        let mut input = e.golive_planner_input(e.cached_pc_budget());
        input.destinations.iter_mut().find(|d| d.id == kick.id).unwrap().quality =
            DestinationQuality::Custom(custom_720.clone());
        crate::golive::plan_encodes(&input).destinations[&kick.id].group_id.clone()
    };
    let notify = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(target_gid.clone(), notify.clone());

    let e_clone = e.clone();
    let kick_id = kick.id.clone();
    let sw_task =
        tokio::spawn(async move { e_clone.set_golive_quality(&kick_id, DestinationQuality::Custom(custom_720)).await });

    for _ in 0..100 {
        if host.started.lock().unwrap().contains_key(&target_gid) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(host.started.lock().unwrap().contains_key(&target_gid));
    assert_eq!(e.golive_running_group(&kick.id).as_deref(), Some(initial_kick_group.as_str()));
    assert_eq!(e.golive_running_group(&yt.id).as_deref(), Some(GROUP_MAIN));

    notify.notify_waiters();
    sw_task.await.unwrap().unwrap();
    assert_eq!(e.golive_running_group(&kick.id).as_deref(), Some(target_gid.as_str()));
    assert_eq!(e.golive_running_group(&yt.id).as_deref(), Some(GROUP_MAIN));

    while let Ok(msg) = got_yt.try_recv() {
        assert!(!matches!(msg, Got::Closed), "YouTube forwarder must never close while Kick switches");
    }
    let mut saw_kick_repub = false;
    for _ in 0..50 {
        match timeout(Duration::from_secs(2), got_kick.recv()).await.expect("kick event").expect("open") {
            Got::Publish(k) if k == "kick-k" => {
                saw_kick_repub = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_kick_repub, "Kick forwarder must switch onto new group hub");
    if initial_kick_group != GROUP_MAIN {
        assert!(host.stopped_groups.lock().unwrap().contains(&initial_kick_group));
    }

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn live_switch_timeout_keeps_old_forwarder_live_and_cleans_up_stalled_group() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    e.set_keyframe_wait_timeout_for_test(Duration::from_millis(25));
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port, mut got) = platform("kick-t").await;
    let d = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port}/app"), "kick-t", Canvas::Main)
        .await
        .unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got.recv()).await.unwrap().unwrap(), Got::Publish(_)) {
            break;
        }
    }

    let q = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q.clone());
    let stalled_gid = crate::golive::plan_encodes(&input).destinations[&d.id].group_id.clone();
    host.blocked_idr_groups.lock().unwrap().insert(stalled_gid.clone());

    let err = e.set_golive_quality(&d.id, DestinationQuality::Custom(q)).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { code: "golive_switch_timeout", .. }), "{err:?}");
    assert!(host.stopped_groups.lock().unwrap().contains(&stalled_gid));
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(GROUP_MAIN));
    assert_eq!(
        e.golive_config().destinations.iter().find(|x| x.id == d.id).unwrap().quality,
        DestinationQuality::SameAsMain
    );
    while let Ok(msg) = got.try_recv() {
        assert!(!matches!(msg, Got::Closed), "old forwarder must stay open on keyframe timeout");
    }

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn live_switch_concurrent_guard_and_stop_cancels_pending_switch() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    e.set_keyframe_wait_timeout_for_test(Duration::from_secs(5));
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port, mut got) = platform("kick-c").await;
    let d = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port}/app"), "kick-c", Canvas::Main)
        .await
        .unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got.recv()).await.unwrap().unwrap(), Got::Publish(_)) {
            break;
        }
    }

    let q1 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q2 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q1.clone());
    let pending_gid = crate::golive::plan_encodes(&input).destinations[&d.id].group_id.clone();

    let gate = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(pending_gid.clone(), gate);

    let e1 = e.clone();
    let d1 = d.id.clone();
    let t1 = tokio::spawn(async move { e1.set_golive_quality(&d1, DestinationQuality::Custom(q1)).await });

    for _ in 0..100 {
        if host.started.lock().unwrap().contains_key(&pending_gid) {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Second switch on the same destination must fail immediately with 409 Conflict.
    let err2 = e.set_golive_quality(&d.id, DestinationQuality::Custom(q2)).await.unwrap_err();
    assert!(matches!(err2, EngineError::Conflict(_)), "{err2:?}");

    // Stop stream cancels the pending switch cleanly before keyframe timeout elapses.
    e.studio_stream(false).await.unwrap();
    let res1 = timeout(Duration::from_secs(2), t1).await.expect("switch aborts cleanly").unwrap().unwrap_err();
    assert!(matches!(res1, EngineError::Refused { code: "golive_switch_cancelled", .. }), "{res1:?}");
    assert!(host.stopped_groups.lock().unwrap().contains(&pending_gid));
}

#[tokio::test]
async fn v37_legacy_host_falls_back_to_single_stream_start() {
    #[derive(Debug, Default)]
    struct FallbackLegacyHost {
        settings: std::sync::Mutex<crate::studio_host::StudioSettings>,
        stream_starts: std::sync::atomic::AtomicUsize,
        streaming: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for FallbackLegacyHost {
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
            self.settings.lock().unwrap().clone()
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            *self.settings.lock().unwrap() = s;
            Ok(None)
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            Ok(json!({}))
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
        async fn stream_start(
            &self,
            _server: String,
            _key: String,
            _t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.stream_starts.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.streaming.store(true, std::sync::atomic::Ordering::Release);
            Ok(json!({ "active": true }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.streaming.store(false, std::sync::atomic::Ordering::Release);
            Ok(())
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            if self.streaming.load(std::sync::atomic::Ordering::Acquire) {
                Ok(json!({ "active": true }))
            } else {
                Ok(serde_json::Value::Null)
            }
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            Err("n/a".into())
        }
        async fn import_obs(&self, _id: String) -> Result<serde_json::Value, String> {
            Err("n/a".into())
        }
    }

    // 1. Legacy host returning `Ok(false)` from `start_encode_group` uses single RTMP `stream_start`.
    let (e, _dir) = engine().await;
    let legacy = Arc::new(FallbackLegacyHost::default());
    *legacy.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    e.set_studio_host(legacy.clone());
    e.studio_stream(true).await.unwrap();
    assert!(!e.golive_uses_encode_groups(), "legacy host returning Ok(false) uses single RTMP ingest");
    assert_eq!(legacy.stream_starts.load(std::sync::atomic::Ordering::Acquire), 1);
    e.studio_stream(false).await.unwrap();

    // 2. 1-session x264 CPU host with YouTube + Kick (`SameAsMain`) collapses to 1 shared encode group.
    let (e2, _dir2) = engine().await;
    let cpu_host = Arc::new(MultiGroupHost::default());
    *cpu_host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        video_kbps: 12_000,
        ..Default::default()
    };
    *cpu_host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "obs_x264".into(),
        cpu_cores: 4,
        host_sig: "win|x264|cores=4".into(),
        ..Default::default()
    });
    e2.set_studio_host(cpu_host.clone());
    pretend_ffmpeg(&e2);
    let (p1, _g1) = ferrocast_testkit::rtmp::platform("k1").await;
    let (p2, _g2) = ferrocast_testkit::rtmp::platform("k2").await;
    let _yt = e2
        .add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p1}/app"), "k1", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let _kick = e2
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{p2}/app"), "k2", crate::golive::Canvas::Main)
        .await
        .unwrap();
    e2.studio_stream(true).await.unwrap();
    assert_eq!(cpu_host.started.lock().unwrap().len(), 1, "1-session host must run exactly 1 encode group");
    assert!(cpu_host.started.lock().unwrap().contains_key(crate::golive::GROUP_MAIN));
    e2.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f1_main_group_never_restarts_or_retires_midstream() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("yt-f1").await;
    let yt =
        e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p_yt}/app"), "yt-f1", Canvas::Main).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), got_yt.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    let custom_720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&yt.id, DestinationQuality::Custom(custom_720)).await.unwrap();

    // Even when zero destinations remain subscribed to `GROUP_MAIN`, `GROUP_MAIN` must stay live!
    assert!(
        !host.stopped_groups.lock().unwrap().contains(&GROUP_MAIN.to_string()),
        "GROUP_MAIN must never retire mid-stream"
    );
    assert!(host.started.lock().unwrap().contains_key(GROUP_MAIN));

    // Switching back to SameAsMain reuses the untouched `GROUP_MAIN` (start_count == 1).
    e.set_golive_quality(&yt.id, DestinationQuality::SameAsMain).await.unwrap();
    assert_eq!(host.start_counts.lock().unwrap().get(GROUP_MAIN).copied(), Some(1), "main encode must never restart");

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f2_failed_secondary_group_never_leaks_main_onto_kick() {
    use crate::golive::{Canvas, DestinationQuality};
    use ferrocast_testkit::rtmp::platform;

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let s = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 30,
        video_kbps: 12_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });

    let (p_yt, _got_yt) = platform("yt-f2").await;
    let (p_kick, mut got_kick) = platform("kick-f2").await;
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{p_kick}/app"), "kick-f2".into())));
    let yt =
        e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p_yt}/app"), "yt-f2", Canvas::Main).await.unwrap();
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == yt.id || d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();
    let kick_id = "kick".to_string();

    // Force Kick's auto-fitted 1080p group to fail on startup.
    let plan = e.plan_golive_encodes(
        host.budget_info.lock().unwrap().as_ref().map_or_else(crate::golive_plan::PcBudget::unknown, |i| {
            crate::golive_plan::PcBudget::from_host_info(i, None, true)
        }),
    );
    let kick_gid = plan.destinations[&kick_id].group_id.clone();
    assert_ne!(kick_gid, crate::golive::GROUP_MAIN);
    host.fail_groups.lock().unwrap().insert(kick_gid.clone());

    let _ = e.studio_stream(true).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_ne!(
        e.golive_running_group(&kick_id).as_deref(),
        Some(crate::golive::GROUP_MAIN),
        "failed secondary group must never fall back to GROUP_MAIN"
    );
    assert!(got_kick.try_recv().is_err(), "Kick must not receive unscaled main packets when its group failed");
    let _ = DestinationQuality::SameAsMain;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f4_unticked_dest_mid_switch_and_concurrent_shared_group_and_budget() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    e.set_keyframe_wait_timeout_for_test(Duration::from_secs(2));
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        nvidia_driver_major: Some(525),
        cpu_cores: 8,
        host_sig: "win|nv=525".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p1, mut g1) = platform("f4-1").await;
    let (p2, mut g2) = platform("f4-2").await;
    let d1 = e.add_golive_destination("D1", &format!("rtmp://127.0.0.1:{p1}/app"), "f4-1", Canvas::Main).await.unwrap();
    let d2 = e.add_golive_destination("D2", &format!("rtmp://127.0.0.1:{p2}/app"), "f4-2", Canvas::Main).await.unwrap();
    // Enable only d1 and d2 while preserving all existing destinations in `order`.
    let set_only = |e_ref: &Engine, enabled_ids: &[&str]| {
        e_ref
            .golive_config()
            .destinations
            .into_iter()
            .map(|d| {
                let on = enabled_ids.contains(&d.id.as_str());
                (d.id, on)
            })
            .collect::<Vec<_>>()
    };
    e.set_golive_destinations(set_only(&e, &[&d1.id, &d2.id])).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), g1.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}
    while !matches!(timeout(Duration::from_secs(2), g2.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    // 1. Untick d1 while it awaits IDR on target group -> switch cancels and removes new group.
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == d1.id).unwrap().quality = DestinationQuality::Custom(q720.clone());
    let gid_720 = crate::golive::plan_encodes(&input).destinations[&d1.id].group_id.clone();

    let gate = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(gid_720.clone(), gate.clone());
    let e1 = e.clone();
    let d1_id = d1.id.clone();
    let sw1 = tokio::spawn(async move { e1.set_golive_quality(&d1_id, DestinationQuality::Custom(q720)).await });
    for _ in 0..100 {
        if host.started.lock().unwrap().contains_key(&gid_720) {
            break;
        }
        tokio::task::yield_now().await;
    }
    e.set_golive_destinations(set_only(&e, &[&d2.id])).await.unwrap();
    gate.notify_waiters();
    let res = sw1.await.unwrap().unwrap_err();
    assert!(matches!(res, EngineError::Refused { code: "golive_switch_cancelled", .. }), "{res:?}");
    assert!(host.stopped_groups.lock().unwrap().contains(&gid_720));

    // Re-enable d1 onto main for concurrent budget test: NVENC <531 + auto_quality => eff_cap=2 (1 main + 1 spare).
    // Two concurrent switches to distinct new groups must serialize budget checks and refuse the 2nd!
    e.set_golive_destinations(set_only(&e, &[&d1.id, &d2.id])).await.unwrap();
    let q_a = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_000, codec: Some(VideoCodec::H264) };
    let q_b = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    let (r_a, r_b) = tokio::join!(
        e.set_golive_quality(&d1.id, DestinationQuality::Custom(q_a)),
        e.set_golive_quality(&d2.id, DestinationQuality::Custom(q_b)),
    );
    let ok_count = usize::from(r_a.is_ok()) + usize::from(r_b.is_ok());
    assert_eq!(ok_count, 1, "serialized budget must allow only 1 new group beyond main: {r_a:?} / {r_b:?}");
    assert!(host.started.lock().unwrap().len() <= 2);
    let _ = GROUP_MAIN;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f5_delay_switch_wakes_on_encoded_idr_and_clears_route_light() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_LIGHT, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        delay_s: 30,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p, mut got) = platform("f5").await;
    let d = e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p}/app"), "f5", Canvas::Main).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), got.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    // Place destination on route_light -> reconcile onto GROUP_LIGHT.
    e.mark_route_light_for_test(&d.id);
    e.reconcile().await;
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(GROUP_LIGHT));

    // Switch quality under delay (target Hub deferred `live.on = false`, emits only `note_encoded_idr`):
    // switch must succeed quickly AND leave GROUP_LIGHT behind even after subsequent `reconcile`.
    let q_6m = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 6_000, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    {
        let pd = input.destinations.iter_mut().find(|x| x.id == d.id).unwrap();
        pd.quality = DestinationQuality::Custom(q_6m.clone());
        pd.route_light = false;
    }
    let gid_6m = crate::golive::plan_encodes(&input).destinations[&d.id].group_id.clone();
    host.delay_idr_only_groups.lock().unwrap().insert(gid_6m.clone());

    timeout(Duration::from_secs(2), e.set_golive_quality(&d.id, DestinationQuality::Custom(q_6m)))
        .await
        .expect("delay_s encoded IDR wake")
        .expect("live switch under delay must succeed");
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(gid_6m.as_str()));

    e.reconcile().await;
    assert_eq!(
        e.golive_running_group(&d.id).as_deref(),
        Some(gid_6m.as_str()),
        "reconcile must not pull switched destination back onto GROUP_LIGHT"
    );
    let _ = GROUP_MAIN;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn n2_live_switch_drains_old_rtmp_session_before_opening_replacement() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform_exclusive};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port, mut rx) = platform_exclusive("n2-key").await;
    let d = e
        .add_golive_destination("Excl", &format!("rtmp://127.0.0.1:{port}/app"), "n2-key", Canvas::Main)
        .await
        .unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        match timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap() {
            Ok(Got::Publish(_)) => break,
            Ok(_) => {}
            Err(why) => panic!("unexpected error on first publish: {why}"),
        }
    }

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_000, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q720)).await.unwrap();

    // Expect old TCP session to close cleanly BEFORE replacement publish arrives with zero overlap!
    let mut saw_close = false;
    let mut saw_second_pub = false;
    for _ in 0..100 {
        match timeout(Duration::from_secs(2), rx.recv()).await.expect("rtmp activity").expect("open") {
            Err(why) => panic!("replacement publish overlapped old RTMP session: {why}"),
            Ok(Got::Closed) => saw_close = true,
            Ok(Got::Publish(k)) if k == "n2-key" => {
                assert!(saw_close, "replacement publish arrived before previous connection closed");
                saw_second_pub = true;
                break;
            }
            Ok(_) => {}
        }
    }
    assert!(saw_second_pub, "replacement forwarder must publish cleanly after drain");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn n3_vertical_only_keeps_studio_live_and_allows_live_switch() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_VERTICAL, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_on: true,
        vertical_size: Some((1080, 1920)),
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p_v, mut got_v) = platform("vert-n3").await;
    let d_v = e
        .add_golive_destination("Shorts", &format!("rtmp://127.0.0.1:{p_v}/app"), "vert-n3", Canvas::Vertical)
        .await
        .unwrap();
    let only_v: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == d_v.id)).collect();
    e.set_golive_destinations(only_v).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), got_v.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    assert!(!host.started.lock().unwrap().contains_key(crate::golive::GROUP_MAIN), "must be vertical-only");
    assert!(e.golive_stream_running(), "vertical-only stream must be reported live");
    assert_eq!(e.golive_running_group(&d_v.id).as_deref(), Some(GROUP_VERTICAL));

    let v_720 = CustomQuality { width: 720, height: 1280, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&d_v.id, DestinationQuality::Custom(v_720))
        .await
        .expect("live switch on vertical-only stream must not treat stream as OBS");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn n4_sequence_header_alone_does_not_satisfy_await_hub_idr() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    e.set_keyframe_wait_timeout_for_test(Duration::from_millis(40));
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p, mut got) = platform("n4").await;
    let d = e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p}/app"), "n4", Canvas::Main).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), got.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    let q_hdr = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q_hdr.clone());
    let gid = crate::golive::plan_encodes(&input).destinations[&d.id].group_id.clone();
    host.header_only_groups.lock().unwrap().insert(gid.clone());

    let err = e.set_golive_quality(&d.id, DestinationQuality::Custom(q_hdr)).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { code: "golive_switch_timeout", .. }), "{err:?}");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn n6_pause_ignores_destination_actively_switching_quality() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    // Hang guards only: slow CI runners need seconds to start and switch an encode.
    e.set_keyframe_wait_timeout_for_test(Duration::from_secs(30));
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p, mut got) = platform("n6").await;
    let d = e.add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{p}/app"), "n6", Canvas::Main).await.unwrap();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(30), got.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q720.clone());
    let gid = crate::golive::plan_encodes(&input).destinations[&d.id].group_id.clone();

    let gate = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(gid.clone(), gate.clone());
    let e_clone = e.clone();
    let d_id = d.id.clone();
    let sw = tokio::spawn(async move { e_clone.set_golive_quality(&d_id, DestinationQuality::Custom(q720)).await });
    // Wait for the switch to start its group (only a hang guard bounds it: a slow CI machine
    // needs more than a fixed number of yields).
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !host.started.lock().unwrap().contains_key(&gid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the switch never started its encode group");

    e.pause(d.id.clone(), "YT".into());
    assert!(
        e.golive_running_group(&d.id).is_some(),
        "pause during in-flight switch must not evict active forwarder mid-switch"
    );
    let view_mid = e.golive_view().await;
    assert!(!matches!(
        view_mid.destinations.iter().find(|x| x.id == d.id).unwrap().health.as_ref().map(|h| &h.state),
        Some(ferrocast_golive::PushState::Paused { .. })
    ));
    // A stored permit: the gated pump may not be waiting yet.
    gate.notify_one();
    sw.await.unwrap().unwrap();
    assert!(
        e.golive_running_group(&d.id).is_some(),
        "pause during in-flight switch must not pause/evict switching destination"
    );
    let view_after = e.golive_view().await;
    assert!(!matches!(
        view_after.destinations.iter().find(|x| x.id == d.id).unwrap().health.as_ref().map(|h| &h.state),
        Some(ferrocast_golive::PushState::Paused { .. })
    ));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f7_view_reports_live_stats_capped_and_golive_live_without_main_group() {
    use crate::golive::GROUP_MAIN;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let s = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 30,
        video_kbps: 12_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_kick, mut got_kick) = platform("kick-f7").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{p_kick}/app"), "kick-f7".into())));
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();

    // Offline view on a multi-encode host must expose the planned auto-fit chip (`separate_encode: true`).
    let offline_view = e.golive_view().await;
    let offline_kick = offline_view.destinations.iter().find(|d| d.id == "kick").unwrap();
    let offline_cap = offline_kick.capped.as_ref().expect("offline multi-encode Kick must expose auto-fit chip");
    assert!(offline_cap.separate_encode, "{offline_cap:?}");
    assert_eq!(offline_cap.kbps, 8_000);

    // Going live with Kick only starts Kick's 1080p group, NEVER GROUP_MAIN.
    let mut live_rx = e.golive_live();
    e.studio_stream(true).await.unwrap();
    while !matches!(timeout(Duration::from_secs(2), got_kick.recv()).await.unwrap().unwrap(), Got::Publish(_)) {}
    assert!(!host.started.lock().unwrap().contains_key(GROUP_MAIN));

    let live_view = e.golive_view().await;
    assert!(live_view.live, "GoLiveView.live must be true when Kick's non-main group is live");
    assert!(
        live_view.stats.kbps > 0 || live_view.stats.video_codec.is_some(),
        "GoLiveView.stats must report non-empty active stats without GROUP_MAIN: {:?}",
        live_view.stats
    );
    let live_kick = live_view.destinations.iter().find(|d| d.id == "kick").unwrap();
    let live_cap = live_kick.capped.as_ref().expect("live multi-encode Kick must keep its auto-fit capped badge");
    assert!(live_cap.separate_encode, "{live_cap:?}");
    assert_eq!(live_cap.kbps, 8_000);

    if !live_rx.borrow().on {
        timeout(Duration::from_secs(2), live_rx.changed()).await.unwrap().unwrap();
    }
    assert!(live_rx.borrow().on, "golive_live() must report live for non-main main-canvas groups");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f9_set_quality_enforces_non_twitch_limits_vertical_cut_and_codec_host() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VerticalSource, VideoCodec};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let s = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        nvidia_driver_major: Some(550),
        cpu_cores: 8,
        vertical_on: true,
        vertical_size: Some((1080, 1920)),
        host_sig: "win|nv=550".into(),
        ..Default::default()
    });
    e.set_studio_host(host);
    e.note_studio_settings(&s);
    let _ = e.refresh_pc_budget().await;

    // 1. Facebook branded destination enforces 4,000 kbps limit via platform_limit (not probe::ceiling None).
    let fb = e
        .add_golive_destination_branded(
            "Facebook",
            "rtmps://live-api-s.facebook.com:443/rtmp/",
            "fb-key",
            Canvas::Main,
            Some("facebook"),
        )
        .await
        .unwrap();
    let too_high_fb = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 8_000, codec: Some(VideoCodec::H264) };
    let err = e.set_golive_quality(&fb.id, DestinationQuality::Custom(too_high_fb)).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "platform_limit_exceeded", .. }),
        "Facebook >4,000 kbps must return 422 platform_limit_exceeded: {err:?}"
    );

    // 2. Vertical custom quality must be rejected when VerticalSource::Cut is active.
    e.set_vertical_source(VerticalSource::Cut).await.unwrap();
    let v_ok = CustomQuality { width: 720, height: 1280, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let err = e.set_golive_quality("youtube_vertical", DestinationQuality::Custom(v_ok)).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }),
        "vertical custom under Cut must return 422 lock: {err:?}"
    );

    // 3. Windows host (H.264 ffenc only) rejects Hevc and Av1 custom codecs with 422 lock.
    let q_hevc = CustomQuality { width: 1280, height: 720, fps: 60, kbps: 4_500, codec: Some(VideoCodec::Hevc) };
    assert!(matches!(
        e.set_golive_quality("youtube", DestinationQuality::Custom(q_hevc)).await.unwrap_err(),
        EngineError::Refused { status: 422, code: "golive_quality_locked", .. }
    ));
    let q_av1 = CustomQuality { width: 1280, height: 720, fps: 60, kbps: 4_500, codec: Some(VideoCodec::Av1) };
    assert!(matches!(
        e.set_golive_quality("youtube", DestinationQuality::Custom(q_av1)).await.unwrap_err(),
        EngineError::Refused { status: 422, code: "golive_quality_locked", .. }
    ));
}

#[tokio::test]
async fn f10_planner_includes_failover_backup_and_upload_now_uses_draft_bitrate() {
    use crate::golive::{Canvas, CustomQuality, DestAdvanced, DestinationQuality, PcBudget, VideoCodec};

    let (e, _dir) = engine().await;
    let host = Arc::new(DirectStudio::default());
    let s = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 14_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    e.set_studio_host(host);
    e.note_studio_settings(&s);
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Twitch)));

    // Unticked Twitch acts as failover backup for ticked YouTube.
    let mut adv = e.golive_config().advanced;
    adv.per.insert("youtube".into(), DestAdvanced { backup: Some("twitch".into()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt).await.unwrap();

    e.seed_backup_failing_for_test("youtube", true);
    let plan = e.plan_golive_encodes(PcBudget::apple_silicon(2));
    assert!(
        plan.destinations.contains_key("twitch"),
        "unticked failover backup for failing primary must be included in planner: {:?}",
        plan.destinations.keys().collect::<Vec<_>>()
    );
    e.seed_backup_failing_for_test("youtube", false);

    // Offline `upload_now` bills custom destination quality bitrate (`3,000` + `160`), not default `6,000`.
    let custom = e.add_golive_destination("Low", "rtmp://127.0.0.1:9/app", "k", Canvas::Main).await.unwrap();
    let q_low = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_000, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&custom.id, DestinationQuality::Custom(q_low)).await.unwrap();
    e.seed_upload_for_test(20_000);
    let cfg = GoLiveConfig {
        destinations: e
            .golive_config()
            .destinations
            .into_iter()
            .map(|mut d| {
                d.enabled = d.id == custom.id;
                d
            })
            .collect(),
        ..e.golive_config()
    };
    let check = e.upload_now_for_test(&cfg).unwrap();
    let row = check.rows.iter().find(|r| r.id == custom.id).unwrap();
    assert_eq!(row.need_kbps, 3_160, "offline upload_now must bill planned custom bitrate: {row:?}");

    // Turning light off midstream evaluates `midstream_misfit` with updated config (`light = false`),
    // and `studio_sync_light` cancels `route_light` when encode budget rejects the light stream.
    e.seed_light_route_for_test("youtube");
    let yt_d = e.golive_config().destinations.into_iter().find(|d| d.id == "youtube").unwrap();
    assert!(e.wants_light_now(&yt_d));
    e.cancel_route_light_destinations();
    let yt_d = e.golive_config().destinations.into_iter().find(|d| d.id == "youtube").unwrap();
    assert!(!e.wants_light_now(&yt_d));
}

#[tokio::test]
async fn f12_kick_multistream_close_reports_allow_multistreaming_when_sibling_live() {
    use crate::golive::Canvas;
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Obs, platform, platform_dropping};

    let (e, _dir) = engine().await;
    let host = Arc::new(DirectStudio::default());
    *host.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(host);

    let (kport, _kgot) = platform_dropping("kick-k", std::time::Duration::from_millis(40)).await;
    let (oport, _ogot) = platform("other-live").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{kport}/app"), "kick-k".into())));
    let other = e
        .add_golive_destination("Other", &format!("rtmp://127.0.0.1:{oport}/app"), "other-live", Canvas::Main)
        .await
        .unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "kick" || d.id == other.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();
    let mut adv = e.golive_config().advanced;
    adv.per
        .insert("kick".into(), crate::golive::DestAdvanced { first_retry_s: 1, max_retry_s: 1, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();

    e.studio_stream(true).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();

    let mut failed_state = None;
    for _ in 0..100 {
        let v = e.golive_view().await;
        if let Some(PushState::Failed { .. }) =
            v.destinations.iter().find(|d| d.id == "kick").and_then(|d| d.health.as_ref()).map(|h| &h.state)
        {
            failed_state = v.destinations.into_iter().find(|d| d.id == "kick").and_then(|d| d.health).map(|h| h.state);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    match failed_state {
        Some(PushState::Failed { reason, detail, .. }) => {
            assert!(reason.contains("Kick refused"), "{reason}");
            assert!(detail.as_deref().is_some_and(|d| d.contains("Allow multistreaming")), "{detail:?}");
        }
        other => panic!("Kick with live sibling must report multistreaming refusal: {other:?}"),
    }
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f13_vertical_only_without_vertical_canvas_fails_cleanly_without_main_fallback() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::platform;

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_on: false,
        vertical_size: None,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p_v, _got_v) = platform("v-off").await;
    let dv = e
        .add_golive_destination("VertOff", &format!("rtmp://127.0.0.1:{p_v}/app"), "v-off", Canvas::Vertical)
        .await
        .unwrap();
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == dv.id)).collect();
    e.set_golive_destinations(order).await.unwrap();

    let err = e.studio_stream(true).await.unwrap_err();
    assert!(
        err.to_string().to_ascii_lowercase().contains("vertical"),
        "must clearly report missing vertical picture: {err}"
    );
    assert!(host.started.lock().unwrap().is_empty(), "must never start a 16:9 main fallback encode");
}

fn ivs_budget_host(width: u32, height: u32, fps: u32, video_kbps: u32, engines: u32) -> Arc<MultiGroupHost> {
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() =
        crate::studio_host::StudioSettings { width, height, fps, video_kbps, ..Default::default() };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(engines),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    host
}

async fn recv_until_coded_frames(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ferrocast_testkit::rtmp::Got>,
    want_key: &str,
    min_coded_frames: usize,
    already_published: bool,
) -> (Option<(u32, u32)>, Vec<u32>) {
    use ferrocast_testkit::rtmp::Got;
    use tokio::time::{Duration, timeout};

    let mut seen_pub = already_published;
    let mut sps_dims = None;
    let mut timestamps = Vec::new();
    while timestamps.len() < min_coded_frames {
        match timeout(Duration::from_secs(5), rx.recv()).await.expect("rtmp packet in time").expect("rtmp open") {
            Got::Publish(k) => {
                assert_eq!(k, want_key);
                seen_pub = true;
            }
            Got::Video(ts, data) => {
                if let Some(d) = ferrocast_golive::flv::avcc_sps_dimensions(&data) {
                    sps_dims = Some(d);
                } else if !ferrocast_golive::flv::video_is_header(&data) && seen_pub {
                    timestamps.push(ts);
                }
            }
            Got::Audio(..) => {}
            Got::Closed => panic!("unexpected Got::Closed for {want_key}"),
        }
    }
    (sps_dims, timestamps)
}

#[tokio::test]
async fn scenario_a_1440p_main_youtube_and_kick_autofit_stay_live() {
    use ferrocast_core::event::Platform;
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{platform_rejecting_above, platform_with_meta};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, mut meta_yt) = platform_with_meta("yt-a").await;
    let (p_kick, mut got_kick, mut meta_kick) =
        platform_rejecting_above("kick-a", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-a".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "kick-a".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();

    let (yt_sps, yt_ts) = recv_until_coded_frames(&mut got_yt, "yt-a", 15, false).await;
    let (kick_sps, kick_ts) = recv_until_coded_frames(&mut got_kick, "kick-a", 15, false).await;
    assert_eq!(yt_sps, Some((2560, 1440)));
    assert_eq!(kick_sps, Some((1920, 1080)));
    assert!(!yt_ts.is_empty() && !kick_ts.is_empty());

    let yt_m = timeout(Duration::from_secs(2), meta_yt.recv()).await.unwrap().unwrap();
    let kick_m = timeout(Duration::from_secs(2), meta_kick.recv()).await.unwrap().unwrap();
    assert_eq!((yt_m.video_width, yt_m.video_height, yt_m.video_bitrate_kbps), (Some(2560), Some(1440), Some(18_000)));
    assert_eq!((kick_m.video_width, kick_m.video_height), (Some(1920), Some(1080)));
    assert!(kick_m.video_bitrate_kbps.is_some_and(|k| k <= 8_000), "kick meta kbps={:?}", kick_m.video_bitrate_kbps);

    let view = e.golive_view().await;
    for id in ["youtube", "kick"] {
        let state = view.destinations.iter().find(|d| d.id == id).and_then(|d| d.health.as_ref()).map(|h| &h.state);
        assert_eq!(state, Some(&PushState::Live), "{id} must stay Live, got {state:?}");
    }
    assert_eq!(e.golive_running_group("youtube").as_deref(), Some(crate::golive::GROUP_MAIN));
    assert_ne!(e.golive_running_group("kick").as_deref(), Some(crate::golive::GROUP_MAIN));

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn scenario_a_twitch_recommended_1080p_limit_accepts_1440p_at_6000kbps() {
    use ferrocast_core::event::Platform;
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{platform_rejecting_above, platform_with_meta};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 4);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, mut meta_yt) = platform_with_meta("yt-tw").await;
    let (p_tw, mut got_tw, mut meta_tw) =
        platform_rejecting_above("tw-a", 2560, 1440, 6000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-tw".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "tw-a".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();

    let (yt_sps, _) = recv_until_coded_frames(&mut got_yt, "yt-tw", 10, false).await;
    let (tw_sps, _) = recv_until_coded_frames(&mut got_tw, "tw-a", 10, false).await;
    assert_eq!(yt_sps, Some((2560, 1440)));
    assert_eq!(tw_sps, Some((2560, 1440)), "Twitch 1080p is recommended (soft), not hard-downscaled");

    let yt_m = timeout(Duration::from_secs(2), meta_yt.recv()).await.unwrap().unwrap();
    let tw_m = timeout(Duration::from_secs(2), meta_tw.recv()).await.unwrap().unwrap();
    assert_eq!((yt_m.video_width, yt_m.video_height, yt_m.video_bitrate_kbps), (Some(2560), Some(1440), Some(18_000)));
    assert_eq!((tw_m.video_width, tw_m.video_height, tw_m.video_bitrate_kbps), (Some(2560), Some(1440), Some(6_000)));

    let view = e.golive_view().await;
    let tw_state =
        view.destinations.iter().find(|d| d.id == "twitch").and_then(|d| d.health.as_ref()).map(|h| &h.state);
    assert_eq!(tw_state, Some(&PushState::Live), "Twitch must accept 1440p@6000 and stay Live");

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn scenario_b_ignore_service_limits_kick_rejects_1440p_with_limit_reason() {
    use ferrocast_core::event::Platform;
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{platform_rejecting_above, platform_with_meta};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, _meta_yt) = platform_with_meta("yt-b").await;
    let (p_kick, mut got_kick, mut meta_kick) =
        platform_rejecting_above("kick-b", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-b".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "kick-b".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    let mut adv = e.golive_config().advanced;
    adv.ignore_service_limits = true;
    adv.per
        .insert("kick".into(), crate::golive::DestAdvanced { first_retry_s: 1, max_retry_s: 1, ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();

    e.studio_stream(true).await.unwrap();

    let (yt_sps, _) = recv_until_coded_frames(&mut got_yt, "yt-b", 10, false).await;
    assert_eq!(yt_sps, Some((2560, 1440)));

    // Wait for Kick to publish on GROUP_MAIN (1440p), send 1440p SPS/metadata, and be closed by fake IVS (~200 ms).
    let mut kick_sps = None;
    let mut saw_kick_close = false;
    for _ in 0..50 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), got_kick.recv()).await.unwrap().unwrap() {
            ferrocast_testkit::rtmp::Got::Publish(k) => {
                assert_eq!(k, "kick-b");
                assert_eq!(e.golive_running_group("kick").as_deref(), Some(crate::golive::GROUP_MAIN));
            }
            ferrocast_testkit::rtmp::Got::Video(_, data) => {
                if let Some(d) = ferrocast_golive::flv::avcc_sps_dimensions(&data) {
                    kick_sps = Some(d);
                }
            }
            ferrocast_testkit::rtmp::Got::Closed => {
                saw_kick_close = true;
                break;
            }
            ferrocast_testkit::rtmp::Got::Audio(..) => {}
        }
    }
    assert!(saw_kick_close, "Kick IVS fake must close 1440p stream");
    assert_eq!(kick_sps, Some((2560, 1440)));
    let kick_m = tokio::time::timeout(std::time::Duration::from_secs(2), meta_kick.recv()).await.unwrap().unwrap();
    assert_eq!(
        (kick_m.video_width, kick_m.video_height, kick_m.video_bitrate_kbps),
        (Some(2560), Some(1440), Some(18_000))
    );

    let mut failed_state = None;
    for _ in 0..120 {
        let v = e.golive_view().await;
        if let Some(st) = v.destinations.into_iter().find(|d| d.id == "kick").and_then(|d| d.health).map(|h| h.state) {
            if let PushState::Reconnecting { ref reason, .. } = st {
                assert!(!reason.contains("Allow multistreaming"), "misdiagnosed Reconnecting: {reason}");
                assert!(reason.contains("1080p") || reason.contains("8,000 kbps"), "{reason}");
            }
            if matches!(st, PushState::Failed { .. }) {
                failed_state = Some(st);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    match failed_state {
        Some(PushState::Failed { reason, detail, refused, .. }) => {
            assert!(refused);
            assert!(!reason.contains("Allow multistreaming"), "{reason}");
            assert!(reason.contains("1080p") || reason.contains("8,000 kbps"), "{reason}");
            let detail = detail.expect("Kick failure detail");
            assert!(!detail.contains("Allow multistreaming"), "misdiagnosed detail: {detail}");
            assert!(detail.contains("2560") && detail.contains("18,000 kbps"), "{detail}");
        }
        other => panic!("expected Kick PushState::Failed with limit reason, got {other:?}"),
    }

    while let Ok(msg) = got_yt.try_recv() {
        assert!(!matches!(msg, ferrocast_testkit::rtmp::Got::Closed), "YouTube must stay Live while Kick fails");
    }
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn scenario_c_identical_custom_encodes_share_one_encoder_group() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{platform_rejecting_above, platform_with_meta};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, _) = platform_with_meta("yt-c").await;
    let (p_tw, mut got_tw, _) =
        platform_rejecting_above("tw-c", 1920, 1080, 6000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    let (p_cust, mut got_cust, _) =
        platform_rejecting_above("cust-c", 1920, 1080, 6000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-c".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "tw-c".into(),
    )));
    let cust = e
        .add_golive_destination("Custom", &format!("rtmp://127.0.0.1:{p_cust}/app"), "cust-c", Canvas::Main)
        .await
        .unwrap();

    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch" || d.id == cust.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    let shared_1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 6_000, codec: Some(VideoCodec::H264) };
    e.set_golive_quality("twitch", DestinationQuality::Custom(shared_1080.clone())).await.unwrap();
    e.set_golive_quality(&cust.id, DestinationQuality::Custom(shared_1080)).await.unwrap();

    e.studio_stream(true).await.unwrap();

    let (yt_sps, _) = recv_until_coded_frames(&mut got_yt, "yt-c", 8, false).await;
    let (tw_sps, _) = recv_until_coded_frames(&mut got_tw, "tw-c", 8, false).await;
    let (cust_sps, _) = recv_until_coded_frames(&mut got_cust, "cust-c", 8, false).await;
    assert_eq!(yt_sps, Some((2560, 1440)));
    assert_eq!(tw_sps, Some((1920, 1080)));
    assert_eq!(cust_sps, Some((1920, 1080)));

    let active_non_main: Vec<_> = e.active_encode_groups().into_iter().filter(|g| g.id != GROUP_MAIN).collect();
    assert_eq!(active_non_main.len(), 1, "identical 1080p60 encodes must share one group: {active_non_main:?}");
    let tw_gid = e.golive_running_group("twitch");
    let cust_gid = e.golive_running_group(&cust.id);
    assert_eq!(tw_gid, cust_gid);
    assert_ne!(tw_gid.as_deref(), Some(GROUP_MAIN));

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn scenario_d_live_switch_kick_to_720p30_zero_drop_on_youtube_and_old_group_stops() {
    use crate::golive::{CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform_rejecting_above, platform_with_meta};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 3);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, _) = platform_with_meta("yt-d").await;
    let (p_kick, mut got_kick, mut meta_kick) =
        platform_rejecting_above("kick-d", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-d".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "kick-d".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();

    let (yt_sps, mut yt_ts) = recv_until_coded_frames(&mut got_yt, "yt-d", 10, false).await;
    let (kick_sps_before, _) = recv_until_coded_frames(&mut got_kick, "kick-d", 10, false).await;
    assert_eq!(yt_sps, Some((2560, 1440)));
    assert_eq!(kick_sps_before, Some((1920, 1080)));
    let kick_m1 = timeout(Duration::from_secs(2), meta_kick.recv()).await.unwrap().unwrap();
    assert_eq!((kick_m1.video_width, kick_m1.video_height), (Some(1920), Some(1080)));

    let old_kick_gid = e.golive_running_group("kick").unwrap();
    assert_ne!(old_kick_gid, GROUP_MAIN);

    let q_720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality("kick", DestinationQuality::Custom(q_720)).await.unwrap();
    let new_kick_gid = e.golive_running_group("kick").unwrap();
    assert_ne!(new_kick_gid, old_kick_gid);

    // Kick disconnects once cleanly from the old group and reconnects with 1280x720 metadata + SPS.
    let mut saw_closed = false;
    let mut kick_sps_after = None;
    let mut post_switch_frames = 0usize;
    let mut republished = false;
    while post_switch_frames < 10 {
        match timeout(Duration::from_secs(5), got_kick.recv()).await.unwrap().unwrap() {
            Got::Closed => {
                assert!(!republished, "new Kick forwarder after switch must not disconnect");
                saw_closed = true;
            }
            Got::Publish(k) => {
                assert_eq!(k, "kick-d");
                republished = true;
            }
            Got::Video(_, data) => {
                if let Some(d) = ferrocast_golive::flv::avcc_sps_dimensions(&data) {
                    if republished {
                        kick_sps_after = Some(d);
                    }
                } else if republished && !ferrocast_golive::flv::video_is_header(&data) {
                    post_switch_frames += 1;
                }
            }
            Got::Audio(..) => {}
        }
    }
    assert!(saw_closed && republished, "Kick must close old connection and republish on new group");
    assert_eq!(kick_sps_after, Some((1280, 720)));
    let kick_m2 = timeout(Duration::from_secs(2), meta_kick.recv()).await.unwrap().unwrap();
    assert_eq!(
        (kick_m2.video_width, kick_m2.video_height, kick_m2.video_frame_rate, kick_m2.video_bitrate_kbps),
        (Some(1280), Some(720), Some(30.0), Some(3_500))
    );

    // YouTube stays completely uninterrupted across the entire switch with continuous timestamps.
    while let Ok(msg) = got_yt.try_recv() {
        match msg {
            Got::Closed => panic!("unexpected Got::Closed for yt-d during Kick switch"),
            Got::Video(ts, data) if !ferrocast_golive::flv::video_is_header(&data) => yt_ts.push(ts),
            _ => {}
        }
    }
    let (_, more_yt_ts) = recv_until_coded_frames(&mut got_yt, "yt-d", 10, true).await;
    yt_ts.extend(more_yt_ts);
    assert!(yt_ts.windows(2).all(|w| w[1] > w[0]), "YouTube timestamps must keep advancing: {yt_ts:?}");

    assert!(host.stopped_groups.lock().unwrap().contains(&old_kick_gid), "old Kick group must stop at 0 refcount");
    assert!(!e.active_encode_groups().iter().any(|g| g.id == old_kick_gid));

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn scenario_e_live_switch_timeout_aborts_and_keeps_kick_old_stream() {
    use crate::golive::{CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{platform_rejecting_above, platform_with_meta};
    use tokio::time::Duration;

    let (e, _dir) = engine().await;
    e.set_keyframe_wait_timeout_for_test(Duration::from_millis(25));
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 3);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (p_yt, mut got_yt, _) = platform_with_meta("yt-e").await;
    let (p_kick, mut got_kick, _) =
        platform_rejecting_above("kick-e", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-e".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "kick-e".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    let _ = recv_until_coded_frames(&mut got_yt, "yt-e", 6, false).await;
    let (kick_sps_before, _) = recv_until_coded_frames(&mut got_kick, "kick-e", 6, false).await;
    assert_eq!(kick_sps_before, Some((1920, 1080)));
    let old_kick_gid = e.golive_running_group("kick").unwrap();

    let q_720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut input = e.golive_planner_input(e.cached_pc_budget());
    input.destinations.iter_mut().find(|x| x.id == "kick").unwrap().quality = DestinationQuality::Custom(q_720.clone());
    let stalled_gid = crate::golive::plan_encodes(&input).destinations["kick"].group_id.clone();
    host.blocked_idr_groups.lock().unwrap().insert(stalled_gid.clone());

    let err = e.set_golive_quality("kick", DestinationQuality::Custom(q_720)).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { code: "golive_switch_timeout", .. }), "{err:?}");

    assert_eq!(e.golive_running_group("kick").as_deref(), Some(old_kick_gid.as_str()));
    assert!(host.stopped_groups.lock().unwrap().contains(&stalled_gid));
    assert!(!e.active_encode_groups().iter().any(|g| g.id == stalled_gid));

    // Both Kick's previous 1080p stream and YouTube remain connected and keep delivering frames.
    while let Ok(msg) = got_yt.try_recv() {
        assert!(!matches!(msg, ferrocast_testkit::rtmp::Got::Closed));
    }
    while let Ok(msg) = got_kick.try_recv() {
        assert!(!matches!(msg, ferrocast_testkit::rtmp::Got::Closed));
    }
    let _ = recv_until_coded_frames(&mut got_kick, "kick-e", 5, true).await;

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn offline_set_golive_quality_refuses_custom_over_pc_budget_422() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let s = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 10_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    // Windows pre-531 NVENC: max 3 sessions, auto_quality=true reserves 1 spare -> effective cap 2.
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        nvidia_driver_major: Some(525),
        cpu_cores: 8,
        host_sig: "win|nv=525".into(),
        ..Default::default()
    });
    e.set_studio_host(host);
    e.note_studio_settings(&s);
    e.register_connector(Arc::new(SignedIn(Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(Platform::Kick)));

    let custom = e.add_golive_destination("Extra", "rtmp://127.0.0.1:9/app", "k", Canvas::Main).await.unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch" || d.id == custom.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    // Group 1: YouTube + Custom (main 1080p60@10000); Group 2: Twitch Custom (1080p60@6000) -> fills 2/2 effective sessions.
    let tw_1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 6_000, codec: Some(VideoCodec::H264) };
    e.set_golive_quality("twitch", DestinationQuality::Custom(tw_1080)).await.unwrap();
    // Moving enabled `custom` onto a 3rd un-collapsible encode offline must fail with 422 encode_budget_exceeded.
    let distinct = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let err = e.set_golive_quality(&custom.id, DestinationQuality::Custom(distinct)).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "encode_budget_exceeded", .. }),
        "expected 422 encode_budget_exceeded offline, got {err:?}"
    );
}

#[tokio::test]
async fn quality_options_enforces_hard_vs_soft_limits_sharing_budget_and_locks() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, QualitySource, VerticalSource, VideoCodec};
    use ferrocast_core::event::Platform;

    let (e, _dir) = engine().await;

    // Before any Studio or OBS source is running: `"source": "none"`.
    let cold_opts = e.golive_quality_options().await;
    assert_eq!(cold_opts.source, QualitySource::None);
    assert!(cold_opts.destinations.contains_key("twitch"));
    assert!(cold_opts.destinations.contains_key("kick"));
    assert!(cold_opts.destinations.contains_key("youtube"));
    assert!(cold_opts.destinations.contains_key("youtube_vertical"));

    let host = ivs_budget_host(2560, 1440, 60, 18_000, 1);
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        nvidia_driver_major: Some(535),
        cpu_cores: 8,
        vertical_on: false,
        vertical_size: Some((1080, 1920)),
        host_sig: "win|nv=535".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 600.0,
        max_sessions_ok: Some(3),
        measured_at: 1,
        host_sig: "win|nv=535".into(),
    });
    e.set_studio_host(host.clone());
    e.register_connector(Arc::new(SignedIn(Platform::Youtube)));
    e.register_connector(Arc::new(SignedIn(Platform::Twitch)));
    e.register_connector(Arc::new(SignedIn(Platform::Kick)));

    let whip = e.add_golive_destination("WHIP", "https://whip.example.com/whip", "", Canvas::Main).await.unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    // First verify Twitch's soft 1080p recommendation before switching Twitch to Custom 1080p60@6M:
    // `same_as_main` resolves 1440p@6M with advisory note; 1440p60 remains selectable; 1080p60 is recommended.
    let pre_opts = e.golive_quality_options().await;
    let twitch = &pre_opts.destinations["twitch"];
    let tw_same = twitch.options.iter().find(|o| o.id == "same_as_main").unwrap();
    assert_eq!((tw_same.resolved.width, tw_same.resolved.height, tw_same.resolved.kbps), (2560, 1440, 6_000));
    assert_eq!(tw_same.note.as_deref(), Some("Above Twitch's recommended 1080p"));
    assert!(!tw_same.recommended);
    let tw_1440 = twitch.options.iter().find(|o| o.id == "1440p60_6000").unwrap();
    assert!(tw_1440.note.as_deref().is_some_and(|n| n.contains("recommended 1080p")));
    assert!(!tw_1440.recommended);
    let tw_1080 = twitch.options.iter().find(|o| o.id == "1080p60_6000").unwrap();
    assert!(tw_1080.recommended, "1080p60 should be Twitch default recommendation");
    assert_eq!(tw_1080.note, None);

    // Pin Twitch to recommended 1080p60@6M so YouTube (1440p) + Twitch (1080p60@6M) occupy 2/2 NVENC sessions.
    e.set_golive_quality(
        "twitch",
        DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 6_000,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();

    let opts = e.golive_quality_options().await;
    // Windows host filters HEVC out of custom_limits.
    let yt = &opts.destinations["youtube"];
    assert_eq!(yt.custom_limits.as_ref().unwrap().codecs, vec![VideoCodec::H264]);
    assert!(yt.custom_limits.as_ref().unwrap().fps_choices == vec![60, 30, 15]);

    // Kick: HARD 1080p limit (never offers 1440p or 2160p rungs), budget 2/2 full from YouTube+Twitch:
    // separate Kick 1080p60@8M must be disallowed with disabled_reason, while Kick 1080p60@6M
    // shares Twitch's encoder and stays allowed!
    let kick = &opts.destinations["kick"];
    assert!(kick.locked_reason.is_none());
    let kick_limits = kick.custom_limits.as_ref().unwrap();
    assert_eq!((kick_limits.max_width, kick_limits.max_height, kick_limits.max_kbps), (1920, 1080, 8_000));
    assert!(kick.options.iter().all(|o| o.resolved.height <= 1080));
    let kick_8m = kick.options.iter().find(|o| o.id == "1080p60_8000").expect("Kick 1080p60@8M rung");
    assert!(!kick_8m.allowed, "new 3rd encode on 2/2 NVENC budget must be disallowed");
    assert!(kick_8m.disabled_reason.is_some());
    let kick_6m = kick.options.iter().find(|o| o.id == "1080p60_6000").expect("Kick shared 1080p60@6M rung");
    assert!(kick_6m.allowed, "shared 1080p60@6M rung must stay allowed: {kick_6m:?}");
    assert_eq!(kick_6m.shares_encoder_with, vec!["Twitch".to_string()]);

    // Direct WHIP destination is locked and omits custom_limits.
    let whip_opts = &opts.destinations[&whip.id];
    assert!(whip_opts.locked_reason.is_some());
    assert!(whip_opts.custom_limits.is_none());

    // Vertical under Cut mode is locked.
    e.set_vertical_source(VerticalSource::Cut).await.unwrap();
    let opts_cut = e.golive_quality_options().await;
    assert!(opts_cut.destinations["youtube_vertical"].locked_reason.is_some());
    e.set_vertical_source(VerticalSource::Canvas).await.unwrap();

    // Twitch Enhanced Broadcasting locks Twitch custom options.
    e.set_golive_enhanced("twitch", true).await.unwrap();
    let opts_eb = e.golive_quality_options().await;
    assert!(opts_eb.destinations["twitch"].locked_reason.as_deref().is_some_and(|s| s.contains("Enhanced")));
    e.set_golive_enhanced("twitch", false).await.unwrap();

    // GoLiveView / DestinationView carry additive quality, resolved_quality, switching_quality, encode_groups, encode_budget.
    let view = e.golive_view().await;
    assert!(!view.encode_groups.is_empty());
    assert_eq!(view.encode_budget.used_sessions, 2);
    let tw_view = view.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(matches!(tw_view.quality, DestinationQuality::Custom(_)));
    assert_eq!(tw_view.resolved_quality.kbps, 6_000);
    assert!(!tw_view.switching_quality);
    assert_ne!(tw_view.resolved_quality.group_id, crate::golive::GROUP_MAIN);

    // Live Studio stream sets `"source": "studio"` and populates `live_fps` / `live_kbps`.
    let order_live: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(order_live).await.unwrap();
    e.studio_stream(true).await.unwrap();
    let live_opts = e.golive_quality_options().await;
    assert_eq!(live_opts.source, QualitySource::Studio);
    let live_view = e.golive_view().await;
    assert!(live_view.encode_groups.iter().any(|g| g.id == crate::golive::GROUP_MAIN && g.live_kbps.is_some()));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f2_encode_groups_start_holds_reconcile_so_kick_never_connects_to_main() {
    use crate::golive::{Canvas, DestinationQuality, GROUP_MAIN, group_id_for_key};
    use ferrocast_testkit::rtmp::{Got, platform, platform_rejecting_above};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct SlowMainStartHost {
        inner: Arc<MultiGroupHost>,
        entered_main: Arc<tokio::sync::Notify>,
        release_main: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for SlowMainStartHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            self.inner.status().await
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.start().await
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.stop().await
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            self.inner.act(act).await
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            let is_main = spec.id == GROUP_MAIN;
            let res = self.inner.start_encode_group(spec, hub).await?;
            if is_main {
                self.entered_main.notify_one();
                self.release_main.notified().await;
            }
            Ok(res)
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        video_encoder: Some("com.apple.videotoolbox.videoencoder.ave.hevc".into()),
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.hevc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    let entered_main = Arc::new(tokio::sync::Notify::new());
    let release_main = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SlowMainStartHost {
        inner: inner.clone(),
        entered_main: entered_main.clone(),
        release_main: release_main.clone(),
    });
    e.set_studio_host(host);
    e.note_studio_settings(&inner.settings.lock().unwrap().clone());
    e.register_connector(Arc::new(SignedIn(ferrocast_core::event::Platform::Youtube)));

    let (port_yt, mut got_yt) = platform("yt-race").await;
    let (port_kick, mut got_kick, _meta_kick) =
        platform_rejecting_above("kick-race", 1920, 1080, 9_000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-race".into())));
    let yt = e
        .add_golive_destination("YouTube", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-race", Canvas::Main)
        .await
        .unwrap();
    let kick = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-race", Canvas::Main)
        .await
        .unwrap();
    // Mark Kick as Kick service (`url: None` so it uses LocalKick) to force auto-fit 1080p60@8000 instead of 1440p60 main.
    let mut cfg = e.golive_config();
    let kd = cfg.destinations.iter_mut().find(|d| d.id == kick.id).unwrap();
    kd.kind = ferrocast_golive::probe::Service::Kick;
    kd.url = None;
    e.save_golive(cfg).await.unwrap();
    let kick_gid = e.plan_golive_encodes(e.cached_pc_budget()).destinations[&kick.id].group_id.clone();
    assert_ne!(kick_gid, GROUP_MAIN);
    let _ = (group_id_for_key, DestinationQuality::SameAsMain, &yt);

    let e_start = e.clone();
    let start_task = tokio::spawn(async move { e_start.studio_stream(true).await });
    timeout(Duration::from_secs(3), entered_main.notified()).await.expect("entered GROUP_MAIN start");
    tokio::time::sleep(Duration::from_millis(60)).await;
    release_main.notify_waiters();
    start_task.await.unwrap().unwrap();

    loop {
        match timeout(Duration::from_secs(3), got_yt.recv()).await.expect("yt event").expect("open") {
            Got::Publish(k) if k == "yt-race" => break,
            Got::Closed => panic!("YouTube closed unexpectedly"),
            _ => {}
        }
    }
    let mut kick_published = false;
    for _ in 0..40 {
        match timeout(Duration::from_millis(200), got_kick.recv()).await {
            Ok(Some(Got::Closed)) => panic!("Kick connected to un-started main group and got rejected by IVS"),
            Ok(Some(Got::Publish(k))) if k == "kick-race" => {
                kick_published = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("kick server closed"),
            Err(_) if kick_published => break,
            Err(_) => {}
        }
    }
    assert!(kick_published, "Kick must publish cleanly on its fitted 1080p60 group");
    assert_eq!(e.golive_running_group(&kick.id).as_deref(), Some(kick_gid.as_str()));
    assert_eq!(inner.start_counts.lock().unwrap().get(&kick_gid).copied(), Some(1));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f4a_live_switch_checks_transient_overlap_against_nvenc_session_budget() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 30,
        video_kbps: 14_000,
        video_encoder: Some("ffmpeg_nvenc".into()),
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        hw_backend: Some("nvenc".into()),
        nvidia_driver_major: Some(525), // 3 raw -> 2 effective under auto_quality
        cpu_cores: 8,
        host_sig: "win-nvenc-525".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());
    e.set_golive_auto_quality(true).await.unwrap();

    let (port_yt, mut got_yt) = platform("yt-ov").await;
    let (port_kick, mut got_kick) = platform("kick-ov").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-ov".into())));
    let yt = e
        .add_golive_destination("YT", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-ov", Canvas::Main)
        .await
        .unwrap();
    let kick = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-ov", Canvas::Main)
        .await
        .unwrap();
    let mut cfg = e.golive_config();
    let kd = cfg.destinations.iter_mut().find(|d| d.id == kick.id).unwrap();
    kd.kind = ferrocast_golive::probe::Service::Kick;
    kd.url = None;
    e.save_golive(cfg).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "yt-ov"), (&mut got_kick, "kick-ov")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    assert_eq!(host.started.lock().unwrap().len(), 2);

    // Steady state of switching Kick to 720p30 is 2 sessions (main + 720p30), but during the IDR handover
    // old g_1080p60_8000 is still running alongside new g_720p30_3500 + main = 3 > 2 effective NVENC sessions!
    let custom_720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let err = e.set_golive_quality(&kick.id, DestinationQuality::Custom(custom_720)).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "encode_budget_exceeded", .. }),
        "expected 422 encode_budget_exceeded during transient overlap, got {err:?}"
    );
    assert_eq!(host.started.lock().unwrap().len(), 2, "must not start a 3rd overlapping group");
    let _ = yt;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f4b_concurrent_switches_preserve_shared_group_on_single_cancel_and_cleanup_on_all_fail() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port_a, mut got_a) = platform("ka-4b").await;
    let (port_b, mut got_b) = platform("kb-4b").await;
    let a = e
        .add_golive_destination("DestA", &format!("rtmp://127.0.0.1:{port_a}/app"), "ka-4b", Canvas::Main)
        .await
        .unwrap();
    let b = e
        .add_golive_destination("DestB", &format!("rtmp://127.0.0.1:{port_b}/app"), "kb-4b", Canvas::Main)
        .await
        .unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "ka-4b"), (&mut got_b, "kb-4b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 4_500, codec: Some(VideoCodec::H264) };
    let target_gid = {
        let mut input = e.golive_planner_input(e.cached_pc_budget());
        input.destinations.iter_mut().find(|d| d.id == a.id).unwrap().quality =
            DestinationQuality::Custom(q720.clone());
        crate::golive::plan_encodes(&input).destinations[&a.id].group_id.clone()
    };
    let notify = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(target_gid.clone(), notify.clone());

    // Spawn A and B switching to `target_gid`, then cancel A mid-wait via untick.
    // With `group_ops` serializing lifecycle operations, unticking A cancels A's pending switch token
    // immediately before waiting on `group_ops`; B then finishes cleanly on `target_gid`.
    let (ea, eb) = (e.clone(), e.clone());
    let (ida, idb, qa, qb) = (a.id.clone(), b.id.clone(), q720.clone(), q720.clone());
    let task_a = tokio::spawn(async move { ea.set_golive_quality(&ida, DestinationQuality::Custom(qa)).await });
    for _ in 0..100 {
        if host.started.lock().unwrap().contains_key(&target_gid) {
            break;
        }
        tokio::task::yield_now().await;
    }
    host.idr_gate.lock().unwrap().remove(&target_gid);
    let task_b = tokio::spawn(async move { eb.set_golive_quality(&idb, DestinationQuality::Custom(qb)).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let disable_a: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id != a.id && d.enabled)).collect();
    e.set_golive_destinations(disable_a).await.unwrap();
    notify.notify_waiters();
    let res_a = task_a.await.unwrap();
    let res_b = task_b.await.unwrap();
    assert!(res_a.is_err(), "cancelled switch A must return error");
    assert!(res_b.is_ok(), "surviving peer switch B must succeed, got {res_b:?}");
    assert!(host.started.lock().unwrap().contains_key(&target_gid), "peer B still needs shared group");
    assert_eq!(e.golive_running_group(&b.id).as_deref(), Some(target_gid.as_str()));

    // Sub-case 2: switch B + C (add C) concurrently to a second group that stalls/fails on IDR: both fail -> group cleaned up!
    e.set_keyframe_wait_timeout_for_test(Duration::from_millis(30));
    let q540 = CustomQuality { width: 960, height: 540, fps: 30, kbps: 2_000, codec: Some(VideoCodec::H264) };
    let (port_c, mut got_c) = platform("kc-4b").await;
    let c = e
        .add_golive_destination("DestC", &format!("rtmp://127.0.0.1:{port_c}/app"), "kc-4b", Canvas::Main)
        .await
        .unwrap();
    let enable_bc: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == b.id || d.id == c.id)).collect();
    e.set_golive_destinations(enable_bc).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_c.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "kc-4b")
        {
            break;
        }
    }
    let fail_gid = {
        let mut input = e.golive_planner_input(e.cached_pc_budget());
        input.destinations.iter_mut().find(|d| d.id == b.id).unwrap().quality =
            DestinationQuality::Custom(q540.clone());
        crate::golive::plan_encodes(&input).destinations[&b.id].group_id.clone()
    };
    host.blocked_idr_groups.lock().unwrap().insert(fail_gid.clone());
    let (eb2, ec2) = (e.clone(), e.clone());
    let (idb2, idc2, qb2, qc2) = (b.id.clone(), c.id.clone(), q540.clone(), q540);
    let tb2 = tokio::spawn(async move { eb2.set_golive_quality(&idb2, DestinationQuality::Custom(qb2)).await });
    let tc2 = tokio::spawn(async move { ec2.set_golive_quality(&idc2, DestinationQuality::Custom(qc2)).await });
    let (rb2, rc2) = tokio::join!(tb2, tc2);
    assert!(rb2.unwrap().is_err());
    assert!(rc2.unwrap().is_err());
    assert!(!host.started.lock().unwrap().contains_key(&fail_gid), "failed shared switch group must not leak");
    let _ = GROUP_MAIN;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f1a_studio_settings_update_syncs_main_group_key_so_switch_back_matches_main() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let init_settings = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = init_settings.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (port_m, mut got_m) = platform("m-1a").await;
    let (port_c, mut got_c) = platform("c-1a").await;
    let main_d = e
        .add_golive_destination("MainD", &format!("rtmp://127.0.0.1:{port_m}/app"), "m-1a", Canvas::Main)
        .await
        .unwrap();
    let cust_d = e
        .add_golive_destination("CustD", &format!("rtmp://127.0.0.1:{port_c}/app"), "c-1a", Canvas::Main)
        .await
        .unwrap();
    e.set_golive_quality(
        &cust_d.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 2_500,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_m, "m-1a"), (&mut got_c, "c-1a")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let next_settings = crate::studio_host::StudioSettings { video_kbps: 5_000, ..init_settings };
    e.studio_set_settings(next_settings).await.unwrap();
    let active_main =
        e.active_encode_groups().into_iter().find(|g| g.id == GROUP_MAIN).expect("GROUP_MAIN remains active");
    assert_eq!(active_main.key.kbps, 5_000, "live GROUP_MAIN key must reflect updated studio bitrate");
    e.set_golive_quality(&cust_d.id, DestinationQuality::SameAsMain).await.unwrap();
    assert_eq!(e.golive_running_group(&cust_d.id).as_deref(), Some(GROUP_MAIN));
    let _ = main_d;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f1b_enabling_or_switching_third_dest_never_disrupts_live_peer_on_tight_budget() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 30,
        video_kbps: 12_000,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "linux".into(),
        encoder_id: "obs_x264".into(),
        cpu_cores: 12, // 2 sessions, 250 Mpx/s
        host_sig: "linux-cpu-12".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());
    pretend_ffmpeg(&e);

    let (port_yt, mut got_yt) = platform("yt-1b").await;
    let (port_kick, mut got_kick) = platform("kick-1b").await;
    let (port_c, mut got_c) = platform("cust-1b").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-1b".into())));
    let yt = e
        .add_golive_destination("YT", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-1b", Canvas::Main)
        .await
        .unwrap();
    let kick = e
        .add_golive_destination("Kick", &format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-1b", Canvas::Main)
        .await
        .unwrap();
    let cust = e
        .add_golive_destination("CustomPeer", &format!("rtmp://127.0.0.1:{port_c}/app"), "cust-1b", Canvas::Main)
        .await
        .unwrap();
    let mut cfg = e.golive_config();
    let kd = cfg.destinations.iter_mut().find(|d| d.id == kick.id).unwrap();
    kd.kind = ferrocast_golive::probe::Service::Kick;
    kd.url = None;
    e.save_golive(cfg).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "yt-1b"), (&mut got_kick, "kick-1b"), (&mut got_c, "cust-1b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let kick_group_before = e.golive_running_group(&kick.id).unwrap();
    assert_ne!(kick_group_before, GROUP_MAIN);
    // Switching `cust` from GROUP_MAIN to 720p30@3500 would force `kick` to coalesce into `720p30`, changing kick's live group!
    let err = e
        .set_golive_quality(
            &cust.id,
            DestinationQuality::Custom(CustomQuality {
                width: 1280,
                height: 720,
                fps: 30,
                kbps: 3_500,
                codec: Some(VideoCodec::H264),
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "encode_budget_exceeded", .. }),
        "must refuse change that would move live peer Kick off its group, got {err:?}"
    );
    assert_eq!(e.golive_running_group(&kick.id).as_deref(), Some(kick_group_before.as_str()));
    assert!(!matches!(got_kick.try_recv(), Ok(Got::Closed)), "Kick must stay live without disconnect");
    let _ = yt;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f4c_start_direct_signals_stop_on_displaced_running() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::Obs;

    let (e, _dir) = engine().await;
    let host = Arc::new(DirectStudio::default());
    e.set_studio_host(host);
    let srt = e.add_golive_destination("SRT", "srt://127.0.0.1:19999", "", Canvas::Main).await.unwrap();

    // Insert a sentinel Running entry before start_direct replaces it.
    let (sentinel_stop_tx, sentinel_stop_rx) = tokio::sync::watch::channel(false);
    let (_htx, sentinel_health) = tokio::sync::watch::channel(ferrocast_golive::push::Health {
        state: ferrocast_golive::push::PushState::Live,
        kbps: 0,
        dropped: 0,
        sent: 0,
        link: None,
        via: None,
        since_ms: 0,
    });
    let (other_live, _) = tokio::sync::watch::channel(false);
    e.insert_sentinel_running_for_test(&srt.id, sentinel_stop_tx, sentinel_health, other_live);

    let (server, key) = e.golive_ingest().await.unwrap();
    let port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(port, "live", &key).await.unwrap();
    tokio::task::yield_now().await;

    let cfg = e.golive_config();
    let d = cfg.destinations.iter().find(|x| x.id == srt.id).unwrap().clone();
    let url = d.url.clone().unwrap();
    e.start_direct_for_test(&d, url).await;
    assert!(*sentinel_stop_rx.borrow(), "displaced sentinel Running task must receive stop = true");
}

#[tokio::test]
async fn r_f4d_live_switch_succeeds_for_active_failover_backup_when_unticked() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host);

    let (port_p, mut got_p) = platform("prim-4d").await;
    let (port_b, mut got_b) = platform("back-4d").await;
    let prim = e
        .add_golive_destination("Primary", &format!("rtmp://127.0.0.1:{port_p}/app"), "prim-4d", Canvas::Main)
        .await
        .unwrap();
    let back = e
        .add_golive_destination("Backup", &format!("rtmp://127.0.0.1:{port_b}/app"), "back-4d", Canvas::Main)
        .await
        .unwrap();

    let mut adv = e.golive_config().advanced;
    adv.per
        .insert(prim.id.clone(), crate::golive::DestAdvanced { backup: Some(back.id.clone()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == prim.id)).collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_p.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "prim-4d")
        {
            break;
        }
    }
    e.seed_backup_failing_for_test(&prim.id, true);
    e.reconcile().await;
    loop {
        if matches!(timeout(Duration::from_secs(3), got_b.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "back-4d")
        {
            break;
        }
    }
    assert!(e.golive_running_group(&back.id).is_some(), "failover backup is live even with enabled=false");

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&back.id, DestinationQuality::Custom(q720))
        .await
        .expect("live failover backup must switch quality smoothly without golive_switch_cancelled");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn r_f8b_ffenc_handover_session_exhausted_counts_unshared_recording_session() {
    use crate::golive::GROUP_MAIN;
    use ferrocast_golive::flv::VideoCodec;
    use ferrocast_golive::probe::{Canvas, EncodeKey};

    let (e, _dir) = engine().await;
    e.note_host_budget_info(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        hw_backend: Some("nvenc".into()),
        nvidia_driver_major: Some(525), // raw limit = 3 sessions
        cpu_cores: 8,
        record_shares_main: true,
        record_extra_sessions: 0,
        host_sig: "win-nv-525".into(),
        ..Default::default()
    });
    e.set_active_group_for_test(crate::studio_host::EncodeGroupSpec {
        id: GROUP_MAIN.into(),
        key: EncodeKey {
            canvas: Canvas::Main,
            width: 1920,
            height: 1080,
            fps: 60,
            fps_ntsc: false,
            kbps: 6_000,
            codec: VideoCodec::H264,
            keyframe_s: 2,
            strict_cbr: false,
            no_bframes: false,
            audio_kbps: 160,
        },
        extra_tracks: Vec::new(),
        vod_track: true, // prevents sharing with plain "Same as stream" recording -> +1 session!
        is_light: false,
        is_twitch_eb: false,
        cap_kbps: None,
    });
    e.set_active_group_for_test(crate::studio_host::EncodeGroupSpec {
        id: "g_other".into(),
        key: EncodeKey {
            canvas: Canvas::Main,
            width: 1280,
            height: 720,
            fps: 30,
            fps_ntsc: false,
            kbps: 3_500,
            codec: VideoCodec::H264,
            keyframe_s: 2,
            strict_cbr: false,
            no_bframes: false,
            audio_kbps: 160,
        },
        extra_tracks: Vec::new(),
        vod_track: false,
        is_light: false,
        is_twitch_eb: false,
        cap_kbps: None,
    });
    assert!(
        e.ffenc_handover_session_exhausted(),
        "2 active stream groups + 1 unshared recording session = 3/3 NVENC sessions exhausted"
    );
}

#[tokio::test]
async fn r_f8c_studio_cap_config_detects_libsvtav1_as_software() {
    let (e, _dir) = engine().await;
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        encoder: "hw".into(),
        video_encoder: Some("libsvtav1".into()),
        ..Default::default()
    });
    let is_sw = e.current_studio_cap_is_software_for_test().await;
    assert!(is_sw, "libsvtav1 must be classified as software encoder");
}

#[tokio::test]
async fn r_n6_live_quality_switch_clears_upload_protection_pause() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host);

    let (port_a, mut got_a) = platform("na-6").await;
    let (port_b, mut got_b) = platform("nb-6").await;
    let _a = e
        .add_golive_destination("Main", &format!("rtmp://127.0.0.1:{port_a}/app"), "na-6", Canvas::Main)
        .await
        .unwrap();
    let b =
        e.add_golive_destination("Sub", &format!("rtmp://127.0.0.1:{port_b}/app"), "nb-6", Canvas::Main).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "na-6"), (&mut got_b, "nb-6")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    e.pause(b.id.clone(), "Main".into());
    loop {
        if matches!(timeout(Duration::from_secs(3), got_b.recv()).await.unwrap(), Some(Got::Closed)) {
            break;
        }
    }
    let state_b = |v: &crate::golive::GoLiveView| {
        v.destinations.iter().find(|d| d.id == b.id).and_then(|d| d.health.clone()).map(|h| h.state)
    };
    assert!(matches!(state_b(&e.golive_view().await), Some(PushState::Paused { .. })));

    e.set_golive_quality(
        &b.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 2_500,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_b.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "nb-6")
        {
            break;
        }
    }
    assert!(!e.protector_is_paused_for_test(&b.id), "switch must release Protector pause");
    assert!(!e.golive_is_paused_for_test(&b.id), "switch must clear GoLive paused entry");
    let st = state_b(&e.golive_view().await);
    assert!(!matches!(st, Some(PushState::Paused { .. })), "view must not stay Paused after quality switch: {st:?}");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn q5_4_options_match_put_for_live_same_as_main_and_offline_over_shared_custom() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "linux".into(),
        encoder_id: "obs_x264".into(),
        cpu_cores: 12, // 2 sessions, 250 Mpx/s
        host_sig: "linux-cpu-12".into(),
        ..Default::default()
    });
    e.set_studio_host(host);
    pretend_ffmpeg(&e);

    let (port_a, mut got_a) = platform("ka-54").await;
    let (port_b, mut got_b) = platform("kb-54").await;
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{port_a}/app"), "ka-54", Canvas::Main).await.unwrap();
    let b =
        e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{port_b}/app"), "kb-54", Canvas::Main).await.unwrap();
    // Subcase A (offline Over budget sharing existing Custom): force Over with A on main 1440p60 + C on Custom(1080p60) + B on Custom(720p30) -> wait:
    // Offline with A (main 1440p60) + Custom 1080p60@6000 already > 250 Mpx/s (Over!), and sharing B onto SAME Custom(1080p60@6000) adds 0 sessions!
    let shared_q = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 6_000, codec: Some(VideoCodec::H264) };
    let (port_c, _got_c) = platform("kc-54").await;
    let c =
        e.add_golive_destination("C", &format!("rtmp://127.0.0.1:{port_c}/app"), "kc-54", Canvas::Main).await.unwrap();
    // Temporarily disable A and B so C fits and establishes the 1080p60@6000 Custom group, then re-enable A (1440p60 + 1080p60 = Over).
    let only_c: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == c.id)).collect();
    e.set_golive_destinations(only_c).await.unwrap();
    e.set_golive_quality(&c.id, DestinationQuality::Custom(shared_q.clone())).await.unwrap();
    let all_abc: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == a.id || d.id == b.id || d.id == c.id))
        .collect();
    e.set_golive_destinations(all_abc).await.unwrap();
    let opts_offline = e.golive_quality_options().await;
    let b_opts = &opts_offline.destinations[&b.id].options;
    let shared_opt_allowed = b_opts
        .iter()
        .find(|o| matches!(&o.quality, DestinationQuality::Custom(q) if q.width == 1920 && q.height == 1080 && q.fps == 60 && q.kbps == 6_000))
        .map(|o| o.allowed);
    let put_shared_res = e.set_golive_quality(&b.id, DestinationQuality::Custom(shared_q)).await;
    assert_eq!(shared_opt_allowed, Some(true), "zero-cost shared custom option must be allowed even when Over");
    assert!(
        put_shared_res.is_ok(),
        "PUT sharing existing custom group must match options and succeed: {put_shared_res:?}"
    );

    // Reset B to 720p30@3000, disable C, change settings to 1080p30@6000 (`62.2` Mpx/s) + Kick custom so live 2/2 overlap hits SameAsMain!
    let disable_c: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id != c.id)).collect();
    e.set_golive_destinations(disable_c).await.unwrap();
    e.set_golive_quality(&c.id, DestinationQuality::SameAsMain).await.unwrap();
    e.studio_set_settings(crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 30,
        video_kbps: 6_000,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 2_000, codec: Some(VideoCodec::H264) };
    // Live with A on 480p30 and B on 720p30 (2/2 sessions; GROUP_MAIN inactive, OR keep A on GROUP_MAIN + B on 720p30):
    // Keep A on GROUP_MAIN (1080p30) and B on Custom(720p30@3500) -> 2/2 sessions! Wait: Make B Service::Kick (`same_as_main` = 1080p30@6000 strict_cbr -> wait shares GROUP_MAIN if bitrate <= 8000!).
    // With B on `Custom(720p30)`, `same_as_main` uses `GROUP_MAIN` (no overlap!). To test overlap of `same_as_main`, use `B = Service::Kick` when `main = 1440p30` (`2560x1440@30 = 110.6 Mpx/s`), so `Kick SameAsMain` fits to `1080p30@8000` (a SEPARATE group from `GROUP_MAIN`!).
    e.studio_set_settings(crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 30,
        video_kbps: 14_000,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut cfg = e.golive_config();
    let bd = cfg.destinations.iter_mut().find(|d| d.id == b.id).unwrap();
    bd.kind = ferrocast_golive::probe::Service::Kick;
    bd.url = None;
    e.save_golive(cfg).await.unwrap();
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_b}/app"), "kb-54".into())));
    e.set_golive_quality(&b.id, DestinationQuality::Custom(q480)).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "ka-54"), (&mut got_b, "kb-54")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let live_opts = e.golive_quality_options().await;
    let same_opt = live_opts.destinations[&b.id]
        .options
        .iter()
        .find(|o| o.quality == DestinationQuality::SameAsMain)
        .expect("same_as_main option present");
    let put_same = e.set_golive_quality(&b.id, DestinationQuality::SameAsMain).await;
    assert_eq!(
        same_opt.allowed,
        put_same.is_ok(),
        "quality-options allowed={} must match PUT SameAsMain={put_same:?}",
        same_opt.allowed
    );
    assert!(!same_opt.allowed, "live 2/2 overlap onto new 1080p30 Kick group must be disallowed");
    let _ = (a, q720, GROUP_MAIN);
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn q5_5_quality_errors_split_400_field_errors_from_422_locks_and_limits() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VerticalSource, VideoCodec};

    let (e, _dir) = engine().await;
    e.note_host_budget_info(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        hw_backend: Some("nvenc".into()),
        cpu_cores: 8,
        vertical_size: Some((1080, 1920)),
        host_sig: "win-nv".into(),
        ..Default::default()
    });
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        width: 3840,
        height: 2160,
        fps: 60,
        video_kbps: 20_000,
        ..Default::default()
    });
    let yt = e.add_golive_destination("YT", "rtmp://127.0.0.1:1935/live", "yt", Canvas::Main).await.unwrap();
    let srt = e.add_golive_destination("SRT", "srt://127.0.0.1:9999", "", Canvas::Main).await.unwrap();
    let vert = e.add_golive_destination("V", "rtmp://127.0.0.1:1935/live", "v", Canvas::Vertical).await.unwrap();
    let mut cfg = e.golive_config();
    cfg.destinations.iter_mut().find(|d| d.id == "kick").unwrap().enabled = false;
    e.save_golive(cfg).await.unwrap();

    let q_valid = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    // 400 Invalid field / shape errors:
    for bad in [
        CustomQuality { width: 1281, ..q_valid.clone() },
        CustomQuality { fps: 45, ..q_valid.clone() },
        CustomQuality { kbps: 100, ..q_valid.clone() },
        CustomQuality { codec: Some(VideoCodec::Vp9), ..q_valid.clone() },
    ] {
        let err = e.set_golive_quality(&yt.id, DestinationQuality::Custom(bad.clone())).await.unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "expected 400 Invalid for {bad:?}, got {err:?}");
    }
    // 422 Refused platform limit (`platform_limit_exceeded`): Kick > 1080p
    let err_lim = e
        .set_golive_quality(
            "kick",
            DestinationQuality::Custom(CustomQuality {
                width: 2560,
                height: 1440,
                fps: 60,
                kbps: 12_000,
                codec: Some(VideoCodec::H264),
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_lim, EngineError::Refused { status: 422, code: "platform_limit_exceeded", .. }),
        "expected 422 platform_limit_exceeded, got {err_lim:?}"
    );
    // 422 Refused mode / codec locks (`golive_quality_locked`):
    let err_srt = e.set_golive_quality(&srt.id, DestinationQuality::Custom(q_valid.clone())).await.unwrap_err();
    assert!(matches!(err_srt, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }), "{err_srt:?}");

    e.set_golive_enhanced("twitch", true).await.unwrap();
    let err_eb = e.set_golive_quality("twitch", DestinationQuality::Custom(q_valid.clone())).await.unwrap_err();
    assert!(matches!(err_eb, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }), "{err_eb:?}");

    e.set_vertical_source(VerticalSource::Cut).await.unwrap();
    let err_cut = e.set_golive_quality(&vert.id, DestinationQuality::Custom(q_valid.clone())).await.unwrap_err();
    assert!(matches!(err_cut, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }), "{err_cut:?}");

    let err_hevc = e
        .set_golive_quality(
            &yt.id,
            DestinationQuality::Custom(CustomQuality { codec: Some(VideoCodec::Hevc), ..q_valid }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err_hevc, EngineError::Refused { status: 422, code: "golive_quality_locked", .. }),
        "{err_hevc:?}"
    );
}

#[tokio::test]
async fn q5_6_quality_option_ids_are_unique_for_codecs_and_ultrawide() {
    use std::collections::HashSet;

    let (e, _dir) = engine().await;
    e.note_host_budget_info(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.hevc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        width: 2560,
        height: 1080, // ultrawide 21:9
        fps: 60,
        video_kbps: 15_000,
        video_encoder: Some("com.apple.videotoolbox.videoencoder.ave.hevc".into()),
        ..Default::default()
    });
    let opts = e.golive_quality_options().await;
    for (dest_id, d_opts) in &opts.destinations {
        let ids: Vec<&str> = d_opts.options.iter().map(|o| o.id.as_str()).collect();
        let uniq: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), uniq.len(), "duplicate quality option IDs for {dest_id}: {ids:?}");
    }
}

#[tokio::test]
async fn q5_8_disabled_vertical_destination_uses_vertical_canvas_for_quality_note() {
    use crate::golive::Canvas;

    let (e, _dir) = engine().await;
    e.note_host_budget_info(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.h264".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_size: Some((1080, 1920)),
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    // Main canvas is 1440p60 (exceeds Twitch 1080p60 rec), while vertical canvas is 1080x1920@60 (fits!).
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    });
    let vert = e.add_golive_destination("TwitchV", "rtmp://127.0.0.1:1935/app", "tv", Canvas::Vertical).await.unwrap();
    let mut cfg = e.golive_config();
    let d = cfg.destinations.iter_mut().find(|d| d.id == vert.id).unwrap();
    d.kind = ferrocast_golive::probe::Service::Twitch;
    d.enabled = false;
    e.save_golive(cfg).await.unwrap();

    let view = e.golive_view().await;
    let dv = view.destinations.iter().find(|d| d.id == vert.id).unwrap();
    assert_eq!(dv.quality_note, None, "vertical 1080x1920@60 destination must not receive 1440p main canvas note");
}

#[tokio::test]
async fn q5_9_live_set_golive_light_checks_budget_and_preserves_old_stream_on_over() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 30,
        video_kbps: 12_000,
        encoder: "software".into(),
        video_encoder: Some("obs_x264".into()),
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "linux".into(),
        encoder_id: "obs_x264".into(),
        cpu_cores: 12, // 2 sessions max
        host_sig: "linux-cpu-12".into(),
        ..Default::default()
    });
    e.set_studio_host(host);
    pretend_ffmpeg(&e);

    let (port_a, mut got_a) = platform("la-59").await;
    let (port_b, mut got_b) = platform("lb-59").await;
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{port_a}/app"), "la-59", Canvas::Main).await.unwrap();
    let b =
        e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{port_b}/app"), "lb-59", Canvas::Main).await.unwrap();
    let mut cfg = e.golive_config();
    let bd = cfg.destinations.iter_mut().find(|d| d.id == b.id).unwrap();
    bd.kind = ferrocast_golive::probe::Service::Kick;
    bd.url = None;
    e.save_golive(cfg).await.unwrap();
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_b}/app"), "lb-59".into())));

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "la-59"), (&mut got_b, "lb-59")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    // 2/2 sessions used (`GROUP_MAIN` 1080p30@12000 for A + `g_1080p30_8000` for Kick B).
    // Turning legacy `light` on for A tries adding a 3rd 720p30@3000 group -> must fail 422 without killing A!
    let err = e.set_golive_light(&a.id, true).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "encode_budget_exceeded", .. }),
        "live set_golive_light(true) over budget must return 422, got {err:?}"
    );
    assert!(!e.golive_config().destinations.iter().find(|d| d.id == a.id).unwrap().light);
    assert!(!matches!(got_a.try_recv(), Ok(Got::Closed)), "A must stay live on refused light switch");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn concurrent_switch_does_not_race_reconcile_or_stop_group_if_unused_teardown() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct SlowStopGroupHost {
        inner: Arc<MultiGroupHost>,
        arm_gid: std::sync::Mutex<Option<String>>,
        entered_stop: Arc<tokio::sync::Notify>,
        release_stop: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for SlowStopGroupHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            self.inner.status().await
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.start().await
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.stop().await
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            self.inner.act(act).await
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            self.inner.start_encode_group(spec, hub).await
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            let should_pause = {
                let mut armed = self.arm_gid.lock().unwrap();
                if armed.as_deref() == Some(id) {
                    *armed = None;
                    true
                } else {
                    false
                }
            };
            if should_pause {
                self.entered_stop.notify_one();
                self.release_stop.notified().await;
            }
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
            self.inner.live_group_key(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    let entered_stop = Arc::new(tokio::sync::Notify::new());
    let release_stop = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SlowStopGroupHost {
        inner: inner.clone(),
        arm_gid: std::sync::Mutex::new(None),
        entered_stop: entered_stop.clone(),
        release_stop: release_stop.clone(),
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("race-a").await;
    let (pb, mut got_b) = platform("race-b").await;
    let (pc, mut got_c) = platform("race-c").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "race-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "race-b", Canvas::Main).await.unwrap();
    let c = e.add_golive_destination("C", &format!("rtmp://127.0.0.1:{pc}/app"), "race-c", Canvas::Main).await.unwrap();

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720.clone())).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "race-a"), (&mut got_b, "race-b"), (&mut got_c, "race-c")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let gid_720 = e.golive_running_group(&a.id).expect("A running on 720p30");

    // 1. `stop_group_if_unused` race: A leaves `gid_720` (pausing inside `stop_encode_group`) while B switches onto `gid_720`.
    let idr_gate = Arc::new(tokio::sync::Notify::new());
    inner.idr_gate.lock().unwrap().insert(gid_720.clone(), idr_gate.clone());
    *host.arm_gid.lock().unwrap() = Some(gid_720.clone());

    let ea = e.clone();
    let ida = a.id.clone();
    let sw_a = tokio::spawn(async move { ea.set_golive_quality(&ida, DestinationQuality::Custom(q480)).await });
    timeout(Duration::from_secs(3), entered_stop.notified()).await.expect("A entered stop_encode_group");

    let prev_starts_1 = inner.start_counts.lock().unwrap().get(&gid_720).copied().unwrap_or(0);
    let eb = e.clone();
    let idb = b.id.clone();
    let q720_b = q720.clone();
    let sw_b = tokio::spawn(async move { eb.set_golive_quality(&idb, DestinationQuality::Custom(q720_b)).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    release_stop.notify_one();
    timeout(Duration::from_secs(3), async {
        while inner.start_counts.lock().unwrap().get(&gid_720).copied().unwrap_or(0) == prev_starts_1
            || !inner.started.lock().unwrap().contains_key(&gid_720)
        {
            tokio::task::yield_now().await;
        }
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B restarted gid_720 after stop_encode_group finished");
    idr_gate.notify_waiters();

    sw_a.await.unwrap().expect("A switch off gid_720 succeeds");
    sw_b.await.unwrap().expect("B switch onto gid_720 must not fail with HubEnded during teardown");
    assert_eq!(e.golive_running_group(&b.id).as_deref(), Some(gid_720.as_str()));
    assert!(e.active_encode_groups().iter().any(|g| g.id == gid_720), "gid_720 must remain in active_groups");
    assert!(inner.started.lock().unwrap().contains_key(&gid_720), "gid_720 must remain live on host");
    assert!(
        e.golive().hub(&gid_720).is_some_and(|h| h.live().borrow().on),
        "gid_720 hub must remain live after concurrent teardown + switch"
    );

    // 2. `reconcile_studio_groups` race: untick B so reconcile stops `gid_720`, while C concurrently switches onto `gid_720`.
    *host.arm_gid.lock().unwrap() = Some(gid_720.clone());
    let disable_b: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.enabled && d.id != b.id)).collect();
    let er = e.clone();
    let rec_task = tokio::spawn(async move { er.set_golive_destinations(disable_b).await });
    timeout(Duration::from_secs(3), entered_stop.notified()).await.expect("reconcile entered stop_encode_group");

    let prev_starts_2 = inner.start_counts.lock().unwrap().get(&gid_720).copied().unwrap_or(0);
    let ec = e.clone();
    let idc = c.id.clone();
    let sw_c = tokio::spawn(async move { ec.set_golive_quality(&idc, DestinationQuality::Custom(q720)).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    release_stop.notify_one();
    timeout(Duration::from_secs(3), async {
        while inner.start_counts.lock().unwrap().get(&gid_720).copied().unwrap_or(0) == prev_starts_2
            || !inner.started.lock().unwrap().contains_key(&gid_720)
        {
            tokio::task::yield_now().await;
        }
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("C restarted gid_720 after reconcile_studio_groups finished");
    idr_gate.notify_waiters();

    rec_task.await.unwrap().expect("untick B succeeds");
    sw_c.await.unwrap().expect("C switch onto gid_720 must not race reconcile_studio_groups teardown");
    assert_eq!(e.golive_running_group(&c.id).as_deref(), Some(gid_720.as_str()));
    assert!(e.active_encode_groups().iter().any(|g| g.id == gid_720));
    assert!(inner.started.lock().unwrap().contains_key(&gid_720));
    assert!(
        e.golive().hub(&gid_720).is_some_and(|h| h.live().borrow().on),
        "gid_720 hub must remain live after concurrent reconcile teardown + switch"
    );
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn failed_secondary_and_vertical_encode_groups_stay_blocked_across_reconcile() {
    use crate::golive::{Canvas, GROUP_MAIN, GROUP_VERTICAL};
    use ferrocast_testkit::rtmp::platform;

    // 1. Secondary main-canvas group fails startup (`kick_gid` in `fail_groups`):
    //    Kick must stay `blocked` with no `Waiting` forwarder, never enter `golive_running`, and never be retried by reconcile().
    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let s = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = s.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_on: false,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());
    e.note_studio_settings(&s);

    let (port_yt, _got_yt) = platform("yt-blk").await;
    let (port_kick, _got_kick) = platform("kick-blk").await;
    let (port_v, _got_v) = platform("vert-blk").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-blk".into())));
    let yt = e
        .add_golive_destination("YT", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-blk", Canvas::Main)
        .await
        .unwrap();
    let vert = e
        .add_golive_destination("Vert", &format!("rtmp://127.0.0.1:{port_v}/app"), "vert-blk", Canvas::Vertical)
        .await
        .unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == yt.id || d.id == "kick" || d.id == vert.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    let kick_gid = e.plan_golive_encodes(e.cached_pc_budget()).destinations["kick"].group_id.clone();
    assert_ne!(kick_gid, GROUP_MAIN);
    host.fail_groups.lock().unwrap().insert(kick_gid.clone());

    e.studio_stream(true).await.unwrap();
    e.reconcile().await;
    e.reconcile().await;

    let v = e.golive_view().await;
    let kick_v = v.destinations.iter().find(|d| d.id == "kick").unwrap();
    assert!(kick_v.blocked.is_some(), "failed Kick encode group must stay blocked across reconcile: {kick_v:?}");
    assert!(kick_v.health.is_none(), "blocked Kick must not park on dead hub as Waiting: {kick_v:?}");
    assert!(!e.golive_running().contains("kick"));
    assert_eq!(
        host.start_attempts.lock().unwrap().get(&kick_gid).copied(),
        Some(1),
        "reconcile must not retry failed secondary group startup"
    );

    let vert_v = v.destinations.iter().find(|d| d.id == vert.id).unwrap();
    assert!(vert_v.blocked.is_some(), "failed vertical encode group must stay blocked across reconcile: {vert_v:?}");
    assert!(vert_v.health.is_none(), "blocked vertical dest must not start forwarder: {vert_v:?}");
    assert!(!e.golive_running().contains(&vert.id));
    assert_eq!(
        host.start_attempts.lock().unwrap().get(GROUP_VERTICAL).copied(),
        Some(1),
        "reconcile must not retry failed vertical group startup"
    );

    e.studio_stream(false).await.unwrap();
    let after_stop = e.golive_view().await;
    assert!(after_stop.destinations.iter().all(|d| d.blocked.is_none()), "stop clears blocked reasons");
}

#[tokio::test]
async fn midstream_enable_custom_rtmp_rejects_peer_disruption_and_budget_overflow() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, Obs, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 30,
        video_kbps: 14_000,
        video_encoder: Some("ffmpeg_nvenc".into()),
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        hw_backend: Some("nvenc".into()),
        nvidia_driver_major: Some(525), // 3 raw -> 2 effective under auto_quality
        cpu_cores: 8,
        host_sig: "win-nv-525".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());
    e.set_golive_auto_quality(true).await.unwrap();

    let (port_yt, mut got_yt) = platform("yt-ms").await;
    let (port_kick, mut got_kick) = platform("kick-ms").await;
    let (port_c, _got_c) = platform("cust-c-ms").await;
    let (port_d, _got_d) = platform("cust-d-ms").await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{port_kick}/app"), "kick-ms".into())));
    let yt = e
        .add_golive_destination("YT", &format!("rtmp://127.0.0.1:{port_yt}/app"), "yt-ms", Canvas::Main)
        .await
        .unwrap();
    let c_coalesce = e
        .add_golive_destination("CustomCoal", &format!("rtmp://127.0.0.1:{port_c}/app"), "cust-c-ms", Canvas::Main)
        .await
        .unwrap();
    let d_extra = e
        .add_golive_destination("CustomExtra", &format!("rtmp://127.0.0.1:{port_d}/app"), "cust-d-ms", Canvas::Main)
        .await
        .unwrap();

    // Disable C & D before configuring their custom qualities on the 2-session host.
    let only_yt_kick: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == yt.id || d.id == "kick")).collect();
    e.set_golive_destinations(only_yt_kick).await.unwrap();
    e.set_golive_quality(
        &c_coalesce.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 30,
            kbps: 6_000,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    e.set_golive_quality(
        &d_extra.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 3_500,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "yt-ms"), (&mut got_kick, "kick-ms")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let kick_group_before = e.golive_running_group("kick").unwrap();
    assert_eq!(host.started.lock().unwrap().len(), 2);

    // 1. Ticking CustomCoal (1080p30@6000) coalesces live Kick (1080p30@8000) & adds a 3rd transient encoder -> must fail 422.
    let with_c: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == yt.id || d.id == "kick" || d.id == c_coalesce.id))
        .collect();
    let err_c = e.set_golive_destinations_checked(with_c, Default::default()).await.unwrap_err();
    assert!(
        matches!(err_c, EngineError::Refused { status: 422, .. }),
        "enabling coalescing custom RTMP mid-stream must return 422, got {err_c:?}"
    );
    assert_eq!(e.golive_running_group("kick").as_deref(), Some(kick_group_before.as_str()));
    assert!(!matches!(got_kick.try_recv(), Ok(Got::Closed)), "Kick must stay live untouched");

    // 2. Ticking CustomExtra (720p30@3500) spawns a 3rd group exceeding 2-session cap -> must fail 422.
    let with_d: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == yt.id || d.id == "kick" || d.id == d_extra.id))
        .collect();
    let err_d = e.set_golive_destinations_checked(with_d, Default::default()).await.unwrap_err();
    assert!(
        matches!(err_d, EngineError::Refused { status: 422, .. }),
        "enabling non-coalescing custom RTMP over budget must return 422, got {err_d:?}"
    );
    assert_eq!(host.started.lock().unwrap().len(), 2, "must not launch 3rd encoder over budget");
    e.studio_stream(false).await.unwrap();

    // 3. OBS live mode: ticking a pre-configured Custom destination must return 422 golive_needs_new_encode.
    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == yt.id)).collect();
    e.set_golive_destinations(only_yt).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let ingest_port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(ingest_port, "live", &key).await.unwrap();
    eventually_async(|| async { e.golive_running().contains(&yt.id) }).await;

    let with_obs_d: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == yt.id || d.id == d_extra.id))
        .collect();
    let err_obs = e.set_golive_destinations_checked(with_obs_d, Default::default()).await.unwrap_err();
    assert!(
        matches!(err_obs, EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. }),
        "OBS live cannot spawn Custom encode when ticked mid-stream: {err_obs:?}"
    );
}

#[tokio::test]
async fn deferred_studio_reshape_keeps_live_main_group_key_unmodified() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let init_settings = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = init_settings.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("def-a").await;
    let (pb, mut got_b) = platform("def-b").await;
    let _a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "def-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "def-b", Canvas::Main).await.unwrap();
    e.set_golive_quality(
        &b.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 3_000,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "def-a"), (&mut got_b, "def-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    *host.defer_reshape.lock().unwrap() = Some("The next stream uses the new settings.".into());
    e.studio_set_settings(crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 12_000,
        ..init_settings
    })
    .await
    .unwrap();

    let active_main = e.active_encode_groups().into_iter().find(|g| g.id == GROUP_MAIN).unwrap();
    assert_eq!(
        (active_main.key.width, active_main.key.height, active_main.key.fps, active_main.key.kbps),
        (1920, 1080, 60, 6_000),
        "deferred reshape must keep live GROUP_MAIN key untouched: {:?}",
        active_main.key
    );
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn quality_options_and_put_reject_mismatched_live_main_and_vertical_group_keys() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, GROUP_VERTICAL, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    // 1. Live GROUP_MAIN mismatch (`defer_reshape` or Pass-2 cap): both `quality-options` and `PUT` must disallow `SameAsMain`.
    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    let init_s = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.settings.lock().unwrap() = init_s.clone();
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_on: true,
        vertical_size: Some((1080, 1920)),
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("mm-a").await;
    let (pb, mut got_b) = platform("mm-b").await;
    let _a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "mm-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "mm-b", Canvas::Main).await.unwrap();
    e.set_golive_quality(
        &b.id,
        DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 3_000,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "mm-a"), (&mut got_b, "mm-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    // Mutate live GROUP_MAIN spec key (e.g. Pass-2 cap at 4,000 while StudioSettings is 6,000).
    let mut main_spec = e.active_encode_groups().into_iter().find(|g| g.id == GROUP_MAIN).unwrap();
    main_spec.key.kbps = 4_000;
    e.set_active_group_for_test(main_spec);

    let opts = e.golive_quality_options().await;
    let same_main_opt =
        opts.destinations[&b.id].options.iter().find(|o| o.quality == DestinationQuality::SameAsMain).unwrap();
    let put_main = e.set_golive_quality(&b.id, DestinationQuality::SameAsMain).await;
    assert_eq!(same_main_opt.allowed, put_main.is_ok(), "options and PUT must agree on GROUP_MAIN mismatch");
    assert!(!same_main_opt.allowed, "mismatched GROUP_MAIN key must be disallowed in quality-options");
    assert!(
        matches!(put_main, Err(EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. })),
        "{put_main:?}"
    );
    e.studio_stream(false).await.unwrap();

    // 2. Live GROUP_VERTICAL mismatch on vertical-only stream: both `quality-options` and `PUT` must disallow `SameAsMain`.
    let (pv1, mut got_v1) = platform("mv-1").await;
    let (pv2, mut got_v2) = platform("mv-2").await;
    let v1 =
        e.add_golive_destination("V1", &format!("rtmp://127.0.0.1:{pv1}/app"), "mv-1", Canvas::Vertical).await.unwrap();
    let v2 =
        e.add_golive_destination("V2", &format!("rtmp://127.0.0.1:{pv2}/app"), "mv-2", Canvas::Vertical).await.unwrap();
    let only_v: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == v1.id || d.id == v2.id)).collect();
    e.set_golive_destinations(only_v).await.unwrap();
    e.set_golive_quality(
        &v2.id,
        DestinationQuality::Custom(CustomQuality {
            width: 540,
            height: 960,
            fps: 30,
            kbps: 2_000,
            codec: Some(VideoCodec::H264),
        }),
    )
    .await
    .unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_v1, "mv-1"), (&mut got_v2, "mv-2")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let mut vert_spec = e.active_encode_groups().into_iter().find(|g| g.id == GROUP_VERTICAL).unwrap();
    vert_spec.key.kbps = 3_500;
    e.set_active_group_for_test(vert_spec);

    let v_opts = e.golive_quality_options().await;
    let same_v_opt =
        v_opts.destinations[&v2.id].options.iter().find(|o| o.quality == DestinationQuality::SameAsMain).unwrap();
    let put_v = e.set_golive_quality(&v2.id, DestinationQuality::SameAsMain).await;
    assert_eq!(same_v_opt.allowed, put_v.is_ok(), "options and PUT must agree on GROUP_VERTICAL mismatch");
    assert!(!same_v_opt.allowed, "mismatched GROUP_VERTICAL key must be disallowed in quality-options");
    assert!(
        matches!(put_v, Err(EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. })),
        "{put_v:?}"
    );
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn shared_switch_group_cleans_up_after_commit_untick_sibling_fail_and_task_abort() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct GatedYtConnector {
        url: String,
        key: String,
        entered_resolve: Arc<tokio::sync::Notify>,
        release_resolve: Arc<tokio::sync::Notify>,
        fail_after_gate: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for GatedYtConnector {
        fn platform(&self) -> Platform {
            Platform::Youtube
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            Default::default()
        }
        async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
        async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            if self.fail_after_gate.load(std::sync::atomic::Ordering::Acquire) {
                self.entered_resolve.notify_one();
                self.release_resolve.notified().await;
                return Err(ferrocast_connector::ConnectorError::Network {
                    platform: Platform::Youtube,
                    message: "gated yt fail".into(),
                });
            }
            Ok(ferrocast_connector::Ingest { url: self.url.clone(), key: self.key.clone(), backup: None, note: None })
        }
    }

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("lk-a").await;
    let (pyt, mut got_yt) = platform("lk-yt").await;
    let entered_resolve = Arc::new(tokio::sync::Notify::new());
    let release_resolve = Arc::new(tokio::sync::Notify::new());
    let yt_conn = Arc::new(GatedYtConnector {
        url: format!("rtmp://127.0.0.1:{pyt}/app"),
        key: "lk-yt".into(),
        entered_resolve: entered_resolve.clone(),
        release_resolve: release_resolve.clone(),
        fail_after_gate: std::sync::atomic::AtomicBool::new(false),
    });
    e.register_connector(yt_conn.clone());

    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "lk-a", Canvas::Main).await.unwrap();
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id || d.id == "youtube")).collect();
    e.set_golive_destinations(order).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "lk-a"), (&mut got_yt, "lk-yt")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    // 1. Serialized switch failure + sibling commit + untick cleanup:
    //    YouTube blocks in `resolve()` under `group_ops` while A is queued to switch to shared `q720`.
    //    Once YouTube fails `resolve()`, G is aborted immediately; A then acquires `group_ops`, starts G,
    //    commits, and unticking A stops and cleans up G with zero leaks!
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 4_500, codec: Some(VideoCodec::H264) };
    let gid_720 = {
        let mut input = e.golive_planner_input(e.cached_pc_budget());
        input.destinations.iter_mut().find(|d| d.id == a.id).unwrap().quality =
            DestinationQuality::Custom(q720.clone());
        crate::golive::plan_encodes(&input).destinations[&a.id].group_id.clone()
    };
    yt_conn.fail_after_gate.store(true, std::sync::atomic::Ordering::Release);

    let eb = e.clone();
    let qb = q720.clone();
    let task_b = tokio::spawn(async move { eb.set_golive_quality("youtube", DestinationQuality::Custom(qb)).await });
    timeout(Duration::from_secs(3), entered_resolve.notified()).await.expect("YouTube entered gated resolve");

    let ea = e.clone();
    let aid = a.id.clone();
    let qa = q720.clone();
    let task_a = tokio::spawn(async move { ea.set_golive_quality(&aid, DestinationQuality::Custom(qa)).await });
    release_resolve.notify_waiters();
    assert!(task_b.await.unwrap().is_err(), "gated YouTube switch must fail");
    task_a.await.unwrap().unwrap();
    assert!(host.started.lock().unwrap().contains_key(&gid_720), "committed switch A keeps shared group alive");
    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt).await.unwrap();
    assert!(!host.started.lock().unwrap().contains_key(&gid_720), "shared group G must be stopped after unticking A");
    assert!(!e.active_encode_groups().iter().any(|g| g.id == gid_720));

    // 2. Cancelled HTTP / aborted switch future leak (no manual reconcile):
    let q540 = CustomQuality { width: 960, height: 540, fps: 30, kbps: 2_000, codec: Some(VideoCodec::H264) };
    let gid_540 = {
        let mut input = e.golive_planner_input(e.cached_pc_budget());
        input.destinations.iter_mut().find(|d| d.id == "youtube").unwrap().quality =
            DestinationQuality::Custom(q540.clone());
        crate::golive::plan_encodes(&input).destinations["youtube"].group_id.clone()
    };
    let gate_540 = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(gid_540.clone(), gate_540);
    let e_abort = e.clone();
    let abort_task =
        tokio::spawn(async move { e_abort.set_golive_quality("youtube", DestinationQuality::Custom(q540)).await });
    eventually_async(|| async { host.started.lock().unwrap().contains_key(&gid_540) }).await;
    abort_task.abort();
    let _ = abort_task.await;

    eventually_async(|| async {
        !host.started.lock().unwrap().contains_key(&gid_540)
            && !e.active_encode_groups().iter().any(|g| g.id == gid_540)
    })
    .await;
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn stopping_studio_stream_clears_cached_group_stats_in_ws_golive_snapshot() {
    use crate::golive::Canvas;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host);

    let (p, mut got) = platform("ws-gs").await;
    let _d = e.add_golive_destination("YT", &format!("rtmp://127.0.0.1:{p}/app"), "ws-gs", Canvas::Main).await.unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "ws-gs") {
            break;
        }
    }

    e.note_golive_group_stats(Some(json!({ "main": { "encode_ms": 4.2, "dropped": 3 } })));
    let mut live_rx = e.subscribe_live();
    e.studio_stream(false).await.unwrap();

    let mut offline_golive = None;
    for _ in 0..20 {
        let frame = timeout(Duration::from_secs(2), live_rx.recv()).await.expect("live frame").expect("open");
        let parsed: serde_json::Value = serde_json::from_str(&frame.text).unwrap();
        if parsed["type"] == "golive" && parsed["data"]["live"] == false {
            offline_golive = Some(parsed["data"].clone());
            break;
        }
    }
    let data = offline_golive.expect("offline golive WS frame emitted on stream stop");
    for grp in data["encode_groups"].as_array().unwrap() {
        assert!(grp["encode_ms"].is_null(), "stale encode_ms emitted on stream stop: {grp:?}");
        assert!(grp["dropped_frames"].is_null(), "stale dropped_frames emitted on stream stop: {grp:?}");
    }
}

#[tokio::test]
async fn golive_upload_check_bills_per_destination_planned_bitrates() {
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::platform;

    let (e, _dir) = engine().await;
    e.note_host_budget_info(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.note_studio_settings(&crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        audio_kbps: 160,
        ..Default::default()
    });

    let (p_yt, _got_yt) = platform("yt-uc").await;
    let (p_kick, _got_kick) = platform("kick-uc").await;
    let (p_tw, _got_tw) = platform("tw-uc?bandwidthtest=true").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "yt-uc".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "kick-uc".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "tw-uc".into(),
    )));

    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), matches!(d.id.as_str(), "youtube" | "kick" | "twitch")))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    let uc = e.golive_upload_check().await.unwrap();
    let need_of = |id: &str| uc.rows.iter().find(|r| r.id == id).map(|r| r.need_kbps);
    assert_eq!(need_of("youtube"), Some(18_160));
    assert_eq!(need_of("kick"), Some(8_160), "Kick SameAsMain must bill fitted 8,160 kbps, not 18,160: {:?}", uc.rows);
    assert_eq!(need_of("twitch"), Some(6_160), "Twitch SameAsMain must bill fitted 6,160 kbps: {:?}", uc.rows);
}

#[tokio::test]
async fn legacy_light_options_current_avoids_same_as_main_alias_and_same_quality_put_clears_route_light() {
    use crate::golive::{Canvas, DestinationQuality, GROUP_LIGHT, GROUP_MAIN};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(host);

    let (p, mut got) = platform("rl-9").await;
    let d = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{p}/app"), "rl-9", Canvas::Main).await.unwrap();

    // 1. Legacy v3.7 config (`light: true, quality: SameAsMain`): `options.current` must not match non-light `SameAsMain`.
    let mut cfg = e.golive_config();
    cfg.destinations.iter_mut().find(|x| x.id == d.id).unwrap().light = true;
    e.save_golive(cfg.clone()).await.unwrap();
    let opts = e.golive_quality_options().await;
    let d_opts = &opts.destinations[&d.id];
    assert_ne!(
        d_opts.current,
        DestinationQuality::SameAsMain,
        "legacy light: true destination must not alias non-light SameAsMain option: {d_opts:?}"
    );
    assert!(
        d_opts.options.iter().any(|o| o.quality == d_opts.current),
        "legacy light current should match an option: {d_opts:?}"
    );

    // Clear legacy light and start Studio live, then put destination onto automatic `route_light`.
    cfg.destinations.iter_mut().find(|x| x.id == d.id).unwrap().light = false;
    e.save_golive(cfg).await.unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "rl-9") {
            break;
        }
    }

    e.mark_route_light_for_test(&d.id);
    e.reconcile().await;
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(GROUP_LIGHT));

    // Selecting `SameAsMain` (or `set_golive_light(false)`) must clear `route_light` and restore GROUP_MAIN.
    e.set_golive_quality(&d.id, DestinationQuality::SameAsMain).await.unwrap();
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(GROUP_MAIN));
    let d_after = e.golive_config().destinations.into_iter().find(|x| x.id == d.id).unwrap();
    assert!(!e.wants_light_now(&d_after));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn obs_live_put_custom_quality_returns_422_before_canvas_fps_validation() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::Obs;

    let (e, _dir) = engine().await;
    let c1 = e.add_golive_destination("C1", "rtmp://127.0.0.1:9/app", "obs-c1", Canvas::Main).await.unwrap();
    let (server, key) = e.golive_ingest().await.unwrap();
    let ingest_port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(ingest_port, "live", &key).await.unwrap();
    eventually_async(|| async { e.golive_stream_running() }).await;

    // Studio stopped -> `studio_video == None` -> default canvas_fps = 30.
    // Sending 720p60 Custom while OBS is live must return 422 golive_needs_new_encode, not 400 Invalid!
    let err = e
        .set_golive_quality(
            &c1.id,
            DestinationQuality::Custom(CustomQuality {
                width: 1280,
                height: 720,
                fps: 60,
                kbps: 4_500,
                codec: Some(VideoCodec::H264),
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. }),
        "OBS live Custom 60fps must return 422 golive_needs_new_encode, got {err:?}"
    );
}

#[tokio::test]
async fn studio_stop_cancels_pending_switch_in_resolve_and_clears_encode_groups() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct StoppableMultiHost {
        inner: Arc<MultiGroupHost>,
        running: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for StoppableMultiHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            crate::studio_host::StudioStatus {
                available: true,
                running: self.running.load(std::sync::atomic::Ordering::Acquire),
                ..Default::default()
            }
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.running.store(true, std::sync::atomic::Ordering::Release);
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.running.store(false, std::sync::atomic::Ordering::Release);
            self.inner.stream_stop().await?;
            Ok(self.status().await)
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            self.inner.act(act).await
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            self.inner.start_encode_group(spec, hub).await
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
            self.inner.live_group_key(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    #[derive(Debug)]
    struct BlockingResolveYt {
        url: String,
        key: String,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        gate_next: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for BlockingResolveYt {
        fn platform(&self) -> Platform {
            Platform::Youtube
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            Default::default()
        }
        async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
        async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            if self.gate_next.load(std::sync::atomic::Ordering::Acquire) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(ferrocast_connector::Ingest { url: self.url.clone(), key: self.key.clone(), backup: None, note: None })
        }
    }

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    let host = Arc::new(StoppableMultiHost { inner: inner.clone(), running: std::sync::atomic::AtomicBool::new(true) });
    e.set_studio_host(host);

    let (p_yt, mut got_yt) = platform("stop-yt").await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let yt_conn = Arc::new(BlockingResolveYt {
        url: format!("rtmp://127.0.0.1:{p_yt}/app"),
        key: "stop-yt".into(),
        entered: entered.clone(),
        release: release.clone(),
        gate_next: std::sync::atomic::AtomicBool::new(false),
    });
    e.register_connector(yt_conn.clone());

    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt).await.unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_yt.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "stop-yt")
        {
            break;
        }
    }

    yt_conn.gate_next.store(true, std::sync::atomic::Ordering::Release);
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let e_sw = e.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality("youtube", DestinationQuality::Custom(q720)).await });
    timeout(Duration::from_secs(3), entered.notified()).await.expect("switch entered resolve");

    // studio_stop must cancel the pending switch inside resolve() immediately and clear all encode groups.
    timeout(Duration::from_secs(3), e.studio_stop())
        .await
        .expect("studio_stop must not wait on stalled resolve")
        .unwrap();
    let sw_err = timeout(Duration::from_secs(3), sw)
        .await
        .expect("switch must abort on studio_stop without waiting for release")
        .unwrap()
        .unwrap_err();
    assert!(matches!(sw_err, EngineError::Refused { code: "golive_switch_cancelled", .. }), "{sw_err:?}");
    assert!(e.active_encode_groups().is_empty(), "studio_stop must clear active encode groups");
    assert!(!e.golive_uses_encode_groups());
    assert!(inner.started.lock().unwrap().is_empty(), "studio_stop must stop all host encode groups");
    assert!(e.golive_running().is_empty(), "studio_stop must stop all running destinations");
    let _ = Canvas::Main;
}

#[derive(Debug)]
struct SlowStopHost {
    inner: Arc<MultiGroupHost>,
    studio_running: std::sync::atomic::AtomicBool,
    arm_gid: std::sync::Mutex<Option<String>>,
    entered_stop: Arc<tokio::sync::Notify>,
    release_stop: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::studio_host::StudioHost for SlowStopHost {
    async fn status(&self) -> crate::studio_host::StudioStatus {
        crate::studio_host::StudioStatus {
            available: true,
            running: self.studio_running.load(std::sync::atomic::Ordering::Acquire),
            ..Default::default()
        }
    }
    async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
        self.studio_running.store(true, std::sync::atomic::Ordering::Release);
        Ok(self.status().await)
    }
    async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
        self.studio_running.store(false, std::sync::atomic::Ordering::Release);
        Ok(self.status().await)
    }
    async fn kinds(&self) -> Result<serde_json::Value, String> {
        self.inner.kinds().await
    }
    async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
        self.inner.options(kind).await
    }
    async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
        self.inner.preview(rect).await
    }
    async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
        self.inner.defaults(kind).await
    }
    async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
        self.inner.act(act).await
    }
    async fn settings(&self) -> crate::studio_host::StudioSettings {
        self.inner.settings().await
    }
    async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
        self.inner.set_settings(s).await
    }
    async fn stats(&self) -> Result<serde_json::Value, String> {
        self.inner.stats().await
    }
    async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
        self.inner.record(on).await
    }
    async fn record_pause(&self, on: bool) -> Result<(), String> {
        self.inner.record_pause(on).await
    }
    async fn monitoring(&self) -> Result<serde_json::Value, String> {
        self.inner.monitoring().await
    }
    async fn stream_start(
        &self,
        server: String,
        key: String,
        t: Vec<(u32, u32, u32)>,
    ) -> Result<serde_json::Value, String> {
        self.inner.stream_start(server, key, t).await
    }
    async fn stream_stop(&self) -> Result<(), String> {
        self.inner.stream_stop().await
    }
    async fn stream_stats(&self) -> Result<serde_json::Value, String> {
        self.inner.stream_stats().await
    }
    async fn obs_collections(&self) -> Result<serde_json::Value, String> {
        self.inner.obs_collections().await
    }
    async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
        self.inner.import_obs(id).await
    }
    fn supports_encode_groups(&self) -> bool {
        true
    }
    async fn start_encode_group(
        &self,
        spec: crate::studio_host::EncodeGroupSpec,
        hub: Arc<ferrocast_golive::Hub>,
    ) -> Result<bool, String> {
        self.inner.start_encode_group(spec, hub).await
    }
    async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
        let pause = {
            let mut g = self.arm_gid.lock().unwrap();
            if g.as_deref() == Some(id) {
                *g = None;
                true
            } else {
                false
            }
        };
        if pause {
            self.entered_stop.notify_one();
            self.release_stop.notified().await;
        }
        self.inner.stop_encode_group(id).await
    }
    async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
        self.inner.abort_encode_group(id).await
    }
    async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
        self.inner.live_group_key(id).await
    }
    async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
        self.inner.group_stats().await
    }
    async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
        self.inner.host_budget_info().await
    }
}

#[tokio::test]
async fn set_golive_destinations_does_not_stall_config_lock_behind_encoder_io() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    let entered_stop = Arc::new(tokio::sync::Notify::new());
    let release_stop = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SlowStopHost {
        inner: inner.clone(),
        studio_running: std::sync::atomic::AtomicBool::new(true),
        arm_gid: std::sync::Mutex::new(None),
        entered_stop: entered_stop.clone(),
        release_stop: release_stop.clone(),
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("cfg-a").await;
    let (pb, _got_b) = platform("cfg-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "cfg-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "cfg-b", Canvas::Main).await.unwrap();
    let only_a: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id)).collect();
    e.set_golive_destinations(only_a).await.unwrap();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720)).await.unwrap();

    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_a.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "cfg-a")
        {
            break;
        }
    }
    let gid_720 = e.golive_running_group(&a.id).unwrap();
    *host.arm_gid.lock().unwrap() = Some(gid_720);

    // Pause a switch inside stop_encode_group while holding the group lifecycle lock.
    let e_sw = e.clone();
    let a_id = a.id.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality(&a_id, DestinationQuality::SameAsMain).await });
    timeout(Duration::from_secs(3), entered_stop.notified()).await.expect("switch entered stop_encode_group");

    // Start set_golive_destinations enabling B (which waits on the group lifecycle lock): it must NOT hold config_lock!
    let enable_ab: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id || d.id == b.id)).collect();
    let e_dest = e.clone();
    let dest_task = tokio::spawn(async move { e_dest.set_golive_destinations(enable_ab).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Unrelated config_lock writer must finish immediately without waiting for release_stop!
    timeout(Duration::from_secs(3), e.set_golive_aitum_ok(true))
        .await
        .expect("config_lock must not stall behind encoder I/O")
        .unwrap();

    release_stop.notify_waiters();
    sw.await.unwrap().unwrap();
    dest_task.await.unwrap().unwrap();
    e.studio_stream(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_parallel_switch_reconcile_untick_and_stop_under_group_ops() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2-max".into(),
        ..Default::default()
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("stress-yt").await;
    let (p_a, mut got_a) = platform("stress-a").await;
    let (p_b, mut got_b) = platform("stress-b").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "stress-yt".into(),
    )));
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{p_a}/app"), "stress-a", Canvas::Main).await.unwrap();
    let b =
        e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{p_b}/app"), "stress-b", Canvas::Main).await.unwrap();

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };

    const ITERATIONS: usize = 25;
    for iter in 0..ITERATIONS {
        // Ensure all three destinations start enabled and on SameAsMain before each stream.
        e.set_golive_quality(&a.id, DestinationQuality::SameAsMain).await.unwrap();
        e.set_golive_quality(&b.id, DestinationQuality::SameAsMain).await.unwrap();
        let enable_all: Vec<(String, bool)> = e
            .golive_config()
            .destinations
            .into_iter()
            .map(|d| (d.id.clone(), d.id == "youtube" || d.id == a.id || d.id == b.id))
            .collect();
        e.set_golive_destinations(enable_all.clone()).await.unwrap();
        while got_yt.try_recv().is_ok() {}
        while got_a.try_recv().is_ok() {}
        while got_b.try_recv().is_ok() {}

        let starts_before = host.start_counts.lock().unwrap().get(GROUP_MAIN).copied().unwrap_or(0);
        e.studio_stream(true).await.unwrap();
        for (rx, want) in [(&mut got_yt, "stress-yt"), (&mut got_a, "stress-a"), (&mut got_b, "stress-b")] {
            loop {
                if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want)
                {
                    break;
                }
            }
        }
        while got_yt.try_recv().is_ok() {}

        let sw_q_a = if iter % 2 == 0 {
            DestinationQuality::Custom(q720.clone())
        } else {
            DestinationQuality::Custom(q480.clone())
        };
        let sw_q_b = if iter % 2 == 0 {
            DestinationQuality::Custom(q480.clone())
        } else {
            DestinationQuality::Custom(q720.clone())
        };
        let untick_a: Vec<(String, bool)> = e
            .golive_config()
            .destinations
            .into_iter()
            .map(|d| (d.id.clone(), d.id == "youtube" || d.id == b.id))
            .collect();

        // Phase A (while live): fire switch(a), switch(b), reconcile, untick(a) in parallel (NOT stop yet).
        // YouTube (untouched sibling on GROUP_MAIN) must experience zero disconnects (`Got::Closed`).
        while got_yt.try_recv().is_ok() {}
        let (e1, e2, e3, e4) = (e.clone(), e.clone(), e.clone(), e.clone());
        let (aid_a, bid_a, qa_a, qb_a) = (a.id.clone(), b.id.clone(), sw_q_a.clone(), sw_q_b.clone());
        let t_sw_a = tokio::spawn(async move { e1.set_golive_quality(&aid_a, qa_a).await });
        let t_sw_b = tokio::spawn(async move { e2.set_golive_quality(&bid_a, qb_a).await });
        let t_rec = tokio::spawn(async move { e3.reconcile().await });
        let untick_a_clone = untick_a.clone();
        let t_untick = tokio::spawn(async move { e4.set_golive_destinations(untick_a_clone).await });
        let (r_a, r_b, r_rec, r_untick) = tokio::join!(t_sw_a, t_sw_b, t_rec, t_untick);
        let _ = r_a.unwrap();
        r_b.unwrap().unwrap();
        r_rec.unwrap();
        r_untick.unwrap().unwrap();
        let mut yt_closed_live = 0usize;
        while let Ok(msg) = got_yt.try_recv() {
            if matches!(msg, Got::Closed) {
                yt_closed_live += 1;
            }
        }
        assert_eq!(
            yt_closed_live, 0,
            "iteration {iter}: YouTube on GROUP_MAIN must have 0 disconnects during live switch+reconcile+untick"
        );

        // Phase B: re-enable A, wait for publish, then fire switch(a) + switch(b) + reconcile + untick(a) + Stop in parallel!
        e.set_golive_destinations(enable_all.clone()).await.unwrap();
        loop {
            if matches!(timeout(Duration::from_secs(3), got_a.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "stress-a")
            {
                break;
            }
        }

        let (e1, e2, e3, e4, e5) = (e.clone(), e.clone(), e.clone(), e.clone(), e.clone());
        let (aid, bid) = (a.id.clone(), b.id.clone());
        let t_sw_a = tokio::spawn(async move { e1.set_golive_quality(&aid, sw_q_a).await });
        let t_sw_b = tokio::spawn(async move { e2.set_golive_quality(&bid, sw_q_b).await });
        let t_rec = tokio::spawn(async move { e3.reconcile().await });
        let t_untick = tokio::spawn(async move { e4.set_golive_destinations(untick_a).await });
        let t_stop = tokio::spawn(async move {
            if iter % 3 == 0 {
                tokio::task::yield_now().await;
            }
            e5.studio_stream(false).await
        });

        let (r_a, r_b, r_rec, r_untick, r_stop) = tokio::join!(t_sw_a, t_sw_b, t_rec, t_untick, t_stop);
        let _ = r_a.unwrap();
        let _ = r_b.unwrap();
        r_rec.unwrap();
        r_untick.unwrap().unwrap();
        r_stop.unwrap().unwrap();

        // While Stop may have caused YouTube's final Got::Closed, main must have started ONLY once for this stream.
        let starts_after = host.start_counts.lock().unwrap().get(GROUP_MAIN).copied().unwrap_or(0);
        assert_eq!(
            starts_after - starts_before,
            1,
            "iteration {iter}: GROUP_MAIN must start exactly once per stream and never restart"
        );
        assert!(
            host.started.lock().unwrap().is_empty(),
            "iteration {iter}: no leaked groups on host after Stop: {:?}",
            host.started.lock().unwrap().keys().collect::<Vec<_>>()
        );
        assert!(e.active_encode_groups().is_empty(), "iteration {iter}: active_groups must be empty after Stop");
        assert!(e.golive_running().is_empty(), "iteration {iter}: golive_running must be empty after Stop");

        // Verify next stream publishes cleanly to every enabled destination (no broken stale Arc<Hub>!).
        let enabled_ids: Vec<String> =
            e.golive_config().destinations.into_iter().filter(|d| d.enabled).map(|d| d.id).collect();
        assert!(!enabled_ids.contains(&a.id), "iteration {iter}: unticked A must remain disabled");
    }

    // Final fresh stream check after stress loop: YouTube and B still connect and publish cleanly, and A stays stopped.
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "stress-yt"), (&mut got_b, "stress-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    assert!(!e.golive_running().contains(&a.id));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn switch_commit_serialized_with_studio_stop_and_remove_leaves_no_orphans() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 9_000,
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    let entered_stop = Arc::new(tokio::sync::Notify::new());
    let release_stop = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SlowStopHost {
        inner: inner.clone(),
        studio_running: std::sync::atomic::AtomicBool::new(true),
        arm_gid: std::sync::Mutex::new(None),
        entered_stop: entered_stop.clone(),
        release_stop: release_stop.clone(),
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("f52-a").await;
    let (pb, mut got_b) = platform("f52-b").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "f52-a", Canvas::Main).await.unwrap();
    let b = e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "f52-b", Canvas::Main).await.unwrap();
    let only_ab: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id || d.id == b.id)).collect();
    e.set_golive_destinations(only_ab).await.unwrap();

    // Case 1: switch commits replacement forwarder into `g.running`, enters `stop_group_if_unused_locked`
    // under `group_ops`; `studio_stop()` fires before `group_ops` drops -> zero leaked forwarders/groups.
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720.clone())).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "f52-a"), (&mut got_b, "f52-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let gid_720 = e.golive_running_group(&a.id).unwrap();
    *host.arm_gid.lock().unwrap() = Some(gid_720.clone());

    let e_sw = e.clone();
    let aid = a.id.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality(&aid, DestinationQuality::SameAsMain).await });
    timeout(Duration::from_secs(3), entered_stop.notified()).await.expect("entered stop_encode_group post-commit");

    let e_stop = e.clone();
    let stop_task = tokio::spawn(async move { e_stop.studio_stop().await });
    release_stop.notify_waiters();
    sw.await.unwrap().unwrap();
    stop_task.await.unwrap().unwrap();

    assert!(inner.started.lock().unwrap().is_empty(), "studio_stop must clear every group including new commit");
    assert!(e.active_encode_groups().is_empty());
    assert!(e.golive_running().is_empty());

    // Case 2: same post-commit window raced against `remove_golive_destination`: deleted dest's new forwarder stops cleanly.
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720)).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_a, "f52-a"), (&mut got_b, "f52-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    *host.arm_gid.lock().unwrap() = Some(gid_720);
    let e_sw2 = e.clone();
    let aid2 = a.id.clone();
    let sw2 = tokio::spawn(async move { e_sw2.set_golive_quality(&aid2, DestinationQuality::SameAsMain).await });
    timeout(Duration::from_secs(3), entered_stop.notified()).await.expect("entered stop_encode_group case 2");

    let e_rm = e.clone();
    let aid_rm = a.id.clone();
    let rm_task = tokio::spawn(async move { e_rm.remove_golive_destination(&aid_rm).await });
    release_stop.notify_waiters();
    sw2.await.unwrap().unwrap();
    rm_task.await.unwrap().unwrap();

    assert!(!e.golive_running().contains(&a.id), "deleted destination's newly committed forwarder must stop");
    assert_eq!(e.golive_running_group(&b.id).as_deref(), Some(GROUP_MAIN));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f54_paused_peer_pass2_switch_and_check_midstream_reject_422_and_keep_kick_group() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform, platform_rejecting_above};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 12,
        nvidia_driver_major: Some(555),
        nvidia_driver_minor: Some(0),
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=555".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 360.0,
        max_sessions_ok: Some(5),
        measured_at: 1,
        host_sig: "win|nv=555".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("f54-yt").await;
    let (p_kick, mut got_kick, _) =
        platform_rejecting_above("f54-kick", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    let (p_a, mut got_a) = platform("f54-a").await;
    let (p_c, _got_c) = platform("f54-c").await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "f54-yt".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "f54-kick".into(),
    )));
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{p_a}/app"), "f54-a", Canvas::Main).await.unwrap();
    let c = e.add_golive_destination("C", &format!("rtmp://127.0.0.1:{p_c}/app"), "f54-c", Canvas::Main).await.unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick" || d.id == a.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "f54-yt"), (&mut got_kick, "f54-kick"), (&mut got_a, "f54-a")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let kick_gid = e.golive_running_group("kick").unwrap();
    assert_ne!(kick_gid, GROUP_MAIN);

    // Pause Kick via upload protection while YouTube and A stay on 1440p GROUP_MAIN.
    e.pause("kick".into(), "YouTube".into());
    assert!(!e.golive_running().contains("kick"));

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    // GET quality-options must mark 720p30 disallowed on A because Pass-2 would collapse paused Kick onto 1440p main.
    let opts = e.golive_quality_options().await;
    let a_opts = opts.destinations.get(&a.id).expect("A options");
    if let Some(opt_720) = a_opts.options.iter().find(|o| o.quality == DestinationQuality::Custom(q720.clone())) {
        assert!(!opt_720.allowed, "quality-options must disallow Pass-2 peer-collapsing 720p30 switch");
    }

    // PUT .../quality on A must refuse (422) rather than launching 720p30 + retiring g_kick.
    let err = e.set_golive_quality(&a.id, DestinationQuality::Custom(q720.clone())).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 422, .. }), "expected 422 refusal, got {err:?}");
    assert!(host.started.lock().unwrap().contains_key(&kick_gid), "g_kick must remain started while Kick is paused");

    // Pre-configured Custom C ticked mid-stream must also refuse (422) via check_midstream.
    let c_order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == c.id || d.enabled)).collect();
    // Pre-configure C to 720p30 before testing midstream enable? First untick A cleanly doesn't change test, instead
    // add pre-configured D offline? Wait: C was added while live (`enabled = false`), configure C offline? No, set C quality first?
    // Wait: set_golive_quality on unticked C offline succeeds even while live!
    e.set_golive_quality(&c.id, DestinationQuality::Custom(q720)).await.unwrap();
    let mid_err = e.set_golive_destinations(c_order).await.unwrap_err();
    assert!(
        matches!(mid_err, crate::EngineError::Refused { status: 422, .. }),
        "expected 422 check_midstream refusal, got {mid_err:?}"
    );

    // Unpause Kick via golive_resume: Kick must resume onto `kick_gid` (1080p), NEVER 1440p GROUP_MAIN.
    while got_kick.try_recv().is_ok() {}
    e.golive_resume("kick").await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_kick.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "f54-kick")
        {
            break;
        }
    }
    assert_eq!(e.golive_running_group("kick").as_deref(), Some(kick_gid.as_str()));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f54_standby_failover_under_tight_budget_blocks_backup_without_disrupting_kick() {
    use crate::golive::{Canvas, CustomQuality, DestAdvanced, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform, platform_rejecting_above};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 8,
        nvidia_driver_major: Some(530), // 2 sessions? Wait: <531 is 3, bench overrides max_sessions_ok = 2
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=530".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 600.0,
        max_sessions_ok: Some(3),
        measured_at: 1,
        host_sig: "win|nv=530".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("fail-yt").await;
    let (p_kick, mut got_kick, _) =
        platform_rejecting_above("fail-kick", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    let (p_back, _got_back) = platform("fail-back").await;

    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "fail-yt".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "fail-kick".into(),
    )));
    let srt = e.add_golive_destination("SRT", "srt://127.0.0.1:19999", "", Canvas::Main).await.unwrap();
    let back = e
        .add_golive_destination("Backup", &format!("rtmp://127.0.0.1:{p_back}/app"), "fail-back", Canvas::Main)
        .await
        .unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick" || d.id == srt.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&back.id, DestinationQuality::Custom(q720)).await.unwrap();

    let mut adv = e.golive_config().advanced;
    adv.per.insert(srt.id.clone(), DestAdvanced { backup: Some(back.id.clone()), ..Default::default() });
    e.set_golive_advanced(adv).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "fail-yt"), (&mut got_kick, "fail-kick")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    while got_kick.try_recv().is_ok() {}
    let kick_gid = e.golive_running_group("kick").unwrap();
    assert_ne!(kick_gid, GROUP_MAIN);

    // Trigger SRT failover to Backup (Custom 720p30) on tight 2-session host.
    e.seed_backup_failing_for_test(&srt.id, true);
    e.reconcile().await;

    let mut kick_closed = 0usize;
    while let Ok(msg) = got_kick.try_recv() {
        if matches!(msg, Got::Closed) {
            kick_closed += 1;
        }
    }
    assert_eq!(kick_closed, 0, "backup failover must not disrupt live Kick sibling");
    assert!(
        host.started.lock().unwrap().len() <= 2,
        "must not exceed 2 live encoder sessions: {:?}",
        host.started.lock().unwrap().keys().collect::<Vec<_>>()
    );
    assert!(!e.golive_running().contains(&back.id), "over-budget backup must be blocked cleanly");
    assert_eq!(e.golive_running_group("kick").as_deref(), Some(kick_gid.as_str()));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f54_set_golive_advanced_editing_keep_free_pct_preserves_applied_cap() {
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 12_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.h264".into(),
        engines: None, // Unknown engines => Intel/1-session? Set windows nv=555 with 1 session bench for Pass-2!
        ..Default::default()
    });
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 8,
        nvidia_driver_major: Some(555),
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=555".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 300.0,
        max_sessions_ok: Some(1),
        measured_at: 1,
        host_sig: "win|nv=555".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("adv-yt").await;
    let (p_tw, mut got_tw) = platform("adv-tw").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "adv-yt".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "adv-tw".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "adv-yt"), (&mut got_tw, "adv-tw")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    assert_eq!(e.applied_cap().and_then(|c| c.max_kbps), Some(6_000), "Pass-2 startup must record Twitch cap");

    let mut adv = e.golive_config().advanced;
    adv.keep_free_pct = 25;
    e.set_golive_advanced(adv).await.unwrap();

    assert_eq!(
        e.applied_cap().and_then(|c| c.max_kbps),
        Some(6_000),
        "unrelated keep_free_pct edit must never wipe applied_cap on a Pass-2 stream"
    );
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f55_start_vertical_midstream_recovers_quarantined_vertical_without_restarting_main() {
    use crate::golive::{Canvas, GROUP_MAIN, GROUP_VERTICAL};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct StartVerticalHost {
        inner: Arc<MultiGroupHost>,
    }
    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for StartVerticalHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            self.inner.status().await
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.start().await
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.stop().await
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            match act {
                crate::studio_host::StudioAct::StartVertical { .. } => {
                    if let Some(b) = self.inner.budget_info.lock().unwrap().as_mut() {
                        b.vertical_on = true;
                        b.vertical_size = Some((1080, 1920));
                    }
                    Ok(serde_json::json!({ "vertical_on": true }))
                }
                crate::studio_host::StudioAct::StopVertical => {
                    if let Some(b) = self.inner.budget_info.lock().unwrap().as_mut() {
                        b.vertical_on = false;
                    }
                    Ok(serde_json::json!({ "vertical_on": false }))
                }
                other => self.inner.act(other).await,
            }
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            self.inner.start_encode_group(spec, hub).await
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
            self.inner.live_group_key(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    let (e, _dir) = engine().await;
    let inner = Arc::new(MultiGroupHost::default());
    *inner.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 1920,
        height: 1080,
        fps: 60,
        video_kbps: 6_000,
        ..Default::default()
    };
    *inner.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        vertical_on: false,
        vertical_size: Some((1080, 1920)),
        host_sig: "mac-m2".into(),
        ..Default::default()
    });
    e.set_studio_host(Arc::new(StartVerticalHost { inner: inner.clone() }));

    let (p_yt, mut got_yt) = platform("f55-yt").await;
    let (p_v, mut got_v) = platform("f55-v").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "f55-yt".into(),
    )));
    let vert = e
        .add_golive_destination("VertRTMP", &format!("rtmp://127.0.0.1:{p_v}/app"), "f55-v", Canvas::Vertical)
        .await
        .unwrap();
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == vert.id))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    // Start live while vertical canvas is OFF -> GROUP_MAIN starts cleanly; vertical dest is quarantined.
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_yt.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "f55-yt")
        {
            break;
        }
    }
    assert!(!inner.started.lock().unwrap().contains_key(GROUP_VERTICAL));
    assert!(!e.golive_running().contains(&vert.id));

    // Turn Vertical canvas ON mid-stream via StartVertical -> GROUP_VERTICAL must start and publish without restarting GROUP_MAIN.
    e.studio_act(crate::studio_host::StudioAct::StartVertical { scene: "Vertical".into() }).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_v.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "f55-v")
        {
            break;
        }
    }
    assert!(inner.started.lock().unwrap().contains_key(GROUP_VERTICAL));
    assert_eq!(e.golive_running_group(&vert.id).as_deref(), Some(GROUP_VERTICAL));
    assert_eq!(inner.start_counts.lock().unwrap().get(GROUP_MAIN).copied(), Some(1), "GROUP_MAIN must not restart");

    // Also untick + re-tick vertical dest while vertical canvas is ON -> GROUP_VERTICAL stops then recovers mid-stream!
    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt).await.unwrap();
    assert!(!inner.started.lock().unwrap().contains_key(GROUP_VERTICAL));

    let both_back: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == vert.id))
        .collect();
    e.set_golive_destinations(both_back).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_v.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "f55-v")
        {
            break;
        }
    }
    assert_eq!(e.golive_running_group(&vert.id).as_deref(), Some(GROUP_VERTICAL));
    assert_eq!(inner.start_counts.lock().unwrap().get(GROUP_MAIN).copied(), Some(1));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f57_live_switch_under_platform_exclusive_with_concurrent_reconcile() {
    use crate::golive::{CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform_exclusive};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(1920, 1080, 60, 10_000, 3);
    let s = host.settings.lock().unwrap().clone();
    e.set_studio_host(host);
    e.note_studio_settings(&s);

    let (p_tw, mut got_tw) = platform_exclusive("excl-tw").await;
    let (p_kick, mut got_kick) = platform_exclusive("excl-kick").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "excl-tw".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "excl-kick".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "twitch" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_tw, "excl-tw"), (&mut got_kick, "excl-kick")] {
        loop {
            match timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap() {
                Ok(Got::Publish(k)) if k == want => break,
                Err(why) => panic!("unexpected exclusive overlap at startup for {want}: {why}"),
                _ => {}
            }
        }
    }
    while got_tw.try_recv().is_ok() {}
    while got_kick.try_recv().is_ok() {}

    // Live switch Kick while concurrently hammering `reconcile()`: strict single-publisher server must never see an overlap.
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let (r_sw, ()) = tokio::join!(e.set_golive_quality("kick", DestinationQuality::Custom(q720)), e.reconcile());
    r_sw.unwrap();

    // Wait for clean single-publisher handover on Kick (1 Closed followed by 1 Publish, 0 overlaps).
    let mut saw_closed = false;
    loop {
        match timeout(Duration::from_secs(3), got_kick.recv()).await.unwrap().unwrap() {
            Ok(Got::Closed) => saw_closed = true,
            Ok(Got::Publish(k)) if k == "excl-kick" && saw_closed => break,
            Err(why) => panic!("platform_exclusive saw overlapping publish during switch: {why}"),
            _ => {}
        }
    }
    while let Ok(msg) = got_tw.try_recv() {
        assert!(
            !matches!(msg, Ok(Got::Closed) | Err(_)),
            "Twitch must remain continuously live with zero overlap: {msg:?}"
        );
    }
    assert_ne!(e.golive_running_group("kick").as_deref(), Some(GROUP_MAIN));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn f58_obs_live_v37_light_quality_options_matches_put_200_and_light_off_422() {
    use crate::golive::{Canvas, DestinationQuality};
    use ferrocast_testkit::rtmp::Obs;

    let (e, _dir) = engine().await;
    let d = e.add_golive_destination("ObsLight", "rtmp://127.0.0.1:9/app", "obs-light", Canvas::Main).await.unwrap();
    // Seed persisted v3.7 destination (`light: true, quality: SameAsMain`).
    let mut cfg = e.golive_config();
    cfg.destinations.iter_mut().find(|x| x.id == d.id).unwrap().light = true;
    e.save_golive(cfg).await.unwrap();

    let (server, key) = e.golive_ingest().await.unwrap();
    let ingest_port: u16 = server.rsplit(':').next().unwrap().split('/').next().unwrap().parse().unwrap();
    let _obs = Obs::publish(ingest_port, "live", &key).await.unwrap();
    eventually_async(|| async { e.golive_stream_running() }).await;

    let opts = e.golive_quality_options().await;
    let d_opts = &opts.destinations[&d.id];
    assert!(matches!(d_opts.current, DestinationQuality::Custom(_)), "v3.7 light must resolve to Custom current");
    let cur_opt = d_opts.options.iter().find(|o| o.quality == d_opts.current).expect("matching option for current");
    assert!(cur_opt.allowed, "current option from quality-options must be allowed while OBS is live");

    // Matching PUT .../quality must succeed 200 (consistent with `allowed: true`), while turning light off refuses 422.
    e.set_golive_quality(&d.id, d_opts.current.clone())
        .await
        .expect("PUT quality matching GET quality-options current must return 200 while OBS is live");
    let off_err = e.set_golive_light(&d.id, false).await.unwrap_err();
    assert!(
        matches!(off_err, crate::EngineError::Refused { status: 422, code: "golive_needs_new_encode", .. }),
        "disabling light while OBS is live must still refuse 422: {off_err:?}"
    );
}

#[tokio::test]
async fn mid_drain_cancel_preserves_saved_quality_cleans_old_group_and_keeps_live_dest() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 12_000, 2);
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("drain-yt").await;
    let (p_d, mut got_d) = platform("drain-d").await;
    let (p_c, _got_c) = platform("drain-c").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "drain-yt".into(),
    )));
    let d = e
        .add_golive_destination("DestD", &format!("rtmp://127.0.0.1:{p_d}/app"), "drain-d", Canvas::Main)
        .await
        .unwrap();
    let c = e
        .add_golive_destination("DestC", &format!("rtmp://127.0.0.1:{p_c}/app"), "drain-c", Canvas::Main)
        .await
        .unwrap();

    let q1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 7_000, codec: Some(VideoCodec::H264) };
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q1080.clone())).await.unwrap();

    // Case A: Studio Stop cancels mid-drain -> saved quality stays q1080, and old_group_id is not leaked.
    let only_yt_d: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube" || x.id == d.id)).collect();
    e.set_golive_destinations(only_yt_d.clone()).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "drain-yt"), (&mut got_d, "drain-d")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    let entered_drain = Arc::new(tokio::sync::Notify::new());
    let release_drain = Arc::new(tokio::sync::Notify::new());
    e.arm_slow_drain_for_test(&d.id, entered_drain.clone(), release_drain.clone());
    let e_sw = e.clone();
    let d_id = d.id.clone();
    let q720_a = q720.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality(&d_id, DestinationQuality::Custom(q720_a)).await });
    timeout(Duration::from_secs(3), entered_drain.notified()).await.expect("switch entered drain");

    e.studio_stream(false).await.unwrap();
    release_drain.notify_waiters();
    let err = sw.await.unwrap().unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { code: "golive_switch_cancelled", .. }), "{err:?}");
    let saved_after_stop = e.golive_config().destinations.into_iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        saved_after_stop.quality,
        DestinationQuality::Custom(q1080.clone()),
        "aborted mid-drain switch must never persist new quality to disk"
    );
    assert!(e.active_encode_groups().is_empty(), "old_group_id must not leak on mid-drain Stop");
    assert!(host.started.lock().unwrap().is_empty());

    // Case B: Rejected untick+tick (missing upload confirm -> 409) during drain must keep D live & uncorrupted.
    e.set_golive_destinations(only_yt_d.clone()).await.unwrap();
    e.seed_upload_for_test(10_000);
    while got_yt.try_recv().is_ok() {}
    while got_d.try_recv().is_ok() {}
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "drain-yt"), (&mut got_d, "drain-d")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let old_gid = e.golive_running_group(&d.id).unwrap();
    let entered_drain_b = Arc::new(tokio::sync::Notify::new());
    let release_drain_b = Arc::new(tokio::sync::Notify::new());
    e.arm_slow_drain_for_test(&d.id, entered_drain_b.clone(), release_drain_b.clone());
    let e_sw_b = e.clone();
    let d_id_b = d.id.clone();
    let q720_b = q720.clone();
    let sw_b =
        tokio::spawn(async move { e_sw_b.set_golive_quality(&d_id_b, DestinationQuality::Custom(q720_b)).await });
    timeout(Duration::from_secs(3), entered_drain_b.notified()).await.expect("case B switch entered drain");

    // Untick D while ticking C (without upload confirm -> 409 golive_upload_exceeded).
    let reject_order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube" || x.id == c.id)).collect();
    let e_dest = e.clone();
    let dest_task = tokio::spawn(async move {
        e_dest
            .set_golive_destinations_checked(
                reject_order,
                crate::golive::DestinationConfirm { upload: false, last: true },
            )
            .await
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    release_drain_b.notify_waiters();
    let dest_err = dest_task.await.unwrap().unwrap_err();
    assert!(matches!(dest_err, crate::EngineError::Refused { status: 409, code: "golive_upload_exceeded", .. }));
    let _ = sw_b.await.unwrap();
    assert!(
        e.golive_running().contains(&d.id),
        "enabled destination D must never be silently dropped off air after a rejected untick"
    );

    // Case C: Valid untick of D mid-drain stops D cleanly, keeps old quality on disk, and stops old_gid on host.
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q1080.clone())).await.unwrap();
    let entered_drain_c = Arc::new(tokio::sync::Notify::new());
    let release_drain_c = Arc::new(tokio::sync::Notify::new());
    e.arm_slow_drain_for_test(&d.id, entered_drain_c.clone(), release_drain_c.clone());
    let e_sw_c = e.clone();
    let d_id_c = d.id.clone();
    let sw_c = tokio::spawn(async move { e_sw_c.set_golive_quality(&d_id_c, DestinationQuality::Custom(q720)).await });
    timeout(Duration::from_secs(3), entered_drain_c.notified()).await.expect("case C switch entered drain");

    let untick_d: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube")).collect();
    let e_untick = e.clone();
    let untick_task = tokio::spawn(async move { e_untick.set_golive_destinations(untick_d).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    release_drain_c.notify_waiters();
    let sw_c_err = sw_c.await.unwrap().unwrap_err();
    assert!(matches!(sw_c_err, crate::EngineError::Refused { code: "golive_switch_cancelled", .. }), "{sw_c_err:?}");
    untick_task.await.unwrap().unwrap();
    let saved_after_untick = e.golive_config().destinations.into_iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        saved_after_untick.quality,
        DestinationQuality::Custom(q1080),
        "cancelled switch must keep prior quality saved"
    );
    assert!(
        !host.started.lock().unwrap().contains_key(&old_gid),
        "old_group_id {old_gid} must be stopped after mid-drain untick"
    );
    assert!(!e.active_encode_groups().iter().any(|g| g.id == old_gid));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn stale_stop_stream_later_never_poisons_stream_cancel_and_bad_put_never_cancels() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct StatusGateHost {
        inner: Arc<MultiGroupHost>,
        arm_status: std::sync::atomic::AtomicBool,
        entered_status: Arc<tokio::sync::Notify>,
        release_status: Arc<tokio::sync::Notify>,
        started_notify: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for StatusGateHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            if self.arm_status.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.entered_status.notify_one();
                self.release_status.notified().await;
            }
            crate::studio_host::StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.start().await
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.stop().await
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            match act {
                crate::studio_host::StudioAct::Scenes => Ok(serde_json::json!({ "current": "Main", "scenes": [] })),
                other => self.inner.act(other).await,
            }
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            let res = self.inner.start_encode_group(spec, hub).await;
            self.started_notify.notify_waiters();
            res
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
            self.inner.live_group_key(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    let (e, _dir) = engine().await;
    let inner = ivs_budget_host(1920, 1080, 60, 9_000, 2);
    let entered_status = Arc::new(tokio::sync::Notify::new());
    let release_status = Arc::new(tokio::sync::Notify::new());
    let started_notify = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(StatusGateHost {
        inner: inner.clone(),
        arm_status: std::sync::atomic::AtomicBool::new(false),
        entered_status: entered_status.clone(),
        release_status: release_status.clone(),
        started_notify: started_notify.clone(),
    });
    e.set_studio_host(host.clone());

    let (pa, mut got_a) = platform("stale-a").await;
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "stale-a", Canvas::Main).await.unwrap();
    let only_a: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id)).collect();
    e.set_golive_destinations(only_a).await.unwrap();

    // Part A: Show 1 ends with delayed stop; pause stale stop_stream_later right after `maybe_ours` at `self.rig()`.
    e.patch_show_config(
        serde_json::json!({
            "start_stream": true,
            "stop_stream": true,
            "stop_delay_s": 60,
            "discord": false,
            "recap_message": null
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();
    e.start_show(true).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while !matches!(got_a.recv().await, Some(Got::Publish(k)) if k == "stale-a") {}
    })
    .await
    .expect("Show 1 published stale-a");
    e.end_show().await.unwrap();

    host.arm_status.store(true, std::sync::atomic::Ordering::Release);
    let e_late = e.clone();
    let stale_stop = tokio::spawn(async move { e_late.trigger_delayed_stop_later_for_test().await });
    timeout(Duration::from_secs(3), entered_status.notified()).await.expect("stop_stream_later entered rig()");

    // Start Stream 2 cleanly (`reset_golive_switch_cancel` + `on_stream_started`), then resume stale stop_stream_later.
    e.studio_stream(true).await.unwrap();
    release_status.notify_waiters();
    stale_stop.await.unwrap();

    // Stream 2's switch token MUST NOT be poisoned by the stale Show 1 stop task.
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720))
        .await
        .expect("Stream 2 live quality switch must succeed after stale stop_stream_later");
    e.set_golive_quality(&a.id, DestinationQuality::SameAsMain).await.unwrap();

    // Part B: Malformed (400) or unconfirmed (409) `PUT /golive/destinations` unticking `A` while `A` is awaiting IDR
    // must NOT cancel `A`'s in-flight live switch.
    let mut p_in = e.golive_planner_input(e.cached_pc_budget());
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    p_in.destinations.iter_mut().find(|x| x.id == a.id).unwrap().quality = DestinationQuality::Custom(q480.clone());
    let gid_480 = crate::golive::plan_encodes(&p_in).destinations[&a.id].group_id.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    inner.idr_gate.lock().unwrap().insert(gid_480.clone(), gate.clone());

    let e_sw = e.clone();
    let a_id = a.id.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality(&a_id, DestinationQuality::Custom(q480)).await });
    timeout(Duration::from_secs(3), async {
        while !inner.started.lock().unwrap().contains_key(&gid_480) {
            started_notify.notified().await;
        }
    })
    .await
    .expect("target group started for IDR wait");

    // 400 incomplete order unticking A.
    let err_incomplete = e.set_golive_destinations(vec![(a.id.clone(), false)]).await.unwrap_err();
    assert!(matches!(err_incomplete, crate::EngineError::Invalid(_)));
    // 400 duplicate ID unticking A.
    let dup: Vec<(String, bool)> = e.golive_config().destinations.iter().map(|_| (a.id.clone(), false)).collect();
    let err_dup = e.set_golive_destinations(dup).await.unwrap_err();
    assert!(matches!(err_dup, crate::EngineError::Invalid(_)));
    // 409 missing last-destination confirmation unticking A.
    let all_off: Vec<(String, bool)> = e.golive_config().destinations.into_iter().map(|d| (d.id, false)).collect();
    let err_last = e
        .set_golive_destinations_checked(all_off, crate::golive::DestinationConfirm { upload: true, last: false })
        .await
        .unwrap_err();
    assert!(matches!(err_last, crate::EngineError::Refused { status: 409, code: "golive_last_destination", .. }));

    gate.notify_waiters();
    sw.await
        .unwrap()
        .expect("A's in-flight live switch must complete cleanly after rejected PUT /golive/destinations calls");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn route_light_on_two_session_host_never_kills_kick_and_switching_stays_accurate() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform, platform_rejecting_above};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 12,
        nvidia_driver_major: Some(555),
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=555".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 600.0,
        max_sessions_ok: Some(3),
        measured_at: 1,
        host_sig: "win|nv=555".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("rl2-yt").await;
    let (p_kick, mut got_kick, _) =
        platform_rejecting_above("rl2-kick", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "rl2-yt".into(),
    )));
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "rl2-kick".into(),
    )));
    let order: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick"))
        .collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "rl2-yt"), (&mut got_kick, "rl2-kick")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let kick_gid = e.golive_running_group("kick").unwrap();
    assert_ne!(kick_gid, GROUP_MAIN);

    // Feed real hub traffic on GROUP_MAIN and g_kick so hub.stats().kbps > 0, and simulate slow Kick route.
    let (main_hub, kick_hub) = {
        let s = host.started.lock().unwrap();
        (s[GROUP_MAIN].1.clone(), s[&kick_gid].1.clone())
    };
    for hub in [&main_hub, &kick_hub] {
        for i in 0..=20u32 {
            let mut pkt = vec![if i % 10 == 0 { 0x17 } else { 0x27 }, 0x01];
            pkt.resize(40_000, 0);
            hub.video(i * 100, pkt.into());
        }
    }
    let _yt_tx = e.replace_running_health_for_test("youtube", GROUP_MAIN, 18_000, 60_000);
    let _kick_tx = e.replace_running_health_for_test("kick", &kick_gid, 500, 60_000);
    let t0 = std::time::Instant::now();
    for sec in 0..=u64::from(ferrocast_golive::quality::LIGHTEN_AFTER) + 1 {
        e.quality_tick_at(t0 + Duration::from_secs(sec)).await;
    }

    assert!(
        e.golive_running().contains("kick"),
        "Kick must NEVER be silently killed off air by route-light on a 2-session host"
    );
    assert!(host.started.lock().unwrap().contains_key(&kick_gid), "g_kick must remain started");

    // Part C: While Dest A (youtube) holds group_ops inside IDR wait, an invalid or queued PUT on Kick
    // must NOT falsely flip Kick's `switching_quality` in GoLiveView.
    e.studio_stream(false).await.unwrap();
    let host4 = ivs_budget_host(2560, 1440, 60, 12_000, 2);
    e.set_studio_host(host4.clone());
    e.studio_stream(true).await.unwrap();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let mut p_in = e.golive_planner_input(e.cached_pc_budget());
    p_in.destinations.iter_mut().find(|d| d.id == "youtube").unwrap().quality =
        DestinationQuality::Custom(q720.clone());
    let yt_gid = crate::golive::plan_encodes(&p_in).destinations["youtube"].group_id.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    host4.idr_gate.lock().unwrap().insert(yt_gid.clone(), gate.clone());

    let e_yt = e.clone();
    let sw_yt = tokio::spawn(async move { e_yt.set_golive_quality("youtube", DestinationQuality::Custom(q720)).await });
    for _ in 0..100 {
        if host4.started.lock().unwrap().contains_key(&yt_gid) {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Invalid 422 request for Kick while YouTube holds group_ops.
    let bad_kick = e
        .set_golive_quality(
            "kick",
            DestinationQuality::Custom(CustomQuality {
                width: 2560,
                height: 1440,
                fps: 60,
                kbps: 8_000,
                codec: Some(VideoCodec::H264),
            }),
        )
        .await;
    assert!(bad_kick.is_err());

    // Valid queued switch for Kick while YouTube still holds group_ops.
    let e_k = e.clone();
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    let sw_k = tokio::spawn(async move { e_k.set_golive_quality("kick", DestinationQuality::Custom(q480)).await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let view = e.golive_view().await;
    let yt_v = view.destinations.iter().find(|d| d.id == "youtube").unwrap();
    let kick_v = view.destinations.iter().find(|d| d.id == "kick").unwrap();
    assert!(yt_v.switching_quality, "active switch holding group_ops must show switching_quality=true");
    assert!(
        !kick_v.switching_quality,
        "queued destination waiting for group_ops must not expose switching_quality=true prematurely"
    );

    gate.notify_waiters();
    sw_yt.await.unwrap().unwrap();
    sw_k.await.unwrap().unwrap();
    e.studio_stream(false).await.unwrap();
    let _ = Canvas::Main;
}

#[tokio::test]
async fn simultaneous_untick_and_tick_swaps_encode_groups_on_two_session_host() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = Arc::new(MultiGroupHost::default());
    *host.settings.lock().unwrap() = crate::studio_host::StudioSettings {
        width: 2560,
        height: 1440,
        fps: 60,
        video_kbps: 18_000,
        ..Default::default()
    };
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 12,
        nvidia_driver_major: Some(555),
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=555".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 600.0,
        max_sessions_ok: Some(3),
        measured_at: 1,
        host_sig: "win|nv=555".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("swap-yt").await;
    let (p_a, mut got_a) = platform("swap-a").await;
    let (p_b, mut got_b) = platform("swap-b").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "swap-yt".into(),
    )));
    let a =
        e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{p_a}/app"), "swap-a", Canvas::Main).await.unwrap();
    let b =
        e.add_golive_destination("B", &format!("rtmp://127.0.0.1:{p_b}/app"), "swap-b", Canvas::Main).await.unwrap();

    let initial: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube" || d.id == a.id)).collect();
    e.set_golive_destinations(initial).await.unwrap();

    let q1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 7_000, codec: Some(VideoCodec::H264) };
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q1080)).await.unwrap();
    e.set_golive_quality(&b.id, DestinationQuality::Custom(q720)).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "swap-yt"), (&mut got_a, "swap-a")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    while got_yt.try_recv().is_ok() {}
    let gid_a = e.golive_running_group(&a.id).unwrap();
    assert_ne!(gid_a, GROUP_MAIN);

    // Simultaneously untick A and tick B in one PUT /golive/destinations on a 2-session host.
    let swap_order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube" || d.id == b.id)).collect();
    e.set_golive_destinations(swap_order).await.expect("simultaneous untick A + tick B must succeed within 2 sessions");

    loop {
        if matches!(timeout(Duration::from_secs(3), got_b.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "swap-b")
        {
            break;
        }
    }
    let gid_b = e.golive_running_group(&b.id).expect("B must be running");
    assert_ne!(gid_b, GROUP_MAIN);
    assert_ne!(gid_b, gid_a);
    assert!(!e.golive_running().contains(&a.id));
    assert!(!host.started.lock().unwrap().contains_key(&gid_a), "old group {gid_a} must be stopped");
    assert!(host.started.lock().unwrap().contains_key(&gid_b), "new group {gid_b} must be started");
    let view = e.golive_view().await;
    assert!(
        view.destinations.iter().find(|d| d.id == b.id).unwrap().blocked.is_none(),
        "B must not be quarantined in blocked"
    );
    while let Ok(msg) = got_yt.try_recv() {
        assert!(!matches!(msg, Got::Closed), "YouTube on GROUP_MAIN must have 0 disconnects");
    }
    assert_eq!(host.start_counts.lock().unwrap().get(GROUP_MAIN).copied(), Some(1));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn synthesized_main_group_does_not_inherit_kick_cap() {
    use crate::golive::GROUP_MAIN;
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform_rejecting_above};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    *host.budget_info.lock().unwrap().as_mut().unwrap() = crate::studio_host::HostBudgetInfo {
        os: "macos".into(),
        encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
        engines: Some(2),
        cpu_cores: 10,
        record_shares_main: true,
        host_sig: "mac-m2".into(),
        ..Default::default()
    };
    e.set_studio_host(host);

    let (p_kick, mut got_kick, _) =
        platform_rejecting_above("synth-kick", 1920, 1080, 8000, ferrocast_golive::flv::avcc_sps_dimensions).await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "synth-kick".into(),
    )));
    // Only Kick is enabled while "Same as stream" recording shares GROUP_MAIN (`plan.groups` has only `g_kick`,
    // so `encode_group_specs_for_plan` synthesizes `GROUP_MAIN`).
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "kick")).collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_kick.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "synth-kick")
        {
            break;
        }
    }

    let groups = e.active_encode_groups();
    let main_spec = groups.iter().find(|g| g.id == GROUP_MAIN).expect("synthesized GROUP_MAIN active");
    assert_eq!(main_spec.key.kbps, 18_000);
    assert_eq!(
        main_spec.cap_kbps, None,
        "synthesized GROUP_MAIN (recording only, Kick on separate group) must not inherit Kick's 8000 kbps cap"
    );
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn offline_and_studio_live_put_quality_clears_legacy_light_flag() {
    use crate::golive::{CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::platform;

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(1920, 1080, 60, 6_000, 2);
    e.set_studio_host(host);
    let (p_tw, _got_tw) = platform("mig-tw").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Twitch,
        format!("rtmp://127.0.0.1:{p_tw}/app"),
        "mig-tw".into(),
    )));

    let mut cfg = e.golive_config();
    let tw = cfg.destinations.iter_mut().find(|d| d.id == "twitch").unwrap();
    tw.enabled = true;
    tw.light = true;
    tw.quality = DestinationQuality::SameAsMain;
    e.save_golive(cfg).await.unwrap();

    let legacy_custom = CustomQuality { width: 1280, height: 720, fps: 60, kbps: 3_000, codec: Some(VideoCodec::H264) };
    let after_offline =
        e.set_golive_quality("twitch", DestinationQuality::Custom(legacy_custom.clone())).await.unwrap();
    let tw_off = after_offline.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(!tw_off.light, "offline PUT quality must clear legacy d.light");
    assert_eq!(
        tw_off.quality,
        DestinationQuality::Custom(legacy_custom.clone()),
        "offline PUT quality must persist explicit Custom quality"
    );

    // Also verify Studio-live migration when starting from legacy light: true.
    let mut cfg2 = e.golive_config();
    let tw2 = cfg2.destinations.iter_mut().find(|d| d.id == "twitch").unwrap();
    tw2.light = true;
    tw2.quality = DestinationQuality::SameAsMain;
    e.save_golive(cfg2).await.unwrap();

    e.studio_stream(true).await.unwrap();
    let after_live = e.set_golive_quality("twitch", DestinationQuality::Custom(legacy_custom.clone())).await.unwrap();
    let tw_live = after_live.destinations.iter().find(|d| d.id == "twitch").unwrap();
    assert!(!tw_live.light, "Studio-live PUT quality must clear legacy d.light");
    assert_eq!(tw_live.quality, DestinationQuality::Custom(legacy_custom));
    e.studio_stream(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multithread_exclusive_switch_and_stress_with_slow_resolve_drain_and_ticks() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform_exclusive};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct DelayedResolveYt {
        url: String,
        key: String,
        delay: std::sync::atomic::AtomicBool,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for DelayedResolveYt {
        fn platform(&self) -> Platform {
            Platform::Youtube
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            Default::default()
        }
        async fn send_chat(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
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
        async fn set_mode(&self, _: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            Ok(())
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            if self.delay.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(ferrocast_connector::Ingest { url: self.url.clone(), key: self.key.clone(), backup: None, note: None })
        }
    }

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(1920, 1080, 60, 10_000, 3);
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform_exclusive("f67-yt").await;
    let (p_kick, mut got_kick) = platform_exclusive("f67-kick").await;
    let (p_c, mut got_c) = platform_exclusive("f67-c").await;

    let entered_resolve = Arc::new(tokio::sync::Notify::new());
    let release_resolve = Arc::new(tokio::sync::Notify::new());
    let yt_conn = Arc::new(DelayedResolveYt {
        url: format!("rtmp://127.0.0.1:{p_yt}/app"),
        key: "f67-yt".into(),
        delay: std::sync::atomic::AtomicBool::new(false),
        entered: entered_resolve.clone(),
        release: release_resolve.clone(),
    });
    e.register_connector(yt_conn.clone());
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Kick,
        format!("rtmp://127.0.0.1:{p_kick}/app"),
        "f67-kick".into(),
    )));
    let c =
        e.add_golive_destination("ExclC", &format!("rtmp://127.0.0.1:{p_c}/app"), "f67-c", Canvas::Main).await.unwrap();

    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };

    for iter in 0..8 {
        e.set_golive_quality("youtube", DestinationQuality::SameAsMain).await.unwrap();
        e.set_golive_quality(&c.id, DestinationQuality::SameAsMain).await.unwrap();
        let all_on: Vec<(String, bool)> = e
            .golive_config()
            .destinations
            .into_iter()
            .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "kick" || d.id == c.id))
            .collect();
        e.set_golive_destinations(all_on).await.unwrap();
        while got_yt.try_recv().is_ok() {}
        while got_kick.try_recv().is_ok() {}
        while got_c.try_recv().is_ok() {}

        let starts_before = host.start_counts.lock().unwrap().get(GROUP_MAIN).copied().unwrap_or(0);
        e.studio_stream(true).await.unwrap();
        for (rx, want) in [(&mut got_yt, "f67-yt"), (&mut got_kick, "f67-kick"), (&mut got_c, "f67-c")] {
            loop {
                match timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap() {
                    Ok(Got::Publish(k)) if k == want => break,
                    Err(why) => panic!("iter {iter}: startup exclusive overlap for {want}: {why}"),
                    _ => {}
                }
            }
        }
        while got_kick.try_recv().is_ok() {}

        // Arm slow resolve on YouTube on even iterations and slow drain on ExclC on odd iterations,
        // then race switches, reconcile, all ticks, and Stop deterministically with no wall-clock waits.
        let entered_drain = Arc::new(tokio::sync::Notify::new());
        let release_drain = Arc::new(tokio::sync::Notify::new());
        if iter % 2 == 0 {
            yt_conn.delay.store(true, std::sync::atomic::Ordering::Release);
        } else {
            e.arm_slow_drain_for_test(&c.id, entered_drain.clone(), release_drain.clone());
        }

        let (e1, e2, e3, e4, e5, e6) = (e.clone(), e.clone(), e.clone(), e.clone(), e.clone(), e.clone());
        let cid = c.id.clone();
        let qc = q480.clone();
        let qy = q720.clone();
        let t_sw_yt =
            tokio::spawn(async move { e1.set_golive_quality("youtube", DestinationQuality::Custom(qy)).await });
        let t_sw_c = tokio::spawn(async move { e2.set_golive_quality(&cid, DestinationQuality::Custom(qc)).await });
        let t_rec = tokio::spawn(async move { e3.reconcile().await });
        let t_qt = tokio::spawn(async move { e4.quality_tick_at(std::time::Instant::now()).await });
        let t_bt = tokio::spawn(async move { e5.backup_tick().await });
        let t_pt = tokio::spawn(async move { e6.protect_tick().await });

        let e_stop = e.clone();
        let ent_res = entered_resolve.clone();
        let rel_res = release_resolve.clone();
        let rel_drn = release_drain.clone();
        let t_ctrl = tokio::spawn(async move {
            if iter % 2 == 0 {
                timeout(Duration::from_secs(3), ent_res.notified()).await.expect("entered slow resolve");
                let _ = e_stop.studio_stream(false).await;
                rel_res.notify_waiters();
            } else {
                timeout(Duration::from_secs(3), entered_drain.notified()).await.expect("entered slow drain");
                let _ = e_stop.studio_stream(false).await;
                rel_drn.notify_waiters();
            }
        });

        let (ry, rc, rr, rq, rb, rp, rctl) = tokio::join!(t_sw_yt, t_sw_c, t_rec, t_qt, t_bt, t_pt, t_ctrl);
        let _ = ry.unwrap();
        let _ = rc.unwrap();
        rr.unwrap();
        rq.unwrap();
        rb.unwrap();
        rp.unwrap();
        rctl.unwrap();

        for (label, rx) in [("youtube", &mut got_yt), ("kick", &mut got_kick), ("c", &mut got_c)] {
            while let Ok(msg) = rx.try_recv() {
                if let Err(overlap) = msg {
                    panic!("iter {iter}: platform_exclusive {label} overlap: {overlap}");
                }
            }
        }
        let starts_after = host.start_counts.lock().unwrap().get(GROUP_MAIN).copied().unwrap_or(0);
        assert_eq!(starts_after - starts_before, 1, "iter {iter}: GROUP_MAIN must never restart");
        assert!(e.active_encode_groups().is_empty(), "iter {iter}: no leaked active_groups");
        assert!(host.started.lock().unwrap().is_empty(), "iter {iter}: no leaked host groups");
        assert!(e.golive_running().is_empty(), "iter {iter}: no leaked running forwarders");
    }
}

#[tokio::test]
async fn ticks_skip_when_group_ops_held_and_protect_pause_reconciles_cleanly() {
    use ferrocast_golive::push::PushState;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    async fn wait_got(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Got>, want: impl Fn(&Got) -> bool) -> Got {
        loop {
            let g = timeout(Duration::from_secs(3), rx.recv()).await.expect("rtmp msg in time").expect("open");
            if want(&g) {
                return g;
            }
        }
    }

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(1920, 1080, 60, 10_000, 3);
    e.set_studio_host(host.clone());
    let (pa, mut got_a) = platform("tick-a").await;
    let (pb, mut got_b) = platform("tick-b").await;
    let a = e
        .add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "tick-a", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let b = e
        .add_golive_destination("B", &format!("rtmp://127.0.0.1:{pb}/app"), "tick-b", crate::golive::Canvas::Main)
        .await
        .unwrap();
    let enable_both: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id || d.id == b.id)).collect();
    e.set_golive_destinations(enable_both).await.unwrap();
    e.studio_stream(true).await.unwrap();
    wait_got(&mut got_a, |g| matches!(g, Got::Publish(k) if k == "tick-a")).await;
    wait_got(&mut got_b, |g| matches!(g, Got::Publish(k) if k == "tick-b")).await;

    // While another lifecycle operation holds `group_ops`, background ticks and `pause`
    // must skip via `try_lock` without deadlocking or mutating paused state.
    let b_health = e.replace_running_health_for_test(&b.id, crate::golive::GROUP_MAIN, 500, 60_000);
    let hold = e.golive().group_ops.lock().await;
    e.backup_tick().await;
    e.quality_tick().await;
    assert!(!e.quality_tick_try_at(std::time::Instant::now()).await, "quality_tick must skip when group_ops is held");
    e.protect_tick().await;
    e.pause(b.id.clone(), "A".into());
    assert!(e.golive_running().contains(&b.id), "pause while group_ops held must skip without mutating");
    drop(b_health);
    assert!(
        !matches!(
            e.golive_view()
                .await
                .destinations
                .iter()
                .find(|d| d.id == b.id)
                .and_then(|d| d.health.as_ref())
                .map(|h| &h.state),
            Some(PushState::Paused { .. })
        ),
        "paused state must stay empty while group_ops held"
    );
    drop(hold);

    // Once `group_ops` is free, `pause` and `protect_tick` acquire it cleanly and resume works.
    e.pause(b.id.clone(), "A".into());
    wait_got(&mut got_b, |g| matches!(g, Got::Closed)).await;
    assert!(matches!(
        e.golive_view()
            .await
            .destinations
            .iter()
            .find(|d| d.id == b.id)
            .and_then(|d| d.health.as_ref())
            .map(|h| &h.state),
        Some(PushState::Paused { .. })
    ));
    e.protect_tick().await;
    e.golive_resume(&b.id).await.unwrap();
    wait_got(&mut got_b, |g| matches!(g, Got::Publish(k) if k == "tick-b")).await;

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn save_golive_failure_after_drain_restores_destination_on_old_group() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("sg-yt").await;
    let (p_d, mut got_d) = platform("sg-d").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "sg-yt".into(),
    )));
    let d =
        e.add_golive_destination("DestD", &format!("rtmp://127.0.0.1:{p_d}/app"), "sg-d", Canvas::Main).await.unwrap();
    let only_yt_d: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube" || x.id == d.id)).collect();
    e.set_golive_destinations(only_yt_d).await.unwrap();

    let q1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 7_000, codec: Some(VideoCodec::H264) };
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q1080.clone())).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "sg-yt"), (&mut got_d, "sg-d")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    while got_d.try_recv().is_ok() {}
    let old_gid = e.golive_running_group(&d.id).unwrap();
    let mut p_in = e.golive_planner_input(e.cached_pc_budget());
    p_in.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q720.clone());
    let target_gid = crate::golive::plan_encodes(&p_in).destinations[&d.id].group_id.clone();
    assert_ne!(old_gid, target_gid);

    // Block saving `golive` in SQLite so persistence fails AFTER draining the old forwarder.
    e.db(|s| {
        s.write(|tx| {
            Ok(tx.execute_batch(
                "CREATE TRIGGER block_golive_save BEFORE UPDATE ON settings WHEN NEW.key = 'golive'
                 BEGIN SELECT RAISE(ABORT, 'disk error'); END;",
            )?)
        })
    })
    .await
    .unwrap();

    let err = e.set_golive_quality(&d.id, DestinationQuality::Custom(q720)).await.unwrap_err();
    assert!(!matches!(err, EngineError::Refused { code: "golive_switch_cancelled", .. }), "{err:?}");

    // Destination D must be restored on `old_gid` instead of silently dropped off air.
    assert!(e.golive_running().contains(&d.id), "enabled destination D must stay live after save_golive failure");
    assert_eq!(e.golive_running_group(&d.id).as_deref(), Some(old_gid.as_str()));
    assert!(!host.started.lock().unwrap().contains_key(&target_gid), "target group must be aborted on save failure");
    loop {
        if matches!(timeout(Duration::from_secs(3), got_d.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "sg-d")
        {
            break;
        }
    }
    let view = e.golive_view().await;
    assert!(!view.destinations.iter().find(|x| x.id == d.id).unwrap().switching_quality);
    assert_eq!(
        e.golive_config().destinations.into_iter().find(|x| x.id == d.id).unwrap().quality,
        DestinationQuality::Custom(q1080)
    );

    e.db(|s| s.write(|tx| Ok(tx.execute_batch("DROP TRIGGER block_golive_save")?))).await.unwrap();
    e.studio_stream(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_cancel_recovery_and_future_abort_preserve_live_destination_and_groups() {
    use crate::golive::{Canvas, CustomQuality, DestinationConfirm, DestinationQuality, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(2560, 1440, 60, 18_000, 2);
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("pcr-yt").await;
    let (p_d, mut got_d) = platform("pcr-d").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "pcr-yt".into(),
    )));
    let d =
        e.add_golive_destination("DestD", &format!("rtmp://127.0.0.1:{p_d}/app"), "pcr-d", Canvas::Main).await.unwrap();

    let q1080 = CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 7_000, codec: Some(VideoCodec::H264) };
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };

    // Part A: Single-destination stream (only g_d active, no GROUP_MAIN) cancelled mid-drain by an untick
    // that then fails post-lock in `save_golive` -> `reconcile_locked` must restore g_d and D on air.
    let only_d: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == d.id)).collect();
    e.set_golive_destinations(only_d.clone()).await.unwrap();
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q1080.clone())).await.unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_d.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "pcr-d")
        {
            break;
        }
    }
    let single_gid = e.golive_running_group(&d.id).unwrap();

    let entered_a = Arc::new(tokio::sync::Notify::new());
    let release_a = Arc::new(tokio::sync::Notify::new());
    e.arm_slow_drain_for_test(&d.id, entered_a.clone(), release_a.clone());
    let e_sw_a = e.clone();
    let d_id_a = d.id.clone();
    let q720_a = q720.clone();
    let sw_a =
        tokio::spawn(async move { e_sw_a.set_golive_quality(&d_id_a, DestinationQuality::Custom(q720_a)).await });
    timeout(Duration::from_secs(3), entered_a.notified()).await.expect("Part A entered drain");

    e.db(|s| {
        s.write(|tx| {
            Ok(tx.execute_batch(
                "CREATE TRIGGER block_golive_pcr BEFORE UPDATE ON settings WHEN NEW.key = 'golive'
                 BEGIN SELECT RAISE(ABORT, 'disk error'); END;",
            )?)
        })
    })
    .await
    .unwrap();
    let all_off: Vec<(String, bool)> = e.golive_config().destinations.into_iter().map(|x| (x.id, false)).collect();
    let untick_err =
        e.set_golive_destinations_checked(all_off, DestinationConfirm { upload: true, last: true }).await.unwrap_err();
    assert!(!matches!(untick_err, EngineError::Refused { code: "golive_last_destination", .. }), "{untick_err:?}");
    release_a.notify_waiters();
    let sw_a_err = sw_a.await.unwrap().unwrap_err();
    assert!(matches!(sw_a_err, EngineError::Refused { code: "golive_switch_cancelled", .. }), "{sw_a_err:?}");
    assert!(
        e.golive_running().contains(&d.id),
        "single-destination post-cancel recovery must restore D and {single_gid} on air"
    );
    assert!(host.started.lock().unwrap().contains_key(&single_gid));

    e.db(|s| s.write(|tx| Ok(tx.execute_batch("DROP TRIGGER block_golive_pcr")?))).await.unwrap();
    e.studio_stream(false).await.unwrap();

    // Part B: Multi-thread stress of post-cancel recovery across both `set_golive_destinations`
    // and `remove_golive_destination` failing post-lock after cancelling an in-flight switch.
    let only_yt_d: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube" || x.id == d.id)).collect();
    e.set_golive_destinations(only_yt_d).await.unwrap();
    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "pcr-yt"), (&mut got_d, "pcr-d")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }

    for iter in 0..8 {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        e.arm_slow_drain_for_test(&d.id, entered.clone(), release.clone());
        let e_sw = e.clone();
        let d_id = d.id.clone();
        let q720_b = q720.clone();
        let sw = tokio::spawn(async move { e_sw.set_golive_quality(&d_id, DestinationQuality::Custom(q720_b)).await });
        timeout(Duration::from_secs(3), entered.notified()).await.expect("entered slow drain");

        e.db(|s| {
            s.write(|tx| {
                Ok(tx.execute_batch(
                    "CREATE TRIGGER block_golive_iter BEFORE UPDATE ON settings WHEN NEW.key = 'golive'
                     BEGIN SELECT RAISE(ABORT, 'disk error'); END;",
                )?)
            })
        })
        .await
        .unwrap();

        let err = if iter % 2 == 0 {
            let untick: Vec<(String, bool)> =
                e.golive_config().destinations.into_iter().map(|x| (x.id.clone(), x.id == "youtube")).collect();
            e.set_golive_destinations(untick).await.unwrap_err()
        } else {
            e.remove_golive_destination(&d.id).await.unwrap_err()
        };
        assert!(!matches!(err, EngineError::Refused { code: "golive_last_destination", .. }), "iter {iter}: {err:?}");
        release.notify_waiters();
        let sw_err = sw.await.unwrap().unwrap_err();
        assert!(matches!(sw_err, EngineError::Refused { code: "golive_switch_cancelled", .. }));
        e.db(|s| s.write(|tx| Ok(tx.execute_batch("DROP TRIGGER block_golive_iter")?))).await.unwrap();

        assert!(
            e.golive_running().contains(&d.id),
            "iter {iter}: D must be restored live immediately after post-cancel recovery"
        );
        assert!(
            !e.golive_view().await.destinations.iter().find(|x| x.id == d.id).unwrap().switching_quality,
            "iter {iter}: switching_quality must clear before group_ops unlocks"
        );
    }

    // Part C: Dropping the switch future mid-IDR wait must record and drain `orphaned_groups` and clear
    // `g.switching` before `group_ops` unlocks, so a waiting retry never sees 409 or leaked host groups.
    let mut p_in = e.golive_planner_input(e.cached_pc_budget());
    p_in.destinations.iter_mut().find(|x| x.id == d.id).unwrap().quality = DestinationQuality::Custom(q720.clone());
    let gid_720 = crate::golive::plan_encodes(&p_in).destinations[&d.id].group_id.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    host.idr_gate.lock().unwrap().insert(gid_720.clone(), gate.clone());

    let abort_now = Arc::new(tokio::sync::Notify::new());
    let e_drop = e.clone();
    let d_drop = d.id.clone();
    let abort_wait = abort_now.clone();
    tokio::select! {
        res = e_drop.set_golive_quality(&d_drop, DestinationQuality::Custom(q720.clone())) => {
            panic!("IDR-gated switch finished unexpectedly before abort: {res:?}");
        }
        _ = async {
            while !(host.started.lock().unwrap().contains_key(&gid_720)
                && e.golive().active_switching_ids().contains(&d.id))
            {
                tokio::task::yield_now().await;
            }
            abort_wait.notify_one();
            abort_wait.notified().await;
        } => {}
    }

    // Immediately after future drop, `reconcile` and a retry switch on `d.id` must succeed cleanly.
    e.reconcile().await;
    assert!(!host.started.lock().unwrap().contains_key(&gid_720), "aborted IDR group must be drained");
    assert!(!e.active_encode_groups().iter().any(|g| g.id == gid_720));
    e.set_golive_quality(&d.id, DestinationQuality::Custom(q480))
        .await
        .expect("retry after aborted future must not hit 409 Conflict");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn stop_stream_later_and_start_show_with_race_never_poisons_stream_cancel() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use crate::studio::show::ShowStartOpts;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    #[derive(Debug)]
    struct RunningStudioHost {
        inner: Arc<MultiGroupHost>,
    }

    #[async_trait::async_trait]
    impl crate::studio_host::StudioHost for RunningStudioHost {
        async fn status(&self) -> crate::studio_host::StudioStatus {
            crate::studio_host::StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.start().await
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            self.inner.stop().await
        }
        async fn kinds(&self) -> Result<serde_json::Value, String> {
            self.inner.kinds().await
        }
        async fn options(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.options(kind).await
        }
        async fn preview(&self, rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            self.inner.preview(rect).await
        }
        async fn defaults(&self, kind: String) -> Result<serde_json::Value, String> {
            self.inner.defaults(kind).await
        }
        async fn act(&self, act: crate::studio_host::StudioAct) -> Result<serde_json::Value, String> {
            match act {
                crate::studio_host::StudioAct::Scenes => Ok(serde_json::json!({ "current": "Main", "scenes": [] })),
                other => self.inner.act(other).await,
            }
        }
        async fn settings(&self) -> crate::studio_host::StudioSettings {
            self.inner.settings().await
        }
        async fn set_settings(&self, s: crate::studio_host::StudioSettings) -> Result<Option<String>, String> {
            self.inner.set_settings(s).await
        }
        async fn stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stats().await
        }
        async fn record(&self, on: bool) -> Result<serde_json::Value, String> {
            self.inner.record(on).await
        }
        async fn record_pause(&self, on: bool) -> Result<(), String> {
            self.inner.record_pause(on).await
        }
        async fn monitoring(&self) -> Result<serde_json::Value, String> {
            self.inner.monitoring().await
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            t: Vec<(u32, u32, u32)>,
        ) -> Result<serde_json::Value, String> {
            self.inner.stream_start(server, key, t).await
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.inner.stream_stop().await
        }
        async fn stream_stats(&self) -> Result<serde_json::Value, String> {
            self.inner.stream_stats().await
        }
        async fn obs_collections(&self) -> Result<serde_json::Value, String> {
            self.inner.obs_collections().await
        }
        async fn import_obs(&self, id: String) -> Result<serde_json::Value, String> {
            self.inner.import_obs(id).await
        }
        fn supports_encode_groups(&self) -> bool {
            true
        }
        async fn start_encode_group(
            &self,
            spec: crate::studio_host::EncodeGroupSpec,
            hub: Arc<ferrocast_golive::Hub>,
        ) -> Result<bool, String> {
            self.inner.start_encode_group(spec, hub).await
        }
        async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.stop_encode_group(id).await
        }
        async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
            self.inner.abort_encode_group(id).await
        }
        async fn live_group_key(&self, id: &str) -> Option<crate::studio_host::EncodeKey> {
            self.inner.live_group_key(id).await
        }
        async fn group_stats(&self) -> Result<Option<serde_json::Value>, String> {
            self.inner.group_stats().await
        }
        async fn host_budget_info(&self) -> Option<crate::studio_host::HostBudgetInfo> {
            self.inner.host_budget_info().await
        }
    }

    let (e, _dir) = engine().await;
    let inner = ivs_budget_host(1920, 1080, 60, 9_000, 2);
    let host = Arc::new(RunningStudioHost { inner: inner.clone() });
    e.set_studio_host(host);

    let (pa, mut got_a) = platform("ds-a").await;
    let a = e.add_golive_destination("A", &format!("rtmp://127.0.0.1:{pa}/app"), "ds-a", Canvas::Main).await.unwrap();
    let only_a: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == a.id)).collect();
    e.set_golive_destinations(only_a).await.unwrap();

    e.patch_show_config(
        serde_json::json!({
            "start_stream": true,
            "stop_stream": true,
            "stop_delay_s": 60,
            "discord": false,
            "recap_message": null
        })
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();

    // Part A: While `stop_stream_later` already passed `rig()` and cancelled `stream_cancel` and awaits
    // `group_ops`, `start_show_with` (`stream: Some(false)`) must not hijack `stop_waiting` / bump `epoch`
    // and abandon a live stream with a poisoned `stream_cancel`.
    e.start_show(true).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while !matches!(got_a.recv().await, Some(Got::Publish(k)) if k == "ds-a") {}
    })
    .await
    .unwrap();
    e.end_show().await.unwrap();

    let hold = e.golive().group_ops.lock().await;
    let cancel_tok = e.current_golive_switch_cancel();
    let e_late = e.clone();
    let stop_task = tokio::spawn(async move { e_late.trigger_delayed_stop_later_for_test().await });
    timeout(Duration::from_secs(3), cancel_tok.cancelled())
        .await
        .expect("stop_stream_later cancelled stream_cancel before acquiring group_ops");

    let start_err = e
        .start_show_with(ShowStartOpts { force: false, stream: Some(false) })
        .await
        .expect_err("start_show_with must be rejected once delayed stop is in flight");
    assert!(matches!(start_err, EngineError::Conflict(_)), "{start_err:?}");

    drop(hold);
    stop_task.await.unwrap();
    assert!(!e.golive_stream_running(), "delayed stop must finish stopping the stream");

    // Part B: Even if `stop_stream_later` exits via `!still_ours` after acquiring `group_ops` while the
    // Studio stream stays live, `stream_cancel` must be healed so subsequent quality switches succeed.
    e.start_show(true).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while !matches!(got_a.recv().await, Some(Got::Publish(k)) if k == "ds-a") {}
    })
    .await
    .unwrap();
    e.end_show().await.unwrap();

    let hold2 = e.golive().group_ops.lock().await;
    let cancel_tok2 = e.current_golive_switch_cancel();
    let e_late2 = e.clone();
    let stop_task2 = tokio::spawn(async move { e_late2.trigger_delayed_stop_later_for_test().await });
    timeout(Duration::from_secs(3), cancel_tok2.cancelled()).await.unwrap();

    {
        let mut m = e.studio().machine.lock().unwrap();
        m.epoch += 1;
        m.stop_waiting = false;
    }
    drop(hold2);
    stop_task2.await.unwrap();

    assert!(e.golive_stream_running(), "stream must remain live when delayed stop exits on !still_ours");
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&a.id, DestinationQuality::Custom(q720.clone()))
        .await
        .expect("live switch must succeed instead of failing with golive_switch_cancelled");

    // Part C: Rejected `remove_golive_destination` (409 last destination / 400 built-in) while `a` awaits IDR
    // must not cancel `a`'s in-flight live switch.
    let mut p_in = e.golive_planner_input(e.cached_pc_budget());
    let q480 = CustomQuality { width: 852, height: 480, fps: 30, kbps: 1_500, codec: Some(VideoCodec::H264) };
    p_in.destinations.iter_mut().find(|x| x.id == a.id).unwrap().quality = DestinationQuality::Custom(q480.clone());
    let gid_480 = crate::golive::plan_encodes(&p_in).destinations[&a.id].group_id.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    inner.idr_gate.lock().unwrap().insert(gid_480.clone(), gate.clone());

    let e_sw = e.clone();
    let a_id = a.id.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality(&a_id, DestinationQuality::Custom(q480)).await });
    timeout(Duration::from_secs(3), async {
        while !inner.started.lock().unwrap().contains_key(&gid_480) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let err_last = e.remove_golive_destination(&a.id).await.unwrap_err();
    assert!(matches!(err_last, EngineError::Refused { status: 409, code: "golive_last_destination", .. }));
    let err_builtin = e.remove_golive_destination("youtube").await.unwrap_err();
    assert!(matches!(err_builtin, EngineError::Invalid(_)));

    gate.notify_waiters();
    sw.await.unwrap().expect("rejected remove_golive_destination must not cancel in-flight switch");
    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn reconcile_studio_groups_cancels_route_light_when_session_cap_reached() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, GROUP_LIGHT, GROUP_MAIN, VideoCodec};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let (e, _dir) = engine().await;
    let host = ivs_budget_host(1920, 1080, 60, 10_000, 2);
    *host.budget_info.lock().unwrap() = Some(crate::studio_host::HostBudgetInfo {
        os: "windows".into(),
        encoder_id: "ffmpeg_nvenc".into(),
        cpu_cores: 12,
        nvidia_driver_major: Some(555),
        hw_backend: Some("nvenc".into()),
        host_sig: "win|nv=555".into(),
        ..Default::default()
    });
    *e.studio().studio_bench.write().unwrap() = Some(crate::golive_plan::PersistedStudioBench {
        encoder_id: "ffmpeg_nvenc".into(),
        is_software: false,
        max_mpx_s: 600.0,
        max_sessions_ok: Some(3),
        measured_at: 1,
        host_sig: "win|nv=555".into(),
    });
    e.set_studio_host(host.clone());

    let (p_yt, mut got_yt) = platform("rlc-yt").await;
    let (p_b, mut got_b) = platform("rlc-b").await;
    e.register_connector(Arc::new(LocalPlatform(
        Platform::Youtube,
        format!("rtmp://127.0.0.1:{p_yt}/app"),
        "rlc-yt".into(),
    )));
    let b =
        e.add_golive_destination("DestB", &format!("rtmp://127.0.0.1:{p_b}/app"), "rlc-b", Canvas::Main).await.unwrap();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    e.set_golive_quality(&b.id, DestinationQuality::Custom(q720)).await.unwrap();
    let order: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube" || d.id == b.id)).collect();
    e.set_golive_destinations(order).await.unwrap();

    e.studio_stream(true).await.unwrap();
    for (rx, want) in [(&mut got_yt, "rlc-yt"), (&mut got_b, "rlc-b")] {
        loop {
            if matches!(timeout(Duration::from_secs(3), rx.recv()).await.unwrap(), Some(Got::Publish(k)) if k == want) {
                break;
            }
        }
    }
    let gid_b = e.golive_running_group(&b.id).unwrap();
    assert_eq!(e.golive_running_group("youtube").as_deref(), Some(GROUP_MAIN));

    // Marking YouTube `route_light` plans `[g_b, GROUP_LIGHT]` (2 sessions <= 2), while `keep_main` retains
    // `GROUP_MAIN` in `active_groups`, forcing `reconcile_studio_groups_locked` to hit `overlap == Over` and
    // cancel `route_light` + re-plan back to `GROUP_MAIN`.
    e.mark_route_light_for_test("youtube");
    e.reconcile().await;

    assert_eq!(e.golive_running_group("youtube").as_deref(), Some(GROUP_MAIN));
    assert_eq!(e.golive_running_group(&b.id).as_deref(), Some(gid_b.as_str()));
    assert!(!host.started.lock().unwrap().contains_key(GROUP_LIGHT));
    let view = e.golive_view().await;
    let yt_v = view.destinations.iter().find(|d| d.id == "youtube").unwrap();
    assert!(yt_v.blocked.is_none());
    assert!(!yt_v.lighter_for_route);

    e.studio_stream(false).await.unwrap();
}

#[tokio::test]
async fn viewers_stay_steady_during_quality_switch_drain_and_wake_on_midstream_toggle() {
    use crate::golive::{Canvas, CustomQuality, DestinationQuality, VideoCodec};
    use crate::viewers::run_with_clock;
    use crate::viewers::tests::{FakeClock, MockViewerConnector};
    use ferrocast_core::event::Platform;
    use ferrocast_testkit::rtmp::{Got, platform};
    use tokio::time::{Duration, timeout};

    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    e.stop_viewer_poller();
    e.start_golive(0).await;

    let host = ivs_budget_host(1920, 1080, 60, 9_000, 2);
    e.set_studio_host(host);

    let (p_tw, mut got_tw) = platform("vw-tw").await;
    let (p_yt, mut got_yt) = platform("vw-yt").await;

    #[derive(Debug)]
    struct PolledPlatform {
        inner: LocalPlatform,
        mock: Arc<MockViewerConnector>,
    }

    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for PolledPlatform {
        fn platform(&self) -> Platform {
            self.inner.platform()
        }
        fn capabilities(&self) -> ferrocast_core::moderation::Capabilities {
            self.inner.capabilities()
        }
        async fn send_chat(&self, t: &str, r: Option<&str>) -> ferrocast_connector::Result<()> {
            self.inner.send_chat(t, r).await
        }
        async fn delete_message(&self, id: &str) -> ferrocast_connector::Result<()> {
            self.inner.delete_message(id).await
        }
        async fn timeout(&self, u: &str, s: u32, r: Option<&str>) -> ferrocast_connector::Result<()> {
            self.inner.timeout(u, s, r).await
        }
        async fn ban(&self, u: &str, r: Option<&str>) -> ferrocast_connector::Result<()> {
            self.inner.ban(u, r).await
        }
        async fn unban(&self, u: &str) -> ferrocast_connector::Result<()> {
            self.inner.unban(u).await
        }
        async fn set_mode(&self, m: ferrocast_core::moderation::ModeChange) -> ferrocast_connector::Result<()> {
            self.inner.set_mode(m).await
        }
        async fn ingest(&self) -> ferrocast_connector::Result<ferrocast_connector::Ingest> {
            self.inner.ingest().await
        }
        async fn viewer_count(&self) -> ferrocast_connector::Result<Option<u64>> {
            self.mock.viewer_count().await
        }
    }

    let tw_mock = MockViewerConnector::new(Platform::Twitch, Ok(Some(42)));
    let yt_mock = MockViewerConnector::new(Platform::Youtube, Ok(Some(19)));
    e.register_connector(Arc::new(PolledPlatform {
        inner: LocalPlatform(Platform::Twitch, format!("rtmp://127.0.0.1:{p_tw}/app"), "vw-tw".into()),
        mock: tw_mock.clone(),
    }));
    e.register_connector(Arc::new(PolledPlatform {
        inner: LocalPlatform(Platform::Youtube, format!("rtmp://127.0.0.1:{p_yt}/app"), "vw-yt".into()),
        mock: yt_mock.clone(),
    }));

    // Start with only YouTube enabled so Twitch starts inactive.
    let only_yt: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt).await.unwrap();
    e.studio_stream(true).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_yt.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "vw-yt")
        {
            break;
        }
    }

    let clock = FakeClock::new();
    let stop = Arc::new(tokio::sync::Notify::new());
    let poller = tokio::spawn(run_with_clock(e.clone(), clock.clone(), stop.clone()));

    let (g0, dl0) = clock.wait_sleep_after(0).await;
    assert_eq!(dl0, Some(clock.origin + Duration::from_secs(10)));
    clock.advance(Duration::from_secs(10));
    let (g1, dl1) = clock.wait_sleep_after(g0).await;
    assert_eq!(yt_mock.calls(), 1);
    assert_eq!(dl1, Some(clock.origin + Duration::from_secs(70)), "YouTube sleeps 60s before next poll");

    // Tick Twitch mid-stream -> golive must wake the viewer poller immediately to schedule Twitch's 10s poll.
    let yt_and_tw: Vec<(String, bool)> = e
        .golive_config()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "youtube" || d.id == "twitch"))
        .collect();
    e.set_golive_destinations(yt_and_tw).await.unwrap();
    loop {
        if matches!(timeout(Duration::from_secs(3), got_tw.recv()).await.unwrap(), Some(Got::Publish(k)) if k == "vw-tw")
        {
            break;
        }
    }
    let (g2, dl2) = timeout(Duration::from_secs(2), clock.wait_sleep_after(g1))
        .await
        .expect("mid-stream destination start must wake viewer poller immediately");
    assert_eq!(dl2, Some(clock.origin + Duration::from_secs(20)), "Twitch initial poll due in 10s");

    clock.advance(Duration::from_secs(10));
    let (g3, _) = clock.wait_sleep_after(g2).await;
    assert_eq!(tw_mock.calls(), 1);
    eventually_async(|| async { e.state().viewers.get(&Platform::Twitch).copied() == Some(42) }).await;

    // During a live quality-switch drain on Twitch, viewer count must stay steady and never be cleared.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    e.arm_slow_drain_for_test("twitch", entered.clone(), release.clone());
    let mut overlays = e.subscribe_overlays();
    let q720 = CustomQuality { width: 1280, height: 720, fps: 30, kbps: 3_500, codec: Some(VideoCodec::H264) };
    let e_sw = e.clone();
    let sw = tokio::spawn(async move { e_sw.set_golive_quality("twitch", DestinationQuality::Custom(q720)).await });
    timeout(Duration::from_secs(3), entered.notified()).await.expect("twitch switch entered drain");

    e.wake_viewer_poller();
    let (g4, _) = timeout(Duration::from_secs(2), clock.wait_sleep_after(g3)).await.unwrap();
    assert_eq!(
        e.state().viewers.get(&Platform::Twitch).copied(),
        Some(42),
        "viewers must not flicker to None during live quality-switch drain"
    );
    while let Ok(f) = overlays.try_recv() {
        let v: serde_json::Value = serde_json::from_str(&f.text).unwrap();
        if let Some(viewers_obj) = v["data"]["viewers"].as_object()
            && viewers_obj.contains_key("twitch")
        {
            assert!(!viewers_obj["twitch"].is_null(), "unexpected viewer clear frame during drain: {v}");
        }
    }

    release.notify_waiters();
    sw.await.unwrap().unwrap();
    let (g_post_sw, _) = timeout(Duration::from_secs(2), clock.wait_sleep_after(g4)).await.unwrap();

    // Unticking Twitch mid-stream must wake the viewer poller immediately and clear its viewers.
    let only_yt2: Vec<(String, bool)> =
        e.golive_config().destinations.into_iter().map(|d| (d.id.clone(), d.id == "youtube")).collect();
    e.set_golive_destinations(only_yt2).await.unwrap();
    let (_g5, _) = timeout(Duration::from_secs(2), clock.wait_sleep_after(g_post_sw))
        .await
        .expect("mid-stream destination stop must wake viewer poller immediately");
    assert_eq!(e.state().viewers.get(&Platform::Twitch).copied(), None);

    stop.notify_one();
    poller.await.unwrap();
    e.studio_stream(false).await.unwrap();
    let _ = Canvas::Main;
}
