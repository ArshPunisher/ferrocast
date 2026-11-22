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
    let _keep = keep_streaming(e.golive().hub.clone(), 20_000, 4_000);
    e.set_studio_live(true);
    e.sync_stream_cap().await;
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
    let _keep = keep_streaming(e.golive().hub.clone(), 175_000, 4_000);
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
    let _keep = keep_streaming(e.golive().hub.clone(), 77_000, 4_000);
    eventually_async(|| async { e.golive().hub.stats().kbps > 5_000 }).await;
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
        Ok(serde_json::Value::Null)
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
    use ferrocast_core::event::Platform;
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

    #[derive(Debug)]
    struct LocalKick(String);
    #[async_trait::async_trait]
    impl ferrocast_connector::Connector for LocalKick {
        fn platform(&self) -> Platform {
            Platform::Kick
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
            Ok(ferrocast_connector::Ingest { url: self.0.clone(), key: "kick-k".into(), backup: None, note: None })
        }
    }

    let (e, _dir) = engine().await;
    let host = Arc::new(LiveHost::default());
    e.set_studio_host(host.clone());

    // 1. Kick alone, refusing multistreaming (accept then close after 40 ms): gives up and
    //    stops the Studio stream so the Studio stops showing LIVE.
    let (kport, _kgot) = platform_dropping("kick-k", std::time::Duration::from_millis(40)).await;
    e.register_connector(Arc::new(LocalKick(format!("rtmp://127.0.0.1:{kport}/app"))));
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
            assert!(reason.contains("Kick refused"), "{reason}");
            assert!(detail.as_deref().is_some_and(|d| d.contains("Allow multistreaming")), "{detail:?}");
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
    push_stream(&g.hub, 150_000); // ~12,000 kbps
    push_stream(&g.lhub, 37_500); // ~3,000 kbps
    let feeds = [keep_streaming(g.hub.clone(), 150_000, 4_000), keep_streaming(g.lhub.clone(), 37_500, 4_000)];

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
    cfg.destinations.iter_mut().find(|d| d.id == "twitch").unwrap().light = false;
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
    let hub = &e.golive().hub;
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
    assert_eq!(kick.capped.as_ref().map(|c| c.summary.as_str()), Some("Capped at 8,000 kbps for Kick"));

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
    let hub = &e.golive().hub;
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
    let (e, _dir) = engine().await;
    let studio = Arc::new(DirectStudio::default());
    *studio.settings.lock().unwrap() = twitch_studio_settings();
    e.set_studio_host(studio.clone());
    let both: Vec<(String, bool)> = GoLiveConfig::default()
        .destinations
        .into_iter()
        .map(|d| (d.id.clone(), d.id == "twitch" || d.id == "youtube"))
        .collect();
    e.set_golive_destinations(both).await.unwrap();
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000), "Twitch on, as far as anyone knows");
    // It couldn't start (no account): the stream is YouTube's alone.
    e.golive().blocked.lock().unwrap().insert("twitch".into(), "Connect Twitch first.".into());
    assert_eq!(e.studio_limit(), None);
    e.sync_stream_cap().await;
    assert_eq!(studio.caps.lock().unwrap().last().copied().flatten(), None, "lifted for the stream on air");
    // It can: the limit is back.
    e.golive().blocked.lock().unwrap().clear();
    assert_eq!(e.studio_limit().map(|l| l.max_kbps), Some(6_000));
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
    let feed = keep_streaming(e.golive().hub.clone(), 90_000, 4_000);
    let twitch = |v: &crate::golive::GoLiveView| v.destinations.iter().find(|d| d.id == "twitch").cloned().unwrap();
    let v = e.golive_view().await;
    assert_eq!(twitch(&v).capped, None, "not claimed");
    assert_eq!(twitch(&v).issues.len(), 1, "{:?}", twitch(&v).issues);
    assert_eq!(twitch(&v).issues[0].code, ferrocast_golive::probe::RuleCode::Bitrate);
    // Applied again (the recording stopped): held, as before.
    *studio.cap_refused.lock().unwrap() = None;
    e.sync_stream_cap().await;
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
    let _keep = keep_streaming(e.golive().hub.clone(), 20_000, 4_000);
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
