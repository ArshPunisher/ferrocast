//! One task per destination (R-18): while the stream is live it connects, publishes with the
//! destination's key, sends the codec headers, then the same packets OBS sent (no re-encode),
//! starting at a keyframe. A failure retries with backoff and jitter for as long as the stream
//! runs; one destination failing or falling behind never touches the others. Health goes out on
//! a watch channel, with the reason in words (R-08).

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rml_rtmp::handshake::PeerType;
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionError, ClientSessionEvent, ClientSessionResult, PublishRequestType,
};
use rml_rtmp::time::RtmpTimestamp;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;

use crate::conn::{self, READ_BUF};
use crate::flv;
use crate::hub::{Frame, Hub, Live, Subscription};
use crate::links;
use crate::target::Target;

const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Each step after connecting (handshake, connect, publish) must answer within this.
const STEP_DEADLINE: Duration = Duration::from_secs(10);
/// A write that can't finish in this long means the destination stopped reading.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// How far behind live video (in stream milliseconds) a destination may fall before it drops
/// non-keyframe video until the next keyframe, keeping audio and codec headers intact.
///
/// Matches OBS's `OPT_PFRAME_DROP_THRESHOLD` (`pframe_drop_threshold_usec = 900 ms` in
/// `plugins/obs-outputs/rtmp-stream.c`, `check_to_drop_frames` / `drop_frames`). OBS also has a
/// 700 ms `OPT_DROP_THRESHOLD` that drops only disposable non-reference B-frames using codec-level
/// NAL/OBU priority before escalating to all non-keyframes at 900 ms. Because `golive` forwards
/// FLV tags without re-parsing H.264/HEVC/AV1/VP9 bitstreams, it only distinguishes keyframes from
/// inter frames; using OBS's 900 ms threshold keeps latency under a second without breaking a GOP
/// prematurely at 700 ms on brief jitter (whether or not the stream uses B-frames).
const DROP_THRESHOLD_MS: u32 = 900;
/// Minimum packets still queued behind the current packet in the destination's receiver before
/// time-based dropping starts (at least 5 buffered packets including the current one, matching
/// OBS's `num_packets < 5` guard in `check_to_drop_frames`). Prevents a sparse timestamp gap or
/// brief source pause from falsely dropping frames.
const MIN_BACKLOG_PACKETS: usize = 4;
/// How long a destination waits before trying again: the first wait, doubling up to the
/// longest (Advanced mode can change both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self { first: Duration::from_secs(1), max: Duration::from_secs(30) }
    }
}
/// How often health (bitrate) is updated while live.
const HEALTH_EVERY: Duration = Duration::from_secs(1);

/// Where one copy of the stream goes. The key is a secret: never logged or shown.
#[derive(Clone)]
pub struct Destination {
    pub id: String,
    pub target: Target,
    pub key: String,
    pub retry: Retry,
    /// Gets the stream's extra audio tracks (Twitch's VOD track); others get only the main one.
    pub extra_audio: bool,
    /// Gets the stream's extra video tracks (Twitch Enhanced Broadcasting's other qualities);
    /// others get only the main picture.
    pub extra_video: bool,
}

impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Destination").field("id", &self.id).field("target", &self.target).finish_non_exhaustive()
    }
}

/// One destination's state, for the health line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PushState {
    /// Waiting for OBS to start streaming.
    Waiting,
    Connecting,
    Live,
    /// The last try failed; the next one starts in `retry_in_ms`.
    Reconnecting {
        attempt: u32,
        retry_in_ms: u64,
        reason: String,
    },
    Stopped,
    /// Upload protection paused it to keep the destinations above smooth (the engine sets
    /// this; the forwarder isn't running). It comes back by itself after `until_ms`.
    Paused {
        until_ms: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    #[serde(flatten)]
    pub state: PushState,
    /// What the destination got over the last second, in kilobits per second.
    pub kbps: u32,
    /// Packets skipped because this destination fell behind (counted per stream).
    pub dropped: u64,
    /// Bytes sent since this destination started (for callers that work out their own rate).
    #[serde(skip)]
    pub sent: u64,
    /// How the network to it is doing (direct destinations, once connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
    /// The connection it moved to after the usual one failed ("iPhone USB"); none: the usual.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// When the current state began (ms since the Unix epoch).
    pub since_ms: u64,
}

