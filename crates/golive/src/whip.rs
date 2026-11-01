//! WebRTC by WHIP (RFC 9725), as OBS's WHIP output sends: an offer posted to the endpoint (its
//! bearer token the key), the answer back, then the stream over WebRTC with str0m: H.264 as it is
//! (no B-frames: WebRTC can't carry them), and the AAC made Opus (WebRTC's sound). The session
//! is deleted at the end, as the spec asks.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::Packet;
use symphonia_core::units::{Duration as Dur, Timestamp};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, watch};

use crate::direct::{Direct, DirectError};
use crate::hub::{Frame, Hub, Subscription};
use crate::push::{Ended, Health, Link, PushState, set_state};
use crate::ts::{VideoSetup, annex_b, video_setup};

/// Opus frames: 20 ms at 48 kHz.
const OPUS_FRAME: usize = 960;
/// How long to wait for the endpoint, and for the connection after it answered.
const ANSWER_DEADLINE: Duration = Duration::from_secs(10);
const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
/// Gone this long, the connection has broken.
const LOST_AFTER: Duration = Duration::from_secs(10);

/// The address this computer's traffic to `host` leaves by (no packet is sent; the name is
/// looked up without holding a worker thread, for at most a few seconds).
async fn local_ip(host: &str, port: u16) -> Option<IpAddr> {
    let to =
        tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await.ok()?.ok()?.next()?;
    let any: SocketAddr =
        if to.is_ipv6() { (Ipv6Addr::UNSPECIFIED, 0).into() } else { (Ipv4Addr::UNSPECIFIED, 0).into() };
    let probe = UdpSocket::bind(any).await.ok()?;
    probe.connect(to).await.ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

/// The stream's AAC decoded and made Opus, 20 ms at a time, timed in 48 kHz samples.
struct Sound {
    aac: symphonia_codec_aac::AacDecoder,
    opus: opus::Encoder,
    /// Decoded, interleaved stereo waiting for a whole Opus frame.
    pcm: Vec<f32>,
    /// Samples sent (the RTP clock), from the first frame's time.
    samples: u64,
    channels: usize,
    /// The stream's sound rate (made 48 kHz on the way) and where the resampling is between
    /// the last two input frames.
    rate: u32,
    pos: f64,
    last: [f32; 2],
}

impl Sound {
    /// From AAC's AudioSpecificConfig (any rate: made 48 kHz).
    fn new(asc: &[u8], kbps: u32) -> Result<Self, DirectError> {
        let bad = |why: String| DirectError::Refused(format!("WebRTC's sound couldn't be made: {why}."));
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_AAC).with_extra_data(asc.to_vec().into_boxed_slice());
        let aac = symphonia_codec_aac::AacDecoder::try_new(&p, &AudioDecoderOptions::default())
            .map_err(|e| bad(e.to_string()))?;
        let rate = aac.codec_params().sample_rate.unwrap_or(0);
        if !(8_000..=96_000).contains(&rate) {
            return Err(bad(format!("the stream's sound is {rate} Hz")));
        }
        let channels = aac.codec_params().channels.as_ref().map_or(2, |c| c.count()).clamp(1, 2);
        let mut opus = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(|e| bad(e.to_string()))?;
        let _ = opus.set_bitrate(opus::Bitrate::Bits((kbps.clamp(32, 256) * 1000) as i32));
        Ok(Self {
            aac,
            opus,
            pcm: Vec::with_capacity(OPUS_FRAME * 4),
            samples: 0,
            channels,
            rate,
            pos: 0.0,
            last: [0.0; 2],
        })
    }

    /// One AAC frame in → the Opus packets now whole, each with its RTP time.
    fn push(&mut self, frame: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let packet = Packet::new(0, Timestamp::new(0), Dur::new(1024), frame.to_vec());
        let mut decoded: Vec<f32> = Vec::new();
        match self.aac.decode(&packet) {
            Ok(buf) => buf.copy_to_vec_interleaved(&mut decoded),
            // A frame that won't decode: left out (as a lost packet would be).
            Err(_) => return Vec::new(),
        }
        let stereo: Vec<f32> =
            if self.channels == 1 { decoded.iter().flat_map(|v| [*v, *v]).collect() } else { decoded };
        if self.rate == 48_000 {
            self.pcm.extend_from_slice(&stereo);
        } else {
            self.resample(&stereo);
        }
        let mut out = Vec::new();
        while self.pcm.len() >= OPUS_FRAME * 2 {
            let chunk: Vec<f32> = self.pcm.drain(..OPUS_FRAME * 2).collect();
            if let Ok(p) = self.opus.encode_vec_float(&chunk, 4000) {
                out.push((self.samples, p));
            }
            self.samples += OPUS_FRAME as u64;
        }
        out
    }

    /// Stereo at the stream's rate onto the 48 kHz queue, by straight lines between samples
    /// (carried across frames, so there's no seam).
    fn resample(&mut self, stereo: &[f32]) {
        let step = f64::from(self.rate) / 48_000.0;
        let frames = stereo.len() / 2;
        // Frame -1 is the last one of the frame before.
        let at = |i: isize, c: usize| if i < 0 { self.last[c] } else { stereo[i as usize * 2 + c] };
        while self.pos < frames as f64 - 1.0 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..2 {
                let (a, b) = (at(i, c), at(i + 1, c));
                self.pcm.push(a + (b - a) * t);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last = [stereo[(frames - 1) * 2], stereo[(frames - 1) * 2 + 1]];
        }
    }
}

