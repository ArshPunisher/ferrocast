//! One task per destination (R-18): while the stream is live it connects, publishes with the
//! destination's key, sends the codec headers, then the same packets OBS sent (no re-encode),
//! starting at a keyframe. A failure retries with backoff and jitter for as long as the stream
//! runs; one destination failing or falling behind never touches the others. Health goes out on
//! a watch channel, with the reason in words (R-08).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// Beyond `2×` [`DROP_THRESHOLD_MS`], if older keyframes sit ahead of the lagging stream, skip
/// forward to the newest keyframe (dropping intermediate keyframes and their GOP audio) so a link
/// slower than keyframes alone does not drift endlessly behind live (finding M3.4, beyond OBS).
const KEY_SKIP_THRESHOLD_MS: u32 = DROP_THRESHOLD_MS * 2;
/// Minimum packets still queued behind the current packet in the destination's receiver before
/// time-based dropping starts (at least 5 buffered packets including the current one, matching
/// OBS's `num_packets < 5` guard in `check_to_drop_frames`). Prevents a sparse timestamp gap or
/// brief source pause from falsely dropping frames.
const MIN_BACKLOG_PACKETS: usize = 4;
/// Default consecutive failures before a destination gives up for the rest of the stream.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 6;
/// Consecutive auth/key refusals before giving up (no point hammering a bad key).
pub const REFUSED_MAX_ATTEMPTS: u32 = 3;
/// How long a destination waits for a network that is down (a Wi-Fi drop, a router reboot) before
/// giving up for this stream. OBS rides out outages of about this length.
pub const NETWORK_PATIENCE: Duration = Duration::from_secs(10 * 60);
/// The longest wait between tries while the network is down: it comes back and the stream follows
/// within about this long.
const NETWORK_RETRY_MAX: Duration = Duration::from_secs(8);

/// How long a destination waits before trying again: the first wait, doubling up to the
/// longest (Advanced mode can change both), and how many failures in a row before giving up
/// (`0`: keep trying forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub first: Duration,
    pub max: Duration,
    pub max_attempts: u32,
}

