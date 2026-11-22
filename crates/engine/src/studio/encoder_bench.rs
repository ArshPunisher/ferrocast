//! Auto-config encoder stress benchmark and rollback state machine (Phase 2 #16).
//!
//! Runs a cancellable (~20-30 s) hardware-first (`VideoToolbox`, `NVENC`, `QSV`, `AMF`, then
//! `libx264` presets) encode benchmark over a canvas-shaped synthetic/composited workload, combines
//! results with the Go live upload check, applies recommended `StudioSettings` with a snapshot, and
//! enforces a 30 s Keep/Undo confirmation window plus a 1-minute dropped/lagged-frame watchdog.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::golive::{Canvas, UploadCheck};
use ferrocast_golive::bench::{
    AutoConfigMode, BenchCandidate, BenchRecommendation, CandidateMeasurement, EncoderFamily, PickInput,
    PlatformTarget, candidate_ladder, evaluate_candidate, pick_recommendation, stream_ceiling,
};
use ferrocore::{
    Blend, Bounds, Compositor, Content, Effect, Filter, Item, Picture, Placement, Scene, ffenc,
    video::{
        Frame, H264Profile, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder, X264_PRESETS,
        dropped_frames, x264_index,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Notify;

use crate::Engine;
use crate::EngineError;
use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::now;
use crate::studio_host::StudioSettings;

/// Automatic rollback timeout when user does nothing after Apply.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
/// First-minute stability watch window for dropped or lagged frames after Apply.
pub const WATCH_WINDOW: Duration = Duration::from_secs(60);
/// Minimum delta of dropped + lagged + skipped frames in the first minute that triggers auto-rollback.
const FIRST_MINUTE_DROP_THRESHOLD: u64 = 5;

/// Benchmark lifecycle state reported over REST and WebSocket (`auto_config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoConfigStateKind {
    Idle,
    Running,
    Failed,
    Done,
    Applied,
    RolledBack,
}

/// Active phase while benchmark is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoConfigPhase {
    Upload,
    Discovering,
    Encoding,
    Picking,
}

/// Why settings were rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackTrigger {
    UserUndo,
    Timeout,
    DroppedFrames,
}

/// Compact summary of applied or restored Studio output settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedSummary {
    pub encoder: String,
    pub video_encoder: Option<String>,
    pub encoder_settings: Map<String, Value>,
    pub scale: Option<[u32; 2]>,
    pub fps: u32,
    pub video_kbps: u32,
    pub label: String,
}

impl AppliedSummary {
    fn from_settings(s: &StudioSettings, label: Option<String>) -> Self {
        let lbl = label.unwrap_or_else(|| {
            if let Some(v) = &s.video_encoder {
                v.clone()
            } else if s.encoder == "software" {
                s.encoder_settings
                    .get("preset")
                    .and_then(Value::as_str)
                    .map_or_else(|| "x264".into(), |p| format!("x264 ({p})"))
            } else {
                "Hardware encoder".into()
            }
        });
        Self {
            encoder: s.encoder.clone(),
            video_encoder: s.video_encoder.clone(),
            encoder_settings: s.encoder_settings.clone(),
            scale: s.scale,
            fps: s.fps,
            video_kbps: s.video_kbps,
            label: lbl,
        }
    }
}

/// Public snapshot of the auto-config state machine.
#[derive(Debug, Clone, Serialize)]
pub struct AutoConfigView {
    pub state: AutoConfigStateKind,
    pub job_id: Option<String>,
    pub mode: AutoConfigMode,
    pub phase: Option<AutoConfigPhase>,
    pub progress_pct: u8,
    pub current_label: Option<String>,
    pub elapsed_ms: u64,
    pub estimated_total_ms: u64,
    pub candidates: Vec<BenchCandidate>,
    pub upload_check: Option<UploadCheck>,
    pub upload_error: Option<String>,
    pub picked: Option<BenchRecommendation>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub applied: Option<AppliedSummary>,
    pub confirmed: bool,
    pub confirm_remaining_s: u32,
    pub watch_remaining_s: u32,
    pub first_minute_drops: u64,
    pub first_minute_lags: u64,
    pub rollback_trigger: Option<RollbackTrigger>,
    pub rollback_reason: Option<String>,
    pub restored: Option<AppliedSummary>,
}

/// Request payload for `POST /api/v1/studio/engine/auto-config/benchmark`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BenchmarkReq {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub fps_preference: Option<u32>,
    #[serde(default)]
    pub include_upload: Option<bool>,
    #[serde(default)]
    pub max_seconds: Option<u64>,
}

/// Request payload for `POST /api/v1/studio/engine/auto-config/apply`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApplyReq {
    #[serde(default)]
    pub encoder: Option<String>,
    #[serde(default)]
    pub video_encoder: Option<Option<String>>,
    #[serde(default)]
    pub encoder_settings: Option<Map<String, Value>>,
    #[serde(default)]
    pub scale: Option<Option<[u32; 2]>>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub video_kbps: Option<u32>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Tuning knobs for benchmark speed (production ~22-28 s, tests <2 s).
#[derive(Debug, Clone, Copy)]
pub struct BenchTiming {
    pub total_encode_budget: Duration,
    pub step_duration: Duration,
    pub burst_frames: u32,
    pub warmup_frames: u32,
    pub confirm_timeout: Duration,
    pub watch_window: Duration,
    pub poll_interval: Duration,
}

impl Default for BenchTiming {
    fn default() -> Self {
        Self {
            total_encode_budget: Duration::from_secs(20),
            step_duration: Duration::from_millis(1800),
            burst_frames: 24,
            warmup_frames: 5,
            confirm_timeout: CONFIRM_TIMEOUT,
            watch_window: WATCH_WINDOW,
            poll_interval: Duration::from_millis(500),
        }
    }
}