pub(crate) async fn session(
    d: &Direct,
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
) -> Result<Ended, DirectError> {
    let url = crate::direct::whip_address(&d.url)?;
    let host = url.host_str().unwrap_or_default().to_string();
    let name = host.clone();
    let port = url.port_or_known_default().unwrap_or(443);

    // Our side: one UDP socket on the address that reaches the endpoint.
    let ip = local_ip(&host, port).await.ok_or_else(|| DirectError::Unreachable(name.clone()))?;
    let socket = UdpSocket::bind((ip, 0)).await.map_err(|_| DirectError::Unreachable(name.clone()))?;
    let local = socket.local_addr().map_err(|_| DirectError::Unreachable(name.clone()))?;
    // Stats once a second: the link's ping, loss and resends.
    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(true, false)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(Instant::now());
    let cand = Candidate::host(local, "udp").map_err(|e| DirectError::Refused(e.to_string()))?;
    rtc.add_local_candidate(cand);
    let mut api = rtc.sdp_api();
    let vmid =
        api.add_media(MediaKind::Video, Direction::SendOnly, Some("ferrocast".into()), Some("video".into()), None);
    let amid =
        api.add_media(MediaKind::Audio, Direction::SendOnly, Some("ferrocast".into()), Some("audio".into()), None);
    let (offer, pending) =
        api.apply().ok_or_else(|| DirectError::Refused("The WebRTC offer couldn't be made.".into()))?;

    // The offer posted; the answer and the session's own address back.
    let (client, session_url, text) = post_offer(&url, d.key.trim(), offer.to_sdp_string(), &name).await?;
    let answer = SdpAnswer::from_sdp_string(&text)
        .map_err(|_| DirectError::Refused(format!("{name} answered something that isn't WebRTC.")))?;
    rtc.sdp_api()
        .accept_answer(pending, answer)
        .map_err(|e| DirectError::Refused(format!("{name}'s answer won't do: {e}.")))?;

    // However the session ends (stopped mid-stream too), it's deleted as the spec asks.
    let _end = session_url.map(|url| EndSession { client: client.clone(), url, key: d.key.trim().to_string() });
    let out = stream_to(&mut rtc, &socket, local, hub, health, stream, vmid, amid, &name).await;
    rtc.disconnect();
    out
}