impl Default for Retry {
    fn default() -> Self {
        Self { first: Duration::from_secs(1), max: Duration::from_secs(30), max_attempts: DEFAULT_MAX_ATTEMPTS }
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
    /// A second server for the same key, tried after [`BACKUP_AFTER`] failures to connect to
    /// `target` (YouTube's backup ingest), and `target` again once the backup has held.
    pub backup: Option<Target>,
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
    /// Gave up for this stream after repeated failures or a platform refusal (contract C-1).
    Failed {
        attempts: u32,
        /// One short line saying what happened.
        reason: String,
        /// Cause and what to do, shown behind an (i).
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// True when the platform refused the stream (bad key, Kick multistreaming) rather than a network outage.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        refused: bool,
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
    /// The server hung up when the stream key was sent (Twitch does this for a wrong key, and an
    /// ingest node that is draining does it for anything). Which one is only known from whether
    /// the key worked before in this stream.
    #[error("{0} closed the connection when the stream started.")]
    PublishDropped(String),
    /// No way out to the internet (no route, no DNS, nothing answering): a Wi-Fi drop or a router
    /// reboot. Not the platform's doing, so it isn't counted against the destination.
    #[error("Your network is down: waiting for it to reach {0}.")]
    NetworkDown(String),
}

impl PushError {
    /// The network failed (not the server saying no): another connection may do better.
    fn is_network(&self) -> bool {
        matches!(
            self,
            Self::Unreachable(_)
                | Self::PublishDropped(_)
                | Self::NetworkDown(_)
                | Self::Slow(_)
                | Self::Closed(_)
                | Self::Stalled(_)
                | Self::Broken(_)
        )
    }

    /// The server could not be had at all (not a no to this stream or its key): another server
    /// for the same key may answer.
    fn is_connect_failure(&self) -> bool {
        !matches!(self, Self::Refused(..) | Self::KeyRefused(_))
    }
}

/// How many tries in a row, before the stream is live, may fail on one server before the other is
/// tried (when a destination has a backup).
pub const BACKUP_AFTER: u32 = 3;

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
        // Tries made while the network was down (not counted in `attempt`), and since when it is.
        let mut net_tries = 0u32;
        let mut down_since: Option<Instant> = None;
        // This stream has been live on this destination: its key works.
        let mut ever_live = false;
        // The connection used: the system's default until that fails, then the next one.
        let mut via: Option<links::Network> = None;
        // The server used: the destination's own, until it can't be reached `BACKUP_AFTER` times in
        // a row, then its backup (and back, the same way); each stream starts on the own one.
        let mut on_backup = false;
        let mut connect_failures = 0u32;
        'connect: loop {
            let target = match (&dest.backup, on_backup) {
                (Some(b), true) => b,
                _ => &dest.target,
            };
            // A retry doesn't blank "Reconnecting" (and why) for the length of the try: only a
            // first connect, or one after a live session, shows Connecting.
            if !matches!(health.borrow().state, PushState::Reconnecting { .. }) {
                set_state(&health, PushState::Connecting);
            }
            let mut live_at: Option<Instant> = None;
            let outcome = tokio::select! {
                _ = stop.wait_for(|s| *s) => Ok(Ended::Stopped),
                r = session(&dest, target, &hub, &health, stream, via.as_ref(), &mut live_at) => r,
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
                    let (was_live, live_for_ms) = match live_at {
                        Some(t) => (true, u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)),
                        None => (false, 0),
                    };
                    // Kick taking the stream and closing it at once (no multistream) doesn't prove the
                    // key or the address works; any other session that was live does, however short
                    // (a drop half a second in, then a reconnect, is still a server that took the key).
                    ever_live |= was_live && !(live_for_ms < STAYED_LIVE_MS && is_kick(&dest));
                    // A hang-up at the publish: a wrong key before the key ever worked in this
                    // stream (what it was always taken for), a server dropping the connection after.
                    let err = match err {
                        PushError::PublishDropped(h) if !ever_live => PushError::KeyRefused(h),
                        e => e,
                    };
                    let key_refused = matches!(err, PushError::KeyRefused(_));
                    let rapid_close = was_live && live_for_ms < STAYED_LIVE_MS;
                    let stayed = was_live && live_for_ms >= STAYED_LIVE_MS;
                    if stayed {
                        attempt = 0;
                    }
                    // The server: after a backup that held, the destination's own again (it may
                    // have come back); after enough failures to connect, the other one.
                    let mut switched = false;
                    if dest.backup.is_some() {
                        if stayed && on_backup {
                            on_backup = false;
                            connect_failures = 0;
                            switched = true;
                        } else if !was_live && err.is_connect_failure() && !key_refused {
                            connect_failures += 1;
                            if connect_failures >= BACKUP_AFTER {
                                on_backup = !on_backup;
                                connect_failures = 0;
                                switched = true;
                            }
                        } else {
                            connect_failures = 0;
                        }
                    }
                    // A network that is down isn't the destination failing: its tries are counted
                    // apart and don't use up the ones an ordinary failure gets, and it is waited for
                    // (up to NETWORK_PATIENCE) rather than given up on after a few seconds.
                    // Also a server that took this stream earlier and now refuses connections at all
                    // (an ingest restarting, one's own server rebooting): an outage to wait for. One
                    // that never took it stays an ordinary failure (a wrong address says so fast).
                    let net_down = matches!(err, PushError::NetworkDown(_))
                        || (ever_live && matches!(err, PushError::Unreachable(_) | PushError::Slow(_)));
                    if net_down {
                        net_tries = net_tries.saturating_add(1);
                        down_since.get_or_insert_with(Instant::now);
                    } else {
                        attempt = attempt.saturating_add(1);
                        net_tries = 0;
                        // The patience clock restarts only once a session really took the stream
                        // again, not on one odd error between two "network down"s.
                        if was_live {
                            down_since = None;
                        }
                    }
                    let kick_refused = rapid_close && is_kick(&dest);
                    let server_refused = key_refused || matches!(err, PushError::Refused(..));
                    let is_refused = kick_refused || server_refused;
                    let limit = if dest.retry.max_attempts == 0 {
                        0
                    } else if server_refused {
                        dest.retry.max_attempts.min(REFUSED_MAX_ATTEMPTS)
                    } else {
                        dest.retry.max_attempts
                    };
                    let target = match (&dest.backup, on_backup) {
                        (Some(b), true) => b,
                        _ => &dest.target,
                    };
                    // Never hop network interfaces on a server refusal or rapid policy close:
                    // the connection reached the server and the platform itself closed it.
                    if err.is_network() && !key_refused && !rapid_close && may_move(target) {
                        via = links::next_after(via.as_ref(), &links::fallbacks(), links::default_ip());
                    }
                    let give_up = if net_down {
                        limit > 0 && down_since.is_some_and(|t| t.elapsed() >= NETWORK_PATIENCE)
                    } else {
                        limit > 0 && attempt >= limit
                    };
                    if give_up {
                        let (short_reason, detail) = failure_text(&dest, &err, kick_refused);
                        tracing::warn!(
                            dest = %dest.id,
                            attempts = attempt.max(net_tries),
                            refused = is_refused,
                            on_backup,
                            error = %err,
                            "golive: destination failed, giving up for this stream"
                        );
                        set_state(
                            &health,
                            PushState::Failed {
                                attempts: attempt.max(net_tries),
                                reason: short_reason,
                                detail,
                                refused: is_refused,
                            },
                        );
                        // A refusal (a bad key, Kick's multistream) stays parked: asking again won't
                        // change the answer. Anything else (an outage that outlasted the tries, a
                        // platform node down for minutes) is tried again slowly while the stream
                        // goes on, so one platform's bad half hour doesn't end it for the whole stream.
                        if is_refused {
                            tokio::select! {
                                _ = stop.wait_for(|s| *s) => break 'streams,
                                _ = live.wait_for(|l| !still_on(l)) => break,
                            }
                        }
                        // One try per wait (1, 2, then 5 minutes), still Failed while it runs; only a
                        // session that really goes live starts the ordinary loop again.
                        let mut step = 0usize;
                        loop {
                            let wait = SLOW_RETRY[step.min(SLOW_RETRY.len() - 1)].mul_f64(0.8 + fastrand::f64() * 0.4);
                            tokio::select! {
                                _ = stop.wait_for(|s| *s) => break 'streams,
                                _ = live.wait_for(|l| !still_on(l)) => break 'connect,
                                _ = tokio::time::sleep(wait) => {}
                            }
                            let mut probe_live: Option<Instant> = None;
                            let probed = tokio::select! {
                                _ = stop.wait_for(|s| *s) => Ok(Ended::Stopped),
                                r = session(&dest, target, &hub, &health, stream, via.as_ref(), &mut probe_live) => r,
                            };
                            match probed {
                                Ok(Ended::Stopped) => break 'streams,
                                Ok(Ended::StreamEnded) => break 'connect,
                                Err(_) => {
                                    if !still_on(&live.borrow()) {
                                        break 'connect;
                                    }
                                    if probe_live.is_some() {
                                        attempt = 0;
                                        net_tries = 0;
                                        down_since = None;
                                        continue 'connect;
                                    }
                                    step += 1;
                                }
                            }
                        }
                    }
                    let shown = if net_down { net_tries } else { attempt };
                    let wait = if net_down {
                        backoff(net_tries, &dest.retry).min(NETWORK_RETRY_MAX)
                    } else {
                        backoff(attempt, &dest.retry)
                    };
                    tracing::info!(dest = %dest.id, attempt = shown, error = %err, via = ?via.as_ref().map(|n| &n.name), on_backup, "golive: destination failed, retrying");
                    let reason = if switched {
                        format!(
                            "{err} Trying {}.",
                            if on_backup { "its backup server" } else { "its main server again" }
                        )
                    } else if kick_refused {
                        kick_multistream_detail(&err)
                    } else {
                        err.to_string()
                    };
                    set_state(
                        &health,
                        PushState::Reconnecting { attempt: shown, retry_in_ms: wait.as_millis() as u64, reason },
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

/// How long a destination that gave up (not for a refusal) waits before each single try, for as long
/// as the stream goes on: it stays Failed meanwhile.
#[cfg(not(test))]
const SLOW_RETRY: [Duration; 3] = [Duration::from_secs(60), Duration::from_secs(120), Duration::from_secs(300)];
#[cfg(test)]
const SLOW_RETRY: [Duration; 3] = [Duration::from_millis(150), Duration::from_millis(300), Duration::from_millis(600)];

/// How long a session stays live before its failure starts the retry count over.
pub(crate) const STAYED_LIVE_MS: u64 = 30_000;

/// Kick's built-in destination or a custom server pointed at Kick's IVS ingest.
fn is_kick(dest: &Destination) -> bool {
    let id = dest.id.to_ascii_lowercase();
    let host = dest.target.host.to_ascii_lowercase();
    id.starts_with("kick") || host.contains("kick") || host.starts_with("fa723fc1b171.")
}

fn kick_multistream_detail(err: &PushError) -> String {
    format!(
        "{err} Kick took the stream and closed it at once: streaming to other platforms too \
         needs \"Allow multistreaming\" in Kick's settings (Kick Partners), else turn Kick off."
    )
}

/// One short line and an optional `(i)` detail for terminal `PushState::Failed`.
fn failure_text(dest: &Destination, err: &PushError, kick_refused: bool) -> (String, Option<String>) {
    if kick_refused {
        return ("Kick refused the stream.".into(), Some(kick_multistream_detail(err)));
    }
    match err {
        PushError::NetworkDown(host) => (
            "The network stayed down; stopped trying.".into(),
            Some(format!(
                "{host} couldn't be reached for {} minutes. Start the destination again once the internet is back.",
                NETWORK_PATIENCE.as_secs() / 60
            )),
        ),
        PushError::KeyRefused(_) => ("Stream key was refused.".into(), Some(err.to_string())),
        PushError::Refused(_, _) => ("The platform refused the stream.".into(), Some(err.to_string())),
        _ => {
            let full = err.to_string();
            if full.chars().count() <= 60 {
                (full, None)
            } else {
                (format!("{} stopped taking the stream.", dest.target.host), Some(full))
            }
        }
    }
}

/// 1, 2, 4, 8, 16, 30, 30… seconds by default, each ±20% so many destinations don't retry in
/// step.
pub(crate) fn backoff(attempt: u32, retry: &Retry) -> Duration {
    let base = retry.first.saturating_mul(1u32 << attempt.saturating_sub(1).min(8)).min(retry.max);
    let jitter = 0.8 + fastrand::f64() * 0.4;
    base.mul_f64(jitter)
}

pub(crate) fn now_ms() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    #[cfg(test)]
    let t = t.saturating_add_signed(fault::CLOCK_STEP_MS.with(std::cell::Cell::get));
    t
}

/// What a failed TCP connect says. No route, no network or an interface that's down is the network
/// being down (waited for, even before the stream was ever live). A name that can't be looked up, a
/// refusal or a reset is a wrong address or that server not taking it: counted like any failure,
/// and only waited for by a destination that was live before (it worked a moment ago).
fn connect_error(e: &std::io::Error, host: &str) -> PushError {
    use std::io::ErrorKind as K;
    if matches!(e.kind(), K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown) {
        return PushError::NetworkDown(host.to_string());
    }
    if e.kind() == K::TimedOut {
        return PushError::Slow(host.to_string());
    }
    PushError::Unreachable(host.to_string())
}

/// Tests take the network away from one destination (by its port) and give it back.
#[cfg(test)]
pub(crate) mod fault {
    use std::sync::Mutex;
    static DOWN: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    pub(crate) fn set(port: u16, down: bool) {
        let mut d = DOWN.lock().unwrap_or_else(|e| e.into_inner());
        d.retain(|p| *p != port);
        if down {
            d.push(port);
        }
    }
    thread_local! {
        /// The wall clock stepped by this many ms in this test (an NTP correction, a wake from sleep).
        pub(crate) static CLOCK_STEP_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }
    pub(crate) fn is_down(port: u16) -> bool {
        DOWN.lock().unwrap_or_else(|e| e.into_inner()).contains(&port)
    }
    static SLOW: Mutex<Vec<(u16, u64)>> = Mutex::new(Vec::new());
    /// Connecting to this port takes this long (ms) before it fails: a connect that hangs.
    pub(crate) fn set_slow(port: u16, ms: u64) {
        let mut d = SLOW.lock().unwrap_or_else(|e| e.into_inner());
        d.retain(|(p, _)| *p != port);
        if ms > 0 {
            d.push((port, ms));
        }
    }
    pub(crate) fn slow_ms(port: u16) -> u64 {
        SLOW.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _)| *p == port).map_or(0, |(_, m)| *m)
    }
    static CONNECTS: Mutex<Vec<(u16, u32)>> = Mutex::new(Vec::new());
    /// Counts a connect attempt to this port.
    pub(crate) fn count_connect(port: u16) {
        let mut d = CONNECTS.lock().unwrap_or_else(|e| e.into_inner());
        match d.iter_mut().find(|(p, _)| *p == port) {
            Some((_, n)) => *n += 1,
            None => d.push((port, 1)),
        }
    }
    pub(crate) fn connects(port: u16) -> u32 {
        CONNECTS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _)| *p == port).map_or(0, |(_, n)| *n)
    }
    static NO_NAME: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    /// The destination's name can't be looked up (a typo, or no DNS).
    pub(crate) fn set_no_name(port: u16, on: bool) {
        let mut d = NO_NAME.lock().unwrap_or_else(|e| e.into_inner());
        d.retain(|p| *p != port);
        if on {
            d.push(port);
        }
    }
    pub(crate) fn no_name(port: u16) -> bool {
        NO_NAME.lock().unwrap_or_else(|e| e.into_inner()).contains(&port)
    }
}