impl BenchTiming {
    /// Fast timing used by automated real-hardware integration tests.
    pub fn fast_test() -> Self {
        Self {
            total_encode_budget: Duration::from_secs(12),
            step_duration: Duration::from_millis(250),
            burst_frames: 10,
            warmup_frames: 3,
            confirm_timeout: Duration::from_millis(180),
            watch_window: Duration::from_millis(400),
            poll_interval: Duration::from_millis(25),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct StatBaseline {
    lagged: Option<u64>,
    skipped: Option<u64>,
    dropped: Option<u64>,
    accumulated_lags: u64,
    accumulated_drops: u64,
}

impl StatBaseline {
    fn observe(&mut self, lagged: u64, skipped: u64, dropped: u64) -> (u64, u64) {
        let step_counter = |prev: &mut Option<u64>, cur: u64| -> u64 {
            match *prev {
                None => {
                    *prev = Some(cur);
                    0
                }
                Some(p) if cur >= p => {
                    *prev = Some(cur);
                    cur - p
                }
                // Stream or engine restarted mid-window: new baseline starts at cur.
                Some(_) => {
                    *prev = Some(cur);
                    cur
                }
            }
        };
        let dl = step_counter(&mut self.lagged, lagged) + step_counter(&mut self.skipped, skipped);
        let dd = step_counter(&mut self.dropped, dropped);
        self.accumulated_lags = self.accumulated_lags.saturating_add(dl);
        self.accumulated_drops = self.accumulated_drops.saturating_add(dd);
        (self.accumulated_drops, self.accumulated_lags)
    }
}

#[derive(Debug)]
struct AppliedInner {
    snapshot: StudioSettings,
    summary: AppliedSummary,
    confirmed: bool,
    applied_mono: Instant,
    confirm_deadline: Instant,
    watch_deadline: Instant,
    baseline: StatBaseline,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    picked: Option<BenchRecommendation>,
    mode: AutoConfigMode,
    duration_ms: Option<u64>,
}

#[derive(Debug)]
struct RolledBackInner {
    trigger: RollbackTrigger,
    reason: String,
    restored: AppliedSummary,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    picked: Option<BenchRecommendation>,
    mode: AutoConfigMode,
    duration_ms: Option<u64>,
}

#[derive(Debug)]
struct RunningInner {
    job_id: String,
    mode: AutoConfigMode,
    phase: AutoConfigPhase,
    progress_pct: u8,
    current_label: Option<String>,
    started_mono: Instant,
    estimated_total_ms: u64,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    cancel: Arc<AtomicBool>,
    cancelling: bool,
    cancel_notify: Arc<Notify>,
    worker_done: tokio::sync::watch::Receiver<bool>,
}

#[derive(Debug)]
struct DoneInner {
    mode: AutoConfigMode,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    upload_error: Option<String>,
    picked: BenchRecommendation,
    duration_ms: u64,
}

#[derive(Debug)]
struct FailedInner {
    mode: AutoConfigMode,
    error: String,
    candidates: Vec<BenchCandidate>,
}

#[derive(Debug, Default)]
enum MachineState {
    #[default]
    Idle,
    Running(RunningInner),
    Failed(FailedInner),
    Done(DoneInner),
    Applied(Box<AppliedInner>),
    RolledBack(RolledBackInner),
}

/// Engine-held state for encoder benchmarking and setting rollback.
#[derive(Debug)]
pub struct AutoConfigSlot {
    seq: u64,
    state: MachineState,
    timing: BenchTiming,
    #[cfg(test)]
    test_session_alive: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    test_step_delay: Option<Duration>,
}

impl Default for AutoConfigSlot {
    fn default() -> Self {
        Self {
            seq: 0,
            state: MachineState::Idle,
            timing: BenchTiming::default(),
            #[cfg(test)]
            test_session_alive: None,
            #[cfg(test)]
            test_step_delay: None,
        }
    }
}

impl AutoConfigSlot {
    pub fn set_timing_for_test(&mut self, timing: BenchTiming) {
        self.timing = timing;
    }

    #[cfg(test)]
    fn set_worker_hook_for_test(&mut self, alive: Arc<AtomicBool>, delay: Duration) {
        self.test_session_alive = Some(alive);
        self.test_step_delay = Some(delay);
    }

    fn next_job_id(&mut self) -> String {
        self.seq = self.seq.saturating_add(1);
        format!("bench-{}-{}", now().0, self.seq)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, MachineState::Running(_))
    }

    pub fn view(&self) -> AutoConfigView {
        let t_now = Instant::now();
        match &self.state {
            MachineState::Idle => AutoConfigView {
                state: AutoConfigStateKind::Idle,
                job_id: None,
                mode: AutoConfigMode::Stream,
                phase: None,
                progress_pct: 0,
                current_label: None,
                elapsed_ms: 0,
                estimated_total_ms: 0,
                candidates: Vec::new(),
                upload_check: None,
                upload_error: None,
                picked: None,
                duration_ms: None,
                error: None,
                applied: None,
                confirmed: false,
                confirm_remaining_s: 0,
                watch_remaining_s: 0,
                first_minute_drops: 0,
                first_minute_lags: 0,
                rollback_trigger: None,
                rollback_reason: None,
                restored: None,
            },
            MachineState::Running(r) => AutoConfigView {
                state: AutoConfigStateKind::Running,
                job_id: Some(r.job_id.clone()),
                mode: r.mode,
                phase: Some(r.phase),
                progress_pct: r.progress_pct,
                current_label: r.current_label.clone(),
                elapsed_ms: r.started_mono.elapsed().as_millis() as u64,
                estimated_total_ms: r.estimated_total_ms,
                candidates: r.candidates.clone(),
                upload_check: r.upload_check.clone(),
                upload_error: None,
                picked: None,
                duration_ms: None,
                error: None,
                applied: None,
                confirmed: false,
                confirm_remaining_s: 0,
                watch_remaining_s: 0,
                first_minute_drops: 0,
                first_minute_lags: 0,
                rollback_trigger: None,
                rollback_reason: None,
                restored: None,
            },
            MachineState::Failed(f) => AutoConfigView {
                state: AutoConfigStateKind::Failed,
                job_id: None,
                mode: f.mode,
                phase: None,
                progress_pct: 0,
                current_label: None,
                elapsed_ms: 0,
                estimated_total_ms: 0,
                candidates: f.candidates.clone(),
                upload_check: None,
                upload_error: None,
                picked: None,
                duration_ms: None,
                error: Some(f.error.clone()),
                applied: None,
                confirmed: false,
                confirm_remaining_s: 0,
                watch_remaining_s: 0,
                first_minute_drops: 0,
                first_minute_lags: 0,
                rollback_trigger: None,
                rollback_reason: None,
                restored: None,
            },
            MachineState::Done(d) => AutoConfigView {
                state: AutoConfigStateKind::Done,
                job_id: None,
                mode: d.mode,
                phase: None,
                progress_pct: 100,
                current_label: None,
                elapsed_ms: d.duration_ms,
                estimated_total_ms: d.duration_ms,
                candidates: d.candidates.clone(),
                upload_check: d.upload_check.clone(),
                upload_error: d.upload_error.clone(),
                picked: Some(d.picked.clone()),
                duration_ms: Some(d.duration_ms),
                error: None,
                applied: None,
                confirmed: false,
                confirm_remaining_s: 0,
                watch_remaining_s: 0,
                first_minute_drops: 0,
                first_minute_lags: 0,
                rollback_trigger: None,
                rollback_reason: None,
                restored: None,
            },
            MachineState::Applied(a) => {
                let confirm_rem = if a.confirmed || t_now >= a.confirm_deadline {
                    0
                } else {
                    ceil_secs(a.confirm_deadline.saturating_duration_since(t_now))
                };
                let watch_rem = if t_now >= a.watch_deadline {
                    0
                } else {
                    ceil_secs(a.watch_deadline.saturating_duration_since(t_now))
                };
                AutoConfigView {
                    state: AutoConfigStateKind::Applied,
                    job_id: None,
                    mode: a.mode,
                    phase: None,
                    progress_pct: 100,
                    current_label: None,
                    elapsed_ms: a.applied_mono.elapsed().as_millis() as u64,
                    estimated_total_ms: 0,
                    candidates: a.candidates.clone(),
                    upload_check: a.upload_check.clone(),
                    upload_error: None,
                    picked: a.picked.clone(),
                    duration_ms: a.duration_ms,
                    error: None,
                    applied: Some(a.summary.clone()),
                    confirmed: a.confirmed,
                    confirm_remaining_s: confirm_rem,
                    watch_remaining_s: watch_rem,
                    first_minute_drops: a.baseline.accumulated_drops,
                    first_minute_lags: a.baseline.accumulated_lags,
                    rollback_trigger: None,
                    rollback_reason: None,
                    restored: None,
                }
            }
            MachineState::RolledBack(rb) => AutoConfigView {
                state: AutoConfigStateKind::RolledBack,
                job_id: None,
                mode: rb.mode,
                phase: None,
                progress_pct: 100,
                current_label: None,
                elapsed_ms: 0,
                estimated_total_ms: 0,
                candidates: rb.candidates.clone(),
                upload_check: rb.upload_check.clone(),
                upload_error: None,
                picked: rb.picked.clone(),
                duration_ms: rb.duration_ms,
                error: None,
                applied: None,
                confirmed: false,
                confirm_remaining_s: 0,
                watch_remaining_s: 0,
                first_minute_drops: 0,
                first_minute_lags: 0,
                rollback_trigger: Some(rb.trigger),
                rollback_reason: Some(rb.reason.clone()),
                restored: Some(rb.restored.clone()),
            },
        }
    }
}

fn ceil_secs(d: Duration) -> u32 {
    if d.is_zero() {
        return 0;
    }
    let ms = d.as_millis();
    ms.div_ceil(1000).min(u128::from(u32::MAX)) as u32
}

/// RAII temporary workspace deleted unconditionally on completion, cancellation, failure, or panic.
struct BenchTempDir(PathBuf);

impl BenchTempDir {
    fn create(base: Option<PathBuf>, job_id: &str) -> Result<Self, String> {
        let root = base.unwrap_or_else(std::env::temp_dir).join("auto-config-tmp");
        let dir = root.join(job_id);
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't prepare scratch folder: {e}"))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for BenchTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if let Some(parent) = self.0.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some("auto-config-tmp")
            && parent.read_dir().is_ok_and(|mut r| r.next().is_none())
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Discovered encoder candidate on this machine (hardware first, then x264 presets).
#[derive(Debug, Clone)]
struct DiscoveredEncoder {
    backend: String,
    studio_encoder_id: String,
    label: String,
    family: EncoderFamily,
    ffmpeg_name: Option<String>,
    preset: Option<String>,
}

fn discover_encoders_cancellable(cancel: &AtomicBool) -> Vec<DiscoveredEncoder> {
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if cancel.load(Ordering::Acquire) {
            return out;
        }
        out.push(DiscoveredEncoder {
            backend: "videotoolbox".into(),
            studio_encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
            label: "Apple VideoToolbox".into(),
            family: EncoderFamily::VideoToolbox,
            ffmpeg_name: None,
            preset: None,
        });
    }

    let ff_list = ffenc::probe_encoders_cancellable(Some(cancel));
    for enc in &ff_list {
        match enc.as_str() {
            "h264_nvenc" => out.push(DiscoveredEncoder {
                backend: "h264_nvenc".into(),
                studio_encoder_id: "ffmpeg_hardware".into(),
                label: "NVIDIA NVENC".into(),
                family: EncoderFamily::Nvenc,
                ffmpeg_name: Some(enc.clone()),
                preset: None,
            }),
            "h264_qsv" => out.push(DiscoveredEncoder {
                backend: "h264_qsv".into(),
                studio_encoder_id: "ffmpeg_hardware".into(),
                label: "Intel Quick Sync".into(),
                family: EncoderFamily::Qsv,
                ffmpeg_name: Some(enc.clone()),
                preset: None,
            }),
            "h264_amf" => out.push(DiscoveredEncoder {
                backend: "h264_amf".into(),
                studio_encoder_id: "ffmpeg_hardware".into(),
                label: "AMD AMF".into(),
                family: EncoderFamily::Amf,
                ffmpeg_name: Some(enc.clone()),
                preset: None,
            }),
            "h264_mf" => out.push(DiscoveredEncoder {
                backend: "h264_mf".into(),
                studio_encoder_id: "ffmpeg_hardware".into(),
                label: "Hardware (Media Foundation)".into(),
                family: EncoderFamily::MediaFoundation,
                ffmpeg_name: Some(enc.clone()),
                preset: None,
            }),
            // On non-macOS, probe `h264_videotoolbox` if present; on macOS native VT is already first.
            "h264_videotoolbox" if cfg!(not(target_os = "macos")) => out.push(DiscoveredEncoder {
                backend: "h264_videotoolbox".into(),
                studio_encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
                label: "Apple VideoToolbox".into(),
                family: EncoderFamily::VideoToolbox,
                ffmpeg_name: Some(enc.clone()),
                preset: None,
            }),
            _ => {}
        }
    }

    if ff_list.iter().any(|e| e == "libx264") {
        for preset in ["faster", "veryfast", "superfast", "ultrafast"] {
            out.push(DiscoveredEncoder {
                backend: "libx264".into(),
                studio_encoder_id: "obs_x264".into(),
                label: format!("x264 ({preset})"),
                family: EncoderFamily::X264,
                ffmpeg_name: Some("libx264".into()),
                preset: Some(preset.into()),
            });
        }
    }
    out
}

/// Dynamic canvas-shaped NV12 frame set: composited with `ferrocore::Compositor` when GPU is
/// available, or CPU-synthesized moving gradients/patterns in headless environments.
struct BenchFrames {
    y_stride: usize,
    uv_stride: usize,
    frames: Vec<(Vec<u8>, Vec<u8>)>,
}

impl BenchFrames {
    fn build(compositor: Option<&Compositor>, width: u32, height: u32, cancel: &AtomicBool) -> Result<Self, StepAbort> {
        const COUNT: u32 = 8;
        if let Some(c) = compositor
            && let Ok(rendered) = render_compositor_frames(c, width, height, COUNT, cancel)
        {
            return Ok(rendered);
        }
        synthetic_nv12_frames(width, height, COUNT as usize, cancel)
    }

    fn frame(&self, idx: usize) -> Frame<'_> {
        let (y, uv) = &self.frames[idx % self.frames.len()];
        Frame::Nv12 { y, y_stride: self.y_stride, uv, uv_stride: self.uv_stride }
    }
}

fn gradient_rgba(w: u32, h: u32, shift: u32) -> Picture {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = (((x + shift * 17) * 255) / w.max(1)) as u8;
            let g = (((y + shift * 11) * 255) / h.max(1)) as u8;
            let b = (((x ^ y).wrapping_add(shift * 29)) & 0xff) as u8;
            px.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Picture { width: w, height: h, rgba: px, premultiplied: false }
}

fn render_compositor_frames(
    c: &Compositor,
    width: u32,
    height: u32,
    count: u32,
    cancel: &AtomicBool,
) -> Result<BenchFrames, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    c.upload(1, &Picture::solid(width, height, [18, 22, 34, 255])).map_err(|e| e.to_string())?;
    c.upload(2, &gradient_rgba(width, height, 1)).map_err(|e| e.to_string())?;
    let logo_sz = (width.min(height) / 3).max(64);
    c.upload(4, &gradient_rgba(logo_sz, logo_sz, 3)).map_err(|e| e.to_string())?;
    let cam_w = (width * 2 / 3).max(64) & !1;
    let cam_h = (height * 2 / 3).max(64) & !1;
    let cams: Vec<Picture> = (0..count).map(|k| gradient_rgba(cam_w, cam_h, k + 2)).collect();

    let up = |id: u64, w: u32, h: u32| Content::Uploaded { id, width: w, height: h, premultiplied: false };
    let plain = |content: Content, placement: Placement| Item {
        content,
        effects: Vec::new(),
        placement,
        filter: Filter::Disable,
        blend: Blend::Normal,
        srgb_off: false,
        linear: true,
        opacity: 1.0,
    };

    let mut out = Vec::with_capacity(count as usize);
    let mut y_stride = width as usize;
    let mut uv_stride = width as usize;

    for (i, cam) in cams.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        c.upload(3, cam).map_err(|e| e.to_string())?;
        let offset = (i as f32) * 18.0;
        let scene = Scene {
            width,
            height,
            items: vec![
                plain(up(1, width, height), Placement::default()),
                plain(
                    up(2, width, height),
                    Placement {
                        bounds: Bounds::Fit,
                        bounds_width: width as f32,
                        bounds_height: height as f32,
                        ..Default::default()
                    },
                ),
                Item {
                    effects: vec![
                        Effect::RoundedCorners { radius: 24.0 },
                        Effect::Border { width: 4.0, rgba: [1.0, 0.55, 0.1, 1.0], radius: 24.0 },
                    ],
                    ..plain(
                        up(3, cam_w, cam_h),
                        Placement {
                            x: (width as f32) * 0.62,
                            y: (height as f32) * 0.56,
                            scale_x: 0.45,
                            scale_y: 0.45,
                            ..Default::default()
                        },
                    )
                },
                Item {
                    filter: Filter::Bicubic,
                    ..plain(
                        up(4, logo_sz, logo_sz),
                        Placement {
                            x: 48.0 + offset,
                            y: 48.0,
                            rotation: 6.0,
                            scale_x: 0.5,
                            scale_y: 0.5,
                            crop: [8, 8, 8, 8],
                            ..Default::default()
                        },
                    )
                },
                plain(
                    Content::Picture(Picture::solid((width * 3 / 5).max(64), (height / 10).max(24), [0, 0, 0, 175])),
                    Placement { x: 40.0 + offset * 0.5, y: (height as f32) * 0.82, ..Default::default() },
                ),
            ],
        };
        let f = c.render_nv12(&scene).map_err(|e| e.to_string())?;
        y_stride = width as usize;
        uv_stride = width as usize;
        out.push((f.y, f.uv));
    }

