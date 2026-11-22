//! Auto-config encoder benchmark scoring and pick logic (Roadmap Phase 2 #16).
//!
//! Pure, deterministic functions that evaluate measured encoder benchmark runs and combine them
//! with upload bandwidth and platform ceilings (Twitch 6 Mbps, Kick 8 Mbps, multi-platform share)
//! to recommend encoder, preset, resolution scale, frame rate, and bitrate with clear headroom
//! explanations.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::probe::{self, HEADROOM, Service};

/// Maximum average frame encode budget fraction that leaves healthy headroom (28% spare).
pub const MAX_FRAME_BUDGET_RATIO: f64 = 0.72;
/// Maximum 95th-percentile frame encode time relative to the frame interval.
pub const MAX_P95_BUDGET_RATIO: f64 = 1.05;
/// Maximum host CPU utilization (%) before software encoding starves games/compositor.
pub const MAX_CPU_PCT: f64 = 85.0;
/// Maximum host GPU utilization (%) before GPU scenes/games starve.
pub const MAX_GPU_PCT: f64 = 90.0;

/// Target usage mode for auto-configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoConfigMode {
    #[default]
    Stream,
    Record,
}

/// Hardware or software encoder family under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderFamily {
    VideoToolbox,
    Nvenc,
    Qsv,
    Amf,
    MediaFoundation,
    X264,
}

impl EncoderFamily {
    /// Hardware acceleration preference rank (lower is preferred).
    fn hw_rank(self) -> u8 {
        match self {
            Self::VideoToolbox => 0,
            Self::Nvenc => 1,
            Self::Qsv => 2,
            Self::Amf => 3,
            Self::MediaFoundation => 4,
            Self::X264 => 5,
        }
    }

    pub fn is_hardware(self) -> bool {
        !matches!(self, Self::X264)
    }

    pub fn studio_encoder_id(self) -> &'static str {
        match self {
            Self::VideoToolbox => "com.apple.videotoolbox.videoencoder.ave.avc",
            Self::Nvenc | Self::Qsv | Self::Amf | Self::MediaFoundation => "ffmpeg_hardware",
            Self::X264 => "obs_x264",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::VideoToolbox => "Apple VideoToolbox",
            Self::Nvenc => "NVIDIA NVENC",
            Self::Qsv => "Intel Quick Sync",
            Self::Amf => "AMD AMF",
            Self::MediaFoundation => "Hardware (Media Foundation)",
            Self::X264 => "x264 (Software)",
        }
    }
}

/// What limited the chosen recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitedBy {
    Headroom,
    Upload,
    PlatformCap,
    Balanced,
}

/// Raw measured counters for one benchmark candidate step.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateMeasurement {
    pub encoder_backend: String,
    pub studio_encoder_id: String,
    pub label: String,
    pub family: EncoderFamily,
    pub preset: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub frames_tested: u32,
    pub encode_ms_avg: f64,
    pub encode_ms_p95: f64,
    pub encode_ms_max: f64,
    pub fps_achieved: f64,
    pub skipped_frames: u64,
    pub lagged_frames: u64,
    pub cpu_pct: Option<f64>,
    pub gpu_pct: Option<f64>,
    pub elapsed_ms: u64,
}

/// Evaluated candidate step with pass/fail verdict and plain-English explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchCandidate {
    pub encoder_backend: String,
    pub studio_encoder_id: String,
    pub label: String,
    pub family: EncoderFamily,
    pub hardware: bool,
    pub preset: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub frames_tested: u32,
    pub encode_ms_avg: f64,
    pub encode_ms_p95: f64,
    pub encode_ms_max: f64,
    pub budget_ms: f64,
    pub fps_achieved: f64,
    pub headroom_pct: f64,
    pub skipped_frames: u64,
    pub lagged_frames: u64,
    pub cpu_pct: Option<f64>,
    pub gpu_pct: Option<f64>,
    pub elapsed_ms: u64,
    pub passed: bool,
    pub reason: String,
}

/// Destination info needed to combine upload bandwidth and platform ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformTarget {
    pub name: String,
    pub service: Service,
    #[serde(default)]
    pub enhanced: bool,
}

