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

/// Which picture a destination sends: the main canvas or the upright vertical canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Canvas {
    #[default]
    Main,
    Vertical,
}

/// One tier in a platform's `(resolution, fps) -> bitrate` matrix (OBS `services.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateCap {
    /// Short edge of the video frame (e.g. `1080` for `1920x1080` or `1080x1920`, `720` for `1280x720`).
    pub max_short_edge: u32,
    pub max_fps: u32,
    pub max_kbps: u32,
}

/// Encoding limits for platforms whose ingest rejects, disconnects, or warns above a ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformLimit {
    pub id: &'static str,
    pub service: Service,
    pub name: &'static str,
    /// Hard maximum long edge (`u32::MAX` = no hard edge limit). Portrait and landscape share one check.
    pub max_long_edge: u32,
    /// Hard maximum short edge (`u32::MAX` = no hard edge limit).
    pub max_short_edge: u32,
    /// Hard maximum frame rate (`u32::MAX` = no hard fps limit).
    pub max_fps: u32,
    /// Soft recommended long edge (`None` = no advisory resolution note).
    pub rec_long_edge: Option<u32>,
    /// Soft recommended short edge (`None` = no advisory resolution note).
    pub rec_short_edge: Option<u32>,
    /// Soft recommended frame rate (`None` = no advisory fps note).
    pub rec_fps: Option<u32>,
    pub max_kbps: u32,
    /// Optional `(short_edge, fps)` ceiling matrix sorted ascending by `(max_short_edge, max_fps)`.
    pub bitrate_matrix: &'static [BitrateCap],
    /// Recommended keyframe interval (seconds) when clamping or coalescing encodes.
    pub keyframe_secs: u32,
    /// Maximum keyframe interval (seconds) tolerated before `fit()` clamps to `keyframe_secs`.
    pub max_keyframe_secs: u32,
    pub codecs: &'static [VideoCodec],
    pub max_audio_kbps: u32,
    pub strict_cbr: bool,
    pub force_no_bframes: bool,
}

const CODECS_H264_ONLY: &[VideoCodec] = &[VideoCodec::H264];
const CODECS_H264_HEVC: &[VideoCodec] = &[VideoCodec::H264, VideoCodec::Hevc];
const CODECS_YOUTUBE: &[VideoCodec] = &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];

/// Facebook Live bitrate matrix.
/// Source: verified in OBS `plugins/rtmp-services/data/services.json` ("Facebook Live", `bitrate matrix`).
const FACEBOOK_BITRATE_MATRIX: &[BitrateCap] = &[
    BitrateCap { max_short_edge: 360, max_fps: 30, max_kbps: 1_000 },
    BitrateCap { max_short_edge: 360, max_fps: 60, max_kbps: 1_500 },
    BitrateCap { max_short_edge: 480, max_fps: 30, max_kbps: 2_000 },
    BitrateCap { max_short_edge: 480, max_fps: 60, max_kbps: 3_000 },
    BitrateCap { max_short_edge: 720, max_fps: 30, max_kbps: 4_000 },
    BitrateCap { max_short_edge: 720, max_fps: 60, max_kbps: 6_000 },
    BitrateCap { max_short_edge: 1080, max_fps: 30, max_kbps: 6_000 },
    BitrateCap { max_short_edge: 1080, max_fps: 60, max_kbps: 9_000 },
];

/// Twitch non-Enhanced limit: 6,000 kbps H.264 / 2 s keyint / strict CBR are hard;
/// 1080p @ 60 fps is recommended only (1440p+ stays selectable with an advisory note).
pub const TWITCH_LIMIT: PlatformLimit = PlatformLimit {
    id: "twitch",
    service: Service::Twitch,
    name: "Twitch",
    max_long_edge: u32::MAX,
    max_short_edge: u32::MAX,
    max_fps: u32::MAX,
    rec_long_edge: Some(1920),
    rec_short_edge: Some(1080),
    rec_fps: Some(60),
    max_kbps: 6_000,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 2,
    codecs: CODECS_H264_ONLY,
    max_audio_kbps: 320,
    strict_cbr: true,
    force_no_bframes: false,
};

/// Kick standard ingest limit (Amazon IVS).
/// Sources: 1080p hard drop empirically verified on `fa723fc1b171.global-contribute.live-video.net`;
/// H.264 and keyint 2 s verified in OBS `services.json` ("Amazon IVS"); 60 fps, 8,000 kbps (IVS Standard
/// hard cap 8,500 kbps), and 320 kbps audio max unverified in `services.json`.
pub const KICK_LIMIT: PlatformLimit = PlatformLimit {
    id: "kick",
    service: Service::Kick,
    name: "Kick",
    max_long_edge: 1920,
    max_short_edge: 1080,
    max_fps: 60,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 8_000,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 2,
    codecs: CODECS_H264_ONLY,
    max_audio_kbps: 320,
    strict_cbr: true,
    force_no_bframes: false,
};

/// YouTube Live standard landscape limit.
/// Sources: bitrate 51,000 kbps, rec keyint 2 s (accepts up to 4 s), codecs H.264/HEVC/AV1, audio 160 kbps
/// verified in OBS `services.json` ("YouTube - RTMPS"); 3840x2160 and 60 fps max unverified in `services.json`.
pub const YOUTUBE_LIMIT: PlatformLimit = PlatformLimit {
    id: "youtube",
    service: Service::Youtube,
    name: "YouTube",
    max_long_edge: 3840,
    max_short_edge: 2160,
    max_fps: 60,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 51_000,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 4,
    codecs: CODECS_YOUTUBE,
    max_audio_kbps: 160,
    strict_cbr: false,
    force_no_bframes: false,
};

/// YouTube Shorts Live portrait limit.
/// Sources: keyint, codecs, audio 160 kbps verified in OBS `services.json` ("YouTube - RTMPS");
/// conservative 1080x1920 @ 60 fps / 15,000 kbps vertical ceiling unverified against live Shorts ingest.
pub const YOUTUBE_VERTICAL_LIMIT: PlatformLimit = PlatformLimit {
    id: "youtube_vertical",
    service: Service::Youtube,
    name: "YouTube Shorts",
    max_long_edge: 1920,
    max_short_edge: 1080,
    max_fps: 60,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 15_000,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 4,
    codecs: CODECS_YOUTUBE,
    max_audio_kbps: 160,
    strict_cbr: false,
    force_no_bframes: false,
};

/// Facebook Live limit.
/// Source: 1920x1080, 60 fps, 9,000 kbps + full bitrate matrix, keyint 2 s, H.264, 128 kbps audio
/// all verified in OBS `services.json` ("Facebook Live").
pub const FACEBOOK_LIMIT: PlatformLimit = PlatformLimit {
    id: "facebook",
    service: Service::Custom,
    name: "Facebook",
    max_long_edge: 1920,
    max_short_edge: 1080,
    max_fps: 60,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 9_000,
    bitrate_matrix: FACEBOOK_BITRATE_MATRIX,
    keyframe_secs: 2,
    max_keyframe_secs: 2,
    codecs: CODECS_H264_ONLY,
    max_audio_kbps: 128,
    strict_cbr: true,
    force_no_bframes: false,
};

