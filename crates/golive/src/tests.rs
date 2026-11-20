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
    serve_platform_with(sock, key, tx, false).await
}

/// As [`serve_platform`]; with `hang_up_on_publish` it takes the handshake and the connect, then
/// closes the socket at the publish without a word (an ingest node draining, or Twitch on a key).
async fn serve_platform_with(
    sock: &mut TcpStream,
    key: &str,
    tx: &mpsc::UnboundedSender<Got>,
    hang_up_on_publish: bool,
) -> Option<()> {
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
                            if hang_up_on_publish {
                                return None;
                            }
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
        retry: Default::default(),
        extra_audio: false,
        extra_video: false,
        backup: None,
    }
}

fn start(w: &World, d: Destination) -> watch::Receiver<Health> {
    let (tx, rx) = watch::channel(Health {
        state: PushState::Waiting,
        kbps: 0,
        dropped: 0,
        sent: 0,
        link: None,
        via: None,
        since_ms: 0,
    });
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

/// The Studio feeds its hub itself (no OBS): its `onMetaData` goes first too, as OBS's does.
/// Kick hung up every few seconds on a stream without one (15 Nov).
#[tokio::test]
async fn a_stream_made_here_sends_its_metadata_before_anything() {
    let w = world().await;
    w.hub.start();
    let mut m = StreamMetadata::new();
    m.video_width = Some(1920);
    m.video_height = Some(1080);
    m.video_frame_rate = Some(60.0);
    w.hub.metadata(m);
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));
    assert_eq!(w.hub.stats().width, Some(1920));

    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, destination(dport, "k"));
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    assert_eq!(next(&mut got).await, Got::Meta);
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
}

/// A picture changed while live (Optimize): a platform rejoining after it is told the new size,
/// not the one the stream began with.
#[tokio::test]
async fn metadata_changed_reaches_those_who_join_later() {
    let w = world().await;
    w.hub.start();
    let mut m = StreamMetadata::new();
    m.video_width = Some(1920);
    m.video_height = Some(1080);
    m.audio_sample_rate = Some(48_000);
    w.hub.metadata(m);
    w.hub.metadata_changed(|m| {
        m.video_width = Some(1280);
        m.video_height = Some(720);
    });
    assert_eq!(w.hub.stats().width, Some(1280));
    let kept = w.hub.subscribe().headers.metadata.unwrap();
    assert_eq!((kept.video_width, kept.video_height, kept.audio_sample_rate), (Some(1280), Some(720), Some(48_000)));
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

/// A router reboot or a Wi-Fi drop of a minute: connecting fails at once (no route), over and over.
/// That is not the platform failing, so the tries aren't counted against it: the destination waits
/// for the network (still Reconnecting, never Failed) and goes live again by itself, and a later
/// ordinary failure still has all its own tries.
/// A platform whose first connection is taken and dropped after `live` (a long stream, then the
/// network or the node goes), whose next `resets` connections hang up at the publish, and which
/// then takes the stream for good.
async fn flaky_platform(
    listener: TcpListener,
    key: &'static str,
    live: Duration,
    resets: usize,
) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut n = 0usize;
        while let Ok((mut sock, _)) = listener.accept().await {
            let (tx, this) = (tx.clone(), n);
            n += 1;
            tokio::spawn(async move {
                if (1..=resets).contains(&this) {
                    let _ = serve_platform_with(&mut sock, key, &tx, true).await;
                } else if this == 0 {
                    let (itx, mut irx) = mpsc::unbounded_channel();
                    let serve = serve_platform(&mut sock, key, &itx);
                    let drop_later = async {
                        while let Some(g) = irx.recv().await {
                            let published = matches!(g, Got::Publish(_));
                            let _ = tx.send(g);
                            if published {
                                break;
                            }
                        }
                        tokio::time::sleep(live).await;
                    };
                    tokio::select! { _ = serve => {} _ = drop_later => {} }
                } else {
                    let _ = serve_platform(&mut sock, key, &tx).await;
                }
                let _ = tx.send(Got::Closed);
            });
        }
    });
    rx
}

/// An ingest node that drains hangs up at the publish, over and over, while a stream that was live
/// is reconnecting. That is the server dropping the connection, not the key being refused: the key
/// worked a moment ago, so it is retried with all the ordinary tries (and the backup ingest),
/// and it is not reported as a refused key.
#[tokio::test]
async fn a_hang_up_at_the_publish_after_the_stream_was_live_is_not_a_refused_key() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = flaky_platform(l, "k", Duration::from_millis(300), 4).await;
    let mut d = destination(dport, "k");
    d.retry = push::Retry { first: Duration::from_millis(20), max: Duration::from_millis(60), max_attempts: 6 };
    let health = start(&w, d);
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    // The first connection takes the stream; then it drops, four hang-ups follow at the publish;
    // the sixth connection is taken. Never Failed on the way.
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    loop {
        let state = health.borrow().state.clone();
        if let PushState::Failed { reason, refused, .. } = state {
            panic!("gave up (refused {refused}): {reason}");
        }
        match next(&mut got).await {
            Got::Publish(k) => {
                assert_eq!(k, "k");
                break;
            }
            _ => continue,
        }
    }
    let mut health = health;
    state_is(&mut health, |s| *s == PushState::Live).await;
}

