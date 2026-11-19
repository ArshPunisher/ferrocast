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
    let mut p = Publisher::open(t, key, None).await?;
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
    verdict_keeping(upload_kbps, needs, ((1.0 - HEADROOM) * 100.0).round() as u8)
}

/// [`verdict`] keeping `free_pct` percent of the upload for the game and chat (Advanced mode).
pub fn verdict_keeping(upload_kbps: u32, needs: &[Need], free_pct: u8) -> Verdict {
    let need: u32 = needs.iter().map(|n| n.kbps).sum();
    let usable = (f64::from(upload_kbps) * (1.0 - f64::from(free_pct.min(90)) / 100.0)) as u32;
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
        let mut m = format!("Upload {}, {} needed.", mbps(upload_kbps), mbps(need));
        if fit_count > 0 {
            m.push_str(&format!(
                " Send {}, or lower the bitrate to about {} each.",
                count(fit_count).to_lowercase(),
                mbps(each)
            ));
        } else {
            m.push_str(&format!(" Lower the bitrate to about {}.", mbps(usable)));
        }
        m
    };
    Verdict { upload_kbps, need_kbps: need, fits, fit_count, message }
}

/// Hard encoding limits for platforms whose ingest rejects or stutters above a ceiling
/// (OBS `services.json`: Twitch 6,000 kbps H.264 / 2 s; Kick 8,000 kbps H.264 / 2 s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformLimit {
    pub service: Service,
    pub name: &'static str,
    pub max_kbps: u32,
    pub keyframe_secs: u32,
    pub h264_only: bool,
}

/// Strict cap for `s` on a single-track main stream (`enhanced` Twitch uses Enhanced Broadcasting
/// tracks instead of the legacy 6,000 kbps ceiling).
pub fn ceiling(s: Service, enhanced: bool) -> Option<PlatformLimit> {
    match s {
        Service::Twitch if !enhanced => {
            Some(PlatformLimit { service: s, name: "Twitch", max_kbps: 6_000, keyframe_secs: 2, h264_only: true })
        }
        Service::Kick => {
            Some(PlatformLimit { service: s, name: "Kick", max_kbps: 8_000, keyframe_secs: 2, h264_only: true })
        }
        Service::Twitch | Service::Youtube | Service::Custom => None,
    }
}

/// Tightest ceiling across enabled single-track main destinations (`(service, enhanced)`).
pub fn effective_limit<I>(dests: I) -> Option<PlatformLimit>
where
    I: IntoIterator<Item = (Service, bool)>,
{
    dests.into_iter().filter_map(|(s, enh)| ceiling(s, enh)).min_by_key(|c| c.max_kbps)
}

/// Category of a platform rule broken by a live stream (Contract C-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleCode {
    Bitrate,
    Codec,
    Keyint,
}

/// One structured platform rule issue (Contract C-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleNotice {
    pub code: RuleCode,
    pub summary: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_kbps: Option<u32>,
}

/// Auto-cap metadata shown as a calm one-line chip on destinations constrained for compatibility (Contract C-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CappedInfo {
    pub kbps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<(u32, u32)>,
    pub separate_encode: bool,
    pub summary: String,
    pub detail: String,
}