/// X / Twitter Live limit.
/// Sources: 12,000 kbps, 60 fps, keyint 3 s, H.264, 128 kbps audio verified in OBS `services.json` ("Twitter");
/// 1920x1080 resolution ceiling unverified in `services.json`.
pub const X_LIMIT: PlatformLimit = PlatformLimit {
    id: "x",
    service: Service::Custom,
    name: "X",
    max_long_edge: 1920,
    max_short_edge: 1080,
    max_fps: 60,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 12_000,
    bitrate_matrix: &[],
    keyframe_secs: 3,
    max_keyframe_secs: 3,
    codecs: CODECS_H264_ONLY,
    max_audio_kbps: 128,
    strict_cbr: false,
    force_no_bframes: false,
};

/// Instagram Live limit.
/// Source: unverified (not present in OBS `services.json`); conservative 720x1280 @ 30 fps / 4,000 kbps
/// H.264 / 2 s keyint / 128 kbps audio default, overridable via Custom quality or `ignore_service_limits`.
pub const INSTAGRAM_LIMIT: PlatformLimit = PlatformLimit {
    id: "instagram",
    service: Service::Custom,
    name: "Instagram",
    max_long_edge: 1280,
    max_short_edge: 720,
    max_fps: 30,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: 4_000,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 2,
    codecs: CODECS_H264_ONLY,
    max_audio_kbps: 128,
    strict_cbr: true,
    force_no_bframes: false,
};

/// WebRTC (WHIP) direct protocol constraint (no platform resolution/fps/bitrate cap, B-frames disallowed).
/// Source: verified in `crates/golive/src/direct.rs` and `crates/golive/src/whip.rs`.
pub const WHIP_LIMIT: PlatformLimit = PlatformLimit {
    id: "whip",
    service: Service::Custom,
    name: "WebRTC (WHIP)",
    max_long_edge: u32::MAX,
    max_short_edge: u32::MAX,
    max_fps: u32::MAX,
    rec_long_edge: None,
    rec_short_edge: None,
    rec_fps: None,
    max_kbps: u32::MAX,
    bitrate_matrix: &[],
    keyframe_secs: 2,
    max_keyframe_secs: 2,
    codecs: CODECS_H264_HEVC,
    max_audio_kbps: u32::MAX,
    strict_cbr: false,
    force_no_bframes: true,
};

impl PlatformLimit {
    /// Builds a generic cap from an active shared-stream limit (`StreamCap`).
    pub fn custom_cap(max_kbps: u32, keyframe_secs: u32, h264_only: bool) -> Self {
        Self {
            id: "custom",
            service: Service::Custom,
            name: "Stream",
            max_long_edge: u32::MAX,
            max_short_edge: u32::MAX,
            max_fps: u32::MAX,
            rec_long_edge: None,
            rec_short_edge: None,
            rec_fps: None,
            max_kbps,
            bitrate_matrix: &[],
            keyframe_secs,
            max_keyframe_secs: keyframe_secs,
            codecs: if h264_only { CODECS_H264_ONLY } else { CODECS_YOUTUBE },
            max_audio_kbps: u32::MAX,
            strict_cbr: false,
            force_no_bframes: false,
        }
    }

    /// Returns an advisory note when `width`×`height` @ `fps` exceeds soft recommendations.
    pub fn recommendation_note(&self, width: u32, height: u32, fps: u32) -> Option<String> {
        if let (Some(rl), Some(rs)) = (self.rec_long_edge, self.rec_short_edge)
            && (width.max(height) > rl || width.min(height) > rs)
        {
            return Some(format!("Above {}'s recommended {rs}p", self.name));
        }
        if let Some(rf) = self.rec_fps
            && fps > rf
        {
            return Some(format!("Above {}'s recommended {rf} fps", self.name));
        }
        None
    }

    pub fn is_h264_only(&self) -> bool {
        self.codecs == [VideoCodec::H264]
    }

    /// Portrait- and landscape-safe resolution check (`long <= max_long_edge && short <= max_short_edge`).
    pub fn allows_resolution(&self, width: u32, height: u32) -> bool {
        width > 0 && height > 0 && width.max(height) <= self.max_long_edge && width.min(height) <= self.max_short_edge
    }

    pub fn allows_codec(&self, codec: VideoCodec) -> bool {
        self.codecs.contains(&codec)
    }

    pub fn fallback_codec(&self, preferred: VideoCodec) -> VideoCodec {
        if self.allows_codec(preferred) { preferred } else { self.codecs.first().copied().unwrap_or(VideoCodec::H264) }
    }

    /// Maximum allowed video bitrate (kbps) for `(width, height, fps)`, honoring `bitrate_matrix`.
    pub fn max_kbps_for(&self, width: u32, height: u32, fps: u32) -> u32 {
        if self.bitrate_matrix.is_empty() {
            return self.max_kbps;
        }
        let short = width.min(height);
        self.bitrate_matrix
            .iter()
            .find(|entry| entry.max_short_edge >= short && entry.max_fps >= fps)
            .map_or(self.max_kbps, |entry| entry.max_kbps.min(self.max_kbps))
    }
}

/// Platform limits for `(kind, brand, canvas)`, returning `None` for unbranded custom RTMP/SRT/RIST servers.
pub fn platform_limit(kind: Service, brand: Option<&str>, canvas: Canvas) -> Option<PlatformLimit> {
    match (kind, brand.map(str::trim), canvas) {
        (Service::Twitch, _, _) => Some(TWITCH_LIMIT),
        (Service::Kick, _, _) => Some(KICK_LIMIT),
        (Service::Youtube, _, Canvas::Main) => Some(YOUTUBE_LIMIT),
        (Service::Youtube, _, Canvas::Vertical) => Some(YOUTUBE_VERTICAL_LIMIT),
        (Service::Custom, Some(b), _) if b.eq_ignore_ascii_case("facebook") => Some(FACEBOOK_LIMIT),
        (Service::Custom, Some(b), _) if b.eq_ignore_ascii_case("x") => Some(X_LIMIT),
        (Service::Custom, Some(b), _) if b.eq_ignore_ascii_case("instagram") => Some(INSTAGRAM_LIMIT),
        (Service::Custom, _, _) => None,
    }
}

/// Direct-protocol constraint for a non-RTMP destination (`WHIP` disables B-frames; `SRT`/`RIST` are uncapped).
pub fn direct_limit(kind: crate::direct::Kind) -> Option<PlatformLimit> {
    match kind {
        crate::direct::Kind::Whip => Some(WHIP_LIMIT),
        crate::direct::Kind::Srt | crate::direct::Kind::Rist => None,
    }
}

