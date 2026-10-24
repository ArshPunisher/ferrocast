//! End to end on this machine: a fake OBS publishes to the real [`Ingest`], and [`push::run`]
//! forwards to fake platform servers. Both fakes are rml_rtmp sessions, so the bytes on the wire
//! are real RTMP.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rml_rtmp::handshake::PeerType;
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType, ServerSession,
    ServerSessionConfig, ServerSessionEvent, ServerSessionResult, StreamMetadata,
};
use rml_rtmp::time::RtmpTimestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use crate::conn;
use crate::hub::Hub;
use crate::ingest::Ingest;
use crate::push::{self, Destination, Health, PushState};
use crate::target::Target;

const WAIT: Duration = Duration::from_secs(5);
const OBS_KEY: &str = "obs-key-7f3a";

/// What a fake platform saw, in order.
#[derive(Debug, Clone, PartialEq)]
enum Got {
    Publish(String),
    Meta,
    Video(u32, Bytes),
    Audio(u32, Bytes),
    Closed,
}

/// A fake platform: accepts any app, and publishing only with `key`.
async fn platform(listener: TcpListener, key: &'static str) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = serve_platform(&mut sock, key, &tx).await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    rx
}

async fn serve_platform(sock: &mut TcpStream, key: &str, tx: &mpsc::UnboundedSender<Got>) -> Option<()> {
    let rest = conn::handshake(sock, PeerType::Server).await.ok()?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue: Vec<ServerSessionResult> = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    loop {
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await.ok()?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                            queue.extend(s.accept_request(request_id).ok()?)
                        }
                        ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
                            if stream_key == key {
                                let _ = tx.send(Got::Publish(stream_key));
                                queue.extend(s.accept_request(request_id).ok()?);
                            } else {
                                // Like Twitch: refuse, then hang up.
                                for r in s.reject_request(request_id, "NetStream.Publish.BadName", "bad key").ok()? {
                                    if let ServerSessionResult::OutboundResponse(p) = r {
                                        sock.write_all(&p.bytes).await.ok()?;
                                    }
                                }
                                return None;
                            }
                        }
                        ServerSessionEvent::StreamMetadataChanged { .. } => {
                            let _ = tx.send(Got::Meta);
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Got::Video(timestamp.value, data));
                        }
                        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                            let _ = tx.send(Got::Audio(timestamp.value, data));
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

/// A fake OBS, publishing to the ingest.
struct Obs {
    sock: TcpStream,
    c: ClientSession,
}

impl Obs {
    async fn publish(port: u16, app: &str, key: &str) -> Result<Self, String> {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let rest = conn::handshake(&mut sock, PeerType::Client).await.map_err(|e| e.to_string())?;
        let (c, first) = ClientSession::new(ClientSessionConfig::new()).map_err(|e| e.to_string())?;
        let mut obs = Self { sock, c };
        obs.send(first).await?;
        let r = obs.c.handle_input(&rest).map_err(|e| e.to_string())?;
        obs.send(r).await?;
        let r = obs.c.request_connection(app.to_string()).map_err(|e| e.to_string())?;
        obs.send(vec![r]).await?;
        obs.until(|e| matches!(e, ClientSessionEvent::ConnectionRequestAccepted)).await?;
        let r = obs.c.request_publishing(key.to_string(), PublishRequestType::Live).map_err(|e| e.to_string())?;
        obs.send(vec![r]).await?;
        obs.until(|e| matches!(e, ClientSessionEvent::PublishRequestAccepted)).await?;
        Ok(obs)
    }