    Ok(BenchFrames { y_stride, uv_stride, frames: out })
}

fn synthetic_nv12_frames(width: u32, height: u32, count: usize, cancel: &AtomicBool) -> Result<BenchFrames, StepAbort> {
    let w = width as usize;
    let h = height as usize;
    let mut frames = Vec::with_capacity(count);
    for k in 0..count {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepAbort::Cancelled);
        }
        let mut y = vec![16u8; w * h];
        let mut uv = vec![128u8; w * (h / 2)];
        let bar_x = (k * (w / count.max(1))) % w;
        for row in 0..h {
            let off = row * w;
            let base = ((row + k * 13) & 0x7f) as u8;
            for col in 0..w {
                let checker = (((row >> 4) ^ ((col + k * 7) >> 4)) & 1) as u8;
                let bar = if col >= bar_x && col < (bar_x + w / 8).min(w) { 120u8 } else { 0u8 };
                y[off + col] = (24 + base / 2 + checker * 44).saturating_add(bar).min(235);
            }
        }
        for row in 0..(h / 2) {
            let off = row * w;
            for col in (0..w).step_by(2) {
                uv[off + col] = (80 + ((col + k * 11) % 96) as u8).min(240);
                uv[off + col + 1] = (80 + ((row + k * 9) % 96) as u8).min(240);
            }
        }
        frames.push((y, uv));
    }
    Ok(BenchFrames { y_stride: w, uv_stride: w, frames })
}

/// Parses `ioreg -r -d 1 -w 0 -c IOAccelerator` stdout for macOS GPU utilization %.
pub fn parse_macos_gpu_pct(out: &str) -> Option<f64> {
    for key in ["\"Device Utilization %\"=", "\"Renderer Utilization %\"="] {
        for part in out.split(key).skip(1) {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if let Ok(v) = digits.parse::<f64>()
                && (0.0..=100.0).contains(&v)
            {
                return Some(v);
            }
        }
    }
    None
}

/// Parses `top -l 2 -n 0 -F -R` stdout for total macOS CPU load % (`user + sys`), taking the final sample.
pub fn parse_macos_top_cpu_pct(out: &str) -> Option<f64> {
    let mut latest = None;
    for line in out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("CPU usage:") {
            let mut user = None;
            let mut sys = None;
            let mut idle = None;
            for seg in rest.split(',') {
                let s = seg.trim();
                let num = s.split('%').next().and_then(|v| v.trim().parse::<f64>().ok());
                if s.contains("user") {
                    user = num;
                } else if s.contains("sys") {
                    sys = num;
                } else if s.contains("idle") {
                    idle = num;
                }
            }
            if let (Some(u), Some(s)) = (user, sys) {
                latest = Some((u + s).clamp(0.0, 100.0));
            } else if let Some(i) = idle {
                latest = Some((100.0 - i).clamp(0.0, 100.0));
            }
        }
    }
    latest
}

#[cfg(target_os = "linux")]
fn read_proc_stat_totals() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let vals: Vec<u64> = line.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    if vals.len() < 4 {
        return None;
    }
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0);
    let total: u64 = vals.iter().sum();
    Some((total.saturating_sub(idle), total))
}

fn read_os_gpu_pct() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/ioreg")
            .args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"])
            .output()
            .ok()?;
        if out.status.success() {
            return parse_macos_gpu_pct(&String::from_utf8_lossy(&out.stdout));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(out) = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(v) = text.lines().next().and_then(|l| l.trim().parse::<f64>().ok())
                && (0.0..=100.0).contains(&v)
            {
                return Some(v);
            }
        }
    }
    None
}

fn read_os_cpu_pct() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/top").args(["-l", "2", "-n", "0", "-F", "-R"]).output().ok()?;
        if out.status.success() {
            return parse_macos_top_cpu_pct(&String::from_utf8_lossy(&out.stdout));
        }
    }
    None
}

struct LoadMonitor {
    host_cpu_supported: bool,
    rt: Option<tokio::runtime::Handle>,
    #[cfg(target_os = "linux")]
    prev_proc: Option<(u64, u64)>,
}

impl LoadMonitor {
    fn new(host_cpu_supported: bool, rt: Option<tokio::runtime::Handle>) -> Self {
        Self {
            host_cpu_supported,
            rt,
            #[cfg(target_os = "linux")]
            prev_proc: read_proc_stat_totals(),
        }
    }

    fn sample(&mut self, engine: &Engine) -> (Option<f64>, Option<f64>) {
        if self.host_cpu_supported
            && let Ok(host) = engine.studio_host()
            && let Some(rt) = &self.rt
        {
            let (c, g) = rt.block_on(host.machine_load());
            if c.is_some() || g.is_some() {
                return (c, g.or_else(read_os_gpu_pct));
            }
        }
        #[cfg(target_os = "linux")]
        let cpu = if let (Some((b0, t0)), Some((b1, t1))) = (self.prev_proc, read_proc_stat_totals()) {
            self.prev_proc = Some((b1, t1));
            let dt = t1.saturating_sub(t0);
            if dt > 0 { Some(((b1.saturating_sub(b0)) as f64 / dt as f64) * 100.0) } else { None }
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let cpu = read_os_cpu_pct();

        (cpu, read_os_gpu_pct())
    }
}

type LoadSample = (Instant, Option<f64>, Option<f64>);
type LoadSampleLog = Arc<std::sync::Mutex<Vec<LoadSample>>>;

/// Samples OS CPU and GPU load on a helper thread so the encoder pacing loop never stalls on `top` or `ioreg`.
struct BackgroundLoadSampler {
    stop: Arc<AtomicBool>,
    samples: LoadSampleLog,
}

impl BackgroundLoadSampler {
    fn start(
        host_cpu_supported: bool,
        rt: Option<tokio::runtime::Handle>,
        engine: Engine,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop_t = stop.clone();
        let samples_t = samples.clone();
        let _ = std::thread::Builder::new().name("auto-config-load".into()).spawn(move || {
            if let Some(g0) = read_os_gpu_pct() {
                samples_t.lock().unwrap_or_else(|p| p.into_inner()).push((Instant::now(), None, Some(g0)));
            }
            let mut mon = LoadMonitor::new(host_cpu_supported, rt);
            while !stop_t.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                let (c, g) = mon.sample(&engine);
                if stop_t.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                    break;
                }
                if c.is_some() || g.is_some() {
                    let mut guard = samples_t.lock().unwrap_or_else(|p| p.into_inner());
                    guard.push((Instant::now(), c, g));
                    if guard.len() > 256 {
                        let excess = guard.len() - 256;
                        guard.drain(..excess);
                    }
                }
                for _ in 0..4 {
                    if stop_t.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        Self { stop, samples }
    }

    fn window_or_latest(&self, from: Instant, to: Instant) -> (Option<f64>, Option<f64>) {
        let guard = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        let mut cpus = Vec::new();
        let mut gpus = Vec::new();
        for &(ts, c, g) in guard.iter() {
            if ts >= from && ts <= to {
                if let Some(v) = c {
                    cpus.push(v);
                }
                if let Some(v) = g {
                    gpus.push(v);
                }
            }
        }
        if cpus.is_empty()
            && let Some(c) = guard.iter().rev().find_map(|s| s.1)
        {
            cpus.push(c);
        }
        if gpus.is_empty()
            && let Some(g) = guard.iter().rev().find_map(|s| s.2)
        {
            gpus.push(g);
        }
        let avg = |xs: &[f64]| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
        (avg(&cpus), avg(&gpus))
    }
}

impl Drop for BackgroundLoadSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Debug)]
enum StepAbort {
    Cancelled,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
fn run_candidate_step(
    compositor: Option<&Compositor>,
    cache: &mut std::collections::HashMap<(u32, u32), BenchFrames>,
    cand: &DiscoveredEncoder,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    timing: BenchTiming,
    cancel: &Arc<AtomicBool>,
    sampler: &BackgroundLoadSampler,
) -> Result<BenchCandidate, StepAbort> {
    if cancel.load(Ordering::Relaxed) {
        return Err(StepAbort::Cancelled);
    }
    let step_start = Instant::now();
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry((width, height)) {
        e.insert(BenchFrames::build(compositor, width, height, cancel)?);
    }
    let frames_data = match cache.get(&(width, height)) {
        Some(f) => f,
        None => return Err(StepAbort::Failed("Benchmark frame workload missing.".into())),
    };

    let cfg = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width,
        height,
        fps_num: fps.max(1),
        fps_den: 1,
        bitrate_kbps: bitrate_kbps.clamp(500, 60_000),
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: !cand.family.is_hardware(),
        profile: H264Profile::High,
        bframes: false,
        preset: cand.preset.as_deref().and_then(|p| x264_index(&X264_PRESETS, p)),
        tune: None,
    };

    // Phase 1: Synchronous/backpressured throughput measurement to find true ms/frame.
    let mut cap_enc = VideoEncoder::for_benchmark(cfg, cand.ffmpeg_name.as_deref(), Some(2), Some(cancel.clone()))
        .map_err(|e| StepAbort::Failed(format!("{} failed to start: {e}", cand.label)))?;
    let total_burst = timing.burst_frames.max(timing.warmup_frames + 5);
    let mut frame_times_ms = Vec::with_capacity(total_burst as usize);
    let mut steady_start = None;
    let frame_us = 1_000_000i64 / i64::from(fps.max(1));