/// Strict cap for `s` on a single-track main stream (`enhanced` Twitch uses Enhanced Broadcasting
/// tracks instead of the legacy 6,000 kbps ceiling).
pub fn ceiling(s: Service, enhanced: bool) -> Option<PlatformLimit> {
    match s {
        Service::Twitch if !enhanced => Some(TWITCH_LIMIT),
        Service::Kick if !enhanced => Some(KICK_LIMIT),
        Service::Twitch | Service::Kick | Service::Youtube | Service::Custom => None,
    }
}

/// Tightest ceiling across enabled single-track main destinations (`(service, enhanced)`).
pub fn effective_limit<I>(dests: I) -> Option<PlatformLimit>
where
    I: IntoIterator<Item = (Service, bool)>,
{
    dests.into_iter().filter_map(|(s, enh)| ceiling(s, enh)).min_by_key(|c| c.max_kbps)
}

/// Minimum allowed custom video bitrate (kbps).
pub const MIN_CUSTOM_KBPS: u32 = 500;
/// Maximum allowed custom video bitrate (kbps) when unthrottled by a platform limit.
pub const MAX_CUSTOM_KBPS: u32 = 60_000;

/// Per-destination quality mode persisted in `DestinationConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DestinationQuality {
    /// Follow the canvas main stream; auto-fit to platform limits unless `ignore_service_limits`.
    #[default]
    SameAsMain,
    /// Explicit per-destination video output settings.
    Custom(CustomQuality),
}

/// Explicit per-destination video output settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomQuality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    /// `None` inherits the main stream's codec if allowed by the platform, else falls back to H.264.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<VideoCodec>,
}

/// Categorized failure from [`CustomQuality::validate_typed`] distinguishing request-shape/field
/// errors (`400`) from platform ceiling refusals (`422`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomQualityError {
    Field(String),
    PlatformLimit(String),
}

impl CustomQualityError {
    pub fn into_message(self) -> String {
        match self {
            Self::Field(m) | Self::PlatformLimit(m) => m,
        }
    }
}

impl CustomQuality {
    /// Validates user-supplied quality against the canvas bounds and optional platform ceiling,
    /// returning a flat message.
    pub fn validate(
        &self,
        canvas: Canvas,
        canvas_width: u32,
        canvas_height: u32,
        canvas_fps: u32,
        limit: Option<&PlatformLimit>,
        ignore_service_limits: bool,
    ) -> Result<(), String> {
        self.validate_typed(canvas, canvas_width, canvas_height, canvas_fps, limit, ignore_service_limits)
            .map_err(CustomQualityError::into_message)
    }

    /// Validates user-supplied quality while distinguishing field/canvas bounds errors from
    /// platform-limit violations.
    pub fn validate_typed(
        &self,
        canvas: Canvas,
        canvas_width: u32,
        canvas_height: u32,
        canvas_fps: u32,
        limit: Option<&PlatformLimit>,
        ignore_service_limits: bool,
    ) -> Result<(), CustomQualityError> {
        if canvas_width < 2 || canvas_height < 2 || canvas_fps == 0 {
            return Err(CustomQualityError::Field("Canvas resolution and frame rate must be positive.".into()));
        }
        if self.width < 2
            || self.height < 2
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.width.min(self.height) < 72
            || self.width.max(self.height) < 128
        {
            return Err(CustomQualityError::Field(
                "Resolution must use even dimensions of at least 128×72 (or 72×128).".into(),
            ));
        }
        if self.width > canvas_width || self.height > canvas_height {
            return Err(CustomQualityError::Field(format!(
                "Resolution {}×{} cannot exceed the {}×{} canvas.",
                self.width, self.height, canvas_width, canvas_height
            )));
        }
        if canvas == Canvas::Vertical && self.height <= self.width {
            return Err(CustomQualityError::Field(
                "Vertical destinations require portrait dimensions (height > width).".into(),
            ));
        }
        if self.fps == 0 || self.fps > canvas_fps {
            return Err(CustomQualityError::Field(format!(
                "FPS ({}) cannot exceed canvas FPS ({canvas_fps}).",
                self.fps
            )));
        }
        if !canvas_fps.is_multiple_of(self.fps) {
            return Err(CustomQualityError::Field(format!(
                "FPS ({}) must divide canvas FPS ({canvas_fps}) evenly.",
                self.fps
            )));
        }
        if !(MIN_CUSTOM_KBPS..=MAX_CUSTOM_KBPS).contains(&self.kbps) {
            return Err(CustomQualityError::Field(format!(
                "Bitrate must be between {MIN_CUSTOM_KBPS} and {MAX_CUSTOM_KBPS} kbps."
            )));
        }
        if self.codec == Some(VideoCodec::Vp9) {
            return Err(CustomQualityError::Field("VP9 is not supported for streaming output.".into()));
        }
        if !ignore_service_limits && let Some(lim) = limit {
            if !lim.allows_resolution(self.width, self.height) {
                return Err(CustomQualityError::PlatformLimit(format!(
                    "{} accepts at most {}×{} (or {}×{}).",
                    lim.name, lim.max_long_edge, lim.max_short_edge, lim.max_short_edge, lim.max_long_edge
                )));
            }
            if self.fps > lim.max_fps {
                return Err(CustomQualityError::PlatformLimit(format!(
                    "{} accepts at most {} fps.",
                    lim.name, lim.max_fps
                )));
            }
            let max_kbps = lim.max_kbps_for(self.width, self.height, self.fps);
            if self.kbps > max_kbps {
                return Err(CustomQualityError::PlatformLimit(format!(
                    "{} accepts at most {} at {}×{}@{} fps.",
                    lim.name,
                    kbps_text(max_kbps),
                    self.width,
                    self.height,
                    self.fps
                )));
            }
            if let Some(c) = self.codec
                && !lim.allows_codec(c)
            {
                return Err(CustomQualityError::PlatformLimit(format!(
                    "{} does not support {}.",
                    lim.name,
                    codec_name(c)
                )));
            }
        }
        Ok(())
    }
}

/// Canonical encoder identity used to deduplicate identical destination encodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EncodeKey {
    pub canvas: Canvas,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub fps_ntsc: bool,
    pub kbps: u32,
    pub codec: VideoCodec,
    pub keyframe_s: u32,
    pub strict_cbr: bool,
    pub no_bframes: bool,
    pub audio_kbps: u32,
}

impl EncodeKey {
    /// Returns true when `other` matches every encoding property except `strict_cbr`.
    pub fn same_shape(&self, other: &Self) -> bool {
        (
            self.canvas,
            self.width,
            self.height,
            self.fps,
            self.fps_ntsc,
            self.kbps,
            self.codec,
            self.keyframe_s,
            self.no_bframes,
            self.audio_kbps,
        ) == (
            other.canvas,
            other.width,
            other.height,
            other.fps,
            other.fps_ntsc,
            other.kbps,
            other.codec,
            other.keyframe_s,
            other.no_bframes,
            other.audio_kbps,
        )
    }
}