/// Offers a session as the stream would (picture and sound), checks the answer, and deletes it
/// again: how long the server took to answer.
pub(crate) async fn reach(d: &Direct) -> Result<Duration, DirectError> {
    let url = crate::direct::whip_address(&d.url)?;
    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let ip = local_ip(&host, port).await.ok_or_else(|| DirectError::Unreachable(host.clone()))?;
    let socket = UdpSocket::bind((ip, 0)).await.map_err(|_| DirectError::Unreachable(host.clone()))?;
    let local = socket.local_addr().map_err(|_| DirectError::Unreachable(host.clone()))?;
    let mut rtc = Rtc::builder().clear_codecs().enable_h264(true).enable_opus(true, false).build(Instant::now());
    let cand = Candidate::host(local, "udp").map_err(|e| DirectError::Refused(e.to_string()))?;
    rtc.add_local_candidate(cand);
    let mut api = rtc.sdp_api();
    api.add_media(MediaKind::Video, Direction::SendOnly, Some("ferrocast".into()), Some("video".into()), None);
    api.add_media(MediaKind::Audio, Direction::SendOnly, Some("ferrocast".into()), Some("audio".into()), None);
    let (offer, pending) =
        api.apply().ok_or_else(|| DirectError::Refused("The WebRTC offer couldn't be made.".into()))?;
    let started = Instant::now();
    let (client, session_url, text) = post_offer(&url, d.key.trim(), offer.to_sdp_string(), &host).await?;
    let took = started.elapsed();
    let _end = session_url.map(|url| EndSession { client, url, key: d.key.trim().to_string() });
    let answer = SdpAnswer::from_sdp_string(&text)
        .map_err(|_| DirectError::Refused(format!("{host} answered something that isn't WebRTC.")))?;
    rtc.sdp_api()
        .accept_answer(pending, answer)
        .map_err(|e| DirectError::Refused(format!("{host}'s answer won't do: {e}.")))?;
    Ok(took)
}

/// Posts a WHIP offer (with the token, if any) → the client, the session's address and the
/// answer.
async fn post_offer(
    url: &reqwest::Url,
    key: &str,
    sdp: String,
    name: &str,
) -> Result<(reqwest::Client, Option<reqwest::Url>, String), DirectError> {
    let client =
        reqwest::Client::builder().timeout(ANSWER_DEADLINE).build().map_err(|e| DirectError::Refused(e.to_string()))?;
    let mut req = client.post(url.clone()).header("content-type", "application/sdp").body(sdp);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() { DirectError::Slow(name.to_string()) } else { DirectError::Unreachable(name.to_string()) }
    })?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(DirectError::Refused(format!("{name} refused the token (the key).")));
    }
    if !status.is_success() {
        return Err(DirectError::Refused(format!("{name} answered {status}.")));
    }
    let session_url = resp.headers().get("location").and_then(|v| v.to_str().ok()).and_then(|l| url.join(l).ok());
    let text = resp.text().await.map_err(|_| DirectError::Broken(name.to_string()))?;
    Ok((client, session_url, text))
}

/// Deletes the WHIP session when dropped (a server that keeps it a while anyway lets it go).
struct EndSession {
    client: reqwest::Client,
    url: reqwest::Url,
    key: String,
}

impl Drop for EndSession {
    fn drop(&mut self) {
        let mut del = self.client.delete(self.url.clone());
        if !self.key.is_empty() {
            del = del.bearer_auth(&self.key);
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(3), del.send()).await;
            });
        }
    }
}

/// The payload type the answer settled for `codec` on `mid` (H.264: packetization mode 1 first).
fn pt_for(rtc: &mut Rtc, mid: Mid, codec: Codec) -> Option<Pt> {
    let w = rtc.writer(mid)?;
    let mut ps: Vec<_> = w.payload_params().filter(|p| p.spec().codec == codec).collect();
    ps.sort_by_key(|p| p.spec().format.packetization_mode != Some(1));
    ps.first().map(|p| p.pt())
}