/// Inputs required to select the best configuration from benchmark results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickInput {
    pub mode: AutoConfigMode,
    pub fps_preference: u32,
    pub canvas: [u32; 2],
    pub upload_kbps: Option<u32>,
    pub platforms: Vec<PlatformTarget>,
}

/// Final recommendation produced from benchmark measurements + upload check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRecommendation {
    pub encoder: String,
    pub video_encoder: Option<String>,
    pub studio_encoder_id: String,
    pub encoder_settings: Map<String, Value>,
    pub label: String,
    pub preset: Option<String>,
    pub scale: Option<[u32; 2]>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub headroom_pct: f64,
    pub limited_by: LimitedBy,
    pub why: String,
}

/// Evaluates a single benchmark run against fps target, headroom, skipped/lagged frames, and CPU/GPU limits.
pub fn evaluate_candidate(m: CandidateMeasurement) -> BenchCandidate {
    let fps = m.fps.max(1);
    let budget_ms = 1000.0 / f64::from(fps);
    let headroom_pct = ((1.0 - (m.encode_ms_avg / budget_ms)) * 100.0).clamp(-100.0, 100.0);
    let max_lag = u64::from(m.frames_tested) / 100;

    let fail_reason = if m.frames_tested < 10 {
        Some(format!("Too few frames measured ({}).", m.frames_tested))
    } else if m.skipped_frames > 0 {
        Some(format!("Encoder skipped {} frames because it fell behind.", m.skipped_frames))
    } else if m.lagged_frames > max_lag {
        Some(format!("{} lagged frames missed the {} fps deadline ({budget_ms:.1} ms).", m.lagged_frames, fps))
    } else if m.encode_ms_avg > budget_ms * MAX_FRAME_BUDGET_RATIO {
        let ceiling_ms = budget_ms * MAX_FRAME_BUDGET_RATIO;
        Some(format!(
            "{:.1} ms/frame exceeds {ceiling_ms:.1} ms headroom ceiling ({:.0}% headroom).",
            m.encode_ms_avg, headroom_pct
        ))
    } else if m.encode_ms_p95 > budget_ms * MAX_P95_BUDGET_RATIO {
        Some(format!(
            "Frame time spikes reached {:.1} ms (95th percentile), risking stutters at {fps} fps.",
            m.encode_ms_p95
        ))
    } else if m.fps_achieved < f64::from(fps) * 0.96 {
        Some(format!("Achieved {:.1} fps, below the {fps} fps target.", m.fps_achieved))
    } else if let Some(cpu) = m.cpu_pct.filter(|c| *c > MAX_CPU_PCT) {
        Some(format!("CPU load hit {cpu:.0}%, leaving too little room for games and scenes."))
    } else {
        m.gpu_pct
            .filter(|g| *g > MAX_GPU_PCT)
            .map(|gpu| format!("GPU load hit {gpu:.0}%, leaving too little room for scene compositing."))
    };

    let (passed, reason) = match fail_reason {
        Some(r) => (false, r),
        None => {
            let mut parts =
                vec![format!("Held {fps} fps at {:.1} ms/frame ({:.0}% headroom)", m.encode_ms_avg, headroom_pct)];
            if let Some(cpu) = m.cpu_pct {
                parts.push(format!("{cpu:.0}% CPU"));
            }
            if let Some(gpu) = m.gpu_pct {
                parts.push(format!("{gpu:.0}% GPU"));
            }
            (true, parts.join(", "))
        }
    };

    BenchCandidate {
        encoder_backend: m.encoder_backend,
        studio_encoder_id: m.studio_encoder_id,
        label: m.label,
        family: m.family,
        hardware: m.family.is_hardware(),
        preset: m.preset,
        width: m.width,
        height: m.height,
        fps,
        bitrate_kbps: m.bitrate_kbps,
        frames_tested: m.frames_tested,
        encode_ms_avg: round2(m.encode_ms_avg),
        encode_ms_p95: round2(m.encode_ms_p95),
        encode_ms_max: round2(m.encode_ms_max),
        budget_ms: round2(budget_ms),
        fps_achieved: round1(m.fps_achieved),
        headroom_pct: round1(headroom_pct),
        skipped_frames: m.skipped_frames,
        lagged_frames: m.lagged_frames,
        cpu_pct: m.cpu_pct.map(round1),
        gpu_pct: m.gpu_pct.map(round1),
        elapsed_ms: m.elapsed_ms,
        passed,
        reason,
    }
}

