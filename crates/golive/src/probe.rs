//! The upload check (owner's pick 4B): how fast this PC uploads, how far each platform's server
//! is, what each destination needs, and each platform's rules held against the real stream.
//!
//! OBS's auto-configuration measures upload with Twitch's bandwidth-test mode (`?bandwidthtest=true`
//! after the key: Twitch takes the data and shows nothing) and checks only its own settings. We
//! measure the same way, add the delay to every destination's own server, and check the stream
//! OBS really sends (codec, keyframe gap, bitrate) instead of what its settings say.

use std::time::{Duration, Instant};

use bytes::Bytes;
use rml_rtmp::time::RtmpTimestamp;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::flv::VideoCodec;
use crate::hub::StreamStats;
use crate::push::{Publisher, PushError};
use crate::target::Target;

/// Which platform a destination is, for its rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Twitch,
    Youtube,
    Kick,
    Custom,
}

/// Twitch's bandwidth-test switch, put after the stream key.
pub const TWITCH_BANDWIDTH_TEST: &str = "?bandwidthtest=true";
/// Share of the measured upload a stream may use: the rest is for the game, chat and retries.
pub const HEADROOM: f64 = 0.8;
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// The first part of an upload test fills the PC's own send buffers; it isn't counted.
const WARM_UP: Duration = Duration::from_secs(1);
/// One test packet, about a 1080p frame.
const CHUNK: usize = 64 * 1024;

/// Time to open a connection to the server (the name already looked up), best of three.
pub async fn delay(t: &Target) -> Result<Duration, PushError> {
    let addr = timeout(CONNECT_DEADLINE, tokio::net::lookup_host((t.host.as_str(), t.port)))
        .await
        .map_err(|_| PushError::Slow(t.host.clone()))?
        .map_err(|_| PushError::Unreachable(t.host.clone()))?
        .next()
        .ok_or_else(|| PushError::Unreachable(t.host.clone()))?;
    let mut best: Option<Duration> = None;
    for _ in 0..3 {
        let start = Instant::now();
        let ok = timeout(CONNECT_DEADLINE, TcpStream::connect(addr)).await.is_ok_and(|c| c.is_ok());
        if ok {
            let d = start.elapsed();
            best = Some(best.map_or(d, |b| b.min(d)));
        }
    }
    best.ok_or_else(|| PushError::Unreachable(t.host.clone()))
}

/// Publishes test data to `t` as fast as the line takes it for `length`, and returns the
/// upload speed in kbps. Use only where nothing goes live (Twitch with
/// [`TWITCH_BANDWIDTH_TEST`] after the key, or a test server).
pub async fn upload(t: &Target, key: &str, length: Duration) -> Result<u32, PushError> {
    let mut p = Publisher::open(t, key).await?;
    let header = p.client.publish_video_data(Bytes::from_static(&[0x17, 0x00, 0, 0, 0]), RtmpTimestamp::new(0), false);
    p.send(header).await?;
    let mut payload = vec![0u8; CHUNK];
    payload[0] = 0x27;
    payload[1] = 0x01;
    // Not all zeros, so nothing on the way can squeeze it smaller.
    payload[5..].iter_mut().for_each(|b| *b = fastrand::u8(..));
    let payload = Bytes::from(payload);
    let start = Instant::now();
    let mut counted: Option<(Instant, u64)> = None;
    let mut ts = 0u32;
    while start.elapsed() < length {
        ts = ts.wrapping_add(33);
        let r = p.client.publish_video_data(payload.clone(), RtmpTimestamp::new(ts), false);
        p.send(r).await?;
        if counted.is_none() && start.elapsed() >= WARM_UP {
            counted = Some((Instant::now(), p.sent));
        }
    }
    let (from, sent) = counted.ok_or_else(|| PushError::Slow(t.host.clone()))?;
    let secs = from.elapsed().as_secs_f64().max(0.001);
    Ok(((p.sent - sent) as f64 * 8.0 / 1000.0 / secs) as u32)
}