    for i in 0..total_burst {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepAbort::Cancelled);
        }
        if i == timing.warmup_frames {
            steady_start = Some(Instant::now());
        }
        let t0 = Instant::now();
        let key = i % (fps.max(1) * 2) == 0;
        if let Err(e) = cap_enc.encode(frames_data.frame(i as usize), i64::from(i) * frame_us, key) {
            if cancel.load(Ordering::Acquire) {
                return Err(StepAbort::Cancelled);
            }
            return Err(StepAbort::Failed(format!("{} encode error: {e}", cand.label)));
        }
        let dt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if i >= timing.warmup_frames {
            frame_times_ms.push(dt_ms);
        }
    }
    if cancel.load(Ordering::Acquire) {
        drop(cap_enc);
        return Err(StepAbort::Cancelled);
    }
    let _ = cap_enc.flush();
    let steady_elapsed_ms = steady_start.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
    let steady_count = (total_burst - timing.warmup_frames).max(1) as f64;
    let throughput_avg_ms = (steady_elapsed_ms / steady_count).max(0.05);
    drop(cap_enc);

    if cancel.load(Ordering::Relaxed) {
        return Err(StepAbort::Cancelled);
    }

    // Phase 2: Real-time paced stability pass (verifies skipped & lagged frames without OS sampler stalls).
    let mut rt_enc = VideoEncoder::for_benchmark(cfg, cand.ffmpeg_name.as_deref(), None, Some(cancel.clone()))
        .map_err(|e| StepAbort::Failed(format!("{} failed to open realtime pass: {e}", cand.label)))?;
    let rt_warmup = timing.warmup_frames.clamp(1, 3);
    for w_i in 0..rt_warmup {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepAbort::Cancelled);
        }
        rt_enc
            .encode(frames_data.frame(w_i as usize), i64::from(w_i) * frame_us, w_i == 0)
            .map_err(|e| StepAbort::Failed(format!("{} realtime warmup error: {e}", cand.label)))?;
    }

    let dropped_before = dropped_frames();
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let target_rt_frames = ((timing.step_duration.as_secs_f64() * f64::from(fps.max(1))).round() as u32).max(10);

    let rt_start = Instant::now();
    let mut next_due = rt_start + frame_interval;
    let mut lagged_frames = 0u64;
    let mut sent_rt = 0u32;

    for i in 0..target_rt_frames {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepAbort::Cancelled);
        }
        let pts_idx = rt_warmup + i;
        let work_t0 = Instant::now();
        let key = pts_idx.is_multiple_of(fps.max(1) * 2);
        rt_enc
            .encode(frames_data.frame(pts_idx as usize), i64::from(pts_idx) * frame_us, key)
            .map_err(|e| StepAbort::Failed(format!("{} realtime encode error: {e}", cand.label)))?;
        let work_ms = work_t0.elapsed().as_secs_f64() * 1000.0;
        if work_ms > frame_interval.as_secs_f64() * 1000.0 {
            lagged_frames += 1;
        }
        sent_rt += 1;

        let now_t = Instant::now();
        if next_due > now_t {
            let mut rem = next_due - now_t;
            while !rem.is_zero() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(StepAbort::Cancelled);
                }
                let slice = rem.min(Duration::from_millis(20));
                std::thread::sleep(slice);
                rem = next_due.saturating_duration_since(Instant::now());
            }
        } else if now_t - next_due > frame_interval {
            lagged_frames += 1;
            if now_t - next_due > frame_interval * 2 {
                next_due = now_t;
            }
        }
        next_due += frame_interval;
    }
    if cancel.load(Ordering::Acquire) {
        drop(rt_enc);
        return Err(StepAbort::Cancelled);
    }
    let _ = rt_enc.flush();
    let rt_elapsed = rt_start.elapsed().as_secs_f64().max(0.001);
    let skipped_frames = rt_enc.skipped().saturating_add(dropped_frames().saturating_sub(dropped_before));
    drop(rt_enc);
    let rt_end = Instant::now();

    // For pipelined native hardware encoders, steady wall throughput reflects true frame budget
    // pressure better than per-submit duration; take max so neither hides latency spikes.
    let measured_avg_sum: f64 = frame_times_ms.iter().sum::<f64>() / (frame_times_ms.len().max(1) as f64);
    let encode_ms_avg = throughput_avg_ms.max(measured_avg_sum);
    frame_times_ms.sort_by(f64::total_cmp);
    let encode_ms_p95 = percentile_sorted(&frame_times_ms, 0.95).max(encode_ms_avg);
    let encode_ms_max = frame_times_ms.last().copied().unwrap_or(encode_ms_avg).max(encode_ms_p95);
    let achieved_rt_fps = (f64::from(sent_rt) / rt_elapsed).min(1000.0 / encode_ms_avg);
    let (cpu_pct, gpu_pct) = sampler.window_or_latest(rt_start, rt_end);

    Ok(evaluate_candidate(CandidateMeasurement {
        encoder_backend: cand.backend.clone(),
        studio_encoder_id: cand.studio_encoder_id.clone(),
        label: cand.label.clone(),
        family: cand.family,
        preset: cand.preset.clone(),
        width,
        height,
        fps,
        bitrate_kbps,
        frames_tested: sent_rt.saturating_add(total_burst).saturating_add(rt_warmup),
        encode_ms_avg,
        encode_ms_p95,
        encode_ms_max,
        fps_achieved: achieved_rt_fps,
        skipped_frames,
        lagged_frames,
        cpu_pct,
        gpu_pct,
        elapsed_ms: step_start.elapsed().as_millis() as u64,
    }))
}

fn validate_scale(scale: Option<[u32; 2]>) -> Result<(), EngineError> {
    if let Some([w, h]) = scale
        && (w < 128 || h < 72 || w > 7680 || h > 4320 || !w.is_multiple_of(2) || !h.is_multiple_of(2))
    {
        return Err(EngineError::Invalid("scale: even sizes from 128x72 to 7680x4320.".into()));
    }
    Ok(())
}

struct BenchmarkTaskParams {
    job_id: String,
    mode: AutoConfigMode,
    fps_pref: u32,
    include_upload: bool,
    canvas: [u32; 2],
    timing: BenchTiming,
    cancel: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    done_tx: tokio::sync::watch::Sender<bool>,
}

enum WorkerOutcome {
    Cancelled,
    Failed(AutoConfigMode, String, Vec<BenchCandidate>),
    Done(WorkerDone),
}

struct WorkerDone {
    mode: AutoConfigMode,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    picked: BenchRecommendation,
}