/// A server that took the stream and then stops accepting connections for a while (an ingest
/// restarting; one's own server rebooting) is an outage, waited for like a network that is down:
/// a destination that never took the stream still gives up on a wrong address in a few tries.
#[tokio::test]
async fn a_server_that_was_live_and_refuses_connections_for_a_while_is_waited_for() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let (tx, mut got) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        while let Ok((mut sock, _)) = l.accept().await {
            let tx = tx.clone();
            conns.spawn(async move {
                let _ = serve_platform(&mut sock, "k", &tx).await;
            });
        }
    });
    let mut d = destination(dport, "k");
    d.retry = push::Retry { first: Duration::from_millis(20), max: Duration::from_millis(60), max_attempts: 3 };
    let mut health = start(&w, d);
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    // The server goes: its listener and connections close; nothing listens for a while.
    server.abort();
    let _ = server.await;
    let state = state_is(&mut health, |s| {
        matches!(s, PushState::Failed { .. }) || matches!(s, PushState::Reconnecting { attempt, .. } if *attempt >= 8)
    })
    .await;
    assert!(matches!(state, PushState::Reconnecting { .. }), "gave up on a server that had been live: {state:?}");
    // It comes back.
    let mut got = platform(TcpListener::bind(("127.0.0.1", dport)).await.unwrap(), "k").await;
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    obs.video(10, KEY).await;
}

#[tokio::test]
async fn a_network_that_is_down_is_waited_for_not_counted_against_the_destination() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    drop(l);
    push::fault::set(dport, true);
    let mut d = destination(dport, "k");
    d.retry = push::Retry { first: Duration::from_millis(20), max: Duration::from_millis(60), max_attempts: 3 };
    let mut health = start(&w, d);
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    // Well past the 3 tries an ordinary refusal would get.
    let state = state_is(&mut health, |s| {
        matches!(s, PushState::Failed { .. }) || matches!(s, PushState::Reconnecting { attempt, .. } if *attempt >= 8)
    })
    .await;
    match state {
        PushState::Reconnecting { reason, .. } => assert!(reason.to_lowercase().contains("network"), "{reason}"),
        s => panic!("gave up while the network was down: {s:?}"),
    }
    // The network comes back with the platform: live by itself.
    push::fault::set(dport, false);
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
        let d = push::backoff(attempt, &push::Retry::default()).as_secs_f64();
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
async fn a_platform_that_takes_the_stream_and_drops_it_at_once_waits_longer_each_time() {
    // 19 Nov, live: Kick took the stream and closed it at once, every 8 s for the whole
    // stream, always "attempt 1". A session live for only a moment doesn't start the count
    // over (one live a while does: STAYED_LIVE_MS), so the waits grow as with any failure.
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let _got = platform_that_drops(l, "k", Duration::from_millis(400)).await;
    let mut health = start(&w, destination(dport, "k"));
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    for want in 1..=2u32 {
        state_is(&mut health, |s| *s == PushState::Live).await;
        match state_is(&mut health, |s| matches!(s, PushState::Reconnecting { .. })).await {
            PushState::Reconnecting { attempt, .. } => assert_eq!(attempt, want, "the count goes on"),
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

/// Twitch's VOD track as OBS sends it: Enhanced RTMP multitrack audio, track 1 (header, frame).
const VOD_HEADER: &[u8] = b"\x95\x00mp4a\x01\x12\x10";
const VOD_FRAME: &[u8] = b"\x95\x01mp4a\x01\x21\x22";
const AAC_FRAME: &[u8] = &[0xAF, 0x01, 0x21, 0x22];

#[tokio::test]
async fn only_twitch_gets_the_vod_track_and_late_it_gets_its_header() {
    let w = world().await;
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    obs.meta().await;
    obs.video(0, AVC_HEADER).await;
    obs.audio(0, AAC_HEADER).await;
    obs.audio(0, VOD_HEADER).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Joining now: "Twitch" (extra tracks) and another platform.
    let (lt, lo) = (free_listener().await, free_listener().await);
    let (pt, po) = (lt.local_addr().unwrap().port(), lo.local_addr().unwrap().port());
    let (mut twitch, mut other) = (platform(lt, "t").await, platform(lo, "o").await);
    let mut ht = start(&w, Destination { extra_audio: true, ..destination(pt, "t") });
    let mut ho = start(&w, destination(po, "o"));
    state_is(&mut ht, |s| *s == PushState::Live).await;
    state_is(&mut ho, |s| *s == PushState::Live).await;
    for rx in [&mut twitch, &mut other] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Meta);
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }
    // The late header of the VOD track: Twitch only.
    assert_eq!(next(&mut twitch).await, Got::Audio(0, Bytes::from_static(VOD_HEADER)));

    obs.video(1000, KEY).await;
    obs.audio(1010, AAC_FRAME).await;
    obs.audio(1010, VOD_FRAME).await;
    obs.video(1040, INTER).await;
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(KEY)));
    assert_eq!(next(&mut twitch).await, Got::Audio(10, Bytes::from_static(AAC_FRAME)));
    assert_eq!(next(&mut twitch).await, Got::Audio(10, Bytes::from_static(VOD_FRAME)));
    assert_eq!(next(&mut other).await, Got::Video(0, Bytes::from_static(KEY)));
    assert_eq!(next(&mut other).await, Got::Audio(10, Bytes::from_static(AAC_FRAME)));
    // The other platform never sees the VOD track: next is the picture.
    assert_eq!(next(&mut other).await, Got::Video(40, Bytes::from_static(INTER)));
}