/// What a destination needs, in the order the streamer set (top first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Need {
    pub name: String,
    pub kbps: u32,
}

/// The upload check's answer, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub upload_kbps: u32,
    pub need_kbps: u32,
    pub fits: bool,
    /// How many of the destinations, from the top, fit at the stream's quality.
    pub fit_count: usize,
    pub message: String,
}

pub fn verdict(upload_kbps: u32, needs: &[Need]) -> Verdict {
    let need: u32 = needs.iter().map(|n| n.kbps).sum();
    let usable = (f64::from(upload_kbps) * HEADROOM) as u32;
    let n = needs.len();
    let mut fit_count = 0;
    let mut total = 0;
    for x in needs {
        total += x.kbps;
        if total > usable {
            break;
        }
        fit_count += 1;
    }
    let fits = need <= usable;
    let message = if n == 0 {
        format!("Your upload is {}. Turn on a platform to check it.", mbps(upload_kbps))
    } else if fits {
        format!(
            "Your upload is {}. {} fit{}, using {}.",
            mbps(upload_kbps),
            count(n),
            if n == 1 { "s" } else { "" },
            mbps(need)
        )
    } else {
        let each = usable / n as u32;
        let mut m = format!("{} need {}; you have {}.", count(n), mbps(need), mbps(upload_kbps));
        if fit_count > 0 {
            m.push_str(&format!(" Send {} at this quality", count(fit_count).to_lowercase()));
            m.push_str(&format!(", or all {n} at about {} each (lower the bitrate in OBS).", mbps(each)));
        } else {
            m.push_str(&format!(" Lower the bitrate in OBS to about {} for one platform.", mbps(usable)));
        }
        m
    };
    Verdict { upload_kbps, need_kbps: need, fits, fit_count, message }
}

/// Each platform's rules, held against the stream OBS really sends. Unknown numbers (not
/// streaming yet) are skipped, never guessed.
pub fn rules(stats: &StreamStats, s: Service) -> Vec<String> {
    let (name, codecs, max_keyint_ms, max_kbps): (&str, &[VideoCodec], u32, u32) = match s {
        // Twitch: H.264 only, a keyframe every 2 s, 6,000 kbps video (OBS's own recommendations).
        Service::Twitch => ("Twitch", &[VideoCodec::H264], 2_000, 6_000),
        // YouTube: H.264, HEVC or AV1; a keyframe at least every 4 s (2 s is best).
        Service::Youtube => ("YouTube", &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1], 4_000, 51_000),
        // Kick: H.264, a keyframe every 2 s, up to 8,000 kbps.
        Service::Kick => ("Kick", &[VideoCodec::H264], 2_000, 8_000),
        Service::Custom => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(c) = stats.video_codec
        && !codecs.contains(&c)
    {
        out.push(format!(
            "{name} doesn't take {} video. Pick H.264 in OBS (Settings, Output, Video Encoder), or turn {name} off.",
            codec_name(c)
        ));
    }
    // A little slack: encoders place keyframes at the frame nearest the interval.
    if let Some(k) = stats.keyframe_interval_ms
        && k > max_keyint_ms + 250
    {
        out.push(format!(
            "{name} needs a keyframe at least every {} s; yours come every {:.1} s. Set Keyframe Interval to 2 in OBS (Settings, Output).",
            max_keyint_ms / 1000,
            f64::from(k) / 1000.0
        ));
    }
    // The measured rate includes audio (up to about 320 kbps).
    if stats.kbps > max_kbps + 400 {
        out.push(format!(
            "{name} takes up to {} of video; you're sending {}. Lower the bitrate in OBS, or viewers may see it stutter.",
            kbps_text(max_kbps),
            kbps_text(stats.kbps)
        ));
    }
    out
}

fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC (H.265)",
        VideoCodec::Av1 => "AV1",
        VideoCodec::Vp9 => "VP9",
    }
}