    async fn send(&mut self, results: Vec<ClientSessionResult>) -> Result<Vec<ClientSessionEvent>, String> {
        let mut events = Vec::new();
        for r in results {
            match r {
                ClientSessionResult::OutboundResponse(p) => {
                    self.sock.write_all(&p.bytes).await.map_err(|e| e.to_string())?
                }
                ClientSessionResult::RaisedEvent(e) => events.push(e),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(events)
    }

    async fn until(&mut self, want: impl Fn(&ClientSessionEvent) -> bool) -> Result<(), String> {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = timeout(WAIT, self.sock.read(&mut buf))
                .await
                .map_err(|_| "timed out".to_string())?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("closed".into());
            }
            let r = self.c.handle_input(&buf[..n]).map_err(|e| e.to_string())?;
            for e in self.send(r).await? {
                if want(&e) {
                    return Ok(());
                }
                if let ClientSessionEvent::ConnectionRequestRejected { description } = &e {
                    return Err(format!("rejected: {description}"));
                }
                if let ClientSessionEvent::UnhandleableOnStatusCode { code } = &e {
                    return Err(format!("refused: {code}"));
                }
            }
        }
    }

    async fn meta(&mut self) {
        let mut m = StreamMetadata::new();
        m.video_width = Some(1920);
        m.video_height = Some(1080);
        let r = self.c.publish_metadata(&m).unwrap();
        self.send(vec![r]).await.unwrap();
    }

    async fn video(&mut self, ts: u32, data: &'static [u8]) {
        let r = self.c.publish_video_data(Bytes::from_static(data), RtmpTimestamp::new(ts), false).unwrap();
        self.send(vec![r]).await.unwrap();
    }

    async fn audio(&mut self, ts: u32, data: &'static [u8]) {
        let r = self.c.publish_audio_data(Bytes::from_static(data), RtmpTimestamp::new(ts), false).unwrap();
        self.send(vec![r]).await.unwrap();
    }
}

const AVC_HEADER: &[u8] = &[0x17, 0x00, 0, 0, 0, 1, 0x64];
const AAC_HEADER: &[u8] = &[0xAF, 0x00, 0x12, 0x10];
const KEY: &[u8] = &[0x17, 0x01, 0, 0, 0, 0xAA];
const INTER: &[u8] = &[0x27, 0x01, 0, 0, 0, 0xBB];
const AAC: &[u8] = &[0xAF, 0x01, 0x21, 0x42];

struct World {
    hub: Arc<Hub>,
    port: u16,
    stop: watch::Sender<bool>,
}

async fn world() -> World {
    let hub = Hub::new();
    let ingest = Ingest::bind(0, OBS_KEY, hub.clone()).await.unwrap();
    assert!(ingest.server_url().starts_with("rtmp://127.0.0.1:"));
    let port = ingest.port();
    let (stop, rx) = watch::channel(false);
    tokio::spawn(ingest.run(rx));
    World { hub, port, stop }
}

fn destination(port: u16, key: &str) -> Destination {
    Destination {
        id: "test".into(),
        target: Target::parse(&format!("rtmp://127.0.0.1:{port}/app")).unwrap(),
        key: key.into(),
    }
}

fn start(w: &World, d: Destination) -> watch::Receiver<Health> {
    let (tx, rx) = watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
    tokio::spawn(push::run(d, w.hub.clone(), tx, w.stop.subscribe()));
    rx
}

async fn state_is(h: &mut watch::Receiver<Health>, want: impl Fn(&PushState) -> bool) -> PushState {
    timeout(WAIT, h.wait_for(|h| want(&h.state))).await.expect("state in time").expect("health open").state.clone()
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    timeout(WAIT, rx.recv()).await.expect("platform got something in time").expect("platform open")
}

async fn free_listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

#[tokio::test]
async fn forwards_headers_then_packets_from_a_keyframe() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform(l, "dest-key").await;
    let mut health = start(&w, destination(dport, "dest-key"));
    assert_eq!(health.borrow().state, PushState::Waiting);

    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("dest-key".into()));

    obs.meta().await;
    obs.video(0, AVC_HEADER).await;
    obs.audio(0, AAC_HEADER).await;
    obs.video(900, INTER).await; // before any keyframe: skipped
    obs.audio(905, AAC).await; // also skipped
    obs.video(1000, KEY).await;
    obs.audio(1010, AAC).await;
    obs.video(1033, INTER).await;

    assert_eq!(next(&mut got).await, Got::Meta);
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
    assert_eq!(next(&mut got).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    assert_eq!(next(&mut got).await, Got::Video(1000, Bytes::from_static(KEY)));
    assert_eq!(next(&mut got).await, Got::Audio(1010, Bytes::from_static(AAC)));
    assert_eq!(next(&mut got).await, Got::Video(1033, Bytes::from_static(INTER)));
}