#[allow(clippy::too_many_arguments)]
async fn stream_to(
    rtc: &mut Rtc,
    socket: &UdpSocket,
    local: SocketAddr,
    hub: &Hub,
    health: &watch::Sender<Health>,
    stream: u64,
    vmid: Mid,
    amid: Mid,
    name: &str,
) -> Result<Ended, DirectError> {
    let Subscription { live, headers, mut rx } = hub.subscribe();
    if !live.on || live.stream != stream {
        return Ok(Ended::StreamEnded);
    }
    let mut video: Option<VideoSetup> = None;
    let mut sound: Option<Sound> = None;
    // Opus at 128 kbps: transparent for speech and music.
    let kbps = 128;
    if let Some(v) = &headers.video {
        video = Some(video_ready(v)?);
    }
    if let Some(a) = &headers.audio
        && a.get(1) == Some(&0)
    {
        sound = Some(Sound::new(a.get(2..).unwrap_or_default(), kbps)?);
    }
    let (vpt, apt) = (pt_for(rtc, vmid, Codec::H264), pt_for(rtc, amid, Codec::Opus));
    if vpt.is_none() && apt.is_none() {
        return Err(DirectError::Refused(format!("{name} takes neither H.264 nor Opus.")));
    }
    let started = Instant::now();
    let mut connected = false;
    let mut lost_since: Option<Instant> = None;
    let mut waiting_for_key = true;
    // The first frame's time: times count from it.
    let mut base: Option<u32> = None;
    let mut audio_base: Option<u64> = None;
    let mut buf = vec![0u8; 2048];
    let (mut window, mut sent) = (Instant::now(), 0u64);
    // The picture's packets and the receiver's asks for them again, at the last stats.
    let mut counted = (0u64, 0u64);
    loop {
        // Everything str0m has to say, until it wants time.
        let deadline = loop {
            match rtc.poll_output().map_err(|e| DirectError::Refused(e.to_string()))? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    sent += t.contents.len() as u64;
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Output::Event(Event::IceConnectionStateChange(s)) => match s {
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        connected = true;
                        lost_since = None;
                        set_state(health, PushState::Live);
                    }
                    IceConnectionState::Disconnected => {
                        lost_since.get_or_insert_with(Instant::now);
                    }
                    _ => {}
                },
                Output::Event(Event::MediaEgressStats(st)) if st.mid == vmid => {
                    let packets = st.packets.saturating_sub(counted.0).max(1) as f32;
                    let link = Link {
                        rtt_ms: st.rtt.map_or(0, |d| d.as_millis() as u32),
                        lost_pct: st.loss.unwrap_or(0.0) * 100.0,
                        resent_pct: st.nacks.saturating_sub(counted.1) as f32 * 100.0 / packets,
                    };
                    counted = (st.packets, st.nacks);
                    if st.rtt.is_some() {
                        health.send_modify(|h| h.link = Some(link));
                    }
                }
                Output::Event(_) => {}
            }
        };
        if !connected && started.elapsed() > CONNECT_DEADLINE {
            return Err(DirectError::Slow(name.to_string()));
        }
        if lost_since.is_some_and(|t| t.elapsed() > LOST_AFTER) {
            return Err(DirectError::Broken(name.to_string()));
        }
        if window.elapsed() >= Duration::from_secs(1) {
            let k = (sent * 8 / 1000) as u32;
            health.send_modify(|h| {
                h.kbps = k;
                h.sent += sent;
            });
            (window, sent) = (Instant::now(), 0);
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                if let Ok((n, source)) = r
                    && let Ok(contents) = buf[..n].try_into()
                {
                    let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                    rtc.handle_input(input).map_err(|e| DirectError::Refused(e.to_string()))?;
                }
            }
            f = rx.recv() => {
                let frame = match f {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        health.send_modify(|h| h.dropped += n);
                        waiting_for_key = true;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(Ended::StreamEnded),
                };
                match frame {
                    Frame::End => return Ok(Ended::StreamEnded),
                    Frame::Metadata(_) => {}
                    Frame::Video { ts, data, .. } => {
                        if crate::flv::video_is_header(&data) {
                            video = Some(video_ready(&data)?);
                            waiting_for_key = true;
                            continue;
                        }
                        let (Some(setup), Some(pt), true) = (&video, vpt, connected) else { continue };
                        let Some((key, cts, es)) = annex_b(setup, &data, false) else { continue };
                        if cts != 0 {
                            return Err(DirectError::Refused(
                                "WebRTC can't carry B-frames: turn them off in the Studio's video settings to send by WHIP.".into(),
                            ));
                        }
                        if waiting_for_key && !key {
                            continue;
                        }
                        waiting_for_key = false;
                        let b = *base.get_or_insert(ts);
                        let t = MediaTime::new(u64::from(ts.saturating_sub(b)) * 90, Frequency::NINETY_KHZ);
                        if let Some(w) = rtc.writer(vmid) {
                            let _ = w.write(pt, Instant::now(), t, es);
                        }
                    }
                    Frame::Audio { ts, data } => {
                        if crate::flv::audio_extra_track(&data).is_some() || data.first().map(|b| b >> 4) != Some(10) {
                            continue;
                        }
                        if data.get(1) == Some(&0) {
                            sound = Some(Sound::new(data.get(2..).unwrap_or_default(), kbps)?);
                            // Its clock starts again from the next frame's time.
                            audio_base = None;
                            continue;
                        }
                        let (Some(s), Some(pt), true) = (sound.as_mut(), apt, connected) else { continue };
                        // The sound starts where the pictures' clock is (48 kHz).
                        let b = *base.get_or_insert(ts);
                        let start = *audio_base.get_or_insert(u64::from(ts.saturating_sub(b)) * 48);
                        for (at, packet) in s.push(data.get(2..).unwrap_or_default()) {
                            let t = MediaTime::new(start + at, Frequency::FORTY_EIGHT_KHZ);
                            if let Some(w) = rtc.writer(amid) {
                                let _ = w.write(pt, Instant::now(), t, packet);
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e| DirectError::Refused(e.to_string()))?;
            }
        }
    }
}