/// Twitch Enhanced Broadcasting's extra qualities (Enhanced RTMP multitrack video, track 1 here).
const EB_HEADER: &[u8] = b"\x96\x00avc1\x01\x01\x64\x00\x1f";
const EB_KEY: &[u8] = b"\x96\x03avc1\x01\x00\x00\x00\x01";
const EB_INTER: &[u8] = b"\xA6\x03avc1\x01\x00\x00\x00\x02";

#[tokio::test]
async fn only_enhanced_broadcasting_gets_the_other_qualities() {
    let w = world().await;
    let mut obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();
    obs.meta().await;
    obs.video(0, AVC_HEADER).await;
    obs.video(0, EB_HEADER).await;
    obs.audio(0, AAC_HEADER).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (lt, lo) = (free_listener().await, free_listener().await);
    let (pt, po) = (lt.local_addr().unwrap().port(), lo.local_addr().unwrap().port());
    let (mut twitch, mut other) = (platform(lt, "t").await, platform(lo, "o").await);
    let mut ht = start(&w, Destination { extra_video: true, ..destination(pt, "t") });
    let mut ho = start(&w, destination(po, "o"));
    state_is(&mut ht, |s| *s == PushState::Live).await;
    state_is(&mut ho, |s| *s == PushState::Live).await;
    for rx in [&mut twitch, &mut other] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Meta);
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }
    // The late joiner gets the extra track's header too; Twitch only.
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(EB_HEADER)));

    // An extra track's keyframe isn't where a destination starts: the main one's is.
    obs.video(900, EB_KEY).await;
    obs.video(1000, KEY).await;
    obs.video(1000, EB_INTER).await;
    obs.video(1040, INTER).await;
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(KEY)));
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(EB_INTER)));
    assert_eq!(next(&mut other).await, Got::Video(0, Bytes::from_static(KEY)));
    // The other platform never sees track 1: next is the main picture.
    assert_eq!(next(&mut other).await, Got::Video(40, Bytes::from_static(INTER)));
}

/// Listener with a small TCP receive buffer so pausing or throttling reads exerts real TCP
/// backpressure on `push::session`'s writes within a few frames.
async fn small_buf_listener() -> TcpListener {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    let _ = sock.set_recv_buffer_size(16 * 1024);
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    sock.listen(16).unwrap()
}

/// A fake platform whose reads can be paused via `open` (true = read freely, false = stall)
/// and optionally throttled after publishing (`read_cap` bytes per `read_delay`).
async fn controlled_platform(
    listener: TcpListener,
    key: &'static str,
    open: watch::Receiver<bool>,
    throttle: Option<(usize, Duration)>,
) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            let mut open = open.clone();
            tokio::spawn(async move {
                let _ = serve_controlled(&mut sock, key, &tx, &mut open, throttle).await;
                let _ = tx.send(Got::Closed);
            });
        }
    });
    rx
}

