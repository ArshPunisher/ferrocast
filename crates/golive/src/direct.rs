//! The Studio's direct destinations, beside RTMP: SRT and RIST (MPEG-TS, as OBS sends them) and
//! WebRTC by WHIP. Each takes the hub's stream like an RTMP push does: it waits for a stream,
//! connects, sends from the next keyframe, and tries again with the same waits when it drops.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::SinkExt;
use srt_tokio::options::{DataRate, LiveBandwidthMode, Percent};
use tokio::sync::{broadcast, watch};

use crate::hub::{Frame, Hub, Live, Subscription};
use crate::push::{Ended, Health, Link, PushState, Retry, backoff, set_state};
use crate::ts::{PACKET, TsMuxer};

/// How long a connection may take.
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// TS goes out seven packets at a time (1316 bytes: fits any network's packet).
const TS_CHUNK: usize = 7 * PACKET;

/// Which way the stream goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `srt://host:port?…`; the key, if any, is the stream id.
    Srt,
    /// `rist://host:port?…`; the key, if any, is the passphrase.
    Rist,
    /// WebRTC by WHIP: an `https://` endpoint; the key, if any, is its bearer token.
    Whip,
}

impl Kind {
    /// The kind a server address asks for, or None for RTMP (and anything else).
    pub fn of(url: &str) -> Option<Kind> {
        let u = url.trim().to_ascii_lowercase();
        if u.starts_with("srt://") {
            Some(Kind::Srt)
        } else if u.starts_with("rist://") {
            Some(Kind::Rist)
        } else if u.starts_with("https://")
            || ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|p| u.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with([':', '/'])))
        {
            // Plain http only on this computer: a token never crosses the network unencrypted.
            Some(Kind::Whip)
        } else {
            None
        }
    }
}

/// One direct destination. The key is a secret: never logged or shown.
#[derive(Clone)]
pub struct Direct {
    pub id: String,
    pub kind: Kind,
    pub url: String,
    pub key: String,
    pub retry: Retry,
}

impl std::fmt::Debug for Direct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Direct").field("id", &self.id).field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// Why a direct destination failed (words for the page; never the key).
#[derive(Debug, thiserror::Error)]
pub enum DirectError {
    #[error("{0}")]
    Address(String),
    #[error("Couldn't reach {0}.")]
    Unreachable(String),
    #[error("{0} took too long to answer.")]
    Slow(String),
    #[error("The connection to {0} broke.")]
    Broken(String),
    #[error("{0}")]
    Refused(String),
}

/// An `srt://` or `rist://` address: host, port and its `?key=value` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Address {
    pub host: String,
    pub port: u16,
    pub params: Vec<(String, String)>,
}