/// The video's setup from its header: H.264 only (WebRTC endpoints rarely take HEVC).
fn video_ready(header: &[u8]) -> Result<VideoSetup, DirectError> {
    match video_setup(header) {
        Some(s) if !s.hevc => Ok(s),
        Some(_) => Err(DirectError::Refused(
            "WebRTC (WHIP) sends H.264: pick an H.264 encoder in the Studio's video settings.".into(),
        )),
        None => Err(DirectError::Refused("The stream's video can't go over WebRTC.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::{Kind, run};
    use crate::push::Retry;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the test endpoint received: H.264 frames and Opus packets.
    #[derive(Default)]
    struct Got {
        video: AtomicU32,
        audio: AtomicU32,
        deleted: AtomicU32,
    }

    /// A WHIP endpoint on this computer: answers one offer with str0m and counts what arrives.
    async fn endpoint(got: Arc<Got>) -> Option<u16> {
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
        let port = http.local_addr().ok()?.port();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = http.accept().await else { return };
                let got = got.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = vec![0u8; 16384];
                    // Headers, then the body by its length.
                    let (head, body) = loop {
                        let Ok(n) = c.read(&mut buf).await else { return };
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                        if let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&req[..i]).to_string();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            while req.len() < i + 4 + len {
                                let Ok(n) = c.read(&mut buf).await else { return };
                                if n == 0 {
                                    break;
                                }
                                req.extend_from_slice(&buf[..n]);
                            }
                            break (head, String::from_utf8_lossy(&req[i + 4..]).to_string());
                        }
                    };
                    if head.starts_with("DELETE") {
                        got.deleted.fetch_add(1, Ordering::Relaxed);
                        let _ = c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                        return;
                    }
                    assert!(head.to_ascii_lowercase().contains("authorization: bearer tok"), "{head}");
                    let Ok(offer) = str0m::change::SdpOffer::from_sdp_string(&body) else { return };
                    let Ok(sock) = UdpSocket::bind("127.0.0.1:0").await else { return };
                    let Ok(local) = sock.local_addr() else { return };
                    let mut rtc = Rtc::builder()
                        .set_ice_lite(true)
                        .clear_codecs()
                        .enable_h264(true)
                        .enable_opus(true, false)
                        .build(Instant::now());
                    if let Ok(cand) = Candidate::host(local, "udp") {
                        rtc.add_local_candidate(cand);
                    }
                    let Ok(answer) = rtc.sdp_api().accept_offer(offer) else { return };
                    let sdp = answer.to_sdp_string();
                    let reply = format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/sdp\r\nlocation: /whip/session1\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{sdp}",
                        sdp.len()
                    );
                    let _ = c.write_all(reply.as_bytes()).await;
                    let _ = c.shutdown().await;
                    let mut buf = vec![0u8; 2048];
                    let until = Instant::now() + Duration::from_secs(20);
                    while Instant::now() < until {
                        let deadline = loop {
                            match rtc.poll_output() {
                                Ok(Output::Timeout(t)) => break t,
                                Ok(Output::Transmit(t)) => {
                                    let _ = sock.send_to(&t.contents, t.destination).await;
                                }
                                Ok(Output::Event(Event::MediaData(m))) => {
                                    let which =
                                        if m.params.spec().codec == Codec::H264 { &got.video } else { &got.audio };
                                    which.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(Output::Event(_)) => {}
                                Err(_) => return,
                            }
                        };
                        let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
                        tokio::select! {
                            r = sock.recv_from(&mut buf) => {
                                if let Ok((n, source)) = r
                                    && let Ok(contents) = buf[..n].try_into()
                                {
                                    let _ = rtc.handle_input(Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }));
                                }
                            }
                            _ = tokio::time::sleep(wait) => {
                                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                            }
                        }
                    }
                });
            }
        });
        Some(port)
    }

    async fn send(tags: Vec<(u8, u32, Vec<u8>)>, port: u16) -> watch::Receiver<Health> {
        let hub = Hub::new();
        hub.start();
        let (health, h) =
            watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, sent: 0, link: None, since_ms: 0 });
        let (stop_tx, stop) = watch::channel(false);
        let d = Direct {
            id: "w".into(),
            kind: Kind::Whip,
            url: format!("http://127.0.0.1:{port}/whip"),
            key: "tok".into(),
            retry: Retry::default(),
        };
        let sender = tokio::spawn(run(d, hub.clone(), health, stop));
        // The headers first (the hub keeps them), then the connection, then 3 s of stream.
        let mut rest = Vec::new();
        for (kind, ms, body) in tags {
            let header = (kind == 9 && crate::flv::video_is_header(&body)) || (kind == 8 && body.get(1) == Some(&0));
            match (kind, header) {
                (9, true) => hub.video(ms, body.into()),
                (8, true) => hub.audio(ms, body.into()),
                _ => rest.push((kind, ms, body)),
            }
        }
        let mut h2 = h.clone();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            h2.wait_for(|h| h.state == PushState::Live || matches!(h.state, PushState::Reconnecting { .. })),
        )
        .await;
        for (kind, ms, body) in rest {
            match kind {
                9 => hub.video(ms, body.into()),
                8 => hub.audio(ms, body.into()),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let state = h.clone();
        hub.end();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = stop_tx.send(true);
        let _ = sender.await;
        state
    }

    /// Our offer answered by a WebRTC endpoint here, pictures and sound reaching it, and the
    /// session deleted at the end.
    #[tokio::test]
    async fn whip_reaches_an_endpoint() {
        let Some(tags) = crate::ts::tests::sample_tags_with(0) else { return };
        let got = Arc::new(Got::default());
        let Some(port) = endpoint(got.clone()).await else { return };
        let _ = send(tags, port).await;
        // The delete goes out as the session ends.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (v, a) = (got.video.load(Ordering::Relaxed), got.audio.load(Ordering::Relaxed));
        assert!(v >= 60, "pictures arrived: {v} (sound {a}, deleted {})", got.deleted.load(Ordering::Relaxed));
        assert!(a >= 100, "sound arrived as Opus: {a}");
        assert_eq!(got.deleted.load(Ordering::Relaxed), 1, "the session was deleted");
    }

    /// B-frames can't go over WebRTC: refused, saying so.
    #[tokio::test]
    async fn b_frames_are_refused_plainly() {
        let Some(tags) = crate::ts::tests::sample_tags_with(2) else { return };
        let got = Arc::new(Got::default());
        let Some(port) = endpoint(got.clone()).await else { return };
        let h = send(tags, port).await;
        let state = h.borrow().state.clone();
        assert!(
            matches!(&state, PushState::Reconnecting { reason, .. } if reason.contains("B-frames"))
                || got.video.load(Ordering::Relaxed) == 0,
            "{state:?}"
        );
    }
}