async fn serve_controlled(
    sock: &mut TcpStream,
    key: &str,
    tx: &mpsc::UnboundedSender<Got>,
    open: &mut watch::Receiver<bool>,
    throttle: Option<(usize, Duration)>,
) -> Option<()> {
    let rest = conn::handshake(sock, PeerType::Server).await.ok()?;
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue: Vec<ServerSessionResult> = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let mut buf = vec![0u8; 65536];
    let mut published = false;
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
                                published = true;
                                let _ = tx.send(Got::Publish(stream_key));
                                queue.extend(s.accept_request(request_id).ok()?);
                            } else {
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
        if published {
            if !*open.borrow() {
                open.wait_for(|v| *v).await.ok()?;
            }
            if let Some((_, delay)) = throttle {
                tokio::time::sleep(delay).await;
            }
        }
        let max_read = if published { throttle.map_or(buf.len(), |(cap, _)| cap.min(buf.len())) } else { buf.len() };
        let n = sock.read(&mut buf[..max_read]).await.ok()?;
        if n == 0 {
            return None;
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

/// Realistic-size FLV video frame (`size` bytes) so a stalled receiver fills the TCP send buffer
/// within a few frames instead of absorbing an entire test stream in kernel buffers.
fn sized_video(key: bool, seq: u8, size: usize) -> Bytes {
    let mut v = vec![seq; size];
    v[0] = if key { 0x17 } else { 0x27 };
    v[1] = 0x01;
    v[2] = 0;
    v[3] = 0;
    v[4] = 0;
    Bytes::from(v)
}

fn big_video(key: bool, seq: u8) -> Bytes {
    sized_video(key, seq, 64 * 1024)
}

/// Realistic-size Twitch Enhanced Broadcasting extra video frame (track 1).
fn big_eb_video(key: bool, seq: u8) -> Bytes {
    let mut v = vec![seq; 32 * 1024];
    let prefix = if key { EB_KEY } else { EB_INTER };
    v[..prefix.len()].copy_from_slice(prefix);
    Bytes::from(v)
}

#[tokio::test]
async fn a_slow_destination_drops_inter_video_keeps_audio_and_resumes_at_a_keyframe() {
    let w = world().await;
    w.hub.start();
    let mut m = StreamMetadata::new();
    m.video_width = Some(1920);
    m.video_height = Some(1080);
    m.video_frame_rate = Some(20.0);
    w.hub.metadata(m);
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));

    let lf = free_listener().await;
    let ls = small_buf_listener().await;
    let (pf, ps) = (lf.local_addr().unwrap().port(), ls.local_addr().unwrap().port());
    let (open_tx, open_rx) = watch::channel(true);
    let mut fast = platform(lf, "fast").await;
    let mut slow = controlled_platform(ls, "slow", open_rx, None).await;
    let mut hf = start(&w, Destination { id: "fast".into(), ..destination(pf, "fast") });
    let mut hs = start(&w, Destination { id: "slow".into(), ..destination(ps, "slow") });
    state_is(&mut hf, |s| *s == PushState::Live).await;
    state_is(&mut hs, |s| *s == PushState::Live).await;

    // Drain connect + headers on both destinations.
    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Meta);
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }

    // First keyframe + audio: both receive it cleanly.
    w.hub.video(0, big_video(true, 0));
    w.hub.audio(10, Bytes::from_static(AAC));
    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Video(0, _)));
        assert_eq!(next(rx).await, Got::Audio(10, Bytes::from_static(AAC)));
    }

    // Stall the slow destination's TCP reads, then feed 2.5 s of stream (well under BACKLOG=1024
    // so RecvError::Lagged never fires): GOP 1 inter frames (50..=1950), GOP 2 keyframe at 2000
    // + inter frames (2050..=2450), and interleaved audio every 50 ms.
    // Let fast drain each frame as it's published (like real-time pacing) so fast stays caught up
    // while slow stalls on TCP backpressure.
    open_tx.send_replace(false);
    let mut expected_video_ts = vec![0u32];
    let mut expected_audio_ts = vec![10u32];
    for step in 1..=49u32 {
        let ts = step * 50;
        let is_key = ts == 2000;
        expected_video_ts.push(ts);
        expected_audio_ts.push(ts + 10);
        w.hub.video(ts, big_video(is_key, step as u8));
        w.hub.audio(ts + 10, Bytes::from_static(AAC));
        assert!(matches!(next(&mut fast).await, Got::Video(t, _) if t == ts));
        assert_eq!(next(&mut fast).await, Got::Audio(ts + 10, Bytes::from_static(AAC)));
    }
    assert_eq!(hf.borrow().dropped, 0, "a fast destination drops nothing");

    // Give the slow sender a moment to fill its kernel send buffer and block in write_all.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Unblock the slow destination and end the stream cleanly so we can inspect everything it sent.
    open_tx.send_replace(true);
    w.hub.end();

    let mut slow_videos: Vec<(u32, bool)> = vec![(0, true)];
    let mut slow_audios = vec![10u32];
    loop {
        match next(&mut slow).await {
            Got::Video(ts, data) => slow_videos.push((ts, crate::flv::video_is_keyframe(&data))),
            Got::Audio(ts, _) => slow_audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected slow packet: {other:?}"),
        }
    }
    state_is(&mut hs, |s| *s == PushState::Waiting).await;

    let slow_dropped = hs.borrow().dropped;
    assert!(slow_dropped > 0, "slow destination must drop stale non-keyframe video (got dropped={slow_dropped})");
    assert_eq!(
        slow_videos.len() as u64 + slow_dropped,
        expected_video_ts.len() as u64,
        "every dropped video frame is accounted for in Health.dropped"
    );
    assert_eq!(slow_audios, expected_audio_ts, "audio is never dropped by the video-drop path");
    // Every gap in the slow destination's video stream must resume on a keyframe (no orphan P-frames).
    let mut saw_gap = false;
    for pair in slow_videos.windows(2) {
        let ((prev_ts, _), (cur_ts, cur_key)) = (pair[0], pair[1]);
        if cur_ts > prev_ts + 50 {
            saw_gap = true;
            assert!(cur_key, "resumed at non-keyframe ts={cur_ts} after gap from ts={prev_ts}");
        }
    }
    assert!(saw_gap, "slow destination should have skipped stale inter frames up to keyframe 2000");
    assert!(slow_videos.iter().any(|&(ts, key)| ts == 2000 && key), "resumed at keyframe 2000");
    assert!(!slow_videos.iter().any(|&(ts, _)| ts == 1950), "tail inter frame before keyframe 2000 was dropped");
    assert_eq!(slow_videos.last().map(|&(ts, _)| ts), Some(2450), "caught up and sent frames after keyframe");
}