/// Scales `(cw, ch)` to target height `h`, rounding width to an even integer and never upscaling.
pub fn scale_for_height(canvas: [u32; 2], target_h: u32) -> [u32; 2] {
    let cw = canvas[0].max(128) & !1;
    let ch = canvas[1].max(72) & !1;
    if target_h >= ch {
        return [cw, ch];
    }
    let w = (((f64::from(cw) * f64::from(target_h)) / f64::from(ch)).round() as u32) & !1;
    [w.max(128), target_h.max(72) & !1]
}

/// Candidate `(width, height, fps)` ladder in descending quality order for benchmark runs.
pub fn candidate_ladder(canvas: [u32; 2], fps_preference: u32) -> Vec<(u32, u32, u32)> {
    let [cw, ch] = [canvas[0].max(128) & !1, canvas[1].max(72) & !1];
    let top_h = ch.min(1080);
    let mut tiers = Vec::new();
    let add = |list: &mut Vec<(u32, u32, u32)>, h: u32, fps: u32| {
        let [w, rh] = scale_for_height([cw, ch], h);
        let item = (w, rh, fps);
        if !list.contains(&item) {
            list.push(item);
        }
    };

    if fps_preference >= 60 {
        add(&mut tiers, top_h, 60);
        if top_h > 720 {
            add(&mut tiers, 720, 60);
        }
        add(&mut tiers, 720.min(top_h), 30);
        if top_h > 480 {
            add(&mut tiers, 480, 30);
        }
    } else {
        add(&mut tiers, top_h, 30);
        if top_h > 720 {
            add(&mut tiers, 720, 30);
        }
        if top_h > 480 {
            add(&mut tiers, 480, 30);
        }
    }
    tiers
}

/// Upload and platform cap ceiling for streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadCeiling {
    pub bitrate_kbps: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub limited_by: LimitedBy,
    pub cap_platform: Option<&'static str>,
    pub platform_count: usize,
}

/// Computes stream bitrate and resolution/fps ceiling from upload bandwidth and active platforms.
pub fn stream_ceiling(
    upload_kbps: Option<u32>,
    platforms: &[PlatformTarget],
    fps_preference: u32,
    canvas: [u32; 2],
) -> UploadCeiling {
    let limit = probe::effective_limit(platforms.iter().map(|p| (p.service, p.enhanced)));
    let cap = limit.map_or(12_000, |l| l.max_kbps);
    let n = platforms.len().max(1);
    let want_fps = if fps_preference >= 60 { 60 } else { 30 };

    let (bitrate_kbps, limited_by) = match upload_kbps {
        Some(up) => {
            // Keep 20% headroom on single/multi-platform upload matching `probe::HEADROOM` / wizard share.
            let share = (f64::from(up) * HEADROOM * 0.9375) / (n as f64); // 0.75 * upload / n
            let stepped = ((share / 500.0).floor() as u32) * 500;
            let picked = stepped.clamp(1500, cap);
            let why = if (share as u32) < cap && picked < cap {
                LimitedBy::Upload
            } else if cap < (share as u32) || (limit.is_some() && picked == cap) {
                LimitedBy::PlatformCap
            } else {
                LimitedBy::Balanced
            };
            (picked, why)
        }
        None => {
            let why = if limit.is_some() { LimitedBy::PlatformCap } else { LimitedBy::Balanced };
            (cap.min(6000), why)
        }
    };

    let (target_h, max_fps) = if want_fps == 60 {
        if bitrate_kbps >= 6000 {
            (1080, 60)
        } else if bitrate_kbps >= 3500 {
            (720, 60)
        } else if bitrate_kbps >= 2500 {
            (720, 30)
        } else {
            (480, 30)
        }
    } else if bitrate_kbps >= 4500 {
        (1080, 30)
    } else if bitrate_kbps >= 2500 {
        (720, 30)
    } else {
        (480, 30)
    };

    let [max_width, max_height] = scale_for_height(canvas, target_h);
    UploadCeiling {
        bitrate_kbps,
        max_width,
        max_height,
        max_fps,
        limited_by,
        cap_platform: limit.map(|l| l.name),
        platform_count: n,
    }
}

