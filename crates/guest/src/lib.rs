//! Guest cameras: a guest opens a link in their browser, and their camera and mic come into the
//! Studio. The browser sends them over WebRTC (VP8 video, Opus sound); this crate answers its offer
//! and receives them with str0m, then hands the RTP packets, untouched but for their payload type,
//! to the engine's own media source on this computer (`127.0.0.1`), described by an SDP file that
//! libobs's FFmpeg source opens. FFmpeg puts the packets back in order and decodes them.
//!
//! The offer and answer travel some other way (the relay, or the app's own page on this network):
//! this crate only sees their text.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::KeyframeRequestKind;
use str0m::net::{Protocol, Receive};
use str0m::rtp::Ssrc;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

/// The payload types the SDP file gives FFmpeg (the browser's own are mapped onto them).
const VIDEO_PT: u8 = 96;
const AUDIO_PT: u8 = 111;

/// Where Chrome starts and stops its video bitrate (kbps): from the first second sharp enough to
/// stream, instead of climbing from 300 for half a minute; its own estimate still lowers it on a
/// poor network.
const START_KBPS: u32 = 2500;
const MAX_KBPS: u32 = 6000;

/// How long a guest may stay unreachable before they count as gone.
const GONE_AFTER: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum GuestError {
    #[error("the guest's browser sent an offer that isn't valid: {0}")]
    BadOffer(String),
    #[error("couldn't answer the guest's browser: {0}")]
    Answer(String),
    #[error("couldn't open a network port for the guest: {0}")]
    Network(#[from] std::io::Error),
}

/// What's known of a guest's connection now.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    /// The browser and this computer reached each other.
    pub connected: bool,
    /// Pictures and sound have arrived.
    pub video: bool,
    pub audio: bool,
    /// Bytes of media received so far.
    pub bytes: u64,
    /// Media arrived in the last few seconds (a guest who left stops at once; the connection
    /// itself takes longer to notice).
    pub receiving: bool,
    /// The connection ended (the guest left, or was unreachable too long).
    pub ended: bool,
}

#[derive(Debug, Default)]
struct Shared {
    connected: AtomicBool,
    video: AtomicBool,
    audio: AtomicBool,
    ended: AtomicBool,
    bytes: AtomicU64,
    /// When media last arrived: milliseconds after `born`.
    last_ms: AtomicU64,
    stop: AtomicBool,
}

/// How recent media must be for a guest to count as receiving.
const RECEIVING_WITHIN_MS: u64 = 1500;

/// One guest's connection. Dropping it hangs up.
#[derive(Debug)]
pub struct Guest {
    shared: Arc<Shared>,
    born: Instant,
    sdp: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl Guest {
    /// Answers a browser's offer (its JSON, `{"type":"offer","sdp":…}`) → the answer to send back
    /// (the same shape) and the connection, receiving on its own thread. `sdp` is where the file
    /// for the FFmpeg source is written; `host` the address this computer is reached on (None:
    /// the one its traffic leaves by).
    pub fn accept(offer: &str, sdp: &Path, host: Option<IpAddr>) -> Result<(String, Guest), GuestError> {
        let offer: SdpOffer = serde_json::from_str(offer).map_err(|e| GuestError::BadOffer(e.to_string()))?;
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_vp8(true)
            .enable_opus(true, false)
            .set_rtp_mode(true)
            .build(Instant::now());
        // Bound to the one address it's offered on: str0m matches what arrives to that address.
        let host = host.unwrap_or_else(local_address);
        let socket = UdpSocket::bind((host, 0))?;
        let base = socket.local_addr()?;
        let c = Candidate::host(base, "udp").map_err(|e| GuestError::Answer(e.to_string()))?;
        rtc.add_local_candidate(c);
        // A guest on another network reaches this computer at its router's public address.
        if let Some(public) = public_address(&socket)
            && public != base
            && let Ok(c) = Candidate::server_reflexive(public, base, "udp")
        {
            rtc.add_local_candidate(c);
        }
        let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| GuestError::Answer(e.to_string()))?;
        let mut answer = serde_json::to_value(&answer).map_err(|e| GuestError::Answer(e.to_string()))?;
        if let Some(sdp) = answer.get("sdp").and_then(|s| s.as_str()).map(with_bitrates) {
            answer["sdp"] = sdp.into();
        }
        let answer = answer.to_string();

        // The sending socket first, then two pairs held open while they're picked, so none of the
        // six ports is the same.
        let out = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let (video, video_port) = free_pair()?;
        let (audio, audio_port) = free_pair()?;
        // Let go just before FFmpeg takes them.
        drop((video, audio));
        write_sdp(sdp, video_port, audio_port)?;
        let shared = Arc::new(Shared::default());
        let born = Instant::now();
        let s = shared.clone();
        let to =
            (SocketAddr::from((Ipv4Addr::LOCALHOST, video_port)), SocketAddr::from((Ipv4Addr::LOCALHOST, audio_port)));
        let thread = std::thread::Builder::new().name("ferrocast-guest".into()).spawn(move || {
            if let Err(e) = run(rtc, &socket, &out, to, &s, born) {
                tracing::info!(error = %e, "a guest's connection ended");
            }
            s.ended.store(true, Ordering::Release);
            s.connected.store(false, Ordering::Release);
        })?;
        Ok((answer, Guest { shared, born, sdp: sdp.to_path_buf(), thread: Some(thread) }))
    }