/// Connects to `t`, over `via` when given (else the system's default way).
pub(crate) async fn connect(t: &Target, via: Option<&links::Network>) -> Result<Stream, PushError> {
    let host = t.host.clone();
    #[cfg(test)]
    fault::count_connect(t.port);
    #[cfg(test)]
    {
        let ms = fault::slow_ms(t.port);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
    #[cfg(test)]
    if fault::is_down(t.port) {
        return Err(PushError::NetworkDown(host));
    }
    #[cfg(test)]
    if fault::no_name(t.port) {
        return Err(connect_error(&std::io::Error::other("failed to lookup address information"), &host));
    }
    let tcp = match (via, t.pin) {
        (None, Some(addr)) => timeout(CONNECT_DEADLINE, TcpStream::connect(addr)).await,
        (None, None) => timeout(CONNECT_DEADLINE, TcpStream::connect((t.host.as_str(), t.port))).await,
        (Some(n), _) => timeout(CONNECT_DEADLINE, links::tcp_via(n, &t.host, t.port)).await,
    }
    // Not answering at all looks the same whether the link or the server is down (or the address
    // is wrong): an ordinary failure, waited for only by a destination that was live before.
    .map_err(|_| PushError::Slow(host.clone()))?
    .map_err(|e| connect_error(&e, &host))?;
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
    target: &Target,
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
    via: Option<&links::Network>,
    live_at: &mut Option<Instant>,
) -> Result<Ended, PushError> {
    let mut p = Publisher::open(target, &dest.key, via).await?;
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
    // Measured on the monotonic clock: the wall clock steps (NTP, a wake from sleep).
    *live_at = Some(Instant::now());
    let mut base: Option<u32> = None;
    let mut started = false;
    let mut video_drop = VideoDrop::default();
    let progress = p.progress.clone();
    let mut window = (Instant::now(), progress.load(Ordering::Relaxed));
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        health.send_modify(|h| h.dropped = h.dropped.saturating_add(n));
                        if started {
                            video_drop.on_lagged();
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
                            if video_drop.should_drop(
                                track,
                                track_key,
                                hub.newest_video_ts(),
                                hub.newest_key_ts(),
                                hub.prev_key_ts(),
                                ts,
                                rx.len(),
                            ) {
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
                        let send = if extra.is_some() && !dest.extra_audio {
                            false
                        } else if header {
                            true
                        } else if !started {
                            false
                        } else if video_drop.should_drop_audio(ts, hub.newest_video_ts(), hub.newest_key_ts()) {
                            health.send_modify(|h| h.dropped = h.dropped.saturating_add(1));
                            false
                        } else {
                            true
                        };
                        send.then(|| p.client.publish_audio_data(data, stamp(&mut base, ts), false))
                    }
                };
                if let Some(r) = outbound {
                    // While the write goes on (a destination that stopped reading blocks it for
                    // up to the write deadline) the rate is still reported each second: 0 when
                    // the kernel takes nothing, so upload protection sees it at once.
                    let send = p.send(r);
                    tokio::pin!(send);
                    loop {
                        let left = HEALTH_EVERY.saturating_sub(window.0.elapsed()).max(Duration::from_millis(50));
                        tokio::select! {
                            done = &mut send => { done?; break; }
                            _ = tokio::time::sleep(left) => report_rate(health, &mut window, progress.load(Ordering::Relaxed)),
                        }
                    }
                }
            }
            // Only the read races the next packet (a read can be dropped safely); the answer
            // is written after, so a write is never cut off halfway.
            n = p.read_some() => {
                let n = n?;
                p.answer(n).await?;
            }
        }
        report_rate(health, &mut window, progress.load(Ordering::Relaxed));
    }
}