#[tokio::test]
async fn a_continuously_slow_destination_keeps_latency_bounded_and_audio_intact() {
    let w = world().await;
    w.hub.start();
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));

    let lf = free_listener().await;
    let ls = small_buf_listener().await;
    let (pf, ps) = (lf.local_addr().unwrap().port(), ls.local_addr().unwrap().port());
    let (_open_tx, open_rx) = watch::channel(true);
    let mut fast = platform(lf, "fast").await;
    // Slow platform reads at ~2.1 MB/s (32 KB every 15 ms) against ~6.4 MB/s stream (128 KB every
    // 20 ms); 128 KB frames keep the OS loopback send buffer to ~2 frames so measured lag reflects
    // the hub backlog rather than loopback bufferbloat.
    let mut slow = controlled_platform(ls, "slow", open_rx, Some((32 * 1024, Duration::from_millis(15)))).await;
    let mut hf = start(&w, Destination { id: "fast".into(), ..destination(pf, "fast") });
    let mut hs = start(&w, Destination { id: "slow".into(), ..destination(ps, "slow") });
    state_is(&mut hf, |s| *s == PushState::Live).await;
    state_is(&mut hs, |s| *s == PushState::Live).await;

    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }

    // 6 seconds of stream time (120 video + 120 audio = 240 packets << BACKLOG=1024),
    // keyframes every 1000 ms (0, 1000, ..., 5000), paced at 20 ms wall time per 50 ms frame.
    let hub = w.hub.clone();
    let pub_task = tokio::spawn(async move {
        let mut expected_audio = Vec::with_capacity(120);
        for step in 0..120u32 {
            let ts = step * 50;
            let is_key = ts % 1000 == 0;
            hub.video(ts, sized_video(is_key, step as u8, 128 * 1024));
            hub.audio(ts + 10, Bytes::from_static(AAC));
            expected_audio.push(ts + 10);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        hub.end();
        expected_audio
    });

    // Collect what the slow destination receives while measuring how far behind the live stream edge
    // each received video frame is.
    let mut slow_videos: Vec<(u32, bool)> = Vec::new();
    let mut slow_audios: Vec<u32> = Vec::new();
    let mut max_lag_ms = 0u32;
    loop {
        match timeout(Duration::from_secs(10), slow.recv()).await.expect("slow pkt in time").expect("slow open") {
            Got::Video(ts, data) => {
                let live_ts = w.hub.newest_video_ts().unwrap_or(ts);
                max_lag_ms = max_lag_ms.max(live_ts.saturating_sub(ts));
                slow_videos.push((ts, crate::flv::video_is_keyframe(&data)));
            }
            Got::Audio(ts, _) => slow_audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected slow packet: {other:?}"),
        }
    }
    let expected_audio = pub_task.await.unwrap();

    // Drain fast destination too and verify it lost nothing.
    let mut fast_video_count = 0usize;
    let mut fast_audios = Vec::new();
    loop {
        match next(&mut fast).await {
            Got::Video(_, _) => fast_video_count += 1,
            Got::Audio(ts, _) => fast_audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected fast packet: {other:?}"),
        }
    }
    state_is(&mut hs, |s| *s == PushState::Waiting).await;

    assert_eq!(fast_video_count, 120);
    assert_eq!(fast_audios, expected_audio);
    assert_eq!(hf.borrow().dropped, 0);

    let slow_dropped = hs.borrow().dropped;
    assert!(slow_dropped > 0, "continuously slow destination must drop non-keyframe video");
    assert_eq!(slow_audios, expected_audio, "audio must never be dropped");
    // Without bounded dropping, the 1/3-speed receiver falls >4000 ms behind over a 6000 ms stream.
    assert!(max_lag_ms < 2_200, "slow destination latency grew unbounded: max_lag_ms={max_lag_ms}");
    for pair in slow_videos.windows(2) {
        let ((prev_ts, _), (cur_ts, cur_key)) = (pair[0], pair[1]);
        if cur_ts > prev_ts + 50 {
            assert!(cur_key, "resumed at non-keyframe ts={cur_ts} after gap from ts={prev_ts}");
        }
    }
}