    pub fn status(&self) -> Status {
        let s = &self.shared;
        Status {
            connected: s.connected.load(Ordering::Acquire),
            video: s.video.load(Ordering::Acquire),
            audio: s.audio.load(Ordering::Acquire),
            bytes: s.bytes.load(Ordering::Relaxed),
            receiving: {
                let last = s.last_ms.load(Ordering::Relaxed);
                last > 0 && (self.born.elapsed().as_millis() as u64).saturating_sub(last) < RECEIVING_WITHIN_MS
            },
            ended: s.ended.load(Ordering::Acquire),
        }
    }

    /// The SDP file the FFmpeg source opens (`input_format` sdp).
    pub fn sdp(&self) -> &Path {
        &self.sdp
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The settings of libobs's FFmpeg source (`ffmpeg_source`) that play a guest's SDP file.
pub fn source_settings(sdp: &Path) -> serde_json::Value {
    serde_json::json!({
        "is_local_file": false,
        "input": sdp.display().to_string(),
        "input_format": "sdp",
        // FFmpeg only opens RTP that an SDP file names when it's told it may.
        // No start-up buffering (it held seconds of sound back while waiting for a keyframe), and
        // a second at most to learn the streams.
        "ffmpeg_options": "protocol_whitelist=file,udp,rtp reorder_queue_size=64 fflags=nobuffer analyzeduration=1000000",
        "buffering_mb": 0,
        "restart_on_activate": false,
        "close_when_inactive": false,
        "hw_decode": true,
        "clear_on_media_end": false,
        "reconnect_delay_sec": 1,
    })
}

/// The SDP file: the guest's video on one port, their sound on another, both here.
fn sdp_text(video_port: u16, audio_port: u16) -> String {
    format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Ferrocast guest\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=video {video_port} RTP/AVP {VIDEO_PT}\r\na=rtpmap:{VIDEO_PT} VP8/90000\r\n\
         m=audio {audio_port} RTP/AVP {AUDIO_PT}\r\na=rtpmap:{AUDIO_PT} opus/48000/2\r\n"
    )
}

fn write_sdp(path: &Path, video_port: u16, audio_port: u16) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, sdp_text(video_port, audio_port))
}