fn count(n: usize) -> String {
    match n {
        1 => "1 platform".into(),
        _ => format!("{n} platforms"),
    }
}

/// `12.5 Mbps` (one decimal under 10, whole above).
pub fn mbps(kbps: u32) -> String {
    let m = f64::from(kbps) / 1000.0;
    if m < 10.0 { format!("{m:.1} Mbps") } else { format!("{m:.0} Mbps") }
}

/// `6,000 kbps`.
fn kbps_text(kbps: u32) -> String {
    let s = kbps.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} kbps")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn need(name: &str, kbps: u32) -> Need {
        Need { name: name.into(), kbps }
    }

    #[test]
    fn verdicts_in_words() {
        let three = [need("Twitch", 6000), need("YouTube", 6000), need("Kick", 6000)];
        let v = verdict(24_000, &three);
        assert!(v.fits && v.fit_count == 3);
        assert_eq!(v.message, "Your upload is 24 Mbps. 3 platforms fit, using 18 Mbps.");
        let v = verdict(12_000, &three);
        assert!(!v.fits);
        assert_eq!(v.fit_count, 1, "80% of 12 is 9.6: only the first 6 fits");
        assert_eq!(
            v.message,
            "3 platforms need 18 Mbps; you have 12 Mbps. Send 1 platform at this quality, or all 3 at about 3.2 Mbps each (lower the bitrate in OBS)."
        );
        let v = verdict(5_000, &[need("Twitch", 6000)]);
        assert_eq!(v.fit_count, 0);
        assert!(v.message.ends_with("Lower the bitrate in OBS to about 4.0 Mbps for one platform."), "{}", v.message);
        assert_eq!(verdict(8_000, &[]).message, "Your upload is 8.0 Mbps. Turn on a platform to check it.");
        assert!(!verdict(0, &[need("Twitch", 1)]).fits);
    }

    #[test]
    fn platform_rules_against_the_real_stream() {
        let good = StreamStats {
            kbps: 6200,
            keyframe_interval_ms: Some(2000),
            video_codec: Some(VideoCodec::H264),
            ..Default::default()
        };
        assert!(rules(&good, Service::Twitch).is_empty());
        assert!(rules(&good, Service::Youtube).is_empty());
        assert!(rules(&good, Service::Kick).is_empty());
        let hevc =
            StreamStats { video_codec: Some(VideoCodec::Hevc), keyframe_interval_ms: Some(8000), kbps: 9000, ..good };
        let t = rules(&hevc, Service::Twitch);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].starts_with("Twitch doesn't take HEVC (H.265) video."));
        assert!(t[1].contains("yours come every 8.0 s"));
        assert!(t[2].contains("up to 6,000 kbps of video; you're sending 9,000 kbps"));
        // YouTube takes HEVC and 9 Mbps, but not an 8 s keyframe gap.
        let y = rules(&hevc, Service::Youtube);
        assert_eq!(y.len(), 1);
        assert!(y[0].contains("at least every 4 s"));
        assert!(rules(&hevc, Service::Custom).is_empty());
        // Not streaming yet: nothing known, nothing said.
        assert!(rules(&StreamStats::default(), Service::Twitch).is_empty());
        // A 2.2 s gap is an encoder rounding, not a problem.
        let near = StreamStats { keyframe_interval_ms: Some(2200), ..good };
        assert!(rules(&near, Service::Twitch).is_empty());
    }

    #[test]
    fn numbers_in_words() {
        assert_eq!(mbps(9_600), "9.6 Mbps");
        assert_eq!(mbps(12_400), "12 Mbps");
        assert_eq!(kbps_text(6000), "6,000 kbps");
        assert_eq!(kbps_text(51000), "51,000 kbps");
        assert_eq!(kbps_text(900), "900 kbps");
        assert_eq!(kbps_text(1_234_567), "1,234,567 kbps");
    }
}