impl Engine {
    /// Current state of the auto-config encoder benchmark and rollback watchdog.
    pub fn auto_config_status(&self) -> AutoConfigView {
        self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner()).view()
    }

    /// Configures faster benchmark/rollback intervals in tests.
    pub fn set_auto_config_timing_for_test(&self, timing: BenchTiming) {
        self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner()).set_timing_for_test(timing);
    }

    fn emit_auto_config_frame(&self, view: &AutoConfigView) {
        self.studio_frame("auto_config", view);
    }

    /// Refuses with HTTP 409 when a Show or any live output (stream, recording, replay,
    /// virtual camera, NDI, delayed drain) is active, failing closed on status errors.
    async fn ensure_benchmark_idle(&self) -> Result<(), EngineError> {
        self.ensure_outputs_idle(
            "testing the encoder",
            "Stop streaming and recording before testing the encoder.".into(),
            "End your active stream, recording, replay buffer, virtual camera, or NDI output first so the benchmark never affects live viewers."
                .into(),
        )
        .await
    }

    /// Refuses with HTTP 409 if an encoder benchmark is currently active (C2).
    pub(crate) fn ensure_no_benchmark_running(&self) -> Result<(), EngineError> {
        let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_running() {
            return Err(EngineError::Refused {
                status: 409,
                code: "benchmark_running",
                message: "An encoder benchmark is running.".into(),
                hint: "Wait for the benchmark to finish or cancel it first.".into(),
                retry_after: None,
            });
        }
        Ok(())
    }

    /// Starts an encoder stress benchmark (and upload test in `stream` mode unless skipped).
    pub async fn auto_config_benchmark(&self, req: BenchmarkReq) -> Result<AutoConfigView, EngineError> {
        let mode = match req.mode.as_deref().unwrap_or("stream") {
            "stream" => AutoConfigMode::Stream,
            "record" => AutoConfigMode::Record,
            _ => return Err(EngineError::Invalid("mode: stream or record.".into())),
        };
        let fps_pref = req.fps_preference.unwrap_or(60);
        if fps_pref != 30 && fps_pref != 60 {
            return Err(EngineError::Invalid("fps_preference: 30 or 60.".into()));
        }
        let max_seconds = req.max_seconds.unwrap_or(24);
        if !(2..=35).contains(&max_seconds) {
            return Err(EngineError::Invalid("max_seconds: 2 to 35.".into()));
        }
        let include_upload = req.include_upload.unwrap_or(mode == AutoConfigMode::Stream);

        let _op = self.studio().auto_config_op.lock().await;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(Notify::new());
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);

        let (job_id, timing, view, canvas) = {
            // Hold `output_gate` across the idle check, settings read, and claim transition so no
            // output start can race through TOCTOU between checking idle and marking Running (C2).
            let _gate = self.studio().output_gate.lock().await;
            {
                let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                if slot.is_running() {
                    return Err(EngineError::Conflict("An encoder benchmark is already running.".into()));
                }
            }
            self.ensure_benchmark_idle().await?;

            let canvas = if let Ok(host) = self.studio_host() {
                let s = host.settings().await;
                [s.width, s.height]
            } else {
                [1920, 1080]
            };

            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_running() {
                return Err(EngineError::Conflict("An encoder benchmark is already running.".into()));
            }
            let job_id = slot.next_job_id();
            let mut timing = slot.timing;
            if req.max_seconds.is_some() {
                timing.total_encode_budget = Duration::from_secs(max_seconds);
                timing.step_duration =
                    timing.step_duration.min(Duration::from_millis((max_seconds * 1000 / 4).max(200)));
            }
            let phase = if include_upload && mode == AutoConfigMode::Stream {
                AutoConfigPhase::Upload
            } else {
                AutoConfigPhase::Discovering
            };
            slot.state = MachineState::Running(RunningInner {
                job_id: job_id.clone(),
                mode,
                phase,
                progress_pct: 5,
                current_label: Some(if phase == AutoConfigPhase::Upload {
                    "Testing upload speed…".into()
                } else {
                    "Checking available encoders…".into()
                }),
                started_mono: Instant::now(),
                estimated_total_ms: timing.total_encode_budget.as_millis() as u64
                    + if include_upload && mode == AutoConfigMode::Stream { 6000 } else { 0 },
                candidates: Vec::new(),
                upload_check: None,
                cancel: cancel.clone(),
                cancelling: false,
                cancel_notify: cancel_notify.clone(),
                worker_done: done_rx,
            });
            (job_id, timing, slot.view(), canvas)
        };
        self.emit_auto_config_frame(&view);

        let engine = self.clone();
        let params = BenchmarkTaskParams {
            job_id,
            mode,
            fps_pref,
            include_upload,
            canvas,
            timing,
            cancel,
            cancel_notify,
            done_tx,
        };
        tokio::spawn(async move {
            engine.run_benchmark_task(params).await;
        });

        Ok(view)
    }

    async fn run_benchmark_task(self, params: BenchmarkTaskParams) {
        struct WorkerDoneGuard(tokio::sync::watch::Sender<bool>);
        impl Drop for WorkerDoneGuard {
            fn drop(&mut self) {
                let _ = self.0.send(true);
            }
        }
        let job_id = params.job_id.clone();
        let done_guard = WorkerDoneGuard(params.done_tx.clone());
        let outcome = self.run_benchmark_task_inner(params).await;
        // All encoders, sampler threads, and temp folders are dropped before signalling `worker_done` (C3).
        drop(done_guard);
        match outcome {
            WorkerOutcome::Cancelled => self.finish_bench_cancelled(&job_id),
            WorkerOutcome::Failed(mode, error, candidates) => {
                self.finish_bench_failed(&job_id, mode, error, candidates);
            }
            WorkerOutcome::Done(done) => {
                let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                let elapsed_ms = if let MachineState::Running(r) = &slot.state
                    && r.job_id == job_id
                {
                    r.started_mono.elapsed().as_millis() as u64
                } else {
                    return;
                };
                slot.state = MachineState::Done(DoneInner {
                    mode: done.mode,
                    candidates: done.candidates,
                    upload_check: done.upload_check,
                    upload_error: None,
                    picked: done.picked,
                    duration_ms: elapsed_ms,
                });
                let view = slot.view();
                drop(slot);
                self.emit_auto_config_frame(&view);
            }
        }
    }

    async fn run_benchmark_task_inner(&self, params: BenchmarkTaskParams) -> WorkerOutcome {
        let BenchmarkTaskParams {
            job_id,
            mode,
            fps_pref,
            include_upload,
            canvas,
            timing,
            cancel,
            cancel_notify,
            done_tx: _,
        } = params;
        self.share_ffmpeg();
        let data_dir = self.golive_data_dir();
        let _temp_dir = match BenchTempDir::create(data_dir, &job_id) {
            Ok(d) => Some(d),
            Err(e) => return WorkerOutcome::Failed(mode, e, Vec::new()),
        };
        if let Some(d) = &_temp_dir {
            let marker = d.path().join("probe.lock");
            let _ = std::fs::write(&marker, job_id.as_bytes());
        }

        let mut upload_check: Option<UploadCheck> = None;
        if include_upload && mode == AutoConfigMode::Stream {
            let fut = self.golive_upload_check();
            tokio::pin!(fut);
            let res = tokio::select! {
                _ = cancel_notify.notified() => return WorkerOutcome::Cancelled,
                r = &mut fut => r,
            };
            match res {
                Ok(uc) => upload_check = Some(uc),
                Err(e) => return WorkerOutcome::Failed(mode, e.to_string(), Vec::new()),
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return WorkerOutcome::Cancelled;
        }

        // Gather enabled Go live main destinations so pick logic honours service caps & sharing.
        let platforms: Vec<PlatformTarget> = {
            let cfg = self.golive_config();
            let blocked = self.golive().blocked.lock().unwrap_or_else(|p| p.into_inner()).clone();
            cfg.destinations
                .iter()
                .filter(|d| d.enabled && d.canvas == Canvas::Main && !blocked.contains_key(&d.id))
                .map(|d| PlatformTarget {
                    name: d.name.clone().unwrap_or_else(|| format!("{:?}", d.kind)),
                    service: d.kind,
                    enhanced: d.enhanced || cfg.advanced.ignore_service_limits,
                })
                .collect()
        };

        let upload_kbps = upload_check.as_ref().map(|u| u.verdict.upload_kbps);
        let stream_ceil =
            (mode == AutoConfigMode::Stream).then(|| stream_ceiling(upload_kbps, &platforms, fps_pref, canvas));

        {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let MachineState::Running(r) = &mut slot.state
                && r.job_id == job_id
            {
                r.phase = AutoConfigPhase::Discovering;
                r.progress_pct = if include_upload { 25 } else { 10 };
                r.upload_check = upload_check.clone();
                r.current_label = Some("Preparing canvas load and probing encoders…".into());
                let view = slot.view();
                drop(slot);
                self.emit_auto_config_frame(&view);
            }
        }

        let start_encode = Instant::now();
        let engine_ref = self.clone();
        let job_clone = job_id.clone();
        let cancel_clone = cancel.clone();
        let ceil_clone = stream_ceil.clone();

        let host_cpu_supported = self.studio_host().ok().is_some();
        let rt_handle = tokio::runtime::Handle::try_current().ok();
        #[cfg(test)]
        let test_hook = {
            let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            slot.test_session_alive.clone().zip(slot.test_step_delay)
        };

        let res = tokio::task::spawn_blocking(move || -> Result<Vec<BenchCandidate>, String> {
            #[cfg(test)]
            struct TestSessionGuard(Arc<AtomicBool>);
            #[cfg(test)]
            impl Drop for TestSessionGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            #[cfg(test)]
            let _test_guard = test_hook.as_ref().map(|(alive, _)| {
                alive.store(true, Ordering::Release);
                TestSessionGuard(alive.clone())
            });
            #[cfg(test)]
            if let Some((_, delay)) = test_hook {
                std::thread::sleep(delay);
                if cancel_clone.load(Ordering::Acquire) {
                    return Err("cancelled".into());
                }
            }
            let encoders = discover_encoders_cancellable(&cancel_clone);
            if encoders.is_empty() {
                return Err(
                    "No working video encoder was found on this computer. Install FFmpeg or check your graphics drivers."
                        .into(),
                );
            }

            let compositor = Compositor::new().ok();
            let mut cache = std::collections::HashMap::new();
            let sampler =
                BackgroundLoadSampler::start(host_cpu_supported, rt_handle, engine_ref.clone(), cancel_clone.clone());
            let ladder = candidate_ladder(canvas, fps_pref);
            let deadline = start_encode + timing.total_encode_budget;
            let mut results: Vec<BenchCandidate> = Vec::new();
            let mut hw_encoders = Vec::new();
            let mut sw_encoders = Vec::new();
            for e in encoders {
                if e.family.is_hardware() {
                    hw_encoders.push(e);
                } else {
                    sw_encoders.push(e);
                }
            }

            let total_planned_steps = (hw_encoders.len() + sw_encoders.len().min(3)).max(1) as f64;
            let mut completed_steps = 0usize;

            let update_running = |label: String, list: &[BenchCandidate], done_n: usize| {
                let mut slot = engine_ref.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                if let MachineState::Running(r) = &mut slot.state
                    && r.job_id == job_clone
                {
                    r.phase = AutoConfigPhase::Encoding;
                    let base = if include_upload { 30.0 } else { 15.0 };
                    let pct = base + ((done_n as f64 / total_planned_steps) * (92.0 - base));
                    r.progress_pct = pct.clamp(15.0, 95.0) as u8;
                    r.current_label = Some(label);
                    r.candidates = list.to_vec();
                    let v = slot.view();
                    drop(slot);
                    engine_ref.emit_auto_config_frame(&v);
                }
            };

            let target_bitrate = ceil_clone.as_ref().map_or(8000, |c| c.bitrate_kbps);
            let mut best_viable_tier = ladder[0];

            // 1. Hardware encoders first (VideoToolbox / NVENC / QSV / AMF / MF).
            for hw in &hw_encoders {
                let mut hw_passed = false;
                for &(w, h, fps) in &ladder {
                    if cancel_clone.load(Ordering::Relaxed) {
                        return Err("cancelled".into());
                    }
                    if !results.is_empty() && Instant::now() >= deadline {
                        break;
                    }
                    let label = format!("{} ({w}×{h} @ {fps} fps)", hw.label);
                    update_running(format!("Stress testing {label}…"), &results, completed_steps);
                    match run_candidate_step(
                        compositor.as_ref(),
                        &mut cache,
                        hw,
                        w,
                        h,
                        fps,
                        target_bitrate,
                        timing,
                        &cancel_clone,
                        &sampler,
                    ) {
                        Ok(c) => {
                            let passed = c.passed;
                            results.push(c);
                            completed_steps += 1;
                            if passed {
                                hw_passed = true;
                                best_viable_tier = (w, h, fps);
                                if let Some(ceil) = &ceil_clone
                                    && (ceil.max_height < h || ceil.max_fps < fps)
                                    && Instant::now() + timing.step_duration <= deadline
                                {
                                    let up_label = format!("{} ({}×{} @ {} fps)", hw.label, ceil.max_width, ceil.max_height, ceil.max_fps);
                                    update_running(format!("Testing {up_label}…"), &results, completed_steps);
                                    if let Ok(uc) = run_candidate_step(
                                        compositor.as_ref(),
                                        &mut cache,
                                        hw,
                                        ceil.max_width,
                                        ceil.max_height,
                                        ceil.max_fps,
                                        ceil.bitrate_kbps,
                                        timing,
                                        &cancel_clone,
                                        &sampler,
                                    ) {
                                        results.push(uc);
                                    }
                                }
                                break;
                            }
                        }
                        Err(StepAbort::Cancelled) => return Err("cancelled".into()),
                        Err(StepAbort::Failed(msg)) => {
                            tracing::debug!(encoder = %hw.backend, error = %msg, "hardware benchmark step failed");
                            break;
                        }
                    }
                }
                if hw_passed && Instant::now() + timing.step_duration > deadline {
                    break;
                }
            }

            // 2. Software x264 presets (`faster`, `veryfast`, `superfast`, `ultrafast`) at viable tier.
            let has_passing_hw = results.iter().any(|c| c.hardware && c.passed);
            let mut sw_tier = best_viable_tier;
            if let Some(ceil) = &ceil_clone
                && ceil.max_height < sw_tier.1
            {
                sw_tier = (ceil.max_width, ceil.max_height, ceil.max_fps);
            }
            for sw in &sw_encoders {
                if cancel_clone.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                if !results.is_empty() && Instant::now() >= deadline {
                    break;
                }
                // If hardware already passed easily, sample two representative x264 presets for comparison.
                if has_passing_hw
                    && matches!(sw.preset.as_deref(), Some("faster" | "ultrafast"))
                    && results.iter().any(|c| !c.hardware)
                {
                    continue;
                }
                let (w, h, fps) = sw_tier;
                let label = format!("{} ({w}×{h} @ {fps} fps)", sw.label);
                update_running(format!("Stress testing {label}…"), &results, completed_steps);
                match run_candidate_step(
                    compositor.as_ref(),
                    &mut cache,
                    sw,
                    w,
                    h,
                    fps,
                    target_bitrate,
                    timing,
                    &cancel_clone,
                    &sampler,
                ) {
                    Ok(c) => {
                        let passed = c.passed;
                        results.push(c);
                        completed_steps += 1;
                        if !passed && !has_passing_hw {
                            // Step resolution down when software fails at current tier and no hardware works.
                            if let Some(next_tier) = ladder
                                .iter()
                                .copied()
                                .find(|&(nw, nh, nf)| (u64::from(nw) * u64::from(nh), nf) < (u64::from(w) * u64::from(h), fps))
                            {
                                sw_tier = next_tier;
                            }
                        }
                    }
                    Err(StepAbort::Cancelled) => return Err("cancelled".into()),
                    Err(StepAbort::Failed(msg)) => {
                        tracing::debug!(preset = ?sw.preset, error = %msg, "x264 preset benchmark step failed");
                    }
                }
            }

            if results.is_empty() {
                return Err("Every encoder candidate failed to run on this system.".into());
            }
            Ok(results)
        })
        .await;

        let candidates = match res {
            Ok(Ok(list)) => list,
            Ok(Err(e)) if e == "cancelled" => return WorkerOutcome::Cancelled,
            Ok(Err(e)) => return WorkerOutcome::Failed(mode, e, Vec::new()),
            Err(e) => {
                return WorkerOutcome::Failed(mode, format!("Benchmark worker stopped unexpectedly: {e}"), Vec::new());
            }
        };

        if cancel.load(Ordering::Relaxed) {
            return WorkerOutcome::Cancelled;
        }

        let pick_input = PickInput { mode, fps_preference: fps_pref, canvas, upload_kbps, platforms };
        let picked = match pick_recommendation(&candidates, &pick_input) {
            Ok(p) => p,
            Err(e) => return WorkerOutcome::Failed(mode, e, candidates),
        };

        WorkerOutcome::Done(WorkerDone { mode, candidates, upload_check, picked })
    }

    fn finish_bench_cancelled(&self, job_id: &str) {
        let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        if let MachineState::Running(r) = &slot.state
            && r.job_id == job_id
        {
            slot.state = MachineState::Idle;
            let view = slot.view();
            drop(slot);
            self.emit_auto_config_frame(&view);
        }
    }

    fn finish_bench_failed(&self, job_id: &str, mode: AutoConfigMode, error: String, candidates: Vec<BenchCandidate>) {
        let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        if let MachineState::Running(r) = &slot.state
            && r.job_id == job_id
        {
            slot.state = MachineState::Failed(FailedInner { mode, error, candidates });
            let view = slot.view();
            drop(slot);
            self.emit_auto_config_frame(&view);
        }
    }

    /// Requests benchmark cancellation and waits until the background worker has exited and
    /// released all hardware encoder sessions and temporary folders before reporting Idle (C3).
    pub async fn auto_config_cancel(&self) -> AutoConfigView {
        let waiting = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let MachineState::Running(r) = &mut slot.state {
                r.cancel.store(true, Ordering::Release);
                r.cancel_notify.notify_waiters();
                r.cancelling = true;
                r.current_label = Some("Stopping benchmark…".into());
                let job_id = r.job_id.clone();
                let done_rx = r.worker_done.clone();
                let view = slot.view();
                Some((job_id, done_rx, view))
            } else {
                None
            }
        };

        if let Some((job_id, mut done_rx, stopping_view)) = waiting {
            self.emit_auto_config_frame(&stopping_view);
            let _ = done_rx.wait_for(|done| *done).await;
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let MachineState::Running(r) = &slot.state
                && r.job_id == job_id
            {
                slot.state = MachineState::Idle;
            }
            let view = slot.view();
            drop(slot);
            self.emit_auto_config_frame(&view);
            view
        } else {
            self.auto_config_status()
        }
    }

    /// Snapshots current `StudioSettings`, applies recommendation (or explicit overrides), and starts
    /// the 30 s confirmation timeout and 1-minute dropped/lagged-frame rollback watchdog.
    pub async fn auto_config_apply(&self, req: ApplyReq) -> Result<AutoConfigView, EngineError> {
        if let Some(fps) = req.fps
            && ![24, 25, 30, 48, 50, 60, 120].contains(&fps)
        {
            return Err(EngineError::Invalid("fps: 24, 25, 30, 48, 50, 60 or 120.".into()));
        }
        if let Some(kbps) = req.video_kbps
            && !(500..=60_000).contains(&kbps)
        {
            return Err(EngineError::Invalid("video_kbps: 500 to 60000.".into()));
        }
        if let Some(scale) = req.scale {
            validate_scale(scale)?;
        }
        if let Some(enc) = &req.encoder
            && enc != "hardware"
            && enc != "software"
        {
            return Err(EngineError::Invalid("encoder: hardware or software.".into()));
        }

        let (mode, picked, candidates, upload_check, duration_ms, timing) = {
            self.ensure_no_benchmark_running()?;
            let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            match &slot.state {
                MachineState::Done(d) => (
                    d.mode,
                    Some(d.picked.clone()),
                    d.candidates.clone(),
                    d.upload_check.clone(),
                    Some(d.duration_ms),
                    slot.timing,
                ),
                MachineState::RolledBack(rb) => (
                    rb.mode,
                    rb.picked.clone(),
                    rb.candidates.clone(),
                    rb.upload_check.clone(),
                    rb.duration_ms,
                    slot.timing,
                ),
                MachineState::Applied(a) => {
                    (a.mode, a.picked.clone(), a.candidates.clone(), a.upload_check.clone(), a.duration_ms, slot.timing)
                }
                MachineState::Idle | MachineState::Failed(_) | MachineState::Running(_) => {
                    if req.fps.is_none() && req.video_kbps.is_none() && req.encoder.is_none() {
                        return Err(EngineError::Invalid("Run the benchmark or provide settings to apply.".into()));
                    }
                    (AutoConfigMode::Stream, None, Vec::new(), None, None, slot.timing)
                }
            }
        };

        let host = self.studio_host()?;
        let snapshot = host.settings().await;
        let mut next = snapshot.clone();

        if let Some(p) = &picked {
            next.encoder = p.encoder.clone();
            next.video_encoder = p.video_encoder.clone();
            next.encoder_settings = p.encoder_settings.clone();
            next.scale = p.scale;
            next.fps = p.fps;
            next.video_kbps = p.video_kbps;
        }
        if let Some(enc) = req.encoder {
            next.encoder = enc;
        }
        if let Some(ve) = req.video_encoder {
            next.video_encoder = ve;
        }
        if let Some(es) = req.encoder_settings {
            next.encoder_settings = es;
        }
        if let Some(sc) = req.scale {
            next.scale = sc;
        }
        if let Some(fps) = req.fps {
            next.fps = fps;
        }
        if let Some(kbps) = req.video_kbps {
            next.video_kbps = kbps;
        }
        next.check()?;

        let _ = self.studio_set_settings(next.clone()).await?;
        let summary =
            AppliedSummary::from_settings(&next, req.label.or_else(|| picked.as_ref().map(|p| p.label.clone())));

        let mut baseline = StatBaseline::default();
        let (gl_drop, _) = self.golive_trouble();
        if let Ok(st) = host.stats().await {
            let e = &st["engine"];
            let s = &st["stream"];
            let lagged = e["lagged"].as_u64().unwrap_or(0);
            let skipped = e["skipped"].as_u64().unwrap_or(0);
            let dropped = s["dropped"].as_u64().unwrap_or(0).saturating_add(gl_drop);
            baseline.observe(lagged, skipped, dropped);
        }

        let (watch_id, view) = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            let watch_id = slot.seq.saturating_add(1);
            slot.seq = watch_id;
            let now_t = Instant::now();
            slot.state = MachineState::Applied(Box::new(AppliedInner {
                snapshot,
                summary,
                confirmed: false,
                applied_mono: now_t,
                confirm_deadline: now_t + timing.confirm_timeout,
                watch_deadline: now_t + timing.watch_window,
                baseline,
                candidates,
                upload_check,
                picked,
                mode,
                duration_ms,
            }));
            (watch_id, slot.view())
        };
        self.emit_auto_config_frame(&view);

        let engine = self.clone();
        tokio::spawn(async move {
            engine.watch_rollback_loop(watch_id, timing.poll_interval).await;
        });

        Ok(view)
    }

    /// Confirms the applied settings, cancelling the 30 s auto-rollback timer while continuing the
    /// 1-minute dropped/lagged-frame protection window.
    pub fn auto_config_keep(&self) -> Result<AutoConfigView, EngineError> {
        let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        let MachineState::Applied(a) = &mut slot.state else {
            return Err(EngineError::Conflict("There are no newly applied settings waiting to be kept.".into()));
        };
        a.confirmed = true;
        let view = slot.view();
        drop(slot);
        self.emit_auto_config_frame(&view);
        Ok(view)
    }

    /// Immediately restores the pre-apply `StudioSettings` snapshot.
    pub async fn auto_config_undo(&self) -> Result<AutoConfigView, EngineError> {
        let _op = self.studio().auto_config_op.lock().await;
        self.rollback_now(None, RollbackTrigger::UserUndo, "Restored your previous settings.".into()).await
    }

    async fn rollback_now(
        &self,
        expected_watch_id: Option<u64>,
        trigger: RollbackTrigger,
        reason: String,
    ) -> Result<AutoConfigView, EngineError> {
        let (snapshot, mode, candidates, upload_check, picked, duration_ms, restored) = {
            let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(id) = expected_watch_id
                && slot.seq != id
            {
                return Err(EngineError::Conflict("Rollback generation changed.".into()));
            }
            let MachineState::Applied(a) = &slot.state else {
                return Err(EngineError::Conflict("There are no applied settings to undo.".into()));
            };
            (
                a.snapshot.clone(),
                a.mode,
                a.candidates.clone(),
                a.upload_check.clone(),
                a.picked.clone(),
                a.duration_ms,
                AppliedSummary::from_settings(&a.snapshot, None),
            )
        };

        self.studio_set_settings(snapshot).await?;

        let view = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(id) = expected_watch_id
                && slot.seq != id
            {
                return Ok(slot.view());
            }
            slot.state = MachineState::RolledBack(RolledBackInner {
                trigger,
                reason: reason.clone(),
                restored,
                candidates,
                upload_check,
                picked,
                mode,
                duration_ms,
            });
            slot.view()
        };
        self.emit_auto_config_frame(&view);

        if trigger != RollbackTrigger::UserUndo {
            let line = match trigger {
                RollbackTrigger::Timeout => "Encoder settings rolled back (no confirmation in 30 s)",
                RollbackTrigger::DroppedFrames => "Encoder settings rolled back after dropped frames",
                RollbackTrigger::UserUndo => "Previous encoder settings restored",
            };
            self.send_live(LiveFrame::new(
                "notice",
                &Notice::new(NoticeLevel::Warn, line).key("auto_config_rollback").detail(reason),
            ));
        }

        Ok(view)
    }

    async fn watch_rollback_loop(self, watch_id: u64, poll: Duration) {
        let mut interval = tokio::time::interval(poll);
        interval.tick().await;
        loop {
            interval.tick().await;

            let action = {
                let slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                if slot.seq != watch_id {
                    return;
                }
                let MachineState::Applied(a) = &slot.state else {
                    return;
                };
                let now_t = Instant::now();
                if !a.confirmed && now_t >= a.confirm_deadline {
                    Some(WatchDecision::Timeout)
                } else if a.confirmed && now_t >= a.watch_deadline {
                    Some(WatchDecision::WatchComplete)
                } else {
                    None
                }
            };

            match action {
                Some(WatchDecision::Timeout) => {
                    if let Err(e) = self
                        .rollback_now(
                            Some(watch_id),
                            RollbackTrigger::Timeout,
                            "Automatically rolled back after 30 s with no Keep confirmation.".into(),
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "auto-config timeout rollback failed");
                    }
                    return;
                }
                Some(WatchDecision::WatchComplete) => {
                    let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.seq != watch_id {
                        return;
                    }
                    if let MachineState::Applied(a) = std::mem::replace(&mut slot.state, MachineState::Idle) {
                        if let Some(picked) = a.picked {
                            slot.state = MachineState::Done(DoneInner {
                                mode: a.mode,
                                candidates: a.candidates,
                                upload_check: a.upload_check,
                                upload_error: None,
                                picked,
                                duration_ms: a.duration_ms.unwrap_or(0),
                            });
                        }
                        let view = slot.view();
                        drop(slot);
                        self.emit_auto_config_frame(&view);
                    }
                    return;
                }
                None => {}
            }

            // First-minute dropped/lagged check via live stats.
            if let Ok(host) = self.studio_host()
                && let Ok(st) = host.stats().await
            {
                let (gl_drop, _) = self.golive_trouble();
                let lagged = st["engine"]["lagged"].as_u64().unwrap_or(0);
                let skipped = st["engine"]["skipped"].as_u64().unwrap_or(0);
                let dropped = st["stream"]["dropped"].as_u64().unwrap_or(0).saturating_add(gl_drop);

                let bad = {
                    let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.seq != watch_id {
                        return;
                    }
                    let MachineState::Applied(a) = &mut slot.state else {
                        return;
                    };
                    if Instant::now() < a.watch_deadline {
                        let (tot_drops, tot_lags) = a.baseline.observe(lagged, skipped, dropped);
                        (tot_drops + tot_lags >= FIRST_MINUTE_DROP_THRESHOLD).then_some((tot_drops, tot_lags))
                    } else {
                        None
                    }
                };

                if let Some((drops, lags)) = bad {
                    let reason = format!(
                        "Automatically rolled back because the first minute showed {drops} dropped and {lags} lagged/skipped frames."
                    );
                    if let Err(e) = self.rollback_now(Some(watch_id), RollbackTrigger::DroppedFrames, reason).await {
                        tracing::warn!(error = %e, "auto-config drop rollback failed");
                    }
                    return;
                }
            }
        }
    }
}

