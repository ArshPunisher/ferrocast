//! Pure multi-encode planner and PC encode-budget enforcement for per-destination Go live quality.

use std::collections::{BTreeMap, HashMap};

use ferrocast_golive::direct::Kind as DirectKind;
use ferrocast_golive::flv::VideoCodec;
use ferrocast_golive::probe::{
    self, Canvas, CappedInfo, CustomQuality, DestinationQuality, EncodeKey, FittedEncode, MainEncode, PlatformLimit,
    Service,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Canonical group identifier for an active encode pipeline and its [`ferrocast_golive::Hub`].
pub type GroupId = String;

/// Well-known group ID for the uncapped (or Pass-2 shared-cap fallback) main canvas encode.
pub const GROUP_MAIN: &str = "main";
/// Well-known group ID for the vertical canvas encode.
pub const GROUP_VERTICAL: &str = "vertical";
/// Well-known group ID for the on-demand 720p route-fallback encode.
pub const GROUP_LIGHT: &str = "light";
/// Well-known group ID for Twitch Enhanced Broadcasting when its ladder differs from `main`.
pub const GROUP_TWITCH_EB: &str = "twitch_eb";

/// Legacy lighter stream ceilings (720p, <= 3,000 kbps H.264).
pub const LIGHT_MAX_HEIGHT: u32 = 720;
pub const LIGHT_MAX_KBPS: u32 = 3_000;

/// Mpx/s capacity of one Apple Silicon VideoToolbox encode engine (one 4K60 stream).
pub const MPX_PER_ENGINE: f64 = 3840.0 * 2160.0 * 60.0 / 1e6;

/// How the PC encode budget was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSource {
    AppleSilicon,
    Bench,
    NvencDefault,
    HwDefault,
    CpuDefault,
    Unknown,
}

/// Settings DB key under which measured encoder throughput is persisted.
pub const STUDIO_BENCH_KEY: &str = "studio_bench";

/// Persisted benchmark capacity stored under `settings["studio_bench"]`, invalidated when
/// hardware/driver signature (`host_sig`) or encoder family (`hw` vs `x264`) changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedStudioBench {
    pub encoder_id: String,
    #[serde(default)]
    pub is_software: bool,
    pub max_mpx_s: f64,
    #[serde(default)]
    pub max_sessions_ok: Option<u32>,
    pub measured_at: i64,
    pub host_sig: String,
}

impl PersistedStudioBench {
    /// Returns true if this benchmark measurement applies to `host_sig` and the active encoder class.
    pub fn is_valid_for(&self, host_sig: &str, stream_is_software: bool) -> bool {
        !self.host_sig.is_empty()
            && self.host_sig == host_sig
            && self.is_software == stream_is_software
            && self.max_mpx_s.is_finite()
            && self.max_mpx_s > 0.0
    }
}

/// Returns true if `encoder_id` runs software CPU encoding (`libx264` / `obs_x264`) rather than GPU hardware.
pub fn encoder_is_software(encoder_id: &str) -> bool {
    let low = encoder_id.to_ascii_lowercase();
    low.contains("x264") || low.contains("software") || low.contains("svt")
}

/// Conservative concurrent NVENC session cap from the NVIDIA driver branch (`551.23+ -> 8`, `531+ -> 5`, else `3`).
pub fn nvenc_session_cap(major: Option<u32>, minor: Option<u32>) -> u32 {
    match major {
        Some(maj) if maj > 551 || (maj == 551 && minor.unwrap_or(0) >= 23) => 8,
        Some(maj) if maj >= 531 => 5,
        _ => 3,
    }
}

fn cpu_default_mpx_s(cpu_cores: u32) -> f64 {
    if cpu_cores < 8 {
        125.0
    } else if cpu_cores < 12 {
        180.0
    } else {
        250.0
    }
}

fn cpu_default_sessions(cpu_cores: u32) -> u32 {
    if cpu_cores < 8 {
        1
    } else if cpu_cores < 16 {
        2
    } else {
        3
    }
}

pub(crate) fn encoder_is_nvenc(info: &crate::studio_host::HostBudgetInfo) -> bool {
    let enc = info.encoder_id.to_ascii_lowercase();
    enc.contains("nvenc")
        || info.hw_backend.as_deref() == Some("nvenc")
        || (info.hw_backend.is_none()
            && !enc.contains("qsv")
            && !enc.contains("amf")
            && !enc.contains("vaapi")
            && !enc.contains("videotoolbox")
            && info.nvidia_driver_major.is_some())
}

/// Overall encode budget status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    Ok,
    Limit,
    Over,
    Unknown,
}

/// Hardware / CPU encoding budget constraints passed into [`plan_encodes`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PcBudget {
    pub source: BudgetSource,
    pub limit_mpx_s: Option<f64>,
    pub max_sessions: Option<u32>,
    /// True when FFmpeg process handover (`ffenc.rs`) needs one spare session reserved under `auto_quality`.
    pub reserve_spare_session: bool,
    /// Mpx/s consumed outside stream encode groups (e.g. independent recording and replay buffer).
    pub extra_mpx_s: f64,
    /// Concurrent encoder sessions consumed outside stream encode groups.
    pub extra_sessions: u32,
    /// True when an active `"Same as stream"` recording shares `"main"` (0 extra if `"main"` exists,
    /// or 1 extra `"main"` encode if no stream destination uses `"main"`).
    pub record_shares_main: bool,
    /// True when stream groups run on software x264 while `source` tracks hardware engine budget.
    pub stream_is_software: bool,
    pub note: Option<String>,
}

impl Default for PcBudget {
    fn default() -> Self {
        Self::unknown()
    }
}

impl PcBudget {
    pub fn unknown() -> Self {
        Self {
            source: BudgetSource::Unknown,
            limit_mpx_s: None,
            max_sessions: None,
            reserve_spare_session: false,
            extra_mpx_s: 0.0,
            extra_sessions: 0,
            record_shares_main: false,
            stream_is_software: false,
            note: None,
        }
    }

    pub fn apple_silicon(engines: u32) -> Self {
        let e = engines.max(1);
        Self {
            source: BudgetSource::AppleSilicon,
            limit_mpx_s: Some(f64::from(e) * MPX_PER_ENGINE),
            max_sessions: Some(e * 4),
            reserve_spare_session: false,
            extra_mpx_s: 0.0,
            extra_sessions: 0,
            record_shares_main: false,
            stream_is_software: false,
            note: None,
        }
    }

    pub fn with_limits(source: BudgetSource, limit_mpx_s: Option<f64>, max_sessions: Option<u32>) -> Self {
        Self { source, limit_mpx_s, max_sessions, ..Self::unknown() }
    }

    /// Computes cross-OS encode budget from host hardware telemetry and optional persisted `studio_bench`.
    /// Reserves one spare hardware session on `ffenc` backends when `auto_quality` is active because
    /// mid-stream bitrate handover spawns a second FFmpeg encoder until the next IDR frame.
    pub fn from_host_info(
        info: &crate::studio_host::HostBudgetInfo,
        bench: Option<&PersistedStudioBench>,
        auto_quality: bool,
    ) -> Self {
        let stream_is_software = encoder_is_software(&info.encoder_id);
        let valid_bench = bench.filter(|b| b.is_valid_for(&info.host_sig, stream_is_software));

        let base = if stream_is_software {
            let cores = info.cpu_cores.max(1);
            let default_sessions = cpu_default_sessions(cores);
            let sessions = valid_bench
                .and_then(|b| b.max_sessions_ok)
                .map_or(default_sessions, |m| m.min(default_sessions).max(1));
            Self {
                source: if valid_bench.is_some() { BudgetSource::Bench } else { BudgetSource::CpuDefault },
                limit_mpx_s: Some(valid_bench.map_or_else(|| cpu_default_mpx_s(cores), |b| b.max_mpx_s)),
                max_sessions: Some(sessions),
                reserve_spare_session: false,
                stream_is_software: true,
                note: valid_bench.is_none().then(|| {
                    "Estimated CPU encode budget (run Auto-Config benchmark to measure exact x264 headroom).".into()
                }),
                ..Self::unknown()
            }
        } else if info.os == "macos" {
            if let Some(engines) = info.engines.filter(|&e| e > 0) {
                Self::apple_silicon(engines)
            } else {
                Self {
                    source: if valid_bench.is_some() { BudgetSource::Bench } else { BudgetSource::HwDefault },
                    limit_mpx_s: Some(valid_bench.map_or(125.0, |b| b.max_mpx_s)),
                    max_sessions: Some(valid_bench.and_then(|b| b.max_sessions_ok).map_or(2, |m| m.clamp(1, 2))),
                    reserve_spare_session: false,
                    stream_is_software: false,
                    note: valid_bench.is_none().then(|| {
                        "Estimated Intel Mac hardware budget (run Auto-Config benchmark to measure capacity).".into()
                    }),
                    ..Self::unknown()
                }
            }
        } else if matches!(info.os.as_str(), "windows" | "linux") {
            let is_nvenc = encoder_is_nvenc(info);
            let max_sess =
                if is_nvenc { nvenc_session_cap(info.nvidia_driver_major, info.nvidia_driver_minor) } else { 3 };
            let bench_or_default = valid_bench.map_or(250.0, |b| b.max_mpx_s);
            let sessions = valid_bench.and_then(|b| b.max_sessions_ok).map_or(max_sess, |m| m.min(max_sess).max(1));
            Self {
                source: if valid_bench.is_some() {
                    BudgetSource::Bench
                } else if is_nvenc {
                    BudgetSource::NvencDefault
                } else {
                    BudgetSource::HwDefault
                },
                limit_mpx_s: Some(bench_or_default),
                max_sessions: Some(sessions),
                reserve_spare_session: auto_quality,
                stream_is_software: false,
                note: valid_bench.is_none().then(|| {
                    if is_nvenc {
                        "Estimated NVENC budget; run Auto-Config benchmark for exact card capacity.".into()
                    } else {
                        "Estimated hardware encoder budget; run Auto-Config benchmark for exact capacity.".into()
                    }
                }),
                ..Self::unknown()
            }
        } else {
            Self::unknown()
        };

        Self {
            extra_mpx_s: info.record_extra_mpx_s,
            extra_sessions: info.record_extra_sessions,
            record_shares_main: info.record_shares_main,
            ..base
        }
    }

    /// Maximum concurrent stream/recording encoder sessions after reserving a handover spare if requested.
    pub fn effective_max_sessions(&self) -> Option<u32> {
        self.max_sessions.map(|m| if self.reserve_spare_session && m > 1 { m - 1 } else { m })
    }
}

/// Picks secondary Twitch Enhanced Broadcasting tracks strictly smaller than `out_h`.
pub(crate) fn select_eb_extra_tracks(plan: &ferrocast_golive::twitch_eb::Plan, out_h: u32) -> Vec<(u32, u32, u32)> {
    plan.encodes
        .iter()
        .skip(1)
        .filter(|e| {
            e.height < out_h && e.width >= 2 && e.height >= 2 && e.width.is_multiple_of(2) && e.height.is_multiple_of(2)
        })
        .map(|e| (e.width, e.height, e.kbps().max(300)))
        .take(4)
        .collect()
}

/// Twitch Enhanced Broadcasting ladder specification used during planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwitchEbSpec {
    pub top_key: EncodeKey,
    pub extra_tracks: Vec<(u32, u32, u32)>,
}