#[tokio::test]
async fn enhanced_broadcasting_drops_extra_tracks_to_their_own_keyframes() {
    let w = world().await;
    w.hub.start();
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.video(0, Bytes::from_static(EB_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));

    let ls = small_buf_listener().await;
    let ps = ls.local_addr().unwrap().port();
    let (open_tx, open_rx) = watch::channel(true);
    let mut twitch = controlled_platform(ls, "t", open_rx, None).await;
    let mut ht = start(&w, Destination { id: "twitch".into(), extra_video: true, ..destination(ps, "t") });
    state_is(&mut ht, |s| *s == PushState::Live).await;

    assert!(matches!(next(&mut twitch).await, Got::Publish(_)));
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
    assert_eq!(next(&mut twitch).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    assert_eq!(next(&mut twitch).await, Got::Video(0, Bytes::from_static(EB_HEADER)));

    // Initial keyframes on both tracks + audio.
    w.hub.video(0, big_video(true, 0));
    w.hub.video(0, big_eb_video(true, 0));
    w.hub.audio(10, Bytes::from_static(AAC));
    assert!(matches!(next(&mut twitch).await, Got::Video(0, _)));
    assert!(matches!(next(&mut twitch).await, Got::Video(0, _)));
    assert_eq!(next(&mut twitch).await, Got::Audio(10, Bytes::from_static(AAC)));

    // Stall Twitch, then push 2.5 s of main + extra track video + audio.
    // Stagger keyframes: main track keyframe at 2000, extra track keyframe at 2100.
    open_tx.send_replace(false);
    let mut expected_audio = vec![10u32];
    for step in 1..=49u32 {
        let ts = step * 50;
        w.hub.video(ts, big_video(ts == 2000, step as u8));
        w.hub.video(ts, big_eb_video(ts == 2100, step as u8));
        w.hub.audio(ts + 10, Bytes::from_static(AAC));
        expected_audio.push(ts + 10);
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    open_tx.send_replace(true);
    w.hub.end();

    let mut main_videos: Vec<(u32, bool)> = vec![(0, true)];
    let mut extra_videos: Vec<(u32, bool)> = vec![(0, true)];
    let mut audios = vec![10u32];
    loop {
        match next(&mut twitch).await {
            Got::Video(ts, data) => {
                let is_key = crate::flv::video_is_keyframe(&data);
                if crate::flv::video_extra_track(&data).is_some() {
                    extra_videos.push((ts, is_key));
                } else {
                    main_videos.push((ts, is_key));
                }
            }
            Got::Audio(ts, _) => audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected packet: {other:?}"),
        }
    }
    state_is(&mut ht, |s| *s == PushState::Waiting).await;

    assert!(ht.borrow().dropped > 0);
    assert_eq!(audios, expected_audio, "audio is never dropped");
    // Main resumes at keyframe 2000; extra track stays dropped through 2000 & 2050 and resumes at 2100.
    assert!(
        !extra_videos.iter().any(|&(ts, _)| ts == 2000 || ts == 2050),
        "extra track must not send orphan inter frames after main keyframe"
    );
    assert!(extra_videos.iter().any(|&(ts, key)| ts == 2100 && key), "extra track resumed at its own keyframe 2100");
    for (name, track) in [("main", &main_videos), ("extra", &extra_videos)] {
        let mut saw_gap = false;
        for pair in track.windows(2) {
            if pair[1].0 > pair[0].0 + 50 {
                saw_gap = true;
                assert!(pair[1].1, "{name} resumed at non-keyframe ts={} after {}", pair[1].0, pair[0].0);
            }
        }
        assert!(saw_gap, "{name} should have dropped stale inter frames");
    }
}

#[tokio::test]
async fn a_late_destination_joining_mid_gop_counts_zero_drops() {
    let w = world().await;
    w.hub.start();
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));
    w.hub.video(0, Bytes::from_static(KEY));

    let l = free_listener().await;
    let port = l.local_addr().unwrap().port();
    let mut got = platform(l, "k").await;
    let mut health = start(&w, destination(port, "k"));
    state_is(&mut health, |s| *s == PushState::Live).await;
    assert_eq!(next(&mut got).await, Got::Publish("k".into()));
    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
    assert_eq!(next(&mut got).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));

    // 1.5 s of inter frames and audio arrive before the first keyframe this destination sees:
    // skipped because it hasn't started yet, NOT counted as drops.
    for step in 1..=30u32 {
        let ts = step * 50;
        w.hub.video(ts, Bytes::from_static(INTER));
        w.hub.audio(ts + 10, Bytes::from_static(AAC));
    }
    w.hub.video(1600, Bytes::from_static(KEY));
    w.hub.audio(1610, Bytes::from_static(AAC));
    w.hub.video(1650, Bytes::from_static(INTER));

    assert_eq!(next(&mut got).await, Got::Video(0, Bytes::from_static(KEY)));
    assert_eq!(next(&mut got).await, Got::Audio(10, Bytes::from_static(AAC)));
    assert_eq!(next(&mut got).await, Got::Video(50, Bytes::from_static(INTER)));
    assert_eq!(health.borrow().dropped, 0, "pre-keyframe startup skips must not count as drops");
}

