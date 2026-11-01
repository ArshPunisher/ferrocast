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
                    // A session that went live and then failed starts the count over: a
                    // platform that keeps dropping a long stream is retried at once, not
                    // after the longest wait.
                    if health.borrow().state == PushState::Live {
                        attempt = 0;
                    }
                    attempt = attempt.saturating_add(1);
                    let wait = backoff(attempt, &dest.retry);
                    if err.is_network() && may_move(&dest.target) {
                        via = links::next_after(via.as_ref(), &links::fallbacks(), links::default_ip());
                    }
                    tracing::info!(dest = %dest.id, attempt, error = %err, via = ?via.as_ref().map(|n| &n.name), "golive: destination failed, retrying");
                    set_state(
                        &health,
                        PushState::Reconnecting {
                            attempt,
                            retry_in_ms: wait.as_millis() as u64,
                            reason: err.to_string(),
                        },
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
    if dest.extra_audio {
        for (_, a) in headers.extra_audio {
            let r = p.client.publish_audio_data(a, RtmpTimestamp::new(0), false);
            p.send(r).await?;
        }
    }
    set_state(health, PushState::Live);
    let mut base: Option<u32> = None;
    let mut waiting_for_key = true;
    let mut window = (Instant::now(), p.sent);
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        health.send_modify(|h| h.dropped += n);
                        waiting_for_key = true;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(Ended::StreamEnded),
                };
                let r = match frame {
                    Frame::End => return Ok(Ended::StreamEnded),
                    Frame::Metadata(m) => p.client.publish_metadata(&m),
                    // Codec headers always go through; other packets wait for a real keyframe.
                    Frame::Video { ts, data, key } => {
                        if !flv::video_is_header(&data) {
                            if waiting_for_key && !key {
                                continue;
                            }
                            waiting_for_key = false;
                        }
                        p.client.publish_video_data(data, stamp(&mut base, ts), false)
                    }
                    Frame::Audio { ts, data } => {
                        let extra = flv::audio_extra_track(&data);
                        if extra.is_some() && !dest.extra_audio {
                            continue;
                        }
                        let header = flv::audio_is_header(&data) || extra.is_some_and(|(_, h)| h);
                        if waiting_for_key && !header {
                            continue;
                        }
                        p.client.publish_audio_data(data, stamp(&mut base, ts), false)
                    }
                };
                p.send(r).await?;
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
}