/// Main canvas and stream encode inputs passed to [`fit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainEncode {
    pub canvas: Canvas,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub canvas_fps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub fps_ntsc: bool,
    pub kbps: u32,
    pub codec: VideoCodec,
    pub keyframe_s: u32,
    pub vbr: bool,
    pub strict_cbr: bool,
    pub no_bframes: bool,
    pub audio_kbps: u32,
}

impl MainEncode {
    /// Convenience constructor when main stream size and fps match the canvas.
    pub fn from_canvas(canvas: Canvas, width: u32, height: u32, fps: u32, kbps: u32, codec: VideoCodec) -> Self {
        Self {
            canvas,
            canvas_width: width,
            canvas_height: height,
            canvas_fps: fps,
            width,
            height,
            fps,
            fps_ntsc: false,
            kbps,
            codec,
            keyframe_s: 2,
            vbr: false,
            strict_cbr: false,
            no_bframes: false,
            audio_kbps: 160,
        }
    }

    pub fn as_key(&self) -> EncodeKey {
        EncodeKey {
            canvas: self.canvas,
            width: self.width,
            height: self.height,
            fps: self.fps,
            fps_ntsc: self.fps_ntsc,
            kbps: self.kbps,
            codec: self.codec,
            keyframe_s: self.keyframe_s,
            strict_cbr: self.strict_cbr,
            no_bframes: self.no_bframes,
            audio_kbps: self.audio_kbps,
        }
    }
}

/// Effective encode resolved by [`fit`] for one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedEncode {
    pub key: EncodeKey,
    pub separate_encode: bool,
    pub auto_fitted: bool,
    pub capped: Option<CappedInfo>,
    /// Advisory note when chosen/inherited resolution or fps exceeds a soft platform recommendation.
    pub note: Option<String>,
}

/// Resolves a destination's effective encode from `main`, optional `limit`, and its chosen `quality`.
pub fn fit(
    main: &MainEncode,
    limit: Option<&PlatformLimit>,
    quality: &DestinationQuality,
    ignore_service_limits: bool,
) -> Result<FittedEncode, String> {
    if main.width < 2
        || main.height < 2
        || main.canvas_width < 2
        || main.canvas_height < 2
        || main.fps == 0
        || main.canvas_fps == 0
    {
        return Err("Main encode dimensions and frame rate must be positive.".into());
    }
    match quality {
        DestinationQuality::Custom(q) => {
            q.validate(
                main.canvas,
                main.canvas_width,
                main.canvas_height,
                main.canvas_fps,
                limit,
                ignore_service_limits,
            )?;
            let (codec, keyframe_s, strict_cbr, no_bframes, audio_kbps) = match limit {
                Some(lim) if !ignore_service_limits => (
                    q.codec.unwrap_or_else(|| lim.fallback_codec(main.codec)),
                    fitted_keyframe(main.keyframe_s, lim),
                    main.strict_cbr || (lim.strict_cbr && main.vbr),
                    main.no_bframes || lim.force_no_bframes,
                    main.audio_kbps.min(lim.max_audio_kbps),
                ),
                _ => {
                    (q.codec.unwrap_or(main.codec), main.keyframe_s, main.strict_cbr, main.no_bframes, main.audio_kbps)
                }
            };
            let key = EncodeKey {
                canvas: main.canvas,
                width: q.width,
                height: q.height,
                fps: q.fps,
                fps_ntsc: main.fps_ntsc,
                kbps: q.kbps,
                codec,
                keyframe_s,
                strict_cbr,
                no_bframes,
                audio_kbps,
            };
            let separate_encode = key != main.as_key();
            let note = limit
                .filter(|_| !ignore_service_limits)
                .and_then(|lim| lim.recommendation_note(key.width, key.height, key.fps));
            Ok(FittedEncode { key, separate_encode, auto_fitted: false, capped: None, note })
        }
        DestinationQuality::SameAsMain => {
            let main_key = main.as_key();
            let Some(lim) = limit.filter(|_| !ignore_service_limits) else {
                let (w, h) = ((main.width & !1).max(2), (main.height & !1).max(2));
                let mut key = main_key.clone();
                key.width = w;
                key.height = h;
                let separate_encode = key != main_key;
                return Ok(FittedEncode { key, separate_encode, auto_fitted: false, capped: None, note: None });
            };
            let (w, h) = fit_dimensions(main.width, main.height, lim.max_long_edge, lim.max_short_edge);
            let fps = largest_fps_divisor(main.fps, lim.max_fps);
            let kbps = main.kbps.min(lim.max_kbps_for(w, h, fps));
            let codec = lim.fallback_codec(main.codec);
            let keyframe_s = fitted_keyframe(main.keyframe_s, lim);
            let audio_kbps = main.audio_kbps.min(lim.max_audio_kbps);
            let no_bframes = main.no_bframes || lim.force_no_bframes;
            let cbr_enforced = lim.strict_cbr && main.vbr;
            let note = lim.recommendation_note(w, h, fps);
            let auto_fitted = (w, h) != (main.width, main.height)
                || fps != main.fps
                || kbps != main.kbps
                || codec != main.codec
                || keyframe_s != main.keyframe_s
                || audio_kbps != main.audio_kbps
                || cbr_enforced
                || (no_bframes != main.no_bframes);
            if !auto_fitted {
                return Ok(FittedEncode {
                    key: main_key,
                    separate_encode: false,
                    auto_fitted: false,
                    capped: None,
                    note,
                });
            }
            let key = EncodeKey {
                canvas: main.canvas,
                width: w,
                height: h,
                fps,
                fps_ntsc: main.fps_ntsc,
                kbps,
                codec,
                keyframe_s,
                strict_cbr: main.strict_cbr || lim.strict_cbr,
                no_bframes,
                audio_kbps,
            };
            let separate_encode = key != main_key;
            let capped = Some(build_fit_capped_info(main, lim, &key, separate_encode));
            Ok(FittedEncode { key, separate_encode, auto_fitted: true, capped, note })
        }
    }
}

pub fn fitted_keyframe(main_keyframe_s: u32, lim: &PlatformLimit) -> u32 {
    if main_keyframe_s >= 1 && main_keyframe_s <= lim.max_keyframe_secs {
        main_keyframe_s
    } else if lim.keyframe_secs > 0 {
        lim.keyframe_secs
    } else {
        main_keyframe_s
    }
}