#[tokio::test]
async fn a_late_destination_gets_the_headers_and_starts_at_a_keyframe() {
    let w = world().await;
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    obs.meta().await;
    obs.video(0, AVC_HEADER).await;
    obs.audio(0, AAC_HEADER).await;
    obs.video(40, KEY).await;
    obs.video(73, INTER).await;
    // Let the ingest take them in before the destination subscribes.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, destination(dport, "k"));
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    assert_eq!(next(&mut got).await, Got::Meta);
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
    assert_eq!(next(&mut got).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));

    obs.video(5000, INTER).await; // mid-picture: skipped
    obs.video(5100, KEY).await;
    obs.video(5133, INTER).await;
    // Timestamps start at zero for this destination.
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(KEY)));
    assert_eq!(next(&mut got).await, Got::Video(33, Bytes::from_static(INTER)));
}

#[tokio::test]
async fn the_wrong_stream_key_or_app_is_refused() {
    let w = world().await;
    // A wrong key is refused and disconnected, as the platforms do.
    assert!(Obs::publish(w.port, "live", "guess").await.is_err());
    let err = Obs::publish(w.port, "other", OBS_KEY).await.err().unwrap();
    assert!(err.contains("rejected"), "{err}");
    assert!(!w.hub.live().borrow().on);
}

#[tokio::test]
async fn only_one_publisher_at_a_time() {
    let w = world().await;
    let first = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    assert!(Obs::publish(w.port, "live", OBS_KEY).await.is_err());
    assert!(w.hub.live().borrow().on, "the first publisher keeps going");
    drop(first);
    // Once the first is gone, a new one may publish.
    let mut live = w.hub.live();
    timeout(WAIT, live.wait_for(|l| !l.on)).await.unwrap().unwrap();
    Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
}

#[tokio::test]
async fn a_second_key_feeds_its_own_hub() {
    let (main, vertical) = (Hub::new(), Hub::new());
    let ingest = Ingest::bind(0, OBS_KEY, main.clone()).await.unwrap().with_route("obs-key-vertical", vertical.clone());
    let port = ingest.port();
    let (_stop, rx) = watch::channel(false);
    tokio::spawn(ingest.run(rx));
    let _v = Obs::publish(port, "live", "obs-key-vertical").await.unwrap();
    assert!(vertical.live().borrow().on);
    assert!(!main.live().borrow().on, "the vertical stream doesn't start the main one");
    // Both at once, each one publisher.
    let _m = Obs::publish(port, "live", OBS_KEY).await.unwrap();
    assert!(main.live().borrow().on);
    assert!(Obs::publish(port, "live", "obs-key-vertical").await.is_err());
    assert!(Obs::publish(port, "live", "obs-key-vertical-x").await.is_err());
}

#[tokio::test]
async fn a_refused_destination_key_says_so_and_keeps_trying() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let _got = platform(l, "right").await;
    let mut health = start(&w, destination(dport, "wrong"));
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    match state_is(&mut health, |s| matches!(s, PushState::Reconnecting { .. })).await {
        PushState::Reconnecting { attempt, reason, retry_in_ms } => {
            assert_eq!(attempt, 1);
            assert!(reason.contains("refused the stream key"), "{reason}");
            assert!((800..=1200).contains(&retry_in_ms), "{retry_in_ms}");
        }
        s => panic!("{s:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_destination_retries_until_it_answers() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    drop(l); // nothing listens there yet
    let mut health = start(&w, destination(dport, "k"));
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    match state_is(&mut health, |s| matches!(s, PushState::Reconnecting { .. })).await {
        PushState::Reconnecting { reason, .. } => assert!(reason.contains("Couldn't reach 127.0.0.1"), "{reason}"),
        s => panic!("{s:?}"),
    }
    // The platform comes back; the next try goes live without anyone pressing anything.
    let mut got = platform(TcpListener::bind(("127.0.0.1", dport)).await.unwrap(), "k").await;
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    obs.video(10, KEY).await;
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(KEY)));
}