impl TwitchEbSpec {
    /// Builds a spec from a fetched Twitch Enhanced Broadcasting plan, or `None` if the plan has no encodes.
    pub fn from_plan(plan: &ferrocast_golive::twitch_eb::Plan, main: &MainEncode) -> Option<Self> {
        let enc0 = plan.encodes.first()?;
        let width = (enc0.width & !1).max(2);
        let height = (enc0.height & !1).max(2);
        let fps = enc0.framerate.map(|f| (f.numerator / f.denominator.max(1)).max(1)).unwrap_or(main.fps);
        let codec = codec_from_encoder_name(&enc0.kind).unwrap_or(VideoCodec::H264);
        let kbps = if enc0.kbps() > 0 { enc0.kbps() } else { main.kbps.min(probe::TWITCH_LIMIT.max_kbps) };
        let keyframe_s = enc0
            .settings
            .get("keyint_sec")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&k| k > 0)
            .unwrap_or(2);
        let top_key = EncodeKey {
            canvas: Canvas::Main,
            width,
            height,
            fps,
            fps_ntsc: main.fps_ntsc,
            kbps,
            codec,
            keyframe_s,
            strict_cbr: main.strict_cbr,
            no_bframes: main.no_bframes,
            audio_kbps: main.audio_kbps,
        };
        let extra_tracks = select_eb_extra_tracks(plan, enc0.height);
        Some(Self { top_key, extra_tracks })
    }

    /// Pre-stream estimate before `GetClientConfiguration` has been fetched.
    pub fn estimated(main: &MainEncode) -> Self {
        let fitted = probe::fit(main, Some(&probe::TWITCH_LIMIT), &DestinationQuality::SameAsMain, false);
        let top_key = fitted.map_or_else(|_| main.as_key(), |f| f.key);
        let extra_tracks =
            [(1280, 720, 3_000), (852, 480, 1_500)].into_iter().filter(|&(_, h, _)| h < top_key.height).collect();
        Self { top_key, extra_tracks }
    }
}

fn codec_from_encoder_name(kind: &str) -> Option<VideoCodec> {
    let low = kind.to_ascii_lowercase();
    if low.contains("hevc") || low.contains("h265") {
        Some(VideoCodec::Hevc)
    } else if low.contains("av1") {
        Some(VideoCodec::Av1)
    } else if low.contains("h264") || low.contains("avc") || low.contains("x264") {
        Some(VideoCodec::H264)
    } else {
        None
    }
}

/// Destination description supplied to [`plan_encodes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerDestination {
    pub id: String,
    pub name: String,
    pub kind: Service,
    pub brand: Option<String>,
    pub canvas: Canvas,
    pub enabled: bool,
    pub can_start: bool,
    pub standby: bool,
    pub direct: Option<DirectKind>,
    pub light: bool,
    pub enhanced: bool,
    pub route_light: bool,
    pub quality: DestinationQuality,
    pub vod_track: bool,
}

/// Inputs to [`plan_encodes`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlannerInput {
    pub main: MainEncode,
    pub vertical: Option<MainEncode>,
    pub vertical_cut: bool,
    pub destinations: Vec<PlannerDestination>,
    pub ignore_service_limits: bool,
    pub twitch_eb: Option<TwitchEbSpec>,
    pub whip_simulcast: bool,
    pub budget: PcBudget,
}

/// One deduplicated encode pipeline in [`EncodePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeGroup {
    pub id: GroupId,
    pub key: EncodeKey,
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_tracks: Vec<(u32, u32, u32)>,
    pub vod_track: bool,
    pub is_light: bool,
    pub is_twitch_eb: bool,
}