fn build_fit_capped_info(main: &MainEncode, lim: &PlatformLimit, key: &EncodeKey, separate_encode: bool) -> CappedInfo {
    let summary = format!("Auto-fit: {} · {}", format_res_fps(key.width, key.height, key.fps), bitrate_chip(key.kbps));
    let mut max_parts = Vec::new();
    if lim.max_long_edge < u32::MAX && lim.max_short_edge < u32::MAX {
        let res = if key.height > key.width {
            format!("{}×{}@{}", lim.max_short_edge, lim.max_long_edge, lim.max_fps)
        } else {
            format!("{}p{}", lim.max_short_edge, lim.max_fps)
        };
        max_parts.push(res);
    } else if lim.max_fps < u32::MAX {
        max_parts.push(format!("{} fps", lim.max_fps));
    }
    let tier_kbps = lim.max_kbps_for(key.width, key.height, key.fps);
    if tier_kbps < u32::MAX {
        max_parts.push(bitrate_chip(tier_kbps));
    }
    if lim.is_h264_only() && key.codec != main.codec {
        max_parts.push("H.264".into());
    }
    if key.keyframe_s != main.keyframe_s {
        max_parts.push(format!("{} s keyframes", key.keyframe_s));
    }
    if key.audio_kbps != main.audio_kbps {
        max_parts.push(format!("{} kbps audio", key.audio_kbps));
    }
    if lim.strict_cbr && main.vbr {
        max_parts.push("CBR".into());
    }
    let detail = if max_parts.is_empty() {
        format!("Adjusted to fit {}. Encoded separately so your main stream stays unchanged.", lim.name)
    } else {
        format!(
            "{} accepts up to {}. Encoded separately so your main stream keeps its quality.",
            lim.name,
            max_parts.join(" / ")
        )
    };
    CappedInfo {
        kbps: key.kbps,
        resolution: Some((key.width, key.height)),
        fps: Some(key.fps),
        separate_encode,
        summary,
        detail,
    }
}

pub fn fit_dimensions(src_w: u32, src_h: u32, max_long: u32, max_short: u32) -> (u32, u32) {
    let portrait = src_h > src_w;
    let src_long = src_w.max(src_h);
    let src_short = src_w.min(src_h);
    let cap_long = (max_long & !1).min((src_long & !1).max(2));
    let cap_short = (max_short & !1).min((src_short & !1).max(2));
    let (long, short) = if src_long <= max_long && src_short <= max_short {
        (cap_long, cap_short)
    } else if u64::from(max_long) * u64::from(src_short) <= u64::from(max_short) * u64::from(src_long) {
        let long = cap_long;
        let short = round_even_capped(u64::from(src_short) * u64::from(long), u64::from(src_long), cap_short);
        (long, short)
    } else {
        let short = cap_short;
        let long = round_even_capped(u64::from(src_long) * u64::from(short), u64::from(src_short), cap_long);
        (long, short)
    };
    if portrait { (short, long) } else { (long, short) }
}

fn round_even_capped(num: u64, den: u64, cap: u32) -> u32 {
    let max_even = (cap & !1).max(2);
    if den == 0 {
        return max_even;
    }
    let rounded = (num + den / 2) / den;
    let even = if rounded % 2 == 1 {
        let down = rounded.saturating_sub(1);
        let up = rounded + 1;
        let err_down = num.abs_diff(down * den);
        let err_up = (up * den).abs_diff(num);
        if err_up < err_down { up } else { down }
    } else {
        rounded
    };
    u32::try_from(even).unwrap_or(max_even).clamp(2, max_even)
}

pub fn largest_fps_divisor(src_fps: u32, max_fps: u32) -> u32 {
    if src_fps == 0 {
        return if max_fps == 0 { 30 } else { max_fps.min(30) };
    }
    if max_fps == 0 || src_fps <= max_fps {
        return src_fps;
    }
    let min_div = src_fps.div_ceil(max_fps);
    for k in min_div..=src_fps {
        if src_fps.is_multiple_of(k) {
            return (src_fps / k).max(1);
        }
    }
    max_fps
}

pub fn format_res_fps(w: u32, h: u32, fps: u32) -> String {
    let is_standard_16_9 = matches!(
        (w, h),
        (3840, 2160) | (2560, 1440) | (1920, 1080) | (1280, 720) | (852, 480) | (854, 480) | (640, 360)
    );
    if is_standard_16_9 { format!("{h}p{fps}") } else { format!("{w}×{h}@{fps}") }
}