#[tokio::test]
async fn obs_stopping_ends_the_stream_everywhere_and_waits_for_the_next() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, destination(dport, "k"));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    drop(obs);
    assert_eq!(next(&mut got).await, Got::Closed);
    state_is(&mut health, |s| *s == PushState::Waiting).await;
    // OBS starts again: the destination publishes again.
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
}

#[tokio::test]
async fn stop_ends_destinations_and_the_ingest() {
    let w = world().await;
    let mut health = start(&w, destination(1, "k"));
    w.stop.send_replace(true);
    state_is(&mut health, |s| *s == PushState::Stopped).await;
    // The ingest stops accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(Obs::publish(w.port, "live", OBS_KEY).await.is_err());
}

#[test]
fn backoff_grows_then_caps_with_jitter() {
    for (attempt, base) in [(1, 1.0), (2, 2.0), (3, 4.0), (5, 16.0), (6, 30.0), (40, 30.0)] {
        let d = push::backoff(attempt).as_secs_f64();
        assert!(d >= base * 0.8 - 1e-9 && d <= base * 1.2 + 1e-9, "attempt {attempt}: {d}");
    }
}

#[test]
fn keys_compare_whole() {
    assert!(conn::same_secret("abc", "abc"));
    assert!(!conn::same_secret("abc", "abd"));
    assert!(!conn::same_secret("abc", "abcd"));
    assert!(!conn::same_secret("", "a"));
}

#[test]
fn the_destination_key_never_shows_in_debug() {
    let d = destination(1935, "super-secret-key");
    assert!(!format!("{d:?}").contains("super-secret-key"));
}

#[tokio::test]
async fn a_quick_restart_is_followed_not_missed() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, destination(dport, "k"));
    let obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    // OBS reconnects straight away: the stream number moves on before the destination looks.
    drop(obs);
    let mut live = w.hub.live();
    timeout(WAIT, live.wait_for(|l| !l.on)).await.unwrap().unwrap();
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    assert_eq!(w.hub.live().borrow().stream, 2);
    assert_eq!(next(&mut got).await, Got::Closed);
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    // Like OBS, keep sending keyframes; the destination starts at the first one it sees.
    let mut ts = 0;
    let first = loop {
        obs.video(ts, KEY).await;
        ts += 2000;
        if let Ok(Some(g)) = timeout(Duration::from_millis(200), got.recv()).await {
            break g;
        }
        assert!(ts < 20 * 2000, "no keyframe got through");
    };
    assert_eq!(first, Got::Video(0, Bytes::from_static(KEY)));
}

#[tokio::test]
async fn delay_and_upload_tests_reach_a_real_rtmp_server() {
    let l = free_listener().await;
    let port = l.local_addr().unwrap().port();
    let key = format!("live_1{}", crate::probe::TWITCH_BANDWIDTH_TEST);
    let mut got = platform(l, "live_1?bandwidthtest=true").await;
    let t = Target::parse(&format!("rtmp://127.0.0.1:{port}/app")).unwrap();
    let d = crate::probe::delay(&t).await.unwrap();
    assert!(d < Duration::from_secs(1), "{d:?}");
    let up = tokio::spawn(async move { crate::probe::upload(&t, &key, Duration::from_millis(1500)).await });
    // The delay test's bare connections close first.
    let mut first = next(&mut got).await;
    while first == Got::Closed {
        first = next(&mut got).await;
    }
    assert_eq!(first, Got::Publish("live_1?bandwidthtest=true".into()));
    drop(got); // the fake platform stops keeping packets
    let kbps = up.await.unwrap().unwrap();
    assert!(kbps > 1_000, "{kbps}");
    // Nothing listening: a plain reason.
    let gone = Target::parse("rtmp://127.0.0.1:1/app").unwrap();
    assert!(crate::probe::delay(&gone).await.unwrap_err().to_string().contains("Couldn't reach"));
}