/// Once a second: what the destination took in that time, as kilobits a second, and the bytes
/// counted since the last report. (`window`: when it started and the count then.)
fn report_rate(health: &watch::Sender<Health>, window: &mut (Instant, u64), now_sent: u64) {
    let secs = window.0.elapsed().as_secs_f64();
    if secs < HEALTH_EVERY.as_secs_f64() {
        return;
    }
    let took = now_sent.saturating_sub(window.1);
    let kbps = (took as f64 * 8.0 / 1000.0 / secs) as u32;
    health.send_modify(|h| {
        h.kbps = kbps;
        h.sent += took;
    });
    *window = (Instant::now(), now_sent);
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
struct VideoDrop {
    tracks: [u64; 4],
    /// Main-track keyframe timestamp (ms) we are skipping to when far behind (`> KEY_SKIP_THRESHOLD_MS`),
    /// dropping all older video (including intermediate keyframes) and non-header audio strictly before it.
    skip_to_key: Option<u32>,
}

impl VideoDrop {
    fn is_dropping(&self, track: u8) -> bool {
        (self.tracks[(track / 64) as usize] & (1u64 << (track % 64))) != 0
    }

    fn mark(&mut self, track: u8) {
        self.tracks[(track / 64) as usize] |= 1u64 << (track % 64);
    }

    fn clear(&mut self, track: u8) {
        self.tracks[(track / 64) as usize] &= !(1u64 << (track % 64));
    }

    fn mark_all(&mut self) {
        self.tracks = [u64::MAX; 4];
    }

    fn on_lagged(&mut self) {
        self.mark_all();
        self.skip_to_key = None;
    }

    /// Whether a non-header video packet on `track` should be dropped, updating per-track and
    /// keyframe-skip state.
    ///
    /// Normal lag (`> DROP_THRESHOLD_MS`) drops only inter frames while keeping keyframes and audio
    /// continuous (OBS `drop_frames`). When far behind (`> KEY_SKIP_THRESHOLD_MS`) and an older
    /// keyframe can actually be skipped ahead to `newest_key_ts`, we jump to `newest_key_ts` so a
    /// link slower than keyframes alone does not drift endlessly.
    #[allow(clippy::too_many_arguments)]
    fn should_drop(
        &mut self,
        track: u8,
        key: bool,
        newest_ts: Option<u32>,
        newest_key_ts: Option<u32>,
        prev_key_ts: Option<u32>,
        ts: u32,
        queued: usize,
    ) -> bool {
        let is_main = track == 0;
        let can_skip_ahead = newest_ts
            .zip(newest_key_ts)
            .filter(|&(nv, nk)| behind_ms(nv, ts) > KEY_SKIP_THRESHOLD_MS && behind_ms(nk, ts) > 0)
            .is_some_and(|(_, nk)| {
                queued >= MIN_BACKLOG_PACKETS
                    && ((is_main && key)
                        || prev_key_ts.is_some_and(|pk| behind_ms(pk, ts) > 0)
                        || self.skip_to_key.is_some_and(|target| nk != target))
            });

        if can_skip_ahead && let Some(nk) = newest_key_ts {
            self.skip_to_key = Some(nk);
            self.mark_all();
            return true;
        }

        if let Some(target) = self.skip_to_key {
            if behind_ms(target, ts) > 0 {
                return true;
            }
            if is_main && key {
                // Reached target main keyframe: resume main video and wait for each extra track's keyframe.
                self.skip_to_key = None;
                self.clear(0);
                return false;
            }
            return true;
        }

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

    /// Whether non-header audio at `ts` belongs to skipped GOPs during an active keyframe jump.
    ///
    /// During normal single-GOP P-frame drops, `skip_to_key` is `None` so audio stays continuous.
    /// When skipping older keyframes on links slower than keyframes alone, stale audio before
    /// `target_key_ts` must drop with its video so RTMP/HLS players don't receive seconds of
    /// stalled audio out of sync with live video and so audio doesn't saturate the slow socket.
    fn should_drop_audio(&mut self, ts: u32, newest_ts: Option<u32>, newest_key_ts: Option<u32>) -> bool {
        let Some(target) = self.skip_to_key else {
            return false;
        };
        if behind_ms(target, ts) > 0 {
            return true;
        }
        if let Some((nv, nk)) = newest_ts.zip(newest_key_ts)
            && nk != target
            && behind_ms(nv, ts) > KEY_SKIP_THRESHOLD_MS
            && behind_ms(nk, ts) > 0
        {
            self.skip_to_key = Some(nk);
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
    /// The same count, kept up to date while a write is still going on (as the kernel takes the
    /// bytes), so a destination whose writes block can be seen to be getting nothing.
    progress: Arc<AtomicU64>,
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
        let mut p = Self { client, s, host, buf: vec![0u8; READ_BUF], sent: 0, progress: Arc::default() };
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
        // Twitch over rtmps:// resets the connection instead of closing it cleanly, for a bad key.
        .map_err(|e| match e {
            PushError::Closed(h) | PushError::Broken(h) => PushError::PublishDropped(h),
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
        // The whole write has the deadline, but the bytes the kernel takes are counted as they go.
        let (s, progress) = (&mut self.s, &self.progress);
        let wrote = timeout(WRITE_DEADLINE, async {
            let mut at = 0;
            while at < bytes.len() {
                let n = s.write(&bytes[at..]).await?;
                if n == 0 {
                    return Err(std::io::ErrorKind::WriteZero.into());
                }
                at += n;
                progress.fetch_add(n as u64, Ordering::Relaxed);
            }
            std::io::Result::Ok(())
        })
        .await
        .map_err(|_| PushError::Stalled(self.host.clone()))?;
        wrote.map_err(|_| PushError::Broken(self.host.clone()))?;
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
mod connect_error_tests {
    use super::*;
    use std::io::{Error, ErrorKind as K};

    #[test]
    fn only_no_route_is_the_network_down_a_name_or_a_refusal_is_the_address() {
        let class = |e: Error| match connect_error(&e, "h") {
            PushError::NetworkDown(_) => "down",
            PushError::Slow(_) => "slow",
            PushError::Unreachable(_) => "unreachable",
            _ => "other",
        };
        assert_eq!(class(K::NetworkUnreachable.into()), "down", "ENETUNREACH");
        assert_eq!(class(K::HostUnreachable.into()), "down", "EHOSTUNREACH");
        assert_eq!(class(K::NetworkDown.into()), "down", "ENETDOWN");
        assert_eq!(class(K::TimedOut.into()), "slow", "ETIMEDOUT / WSAETIMEDOUT");
        // A name that can't be looked up (a typo, or no DNS; on Windows WSAHOST_NOT_FOUND 11001,
        // WSATRY_AGAIN 11002, WSANO_DATA 11004 arrive as raw OS errors) is not proof the network is down.
        assert_eq!(class(Error::other("failed to lookup address information")), "unreachable");
        for code in [11001, 11002, 11004] {
            assert_eq!(class(Error::from_raw_os_error(code)), "unreachable", "{code}");
        }
        assert_eq!(class(K::ConnectionRefused.into()), "unreachable");
        assert_eq!(class(K::ConnectionReset.into()), "unreachable");
    }

    #[test]
    fn a_network_that_is_down_never_counts_as_a_refusal_or_blocks_failover() {
        let e = PushError::NetworkDown("h".into());
        assert!(e.is_network(), "another connection may do better");
        assert!(e.is_connect_failure(), "the backup ingest may answer");
        assert!(!matches!(e, PushError::KeyRefused(_) | PushError::Refused(..)));
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
        assert!(!d.should_drop(0, false, Some(1_900), None, None, 1_000, 10));
        assert!(!d.should_drop(0, false, Some(2_500), None, None, 1_000, MIN_BACKLOG_PACKETS - 1));
        assert!(!d.should_drop(0, false, None, None, None, 1_000, 10));
        // With only one keyframe ahead, keyframes never drop even when > 1800 ms behind live.
        assert!(!d.should_drop(0, true, Some(5_000), Some(1_000), None, 1_000, 50));

        // Past threshold with enough backlog: starts dropping track 0.
        assert!(d.should_drop(0, false, Some(1_901), None, None, 1_000, MIN_BACKLOG_PACKETS));
        // Stays dropping track 0 even once caught up, until track 0 sees a keyframe.
        assert!(d.should_drop(0, false, Some(1_050), None, None, 1_050, 0));
        // Track 1 isn't affected by track 0 alone when track 1 isn't behind.
        assert!(!d.should_drop(1, false, Some(1_050), None, None, 1_050, 0));
        // Track 0 keyframe clears track 0; subsequent in-time inter frames pass.
        assert!(!d.should_drop(0, true, Some(2_000), None, None, 2_000, 0));
        assert!(!d.should_drop(0, false, Some(2_033), None, None, 2_033, 0));

        // Wraps across u32::MAX -> 0 cleanly to trigger drop on track 1.
        assert!(d.should_drop(1, false, Some(100), None, None, u32::MAX - 850, MIN_BACKLOG_PACKETS));
        assert!(d.should_drop(1, false, Some(120), None, None, 120, 0));
        assert!(!d.should_drop(1, true, Some(150), None, None, 150, 0));

        // on_lagged (after RecvError::Lagged) drops every track until its own keyframe.
        d.on_lagged();
        assert!(d.should_drop(0, false, Some(3_000), None, None, 3_000, 0));
        assert!(d.should_drop(2, false, Some(3_000), None, None, 3_000, 0));
        assert!(!d.should_drop(0, true, Some(3_050), None, None, 3_050, 0));
        assert!(!d.should_drop(0, false, Some(3_083), None, None, 3_083, 0));
        assert!(d.should_drop(2, false, Some(3_083), None, None, 3_083, 0));
        assert!(!d.should_drop(2, true, Some(3_100), None, None, 3_100, 0));
    }

    #[test]
    fn video_drop_skips_older_keyframes_and_stale_audio_when_far_behind() {
        let mut d = VideoDrop::default();
        // Single future keyframe (prev_key=0, newest_key=2000) > 1800 ms behind: keeps keyframe-less inter drop and keeps audio continuous.
        assert!(d.should_drop(0, false, Some(2_450), Some(2_000), Some(0), 50, 20));
        assert!(!d.should_drop_audio(60, Some(2_450), Some(2_000)));
        assert!(!d.should_drop(0, true, Some(2_450), Some(2_000), Some(0), 2_000, 5));

        // Multiple older keyframes ahead (1000, 2000, newest_key=3000, newest_ts=3200) and > 1800 ms behind: skips to 3000.
        assert!(d.should_drop(0, false, Some(3_200), Some(3_000), Some(2_000), 100, 20));
        assert!(d.should_drop_audio(110, Some(3_200), Some(3_000)));
        // Intermediate keyframes (1000, 2000) and their audio are dropped.
        assert!(d.should_drop(0, true, Some(3_200), Some(3_000), Some(2_000), 1_000, 15));
        assert!(d.should_drop_audio(1_500, Some(3_200), Some(3_000)));
        assert!(d.should_drop(0, true, Some(3_200), Some(3_000), Some(2_000), 2_000, 10));
        assert!(d.should_drop_audio(2_990, Some(3_200), Some(3_000)));
        // Target keyframe 3000 and its audio resume cleanly; extra track 1 stays dropped until its own keyframe.
        assert!(!d.should_drop_audio(3_000, Some(3_200), Some(3_000)));
        assert!(!d.should_drop(0, true, Some(3_200), Some(3_000), Some(2_000), 3_000, 5));
        assert!(d.should_drop(1, false, Some(3_200), Some(3_000), Some(2_000), 3_020, 4));
        assert!(!d.should_drop(1, true, Some(3_250), Some(3_000), Some(2_000), 3_050, 3));

        // Extending target while skipping: arriving at 3000 when newest_key moved to 5000 (newest_ts=5200) skips 3000 too.
        assert!(d.should_drop(0, true, Some(3_200), Some(3_000), Some(2_000), 1_000, 20));
        assert!(d.should_drop_audio(3_000, Some(5_200), Some(5_000)));
        assert!(d.should_drop(0, true, Some(5_200), Some(5_000), Some(4_000), 3_000, 10));
        assert!(!d.should_drop(0, true, Some(5_200), Some(5_000), Some(4_000), 5_000, 4));

        // Wraps across u32::MAX cleanly and resets on RecvError::Lagged without getting stuck.
        assert!(d.should_drop(0, true, Some(1_100), Some(900), Some(400), u32::MAX - 900, 15));
        assert!(d.should_drop_audio(u32::MAX - 500, Some(1_100), Some(900)));
        d.on_lagged();
        assert!(!d.should_drop_audio(200, Some(1_100), Some(900)));
        assert!(!d.should_drop(0, true, Some(1_100), Some(900), Some(400), 950, 2));
    }
}

#[cfg(test)]
mod give_up_text_tests {
    use super::*;

    #[test]
    fn after_the_network_patience_runs_out_it_says_so_not_that_it_is_waiting_or_the_server_stopped() {
        for host in ["live.twitch.tv", "a.rtmps.youtube.com", "fa723fc1b171.global-contribute.live-video.net"] {
            let d = Destination {
                id: "x".into(),
                target: Target::parse(&format!("rtmp://{host}/app")).unwrap(),
                key: "k".into(),
                retry: Retry::default(),
                extra_audio: false,
                extra_video: false,
                backup: None,
            };
            let (reason, detail) = failure_text(&d, &PushError::NetworkDown(host.into()), false);
            assert!(!reason.contains("waiting") && !reason.contains("stopped taking"), "{reason}");
            assert!(detail.is_some_and(|x| x.contains(host)));
        }
    }
}