pub fn bitrate_chip(kbps: u32) -> String {
    if kbps.is_multiple_of(1_000) { format!("{} Mbps", kbps / 1_000) } else { mbps(kbps) }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
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
    let codec_forced = limit.is_h264_only() && is_hevc;
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
    Some(CappedInfo { kbps: applied, resolution: None, fps: None, separate_encode: false, summary, detail })
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
pub fn kbps_text(kbps: u32) -> String {
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

    #[test]
    fn m2_kick_honors_enhanced_ignore_service_limits() {
        assert!(ceiling(Service::Kick, false).is_some());
        assert_eq!(ceiling(Service::Kick, true), None);
    }

    #[test]
    fn platform_limit_table_covers_every_supported_platform_and_matrix() {
        struct Case {
            service: Service,
            brand: Option<&'static str>,
            canvas: Canvas,
            expected_id: Option<&'static str>,
            long_edge: u32,
            short_edge: u32,
            max_fps: u32,
            max_kbps: u32,
            keyint: u32,
            max_keyint: u32,
            max_audio: u32,
            strict_cbr: bool,
            codecs: &'static [VideoCodec],
        }
        let cases = [
            Case {
                service: Service::Twitch,
                brand: None,
                canvas: Canvas::Main,
                expected_id: Some("twitch"),
                long_edge: u32::MAX,
                short_edge: u32::MAX,
                max_fps: u32::MAX,
                max_kbps: 6_000,
                keyint: 2,
                max_keyint: 2,
                max_audio: 320,
                strict_cbr: true,
                codecs: &[VideoCodec::H264],
            },
            Case {
                service: Service::Kick,
                brand: None,
                canvas: Canvas::Main,
                expected_id: Some("kick"),
                long_edge: 1920,
                short_edge: 1080,
                max_fps: 60,
                max_kbps: 8_000,
                keyint: 2,
                max_keyint: 2,
                max_audio: 320,
                strict_cbr: true,
                codecs: &[VideoCodec::H264],
            },
            Case {
                service: Service::Youtube,
                brand: None,
                canvas: Canvas::Main,
                expected_id: Some("youtube"),
                long_edge: 3840,
                short_edge: 2160,
                max_fps: 60,
                max_kbps: 51_000,
                keyint: 2,
                max_keyint: 4,
                max_audio: 160,
                strict_cbr: false,
                codecs: &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1],
            },
            Case {
                service: Service::Youtube,
                brand: None,
                canvas: Canvas::Vertical,
                expected_id: Some("youtube_vertical"),
                long_edge: 1920,
                short_edge: 1080,
                max_fps: 60,
                max_kbps: 15_000,
                keyint: 2,
                max_keyint: 4,
                max_audio: 160,
                strict_cbr: false,
                codecs: &[VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1],
            },
            Case {
                service: Service::Custom,
                brand: Some("facebook"),
                canvas: Canvas::Main,
                expected_id: Some("facebook"),
                long_edge: 1920,
                short_edge: 1080,
                max_fps: 60,
                max_kbps: 9_000,
                keyint: 2,
                max_keyint: 2,
                max_audio: 128,
                strict_cbr: true,
                codecs: &[VideoCodec::H264],
            },
            Case {
                service: Service::Custom,
                brand: Some("x"),
                canvas: Canvas::Main,
                expected_id: Some("x"),
                long_edge: 1920,
                short_edge: 1080,
                max_fps: 60,
                max_kbps: 12_000,
                keyint: 3,
                max_keyint: 3,
                max_audio: 128,
                strict_cbr: false,
                codecs: &[VideoCodec::H264],
            },
            Case {
                service: Service::Custom,
                brand: Some("instagram"),
                canvas: Canvas::Vertical,
                expected_id: Some("instagram"),
                long_edge: 1280,
                short_edge: 720,
                max_fps: 30,
                max_kbps: 4_000,
                keyint: 2,
                max_keyint: 2,
                max_audio: 128,
                strict_cbr: true,
                codecs: &[VideoCodec::H264],
            },
            Case {
                service: Service::Custom,
                brand: None,
                canvas: Canvas::Main,
                expected_id: None,
                long_edge: 0,
                short_edge: 0,
                max_fps: 0,
                max_kbps: 0,
                keyint: 0,
                max_keyint: 0,
                max_audio: 0,
                strict_cbr: false,
                codecs: &[],
            },
        ];

        for c in cases {
            let got = platform_limit(c.service, c.brand, c.canvas);
            assert_eq!(got.map(|l| l.id), c.expected_id, "brand={:?} canvas={:?}", c.brand, c.canvas);
            if let Some(lim) = got {
                assert_eq!((lim.max_long_edge, lim.max_short_edge), (c.long_edge, c.short_edge), "{}", lim.id);
                assert_eq!(lim.max_fps, c.max_fps, "{}", lim.id);
                assert_eq!(lim.max_kbps, c.max_kbps, "{}", lim.id);
                assert_eq!((lim.keyframe_secs, lim.max_keyframe_secs), (c.keyint, c.max_keyint), "{}", lim.id);
                assert_eq!(lim.max_audio_kbps, c.max_audio, "{}", lim.id);
                assert_eq!(lim.strict_cbr, c.strict_cbr, "{}", lim.id);
                assert_eq!(lim.codecs, c.codecs, "{}", lim.id);
                // Portrait and landscape symmetry on the same edge limits.
                assert!(lim.allows_resolution(c.long_edge, c.short_edge), "{}", lim.id);
                assert!(lim.allows_resolution(c.short_edge, c.long_edge), "{}", lim.id);
            }
        }

        // Unsupported brands stay unbranded (never support TikTok).
        assert_eq!(platform_limit(Service::Custom, Some("tiktok"), Canvas::Main), None);

        // Facebook bitrate matrix from OBS services.json.
        let fb = platform_limit(Service::Custom, Some("Facebook"), Canvas::Main).unwrap();
        for (w, h, fps, want) in [
            (640, 360, 30, 1_000),
            (640, 360, 60, 1_500),
            (852, 480, 30, 2_000),
            (852, 480, 60, 3_000),
            (1280, 720, 30, 4_000),
            (1280, 720, 60, 6_000),
            (1920, 1080, 30, 6_000),
            (1920, 1080, 60, 9_000),
            (720, 1280, 30, 4_000),
        ] {
            assert_eq!(fb.max_kbps_for(w, h, fps), want, "{w}x{h}@{fps}");
        }

        // Direct protocol limits: WHIP forbids B-frames, SRT/RIST are uncapped.
        let whip = direct_limit(crate::direct::Kind::Whip).unwrap();
        assert!(whip.force_no_bframes);
        assert_eq!(whip.keyframe_secs, 2);
        assert_eq!(whip.codecs, &[VideoCodec::H264, VideoCodec::Hevc]);
        assert_eq!(direct_limit(crate::direct::Kind::Srt), None);
        assert_eq!(direct_limit(crate::direct::Kind::Rist), None);
    }

    #[test]
    fn fit_1440p60_main_auto_fits_kick_twitch_and_keeps_youtube_unchanged() {
        let main = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::Hevc);

        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let kick = fit(&main, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(kick.separate_encode);
        assert!(kick.auto_fitted);
        assert_eq!((kick.key.width, kick.key.height, kick.key.fps, kick.key.kbps), (1920, 1080, 60, 8_000));
        assert_eq!(kick.key.codec, VideoCodec::H264);
        assert!(kick.key.strict_cbr);
        let kick_cap = kick.capped.expect("kick capped chip");
        assert_eq!(kick_cap.resolution, Some((1920, 1080)));
        assert_eq!(kick_cap.fps, Some(60));
        assert_eq!(kick_cap.kbps, 8_000);
        assert!(kick_cap.separate_encode);

        let twitch_lim = platform_limit(Service::Twitch, None, Canvas::Main).unwrap();
        let twitch = fit(&main, Some(&twitch_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(twitch.separate_encode);
        assert!(twitch.auto_fitted);
        assert_eq!((twitch.key.width, twitch.key.height, twitch.key.fps, twitch.key.kbps), (2560, 1440, 60, 6_000));
        assert_eq!(twitch.key.codec, VideoCodec::H264);
        assert_eq!(twitch.note.as_deref(), Some("Above Twitch's recommended 1080p"));
        let twitch_cap = twitch.capped.expect("twitch capped chip");
        assert_eq!(twitch_cap.resolution, Some((2560, 1440)));
        assert_eq!(twitch_cap.fps, Some(60));
        assert_eq!(twitch_cap.kbps, 6_000);
        assert!(twitch_cap.separate_encode);

        let yt_lim = platform_limit(Service::Youtube, None, Canvas::Main).unwrap();
        let yt = fit(&main, Some(&yt_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(!yt.separate_encode);
        assert!(!yt.auto_fitted);
        assert_eq!(yt.key, main.as_key());
        assert_eq!(yt.capped, None);
    }

    #[test]
    fn fit_vertical_9_16_for_shorts_and_instagram() {
        let vert_1080 = MainEncode::from_canvas(Canvas::Vertical, 1080, 1920, 60, 9_000, VideoCodec::H264);

        let shorts_lim = platform_limit(Service::Youtube, None, Canvas::Vertical).unwrap();
        let shorts = fit(&vert_1080, Some(&shorts_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(!shorts.separate_encode);
        assert!(!shorts.auto_fitted);
        assert_eq!((shorts.key.width, shorts.key.height, shorts.key.fps), (1080, 1920, 60));

        let ig_lim = platform_limit(Service::Custom, Some("instagram"), Canvas::Vertical).unwrap();
        let ig = fit(&vert_1080, Some(&ig_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(ig.separate_encode);
        assert!(ig.auto_fitted);
        assert_eq!((ig.key.width, ig.key.height, ig.key.fps, ig.key.kbps), (720, 1280, 30, 4_000));
        assert_eq!(ig.key.audio_kbps, 128);
        let ig_cap = ig.capped.unwrap();
        assert_eq!(ig_cap.resolution, Some((720, 1280)));
        assert_eq!(ig_cap.fps, Some(30));
        assert!(ig_cap.separate_encode);

        // 1440x2560 portrait vertical downscales proportionally to 1080x1920 for YouTube Shorts.
        let vert_1440 = MainEncode::from_canvas(Canvas::Vertical, 1440, 2560, 60, 18_000, VideoCodec::Hevc);
        let shorts_1440 = fit(&vert_1440, Some(&shorts_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(shorts_1440.separate_encode);
        assert_eq!((shorts_1440.key.width, shorts_1440.key.height, shorts_1440.key.kbps), (1080, 1920, 15_000));
        assert_eq!(shorts_1440.key.codec, VideoCodec::Hevc);
    }

    #[test]
    fn fit_ultrawide_21_9_and_odd_sizes_preserve_aspect_with_even_dims_and_no_upscale() {
        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let ig_lim = platform_limit(Service::Custom, Some("instagram"), Canvas::Main).unwrap();

        // 2560x1080 (21:9): long edge 2560 exceeds 1920, so scales to 1920x810.
        let uw_1080 = MainEncode::from_canvas(Canvas::Main, 2560, 1080, 60, 10_000, VideoCodec::H264);
        let fitted_uw_1080 = fit(&uw_1080, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((fitted_uw_1080.key.width, fitted_uw_1080.key.height), (1920, 810));

        // 3440x1440 (21:9): scales to 1920x804 on Kick and 1280x536 on Instagram.
        let uw_1440 = MainEncode::from_canvas(Canvas::Main, 3440, 1440, 60, 16_000, VideoCodec::H264);
        let fitted_uw_kick = fit(&uw_1440, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((fitted_uw_kick.key.width, fitted_uw_kick.key.height), (1920, 804));
        let fitted_uw_ig = fit(&uw_1440, Some(&ig_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((fitted_uw_ig.key.width, fitted_uw_ig.key.height, fitted_uw_ig.key.fps), (1280, 536, 30));

        // Odd sizes above and below limit produce clean even dims without upscaling.
        let odd_above = MainEncode::from_canvas(Canvas::Main, 2561, 1441, 60, 10_000, VideoCodec::H264);
        let f_above = fit(&odd_above, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((f_above.key.width, f_above.key.height), (1920, 1080));
        assert!(f_above.key.width.is_multiple_of(2) && f_above.key.height.is_multiple_of(2));

        let odd_below = MainEncode::from_canvas(Canvas::Main, 1281, 721, 60, 5_000, VideoCodec::H264);
        let f_below = fit(&odd_below, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((f_below.key.width, f_below.key.height), (1280, 720));
        assert!(f_below.key.width <= odd_below.width && f_below.key.height <= odd_below.height);
    }

    #[test]
    fn fit_main_below_limits_stays_same_as_main_without_upscaling() {
        let small = MainEncode::from_canvas(Canvas::Main, 1280, 720, 30, 3_500, VideoCodec::H264);
        for (svc, brand) in [
            (Service::Twitch, None),
            (Service::Kick, None),
            (Service::Youtube, None),
            (Service::Custom, Some("x")),
            (Service::Custom, Some("facebook")),
            (Service::Custom, Some("instagram")),
        ] {
            let lim = platform_limit(svc, brand, Canvas::Main).unwrap();
            let mut m = small;
            if lim.max_audio_kbps < m.audio_kbps {
                m.audio_kbps = lim.max_audio_kbps;
            }
            let out = fit(&m, Some(&lim), &DestinationQuality::SameAsMain, false).unwrap();
            assert!(!out.separate_encode, "{}: {:?}", lim.id, out);
            assert!(!out.auto_fitted, "{}", lim.id);
            assert_eq!(out.key, m.as_key(), "{}", lim.id);
            assert_eq!(out.capped, None, "{}", lim.id);
        }
    }

    #[test]
    fn fit_facebook_matrix_and_x_keyint_and_codec_fallback() {
        let fb_lim = platform_limit(Service::Custom, Some("facebook"), Canvas::Main).unwrap();
        let main_720p30 = MainEncode::from_canvas(Canvas::Main, 1280, 720, 30, 6_000, VideoCodec::Hevc);
        let fb = fit(&main_720p30, Some(&fb_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!(fb.key.kbps, 4_000, "Facebook 720p30 matrix cap");
        assert_eq!(fb.key.codec, VideoCodec::H264, "Facebook falls back from HEVC to H.264");
        assert_eq!(fb.key.audio_kbps, 128);

        // X with 2s keyframes already satisfies X (rec 3s, <=3s ok) so 1080p60@6000 H.264 128kbps stays shared.
        let mut main_x = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        main_x.audio_kbps = 128;
        let x_lim = platform_limit(Service::Custom, Some("x"), Canvas::Main).unwrap();
        let x_out = fit(&main_x, Some(&x_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert!(!x_out.separate_encode);
        assert_eq!(x_out.key.keyframe_s, 2);

        // Custom quality with codec: None inherits main when allowed, falls back to H.264 otherwise.
        let main_hevc = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::Hevc);
        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let yt_lim = platform_limit(Service::Youtube, None, Canvas::Main).unwrap();
        let custom_no_codec =
            DestinationQuality::Custom(CustomQuality { width: 1920, height: 1080, fps: 60, kbps: 8_000, codec: None });
        assert_eq!(fit(&main_hevc, Some(&kick_lim), &custom_no_codec, false).unwrap().key.codec, VideoCodec::H264);
        assert_eq!(fit(&main_hevc, Some(&yt_lim), &custom_no_codec, false).unwrap().key.codec, VideoCodec::Hevc);
    }

    #[test]
    fn custom_quality_validation_rejects_invalid_and_over_limit_inputs() {
        let main = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::H264);
        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let fb_lim = platform_limit(Service::Custom, Some("facebook"), Canvas::Main).unwrap();

        let bad_cases = [
            // Above Kick resolution (1440p > 1080p)
            (Some(&kick_lim), Canvas::Main, 2560, 1440, 60, 8_000, None),
            // Above Kick bitrate (9000 > 8000)
            (Some(&kick_lim), Canvas::Main, 1920, 1080, 60, 9_000, None),
            // Disallowed codec on Kick
            (Some(&kick_lim), Canvas::Main, 1920, 1080, 60, 6_000, Some(VideoCodec::Hevc)),
            // Above Facebook 720p30 matrix tier (5000 > 4000)
            (Some(&fb_lim), Canvas::Main, 1280, 720, 30, 5_000, None),
            // Odd dimensions
            (Some(&kick_lim), Canvas::Main, 1919, 1080, 60, 6_000, None),
            // Upscaling past canvas
            (None, Canvas::Main, 3840, 2160, 60, 12_000, None),
            // Non-divisor fps (45 does not divide 60)
            (Some(&kick_lim), Canvas::Main, 1920, 1080, 45, 6_000, None),
            // Bitrate below minimum
            (Some(&kick_lim), Canvas::Main, 1280, 720, 30, 400, None),
            // Landscape dimensions on Vertical canvas
            (None, Canvas::Vertical, 1280, 720, 30, 4_000, None),
        ];

        for (lim, canvas, width, height, fps, kbps, codec) in bad_cases {
            let mut m = main;
            m.canvas = canvas;
            if canvas == Canvas::Vertical {
                m.canvas_width = 1080;
                m.canvas_height = 1920;
                m.width = 1080;
                m.height = 1920;
            }
            let q = DestinationQuality::Custom(CustomQuality { width, height, fps, kbps, codec });
            assert!(
                fit(&m, lim, &q, false).is_err(),
                "expected rejection for {width}x{height}@{fps} {kbps} kbps {codec:?}"
            );
        }

        // verify Field vs PlatformLimit distinction in validate_typed
        let over_res = CustomQuality { width: 2560, height: 1440, fps: 60, kbps: 8_000, codec: None };
        assert!(matches!(
            over_res.validate_typed(Canvas::Main, 2560, 1440, 60, Some(&kick_lim), false),
            Err(CustomQualityError::PlatformLimit(_))
        ));
        let bad_odd = CustomQuality { width: 1919, height: 1080, fps: 60, kbps: 6_000, codec: None };
        assert!(matches!(
            bad_odd.validate_typed(Canvas::Main, 2560, 1440, 60, Some(&kick_lim), false),
            Err(CustomQualityError::Field(_))
        ));

        // ignore_service_limits allows over-platform custom quality as long as canvas bounds hold.
        let over_kick = DestinationQuality::Custom(CustomQuality {
            width: 2560,
            height: 1440,
            fps: 60,
            kbps: 15_000,
            codec: Some(VideoCodec::Hevc),
        });
        let bypassed = fit(&main, Some(&kick_lim), &over_kick, true).unwrap();
        assert_eq!((bypassed.key.width, bypassed.key.height, bypassed.key.kbps), (2560, 1440, 15_000));
        assert_eq!(bypassed.key.codec, VideoCodec::Hevc);

        // DestinationQuality JSON round-trip matches API contract.
        let raw = serde_json::to_string(&over_kick).unwrap();
        assert!(raw.contains("\"mode\":\"custom\""));
        let decoded: DestinationQuality = serde_json::from_str(&raw).unwrap();
        assert_eq!(decoded, over_kick);
        let same: DestinationQuality = serde_json::from_str(r#"{"mode":"same_as_main"}"#).unwrap();
        assert_eq!(same, DestinationQuality::SameAsMain);

        // EncodeKey deduplicates identical outputs across destinations (e.g. Kick & Custom 1080p60@8000).
        let main_1440 = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::H264);
        let kick_lim2 = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let kick_fit = fit(&main_1440, Some(&kick_lim2), &DestinationQuality::SameAsMain, false).unwrap();
        let mut custom_shared = main_1440;
        custom_shared.strict_cbr = true;
        let custom_q = DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 8_000,
            codec: Some(VideoCodec::H264),
        });
        let custom_fit = fit(&custom_shared, None, &custom_q, false).unwrap();
        let mut keys = std::collections::HashSet::new();
        keys.insert(kick_fit.key);
        assert!(!keys.insert(custom_fit.key), "matching EncodeKeys must deduplicate");
    }

    #[test]
    fn twitch_1080p_is_recommended_while_kick_1080p_is_hard() {
        let main_1440_h264 = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 6_000, VideoCodec::H264);
        let twitch_lim = platform_limit(Service::Twitch, None, Canvas::Main).unwrap();
        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();

        // Twitch 1440p60 stays selectable in Custom with advisory note; >6000 kbps stays rejected.
        let q_1440 = CustomQuality { width: 2560, height: 1440, fps: 60, kbps: 6_000, codec: Some(VideoCodec::H264) };
        let fitted_custom = fit(&main_1440_h264, Some(&twitch_lim), &DestinationQuality::Custom(q_1440.clone()), false)
            .expect("Twitch 1440p custom must be selectable");
        assert_eq!((fitted_custom.key.width, fitted_custom.key.height), (2560, 1440));
        assert_eq!(fitted_custom.note.as_deref(), Some("Above Twitch's recommended 1080p"));

        let q_over_kbps =
            CustomQuality { width: 2560, height: 1440, fps: 60, kbps: 7_000, codec: Some(VideoCodec::H264) };
        assert!(fit(&main_1440_h264, Some(&twitch_lim), &DestinationQuality::Custom(q_over_kbps), false).is_err());

        // Twitch SameAsMain keeps 1440p60 untouched while enforcing hard 6000 kbps / H264 / 2s / CBR.
        let main_1440_hevc = MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::Hevc);
        let twitch_same = fit(&main_1440_hevc, Some(&twitch_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!(
            (twitch_same.key.width, twitch_same.key.height, twitch_same.key.fps, twitch_same.key.kbps),
            (2560, 1440, 60, 6_000)
        );
        assert_eq!(twitch_same.key.codec, VideoCodec::H264);
        assert!(twitch_same.key.strict_cbr);
        assert_eq!(twitch_same.note.as_deref(), Some("Above Twitch's recommended 1080p"));

        // Kick 1080p remains HARD for both Custom and SameAsMain.
        assert!(fit(&main_1440_h264, Some(&kick_lim), &DestinationQuality::Custom(q_1440), false).is_err());
        let kick_same = fit(&main_1440_hevc, Some(&kick_lim), &DestinationQuality::SameAsMain, false).unwrap();
        assert_eq!((kick_same.key.width, kick_same.key.height, kick_same.key.kbps), (1920, 1080, 8_000));
        assert_eq!(kick_same.note, None);
    }

    #[test]
    fn custom_matching_main_cbr_shares_main_without_strict_cbr_split() {
        let main_cbr = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        assert!(!main_cbr.vbr);
        let kick_lim = platform_limit(Service::Kick, None, Canvas::Main).unwrap();
        let q = DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 6_000,
            codec: Some(VideoCodec::H264),
        });
        let fitted = fit(&main_cbr, Some(&kick_lim), &q, false).unwrap();
        assert!(!fitted.separate_encode, "matching CBR Custom must share main instead of splitting strict_cbr");
        assert_eq!(fitted.key, main_cbr.as_key());
    }
}