/// The answer's SDP with Chrome's bitrate hints on its VP8 (`x-google-start-bitrate`, read from
/// the far side's description): an fmtp line added, or the hints added to the one there.
fn with_bitrates(sdp: &str) -> String {
    let hints = format!("x-google-start-bitrate={START_KBPS};x-google-max-bitrate={MAX_KBPS}");
    let pts: Vec<&str> = sdp
        .lines()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter_map(|r| r.split_once(' ').filter(|(_, c)| c.eq_ignore_ascii_case("VP8/90000")).map(|(pt, _)| pt))
        .collect();
    let mut out = String::with_capacity(sdp.len() + 128);
    for line in sdp.split_inclusive("\r\n") {
        let body = line.trim_end_matches("\r\n");
        let fmtp_of = body.strip_prefix("a=fmtp:").and_then(|f| f.split_once(' ')).map(|(pt, _)| pt);
        if fmtp_of.is_some_and(|pt| pts.contains(&pt)) {
            out.push_str(&format!("{body};{hints}\r\n"));
            continue;
        }
        out.push_str(line);
        if let Some((pt, _)) = body.strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' '))
            && pts.contains(&pt)
            && !sdp.contains(&format!("a=fmtp:{pt} "))
        {
            out.push_str(&format!("a=fmtp:{pt} {hints}\r\n"));
        }
    }
    out
}

/// An even port here whose next one is free too (FFmpeg listens on both: RTP and RTCP), with the
/// two held until the caller lets go.
fn free_pair() -> std::io::Result<((UdpSocket, UdpSocket), u16)> {
    for _ in 0..64 {
        let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let p = a.local_addr()?.port();
        if p % 2 != 0 || p == u16::MAX {
            continue;
        }
        if let Ok(b) = UdpSocket::bind((Ipv4Addr::LOCALHOST, p + 1)) {
            return Ok(((a, b), p));
        }
    }
    Err(std::io::Error::other("no two free ports side by side"))
}

/// This computer's address on its network (the one its traffic leaves by), found without sending
/// anything; 127.0.0.1 without a network.
pub fn local_address() -> IpAddr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Public STUN servers that tell a computer its address as the internet sees it.
const STUN: &[&str] = &["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

/// This socket's address as the internet sees it (its router's public address and port), asked
/// of a public STUN server; None without a network or an answer within a second and a half.
fn public_address(socket: &UdpSocket) -> Option<SocketAddr> {
    use std::hash::{BuildHasher, Hasher};
    use std::net::ToSocketAddrs;
    let servers: Vec<SocketAddr> =
        STUN.iter().filter_map(|s| s.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)).collect();
    // A transaction id no one can guess in advance (to match the answer to this request).
    let mut id = [0u8; 12];
    for chunk in id.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(Instant::now().elapsed().as_nanos());
        chunk.copy_from_slice(&h.finish().to_be_bytes()[..chunk.len()]);
    }
    let mut req = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
    req.extend_from_slice(&id);
    let deadline = Instant::now() + Duration::from_millis(1500);
    for s in &servers {
        let _ = socket.send_to(&req, s);
    }
    let mut buf = [0u8; 512];
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        socket.set_read_timeout(Some(left)).ok()?;
        let Ok((n, from)) = socket.recv_from(&mut buf) else { break };
        if servers.contains(&from)
            && let Some(a) = stun_mapped(&buf[..n], &id)
        {
            return Some(a);
        }
    }
    None
}

/// The address in a STUN Binding success for transaction `id` (XOR-MAPPED-ADDRESS, else
/// MAPPED-ADDRESS; IPv4).
fn stun_mapped(b: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if b.len() < 20 || b[0..2] != [0x01, 0x01] || b[4..8] != [0x21, 0x12, 0xA4, 0x42] || b[8..20] != id[..] {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([b[2], b[3]])).min(b.len() - 20);
    let mut at = 20;
    let mut plain = None;
    while at + 4 <= 20 + len {
        let kind = u16::from_be_bytes([b[at], b[at + 1]]);
        let n = usize::from(u16::from_be_bytes([b[at + 2], b[at + 3]]));
        let v = b.get(at + 4..at + 4 + n)?;
        if v.len() >= 8 && v[1] == 0x01 {
            let port = u16::from_be_bytes([v[2], v[3]]);
            let ip = [v[4], v[5], v[6], v[7]];
            match kind {
                0x0020 => {
                    let ip = Ipv4Addr::new(ip[0] ^ 0x21, ip[1] ^ 0x12, ip[2] ^ 0xA4, ip[3] ^ 0x42);
                    return Some(SocketAddr::from((ip, port ^ 0x2112)));
                }
                0x0001 => plain = Some(SocketAddr::from((Ipv4Addr::from(ip), port))),
                _ => {}
            }
        }
        // Attributes are padded to four bytes.
        at += 4 + n.div_ceil(4) * 4;
    }
    plain
}