impl Health {
    fn new(state: PushState) -> Self {
        Self { state, kbps: 0, dropped: 0, sent: 0, link: None, via: None, since_ms: now_ms() }
    }
}

/// The network to a destination over the last few seconds, as its sender measures it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, serde::Deserialize)]
pub struct Link {
    /// The round trip (ping), in milliseconds.
    pub rtt_ms: u32,
    /// Of the packets sent, the share the network lost (most come back resent), in percent.
    pub lost_pct: f32,
    /// Of the packets sent, the share sent again, in percent.
    pub resent_pct: f32,
}

/// Why a try failed, in words.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("Couldn't reach {0}.")]
    Unreachable(String),
    #[error("The secure connection to {0} failed.")]
    Tls(String),
    #[error("{0} didn't answer like a streaming server.")]
    NotRtmp(String),
    #[error("{0} refused the connection: {1}")]
    Refused(String, String),
    #[error("{0} refused the stream key. Check the key for this destination.")]
    KeyRefused(String),
    #[error("{0} took too long to answer.")]
    Slow(String),
    #[error("{0} closed the connection.")]
    Closed(String),
    #[error("Sending to {0} stalled: your upload may be too slow for it.")]
    Stalled(String),
    #[error("The connection to {0} broke.")]
    Broken(String),
}

impl PushError {
    /// The network failed (not the server saying no): another connection may do better.
    fn is_network(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Slow(_) | Self::Closed(_) | Self::Stalled(_) | Self::Broken(_))
    }
}

/// Whether a destination may move to another connection: not one on this computer or network.
fn may_move(t: &Target) -> bool {
    match t.host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => !(ip.is_loopback() || ip.is_private() || ip.is_link_local()),
        Ok(std::net::IpAddr::V6(ip)) => !ip.is_loopback(),
        Err(_) => t.host != "localhost" && !t.host.ends_with(".local"),
    }
}

/// How a session ended without an error.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// OBS stopped streaming.
    StreamEnded,
    /// Asked to stop.
    Stopped,
}

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type Stream = Pin<Box<dyn Io>>;