fn preset_quality_rank(preset: Option<&str>) -> u8 {
    match preset.map(str::to_ascii_lowercase).as_deref() {
        Some("veryslow") => 9,
        Some("slower") => 8,
        Some("slow") => 7,
        Some("medium") => 6,
        Some("fast") => 5,
        Some("faster") => 4,
        Some("veryfast") => 3,
        Some("superfast") => 2,
        Some("ultrafast") => 1,
        _ => 0,
    }
}

fn record_kbps_for(height: u32, fps: u32) -> u32 {
    match (height, fps >= 60) {
        (h, true) if h >= 1440 => 18_000,
        (h, false) if h >= 1440 => 12_000,
        (h, true) if h >= 1080 => 12_000,
        (h, false) if h >= 1080 => 8_000,
        (h, true) if h >= 720 => 6_500,
        (h, false) if h >= 720 => 4_500,
        _ => 2_500,
    }
}

fn stream_tier_cap(height: u32, fps: u32) -> u32 {
    match (height, fps >= 60) {
        (h, true) if h >= 1080 => 12_000,
        (h, false) if h >= 1080 => 8_000,
        (h, true) if h >= 720 => 6_000,
        (h, false) if h >= 720 => 4_500,
        _ => 3_000,
    }
}

/// Picks the best configuration from tested benchmark candidates combined with upload and platform limits.
pub fn pick_recommendation(candidates: &[BenchCandidate], input: &PickInput) -> Result<BenchRecommendation, String> {
    if candidates.is_empty() {
        return Err("No encoder candidates were tested.".into());
    }

    let [cw, ch] = [input.canvas[0].max(128) & !1, input.canvas[1].max(72) & !1];
    let ceiling = match input.mode {
        AutoConfigMode::Stream => {
            Some(stream_ceiling(input.upload_kbps, &input.platforms, input.fps_preference, [cw, ch]))
        }
        AutoConfigMode::Record => None,
    };

    let (cap_w, cap_h, cap_fps) = match &ceiling {
        Some(c) => (c.max_width, c.max_height, c.max_fps),
        None => {
            let [w, h] = scale_for_height([cw, ch], ch.min(1440));
            (w, h, if input.fps_preference >= 60 { 60 } else { 30 })
        }
    };

    // Candidates whose output matches or exceeds the upload ceiling cleanly prove capacity at `cap`.
    let effective_of = |c: &BenchCandidate| -> (u32, u32, u32) {
        let ew = c.width.min(cap_w) & !1;
        let eh = c.height.min(cap_h) & !1;
        let efps = c.fps.min(cap_fps);
        (ew, eh, efps)
    };

    let passing: Vec<&BenchCandidate> = candidates.iter().filter(|c| c.passed).collect();
    let best = if !passing.is_empty() {
        passing
            .into_iter()
            .max_by(|a, b| {
                let (aw, ah, af) = effective_of(a);
                let (bw, bh, bf) = effective_of(b);
                let apx = u64::from(aw) * u64::from(ah);
                let bpx = u64::from(bw) * u64::from(bh);
                apx.cmp(&bpx)
                    .then_with(|| af.cmp(&bf))
                    .then_with(|| a.hardware.cmp(&b.hardware))
                    .then_with(|| b.family.hw_rank().cmp(&a.family.hw_rank()))
                    .then_with(|| {
                        preset_quality_rank(a.preset.as_deref()).cmp(&preset_quality_rank(b.preset.as_deref()))
                    })
                    .then_with(|| a.headroom_pct.total_cmp(&b.headroom_pct))
            })
            .ok_or_else(|| "No encoder candidate found.".to_string())?
    } else {
        // Fallback: no candidate met headroom, so pick the lightest/least-dropping option.
        candidates
            .iter()
            .min_by(|a, b| {
                let ad = a.skipped_frames + a.lagged_frames;
                let bd = b.skipped_frames + b.lagged_frames;
                let aratio = a.encode_ms_avg / a.budget_ms.max(1.0);
                let bratio = b.encode_ms_avg / b.budget_ms.max(1.0);
                ad.cmp(&bd).then_with(|| aratio.total_cmp(&bratio)).then_with(|| b.hardware.cmp(&a.hardware))
            })
            .ok_or_else(|| "No encoder candidate found.".to_string())?
    };

    let (out_w, out_h, out_fps) = effective_of(best);
    let scale = if out_w == cw && out_h == ch { None } else { Some([out_w, out_h]) };

    let (video_kbps, limited_by) = match (&ceiling, best.passed) {
        (Some(ceil), true) => {
            let encoder_downgraded = out_h < ceil.max_height || out_fps < ceil.max_fps;
            let kbps = if encoder_downgraded {
                ceil.bitrate_kbps.min(stream_tier_cap(out_h, out_fps)).max(1500)
            } else {
                ceil.bitrate_kbps
            };
            let why_limit = if encoder_downgraded { LimitedBy::Headroom } else { ceil.limited_by };
            (kbps, why_limit)
        }
        (Some(ceil), false) => {
            let tier_max = stream_tier_cap(out_h, out_fps);
            (ceil.bitrate_kbps.min(tier_max).max(1500), LimitedBy::Headroom)
        }
        (None, _) => {
            let why_limit = if !best.passed || out_h < ch.min(1440) || out_fps < cap_fps {
                LimitedBy::Headroom
            } else {
                LimitedBy::Balanced
            };
            (record_kbps_for(out_h, out_fps), why_limit)
        }
    };

    let mut encoder_settings = Map::new();
    if !best.hardware
        && let Some(p) = &best.preset
    {
        encoder_settings.insert("preset".into(), Value::String(p.clone()));
    }

    let why = build_why(best, (out_w, out_h, out_fps), video_kbps, limited_by, ceiling.as_ref(), input);

    Ok(BenchRecommendation {
        encoder: if best.hardware { "hardware" } else { "software" }.into(),
        video_encoder: None,
        studio_encoder_id: best.studio_encoder_id.clone(),
        encoder_settings,
        label: best.label.clone(),
        preset: best.preset.clone(),
        scale,
        width: out_w,
        height: out_h,
        fps: out_fps,
        video_kbps,
        headroom_pct: best.headroom_pct,
        limited_by,
        why,
    })
}