/// An RTP packet as FFmpeg reads it: a plain 12-byte header (no extensions), then the payload.
fn rtp_bytes(pt: u8, marker: bool, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + payload.len());
    b.push(0x80);
    b.push((u8::from(marker) << 7) | (pt & 0x7f));
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&ts.to_be_bytes());
    b.extend_from_slice(&ssrc.to_be_bytes());
    b.extend_from_slice(payload);
    b
}

fn run(
    mut rtc: Rtc,
    socket: &UdpSocket,
    out: &UdpSocket,
    to: (SocketAddr, SocketAddr),
    s: &Shared,
    born: Instant,
) -> Result<(), str0m::RtcError> {
    let local = socket.local_addr()?;
    let mut buf = vec![0u8; 2000];
    let mut video_ssrc: Option<Ssrc> = None;
    // Keyframes asked for: every second while the source starts (it may open after the first
    // one went by), then every few seconds (a decoder that lost a packet catches up at the next).
    let mut next_pli = Instant::now();
    let mut video_since: Option<Instant> = None;
    let mut lost_since: Option<Instant> = None;
    loop {
        if s.stop.load(Ordering::Acquire) {
            rtc.disconnect();
            return Ok(());
        }
        let timeout = match rtc.poll_output()? {
            Output::Timeout(t) => t,
            Output::Transmit(t) => {
                // A send that fails (no route for a moment) is a lost packet, as on any network.
                if let Err(e) = socket.send_to(&t.contents, t.destination) {
                    tracing::debug!(error = %e, "a guest packet wasn't sent");
                }
                continue;
            }
            Output::Event(e) => {
                match e {
                    Event::IceConnectionStateChange(state) => match state {
                        IceConnectionState::Connected | IceConnectionState::Completed => {
                            s.connected.store(true, Ordering::Release);
                            lost_since = None;
                        }
                        IceConnectionState::Disconnected => {
                            s.connected.store(false, Ordering::Release);
                            lost_since.get_or_insert_with(Instant::now);
                        }
                        _ => {}
                    },
                    Event::RtpPacket(p) => {
                        let codec = rtc
                            .codec_config()
                            .params()
                            .iter()
                            .find(|c| c.pt() == p.header.payload_type)
                            .map(|c| c.spec().codec);
                        let (pt, dest) = match codec {
                            Some(Codec::Vp8) => {
                                s.video.store(true, Ordering::Release);
                                if video_ssrc.is_none() {
                                    video_ssrc = Some(p.header.ssrc);
                                    video_since = Some(Instant::now());
                                    next_pli = Instant::now();
                                }
                                (VIDEO_PT, to.0)
                            }
                            Some(Codec::Opus) => {
                                s.audio.store(true, Ordering::Release);
                                (AUDIO_PT, to.1)
                            }
                            // Resends (RTX) and anything else: FFmpeg's reorder queue and the next
                            // keyframe cover a lost packet.
                            _ => continue,
                        };
                        s.bytes.fetch_add(p.payload.len() as u64, Ordering::Relaxed);
                        s.last_ms.store((born.elapsed().as_millis() as u64).max(1), Ordering::Relaxed);
                        let h = &p.header;
                        let bytes = rtp_bytes(pt, h.marker, h.sequence_number, h.timestamp, *h.ssrc, &p.payload);
                        // FFmpeg not listening yet (the source starting): dropped, as on a network.
                        let _ = out.send_to(&bytes, dest);
                    }
                    _ => {}
                }
                continue;
            }
        };
        if let Some(t) = lost_since
            && t.elapsed() > GONE_AFTER
        {
            return Ok(());
        }
        if let Some(ssrc) = video_ssrc
            && Instant::now() >= next_pli
        {
            if let Some(rx) = rtc.direct_api().stream_rx(&ssrc) {
                rx.request_keyframe(KeyframeRequestKind::Pli);
            }
            let starting = video_since.is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            next_pli = Instant::now() + Duration::from_secs(if starting { 1 } else { 3 });
        }
        let now = Instant::now();
        let wait = timeout.saturating_duration_since(now).min(Duration::from_millis(100));
        if wait.is_zero() {
            rtc.handle_input(Input::Timeout(now))?;
            continue;
        }
        socket.set_read_timeout(Some(wait))?;
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => match buf[..n].try_into() {
                Ok(contents) => Input::Receive(
                    Instant::now(),
                    Receive { proto: Protocol::Udp, source, destination: local, contents },
                ),
                // Not WebRTC (a stray packet): ignored.
                Err(_) => continue,
            },
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                Input::Timeout(Instant::now())
            }
            // Windows reports a candidate that went away (ICMP "port unreachable") on the next
            // read; ICE carries on with the others. Anything else passing is no reason to hang up.
            Err(e) => {
                tracing::debug!(error = %e, "a guest packet couldn't be read");
                // Never a spin, should it repeat.
                std::thread::sleep(Duration::from_millis(10));
                Input::Timeout(Instant::now())
            }
        };
        rtc.handle_input(input)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_is_written_as_ffmpeg_reads_it() {
        let b = rtp_bytes(96, true, 0x1234, 0xA0B0_C0D0, 7, &[9, 9]);
        assert_eq!(b, [0x80, 0x80 | 96, 0x12, 0x34, 0xA0, 0xB0, 0xC0, 0xD0, 0, 0, 0, 7, 9, 9]);
    }

    #[test]
    fn the_sdp_names_both_ports() {
        let t = sdp_text(50000, 50002);
        assert!(t.contains("m=video 50000 RTP/AVP 96\r\na=rtpmap:96 VP8/90000"));
        assert!(t.contains("m=audio 50002 RTP/AVP 111\r\na=rtpmap:111 opus/48000/2"));
    }

    #[test]
    fn chrome_is_told_to_start_sharp() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\na=rtpmap:96 VP8/90000\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\n";
        let out = with_bitrates(sdp);
        assert!(
            out.contains(
                "a=rtpmap:96 VP8/90000\r\na=fmtp:96 x-google-start-bitrate=2500;x-google-max-bitrate=6000\r\n"
            )
        );
        assert!(out.contains("a=fmtp:97 apt=96\r\n"), "the resend line is left alone");
        let had = "a=rtpmap:96 VP8/90000\r\na=fmtp:96 max-fs=12288\r\n";
        assert_eq!(
            with_bitrates(had),
            "a=rtpmap:96 VP8/90000\r\na=fmtp:96 max-fs=12288;x-google-start-bitrate=2500;x-google-max-bitrate=6000\r\n"
        );
    }

    #[test]
    fn the_public_address_is_read_from_a_stun_answer() {
        let id = [7u8; 12];
        // 203.0.113.5:50000, XORed with the magic cookie as STUN sends it.
        let mut b = vec![0x01, 0x01, 0x00, 0x0C, 0x21, 0x12, 0xA4, 0x42];
        b.extend_from_slice(&id);
        b.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        b.extend_from_slice(&(50000u16 ^ 0x2112).to_be_bytes());
        b.extend_from_slice(&[203 ^ 0x21, 0x12, 113 ^ 0xA4, 5 ^ 0x42]); // 0 ^ 0x12 = 0x12
        assert_eq!(stun_mapped(&b, &id), Some("203.0.113.5:50000".parse().unwrap()));
        assert_eq!(stun_mapped(&b, &[8u8; 12]), None, "someone else's answer");
        assert_eq!(stun_mapped(&b[..10], &id), None, "cut short");
    }

    #[test]
    fn ports_come_in_even_pairs() {
        let (held, p) = free_pair().expect("two free ports");
        assert_eq!(p % 2, 0);
        let (_, q) = free_pair().expect("two more");
        assert!(q != p && q != p + 1, "held ports aren't handed out twice");
        drop(held);
    }
}