#[tokio::test]
async fn kick_gives_up_after_repeated_rapid_drops_and_says_why() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let mut got = platform_that_drops(l, "k", Duration::from_millis(50)).await;
    let dest = Destination {
        id: "kick".into(),
        retry: push::Retry { first: Duration::from_millis(20), max: Duration::from_millis(40), max_attempts: 4 },
        ..destination(dport, "k")
    };
    let mut health = start(&w, dest);
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();

    match state_is(&mut health, |s| matches!(s, PushState::Failed { .. })).await {
        PushState::Failed { attempts, reason, detail, refused } => {
            assert_eq!(attempts, 4);
            assert!(refused, "Kick rapid drops count as refused");
            assert!(reason.contains("Kick refused"), "{reason}");
            let detail = detail.expect("Kick refusal explains why");
            assert!(detail.contains("Allow multistreaming"), "{detail}");
        }
        s => panic!("expected Failed, got {s:?}"),
    }
    // Drain any queued events from the 4 attempts, then verify no further retries happen.
    while got.try_recv().is_ok() {}
    assert!(
        timeout(Duration::from_millis(250), got.recv()).await.is_err(),
        "a failed destination must stop retrying for the rest of this stream"
    );
}

#[tokio::test]
async fn a_refused_stream_key_gives_up_after_bounded_attempts() {
    let w = world().await;
    let l = free_listener().await;
    let dport = l.local_addr().unwrap().port();
    let _got = platform(l, "right").await;
    let dest = Destination {
        id: "twitch".into(),
        retry: push::Retry {
            first: Duration::from_millis(20),
            max: Duration::from_millis(40),
            max_attempts: push::DEFAULT_MAX_ATTEMPTS,
        },
        ..destination(dport, "wrong")
    };
    let mut health = start(&w, dest);
    let _obs = Obs::publish(w.port, "live", OBS_KEY).await.unwrap();

    match state_is(&mut health, |s| matches!(s, PushState::Failed { .. })).await {
        PushState::Failed { attempts, reason, detail, refused } => {
            assert_eq!(attempts, push::REFUSED_MAX_ATTEMPTS);
            assert!(refused);
            assert!(reason.contains("key"), "{reason}");
            assert!(detail.as_deref().is_some_and(|d| d.contains("refused the stream key")), "{detail:?}");
        }
        s => panic!("expected Failed, got {s:?}"),
    }
}

#[tokio::test]
async fn a_stalled_destination_far_behind_skips_older_keyframes_and_stale_audio() {
    let w = world().await;
    w.hub.start();
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));

    let lf = free_listener().await;
    let ls = small_buf_listener().await;
    let (pf, ps) = (lf.local_addr().unwrap().port(), ls.local_addr().unwrap().port());
    let (open_tx, open_rx) = watch::channel(true);
    let mut fast = platform(lf, "fast").await;
    let mut slow = controlled_platform(ls, "slow", open_rx, None).await;
    let mut hf = start(&w, Destination { id: "fast".into(), ..destination(pf, "fast") });
    let mut hs = start(&w, Destination { id: "slow".into(), ..destination(ps, "slow") });
    state_is(&mut hf, |s| *s == PushState::Live).await;
    state_is(&mut hs, |s| *s == PushState::Live).await;

    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }

    w.hub.video(0, big_video(true, 0));
    w.hub.audio(10, Bytes::from_static(AAC));
    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Video(0, _)));
        assert_eq!(next(rx).await, Got::Audio(10, Bytes::from_static(AAC)));
    }

    // Stall slow, then send 4.25 s across 10 GOPs (keyframes every 400 ms at 400..=4000).
    open_tx.send_replace(false);
    let mut total_video = 1u64;
    let mut total_audio = 1u64;
    for step in 1..=85u32 {
        let ts = step * 50;
        let is_key = ts.is_multiple_of(400);
        w.hub.video(ts, big_video(is_key, step as u8));
        w.hub.audio(ts + 10, Bytes::from_static(AAC));
        total_video += 1;
        total_audio += 1;
        assert!(matches!(next(&mut fast).await, Got::Video(t, _) if t == ts));
        assert_eq!(next(&mut fast).await, Got::Audio(ts + 10, Bytes::from_static(AAC)));
    }
    assert_eq!(hf.borrow().dropped, 0);

    tokio::time::sleep(Duration::from_millis(80)).await;
    open_tx.send_replace(true);
    w.hub.end();

    let mut slow_videos = vec![(0u32, true)];
    let mut slow_audios = vec![10u32];
    loop {
        match next(&mut slow).await {
            Got::Video(ts, data) => slow_videos.push((ts, crate::flv::video_is_keyframe(&data))),
            Got::Audio(ts, _) => slow_audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected packet: {other:?}"),
        }
    }
    state_is(&mut hs, |s| *s == PushState::Waiting).await;

    // Slow destination jumped directly to the newest keyframe (4000), skipping intermediate GOPs
    // and dropping the stale audio belonging to those skipped GOPs.
    let dropped = hs.borrow().dropped;
    assert_eq!(
        slow_videos.len() as u64 + slow_audios.len() as u64 + dropped,
        total_video + total_audio,
        "every dropped video + skipped audio is accounted for in Health.dropped"
    );
    assert!(slow_videos.iter().any(|&(ts, key)| ts == 4000 && key), "resumed at newest keyframe 4000");
    assert!(
        !slow_videos.iter().any(|&(ts, key)| (1200..4000).contains(&ts) && key),
        "intermediate keyframes before 4000 were skipped: {slow_videos:?}"
    );
    assert!(
        slow_audios.contains(&4010) && !slow_audios.iter().any(|&ts| (1200..4000).contains(&ts)),
        "stale audio in skipped GOPs dropped and resumed with newest keyframe: {slow_audios:?}"
    );
    for pair in slow_videos.windows(2) {
        if pair[1].0 > pair[0].0 + 50 {
            assert!(pair[1].1, "no orphan P-frame after gap: {:?} -> {:?}", pair[0], pair[1]);
        }
    }
}