impl Address {
    pub(crate) fn parse(url: &str) -> Result<Self, DirectError> {
        let bad = |why: &str| DirectError::Address(format!("The address {why}."));
        let rest = url.trim().split_once("://").map(|(_, r)| r).ok_or_else(|| bad("needs srt:// or rist://"))?;
        let (hostport, query) = rest.split_once('?').unwrap_or((rest, ""));
        let hostport = hostport.trim_end_matches('/');
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad("has a port that isn't a number"))?),
            None => return Err(bad("needs a port (host:port)")),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let params: Vec<(String, String)> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (k.to_ascii_lowercase(), percent_decode(v))
            })
            .collect();
        // Only an SRT listener may leave the host out (`srt://:9000?mode=listener`: any address here).
        let listener = params.iter().any(|(k, v)| k == "mode" && v.eq_ignore_ascii_case("listener"));
        if (host.is_empty() && !listener) || port == 0 {
            return Err(bad("needs a host and a port"));
        }
        Ok(Self { host: host.to_string(), port, params })
    }

    pub(crate) fn param(&self, k: &str) -> Option<&str> {
        self.params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }

    fn name(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// `%xx` escapes in a setting.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `d` can work at all (its address reads, its settings make sense): checked before it
/// starts, so a destination that never could isn't retried for the whole stream.
pub fn check(d: &Direct) -> Result<(), DirectError> {
    match d.kind {
        Kind::Srt => {
            let a = Address::parse(&d.url)?;
            if let Some(pass) = a.param("passphrase").filter(|p| !p.is_empty())
                && !(10..=79).contains(&pass.chars().count())
            {
                return Err(DirectError::Address("An SRT passphrase is 10 to 79 letters long.".into()));
            }
            if SrtMode::of(&a)? == SrtMode::Listener {
                listen_addr(&a)?;
            }
            srt_bandwidth(&a)?;
            Ok(())
        }
        Kind::Rist => crate::rist::check(d, &Address::parse(&d.url)?),
        Kind::Whip => whip_address(&d.url).map(|_| ()),
    }
}

/// A WHIP endpoint: https, or plain http only to this computer (the token never crosses the
/// network unencrypted). Read as a URL, so a host hidden after `user@` is the host it is.
pub(crate) fn whip_address(url: &str) -> Result<reqwest::Url, DirectError> {
    let u = reqwest::Url::parse(url.trim())
        .map_err(|_| DirectError::Address("The WHIP address isn't a web address.".into()))?;
    let here = u.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost")
            || h.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if u.scheme() == "https" || (u.scheme() == "http" && here) {
        Ok(u)
    } else {
        Err(DirectError::Address("A WHIP address is https:// (plain http:// only on this computer).".into()))
    }
}

/// Sends the hub's streams to `d` until `stop` (as `push::run` does for RTMP).
pub async fn run(d: Direct, hub: Arc<Hub>, health: watch::Sender<Health>, mut stop: watch::Receiver<bool>) {
    let mut live = hub.live();
    health.send_replace(Health {
        state: PushState::Waiting,
        kbps: 0,
        dropped: 0,
        sent: 0,
        link: None,
        since_ms: crate::push::now_ms(),
    });
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
        loop {
            set_state(&health, PushState::Connecting);
            let outcome = tokio::select! {
                _ = stop.wait_for(|s| *s) => Ok(Ended::Stopped),
                r = session(&d, &hub, &health, stream) => r,
            };
            match outcome {
                Ok(Ended::Stopped) => break 'streams,
                Ok(Ended::StreamEnded) => break,
                Err(err) => {
                    if !still_on(&live.borrow()) {
                        break;
                    }
                    if health.borrow().state == PushState::Live {
                        attempt = 0;
                    }
                    attempt = attempt.saturating_add(1);
                    let wait = backoff(attempt, &d.retry);
                    tracing::info!(dest = %d.id, attempt, error = %err, "direct destination failed, retrying");
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
    }
    set_state(&health, PushState::Stopped);
}

async fn session(d: &Direct, hub: &Hub, health: &watch::Sender<Health>, stream: u64) -> Result<Ended, DirectError> {
    match d.kind {
        Kind::Srt => srt_session(d, hub, health, stream).await,
        Kind::Rist => crate::rist::session(d, hub, health, stream).await,
        Kind::Whip => crate::whip::session(d, hub, health, stream).await,
    }
}

/// Where TS chunks go (an SRT socket, RIST's RTP).
pub(crate) trait TsSink {
    async fn send(&mut self, chunk: Bytes) -> Result<(), DirectError>;
}

/// The hub's stream as TS, chunk by chunk, to `sink` (headers first, then from a keyframe).
/// Ends when the stream does; Err when the sink fails.
pub(crate) async fn feed_ts(
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
    sink: &mut impl TsSink,
) -> Result<Ended, DirectError> {
    let Subscription { live, headers, mut rx } = hub.subscribe();
    if !live.on || live.stream != stream {
        return Ok(Ended::StreamEnded);
    }
    let mut mux = TsMuxer::new();
    if let Some(v) = &headers.video {
        mux.video(0, v);
    }
    if let Some(a) = &headers.audio {
        mux.audio(0, a);
    }
    set_state(health, PushState::Live);
    let mut pending = Vec::with_capacity(TS_CHUNK * 4);
    let (mut window, mut sent) = (Instant::now(), 0u64);
    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                health.send_modify(|h| h.dropped += n);
                mux.resync();
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(Ended::StreamEnded),
        };
        match frame {
            Frame::End => return Ok(Ended::StreamEnded),
            Frame::Metadata(_) => {}
            Frame::Video { ts, data, .. } => pending.extend(mux.video(ts, &data)),
            Frame::Audio { ts, data } => pending.extend(mux.audio(ts, &data)),
        }
        while pending.len() >= TS_CHUNK {
            let chunk = Bytes::copy_from_slice(&pending[..TS_CHUNK]);
            pending.drain(..TS_CHUNK);
            sent += chunk.len() as u64;
            sink.send(chunk).await?;
        }
        if window.elapsed() >= Duration::from_secs(1) {
            let kbps = (sent * 8 / 1000) as u32;
            health.send_modify(|h| {
                h.kbps = kbps;
                h.sent += sent;
            });
            (window, sent) = (Instant::now(), 0);
        }
    }
}

/// How an SRT connection is made, as libsrt's `mode=` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SrtMode {
    /// Connects to the server (the default, as OBS does).
    Caller,
    /// Waits here for the receiver to connect (`srt://:port?mode=listener`).
    Listener,
    /// Both sides connect to each other at once (`localport=` here, the port there by default).
    Rendezvous,
}