enum WatchDecision {
    Timeout,
    WatchComplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio_host::{StudioAct, StudioHost};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    #[derive(Debug)]
    struct BenchMockHost {
        settings: Mutex<StudioSettings>,
        streaming: AtomicBool,
        recording: AtomicBool,
        dropped: AtomicU64,
        lagged: AtomicU64,
        skipped: AtomicU64,
        save_count: AtomicU64,
        stats_override: Mutex<Option<Result<Value, String>>>,
        stream_stats_override: Mutex<Option<Result<Value, String>>>,
        record_stats_override: Mutex<Option<Result<Value, String>>>,
        delay_drain_val: Mutex<Option<(f64, f64)>>,
        settings_barrier: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    }

    impl Default for BenchMockHost {
        fn default() -> Self {
            Self {
                settings: Mutex::new(StudioSettings::default()),
                streaming: AtomicBool::new(false),
                recording: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                lagged: AtomicU64::new(0),
                skipped: AtomicU64::new(0),
                save_count: AtomicU64::new(0),
                stats_override: Mutex::new(None),
                stream_stats_override: Mutex::new(None),
                record_stats_override: Mutex::new(None),
                delay_drain_val: Mutex::new(None),
                settings_barrier: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl StudioHost for BenchMockHost {
        fn delay_drain(&self) -> Option<(f64, f64)> {
            *self.delay_drain_val.lock().unwrap()
        }
        async fn record_source(&self, _name: String, on: bool) -> Result<Value, String> {
            Ok(serde_json::json!({ "active": on }))
        }
        async fn replay(&self, on: bool) -> Result<Value, String> {
            Ok(serde_json::json!({ "active": on }))
        }
        async fn virtual_camera(&self, on: bool, _source: Option<String>) -> Result<Value, String> {
            Ok(serde_json::json!({ "active": on }))
        }
        async fn ndi_output(&self, on: bool, _name: Option<String>) -> Result<Value, String> {
            Ok(serde_json::json!({ "active": on }))
        }
        async fn restart(&self) -> Result<(), String> {
            Ok(())
        }
        async fn shelf(&self, _kind: String, _act: crate::studio_host::ShelfAct) -> Result<Value, String> {
            Ok(serde_json::json!({ "ok": true }))
        }
        async fn status(&self) -> crate::studio_host::StudioStatus {
            crate::studio_host::StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<crate::studio_host::StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<crate::studio_host::StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "encoders": [] }))
        }
        async fn options(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn preview(&self, _rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn act(&self, _act: StudioAct) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn stats(&self) -> Result<Value, String> {
            if let Some(res) = self.stats_override.lock().unwrap().clone() {
                return res;
            }
            Ok(serde_json::json!({
                "engine": {
                    "cpu": 12.0,
                    "fps": 60,
                    "frame_ms": 3.0,
                    "lagged": self.lagged.load(Ordering::Relaxed),
                    "skipped": self.skipped.load(Ordering::Relaxed),
                    "frames": 600,
                    "memory_mb": 200
                },
                "canvas": { "width": 1920, "height": 1080, "fps": 60 },
                "stream": {
                    "active": self.streaming.load(Ordering::Relaxed),
                    "reconnecting": false,
                    "bytes": 1000,
                    "frames": 600,
                    "dropped": self.dropped.load(Ordering::Relaxed),
                    "error": Value::Null
                },
                "record": { "active": self.recording.load(Ordering::Relaxed) }
            }))
        }
        async fn record_stats(&self) -> Result<Value, String> {
            if let Some(res) = self.record_stats_override.lock().unwrap().clone() {
                return res;
            }
            Ok(Value::Null)
        }
        async fn settings(&self) -> StudioSettings {
            let barrier = self.settings_barrier.lock().unwrap().clone();
            if let Some(b) = barrier {
                b.wait().await;
            }
            self.settings.lock().unwrap().clone()
        }
        async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String> {
            *self.settings.lock().unwrap() = s;
            self.save_count.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }
        async fn record(&self, on: bool) -> Result<Value, String> {
            self.recording.store(on, Ordering::Relaxed);
            Ok(serde_json::json!({ "active": on }))
        }
        async fn record_pause(&self, _on: bool) -> Result<(), String> {
            Ok(())
        }
        async fn monitoring(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "devices": [] }))
        }
        async fn stream_start(&self, _: String, _: String, _: Vec<(u32, u32, u32)>) -> Result<Value, String> {
            self.streaming.store(true, Ordering::Relaxed);
            Ok(serde_json::json!({ "active": true }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.streaming.store(false, Ordering::Relaxed);
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            if let Some(res) = self.stream_stats_override.lock().unwrap().clone() {
                return res;
            }
            if self.streaming.load(Ordering::Relaxed) {
                Ok(serde_json::json!({ "active": true, "dropped": self.dropped.load(Ordering::Relaxed) }))
            } else {
                Ok(Value::Null)
            }
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn import_obs(&self, _id: String) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }

    async fn test_engine() -> (Engine, Arc<BenchMockHost>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start(dir.path()).await.unwrap();
        let host = Arc::new(BenchMockHost::default());
        engine.set_studio_host(host.clone());
        engine.set_auto_config_timing_for_test(BenchTiming::fast_test());
        (engine, host, dir)
    }

    #[test]
    fn parses_macos_ioreg_gpu_and_top_cpu() {
        let sample_ioreg = r#"+-o AGXAccelerator <class AGXAccelerator> { "PerformanceStatistics" = {"Device Utilization %"=34,"Renderer Utilization %"=29} }"#;
        assert_eq!(parse_macos_gpu_pct(sample_ioreg), Some(34.0));
        let sample_top = "CPU usage: 18.42% user, 9.18% sys, 72.40% idle\nSharedLibs: 100M";
        let cpu = parse_macos_top_cpu_pct(sample_top).unwrap();
        assert!((cpu - 27.6).abs() < 0.01);
        let sample_top_two =
            "CPU usage: 100.0% user, 0.0% sys, 0.0% idle\nPhysMem: 10G\nCPU usage: 14.20% user, 6.80% sys, 79.00% idle";
        let cpu2 = parse_macos_top_cpu_pct(sample_top_two).unwrap();
        assert!((cpu2 - 21.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn benchmark_refuses_with_409_when_streaming_or_recording() {
        let (engine, host, _dir) = test_engine().await;
        host.streaming.store(true, Ordering::Relaxed);
        let err = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("stream".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(4),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "{err:?}");

        host.streaming.store(false, Ordering::Relaxed);
        host.recording.store(true, Ordering::Relaxed);
        let err = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(4),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "{err:?}");
    }

    #[tokio::test]
    async fn benchmark_refuses_every_live_output_and_fails_closed_on_error() {
        let (engine, host, _dir) = test_engine().await;
        let bench_req = || BenchmarkReq {
            mode: Some("record".into()),
            fps_preference: Some(60),
            include_upload: Some(false),
            max_seconds: Some(4),
        };

        // 1. Virtual camera active.
        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "virtualcam": { "active": true } })));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "virtualcam: {err:?}");