/// Builds the calm [`CappedInfo`] chip when Studio fitted configured settings to `limit`, or
/// `None` if configured settings already fit within `limit`.
pub fn capped_info(
    limit: PlatformLimit,
    configured_kbps: u32,
    is_hevc: bool,
    keyframe_s: u32,
    vbr: bool,
) -> Option<CappedInfo> {
    let kbps_capped = configured_kbps > limit.max_kbps;
    let codec_forced = limit.h264_only && is_hevc;
    let keyint_forced = limit.keyframe_secs > 0 && keyframe_s != limit.keyframe_secs;
    if !kbps_capped && !codec_forced && !keyint_forced && !vbr {
        return None;
    }
    let applied = configured_kbps.min(limit.max_kbps);
    let summary = if kbps_capped {
        format!("Capped at {} for {}", kbps_text(limit.max_kbps), limit.name)
    } else if codec_forced {
        format!("H.264 set for {}", limit.name)
    } else if keyint_forced {
        format!("{} s keyframes for {}", limit.keyframe_secs, limit.name)
    } else {
        format!("CBR enabled for {}", limit.name)
    };
    let consequence = if limit.service == Service::Kick { "doesn't disconnect" } else { "viewers don't stutter" };
    let detail = if kbps_capped {
        format!(
            "Your {} stream is capped at {} H.264 ({} s keyframes, 1× VBV) so {} {}.",
            kbps_text(configured_kbps),
            kbps_text(applied),
            limit.keyframe_secs,
            limit.name,
            consequence
        )
    } else {
        format!(
            "Encoded at {} H.264 with {} s keyframes and 1× VBV CBR so {} {}.",
            kbps_text(applied),
            limit.keyframe_secs,
            limit.name,
            consequence
        )
    };
    Some(CappedInfo { kbps: applied, resolution: None, separate_encode: false, summary, detail })
}

/// Structured rules broken by a live stream against platform `s`. Unknown numbers are skipped;
/// Twitch Enhanced Broadcasting (`enhanced == true`) is exempt from the legacy 6,000 kbps check.
pub fn rules_structured(stats: &StreamStats, s: Service, enhanced: bool) -> Vec<RuleNotice> {
    if enhanced && s == Service::Twitch {
        return Vec::new();
    }
    let (name, codecs, max_keyint_ms, max_kbps): (&str, &[VideoCodec], u32, u32) = match s {
        Service::Twitch => ("Twitch", &[VideoCodec::H264], 2_000, 6_000),
        Service::Youtube => ("YouTube", &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1], 4_000, 51_000),
        Service::Kick => ("Kick", &[VideoCodec::H264], 2_000, 8_000),
        Service::Custom => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(c) = stats.video_codec
        && !codecs.contains(&c)
    {
        out.push(RuleNotice {
            code: RuleCode::Codec,
            summary: format!("{name} requires H.264"),
            detail: format!("{name} doesn't take {}. Switch to H.264, or turn {name} off.", codec_name(c)),
            fix_kbps: None,
        });
    }
    // A little slack: encoders place keyframes at the frame nearest the interval.
    if let Some(k) = stats.keyframe_interval_ms
        && k > max_keyint_ms + 250
    {
        out.push(RuleNotice {
            code: RuleCode::Keyint,
            summary: format!("Keyframes every {:.1} s (need {} s)", f64::from(k) / 1000.0, max_keyint_ms / 1000),
            detail: format!(
                "{name} needs a keyframe every {} s; yours is {:.1} s. Set it to 2 s.",
                max_keyint_ms / 1000,
                f64::from(k) / 1000.0
            ),
            fix_kbps: None,
        });
    }
    // The measured rate includes audio (up to about 320 kbps).
    if stats.kbps > max_kbps + 400 {
        let fix = max_kbps.saturating_sub(500);
        let then = if s == Service::Kick { "Kick cuts it off" } else { "it may stutter" };
        out.push(RuleNotice {
            code: RuleCode::Bitrate,
            summary: format!("Over {name}'s {} limit", kbps_text(max_kbps)),
            detail: format!(
                "Over {name}'s {} limit, {then}. Lower the bitrate to {} or less.",
                kbps_text(max_kbps),
                kbps_text(fix),
            ),
            fix_kbps: Some(fix),
        });
    }
    out
}