impl SrtMode {
    fn of(a: &Address) -> Result<Self, DirectError> {
        match a.param("mode").map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("caller") => Ok(Self::Caller),
            Some("listener") => Ok(Self::Listener),
            Some("rendezvous") => Ok(Self::Rendezvous),
            Some(_) => Err(DirectError::Address("An SRT mode is caller, listener or rendezvous.".into())),
        }
    }
}

/// Where an SRT listener waits: an IP address here (none: all of them), and the port.
fn listen_addr(a: &Address) -> Result<std::net::SocketAddr, DirectError> {
    let ip = if a.host.is_empty() {
        std::net::IpAddr::from([0, 0, 0, 0])
    } else {
        a.host.parse().map_err(|_| DirectError::Address("An SRT listener's host is an IP address here.".into()))?
    };
    Ok(std::net::SocketAddr::new(ip, a.port))
}

/// SRT's sending limit, as libsrt's addresses write it (bytes per second): `maxbw=` a cap, or
/// `inputbw=` with `oheadbw=` (percent, 5 to 100) on top; None leaves SRT's own (input plus 25%).
fn srt_bandwidth(a: &Address) -> Result<Option<LiveBandwidthMode>, DirectError> {
    let num = |k: &str| -> Result<Option<i64>, DirectError> {
        a.param(k)
            .filter(|v| !v.is_empty())
            .map(|v| v.parse::<i64>().map_err(|_| DirectError::Address(format!("SRT's {k} is a number."))))
            .transpose()
    };
    let overhead = num("oheadbw")?.unwrap_or(25);
    if !(5..=100).contains(&overhead) {
        return Err(DirectError::Address("SRT's oheadbw is 5 to 100 percent.".into()));
    }
    Ok(match (num("maxbw")?, num("inputbw")?) {
        // libsrt: -1 unlimited, 0 worked out from the input.
        (Some(max), _) if max > 0 => Some(LiveBandwidthMode::Max(DataRate(max as u64))),
        (Some(-1), _) => Some(LiveBandwidthMode::Unlimited),
        (_, Some(input)) if input > 0 => {
            Some(LiveBandwidthMode::Input { rate: DataRate(input as u64), overhead: Percent(overhead as u64) })
        }
        _ => None,
    })
}

/// SRT as a caller (as OBS's SRT output connects), a listener or rendezvous (`mode=`): the key,
/// or `streamid=`, names the stream; `passphrase=` (10 to 79 letters) encrypts it, `pbkeylen=`
/// 16, 24 or 32; `latency=` in microseconds, as libsrt's addresses write it (120 ms when not
/// said); `maxbw=`, `inputbw=` and `oheadbw=` cap the rate.
async fn srt_session(d: &Direct, hub: &Hub, health: &watch::Sender<Health>, stream: u64) -> Result<Ended, DirectError> {
    let a = Address::parse(&d.url)?;
    let name = a.name();
    let mut sock = srt_connect(d, &a, None).await?;
    // A delay the streamer didn't set follows the network: SRT wants at least four round
    // trips to resend in time, so a far server gets a longer one (once, on connecting).
    if a.param("latency").is_none()
        && let Some(want) = srt_latency_for(sock.settings().rtt)
    {
        tracing::info!(dest = %d.id, latency_ms = want.as_millis() as u64, "SRT delay follows the network");
        let _ = sock.close().await;
        sock = srt_connect(d, &a, Some(want)).await?;
    }
    let stats = sock.statistics().clone();
    let mut sink = SrtSink { sock, name };
    let out = tokio::select! {
        out = feed_ts(hub, health, stream, &mut sink) => out,
        () = srt_link(stats, health) => Ok(Ended::StreamEnded),
    };
    let _ = sink.sock.close().await;
    out
}