/// Runs one destination until `stop` turns true. Health is sent on `health`.
pub async fn run(dest: Destination, hub: Arc<Hub>, health: watch::Sender<Health>, mut stop: watch::Receiver<bool>) {
    let mut live = hub.live();
    health.send_replace(Health::new(PushState::Waiting));
    // The last stream this destination finished with; it waits for a newer one.
    let mut done = 0u64;
    'streams: loop {
        let stream = tokio::select! {
            _ = stop.wait_for(|s| *s) => break,
            l = live.wait_for(|l| l.on && l.stream > done) => match l {
                Ok(l) => l.stream,
                Err(_) => break,
            },
        };
        let still_on = |l: &Live| l.on && l.stream == stream;
        let mut attempt = 0u32;
        // The connection used: the system's default until that fails, then the next one.
        let mut via: Option<links::Network> = None;
        loop {
            set_state(&health, PushState::Connecting);
            let outcome = tokio::select! {
                _ = stop.wait_for(|s| *s) => Ok(Ended::Stopped),
                r = session(&dest, &hub, &health, stream, via.as_ref()) => r,
            };
            match outcome {
                Ok(Ended::Stopped) => break 'streams,
                Ok(Ended::StreamEnded) => break,
                Err(err) => {
                    if !still_on(&live.borrow()) {
                        break;
                    }
                    // A session that stayed live a while and then failed starts the count over: a
                    // platform that keeps dropping a long stream is retried at once, not after
                    // the longest wait. One that took the stream and closed it at once doesn't
                    // (19 Nov, live: Kick took it and closed it every 8 s for the whole stream,
                    // always "attempt 1", never waiting longer and never saying why).
                    let (was_live, live_for_ms) = {
                        let h = health.borrow();
                        (h.state == PushState::Live, now_ms().saturating_sub(h.since_ms))
                    };
                    if was_live && live_for_ms >= STAYED_LIVE_MS {
                        attempt = 0;
                    }
                    attempt = attempt.saturating_add(1);
                    let wait = backoff(attempt, &dest.retry);
                    if err.is_network() && may_move(&dest.target) {
                        via = links::next_after(via.as_ref(), &links::fallbacks(), links::default_ip());
                    }
                    tracing::info!(dest = %dest.id, attempt, error = %err, via = ?via.as_ref().map(|n| &n.name), "golive: destination failed, retrying");
                    let reason = if was_live && live_for_ms < STAYED_LIVE_MS && dest.id.starts_with("kick") {
                        format!(
                            "{err} Kick took the stream and closed it at once: streaming to other platforms too \
                             needs \"Allow multistreaming\" in Kick's settings (Kick Partners), else turn Kick off."
                        )
                    } else {
                        err.to_string()
                    };
                    set_state(
                        &health,
                        PushState::Reconnecting { attempt, retry_in_ms: wait.as_millis() as u64, reason },
                    );
                    tokio::select! {
                        _ = stop.wait_for(|s| *s) => break 'streams,
                        _ = live.wait_for(|l| !still_on(l)) => break,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
        done = stream;
        set_state(&health, PushState::Waiting);
        health.send_modify(|h| h.via = None);
    }
    set_state(&health, PushState::Stopped);
}

pub(crate) fn set_state(health: &watch::Sender<Health>, state: PushState) {
    health.send_modify(|h| {
        if h.state != state {
            h.state = state;
            h.since_ms = now_ms();
            h.kbps = 0;
        }
    });
}

/// How long a session stays live before its failure starts the retry count over.
const STAYED_LIVE_MS: u64 = 30_000;

/// 1, 2, 4, 8, 16, 30, 30… seconds by default, each ±20% so many destinations don't retry in
/// step.
pub(crate) fn backoff(attempt: u32, retry: &Retry) -> Duration {
    let base = retry.first.saturating_mul(1u32 << attempt.saturating_sub(1).min(8)).min(retry.max);
    let jitter = 0.8 + fastrand::f64() * 0.4;
    base.mul_f64(jitter)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Connects to `t`, over `via` when given (else the system's default way).
pub(crate) async fn connect(t: &Target, via: Option<&links::Network>) -> Result<Stream, PushError> {
    let host = t.host.clone();
    let tcp = match (via, t.pin) {
        (None, Some(addr)) => timeout(CONNECT_DEADLINE, TcpStream::connect(addr)).await,
        (None, None) => timeout(CONNECT_DEADLINE, TcpStream::connect((t.host.as_str(), t.port))).await,
        (Some(n), _) => timeout(CONNECT_DEADLINE, links::tcp_via(n, &t.host, t.port)).await,
    }
    .map_err(|_| PushError::Slow(host.clone()))?
    .map_err(|_| PushError::Unreachable(host.clone()))?;
    let _ = tcp.set_nodelay(true);
    if !t.tls {
        return Ok(Box::pin(tcp));
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| PushError::Tls(host.clone()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(t.host.clone()).map_err(|_| PushError::Tls(host.clone()))?;
    let tls = timeout(CONNECT_DEADLINE, tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp))
        .await
        .map_err(|_| PushError::Slow(host.clone()))?
        .map_err(|_| PushError::Tls(host))?;
    Ok(Box::pin(tls))
}

/// One connection: connect, publish, forward until the stream ends, `stop`, or an error.
async fn session(
    dest: &Destination,
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
    via: Option<&links::Network>,
) -> Result<Ended, PushError> {
    let mut p = Publisher::open(&dest.target, &dest.key, via).await?;
    // Its name asks the system (a quick command): off the async threads.
    let label = match via {
        Some(n) => {
            let name = n.name.clone();
            Some(tokio::task::spawn_blocking(move || links::label(&name)).await.unwrap_or_else(|_| n.name.clone()))
        }
        None => None,
    };
    health.send_modify(|h| h.via = label);

    // Headers first, then packets from the next keyframe, timestamps from zero.
    let Subscription { live, headers, mut rx } = hub.subscribe();
    // The stream may have ended (or a new one begun) while this connection was being set up.
    if !live.on || live.stream != stream {
        return Ok(Ended::StreamEnded);
    }
    if let Some(m) = &headers.metadata {
        let r = p.client.publish_metadata(m);
        p.send(r).await?;
    }
    if let Some(v) = headers.video {
        let r = p.client.publish_video_data(v, RtmpTimestamp::new(0), false);
        p.send(r).await?;
    }
    if let Some(a) = headers.audio {
        let r = p.client.publish_audio_data(a, RtmpTimestamp::new(0), false);
        p.send(r).await?;
    }
    if dest.extra_video {
        for (_, v) in headers.extra_video {
            let r = p.client.publish_video_data(v, RtmpTimestamp::new(0), false);
            p.send(r).await?;
        }
    }
    if dest.extra_audio {
        for (_, a) in headers.extra_audio {
            let r = p.client.publish_audio_data(a, RtmpTimestamp::new(0), false);
            p.send(r).await?;
        }
    }
    set_state(health, PushState::Live);
    let mut base: Option<u32> = None;
    let mut started = false;
    let mut video_drop = VideoDrop::default();
    let mut window = (Instant::now(), p.sent);
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        health.send_modify(|h| h.dropped = h.dropped.saturating_add(n));
                        if started {
                            video_drop.mark_all();
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        p.finish().await;
                        return Ok(Ended::StreamEnded);
                    }
                };
                let outbound = match frame {
                    Frame::End => {
                        p.finish().await;
                        return Ok(Ended::StreamEnded);
                    }
                    Frame::Metadata(m) => Some(p.client.publish_metadata(&m)),
                    // Codec headers always go through; initial frames wait for a main keyframe,
                    // and once live, stale non-keyframe video drops until its track's next keyframe.
                    Frame::Video { ts, data, key } => {
                        let extra = flv::video_extra_track(&data);
                        let header = flv::video_is_header(&data) || extra.is_some_and(|(_, h)| h);
                        let send = if extra.is_some() && !dest.extra_video {
                            false
                        } else if header {
                            true
                        } else if !started {
                            started = key;
                            key
                        } else {
                            let track = extra.map_or(0, |(t, _)| t);
                            let track_key = if extra.is_some() { flv::video_is_keyframe(&data) } else { key };
                            if video_drop.should_drop(track, track_key, hub.newest_video_ts(), ts, rx.len()) {
                                health.send_modify(|h| h.dropped = h.dropped.saturating_add(1));
                                false
                            } else {
                                true
                            }
                        };
                        send.then(|| p.client.publish_video_data(data, stamp(&mut base, ts), false))
                    }
                    Frame::Audio { ts, data } => {
                        let extra = flv::audio_extra_track(&data);
                        let header = flv::audio_is_header(&data) || extra.is_some_and(|(_, h)| h);
                        let send = (extra.is_none() || dest.extra_audio) && (started || header);
                        send.then(|| p.client.publish_audio_data(data, stamp(&mut base, ts), false))
                    }
                };
                if let Some(r) = outbound {
                    p.send(r).await?;
                }
            }
            // Only the read races the next packet (a read can be dropped safely); the answer
            // is written after, so a write is never cut off halfway.
            n = p.read_some() => {
                let n = n?;
                p.answer(n).await?;
            }
        }
        if window.0.elapsed() >= HEALTH_EVERY {
            let secs = window.0.elapsed().as_secs_f64();
            let kbps = ((p.sent - window.1) as f64 * 8.0 / 1000.0 / secs) as u32;
            health.send_modify(|h| {
                h.kbps = kbps;
                h.sent += p.sent - window.1;
            });
            window = (Instant::now(), p.sent);
        }
    }
}

/// How far `ts` lags behind `newest` in stream milliseconds, handling `u32` wrap (~49.7 days).
/// Returns 0 if `ts` is ahead of `newest` (e.g. a timestamp reset or slightly ahead extra track).
fn behind_ms(newest: u32, ts: u32) -> u32 {
    let diff = newest.wrapping_sub(ts);
    if diff < u32::MAX / 2 { diff } else { 0 }
}

/// Per-track drop gate for a live destination (port of OBS `rtmp-stream.c`
/// `check_to_drop_frames` / `drop_frames`, extended per video track so Twitch Enhanced
/// Broadcasting extra qualities each resume cleanly on their own keyframe).
#[derive(Default)]
struct VideoDrop([u64; 4]);

impl VideoDrop {
    fn is_dropping(&self, track: u8) -> bool {
        (self.0[(track / 64) as usize] & (1u64 << (track % 64))) != 0
    }

    fn mark(&mut self, track: u8) {
        self.0[(track / 64) as usize] |= 1u64 << (track % 64);
    }

    fn clear(&mut self, track: u8) {
        self.0[(track / 64) as usize] &= !(1u64 << (track % 64));
    }

    fn mark_all(&mut self) {
        self.0 = [u64::MAX; 4];
    }

    /// Whether a non-header video packet on `track` should be dropped, updating the track's
    /// drop state. Keyframes are never dropped and end dropping for their track.
    fn should_drop(&mut self, track: u8, key: bool, newest_ts: Option<u32>, ts: u32, queued: usize) -> bool {
        if key {
            self.clear(track);
            return false;
        }
        if self.is_dropping(track) {
            return true;
        }
        if queued >= MIN_BACKLOG_PACKETS && newest_ts.is_some_and(|newest| behind_ms(newest, ts) > DROP_THRESHOLD_MS) {
            self.mark(track);
            return true;
        }
        false
    }
}

/// Timestamps start at zero for each destination's session (a destination joining mid-stream
/// would otherwise start hours in).
fn stamp(base: &mut Option<u32>, ts: u32) -> RtmpTimestamp {
    let b = *base.get_or_insert(ts);
    RtmpTimestamp::new(ts.wrapping_sub(b))
}

/// A connection that a server accepted for publishing: the forwarder's and the upload test's.
pub(crate) struct Publisher {
    pub(crate) client: ClientSession,
    s: Stream,
    host: String,
    buf: Vec<u8>,
    /// Bytes written since the connection opened.
    pub(crate) sent: u64,
}

impl Publisher {
    /// Connects, shakes hands, connects to the app and asks to publish with `key`.
    pub(crate) async fn open(t: &Target, key: &str, via: Option<&links::Network>) -> Result<Self, PushError> {
        let host = t.host.clone();
        let mut s = connect(t, via).await?;
        let rest = timeout(STEP_DEADLINE, conn::handshake(&mut s, PeerType::Client))
            .await
            .map_err(|_| PushError::Slow(host.clone()))?
            .map_err(|e| match e {
                conn::HandshakeFail::Closed => PushError::Closed(host.clone()),
                conn::HandshakeFail::NotRtmp => PushError::NotRtmp(host.clone()),
                conn::HandshakeFail::Io(_) => PushError::Broken(host.clone()),
            })?;
        let mut cfg = ClientSessionConfig::new();
        cfg.tc_url = Some(t.tc_url.clone());
        cfg.flash_version = "FMLE/3.0 (compatible; FMSc/1.0)".to_string();
        let (client, first) = ClientSession::new(cfg).map_err(|_| PushError::NotRtmp(host.clone()))?;
        let mut p = Self { client, s, host, buf: vec![0u8; READ_BUF], sent: 0 };
        p.results(first).await?;
        let mut pending = p.client.handle_input(&rest).map_err(|_| p.not_rtmp())?;

        let r = p.client.request_connection(t.app.clone());
        p.send(r).await?;
        let host = p.host.clone();
        p.wait_for(&mut pending, |ev| match ev {
            ClientSessionEvent::ConnectionRequestAccepted => Some(Ok(())),
            ClientSessionEvent::ConnectionRequestRejected { description } => {
                Some(Err(PushError::Refused(host.clone(), description.clone())))
            }
            _ => None,
        })
        .await?;
        let r = p.client.request_publishing(key.to_string(), PublishRequestType::Live);
        p.send(r).await?;
        // Platforms answer a bad key with an error status, or by hanging up (Twitch).
        p.wait_for(&mut pending, |ev| match ev {
            ClientSessionEvent::PublishRequestAccepted => Some(Ok(())),
            ClientSessionEvent::UnhandleableOnStatusCode { code }
                if code.contains("BadName") || code.contains("Failed") =>
            {
                Some(Err(PushError::KeyRefused(host.clone())))
            }
            _ => None,
        })
        .await
        // Twitch over rtmps:// resets the connection instead of closing it cleanly.
        .map_err(|e| match e {
            PushError::Closed(h) | PushError::Broken(h) => PushError::KeyRefused(h),
            e => e,
        })?;
        Ok(p)
    }

    fn not_rtmp(&self) -> PushError {
        PushError::NotRtmp(self.host.clone())
    }

    /// Writes one session result (a packet to send, or an error making it).
    pub(crate) async fn send(&mut self, r: Result<ClientSessionResult, ClientSessionError>) -> Result<(), PushError> {
        let r = r.map_err(|_| self.not_rtmp())?;
        self.results(vec![r]).await.map(|_| ())
    }

    /// Writes outbound packets; returns the events among the results.
    async fn results(&mut self, results: Vec<ClientSessionResult>) -> Result<Vec<ClientSessionEvent>, PushError> {
        let mut events = Vec::new();
        for r in results {
            match r {
                ClientSessionResult::OutboundResponse(p) => self.write(&p.bytes).await?,
                ClientSessionResult::RaisedEvent(ev) => events.push(ev),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(events)
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), PushError> {
        timeout(WRITE_DEADLINE, self.s.write_all(bytes))
            .await
            .map_err(|_| PushError::Stalled(self.host.clone()))?
            .map_err(|_| PushError::Broken(self.host.clone()))?;
        self.sent += bytes.len() as u64;
        Ok(())
    }

    /// Reads what the server sent (acknowledgements, pings) into the buffer. Safe to drop
    /// in `select!`: nothing is lost if it never completes.
    pub(crate) async fn read_some(&mut self) -> Result<usize, PushError> {
        let n = self.s.read(&mut self.buf).await.map_err(|_| PushError::Broken(self.host.clone()))?;
        if n == 0 {
            return Err(PushError::Closed(self.host.clone()));
        }
        Ok(n)
    }

    /// Answers the `n` bytes [`Self::read_some`] read.
    pub(crate) async fn answer(&mut self, n: usize) -> Result<(), PushError> {
        let results = self.client.handle_input(&self.buf[..n]).map_err(|_| self.not_rtmp())?;
        self.results(results).await.map(|_| ())
    }

    /// Cleanly closes the outbound stream (TCP FIN / TLS `close_notify`) and briefly drains
    /// inbound bytes (e.g. an unread RTMP Ack) so dropping the socket doesn't emit a TCP RST
    /// that discards the last in-flight packets at the server.
    async fn finish(mut self) {
        let _ = timeout(Duration::from_millis(500), async {
            let _ = self.s.shutdown().await;
            while matches!(self.s.read(&mut self.buf).await, Ok(n) if n > 0) {}
        })
        .await;
    }

    /// Reads and answers the server until `done` picks an event (within the step deadline).
    async fn wait_for(
        &mut self,
        pending: &mut Vec<ClientSessionResult>,
        mut done: impl FnMut(&ClientSessionEvent) -> Option<Result<(), PushError>>,
    ) -> Result<(), PushError> {
        let deadline = Instant::now() + STEP_DEADLINE;
        loop {
            let events = self.results(std::mem::take(pending)).await?;
            for ev in &events {
                if let Some(r) = done(ev) {
                    return r;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let n = timeout(left, self.s.read(&mut self.buf))
                .await
                .map_err(|_| PushError::Slow(self.host.clone()))?
                .map_err(|_| PushError::Broken(self.host.clone()))?;
            if n == 0 {
                return Err(PushError::Closed(self.host.clone()));
            }
            *pending = self.client.handle_input(&self.buf[..n]).map_err(|_| self.not_rtmp())?;
        }
    }
}

#[cfg(test)]
mod via_tests {
    use super::*;

    /// A real server over a chosen connection: FERROCAST_VIA (a connection's name) and
    /// FERROCAST_RTMP (an rtmp:// address with its key path), e.g. a local MediaMTX.
    #[tokio::test]
    #[ignore = "needs an RTMP server and a second connection"]
    async fn publishes_over_the_connection_asked() {
        let (Ok(via), Ok(url)) = (std::env::var("FERROCAST_VIA"), std::env::var("FERROCAST_RTMP")) else { return };
        let net = links::networks().into_iter().find(|n| n.name == via).expect("no such connection");
        let t = Target::parse(&url).unwrap();
        let p = Publisher::open(&t, "test", Some(&net)).await.unwrap();
        drop(p);
        println!("published over {} ({})", links::label(&net.name), net.ip);
    }

    #[test]
    fn only_far_servers_and_network_failures_move() {
        let t = |u: &str| Target::parse(u).unwrap();
        assert!(may_move(&t("rtmp://live.twitch.tv/app")));
        assert!(!may_move(&t("rtmp://127.0.0.1/live")));
        assert!(!may_move(&t("rtmp://192.168.1.9/live")));
        assert!(!may_move(&t("rtmp://localhost/live")));
        assert!(PushError::Unreachable("x".into()).is_network());
        assert!(!PushError::KeyRefused("x".into()).is_network());
    }

    #[test]
    fn behind_ms_handles_wrap_and_backwards_resets() {
        assert_eq!(behind_ms(1_000, 1_000), 0);
        assert_eq!(behind_ms(1_900, 1_000), 900);
        assert_eq!(behind_ms(1_901, 1_000), 901);
        // Wraps cleanly across u32::MAX -> 0 (~49.7 days of stream time).
        assert_eq!(behind_ms(100, u32::MAX - 899), 1_000);
        assert_eq!(behind_ms(0, u32::MAX), 1);
        // Packet ahead of newest (or timestamp reset backwards): never treated as huge lag.
        assert_eq!(behind_ms(1_000, 1_050), 0);
        assert_eq!(behind_ms(100, 600_000), 0);
        assert_eq!(behind_ms(u32::MAX, 0), 0);
    }

    #[test]
    fn video_drop_respects_threshold_backlog_keyframes_and_extra_tracks() {
        let mut d = VideoDrop::default();
        // At threshold (900 ms) or sparse queue (< 4 queued behind): never drops.
        assert!(!d.should_drop(0, false, Some(1_900), 1_000, 10));
        assert!(!d.should_drop(0, false, Some(2_500), 1_000, MIN_BACKLOG_PACKETS - 1));
        assert!(!d.should_drop(0, false, None, 1_000, 10));
        // Keyframes never drop even when far behind.
        assert!(!d.should_drop(0, true, Some(5_000), 1_000, 50));

        // Past threshold with enough backlog: starts dropping track 0.
        assert!(d.should_drop(0, false, Some(1_901), 1_000, MIN_BACKLOG_PACKETS));
        // Stays dropping track 0 even once caught up, until track 0 sees a keyframe.
        assert!(d.should_drop(0, false, Some(1_050), 1_050, 0));
        // Track 1 isn't affected by track 0 alone when track 1 isn't behind.
        assert!(!d.should_drop(1, false, Some(1_050), 1_050, 0));
        // Track 0 keyframe clears track 0; subsequent in-time inter frames pass.
        assert!(!d.should_drop(0, true, Some(2_000), 2_000, 0));
        assert!(!d.should_drop(0, false, Some(2_033), 2_033, 0));

        // Wraps across u32::MAX -> 0 cleanly to trigger drop on track 1.
        assert!(d.should_drop(1, false, Some(100), u32::MAX - 850, MIN_BACKLOG_PACKETS));
        assert!(d.should_drop(1, false, Some(120), 120, 0));
        assert!(!d.should_drop(1, true, Some(150), 150, 0));

        // mark_all (after RecvError::Lagged) drops every track until its own keyframe.
        d.mark_all();
        assert!(d.should_drop(0, false, Some(3_000), 3_000, 0));
        assert!(d.should_drop(2, false, Some(3_000), 3_000, 0));
        assert!(!d.should_drop(0, true, Some(3_050), 3_050, 0));
        assert!(!d.should_drop(0, false, Some(3_083), 3_083, 0));
        assert!(d.should_drop(2, false, Some(3_083), 3_083, 0));
        assert!(!d.should_drop(2, true, Some(3_100), 3_100, 0));
    }
}