        // 2. NDI active using DesktopStudio shape `{ "name": "Ferrocast" }` (no "active" field).
        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "ndi": { "name": "Ferrocast" } })));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "ndi desktop shape: {err:?}"
        );

        // And `ndi: { "active": false }` is NOT treated as active.
        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "ndi": { "active": false } })));
        engine.ensure_benchmark_idle().await.expect("inactive ndi object must allow idle check");

        // 3. Reconnecting main stream, vertical stream, and light stream.
        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "stream": { "reconnecting": true } })));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "reconnecting: {err:?}");

        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "vertical_stream": { "active": true } })));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "vertical_stream: {err:?}"
        );

        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "light_stream": { "active": true } })));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "light_stream: {err:?}");

        // 4. Delayed stream tail still draining.
        *host.stats_override.lock().unwrap() = None;
        *host.delay_drain_val.lock().unwrap() = Some((4.0, 2.5));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "delay_drain: {err:?}");
        *host.delay_drain_val.lock().unwrap() = None;

        // 5. Show countdown active.
        engine.studio().machine.lock().unwrap().phase = crate::studio::show::BroadcastPhase::Countdown;
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "show countdown: {err:?}"
        );
        engine.studio().machine.lock().unwrap().phase = crate::studio::show::BroadcastPhase::Idle;

        // 6. Fail CLOSED when stats / stream_stats / record_stats error out while Studio is running.
        *host.stats_override.lock().unwrap() = Some(Err("studio thread timed out".into()));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "stats err: {err:?}");
        *host.stats_override.lock().unwrap() = None;

        *host.stream_stats_override.lock().unwrap() = Some(Err("stream stats unavailable".into()));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "stream_stats err: {err:?}"
        );
        *host.stream_stats_override.lock().unwrap() = None;

        *host.record_stats_override.lock().unwrap() = Some(Err("record stats unavailable".into()));
        let err = engine.auto_config_benchmark(bench_req()).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }),
            "record_stats err: {err:?}"
        );
        *host.record_stats_override.lock().unwrap() = None;

        // Shared `ensure_idle_for_switch` also catches DesktopStudio NDI shape and fails closed.
        *host.stats_override.lock().unwrap() = Some(Ok(serde_json::json!({ "ndi": { "name": "Studio Out" } })));
        let err = engine.ensure_idle_for_switch("profiles").await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "switch ndi: {err:?}");
        *host.stats_override.lock().unwrap() = Some(Err("studio unresponsive".into()));
        let err = engine.ensure_idle_for_switch("profiles").await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 409, code: "studio_busy", .. }), "switch err: {err:?}");
    }

    #[tokio::test]
    async fn apply_keep_clears_timeout_and_allows_clean_watch_completion() {
        let (engine, host, _dir) = test_engine().await;
        host.settings.lock().unwrap().video_kbps = 3000;
        host.settings.lock().unwrap().fps = 30;
        let view = engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                video_encoder: Some(None),
                encoder_settings: Some(Map::new()),
                scale: Some(None),
                fps: Some(60),
                video_kbps: Some(6000),
                label: Some("VideoToolbox".into()),
            })
            .await
            .unwrap();
        assert_eq!(view.state, AutoConfigStateKind::Applied);
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);
        assert!(!view.confirmed);

        let kept = engine.auto_config_keep().unwrap();
        assert_eq!(kept.state, AutoConfigStateKind::Applied);
        assert!(kept.confirmed);

        // Wait past fast confirm_timeout (180 ms) — still not rolled back because Keep was clicked.
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);
        assert_eq!(host.settings.lock().unwrap().fps, 60);

        // Wait past watch_window (400 ms total) — finishes cleanly.
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(engine.auto_config_status().state, AutoConfigStateKind::Idle);
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);
    }

    #[tokio::test]
    async fn apply_undo_restores_exact_snapshot_immediately() {
        let (engine, host, _dir) = test_engine().await;
        host.settings.lock().unwrap().video_kbps = 2500;
        host.settings.lock().unwrap().fps = 30;
        host.settings.lock().unwrap().scale = Some([1280, 720]);
        host.settings.lock().unwrap().video_encoder = Some("obs_x264".into());

        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                video_encoder: Some(None),
                scale: Some(None),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);

        let rb = engine.auto_config_undo().await.unwrap();
        assert_eq!(rb.state, AutoConfigStateKind::RolledBack);
        assert_eq!(rb.rollback_trigger, Some(RollbackTrigger::UserUndo));
        let restored = host.settings.lock().unwrap().clone();
        assert_eq!(restored.video_kbps, 2500);
        assert_eq!(restored.fps, 30);
        assert_eq!(restored.scale, Some([1280, 720]));
        assert_eq!(restored.video_encoder.as_deref(), Some("obs_x264"));
    }

    #[tokio::test]
    async fn apply_timeout_auto_rolls_back_without_confirmation() {
        let (engine, host, _dir) = test_engine().await;
        host.settings.lock().unwrap().video_kbps = 4000;
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(8000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(host.settings.lock().unwrap().video_kbps, 8000);

        tokio::time::sleep(Duration::from_millis(260)).await;
        let status = engine.auto_config_status();
        assert_eq!(status.state, AutoConfigStateKind::RolledBack);
        assert_eq!(status.rollback_trigger, Some(RollbackTrigger::Timeout));
        assert_eq!(host.settings.lock().unwrap().video_kbps, 4000);
    }

    #[tokio::test]
    async fn apply_first_minute_dropped_or_lagged_frames_trigger_auto_rollback_even_after_keep() {
        let (engine, host, _dir) = test_engine().await;
        host.settings.lock().unwrap().video_kbps = 3500;
        host.settings.lock().unwrap().fps = 30;
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("software".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        engine.auto_config_keep().unwrap();

        // Stream starts cleanly, then starts dropping/lagging frames within the watch window.
        host.dropped.store(4, Ordering::Relaxed);
        host.lagged.store(3, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(120)).await;

        let status = engine.auto_config_status();
        assert_eq!(status.state, AutoConfigStateKind::RolledBack);
        assert_eq!(status.rollback_trigger, Some(RollbackTrigger::DroppedFrames));
        assert!(status.rollback_reason.unwrap_or_default().contains("dropped"));
        assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);
        assert_eq!(host.settings.lock().unwrap().fps, 30);
    }

    #[tokio::test]
    async fn real_encoder_stress_benchmark_and_mid_run_cancel_on_this_mac() {
        let dir = tempfile::tempdir().unwrap();
        let home_dir = std::env::var_os("FERROCAST_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.path().to_path_buf());
        let _ = std::fs::create_dir_all(&home_dir);
        let engine = Engine::start(&home_dir).await.unwrap();
        let host = Arc::new(BenchMockHost::default());
        engine.set_studio_host(host.clone());
        engine.set_auto_config_timing_for_test(BenchTiming::fast_test());

        // 1. Mid-run cancel cleans up state and temp folder immediately.
        let running = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(8),
            })
            .await
            .unwrap();
        assert_eq!(running.state, AutoConfigStateKind::Running);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let cancelled = engine.auto_config_cancel().await;
        assert_eq!(cancelled.state, AutoConfigStateKind::Idle);
        assert!(!home_dir.join("auto-config-tmp").exists(), "temp folder cleaned after cancel");

        // 2. Full real stress run on this computer (VideoToolbox + libx264 presets).
        let start = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(12),
            })
            .await
            .unwrap();
        assert_eq!(start.state, AutoConfigStateKind::Running);

        let mut final_view = engine.auto_config_status();
        for _ in 0..200 {
            if final_view.state != AutoConfigStateKind::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            final_view = engine.auto_config_status();
        }
        assert_eq!(final_view.state, AutoConfigStateKind::Done, "{:?}", final_view.error);
        assert!(!final_view.candidates.is_empty(), "must test at least one encoder candidate");
        assert!(final_view.picked.is_some(), "must pick a recommended config");
        assert!(!home_dir.join("auto-config-tmp").exists(), "temp folder cleaned after finish");

        eprintln!(
            "REAL MAC BENCHMARK: duration={}ms candidates={} picked={:?}",
            final_view.duration_ms.unwrap_or(0),
            final_view.candidates.len(),
            final_view.picked
        );
        for c in &final_view.candidates {
            eprintln!(
                "  - {} {}x{}@{} ({:.2} ms avg, {:.2} ms p95, {:.1} fps, {:.1}% headroom, skip={}, lag={}, cpu={:?}, gpu={:?}) passed={}: {}",
                c.label,
                c.width,
                c.height,
                c.fps,
                c.encode_ms_avg,
                c.encode_ms_p95,
                c.fps_achieved,
                c.headroom_pct,
                c.skipped_frames,
                c.lagged_frames,
                c.cpu_pct,
                c.gpu_pct,
                c.passed,
                c.reason
            );
        }
    }

    #[tokio::test]
    async fn output_starts_refused_while_benchmark_running() {
        let (engine, _host, _dir) = test_engine().await;
        let running = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(8),
            })
            .await
            .unwrap();
        assert_eq!(running.state, AutoConfigStateKind::Running);

        fn assert_busy<T: std::fmt::Debug>(res: Result<T, EngineError>, what: &str) {
            match res {
                Err(EngineError::Refused { status: 409, code, .. }) => {
                    assert!(matches!(code, "studio_busy" | "benchmark_running"), "{what}: unexpected code {code}");
                }
                other => panic!("{what}: expected 409 refusal while benchmark running, got {other:?}"),
            }
        }

        assert_busy(engine.studio_stream(true).await, "studio_stream(true)");
        assert_busy(engine.studio_record(true).await, "studio_record(true)");
        assert_busy(engine.studio_record_source("Cam", true).await, "studio_record_source(true)");
        assert_busy(engine.studio_replay(true).await, "studio_replay(true)");
        assert_busy(engine.studio_virtual_camera(true, None).await, "studio_virtual_camera(true)");
        assert_busy(engine.studio_ndi_output(true, None).await, "studio_ndi_output(true)");
        assert_busy(
            engine.start_show_with(crate::studio::show::ShowStartOpts { force: true, stream: Some(false) }).await,
            "start_show_with",
        );
        assert_busy(engine.studio_restart().await, "studio_restart");
        assert_busy(
            engine.studio_shelf("collections", crate::studio_host::ShelfAct::Switch { id: "other".into() }).await,
            "studio_shelf(Switch)",
        );

        // Stopping an output stays allowed even while benchmark runs.
        assert!(engine.studio_record(false).await.is_ok(), "stopping record stays allowed");

        let _ = engine.auto_config_cancel().await;
        assert!(engine.studio_record(true).await.is_ok(), "outputs allowed again once cancelled");
    }

    #[tokio::test]
    async fn no_toctou_between_benchmark_claim_and_output_start_and_isolated_temp_dir() {
        // Temp dir isolation: sibling job directory must NOT be deleted when creating a new BenchTempDir.
        let tmp_parent = tempfile::tempdir().unwrap();
        let sibling = tmp_parent.path().join("auto-config-tmp").join("existing-job");
        std::fs::create_dir_all(&sibling).unwrap();
        let keep_file = sibling.join("keep.txt");
        std::fs::write(&keep_file, b"ok").unwrap();
        let created = BenchTempDir::create(Some(tmp_parent.path().to_path_buf()), "new-job").unwrap();
        assert!(keep_file.exists(), "BenchTempDir::create must not wipe sibling job folders");
        drop(created);
        assert!(keep_file.exists(), "Dropping BenchTempDir must leave sibling job folders alone");

        // TOCTOU race between `auto_config_benchmark` and `studio_record(true)`:
        // Pause `auto_config_benchmark` inside `host.settings()`, trigger `studio_record(true)` while
        // it waits, then release the barrier. Both must NOT succeed simultaneously.
        let (engine, host, _dir) = test_engine().await;
        // Let hardware encoding be used on `record` without triggering ffmpeg download inside `studio_record`.
        host.settings.lock().unwrap().encoder = "hardware".into();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *host.settings_barrier.lock().unwrap() = Some(barrier.clone());

        let e_bench = engine.clone();
        let bench_handle = tokio::spawn(async move {
            e_bench
                .auto_config_benchmark(BenchmarkReq {
                    mode: Some("record".into()),
                    fps_preference: Some(60),
                    include_upload: Some(false),
                    max_seconds: Some(4),
                })
                .await
        });

        // Wait briefly so `bench_handle` reaches the `host.settings()` barrier if it unlocked `output_gate`.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let e_rec = engine.clone();
        let rec_handle = tokio::spawn(async move { e_rec.studio_record(true).await });

        // Give `rec_handle` a chance to contend on the lock/settings barrier, then release barrier.
        tokio::time::sleep(Duration::from_millis(30)).await;
        *host.settings_barrier.lock().unwrap() = None;
        // Release whichever task(s) are waiting on the 2-party barrier without deadlocking if serialized.
        let _ = tokio::time::timeout(Duration::from_millis(100), barrier.wait()).await;

        let bench_res = bench_handle.await.unwrap();
        let rec_res = rec_handle.await.unwrap();
        assert!(
            !(bench_res.is_ok() && rec_res.is_ok()),
            "TOCTOU race: both benchmark ({bench_res:?}) and studio_record(true) ({rec_res:?}) succeeded!"
        );
        let _ = engine.auto_config_cancel().await;
    }

    #[tokio::test]
    async fn cancel_waits_for_worker_and_releases_sessions_before_idle() {
        let (engine, _host, _dir) = test_engine().await;
        let session_alive = Arc::new(AtomicBool::new(false));
        engine
            .studio()
            .auto_config
            .lock()
            .unwrap()
            .set_worker_hook_for_test(session_alive.clone(), Duration::from_millis(160));

        let running = engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(4),
            })
            .await
            .unwrap();
        assert_eq!(running.state, AutoConfigStateKind::Running);

        // Wait until the worker enters the simulated encoder session.
        for _ in 0..40 {
            if session_alive.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(session_alive.load(Ordering::Acquire), "worker should hold encoder session");

        let e_cancel = engine.clone();
        let cancel_task = tokio::spawn(async move { e_cancel.auto_config_cancel().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            engine.auto_config_status().state,
            AutoConfigStateKind::Running,
            "state must remain Running while worker session teardown is still in flight"
        );
        assert!(
            matches!(engine.studio_record(true).await, Err(EngineError::Refused { status: 409, .. })),
            "outputs must still be blocked while worker session is tearing down"
        );

        let cancelled = cancel_task.await.unwrap();
        assert_eq!(cancelled.state, AutoConfigStateKind::Idle);
        assert!(
            !session_alive.load(Ordering::Acquire),
            "auto_config_cancel reported Idle while worker still held hardware encoder session!"
        );
        assert!(engine.studio_record(true).await.is_ok(), "outputs allowed once cancel waited for teardown");
    }
}