/// SRT's delay when the streamer didn't set one.
const SRT_LATENCY: Duration = Duration::from_millis(120);

/// The delay a round trip asks for, when more than the default: four round trips (as
/// Haivision advises), to the next 10 ms, at most 8 s.
pub(crate) fn srt_latency_for(rtt: Duration) -> Option<Duration> {
    let want = (rtt * 4).as_millis().div_ceil(10) as u64 * 10;
    (Duration::from_millis(want) > SRT_LATENCY).then(|| Duration::from_millis(want.min(8_000)))
}

/// An SRT connection made as the address says (`latency` instead of its own, if given).
async fn srt_connect(d: &Direct, a: &Address, latency: Option<Duration>) -> Result<srt_tokio::SrtSocket, DirectError> {
    let name = a.name();
    let mode = SrtMode::of(a)?;
    let stream_id = Some(d.key.trim()).filter(|k| !k.is_empty()).or_else(|| a.param("streamid"));
    let mut b = srt_tokio::SrtSocket::builder();
    let latency = latency.unwrap_or_else(|| {
        a.param("latency")
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(SRT_LATENCY, |us| Duration::from_micros(us.clamp(20_000, 8_000_000)))
    });
    b = b.latency(latency);
    if let Some(bw) = srt_bandwidth(a)? {
        b = b.bandwidth(bw);
    }
    if let Some(pass) = a.param("passphrase").filter(|p| !p.is_empty()) {
        if !(10..=79).contains(&pass.chars().count()) {
            return Err(DirectError::Address("An SRT passphrase is 10 to 79 letters long.".into()));
        }
        let size =
            a.param("pbkeylen").and_then(|v| v.parse::<u16>().ok()).filter(|n| matches!(n, 16 | 24 | 32)).unwrap_or(16);
        b = b.encryption(size, pass);
    }
    match mode {
        SrtMode::Listener => {
            let here = listen_addr(a)?;
            // No deadline: it waits for the receiver as long as the stream runs.
            b.listen_on(here).await.map_err(|e| DirectError::Refused(format!("Couldn't listen on {name} ({e}).")))
        }
        SrtMode::Caller | SrtMode::Rendezvous => {
            let addr = tokio::net::lookup_host((a.host.as_str(), a.port))
                .await
                .ok()
                .and_then(|mut it| it.next())
                .ok_or_else(|| DirectError::Unreachable(name.clone()))?;
            let local = a.param("localport").and_then(|p| p.parse::<u16>().ok()).unwrap_or(a.port);
            let connect = async move {
                if mode == SrtMode::Caller {
                    b.call(addr, stream_id).await
                } else {
                    b.local_port(local).rendezvous(addr).await
                }
            };
            let secret = a.param("passphrase").is_some_and(|p| !p.is_empty());
            tokio::time::timeout(CONNECT_DEADLINE, connect)
                .await
                .map_err(|_| DirectError::Slow(name.clone()))?
                .map_err(|e| srt_refusal(&name, &e.to_string(), secret))
        }
    }
}

/// Why SRT didn't connect, in words: nothing there, a refusal (the passphrase or stream id, when
/// there's one to check), or what the server said.
fn srt_refusal(name: &str, why: &str, secret: bool) -> DirectError {
    let why = why.trim();
    if why.is_empty() || (why.contains("refused") && !why.contains("ejected")) {
        DirectError::Unreachable(format!("{name} (nothing answered there)"))
    } else if why.contains("ejected") && secret {
        DirectError::Refused(format!("{name} refused the connection: check the passphrase and the stream id."))
    } else if why.contains("ejected") {
        DirectError::Refused(format!(
            "{name} refused the connection: check the stream id (the key), and whether it wants a passphrase."
        ))
    } else {
        DirectError::Refused(format!("{name} refused the SRT connection ({why})."))
    }
}