#[tokio::test]
async fn a_link_slower_than_keyframes_alone_skips_to_newest_keyframe() {
    let w = world().await;
    w.hub.start();
    w.hub.video(0, Bytes::from_static(AVC_HEADER));
    w.hub.audio(0, Bytes::from_static(AAC_HEADER));

    let lf = free_listener().await;
    let ls = small_buf_listener().await;
    let (pf, ps) = (lf.local_addr().unwrap().port(), ls.local_addr().unwrap().port());
    let (_open_tx, open_rx) = watch::channel(true);
    let mut fast = platform(lf, "fast").await;
    // Slow platform drains only 16 KB per 100 ms (160 KB/s), while keyframes alone are 128 KB every
    // 400 ms stream time (320 KB/s stream -> 640 KB/s wall rate at 2x pacing): keyframes alone
    // exceed the slow link speed, so without keyframe skipping it drifts farther behind forever.
    let mut slow = controlled_platform(ls, "slow", open_rx, Some((16 * 1024, Duration::from_millis(100)))).await;
    let mut hf = start(&w, Destination { id: "fast".into(), ..destination(pf, "fast") });
    let mut hs = start(&w, Destination { id: "slow".into(), ..destination(ps, "slow") });
    state_is(&mut hf, |s| *s == PushState::Live).await;
    state_is(&mut hs, |s| *s == PushState::Live).await;

    for rx in [&mut fast, &mut slow] {
        assert!(matches!(next(rx).await, Got::Publish(_)));
        assert_eq!(next(rx).await, Got::Video(0, Bytes::from_static(AVC_HEADER)));
        assert_eq!(next(rx).await, Got::Audio(0, Bytes::from_static(AAC_HEADER)));
    }

    let hub = w.hub.clone();
    let pub_task = tokio::spawn(async move {
        // 120 frames = 6.0 s stream time (keyframes every 400 ms: 0, 400, ..., 5600), 25 ms wall step.
        for step in 0..120u32 {
            let ts = step * 50;
            let is_key = ts.is_multiple_of(400);
            let bytes = if is_key { 128 * 1024 } else { 32 * 1024 };
            hub.video(ts, sized_video(is_key, step as u8, bytes));
            hub.audio(ts + 10, Bytes::from_static(AAC));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        hub.end();
    });

    let mut slow_videos = Vec::new();
    let mut slow_audios = Vec::new();
    loop {
        match timeout(Duration::from_secs(12), slow.recv()).await.expect("slow pkt").expect("slow open") {
            Got::Video(ts, data) => slow_videos.push((ts, crate::flv::video_is_keyframe(&data))),
            Got::Audio(ts, _) => slow_audios.push(ts),
            Got::Closed => break,
            other => panic!("unexpected packet: {other:?}"),
        }
    }
    pub_task.await.unwrap();

    let mut fast_count = 0usize;
    loop {
        match next(&mut fast).await {
            Got::Video(_, _) => fast_count += 1,
            Got::Audio(_, _) => {}
            Got::Closed => break,
            other => panic!("unexpected packet: {other:?}"),
        }
    }
    state_is(&mut hs, |s| *s == PushState::Waiting).await;
    assert_eq!(fast_count, 120);
    assert_eq!(hf.borrow().dropped, 0);

    // Slow platform jumped across intermediate keyframes cleanly, reached the tail of the stream
    // quickly instead of falling behind forever, never emitted orphan inter frames, and dropped
    // skipped audio in lockstep with skipped GOPs.
    let key_ts: Vec<u32> = slow_videos.iter().filter(|&&(_, k)| k).map(|&(t, _)| t).collect();
    assert!(key_ts.len() < 15, "must have skipped intermediate keyframes when slower than keyframes: {key_ts:?}");
    assert!(
        key_ts.last().copied().unwrap_or(0) >= 4800,
        "jumped near the live tail instead of stalling far behind: {key_ts:?}"
    );
    assert!(slow_audios.len() < 120, "skipped-GOP audio dropped with video: sent {}", slow_audios.len());
    for pair in slow_videos.windows(2) {
        if pair[1].0 > pair[0].0 + 50 {
            assert!(pair[1].1, "no orphan inter frame after gap: {:?} -> {:?}", pair[0], pair[1]);
        }
    }
}
mod platforms;