/// A fake platform that accepts the stream, then hangs up `after` the publish was accepted
/// (as Kick does with a stream over its bitrate limit).
async fn platform_that_drops(
    listener: TcpListener,
    key: &'static str,
    after: Duration,
) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (itx, mut irx) = mpsc::unbounded_channel();
                let serve = serve_platform(&mut sock, key, &itx);
                let drop_after_publish = async {
                    while let Some(g) = irx.recv().await {
                        let published = matches!(g, Got::Publish(_));
                        let _ = tx.send(g);
                        if published {
                            break;
                        }
                    }
                    tokio::time::sleep(after).await;
                };
                tokio::select! {
                    _ = serve => {}
                    _ = drop_after_publish => {}
                }
                let _ = tx.send(Got::Closed);
            });
        }
    });
    rx
}

#[tokio::test]
async fn a_platform_that_keeps_dropping_a_live_stream_is_retried_from_the_first_wait() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let _got = platform_that_drops(l, "k", Duration::from_millis(400)).await;
    let mut health = start(&w, destination(dport, "k"));
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    for _ in 0..2 {
        state_is(&mut health, |s| *s == PushState::Live).await;
        match state_is(&mut health, |s| matches!(s, PushState::Reconnecting { .. })).await {
            PushState::Reconnecting { attempt, .. } => assert_eq!(attempt, 1, "the count starts over after being live"),
            s => panic!("{s:?}"),
        }
    }
}

/// ffmpeg on this machine, for the cut-from-stream tests (skipped where there's none).
fn ffmpeg() -> Option<std::path::PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
}

#[tokio::test]
async fn the_cutter_makes_an_upright_stream_from_the_normal_one() {
    use crate::cut::{CutConfig, Encoder};
    use crate::cutter::{self, CutState};
    let Some(ff) = ffmpeg() else { return };
    let (main, vertical) = (Hub::new(), Hub::new());
    let ingest = Ingest::bind(0, OBS_KEY, main.clone()).await.unwrap().with_route("vkey-test", vertical.clone());
    let port = ingest.port();
    let (stop, rx) = watch::channel(false);
    tokio::spawn(ingest.run(rx));
    let (state_tx, mut state) = watch::channel(CutState::Waiting);
    let publish = format!("rtmp://127.0.0.1:{port}/live/vkey-test");
    let enc = vec![Encoder { name: "libx264", on: "the processor" }];
    tokio::spawn(cutter::run(ff.clone(), main.clone(), CutConfig::default(), enc, publish, state_tx, stop.subscribe()));
    // "OBS": a real H.264 picture, 1920×1080, to the normal key.
    let mut obs = tokio::process::Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "15"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "60", "-c:a", "aac", "-f", "flv"])
        .arg(format!("rtmp://127.0.0.1:{port}/live/{OBS_KEY}"))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut vlive = vertical.live();
    timeout(Duration::from_secs(20), vlive.wait_for(|l| l.on)).await.expect("vertical stream in time").unwrap();
    // Its size shows once a few packets are in.
    let mut size = None;
    for _ in 0..40 {
        let s = vertical.stats();
        if let (Some(w), Some(h)) = (s.width, s.height) {
            size = Some((w, h));
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(size, Some((1080, 1920)));
    timeout(Duration::from_secs(10), state.wait_for(|s| matches!(s, CutState::Running { .. })))
        .await
        .expect("running in time")
        .unwrap();
    // OBS stops: the vertical stream ends too, and the cutter waits for the next one.
    let _ = obs.kill().await;
    timeout(Duration::from_secs(15), vlive.wait_for(|l| !l.on)).await.expect("vertical ended").unwrap();
    timeout(Duration::from_secs(10), state.wait_for(|s| *s == CutState::Waiting)).await.unwrap().unwrap();
    stop.send_replace(true);
}