/// Connects to an SRT server and lets go (a caller or rendezvous): the round trip, and the
/// delay it asks for when the address doesn't set one. A listener can't be tried from here.
pub(crate) async fn srt_reach(d: &Direct) -> Result<(Duration, Option<Duration>), DirectError> {
    let a = Address::parse(&d.url)?;
    if SrtMode::of(&a)? == SrtMode::Listener {
        return Err(DirectError::Address(format!(
            "This waits on port {} for your receiver to connect, so there's nothing to try from here.",
            a.port
        )));
    }
    let mut sock = srt_connect(d, &a, None).await?;
    let rtt = sock.settings().rtt;
    let _ = sock.close().await;
    // A longer delay than the address sets (or SRT's own), when the network asks for it.
    let set = a.param("latency").and_then(|v| v.parse::<u64>().ok()).map_or(SRT_LATENCY, Duration::from_micros);
    Ok((rtt, srt_latency_for(rtt).filter(|want| *want > set)))
}

/// The SRT connection's ping, loss and resends, from its statistics (once a second), until they
/// end with it.
async fn srt_link(
    mut stats: impl futures::Stream<Item = srt_tokio::SocketStatistics> + Unpin,
    health: &watch::Sender<Health>,
) {
    use futures::StreamExt;
    let mut last: Option<(u64, u64, u64)> = None;
    while let Some(s) = stats.next().await {
        let now = (s.tx_data, s.tx_loss_data, s.tx_retransmit_data);
        if let Some((data, loss, resent)) = last {
            let sent = now.0.saturating_sub(data).max(1) as f32;
            let link = Link {
                rtt_ms: s.tx_average_rtt.as_millis() as u32,
                lost_pct: now.1.saturating_sub(loss) as f32 * 100.0 / sent,
                resent_pct: now.2.saturating_sub(resent) as f32 * 100.0 / sent,
            };
            health.send_modify(|h| h.link = Some(link));
        }
        last = Some(now);
    }
    // Ended: the sending side says why.
    std::future::pending::<()>().await;
}

struct SrtSink {
    sock: srt_tokio::SrtSocket,
    name: String,
}