/// Effective encode decision for a single destination in [`EncodePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDestination {
    pub id: String,
    pub group_id: GroupId,
    pub key: EncodeKey,
    pub separate_encode: bool,
    pub auto_fitted: bool,
    pub coalesced: bool,
    pub shared_cap_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped: Option<CappedInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
    pub on_light: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Measured PC budget check returned alongside an [`EncodePlan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetCheck {
    pub status: BudgetStatus,
    pub source: BudgetSource,
    pub used_mpx_s: f64,
    pub limit_mpx_s: Option<f64>,
    pub used_sessions: u32,
    pub max_sessions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Complete encode grouping plan returned by [`plan_encodes`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodePlan {
    pub main: EncodeKey,
    pub vertical: Option<EncodeKey>,
    pub groups: Vec<EncodeGroup>,
    pub destinations: HashMap<String, PlannedDestination>,
    pub budget: BudgetCheck,
}

impl EncodePlan {
    /// Returns the planned [`EncodeGroup`] for destination `dest_id`, if active.
    pub fn group_for_destination(&self, dest_id: &str) -> Option<&EncodeGroup> {
        let planned = self.destinations.get(dest_id)?;
        self.groups.iter().find(|g| g.id == planned.group_id)
    }

    /// Returns true if any active destination or recording requires the `"main"` encode group.
    pub fn has_main_group(&self) -> bool {
        self.groups.iter().any(|g| g.id == GROUP_MAIN)
    }
}

/// Deterministic group ID for an [`EncodeKey`] (collision-free across all key fields).
pub fn group_id_for_key(key: &EncodeKey) -> GroupId {
    let mut hasher = Sha256::new();
    let canvas_tag = match key.canvas {
        Canvas::Main => 0u8,
        Canvas::Vertical => 1u8,
    };
    let codec_tag = match key.codec {
        VideoCodec::H264 => 0u8,
        VideoCodec::Hevc => 1u8,
        VideoCodec::Av1 => 2u8,
        VideoCodec::Vp9 => 3u8,
    };
    hasher.update([canvas_tag]);
    hasher.update(key.width.to_be_bytes());
    hasher.update(key.height.to_be_bytes());
    hasher.update(key.fps.to_be_bytes());
    hasher.update([u8::from(key.fps_ntsc)]);
    hasher.update(key.kbps.to_be_bytes());
    hasher.update([codec_tag]);
    hasher.update(key.keyframe_s.to_be_bytes());
    hasher.update([u8::from(key.no_bframes)]);
    hasher.update(key.audio_kbps.to_be_bytes());
    let digest = hasher.finalize();
    let short_hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let prefix = if key.canvas == Canvas::Vertical { "g_v" } else { "g" };
    let codec_str = match key.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
        VideoCodec::Vp9 => "vp9",
    };
    format!("{prefix}_{}x{}_{}_{}_{}_{}", key.width, key.height, key.fps, key.kbps, codec_str, short_hex)
}

/// Computes the legacy 720p <= 3,000 kbps H.264 [`CustomQuality`] for a main canvas destination.
pub fn legacy_light_custom_quality(
    main: &MainEncode,
    limit: Option<&PlatformLimit>,
    ignore_service_limits: bool,
) -> CustomQuality {
    let src_w = (main.width & !1).max(2);
    let src_h = (main.height & !1).max(2);
    let (mut w, mut h) = if src_h > LIGHT_MAX_HEIGHT {
        let scaled_w = ((u64::from(src_w) * u64::from(LIGHT_MAX_HEIGHT)) / u64::from(src_h)) as u32;
        ((scaled_w & !1).clamp(128, src_w), LIGHT_MAX_HEIGHT)
    } else {
        (src_w.max(128), src_h.max(72))
    };
    let mut fps = main.fps.max(1);
    let mut kbps = main.kbps.clamp(probe::MIN_CUSTOM_KBPS, LIGHT_MAX_KBPS);
    if !ignore_service_limits && let Some(lim) = limit {
        (w, h) = probe::fit_dimensions(w, h, lim.max_long_edge, lim.max_short_edge);
        fps = probe::largest_fps_divisor(fps, lim.max_fps);
        kbps = kbps.min(lim.max_kbps_for(w, h, fps)).max(probe::MIN_CUSTOM_KBPS);
    }
    CustomQuality { width: w, height: h, fps, kbps, codec: Some(VideoCodec::H264) }
}

/// Canonical [`EncodeKey`] for the on-demand `"light"` route-fallback stream.
pub fn light_encode_key(main: &MainEncode, any_whip: bool) -> EncodeKey {
    let q = legacy_light_custom_quality(main, None, true);
    EncodeKey {
        canvas: Canvas::Main,
        width: q.width,
        height: q.height,
        fps: q.fps,
        fps_ntsc: main.fps_ntsc,
        kbps: q.kbps,
        codec: VideoCodec::H264,
        keyframe_s: main.keyframe_s.clamp(1, 2),
        strict_cbr: main.strict_cbr,
        no_bframes: main.no_bframes || any_whip,
        audio_kbps: main.audio_kbps,
    }
}

/// Returns true when `key` carries a higher bitrate than `light` so switching to `"light"` saves bandwidth.
pub fn is_heavier_than_light(key: &EncodeKey, light: &EncodeKey) -> bool {
    key.canvas == Canvas::Main && key.kbps > light.kbps
}

fn default_vertical_base(main: &MainEncode) -> MainEncode {
    MainEncode {
        canvas: Canvas::Vertical,
        canvas_width: 1080,
        canvas_height: 1920,
        canvas_fps: main.canvas_fps,
        width: 1080,
        height: 1920,
        fps: main.fps,
        fps_ntsc: main.fps_ntsc,
        kbps: main.kbps,
        codec: main.codec,
        keyframe_s: main.keyframe_s,
        vbr: main.vbr,
        strict_cbr: main.strict_cbr,
        no_bframes: main.no_bframes,
        audio_kbps: main.audio_kbps,
    }
}

fn destination_limit(d: &PlannerDestination) -> Option<PlatformLimit> {
    match d.direct {
        Some(dk) => probe::direct_limit(dk),
        None => probe::platform_limit(d.kind, d.brand.as_deref(), d.canvas),
    }
}

fn effective_ignore_limits(d: &PlannerDestination, ignore_service_limits: bool) -> bool {
    // WebRTC (WHIP) cannot transport B-frames regardless of `ignore_service_limits`.
    if d.direct == Some(DirectKind::Whip) { false } else { ignore_service_limits }
}

fn clamp_custom_for_fit(
    q: &CustomQuality,
    base: &MainEncode,
    limit: Option<&PlatformLimit>,
    ignore_limits: bool,
) -> CustomQuality {
    let max_w = (base.canvas_width & !1).max(72);
    let max_h = (base.canvas_height & !1).max(72);
    let (mut w, mut h) = if q.width <= max_w && q.height <= max_h {
        ((q.width & !1).max(2), (q.height & !1).max(2))
    } else {
        let scale = (f64::from(max_w) / f64::from(q.width.max(2))).min(f64::from(max_h) / f64::from(q.height.max(2)));
        let sw = (((f64::from(q.width) * scale).floor() as u32) & !1).clamp(2, max_w);
        let sh = (((f64::from(q.height) * scale).floor() as u32) & !1).clamp(2, max_h);
        (sw, sh)
    };
    if w.min(h) < 72 || w.max(h) < 128 {
        (w, h) = (base.width.min(max_w), base.height.min(max_h));
    }
    if base.canvas == Canvas::Vertical && h <= w {
        (w, h) = (base.width.min(base.height), base.width.max(base.height));
    }
    let mut fps = probe::largest_fps_divisor(base.canvas_fps.max(1), q.fps.max(1));
    let mut kbps = q.kbps.clamp(probe::MIN_CUSTOM_KBPS, probe::MAX_CUSTOM_KBPS);
    let mut codec = q.codec.filter(|c| *c != VideoCodec::Vp9);
    if !ignore_limits && let Some(lim) = limit {
        (w, h) = probe::fit_dimensions(w, h, lim.max_long_edge, lim.max_short_edge);
        fps = probe::largest_fps_divisor(base.canvas_fps.max(1), fps.min(lim.max_fps));
        kbps = kbps.min(lim.max_kbps_for(w, h, fps)).max(probe::MIN_CUSTOM_KBPS);
        if let Some(c) = codec
            && !lim.allows_codec(c)
        {
            codec = Some(lim.fallback_codec(c));
        }
    }
    CustomQuality { width: w, height: h, fps, kbps, codec }
}

fn resolve_normal_fit(
    d: &PlannerDestination,
    base: &MainEncode,
    vertical_cut: bool,
    ignore_service_limits: bool,
) -> (FittedEncode, Option<PlatformLimit>) {
    if d.canvas == Canvas::Vertical && vertical_cut {
        return (
            FittedEncode { key: base.as_key(), separate_encode: false, auto_fitted: false, capped: None, note: None },
            None,
        );
    }
    let limit = destination_limit(d);
    let ignore = effective_ignore_limits(d, ignore_service_limits);
    let effective_quality =
        if d.quality == DestinationQuality::SameAsMain && d.light && d.canvas == Canvas::Main && d.direct.is_none() {
            DestinationQuality::Custom(legacy_light_custom_quality(base, limit.as_ref(), ignore))
        } else {
            d.quality.clone()
        };
    let fitted = match probe::fit(base, limit.as_ref(), &effective_quality, ignore) {
        Ok(f) => f,
        Err(_) => match &effective_quality {
            DestinationQuality::Custom(q) => {
                let clamped = DestinationQuality::Custom(clamp_custom_for_fit(q, base, limit.as_ref(), ignore));
                probe::fit(base, limit.as_ref(), &clamped, ignore).unwrap_or_else(|_| FittedEncode {
                    key: base.as_key(),
                    separate_encode: false,
                    auto_fitted: false,
                    capped: None,
                    note: None,
                })
            }
            DestinationQuality::SameAsMain => FittedEncode {
                key: base.as_key(),
                separate_encode: false,
                auto_fitted: false,
                capped: None,
                note: None,
            },
        },
    };
    (fitted, limit)
}

#[derive(Debug, Clone)]
struct WorkingDest {
    input: PlannerDestination,
    group_id: GroupId,
    key: EncodeKey,
    separate_encode: bool,
    auto_fitted: bool,
    coalesced: bool,
    shared_cap_fallback: bool,
    capped: Option<CappedInfo>,
    note: Option<String>,
    is_same_as_main: bool,
    is_auto_fitted_same_as_main: bool,
    is_eb: bool,
    is_manual_light: bool,
    on_light: bool,
    limit: Option<PlatformLimit>,
}

#[derive(Debug, Clone)]
struct WorkingGroup {
    id: GroupId,
    key: EncodeKey,
    members: Vec<usize>,
    extra_tracks: Vec<(u32, u32, u32)>,
    vod_track: bool,
    is_light: bool,
    is_twitch_eb: bool,
}

fn mpx_s(w: u32, h: u32, fps: u32) -> f64 {
    f64::from(w) * f64::from(h) * f64::from(fps) / 1e6
}

struct BudgetRow<'a> {
    id: &'a str,
    key: &'a EncodeKey,
    extra_tracks: &'a [(u32, u32, u32)],
    vod_track: bool,
}

fn evaluate_budget_rows<'a>(
    rows: impl Iterator<Item = BudgetRow<'a>>,
    main: &MainEncode,
    budget: &PcBudget,
) -> BudgetCheck {
    let mut used_mpx_s = budget.extra_mpx_s;
    let mut used_sessions = budget.extra_sessions;
    let mut main_shares_with_record = false;
    let stream_counted =
        !(budget.stream_is_software && !matches!(budget.source, BudgetSource::CpuDefault | BudgetSource::Bench));
    if stream_counted {
        for row in rows {
            used_mpx_s += mpx_s(row.key.width, row.key.height, row.key.fps);
            used_sessions = used_sessions.saturating_add(1);
            for &(tw, th, _) in row.extra_tracks {
                used_mpx_s += mpx_s(tw, th, row.key.fps);
                used_sessions = used_sessions.saturating_add(1);
            }
            if row.id == GROUP_MAIN
                && row.extra_tracks.is_empty()
                && !row.vod_track
                && row.key.width == main.canvas_width
                && row.key.height == main.canvas_height
                && row.key.fps == main.canvas_fps
            {
                main_shares_with_record = true;
            }
        }
        if budget.record_shares_main && !main_shares_with_record {
            used_mpx_s += mpx_s(main.canvas_width, main.canvas_height, main.canvas_fps);
            used_sessions = used_sessions.saturating_add(1);
        }
    }
    let eff_max = budget.effective_max_sessions();
    let status = if budget.source == BudgetSource::Unknown && budget.limit_mpx_s.is_none() && eff_max.is_none() {
        BudgetStatus::Unknown
    } else if budget.limit_mpx_s.is_some_and(|l| used_mpx_s > l) || eff_max.is_some_and(|m| used_sessions > m) {
        BudgetStatus::Over
    } else if budget.limit_mpx_s.is_some_and(|l| used_mpx_s > l * 0.9) {
        BudgetStatus::Limit
    } else {
        BudgetStatus::Ok
    };
    BudgetCheck {
        status,
        source: budget.source,
        used_mpx_s: (used_mpx_s * 10.0).round() / 10.0,
        limit_mpx_s: budget.limit_mpx_s.map(|l| (l * 10.0).round() / 10.0),
        used_sessions,
        max_sessions: eff_max,
        note: budget.note.clone(),
    }
}

fn evaluate_budget(groups: &[WorkingGroup], main: &MainEncode, budget: &PcBudget) -> BudgetCheck {
    evaluate_budget_rows(
        groups.iter().map(|g| BudgetRow {
            id: &g.id,
            key: &g.key,
            extra_tracks: &g.extra_tracks,
            vod_track: g.vod_track,
        }),
        main,
        budget,
    )
}

/// Evaluates steady-state `plan` combined with any still-running or reserved encode groups not yet in `plan`.
pub(crate) fn evaluate_plan_with_extra_groups(
    plan: &EncodePlan,
    main: &MainEncode,
    budget: &PcBudget,
    extra_groups: &[crate::studio_host::EncodeGroupSpec],
) -> BudgetCheck {
    let plan_rows = plan.groups.iter().map(|g| BudgetRow {
        id: &g.id,
        key: &g.key,
        extra_tracks: &g.extra_tracks,
        vod_track: g.vod_track,
    });
    let extra_rows = extra_groups.iter().filter(|spec| !plan.groups.iter().any(|g| g.id == spec.id)).map(|spec| {
        BudgetRow { id: &spec.id, key: &spec.key, extra_tracks: &spec.extra_tracks, vod_track: spec.vod_track }
    });
    evaluate_budget_rows(plan_rows.chain(extra_rows), main, budget)
}

fn rebuild_groups(dests: &[WorkingDest], eb_extra: &[(u32, u32, u32)]) -> Vec<WorkingGroup> {
    let mut order: Vec<GroupId> = Vec::new();
    let mut by_id: HashMap<GroupId, WorkingGroup> = HashMap::new();
    for (idx, d) in dests.iter().enumerate() {
        if let Some(g) = by_id.get_mut(&d.group_id) {
            g.members.push(idx);
            g.key.strict_cbr |= d.key.strict_cbr;
            g.vod_track |= d.input.vod_track;
            if d.is_eb {
                g.is_twitch_eb = true;
                if g.extra_tracks.is_empty() {
                    g.extra_tracks = eb_extra.to_vec();
                }
            }
            if d.on_light {
                g.is_light = true;
            }
        } else {
            order.push(d.group_id.clone());
            by_id.insert(
                d.group_id.clone(),
                WorkingGroup {
                    id: d.group_id.clone(),
                    key: d.key.clone(),
                    members: vec![idx],
                    extra_tracks: if d.is_eb { eb_extra.to_vec() } else { Vec::new() },
                    vod_track: d.input.vod_track,
                    is_light: d.on_light,
                    is_twitch_eb: d.is_eb,
                },
            );
        }
    }
    order.into_iter().filter_map(|id| by_id.remove(&id)).collect()
}

fn group_is_auto_fitted_only(g: &WorkingGroup, dests: &[WorkingDest]) -> bool {
    !g.is_twitch_eb
        && !g.is_light
        && !g.members.is_empty()
        && g.members.iter().all(|&i| dests[i].is_auto_fitted_same_as_main)
}

fn merged_compatible_key(a: &EncodeKey, b: &EncodeKey) -> Option<EncodeKey> {
    if (a.canvas, a.fps_ntsc, a.codec) != (b.canvas, b.fps_ntsc, b.codec) {
        return None;
    }
    let a_px = u64::from(a.width) * u64::from(a.height);
    let b_px = u64::from(b.width) * u64::from(b.height);
    let (width, height) = if a_px <= b_px { (a.width, a.height) } else { (b.width, b.height) };
    if width > a.width || height > a.height || width > b.width || height > b.height {
        return None;
    }
    Some(EncodeKey {
        canvas: a.canvas,
        width,
        height,
        fps: a.fps.min(b.fps),
        fps_ntsc: a.fps_ntsc,
        kbps: a.kbps.min(b.kbps),
        codec: a.codec,
        keyframe_s: a.keyframe_s.min(b.keyframe_s).max(1),
        strict_cbr: a.strict_cbr || b.strict_cbr,
        no_bframes: a.no_bframes || b.no_bframes,
        audio_kbps: a.audio_kbps.min(b.audio_kbps),
    })
}

fn key_satisfies_member(key: &EncodeKey, d: &WorkingDest, ignore_service_limits: bool) -> bool {
    let ignore = effective_ignore_limits(&d.input, ignore_service_limits);
    if ignore {
        return true;
    }
    let Some(lim) = &d.limit else { return true };
    lim.allows_resolution(key.width, key.height)
        && key.fps <= lim.max_fps
        && key.kbps <= lim.max_kbps_for(key.width, key.height, key.fps)
        && lim.allows_codec(key.codec)
        && key.keyframe_s <= lim.max_keyframe_secs
        && key.audio_kbps <= lim.max_audio_kbps
        && (!lim.strict_cbr || key.strict_cbr)
        && (!lim.force_no_bframes || key.no_bframes)
}

fn join_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        2 => format!("{} and {}", names[0], names[1]),
        _ => {
            let head = names[..names.len() - 1].join(", ");
            format!("{head}, and {}", names[names.len() - 1])
        }
    }
}

fn coalesce_pass(
    dests: &mut [WorkingDest],
    groups: &mut Vec<WorkingGroup>,
    main: &MainEncode,
    vertical: &MainEncode,
    eb_extra: &[(u32, u32, u32)],
    ignore_service_limits: bool,
    budget: &PcBudget,
) {
    while evaluate_budget(groups, main, budget).status == BudgetStatus::Over {
        let mut best: Option<(bool, u32, usize, usize, EncodeKey)> = None;
        for (di, donor) in groups.iter().enumerate() {
            if !group_is_auto_fitted_only(donor, dests) {
                continue;
            }
            for (ti, target) in groups.iter().enumerate() {
                if di == ti || target.is_twitch_eb || target.is_light {
                    continue;
                }
                let Some(merged) = merged_compatible_key(&donor.key, &target.key) else {
                    continue;
                };
                let res_changed = (merged.width, merged.height, merged.fps)
                    != (
                        donor.key.width.max(target.key.width),
                        donor.key.height.max(target.key.height),
                        donor.key.fps.max(target.key.fps),
                    );
                let target_auto_only = group_is_auto_fitted_only(target, dests);
                let same_as_main_groups_on_canvas = groups
                    .iter()
                    .filter(|g| {
                        g.key.canvas == donor.key.canvas
                            && !g.is_twitch_eb
                            && !g.is_light
                            && g.members.iter().any(|&i| dests[i].is_same_as_main)
                    })
                    .count();
                if !target_auto_only
                    && (!merged.same_shape(&target.key)
                        || (res_changed
                            && merged.strict_cbr != target.key.strict_cbr
                            && same_as_main_groups_on_canvas >= 2))
                {
                    // Never silently alter an uncapped main or explicit Custom shape in Pass 1,
                    // and prefer Pass 2 shared-cap collapsing among SameAsMain peers over forcing
                    // strict CBR onto a lower-resolution Custom group.
                    continue;
                }
                let ok_all = donor
                    .members
                    .iter()
                    .chain(target.members.iter())
                    .all(|&idx| key_satisfies_member(&merged, &dests[idx], ignore_service_limits));
                if !ok_all {
                    continue;
                }
                let kbps_loss = donor.key.kbps.max(target.key.kbps).saturating_sub(merged.kbps);
                let replace = match &best {
                    None => true,
                    Some((best_res_changed, best_loss, best_di, best_ti, _)) => {
                        (res_changed, kbps_loss, donor.id.as_str(), target.id.as_str())
                            < (
                                *best_res_changed,
                                *best_loss,
                                groups[*best_di].id.as_str(),
                                groups[*best_ti].id.as_str(),
                            )
                    }
                };
                if replace {
                    best = Some((res_changed, kbps_loss, di, ti, merged));
                }
            }
        }
        let Some((_, _, di, ti, merged)) = best else {
            break;
        };
        let target_id = if groups[ti].id == GROUP_MAIN || groups[ti].id == GROUP_VERTICAL {
            groups[ti].id.clone()
        } else {
            group_id_for_key(&merged)
        };
        let affected: Vec<usize> = groups[di].members.iter().chain(groups[ti].members.iter()).copied().collect();
        for &idx in &affected {
            let changed = dests[idx].key != merged;
            let base = if dests[idx].input.canvas == Canvas::Vertical { vertical } else { main };
            dests[idx].group_id = target_id.clone();
            dests[idx].key = merged.clone();
            dests[idx].separate_encode = merged != base.as_key();
            if changed {
                dests[idx].coalesced = true;
            }
        }
        // Update coalesced CappedInfo chips once sibling names in the merged group are known.
        let names_in_group: Vec<(usize, String)> =
            affected.iter().map(|&idx| (idx, dests[idx].input.name.clone())).collect();
        for &idx in &affected {
            if dests[idx].coalesced {
                let others: Vec<String> =
                    names_in_group.iter().filter(|(other_idx, _)| *other_idx != idx).map(|(_, n)| n.clone()).collect();
                let res_fps = probe::format_res_fps(merged.width, merged.height, merged.fps);
                let rate = probe::bitrate_chip(merged.kbps);
                let summary = format!("Auto-fit: {res_fps} · {rate}");
                let detail = if others.is_empty() {
                    format!("Adjusted to {res_fps} · {rate} to stay within your PC's encoder limit.")
                } else {
                    format!(
                        "Shares {res_fps} · {rate} with {} to stay within your PC's encoder limit.",
                        join_names(&others)
                    )
                };
                dests[idx].capped = Some(CappedInfo {
                    kbps: merged.kbps,
                    resolution: Some((merged.width, merged.height)),
                    fps: Some(merged.fps),
                    separate_encode: dests[idx].separate_encode,
                    summary,
                    detail,
                });
            }
        }
        *groups = rebuild_groups(dests, eb_extra);
    }
}

fn tightest_shared_key(base: &MainEncode, members: &[&WorkingDest], ignore_service_limits: bool) -> (EncodeKey, bool) {
    let mut w = (base.width & !1).max(2);
    let mut h = (base.height & !1).max(2);
    let mut fps = base.fps.max(1);
    let mut kbps = base.kbps;
    let mut codec = base.codec;
    let mut keyframe_s = base.keyframe_s;
    let mut strict_cbr = base.strict_cbr;
    let mut no_bframes = base.no_bframes;
    let mut audio_kbps = base.audio_kbps;
    let mut any_capped = false;

    for m in members {
        let ignore = effective_ignore_limits(&m.input, ignore_service_limits);
        if ignore {
            continue;
        }
        let Some(lim) = &m.limit else { continue };
        let (lw, lh) = probe::fit_dimensions(w, h, lim.max_long_edge, lim.max_short_edge);
        if (lw, lh) != (w, h) {
            (w, h) = (lw, lh);
            any_capped = true;
        }
        let lfps = probe::largest_fps_divisor(fps, lim.max_fps);
        if lfps != fps {
            fps = lfps;
            any_capped = true;
        }
        let lcodec = lim.fallback_codec(codec);
        if lcodec != codec {
            codec = lcodec;
            any_capped = true;
        }
        let lkeyint = probe::fitted_keyframe(keyframe_s, lim);
        if lkeyint != keyframe_s {
            keyframe_s = lkeyint;
            any_capped = true;
        }
        let laudio = audio_kbps.min(lim.max_audio_kbps);
        if laudio != audio_kbps {
            audio_kbps = laudio;
            any_capped = true;
        }
        if lim.strict_cbr && base.vbr {
            any_capped = true;
        }
        strict_cbr |= lim.strict_cbr;
        no_bframes |= lim.force_no_bframes;
    }

    for m in members {
        let ignore = effective_ignore_limits(&m.input, ignore_service_limits);
        if ignore {
            continue;
        }
        if let Some(lim) = &m.limit {
            let tier_kbps = lim.max_kbps_for(w, h, fps);
            if tier_kbps < kbps {
                kbps = tier_kbps;
                any_capped = true;
            }
            if lim.force_no_bframes && !base.no_bframes {
                any_capped = true;
            }
        }
    }

    let key = EncodeKey {
        canvas: base.canvas,
        width: w,
        height: h,
        fps,
        fps_ntsc: base.fps_ntsc,
        kbps,
        codec,
        keyframe_s,
        strict_cbr,
        no_bframes,
        audio_kbps,
    };
    let changed = any_capped || key != base.as_key();
    (key, changed)
}

fn shared_cap_detail(budget: &PcBudget) -> String {
    match budget.effective_max_sessions() {
        Some(1) => "Your PC can run 1 stream encode at once; lower main quality or turn off a platform.".into(),
        Some(n) => format!("Your PC can run {n} stream encodes at once; lower main quality or turn off a platform."),
        None => {
            "Your PC's video encoder is at capacity for separate encodes; lower main quality or turn off a platform."
                .into()
        }
    }
}

fn shared_cap_pass(
    dests: &mut [WorkingDest],
    groups: &mut Vec<WorkingGroup>,
    input: &PlannerInput,
    vertical: &MainEncode,
    eb_extra: &[(u32, u32, u32)],
) {
    let main = &input.main;
    for canvas in [Canvas::Main, Canvas::Vertical] {
        if evaluate_budget(groups, main, &input.budget).status != BudgetStatus::Over {
            break;
        }
        if canvas == Canvas::Vertical && input.vertical_cut {
            continue;
        }
        let target_group_id = if canvas == Canvas::Main { GROUP_MAIN } else { GROUP_VERTICAL };
        let same_indices: Vec<usize> = dests
            .iter()
            .enumerate()
            .filter(|(_, d)| d.input.canvas == canvas && d.is_same_as_main && !d.is_eb && !d.is_manual_light)
            .map(|(i, _)| i)
            .collect();
        if same_indices.is_empty() {
            continue;
        }
        let distinct_groups: std::collections::HashSet<&str> =
            same_indices.iter().map(|&i| dests[i].group_id.as_str()).collect();
        let needs_collapse = distinct_groups.len() > 1
            || (canvas == Canvas::Main && input.budget.record_shares_main && !distinct_groups.contains(GROUP_MAIN));
        if !needs_collapse {
            continue;
        }
        let base = if canvas == Canvas::Vertical { vertical } else { main };
        let refs: Vec<&WorkingDest> = same_indices.iter().map(|&i| &dests[i]).collect();
        let (shared_key, changed) = tightest_shared_key(base, &refs, input.ignore_service_limits);

        // If an explicit Custom member shared `target_group_id` at `base`, move it to a dedicated hash ID
        // so collapsing `SameAsMain` onto `"main"`/`"vertical"` never modifies a Custom destination.
        for d in dests.iter_mut() {
            if d.input.canvas == canvas
                && d.group_id == target_group_id
                && (!d.is_same_as_main || d.is_eb || d.is_manual_light)
                && d.key != shared_key
            {
                d.group_id = group_id_for_key(&d.key);
                d.separate_encode = true;
            }
        }

        let res_fps = probe::format_res_fps(shared_key.width, shared_key.height, shared_key.fps);
        let rate = probe::bitrate_chip(shared_key.kbps);
        let detail = shared_cap_detail(&input.budget);
        for &idx in &same_indices {
            dests[idx].group_id = target_group_id.to_string();
            dests[idx].key = shared_key.clone();
            dests[idx].separate_encode = false;
            dests[idx].coalesced = false;
            dests[idx].shared_cap_fallback = changed;
            dests[idx].auto_fitted = changed;
            dests[idx].capped = changed.then(|| CappedInfo {
                kbps: shared_key.kbps,
                resolution: Some((shared_key.width, shared_key.height)),
                fps: Some(shared_key.fps),
                separate_encode: false,
                summary: format!("Shared cap: {res_fps} · {rate}"),
                detail: detail.clone(),
            });
        }
        *groups = rebuild_groups(dests, eb_extra);
    }
}

fn apply_on_demand_light(
    dests: &mut [WorkingDest],
    groups: &mut Vec<WorkingGroup>,
    main: &MainEncode,
    any_whip: bool,
    eb_extra: &[(u32, u32, u32)],
    input: &PlannerInput,
) {
    let whip_simulcast = input.whip_simulcast && any_whip;
    let ignore_service_limits = input.ignore_service_limits;
    let budget = &input.budget;
    let light_key = light_encode_key(main, any_whip);
    let member_light_key = |d: &WorkingDest| {
        let mut k = light_key.clone();
        if !effective_ignore_limits(&d.input, ignore_service_limits) && d.limit.as_ref().is_some_and(|l| l.strict_cbr) {
            k.strict_cbr = true;
        }
        k
    };
    let route_light_indices: Vec<usize> = dests
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            d.input.route_light
                && d.input.canvas == Canvas::Main
                && d.input.direct.is_none()
                && !d.is_eb
                && !d.is_manual_light
                && is_heavier_than_light(&d.key, &light_key)
                && key_satisfies_member(&member_light_key(d), d, ignore_service_limits)
        })
        .map(|(i, _)| i)
        .collect();
    if route_light_indices.is_empty() && !whip_simulcast {
        return;
    }

    let mut sim_dests = dests.to_vec();
    for &idx in &route_light_indices {
        let k = member_light_key(&sim_dests[idx]);
        sim_dests[idx].group_id = GROUP_LIGHT.to_string();
        sim_dests[idx].key = k;
        sim_dests[idx].separate_encode = true;
        sim_dests[idx].on_light = true;
    }
    let mut sim_groups = rebuild_groups(&sim_dests, eb_extra);
    if whip_simulcast && !sim_groups.iter().any(|g| g.id == GROUP_LIGHT) {
        sim_groups.push(WorkingGroup {
            id: GROUP_LIGHT.to_string(),
            key: light_key.clone(),
            members: Vec::new(),
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: true,
            is_twitch_eb: false,
        });
    }
    if evaluate_budget(&sim_groups, main, budget).status != BudgetStatus::Over {
        dests.clone_from_slice(&sim_dests);
        *groups = sim_groups;
    }
}

fn sort_groups(groups: &mut [WorkingGroup]) {
    let rank = |id: &str| match id {
        GROUP_MAIN => (0u8, id.to_string()),
        GROUP_VERTICAL => (1u8, id.to_string()),
        GROUP_TWITCH_EB => (2u8, id.to_string()),
        GROUP_LIGHT => (4u8, id.to_string()),
        _ => (3u8, id.to_string()),
    };
    groups.sort_by_key(|a| rank(&a.id));
}

/// Resolves a single destination preview without modifying or charging any budget.
pub fn resolve_preview(
    main: &MainEncode,
    vertical: Option<&MainEncode>,
    vertical_cut: bool,
    d: &PlannerDestination,
    ignore_service_limits: bool,
    twitch_eb: Option<&TwitchEbSpec>,
) -> PlannedDestination {
    let vert_base = vertical.copied().unwrap_or_else(|| default_vertical_base(main));
    let base = if d.canvas == Canvas::Vertical { &vert_base } else { main };
    let is_manual_light = d.light && d.canvas == Canvas::Main && d.direct.is_none();
    let is_eb =
        d.enhanced && d.kind == Service::Twitch && d.canvas == Canvas::Main && d.direct.is_none() && !is_manual_light;
    if is_eb {
        let eb = twitch_eb.cloned().unwrap_or_else(|| TwitchEbSpec::estimated(main));
        let matches_main = eb.top_key == main.as_key();
        let group_id = if matches_main { GROUP_MAIN.into() } else { GROUP_TWITCH_EB.into() };
        return PlannedDestination {
            id: d.id.clone(),
            group_id,
            key: eb.top_key,
            separate_encode: !matches_main,
            auto_fitted: false,
            coalesced: false,
            shared_cap_fallback: false,
            capped: None,
            shared_with: Vec::new(),
            on_light: false,
            note: None,
        };
    }
    let (fitted, _) = resolve_normal_fit(d, base, vertical_cut, ignore_service_limits);
    let group_id = if d.canvas == Canvas::Main && fitted.key == main.as_key() {
        GROUP_MAIN.to_string()
    } else if d.canvas == Canvas::Vertical && fitted.key == vert_base.as_key() {
        GROUP_VERTICAL.to_string()
    } else {
        group_id_for_key(&fitted.key)
    };
    PlannedDestination {
        id: d.id.clone(),
        group_id,
        key: fitted.key,
        separate_encode: fitted.separate_encode,
        auto_fitted: fitted.auto_fitted,
        coalesced: false,
        shared_cap_fallback: false,
        capped: fitted.capped,
        shared_with: Vec::new(),
        on_light: false,
        note: fitted.note,
    }
}

/// Pure planner: resolves every active destination, deduplicates identical [`EncodeKey`]s, enforces
/// [`PcBudget`] via Pass-1 coalescing and Pass-2 shared-cap fallback, isolates Twitch Enhanced
/// Broadcasting when `main` differs, and activates `"light"` only on demand.
pub fn plan_encodes(input: &PlannerInput) -> EncodePlan {
    let vertical_base = input.vertical.unwrap_or_else(|| default_vertical_base(&input.main));
    let any_active_vertical =
        input.destinations.iter().any(|d| d.enabled && d.can_start && !d.standby && d.canvas == Canvas::Vertical);
    let any_whip = input.destinations.iter().any(|d| {
        d.enabled && d.can_start && !d.standby && d.canvas == Canvas::Main && d.direct == Some(DirectKind::Whip)
    });
    let mut main_eff = input.main;
    if any_whip {
        main_eff.no_bframes = true;
        main_eff.keyframe_s = main_eff.keyframe_s.clamp(1, probe::WHIP_LIMIT.max_keyframe_secs);
        main_eff.codec = probe::WHIP_LIMIT.fallback_codec(main_eff.codec);
    }
    let light_key = light_encode_key(&main_eff, any_whip);

    let has_active_eb = input.destinations.iter().any(|d| {
        d.enabled
            && d.can_start
            && !d.standby
            && d.enhanced
            && d.kind == Service::Twitch
            && d.canvas == Canvas::Main
            && d.direct.is_none()
            && !d.wants_light_flag()
    });
    let eb_spec = has_active_eb.then(|| input.twitch_eb.clone().unwrap_or_else(|| TwitchEbSpec::estimated(&main_eff)));
    let eb_extra: Vec<(u32, u32, u32)> = eb_spec.as_ref().map_or_else(Vec::new, |s| s.extra_tracks.clone());

    let mut working: Vec<WorkingDest> = Vec::new();
    for d in &input.destinations {
        if !d.enabled || !d.can_start || d.standby {
            continue;
        }
        let is_manual_light = d.wants_light_flag();
        let is_eb = d.enhanced
            && d.kind == Service::Twitch
            && d.canvas == Canvas::Main
            && d.direct.is_none()
            && !d.wants_light_flag();
        if is_eb && let Some(eb) = &eb_spec {
            let matches_main = eb.top_key == main_eff.as_key();
            let group_id = if matches_main { GROUP_MAIN.into() } else { GROUP_TWITCH_EB.into() };
            working.push(WorkingDest {
                input: d.clone(),
                group_id,
                key: eb.top_key.clone(),
                separate_encode: !matches_main,
                auto_fitted: false,
                coalesced: false,
                shared_cap_fallback: false,
                capped: None,
                note: None,
                is_same_as_main: false,
                is_auto_fitted_same_as_main: false,
                is_eb: true,
                is_manual_light: false,
                on_light: false,
                limit: Some(probe::TWITCH_LIMIT),
            });
            continue;
        }
        let base = if d.canvas == Canvas::Vertical { &vertical_base } else { &main_eff };
        let (fitted, limit) = resolve_normal_fit(d, base, input.vertical_cut, input.ignore_service_limits);
        let group_id = if d.canvas == Canvas::Main && fitted.key == main_eff.as_key() {
            GROUP_MAIN.to_string()
        } else if is_manual_light && fitted.key.same_shape(&light_key) {
            GROUP_LIGHT.to_string()
        } else if d.canvas == Canvas::Vertical && fitted.key == vertical_base.as_key() {
            GROUP_VERTICAL.to_string()
        } else {
            group_id_for_key(&fitted.key)
        };
        let on_light = group_id == GROUP_LIGHT;
        let is_same_as_main = d.quality == DestinationQuality::SameAsMain && !is_manual_light;
        let is_auto_fitted_same_as_main = is_same_as_main && fitted.auto_fitted;
        working.push(WorkingDest {
            input: d.clone(),
            group_id,
            key: fitted.key,
            separate_encode: fitted.separate_encode,
            auto_fitted: fitted.auto_fitted,
            coalesced: false,
            shared_cap_fallback: false,
            capped: fitted.capped,
            note: fitted.note,
            is_same_as_main,
            is_auto_fitted_same_as_main,
            is_eb: false,
            is_manual_light,
            on_light,
            limit,
        });
    }

    let mut groups = rebuild_groups(&working, &eb_extra);
    if evaluate_budget(&groups, &main_eff, &input.budget).status == BudgetStatus::Over {
        coalesce_pass(
            &mut working,
            &mut groups,
            &main_eff,
            &vertical_base,
            &eb_extra,
            input.ignore_service_limits,
            &input.budget,
        );
    }
    if evaluate_budget(&groups, &main_eff, &input.budget).status == BudgetStatus::Over {
        let mut eff_input = input.clone();
        eff_input.main = main_eff;
        shared_cap_pass(&mut working, &mut groups, &eff_input, &vertical_base, &eb_extra);
    }
    apply_on_demand_light(&mut working, &mut groups, &main_eff, any_whip, &eb_extra, input);
    sort_groups(&mut groups);

    let budget = evaluate_budget(&groups, &main_eff, &input.budget);
    let mut group_member_names: BTreeMap<GroupId, Vec<(String, String)>> = BTreeMap::new();
    for g in &groups {
        let names = g.members.iter().map(|&i| (working[i].input.id.clone(), working[i].input.name.clone())).collect();
        group_member_names.insert(g.id.clone(), names);
    }

    let mut dest_map = HashMap::with_capacity(working.len());
    for d in &working {
        let shared_with = group_member_names
            .get(&d.group_id)
            .map(|list| list.iter().filter(|(id, _)| id != &d.input.id).map(|(_, name)| name.clone()).collect())
            .unwrap_or_default();
        let note = (!d.is_eb && !effective_ignore_limits(&d.input, input.ignore_service_limits))
            .then(|| d.limit.as_ref().and_then(|lim| lim.recommendation_note(d.key.width, d.key.height, d.key.fps)))
            .flatten()
            .or_else(|| d.note.clone().filter(|_| d.limit.is_none()));
        dest_map.insert(
            d.input.id.clone(),
            PlannedDestination {
                id: d.input.id.clone(),
                group_id: d.group_id.clone(),
                key: d.key.clone(),
                separate_encode: d.separate_encode,
                auto_fitted: d.auto_fitted,
                coalesced: d.coalesced,
                shared_cap_fallback: d.shared_cap_fallback,
                capped: d.capped.clone(),
                shared_with,
                on_light: d.on_light,
                note,
            },
        );
    }

    let out_groups = groups
        .into_iter()
        .map(|g| {
            let vod_track = g.members.iter().any(|&i| working[i].input.vod_track);
            let members = g.members.iter().map(|&i| working[i].input.id.clone()).collect();
            EncodeGroup {
                id: g.id,
                key: g.key,
                members,
                extra_tracks: g.extra_tracks,
                vod_track,
                is_light: g.is_light,
                is_twitch_eb: g.is_twitch_eb,
            }
        })
        .collect();

    EncodePlan {
        main: main_eff.as_key(),
        vertical: any_active_vertical.then(|| vertical_base.as_key()),
        groups: out_groups,
        destinations: dest_map,
        budget,
    }
}

impl PlannerDestination {
    fn wants_light_flag(&self) -> bool {
        self.light && self.canvas == Canvas::Main && self.direct.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(id: &str, name: &str, kind: Service, canvas: Canvas) -> PlannerDestination {
        PlannerDestination {
            id: id.into(),
            name: name.into(),
            kind,
            brand: None,
            canvas,
            enabled: true,
            can_start: true,
            standby: false,
            direct: None,
            light: false,
            enhanced: false,
            route_light: false,
            quality: DestinationQuality::SameAsMain,
            vod_track: kind == Service::Twitch,
        }
    }

    fn base_1440p60() -> MainEncode {
        MainEncode::from_canvas(Canvas::Main, 2560, 1440, 60, 18_000, VideoCodec::Hevc)
    }

    #[test]
    fn planner_1440p_main_twitch_youtube_kick_produces_three_groups_without_idle_light() {
        let input = PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![
                dest("youtube", "YouTube", Service::Youtube, Canvas::Main),
                dest("kick", "Kick", Service::Kick, Canvas::Main),
                dest("twitch", "Twitch", Service::Twitch, Canvas::Main),
            ],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2), // M2 Max: 8 sessions, 995 Mpx/s
        };
        let plan = plan_encodes(&input);
        assert_eq!(plan.budget.status, BudgetStatus::Ok);
        assert_eq!(plan.groups.len(), 3, "main (YouTube) + 1080p60@8M (Kick) + 1440p60@6M (Twitch)");
        assert!(!plan.groups.iter().any(|g| g.is_light), "light encoder must not run idle");

        let yt = &plan.destinations["youtube"];
        assert_eq!(yt.group_id, GROUP_MAIN);
        assert_eq!((yt.key.width, yt.key.height, yt.key.kbps, yt.key.codec), (2560, 1440, 18_000, VideoCodec::Hevc));
        assert!(!yt.separate_encode);

        let kick = &plan.destinations["kick"];
        assert_ne!(kick.group_id, GROUP_MAIN);
        assert_eq!(
            (kick.key.width, kick.key.height, kick.key.kbps, kick.key.codec),
            (1920, 1080, 8_000, VideoCodec::H264)
        );
        assert!(kick.separate_encode);
        assert!(kick.capped.as_ref().is_some_and(|c| c.separate_encode && c.resolution == Some((1920, 1080))));

        let twitch = &plan.destinations["twitch"];
        assert_ne!(twitch.group_id, kick.group_id);
        assert_eq!(
            (twitch.key.width, twitch.key.height, twitch.key.kbps, twitch.key.codec),
            (2560, 1440, 6_000, VideoCodec::H264)
        );
        assert!(twitch.separate_encode);
        assert_eq!(twitch.note.as_deref(), Some("Above Twitch's recommended 1080p"));
    }

    #[test]
    fn planner_dedups_identical_custom_encodes_and_preserves_vod_track_union() {
        let mut c1 = dest("c_1", "Relay 1", Service::Custom, Canvas::Main);
        c1.quality = DestinationQuality::Custom(CustomQuality {
            width: 2560,
            height: 1440,
            fps: 60,
            kbps: 6_000,
            codec: Some(VideoCodec::H264),
        });
        let mut c2 = dest("c_2", "Relay 2", Service::Custom, Canvas::Main);
        c2.quality = c1.quality.clone();
        let mut main = base_1440p60();
        main.strict_cbr = true;
        let mut twitch = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        twitch.vod_track = true;

        let input = PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![dest("youtube", "YouTube", Service::Youtube, Canvas::Main), c1, c2, twitch],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        };
        let plan = plan_encodes(&input);
        // YouTube gets main; c_1, c_2, and auto-fitted Twitch all have identical 1440p60@6000 H.264 keys -> 1 shared group!
        assert_eq!(plan.groups.len(), 2, "{:?}", plan.groups);
        let shared_group = plan.group_for_destination("c_1").unwrap();
        assert_eq!(shared_group.members, vec!["c_1", "c_2", "twitch"]);
        assert!(shared_group.vod_track, "VOD track is enabled if any member wants it");
        assert_eq!(plan.destinations["c_1"].shared_with, vec!["Relay 2", "Twitch"]);
        assert_eq!(plan.destinations["c_2"].shared_with, vec!["Relay 1", "Twitch"]);
    }

    #[test]
    fn planner_tight_budget_coalesces_auto_fit_first_then_falls_back_to_shared_cap() {
        let dests = vec![
            dest("youtube", "YouTube", Service::Youtube, Canvas::Main),
            dest("kick", "Kick", Service::Kick, Canvas::Main),
            dest("twitch", "Twitch", Service::Twitch, Canvas::Main),
        ];

        // 1. Two-session budget (e.g. NVENC 3 - 1 spare = 2 sessions): Pass 1 coalesces Kick 8M -> Twitch 6M.
        let tight_2 = PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: dests.clone(),
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget {
                source: BudgetSource::NvencDefault,
                limit_mpx_s: Some(500.0),
                max_sessions: Some(3),
                reserve_spare_session: true,
                ..PcBudget::unknown()
            },
        };
        let p1 = plan_encodes(&tight_2);
        assert_eq!(p1.budget.status, BudgetStatus::Ok);
        assert_eq!(p1.groups.len(), 2, "Pass 1 coalesces Kick + Twitch into 1 group while YouTube keeps 1440p60");
        assert_eq!(p1.destinations["youtube"].group_id, GROUP_MAIN);
        assert_eq!(p1.destinations["youtube"].key.height, 1440);
        assert_eq!(p1.destinations["kick"].group_id, p1.destinations["twitch"].group_id);
        assert_eq!(p1.destinations["kick"].key.kbps, 6_000);
        assert!(p1.destinations["kick"].coalesced);
        assert!(!p1.destinations["kick"].shared_cap_fallback);
        assert!(
            p1.destinations["kick"]
                .capped
                .as_ref()
                .is_some_and(|c| c.detail.contains("Shares") && c.detail.contains("Twitch")),
            "{:?}",
            p1.destinations["kick"].capped
        );

        // 2. Single-session budget: Pass 1 still yields 2 > 1 -> Pass 2 falls back to shared main cap.
        let tight_1 =
            PlannerInput { budget: PcBudget::with_limits(BudgetSource::CpuDefault, Some(250.0), Some(1)), ..tight_2 };
        let p2 = plan_encodes(&tight_1);
        assert_eq!(p2.budget.status, BudgetStatus::Ok);
        assert_eq!(p2.groups.len(), 1, "Pass 2 collapses SameAsMain into 1 shared main encode");
        assert_eq!(p2.groups[0].id, GROUP_MAIN);
        for id in ["youtube", "kick", "twitch"] {
            let d = &p2.destinations[id];
            assert_eq!(d.group_id, GROUP_MAIN);
            assert!(!d.separate_encode);
            assert!(d.shared_cap_fallback);
            assert_eq!((d.key.width, d.key.height, d.key.kbps, d.key.codec), (1920, 1080, 6_000, VideoCodec::H264));
            assert!(d.capped.as_ref().is_some_and(|c| c.summary.starts_with("Shared cap:") && !c.separate_encode));
        }
    }

    #[test]
    fn planner_keeps_twitch_eb_in_own_group_when_main_differs_and_shares_when_matching() {
        let mut tw = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        tw.enhanced = true;

        // Main is 1440p60 HEVC 18 Mbps -> EB runs in its own `"twitch_eb"` group, leaving `"main"` clean!
        let eb_plan = ferrocast_golive::twitch_eb::Plan {
            config_id: "cfg-1".into(),
            url: "rtmps://ingest.test/app/key".into(),
            encodes: vec![
                ferrocast_golive::twitch_eb::Encode {
                    kind: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
                    width: 1920,
                    height: 1080,
                    framerate: Some(ferrocast_golive::twitch_eb::Fps { numerator: 60, denominator: 1 }),
                    settings: serde_json::json!({ "bitrate": 6000, "keyint_sec": 2 }),
                    canvas_index: 0,
                },
                ferrocast_golive::twitch_eb::Encode {
                    kind: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
                    width: 1280,
                    height: 720,
                    framerate: Some(ferrocast_golive::twitch_eb::Fps { numerator: 60, denominator: 1 }),
                    settings: serde_json::json!({ "bitrate": 3000 }),
                    canvas_index: 0,
                },
            ],
            warning: None,
        };
        let main_1440 = base_1440p60();
        let eb_spec = TwitchEbSpec::from_plan(&eb_plan, &main_1440).unwrap();
        let p_diff = plan_encodes(&PlannerInput {
            main: main_1440,
            vertical: None,
            vertical_cut: false,
            destinations: vec![dest("youtube", "YouTube", Service::Youtube, Canvas::Main), tw.clone()],
            ignore_service_limits: false,
            twitch_eb: Some(eb_spec),
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p_diff.groups.len(), 2);
        let main_g = p_diff.groups.iter().find(|g| g.id == GROUP_MAIN).unwrap();
        let eb_g = p_diff.groups.iter().find(|g| g.id == GROUP_TWITCH_EB).unwrap();
        assert!(main_g.extra_tracks.is_empty(), "main stays clean for YouTube and Same-as-stream recording");
        assert!(!main_g.is_twitch_eb);
        assert_eq!(eb_g.members, vec!["twitch"]);
        assert_eq!(eb_g.extra_tracks, vec![(1280, 720, 3000)]);
        assert!(eb_g.is_twitch_eb);
        // Budget accounts for main (1) + eb top (1) + eb extra track (1) = 3 sessions.
        assert_eq!(p_diff.budget.used_sessions, 3);

        // Main is 1080p60 H264 6000 kbps 2s (matches EB top) -> EB runs on `"main"` with no duplicate top encoder.
        let main_1080 = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        let eb_spec_match = TwitchEbSpec::from_plan(&eb_plan, &main_1080).unwrap();
        let p_same = plan_encodes(&PlannerInput {
            main: main_1080,
            vertical: None,
            vertical_cut: false,
            destinations: vec![dest("youtube", "YouTube", Service::Youtube, Canvas::Main), tw],
            ignore_service_limits: false,
            twitch_eb: Some(eb_spec_match),
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p_same.groups.len(), 1);
        assert_eq!(p_same.groups[0].id, GROUP_MAIN);
        assert!(p_same.groups[0].is_twitch_eb);
        assert_eq!(p_same.groups[0].extra_tracks, vec![(1280, 720, 3000)]);
        assert_eq!(p_same.budget.used_sessions, 2);
    }

    #[test]
    fn planner_keeps_vertical_untouched_while_main_auto_fits_or_falls_back() {
        let vert = MainEncode::from_canvas(Canvas::Vertical, 1080, 1920, 60, 9_000, VideoCodec::H264);
        let dests = vec![
            dest("youtube", "YouTube", Service::Youtube, Canvas::Main),
            dest("kick", "Kick", Service::Kick, Canvas::Main),
            dest("twitch", "Twitch", Service::Twitch, Canvas::Main),
            dest("youtube_vertical", "YouTube Shorts live", Service::Youtube, Canvas::Vertical),
        ];

        // Ample budget: vertical stays untouched on "vertical", main splits cleanly.
        let p_ok = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: Some(vert),
            vertical_cut: false,
            destinations: dests.clone(),
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p_ok.groups.len(), 4);
        let v_dest = &p_ok.destinations["youtube_vertical"];
        assert_eq!(v_dest.group_id, GROUP_VERTICAL);
        assert_eq!((v_dest.key.width, v_dest.key.height, v_dest.key.fps), (1080, 1920, 60));
        assert!(!v_dest.separate_encode);

        // Tight 2-session budget (e.g. 1 main + 1 vertical): Pass 2 collapses main, vertical STILL untouched!
        let p_tight = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: Some(vert),
            vertical_cut: false,
            destinations: dests,
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::with_limits(BudgetSource::CpuDefault, Some(300.0), Some(2)),
        });
        assert_eq!(p_tight.budget.status, BudgetStatus::Ok);
        assert_eq!(p_tight.groups.len(), 2, "collapsed main + untouched vertical");
        let v_tight = &p_tight.destinations["youtube_vertical"];
        assert_eq!(v_tight.group_id, GROUP_VERTICAL);
        assert_eq!((v_tight.key.width, v_tight.key.height, v_tight.key.fps, v_tight.key.kbps), (1080, 1920, 60, 9_000));
        assert!(!v_tight.shared_cap_fallback);
    }

    #[test]
    fn planner_activates_light_only_on_demand_and_respects_budget_and_lighter_customs() {
        let mut yt = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        let mut kick = dest("kick", "Kick", Service::Kick, Canvas::Main);
        let mut low_custom = dest("c_low", "Low Custom", Service::Custom, Canvas::Main);
        low_custom.quality = DestinationQuality::Custom(CustomQuality {
            width: 852,
            height: 480,
            fps: 30,
            kbps: 1_500,
            codec: Some(VideoCodec::H264),
        });

        // 1. Healthy routes: no light group spun up.
        let p_healthy = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt.clone(), kick.clone(), low_custom.clone()],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert!(!p_healthy.groups.iter().any(|g| g.id == GROUP_LIGHT));

        // 2. Kick route degrades with budget headroom: `"light"` spins up on demand for Kick;
        //    low_custom (1,500 <= 3,000) is NOT moved to higher-bitrate light even if marked route_light.
        kick.route_light = true;
        low_custom.route_light = true;
        let p_light = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt.clone(), kick.clone(), low_custom.clone()],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert!(p_light.groups.iter().any(|g| g.id == GROUP_LIGHT));
        assert_eq!(p_light.destinations["kick"].group_id, GROUP_LIGHT);
        assert!(p_light.destinations["kick"].on_light);
        assert_ne!(p_light.destinations["c_low"].group_id, GROUP_LIGHT);
        assert!(!p_light.destinations["c_low"].on_light);

        // 3. Budget full (max_sessions = 2 already used by YouTube + c_low): light fallback is skipped!
        yt.route_light = false;
        let p_full = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt, low_custom, {
                let mut k = kick;
                k.route_light = false;
                k
            }],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::with_limits(BudgetSource::NvencDefault, Some(500.0), Some(2)),
        });
        // Here YouTube + Kick collapsed/coalesced with budget 2? Wait: Kick (8M) + Low Custom (1.5M) + YouTube (1440p)
        // exceeds 2 sessions -> YouTube + Kick collapse to main (1080p60@8M) + Low Custom = 2 sessions.
        assert_eq!(p_full.budget.used_sessions, 2);
        // Now keep YouTube + Kick (`SameAsMain`) shared on `main` alongside `c_other` (= 2 sessions),
        // and mark Kick `route_light`: spinning up a 3rd `light` session would exceed `max_sessions = 2`,
        // so on-demand `light` is skipped and Kick remains on `main`.
        let yt2 = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        let mut kick_over = dest("kick", "Kick", Service::Kick, Canvas::Main);
        kick_over.route_light = true;
        let mut c_other = dest("c_other", "Other", Service::Custom, Canvas::Main);
        c_other.quality = DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 60,
            kbps: 4_500,
            codec: Some(VideoCodec::H264),
        });
        let p_skip_light = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt2, kick_over, c_other],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::with_limits(BudgetSource::NvencDefault, Some(500.0), Some(2)),
        });
        assert!(!p_skip_light.groups.iter().any(|g| g.id == GROUP_LIGHT), "budget full must skip light");
        assert_eq!(p_skip_light.destinations["kick"].group_id, GROUP_MAIN);
    }

    fn base_host(os: &str, encoder_id: &str) -> crate::studio_host::HostBudgetInfo {
        crate::studio_host::HostBudgetInfo {
            os: os.to_string(),
            encoder_id: encoder_id.to_string(),
            cpu_cores: 8,
            host_sig: format!("{os}|cores=8"),
            ..Default::default()
        }
    }

    #[test]
    fn pc_budget_from_host_info_covers_cross_os_and_nvenc_spare_session() {
        // Apple Silicon: 1 and 2 engines, no FFmpeg handover spare session reserved.
        let mut m1 = base_host("macos", "com.apple.videotoolbox.videoencoder.ave.avc");
        m1.engines = Some(1);
        m1.hw_backend = Some("apple".into());
        let b_m1 = PcBudget::from_host_info(&m1, None, true);
        assert_eq!(b_m1.source, BudgetSource::AppleSilicon);
        assert_eq!(b_m1.max_sessions, Some(4));
        assert!(!b_m1.reserve_spare_session);
        assert_eq!(b_m1.effective_max_sessions(), Some(4));

        // Intel Mac HW fallback (125 Mpx/s, 2 sessions) and valid bench override.
        let intel = base_host("macos", "com.apple.videotoolbox.videoencoder.ave.avc");
        let b_intel = PcBudget::from_host_info(&intel, None, true);
        assert_eq!(b_intel.source, BudgetSource::HwDefault);
        assert_eq!(b_intel.limit_mpx_s, Some(125.0));
        assert_eq!(b_intel.effective_max_sessions(), Some(2));

        // Windows NVENC driver matrix: <531 -> 3, 531..551 -> 5, >=551.23 -> 8, minus 1 spare when auto_quality.
        let mut nv_old = base_host("windows", "ffmpeg_nvenc");
        nv_old.nvidia_driver_major = Some(525);
        assert_eq!(PcBudget::from_host_info(&nv_old, None, false).effective_max_sessions(), Some(3));
        assert_eq!(PcBudget::from_host_info(&nv_old, None, true).effective_max_sessions(), Some(2));

        let mut nv_mid = base_host("windows", "ffmpeg_nvenc");
        nv_mid.nvidia_driver_major = Some(531);
        nv_mid.nvidia_driver_minor = Some(18);
        assert_eq!(PcBudget::from_host_info(&nv_mid, None, false).effective_max_sessions(), Some(5));
        assert_eq!(PcBudget::from_host_info(&nv_mid, None, true).effective_max_sessions(), Some(4));

        let mut nv_551_22 = base_host("windows", "ffmpeg_nvenc");
        nv_551_22.nvidia_driver_major = Some(551);
        nv_551_22.nvidia_driver_minor = Some(22);
        assert_eq!(PcBudget::from_host_info(&nv_551_22, None, false).effective_max_sessions(), Some(5));

        let mut nv_new = base_host("windows", "ffmpeg_nvenc");
        nv_new.nvidia_driver_major = Some(551);
        nv_new.nvidia_driver_minor = Some(23);
        let b_nv_new = PcBudget::from_host_info(&nv_new, None, true);
        assert_eq!(b_nv_new.source, BudgetSource::NvencDefault);
        assert_eq!(b_nv_new.limit_mpx_s, Some(250.0));
        assert_eq!(b_nv_new.max_sessions, Some(8));
        assert_eq!(b_nv_new.effective_max_sessions(), Some(7));

        // Windows QSV / AMF fallback: 250 Mpx/s default, 3 sessions minus 1 spare when auto_quality = true.
        let mut qsv = base_host("windows", "ffmpeg_hardware");
        qsv.hw_backend = Some("qsv".into());
        let b_qsv = PcBudget::from_host_info(&qsv, None, true);
        assert_eq!(b_qsv.source, BudgetSource::HwDefault);
        assert_eq!(b_qsv.limit_mpx_s, Some(250.0));
        assert_eq!(b_qsv.effective_max_sessions(), Some(2));

        // Software x264 core tiers (<8 -> 125/1, 8-11 -> 180/2, 12-15 -> 250/2, 16+ -> 250/3).
        let mut sw4 = base_host("linux", "obs_x264");
        sw4.cpu_cores = 4;
        let b_sw4 = PcBudget::from_host_info(&sw4, None, true);
        assert_eq!(
            (b_sw4.source, b_sw4.limit_mpx_s, b_sw4.effective_max_sessions()),
            (BudgetSource::CpuDefault, Some(125.0), Some(1))
        );
        let mut sw8 = base_host("linux", "obs_x264");
        sw8.cpu_cores = 8;
        assert_eq!(
            (
                PcBudget::from_host_info(&sw8, None, true).limit_mpx_s,
                PcBudget::from_host_info(&sw8, None, true).effective_max_sessions()
            ),
            (Some(180.0), Some(2))
        );
        let mut sw16 = base_host("linux", "obs_x264");
        sw16.cpu_cores = 16;
        assert_eq!(
            (
                PcBudget::from_host_info(&sw16, None, true).limit_mpx_s,
                PcBudget::from_host_info(&sw16, None, true).effective_max_sessions()
            ),
            (Some(250.0), Some(3))
        );
    }

    #[test]
    fn persisted_studio_bench_validates_host_sig_and_encoder_class() {
        let mut info = base_host("windows", "ffmpeg_nvenc");
        info.nvidia_driver_major = Some(551);
        info.nvidia_driver_minor = Some(23);
        info.host_sig = "windows|nv=551.23".into();

        let bench = PersistedStudioBench {
            encoder_id: "ffmpeg_nvenc".into(),
            is_software: false,
            max_mpx_s: 612.0,
            max_sessions_ok: Some(6),
            measured_at: 1_700_000_000_000,
            host_sig: "windows|nv=551.23".into(),
        };
        let b_valid = PcBudget::from_host_info(&info, Some(&bench), true);
        assert_eq!(b_valid.source, BudgetSource::Bench);
        assert_eq!(b_valid.limit_mpx_s, Some(612.0));
        assert_eq!(b_valid.max_sessions, Some(6));
        assert_eq!(b_valid.effective_max_sessions(), Some(5));

        // Driver update invalidates host_sig -> falls back to NvencDefault.
        let mut upgraded = info.clone();
        upgraded.host_sig = "windows|nv=560.70".into();
        let b_stale = PcBudget::from_host_info(&upgraded, Some(&bench), true);
        assert_eq!(b_stale.source, BudgetSource::NvencDefault);
        assert_eq!(b_stale.limit_mpx_s, Some(250.0));

        // Switching to x264 invalidates HW bench -> CpuDefault, AND when a software bench IS valid,
        // `evaluate_budget` must count software stream encodes against it.
        let sw_info = crate::studio_host::HostBudgetInfo {
            encoder_id: "obs_x264".into(),
            host_sig: "windows|nv=551.23".into(),
            cpu_cores: 8,
            ..info
        };
        assert_eq!(PcBudget::from_host_info(&sw_info, Some(&bench), true).source, BudgetSource::CpuDefault);

        let sw_bench = PersistedStudioBench {
            encoder_id: "obs_x264".into(),
            is_software: true,
            max_mpx_s: 150.0,
            max_sessions_ok: Some(2),
            measured_at: 1_700_000_000_000,
            host_sig: sw_info.host_sig.clone(),
        };
        let sw_budget = PcBudget::from_host_info(&sw_info, Some(&sw_bench), true);
        assert_eq!(sw_budget.source, BudgetSource::Bench);
        assert_eq!(sw_budget.limit_mpx_s, Some(150.0));

        let plan = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![dest("yt", "YouTube", Service::Youtube, Canvas::Main)],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: sw_budget,
        });
        assert_eq!(
            plan.budget.status,
            BudgetStatus::Over,
            "software Bench limit must still count software stream encodes"
        );
        assert!(plan.budget.used_mpx_s > 200.0);
    }

    #[test]
    fn whip_folds_no_bframes_into_main_without_orphan_encoder() {
        let mut yt = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        let mut whip = dest("whip", "WebRTC", Service::Custom, Canvas::Main);
        whip.direct = Some(DirectKind::Whip);
        let mut main = base_1440p60();
        main.no_bframes = false;
        let plan = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt.clone(), whip],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(plan.groups.len(), 1, "WHIP + YouTube must share main without orphan encoder: {:?}", plan.groups);
        assert_eq!(plan.groups[0].id, GROUP_MAIN);
        assert!(plan.groups[0].key.no_bframes);
        assert_eq!(plan.destinations["youtube"].group_id, GROUP_MAIN);
        assert_eq!(plan.destinations["whip"].group_id, GROUP_MAIN);

        // Enhanced + Custom + light=true must NOT trigger Twitch EB planning in `has_active_eb`.
        yt.enhanced = true;
        let mut tw = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        tw.enhanced = true;
        tw.light = true;
        tw.quality = DestinationQuality::Custom(CustomQuality {
            width: 1280,
            height: 720,
            fps: 60,
            kbps: 3_000,
            codec: Some(VideoCodec::H264),
        });
        let p_eb = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![tw],
            ignore_service_limits: false,
            twitch_eb: Some(TwitchEbSpec { top_key: main.as_key(), extra_tracks: vec![(1280, 720, 3_000)] }),
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert!(p_eb.groups.iter().all(|g| !g.is_twitch_eb && g.extra_tracks.is_empty()));
    }

    #[test]
    fn qsv_with_nvidia_driver_uses_qsv_sessions_and_250_mpx_default() {
        let mut info = base_host("windows", "obs_qsv11");
        info.hw_backend = Some("qsv".into());
        info.nvidia_driver_major = Some(551);
        info.nvidia_driver_minor = Some(23);
        let budget = PcBudget::from_host_info(&info, None, false);
        assert_eq!(budget.source, BudgetSource::HwDefault);
        assert_eq!(budget.limit_mpx_s, Some(250.0));
        assert_eq!(budget.max_sessions, Some(3));
    }

    #[test]
    fn kick_auto_dedups_with_youtube_custom_regardless_of_strict_cbr() {
        let mut yt = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        yt.quality = DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 8_000,
            codec: Some(VideoCodec::H264),
        });
        let kick = dest("kick", "Kick", Service::Kick, Canvas::Main);
        let mut main = base_1440p60();
        main.vbr = false;
        main.strict_cbr = false;
        let plan = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt, kick],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(plan.groups.len(), 1, "strict_cbr must union rather than split matching keys: {:?}", plan.groups);
        assert!(plan.groups[0].key.strict_cbr);
        assert_eq!(plan.destinations["youtube"].group_id, plan.destinations["kick"].group_id);
    }

    #[test]
    fn record_shares_main_charges_extra_when_main_has_vod_or_eb() {
        let main = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        let mut tw_vod = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        tw_vod.vod_track = true;
        let mut budget = PcBudget::apple_silicon(2);
        budget.record_shares_main = true;

        let p_vod = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![tw_vod],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: budget.clone(),
        });
        assert_eq!(p_vod.groups[0].id, GROUP_MAIN);
        assert_eq!(p_vod.budget.used_sessions, 2, "VOD track on main prevents sharing with recording");

        let mut tw_eb = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        tw_eb.enhanced = true;
        let p_eb = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![tw_eb],
            ignore_service_limits: false,
            twitch_eb: Some(TwitchEbSpec { top_key: main.as_key(), extra_tracks: vec![(1280, 720, 3_000)] }),
            whip_simulcast: false,
            budget,
        });
        assert_eq!(p_eb.groups[0].id, GROUP_MAIN);
        assert_eq!(p_eb.budget.used_sessions, 3, "EB tracks on main prevent sharing with recording");
    }

    #[test]
    fn on_demand_and_manual_light_respect_platform_fps_limits() {
        let main = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        let mut fb = dest("fb", "Facebook", Service::Custom, Canvas::Main);
        fb.brand = Some("facebook".into());
        fb.route_light = true;
        let mut kick = dest("kick", "Kick", Service::Kick, Canvas::Main);
        kick.route_light = true;
        let p = plan_encodes(&PlannerInput {
            main,
            vertical: None,
            vertical_cut: false,
            destinations: vec![dest("yt", "YouTube", Service::Youtube, Canvas::Main), fb, kick],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p.destinations["kick"].group_id, GROUP_LIGHT);
        assert_ne!(
            p.destinations["fb"].group_id, GROUP_LIGHT,
            "Facebook 30 fps max must not route onto 60 fps GROUP_LIGHT"
        );
    }

    #[test]
    fn clamp_custom_for_fit_does_not_swap_portrait_dimensions_into_landscape_canvas() {
        let main = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        let portrait = CustomQuality { width: 1080, height: 1920, fps: 60, kbps: 4_000, codec: Some(VideoCodec::H264) };
        let clamped = super::clamp_custom_for_fit(&portrait, &main, None, true);
        assert!(clamped.width <= main.canvas_width && clamped.height <= main.canvas_height, "{clamped:?}");
        assert!(
            clamped.validate(main.canvas, main.canvas_width, main.canvas_height, main.canvas_fps, None, true).is_ok()
        );
    }

    #[test]
    fn twitch_eb_from_plan_excludes_equal_height_secondary_tracks() {
        let eb_plan = ferrocast_golive::twitch_eb::Plan {
            config_id: "cfg".into(),
            url: "rtmps://ingest/app/key".into(),
            encodes: vec![
                ferrocast_golive::twitch_eb::Encode {
                    kind: "obs_x264".into(),
                    width: 1920,
                    height: 1080,
                    framerate: Some(ferrocast_golive::twitch_eb::Fps { numerator: 60, denominator: 1 }),
                    settings: serde_json::json!({ "bitrate": 6000 }),
                    canvas_index: 0,
                },
                ferrocast_golive::twitch_eb::Encode {
                    kind: "obs_x264".into(),
                    width: 1920,
                    height: 1080,
                    framerate: Some(ferrocast_golive::twitch_eb::Fps { numerator: 30, denominator: 1 }),
                    settings: serde_json::json!({ "bitrate": 4500 }),
                    canvas_index: 0,
                },
            ],
            warning: None,
        };
        let main = MainEncode::from_canvas(Canvas::Main, 1920, 1080, 60, 6_000, VideoCodec::H264);
        let spec = TwitchEbSpec::from_plan(&eb_plan, &main).unwrap();
        assert!(spec.extra_tracks.is_empty(), "equal-height encode must not duplicate top track");
    }

    #[test]
    fn pass2_scaled_main_does_not_share_with_recording_and_bills_extra_session() {
        let mut budget = PcBudget::with_limits(BudgetSource::HwDefault, Some(300.0), Some(2));
        budget.record_shares_main = true;
        let plan = plan_encodes(&PlannerInput {
            main: base_1440p60(),
            vertical: None,
            vertical_cut: false,
            destinations: vec![
                dest("youtube", "YouTube", Service::Youtube, Canvas::Main),
                dest("kick", "Kick", Service::Kick, Canvas::Main),
            ],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget,
        });
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].id, GROUP_MAIN);
        assert_eq!((plan.groups[0].key.width, plan.groups[0].key.height), (1920, 1080));
        assert_eq!(plan.budget.used_sessions, 2, "scaled main cannot share unscaled canvas recording");
        assert!(
            (plan.budget.used_mpx_s - 345.6).abs() < 0.2,
            "expected scaled 1080p60 stream + 1440p60 recording mpx/s (~345.6), got {}",
            plan.budget.used_mpx_s
        );
        assert_eq!(plan.budget.status, BudgetStatus::Over);
    }

    #[test]
    fn coalesce_pass_merges_auto_fitted_strict_cbr_into_custom_non_strict_cbr() {
        let mut yt = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        yt.quality = DestinationQuality::Custom(CustomQuality {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 5_000,
            codec: Some(VideoCodec::H264),
        });
        let twitch = dest("twitch", "Twitch", Service::Twitch, Canvas::Main);
        let vert = dest("vert", "Vertical", Service::Youtube, Canvas::Vertical);
        let vertical = MainEncode::from_canvas(Canvas::Vertical, 1080, 1920, 60, 6_000, VideoCodec::H264);
        let mut main = base_1440p60();
        main.strict_cbr = false;
        main.vbr = false;
        let plan = plan_encodes(&PlannerInput {
            main,
            vertical: Some(vertical),
            vertical_cut: false,
            destinations: vec![yt, twitch, vert],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::with_limits(BudgetSource::NvencDefault, Some(500.0), Some(2)),
        });
        assert!(plan.destinations["twitch"].coalesced, "Twitch auto-fit must coalesce into matching Custom shape");
        assert_eq!(plan.destinations["twitch"].group_id, plan.destinations["youtube"].group_id);
        assert!(plan.groups.iter().find(|g| g.id == plan.destinations["youtube"].group_id).unwrap().key.strict_cbr);
        assert_eq!(plan.groups.len(), 2);
        assert!(!plan.destinations["vert"].shared_cap_fallback);
    }

    #[test]
    fn whip_folds_keyframe_and_codec_constraints_into_main_without_orphan_group() {
        let yt = dest("youtube", "YouTube", Service::Youtube, Canvas::Main);
        let mut whip = dest("whip", "WebRTC", Service::Custom, Canvas::Main);
        whip.direct = Some(DirectKind::Whip);

        let mut main_kf4 = base_1440p60();
        main_kf4.keyframe_s = 4;
        main_kf4.no_bframes = true;
        let p_kf = plan_encodes(&PlannerInput {
            main: main_kf4,
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt.clone(), whip.clone()],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p_kf.groups.len(), 1, "4s main + WHIP must share single main group: {:?}", p_kf.groups);
        assert_eq!(p_kf.groups[0].key.keyframe_s, 2);
        assert_eq!(p_kf.destinations["whip"].group_id, GROUP_MAIN);

        let mut main_av1 = base_1440p60();
        main_av1.codec = VideoCodec::Av1;
        main_av1.no_bframes = true;
        let p_av1 = plan_encodes(&PlannerInput {
            main: main_av1,
            vertical: None,
            vertical_cut: false,
            destinations: vec![yt, whip],
            ignore_service_limits: false,
            twitch_eb: None,
            whip_simulcast: false,
            budget: PcBudget::apple_silicon(2),
        });
        assert_eq!(p_av1.groups.len(), 1, "AV1 main + WHIP must share single H.264 main group: {:?}", p_av1.groups);
        assert_eq!(p_av1.groups[0].key.codec, VideoCodec::H264);
        assert_eq!(p_av1.destinations["whip"].group_id, GROUP_MAIN);
    }
}