/// Each platform's rules, held against the stream OBS really sends. Unknown numbers (not
/// streaming yet) are skipped, never guessed.
pub fn rules(stats: &StreamStats, s: Service) -> Vec<String> {
    rules_structured(stats, s, false).into_iter().map(|r| r.detail).collect()
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
            "Upload 12 Mbps, 18 Mbps needed. Send 1 platform, or lower the bitrate to about 3.2 Mbps each."
        );
        let v = verdict(5_000, &[need("Twitch", 6000)]);
        assert_eq!(v.fit_count, 0);
        assert_eq!(v.message, "Upload 5.0 Mbps, 6.0 Mbps needed. Lower the bitrate to about 4.0 Mbps.");
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
        assert_eq!(t[0], "Twitch doesn't take HEVC (H.265). Switch to H.264, or turn Twitch off.");
        assert!(t[1].contains("yours is 8.0 s"));
        assert_eq!(t[2], "Over Twitch's 6,000 kbps limit, it may stutter. Lower the bitrate to 5,500 kbps or less.");
        // YouTube takes HEVC and 9 Mbps, but not an 8 s keyframe gap.
        let y = rules(&hevc, Service::Youtube);
        assert_eq!(y.len(), 1);
        assert_eq!(y[0], "YouTube needs a keyframe every 4 s; yours is 8.0 s. Set it to 2 s.");
        assert!(rules(&hevc, Service::Custom).is_empty());
        // Not streaming yet: nothing known, nothing said.
        assert!(rules(&StreamStats::default(), Service::Twitch).is_empty());
        // A 2.2 s gap is an encoder rounding, not a problem.
        let near = StreamStats { keyframe_interval_ms: Some(2200), ..good };
        assert!(rules(&near, Service::Twitch).is_empty());
        // Over Kick's limit (the 25 Oct test: 12,001 kbps): it says Kick cuts it, and the number to set.
        let fast = StreamStats { kbps: 12_001, ..good };
        let k = rules(&fast, Service::Kick);
        assert_eq!(k.len(), 1);
        assert!(
            k[0] == "Over Kick's 8,000 kbps limit, Kick cuts it off. Lower the bitrate to 7,500 kbps or less.",
            "{k:?}"
        );
        assert!(rules(&fast, Service::Twitch)[0].contains("stutter"));
        // The dashboard's fix button reads the number out of these words (golive.tsx askedKbps:
        // /to ([\d,]+) kbps or less/): they must keep that shape.
        for r in [&k[0], &rules(&fast, Service::Twitch)[0]] {
            let at = r.find(" to ").map(|i| &r[i + 4..]).unwrap_or_default();
            assert!(at.contains(" kbps or less"), "{r}");
        }
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

    #[test]
    fn platform_ceilings_and_structured_rules() {
        let fast = StreamStats {
            kbps: 9_000,
            keyframe_interval_ms: Some(2_000),
            video_codec: Some(VideoCodec::Hevc),
            ..Default::default()
        };
        // Enhanced Broadcasting exempts Twitch from legacy 6,000 / H.264 rules.
        assert!(rules_structured(&fast, Service::Twitch, true).is_empty());
        assert_eq!(ceiling(Service::Twitch, true), None);

        let twitch_issues = rules_structured(&fast, Service::Twitch, false);
        assert_eq!(twitch_issues.len(), 2);
        assert_eq!(twitch_issues[0].code, RuleCode::Codec);
        assert_eq!(twitch_issues[0].fix_kbps, None);
        assert_eq!(twitch_issues[1].code, RuleCode::Bitrate);
        assert_eq!(twitch_issues[1].fix_kbps, Some(5_500));

        // Twitch (6,000) takes precedence over Kick (8,000); YouTube and Twitch EB stay uncapped.
        let limit = effective_limit([(Service::Youtube, false), (Service::Kick, false), (Service::Twitch, false)])
            .expect("capped");
        assert_eq!(limit.service, Service::Twitch);
        assert_eq!(limit.max_kbps, 6_000);

        let cap = capped_info(limit, 14_000, true, 4, false).expect("chip shown");
        assert_eq!(cap.kbps, 6_000);
        assert_eq!(cap.summary, "Capped at 6,000 kbps for Twitch");
        assert!(!cap.separate_encode);

        // Already conforming settings produce no chip.
        assert_eq!(capped_info(limit, 6_000, false, 2, false), None);
    }
}