impl TsSink for SrtSink {
    async fn send(&mut self, chunk: Bytes) -> Result<(), DirectError> {
        self.sock.send((Instant::now(), chunk)).await.map_err(|_| DirectError::Broken(self.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_say_their_kind() {
        assert_eq!(Kind::of("srt://a:1"), Some(Kind::Srt));
        assert_eq!(Kind::of(" RIST://a:1"), Some(Kind::Rist));
        assert_eq!(Kind::of("https://whip.example/x"), Some(Kind::Whip));
        assert_eq!(Kind::of("http://localhost:8889/whip"), Some(Kind::Whip));
        assert_eq!(Kind::of("http://example.com/whip"), None, "plain http only on this computer");
        assert_eq!(Kind::of("rtmp://a/live"), None);
    }

    #[test]
    fn a_whip_token_never_goes_unencrypted_off_this_computer() {
        assert!(whip_address("https://whip.example/x").is_ok());
        assert!(whip_address("http://127.0.0.1:8889/whip").is_ok());
        assert!(whip_address("http://[::1]/whip").is_ok());
        assert!(whip_address("http://localhost/whip").is_ok());
        // A remote host hidden behind "localhost:80@".
        assert!(whip_address("http://localhost:80@evil.example/whip").is_err());
        assert!(whip_address("http://example.com/whip").is_err());
        assert!(whip_address("ftp://localhost/x").is_err());
    }

    #[test]
    fn srt_and_rist_addresses_are_read() {
        let a = Address::parse("srt://ingest.example:9000?streamid=live%2Fabc&latency=200000")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((a.host.as_str(), a.port), ("ingest.example", 9000));
        assert_eq!(a.param("streamid"), Some("live/abc"));
        assert_eq!(a.param("latency"), Some("200000"));
        let v6 = Address::parse("rist://[::1]:5000").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 5000));
        assert!(Address::parse("srt://nohost").is_err());
        assert!(Address::parse("srt://h:port").is_err());
    }

    #[test]
    fn the_key_never_shows_in_debug() {
        let d = Direct {
            id: "x".into(),
            kind: Kind::Srt,
            url: "srt://a:1".into(),
            key: "s3cret".into(),
            retry: Retry::default(),
        };
        assert!(!format!("{d:?}").contains("s3cret"));
    }

    /// The hub's stream reaches an SRT listener as TS that FFmpeg reads.
    #[tokio::test]
    async fn srt_reaches_a_listener() {
        use futures::TryStreamExt;
        let Some(tags) = crate::ts::tests::sample_tags() else { return };
        let port =
            std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(24001);
        let listener = tokio::spawn(async move {
            let mut rx = srt_tokio::SrtSocket::builder().listen_on(port).await?;
            let mut got = Vec::new();
            while let Some((_, b)) = rx.try_next().await? {
                got.extend_from_slice(&b);
            }
            Ok::<_, std::io::Error>(got)
        });
        let hub = Hub::new();
        hub.start();
        let (health, h) =
            watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, link: None, since_ms: 0 });
        let (stop_tx, stop) = watch::channel(false);
        let d = Direct {
            id: "t".into(),
            kind: Kind::Srt,
            url: format!("srt://127.0.0.1:{port}"),
            key: String::new(),
            retry: Retry::default(),
        };
        let sender = tokio::spawn(run(d, hub.clone(), health, stop));
        let mut h2 = h.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), h2.wait_for(|h| h.state == PushState::Live)).await;
        for (kind, ms, body) in tags {
            match kind {
                9 => hub.video(ms, body.into()),
                8 => hub.audio(ms, body.into()),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        hub.end();
        let got = tokio::time::timeout(Duration::from_secs(10), listener).await;
        let _ = stop_tx.send(true);
        let _ = sender.await;
        let got = match got {
            Ok(Ok(Ok(g))) => g,
            other => panic!("the listener got nothing: {other:?}"),
        };
        assert!(got.len() > 10 * PACKET && got.len() % PACKET == 0, "{} bytes", got.len());
        if let Some(p) = crate::ts::tests::probe(&got) {
            assert!(p.starts_with("h264,") && p.contains("aac,"), "{p}");
        }
    }

    #[test]
    fn srt_delay_follows_the_round_trip() {
        assert_eq!(srt_latency_for(Duration::from_millis(20)), None, "near: SRT's own 120 ms");
        assert_eq!(srt_latency_for(Duration::from_millis(31)), Some(Duration::from_millis(130)));
        assert_eq!(srt_latency_for(Duration::from_millis(250)), Some(Duration::from_millis(1000)));
        assert_eq!(srt_latency_for(Duration::from_secs(5)), Some(Duration::from_secs(8)), "at most 8 s");
    }

    #[test]
    fn srt_refusals_say_what_to_check() {
        let say = |why: &str, secret: bool| srt_refusal("h:1", why, secret).to_string();
        assert!(say("", false).contains("nothing answered"));
        assert!(say("Connection refused (os error 61)", false).contains("nothing answered"));
        assert!(say("Remote rejected connection: peer", true).contains("check the passphrase"));
        assert!(say("Remote rejected connection: peer", false).contains("wants a passphrase"));
    }

    #[test]
    fn srt_modes_and_limits_are_read() {
        let srt = |url: &str| Direct {
            id: "t".into(),
            kind: Kind::Srt,
            url: url.into(),
            key: String::new(),
            retry: Retry::default(),
        };
        assert!(check(&srt("srt://:9000?mode=listener")).is_ok(), "a listener may leave the host out");
        assert!(check(&srt("srt://:9000")).is_err(), "a caller needs a host");
        assert!(check(&srt("srt://a:9000?mode=rendezvous&localport=9001")).is_ok());
        assert!(check(&srt("srt://a:9000?mode=sideways")).is_err());
        assert!(check(&srt("srt://localhost:9000?mode=listener")).is_err(), "a listener's host is an IP");
        assert_eq!(
            listen_addr(&Address::parse("srt://[::1]:9000?mode=listener").unwrap()).unwrap().to_string(),
            "[::1]:9000"
        );
        assert!(check(&srt("srt://a:9000?maxbw=lots")).is_err());
        assert!(check(&srt("srt://a:9000?inputbw=500000&oheadbw=200")).is_err());
        let bw = |url: &str| srt_bandwidth(&Address::parse(url).unwrap()).unwrap();
        assert_eq!(bw("srt://a:1?maxbw=1000000"), Some(LiveBandwidthMode::Max(DataRate(1_000_000))));
        assert_eq!(
            bw("srt://a:1?inputbw=500000&oheadbw=50"),
            Some(LiveBandwidthMode::Input { rate: DataRate(500_000), overhead: Percent(50) })
        );
        assert_eq!(bw("srt://a:1?maxbw=-1"), Some(LiveBandwidthMode::Unlimited));
        assert_eq!(bw("srt://a:1?maxbw=-1&inputbw=500000"), Some(LiveBandwidthMode::Unlimited), "unlimited wins");
        assert_eq!(bw("srt://a:1"), None);
    }

    /// Sends the sample stream through `url` while `receive` takes it on the other side → the
    /// bytes it got.
    async fn srt_through(url: String, receive: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>) -> Vec<u8> {
        let Some(tags) = crate::ts::tests::sample_tags() else { return Vec::new() };
        let hub = Hub::new();
        hub.start();
        let (health, h) =
            watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, link: None, since_ms: 0 });
        let (stop_tx, stop) = watch::channel(false);
        let d = Direct { id: "t".into(), kind: Kind::Srt, url, key: String::new(), retry: Retry::default() };
        let sender = tokio::spawn(run(d, hub.clone(), health, stop));
        let mut h2 = h.clone();
        let _ = tokio::time::timeout(Duration::from_secs(8), h2.wait_for(|h| h.state == PushState::Live)).await;
        for (kind, ms, body) in tags {
            match kind {
                9 => hub.video(ms, body.into()),
                8 => hub.audio(ms, body.into()),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        hub.end();
        let got = tokio::time::timeout(Duration::from_secs(10), receive).await;
        let _ = stop_tx.send(true);
        let _ = sender.await;
        match got {
            Ok(Ok(Ok(g))) => g,
            other => panic!("the receiver got nothing: {other:?}"),
        }
    }

    async fn read_all(mut rx: srt_tokio::SrtSocket) -> std::io::Result<Vec<u8>> {
        use futures::TryStreamExt;
        let mut got = Vec::new();
        while let Some((_, b)) = rx.try_next().await? {
            got.extend_from_slice(&b);
        }
        Ok(got)
    }

    fn free_udp_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(24011)
    }

    /// As a listener, the stream waits here and goes to the receiver that connects.
    #[tokio::test]
    async fn srt_listens_for_a_receiver() {
        if crate::ts::tests::sample_tags().is_none() {
            return;
        }
        let port = free_udp_port();
        let receive = tokio::spawn(async move {
            // Calls until the listener is up.
            for _ in 0..50 {
                if let Ok(Ok(rx)) = tokio::time::timeout(
                    Duration::from_secs(1),
                    srt_tokio::SrtSocket::builder().call(format!("127.0.0.1:{port}").as_str(), None),
                )
                .await
                {
                    return read_all(rx).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::io::Error::other("never connected"))
        });
        let got = srt_through(format!("srt://:{port}?mode=listener"), receive).await;
        assert!(got.len() > 10 * PACKET && got.len() % PACKET == 0, "{} bytes", got.len());
    }

    /// In rendezvous, both sides connect to each other.
    #[tokio::test]
    async fn srt_meets_in_rendezvous() {
        if crate::ts::tests::sample_tags().is_none() {
            return;
        }
        let (ours, theirs) = (free_udp_port(), free_udp_port());
        let receive = tokio::spawn(async move {
            let rx = srt_tokio::SrtSocket::builder()
                .local_port(theirs)
                .rendezvous(format!("127.0.0.1:{ours}").as_str())
                .await?;
            read_all(rx).await
        });
        let got =
            srt_through(format!("srt://127.0.0.1:{theirs}?mode=rendezvous&localport={ours}&maxbw=4000000"), receive)
                .await;
        assert!(got.len() > 10 * PACKET && got.len() % PACKET == 0, "{} bytes", got.len());
    }
}