fn build_why(
    best: &BenchCandidate,
    (out_w, out_h, out_fps): (u32, u32, u32),
    video_kbps: u32,
    limited_by: LimitedBy,
    ceiling: Option<&UploadCeiling>,
    input: &PickInput,
) -> String {
    let mbps = f64::from(video_kbps) / 1000.0;
    let enc_phrase = if !best.passed {
        format!(
            "All tested settings pushed the encoder near or past {} fps capacity; {} was lightest ({:.1} ms/frame).",
            best.fps, best.label, best.encode_ms_avg
        )
    } else if best.width != out_w || best.height != out_h || best.fps != out_fps {
        format!(
            "{} passed {}p{} at {:.1} ms/frame ({:.0}% headroom); output sized to {}×{} @ {} fps.",
            best.label, best.height, best.fps, best.encode_ms_avg, best.headroom_pct, out_w, out_h, out_fps
        )
    } else {
        format!(
            "{} held {}p{} at {:.1} ms/frame ({:.0}% headroom).",
            best.label, out_h, out_fps, best.encode_ms_avg, best.headroom_pct
        )
    };

    match ceiling {
        Some(ceil) => {
            let up_phrase = match input.upload_kbps {
                Some(up) if ceil.platform_count > 1 => {
                    format!(" {:.1} Mbps upload shared by {} platforms", f64::from(up) / 1000.0, ceil.platform_count)
                }
                Some(up) => format!(" {:.1} Mbps upload measured", f64::from(up) / 1000.0),
                None => String::new(),
            };
            let limit_phrase = match limited_by {
                LimitedBy::Upload => format!(", stepped bitrate to {mbps:.1} Mbps so upload stays smooth."),
                LimitedBy::PlatformCap => format!(
                    ", capped bitrate at {mbps:.1} Mbps for {}.",
                    ceil.cap_platform.unwrap_or("platform limits")
                ),
                LimitedBy::Headroom => {
                    format!(" Targeted {mbps:.1} Mbps at {out_h}p{out_fps} to match encoder headroom.")
                }
                LimitedBy::Balanced => format!(" Set {mbps:.1} Mbps."),
            };
            format!("{enc_phrase}{up_phrase}{limit_phrase}")
        }
        None => format!("{enc_phrase} Local recording set to {mbps:.1} Mbps."),
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meas(
        family: EncoderFamily,
        preset: Option<&str>,
        w: u32,
        h: u32,
        fps: u32,
        avg_ms: f64,
        cpu: Option<f64>,
    ) -> BenchCandidate {
        let backend = match family {
            EncoderFamily::VideoToolbox => "videotoolbox",
            EncoderFamily::Nvenc => "h264_nvenc",
            EncoderFamily::Qsv => "h264_qsv",
            EncoderFamily::Amf => "h264_amf",
            EncoderFamily::MediaFoundation => "h264_mf",
            EncoderFamily::X264 => "libx264",
        };
        let label = match preset {
            Some(p) => format!("x264 ({p})"),
            None => family.display_name().to_string(),
        };
        evaluate_candidate(CandidateMeasurement {
            encoder_backend: backend.into(),
            studio_encoder_id: family.studio_encoder_id().into(),
            label,
            family,
            preset: preset.map(String::from),
            width: w,
            height: h,
            fps,
            bitrate_kbps: 6000,
            frames_tested: 120,
            encode_ms_avg: avg_ms,
            encode_ms_p95: avg_ms * 1.18,
            encode_ms_max: avg_ms * 1.45,
            fps_achieved: (fps as f64).min(1000.0 / avg_ms),
            skipped_frames: 0,
            lagged_frames: 0,
            cpu_pct: cpu,
            gpu_pct: if family.is_hardware() { Some(24.0) } else { None },
            elapsed_ms: 2000,
        })
    }

    fn twitch() -> PlatformTarget {
        PlatformTarget { name: "Twitch".into(), service: Service::Twitch, enhanced: false }
    }

    fn youtube() -> PlatformTarget {
        PlatformTarget { name: "YouTube".into(), service: Service::Youtube, enhanced: false }
    }

    fn kick() -> PlatformTarget {
        PlatformTarget { name: "Kick".into(), service: Service::Kick, enhanced: false }
    }

    #[test]
    fn apple_silicon_videotoolbox_beats_x264_and_caps_at_twitch_6mbps() {
        let candidates = vec![
            meas(EncoderFamily::VideoToolbox, None, 1920, 1080, 60, 2.2, Some(18.0)),
            meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 60, 7.5, Some(52.0)),
        ];
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(40_000),
                platforms: vec![twitch()],
            },
        )
        .unwrap();
        assert_eq!(rec.encoder, "hardware");
        assert_eq!(rec.studio_encoder_id, "com.apple.videotoolbox.videoencoder.ave.avc");
        assert_eq!(rec.video_kbps, 6000);
        assert_eq!(rec.scale, None);
        assert_eq!(rec.fps, 60);
        assert_eq!(rec.limited_by, LimitedBy::PlatformCap);
        assert!(rec.why.contains("VideoToolbox"));
    }

    #[test]
    fn nvenc_multi_platform_splits_upload_and_picks_nvenc() {
        let candidates = vec![
            meas(EncoderFamily::Nvenc, None, 1920, 1080, 60, 3.1, Some(15.0)),
            meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 60, 6.2, Some(48.0)),
        ];
        // 12 Mbps * 0.75 / 2 = 4500 kbps each -> scales 1080p canvas down to 720p60
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(12_000),
                platforms: vec![twitch(), youtube()],
            },
        )
        .unwrap();
        assert_eq!(rec.encoder, "hardware");
        assert_eq!(rec.studio_encoder_id, "ffmpeg_hardware");
        assert_eq!(rec.video_kbps, 4500);
        assert_eq!(rec.scale, Some([1280, 720]));
        assert_eq!(rec.fps, 60);
        assert_eq!(rec.limited_by, LimitedBy::Upload);
        assert!(rec.why.contains("shared by 2 platforms"));
    }

    #[test]
    fn qsv_steps_down_from_1080p60_to_720p60_when_1080p60_blows_headroom() {
        let candidates = vec![
            meas(EncoderFamily::Qsv, None, 1920, 1080, 60, 14.2, Some(22.0)), // >12ms (only 15% headroom -> fail)
            meas(EncoderFamily::Qsv, None, 1280, 720, 60, 5.4, Some(19.0)),   // passes 67% headroom
            meas(EncoderFamily::X264, Some("ultrafast"), 1280, 720, 60, 8.1, Some(68.0)),
        ];
        assert!(!candidates[0].passed);
        assert!(candidates[1].passed);
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(25_000),
                platforms: vec![kick()],
            },
        )
        .unwrap();
        assert_eq!(rec.encoder, "hardware");
        assert_eq!(rec.scale, Some([1280, 720]));
        assert_eq!(rec.fps, 60);
        assert_eq!(rec.limited_by, LimitedBy::Headroom);
    }

    #[test]
    fn amf_with_skipped_frames_loses_to_healthy_software_x264() {
        let amf_bad = evaluate_candidate(CandidateMeasurement {
            encoder_backend: "h264_amf".into(),
            studio_encoder_id: "ffmpeg_hardware".into(),
            label: "AMD AMF".into(),
            family: EncoderFamily::Amf,
            preset: None,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 6000,
            frames_tested: 120,
            encode_ms_avg: 6.0,
            encode_ms_p95: 9.0,
            encode_ms_max: 24.0,
            fps_achieved: 55.0,
            skipped_frames: 4,
            lagged_frames: 0,
            cpu_pct: Some(20.0),
            gpu_pct: Some(50.0),
            elapsed_ms: 2100,
        });
        assert!(!amf_bad.passed);
        let x264_good = meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 60, 9.2, Some(54.0));
        let rec = pick_recommendation(
            &[amf_bad, x264_good],
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(30_000),
                platforms: vec![youtube()],
            },
        )
        .unwrap();
        assert_eq!(rec.encoder, "software");
        assert_eq!(rec.preset.as_deref(), Some("veryfast"));
        assert_eq!(rec.encoder_settings.get("preset").and_then(Value::as_str), Some("veryfast"));
    }

    #[test]
    fn cpu_only_picks_highest_quality_x264_preset_with_headroom_and_cpu_ceiling() {
        // `faster` saturates CPU (91%) and `medium` exceeds time budget; `veryfast` passes cleanly.
        let candidates = vec![
            meas(EncoderFamily::X264, Some("medium"), 1920, 1080, 60, 15.5, Some(78.0)),
            meas(EncoderFamily::X264, Some("faster"), 1920, 1080, 60, 10.4, Some(91.0)),
            meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 60, 8.8, Some(64.0)),
            meas(EncoderFamily::X264, Some("superfast"), 1920, 1080, 60, 6.1, Some(49.0)),
            meas(EncoderFamily::X264, Some("ultrafast"), 1920, 1080, 60, 4.2, Some(37.0)),
        ];
        assert!(!candidates[0].passed);
        assert!(!candidates[1].passed);
        assert!(candidates[2].passed);
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(30_000),
                platforms: vec![twitch()],
            },
        )
        .unwrap();
        assert_eq!(rec.encoder, "software");
        assert_eq!(rec.preset.as_deref(), Some("veryfast"));
        assert_eq!(rec.scale, None);
        assert_eq!(rec.fps, 60);
    }

    #[test]
    fn weak_cpu_laptop_steps_down_to_720p30_superfast() {
        let candidates = vec![
            meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 60, 26.0, Some(94.0)),
            meas(EncoderFamily::X264, Some("veryfast"), 1280, 720, 60, 14.5, Some(82.0)),
            meas(EncoderFamily::X264, Some("veryfast"), 1280, 720, 30, 25.0, Some(88.0)),
            meas(EncoderFamily::X264, Some("superfast"), 1280, 720, 30, 13.5, Some(61.0)),
            meas(EncoderFamily::X264, Some("ultrafast"), 1280, 720, 30, 10.0, Some(50.0)),
        ];
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(20_000),
                platforms: vec![youtube()],
            },
        )
        .unwrap();
        assert_eq!(rec.scale, Some([1280, 720]));
        assert_eq!(rec.fps, 30);
        assert_eq!(rec.preset.as_deref(), Some("superfast"));
        assert!(rec.video_kbps <= 4500);
    }

    #[test]
    fn potato_machine_when_everything_fails_picks_lightest_and_warns() {
        let candidates = vec![
            meas(EncoderFamily::X264, Some("veryfast"), 1920, 1080, 30, 48.0, Some(99.0)),
            meas(EncoderFamily::X264, Some("ultrafast"), 852, 480, 30, 27.0, Some(89.0)),
        ];
        assert!(!candidates[0].passed);
        assert!(!candidates[1].passed);
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 30,
                canvas: [1920, 1080],
                upload_kbps: Some(10_000),
                platforms: vec![twitch()],
            },
        )
        .unwrap();
        assert_eq!(rec.scale, Some([852, 480]));
        assert_eq!(rec.fps, 30);
        assert_eq!(rec.preset.as_deref(), Some("ultrafast"));
        assert_eq!(rec.limited_by, LimitedBy::Headroom);
        assert!(rec.why.contains("near or past 30 fps capacity"));
    }

    #[test]
    fn record_mode_ignores_slow_upload_and_picks_higher_bitrate() {
        let candidates = vec![meas(EncoderFamily::VideoToolbox, None, 1920, 1080, 60, 2.5, Some(15.0))];
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Record,
                fps_preference: 60,
                canvas: [1920, 1080],
                upload_kbps: Some(1_500),
                platforms: vec![twitch()],
            },
        )
        .unwrap();
        assert_eq!(rec.scale, None);
        assert_eq!(rec.fps, 60);
        assert_eq!(rec.video_kbps, 12_000);
        assert_eq!(rec.limited_by, LimitedBy::Balanced);
    }

    #[test]
    fn gpu_saturated_hardware_candidate_is_rejected_for_clean_software_or_lower_tier() {
        let gpu_full = evaluate_candidate(CandidateMeasurement {
            encoder_backend: "h264_nvenc".into(),
            studio_encoder_id: "ffmpeg_hardware".into(),
            label: "NVIDIA NVENC".into(),
            family: EncoderFamily::Nvenc,
            preset: None,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 6000,
            frames_tested: 120,
            encode_ms_avg: 5.0,
            encode_ms_p95: 8.0,
            encode_ms_max: 11.0,
            fps_achieved: 60.0,
            skipped_frames: 0,
            lagged_frames: 0,
            cpu_pct: Some(25.0),
            gpu_pct: Some(96.0),
            elapsed_ms: 2000,
        });
        assert!(!gpu_full.passed);
        assert!(gpu_full.reason.contains("GPU load"));
    }

    #[test]
    fn tight_upload_scales_4k_canvas_to_even_dimensions_without_upscaling() {
        let candidates = vec![meas(EncoderFamily::Nvenc, None, 1920, 1080, 60, 2.8, Some(14.0))];
        let rec = pick_recommendation(
            &candidates,
            &PickInput {
                mode: AutoConfigMode::Stream,
                fps_preference: 60,
                canvas: [3840, 2160],
                upload_kbps: Some(3_000),
                platforms: vec![youtube()],
            },
        )
        .unwrap();
        assert_eq!(rec.fps, 30);
        assert_eq!(rec.video_kbps, 2000);
        assert_eq!(rec.scale, Some([852, 480]));
        assert_eq!(rec.limited_by, LimitedBy::Upload);
    }

    #[test]
    fn candidate_ladder_keeps_720p_canvas_under_native_bounds() {
        let tiers = candidate_ladder([1280, 720], 60);
        assert_eq!(tiers, vec![(1280, 720, 60), (1280, 720, 30), (852, 480, 30)]);
    }
}
