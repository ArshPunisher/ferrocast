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
    PlatformTarget, candidate_ladder_for_mode, evaluate_candidate, pick_recommendation, stream_ceiling,
};
use ferrocore::{
    Blend, Bounds, Compositor, Content, Effect, Filter, Item, Picture, Placement, Scene, ffenc,
    video::{
        Frame, H264Profile, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder, X264_PRESETS, x264_index,
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

    fn matches_output_fields(&self, s: &StudioSettings) -> bool {
        self.encoder == s.encoder
            && self.video_encoder == s.video_encoder
            && self.encoder_settings == s.encoder_settings
            && self.scale == s.scale
            && self.fps == s.fps
            && self.video_kbps == s.video_kbps
    }
}

/// The six output-affecting settings touched by auto-config apply/rollback (C6/H6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutputSnapshot {
    pub encoder: String,
    pub video_encoder: Option<String>,
    pub encoder_settings: Map<String, Value>,
    pub scale: Option<[u32; 2]>,
    pub fps: u32,
    pub video_kbps: u32,
}

impl OutputSnapshot {
    pub(crate) fn from_settings(s: &StudioSettings) -> Self {
        Self {
            encoder: s.encoder.clone(),
            video_encoder: s.video_encoder.clone(),
            encoder_settings: s.encoder_settings.clone(),
            scale: s.scale,
            fps: s.fps,
            video_kbps: s.video_kbps,
        }
    }

    pub(crate) fn apply_to(&self, target: &mut StudioSettings) {
        target.encoder = self.encoder.clone();
        target.video_encoder = self.video_encoder.clone();
        target.encoder_settings = self.encoder_settings.clone();
        target.scale = self.scale;
        target.fps = self.fps;
        target.video_kbps = self.video_kbps;
    }
}

/// On-disk journal recording an unconfirmed auto-config apply so a quit or crash restores the
/// previous output settings cleanly (C6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingApply {
    pub version: u32,
    pub snapshot: OutputSnapshot,
    pub mode: AutoConfigMode,
    pub applied_label: String,
    pub saved_at_ms: u64,
}

pub(crate) fn pending_apply_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auto-config-pending.json")
}

fn restore_output_snapshot_to_profile_file(data_dir: &Path, snap: &OutputSnapshot) -> std::io::Result<StudioSettings> {
    ferrocast_store::durable::flush_saves(Duration::from_millis(500));
    let profile_path = ferrocast_store::studio_library::load(data_dir)
        .current_file(data_dir, ferrocast_store::studio_library::Kind::Profiles);
    let mut current =
        ferrocast_store::durable::load_json_with_backup::<StudioSettings>(&profile_path, |s| s.check().is_ok())
            .map(|loaded| loaded.value)
            .unwrap_or_default();
    snap.apply_to(&mut current);
    if current.check().is_err() {
        current = StudioSettings::default();
        snap.apply_to(&mut current);
    }
    ferrocast_store::durable::save_json_pretty(&profile_path, &current)?;
    Ok(current)
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
    #[serde(default)]
    pub rollback_offered: bool,
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

fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Request payload for `POST /api/v1/studio/engine/auto-config/apply`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApplyReq {
    #[serde(default)]
    pub encoder: Option<String>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub video_encoder: Option<Option<String>>,
    #[serde(default)]
    pub encoder_settings: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_some")]
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
            burst_frames: 45,
            warmup_frames: 10,
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
    accumulated_lags: u64,
    accumulated_drops: u64,
}

impl StatBaseline {
    fn start_live(&mut self, skipped: u64, lagged: u64) {
        self.skipped = Some(skipped);
        self.lagged = Some(lagged);
    }

    /// Observes only encoder-caused skips (`skipped`) and engine render lag (`lagged`) while an
    /// output is actively encoding; network socket drops are excluded (C5).
    fn observe_live(&mut self, skipped: u64, lagged: u64) -> (u64, u64) {
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
                // Engine restarted mid-window: new baseline starts at cur.
                Some(_) => {
                    *prev = Some(cur);
                    cur
                }
            }
        };
        let ds = step_counter(&mut self.skipped, skipped);
        let dl = step_counter(&mut self.lagged, lagged);
        self.accumulated_drops = self.accumulated_drops.saturating_add(ds);
        self.accumulated_lags = self.accumulated_lags.saturating_add(dl);
        (self.accumulated_drops, self.accumulated_lags)
    }
}

#[derive(Debug)]
struct AppliedInner {
    snapshot: StudioSettings,
    summary: AppliedSummary,
    confirmed: bool,
    rolling_back: bool,
    applied_mono: Instant,
    confirm_deadline: Instant,
    watch_window: Duration,
    watch_started: Option<Instant>,
    watch_deadline: Option<Instant>,
    was_live: bool,
    pending_offer: Option<(RollbackTrigger, String)>,
    rollback_offered: bool,
    rollback_trigger: Option<RollbackTrigger>,
    rollback_reason: Option<String>,
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
                rollback_offered: false,
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
                rollback_offered: false,
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
                rollback_offered: false,
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
                rollback_offered: false,
                rollback_trigger: None,
                rollback_reason: None,
                restored: None,
            },
            MachineState::Applied(a) => {
                let confirm_rem = if a.confirmed || a.rollback_offered || t_now >= a.confirm_deadline {
                    0
                } else {
                    ceil_secs(a.confirm_deadline.saturating_duration_since(t_now))
                };
                let watch_rem = match a.watch_deadline {
                    None => ceil_secs(a.watch_window),
                    Some(d) if t_now >= d => 0,
                    Some(d) => ceil_secs(d.saturating_duration_since(t_now)),
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
                    rollback_offered: a.rollback_offered,
                    rollback_trigger: a.rollback_trigger,
                    rollback_reason: a.rollback_reason.clone(),
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
                rollback_offered: false,
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

/// Discovers only the encoder candidates that the live engine (`VideoEncoder::new` /
/// `encode_of`) will actually select on this machine (C4).
fn discover_encoders_cancellable(cancel: &AtomicBool) -> Vec<DiscoveredEncoder> {
    let (live_hw, has_x264) = ffenc::discover_live_h264_and_x264(Some(cancel));
    if cancel.load(Ordering::Acquire) {
        return Vec::new();
    }
    select_bench_encoders(cfg!(target_os = "macos"), live_hw.as_deref(), has_x264)
}

fn select_bench_encoders(is_macos: bool, live_ff_hw: Option<&str>, has_libx264: bool) -> Vec<DiscoveredEncoder> {
    let mut out = Vec::new();
    if is_macos {
        out.push(DiscoveredEncoder {
            backend: "videotoolbox".into(),
            studio_encoder_id: "com.apple.videotoolbox.videoencoder.ave.avc".into(),
            label: "Apple VideoToolbox".into(),
            family: EncoderFamily::VideoToolbox,
            ffmpeg_name: None,
            preset: None,
        });
    } else if let Some(enc) = live_ff_hw {
        let (label, family) = match enc {
            "h264_nvenc" => ("NVIDIA NVENC", EncoderFamily::Nvenc),
            "h264_qsv" => ("Intel Quick Sync", EncoderFamily::Qsv),
            "h264_amf" => ("AMD AMF", EncoderFamily::Amf),
            "h264_mf" => ("Hardware (Media Foundation)", EncoderFamily::MediaFoundation),
            _ => ("Hardware (FFmpeg)", EncoderFamily::Nvenc),
        };
        out.push(DiscoveredEncoder {
            backend: enc.into(),
            studio_encoder_id: "ffmpeg_hardware".into(),
            label: label.into(),
            family,
            ffmpeg_name: Some(enc.into()),
            preset: None,
        });
    }

    if has_libx264 {
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
const BENCH_WORKLOAD_FRAMES: u32 = 24;

struct BenchFrames {
    y_stride: usize,
    uv_stride: usize,
    frames: Vec<(Vec<u8>, Vec<u8>)>,
}

impl BenchFrames {
    fn build(compositor: Option<&Compositor>, width: u32, height: u32, cancel: &AtomicBool) -> Result<Self, StepAbort> {
        if let Some(c) = compositor
            && let Ok(rendered) = render_compositor_frames(c, width, height, BENCH_WORKLOAD_FRAMES, cancel)
        {
            return Ok(rendered);
        }
        synthetic_nv12_frames(width, height, BENCH_WORKLOAD_FRAMES as usize, cancel)
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

struct LoadMonitor {
    host_cpu_supported: bool,
    rt: Option<tokio::runtime::Handle>,
    sys: sysinfo::System,
    last_cpu_at: Instant,
    last_cpu: Option<f64>,
}

impl LoadMonitor {
    fn new(host_cpu_supported: bool, rt: Option<tokio::runtime::Handle>) -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        Self { host_cpu_supported, rt, sys, last_cpu_at: Instant::now(), last_cpu: None }
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
        if self.last_cpu_at.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
            self.sys.refresh_cpu_usage();
            self.last_cpu_at = Instant::now();
            self.last_cpu = Some(f64::from(self.sys.global_cpu_usage()).clamp(0.0, 100.0));
        }
        (self.last_cpu, read_os_gpu_pct())
    }
}

type LoadSample = (Instant, Option<f64>, Option<f64>);
type LoadSampleLog = Arc<std::sync::Mutex<Vec<LoadSample>>>;

/// Samples OS CPU and GPU load via `sysinfo` on a helper thread so the encoder pacing loop never
/// stalls on subprocesses or `ioreg`. Joins the thread deterministically on `Drop`.
struct BackgroundLoadSampler {
    stop: Arc<AtomicBool>,
    samples: LoadSampleLog,
    handle: Option<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    worker_done: Arc<AtomicBool>,
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
        let worker_done = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let samples_t = samples.clone();
        let done_t = worker_done.clone();
        let handle = std::thread::Builder::new()
            .name("auto-config-load".into())
            .spawn(move || {
                struct MarkDone(Arc<AtomicBool>);
                impl Drop for MarkDone {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _mark_done = MarkDone(done_t);
                let mut mon = LoadMonitor::new(host_cpu_supported, rt);
                if let Some(g0) = read_os_gpu_pct() {
                    samples_t.lock().unwrap_or_else(|p| p.into_inner()).push((Instant::now(), None, Some(g0)));
                }
                while !stop_t.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                    for _ in 0..4 {
                        if stop_t.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
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
                }
            })
            .ok();
        if handle.is_none() {
            worker_done.store(true, Ordering::Release);
        }
        Self {
            stop,
            samples,
            handle,
            #[cfg(test)]
            worker_done,
        }
    }

    /// Averages ONLY samples captured strictly inside `[from, to]` so earlier candidates never
    /// bleed stale readings into later candidates (H3).
    fn window_sample(&self, from: Instant, to: Instant) -> (Option<f64>, Option<f64>) {
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
        let avg = |xs: &[f64]| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
        (avg(&cpus), avg(&gpus))
    }
}

impl Drop for BackgroundLoadSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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

fn run_phase1_encode_loop<T, FEncode, FFlush>(
    state: &mut T,
    cand_label: &str,
    total_burst: u32,
    warmup_frames: u32,
    cancel: &AtomicBool,
    mut encode_fn: FEncode,
    mut flush_fn: FFlush,
) -> Result<(Vec<f64>, f64), StepAbort>
where
    FEncode: FnMut(&mut T, u32) -> Result<(), String>,
    FFlush: FnMut(&mut T),
{
    let mut frame_times_ms = Vec::with_capacity(total_burst as usize);
    let mut steady_start = None;

    for i in 0..total_burst {
        if cancel.load(Ordering::Relaxed) {
            return Err(StepAbort::Cancelled);
        }
        if i == warmup_frames {
            steady_start = Some(Instant::now());
        }
        let t0 = Instant::now();
        if let Err(e) = encode_fn(state, i) {
            if cancel.load(Ordering::Acquire) {
                return Err(StepAbort::Cancelled);
            }
            return Err(StepAbort::Failed(format!("{cand_label} encode error: {e}")));
        }
        let dt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if i >= warmup_frames {
            frame_times_ms.push(dt_ms);
        }
    }
    if cancel.load(Ordering::Acquire) {
        return Err(StepAbort::Cancelled);
    }
    // Capture steady-window elapsed time BEFORE flushing/closing the encoder process so FFmpeg
    // teardown or child exit waits never inflate per-frame benchmark latency (H2).
    let steady_elapsed_ms = steady_start.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
    let steady_count = (total_burst.saturating_sub(warmup_frames)).max(1) as f64;
    let throughput_avg_ms = (steady_elapsed_ms / steady_count).max(0.05);
    flush_fn(state);
    Ok((frame_times_ms, throughput_avg_ms))
}

#[allow(clippy::too_many_arguments)]
fn run_software_candidates<FProgress, FStep>(
    sw_encoders: &[DiscoveredEncoder],
    ladder: &[(u32, u32, u32)],
    start_tier: (u32, u32, u32),
    has_passing_hw: bool,
    cancel: &AtomicBool,
    deadline: Instant,
    results: &mut Vec<BenchCandidate>,
    completed_steps: &mut usize,
    mut on_progress: FProgress,
    mut run_step: FStep,
) -> Result<(), StepAbort>
where
    FProgress: FnMut(String, &[BenchCandidate], usize),
    FStep: FnMut(&DiscoveredEncoder, u32, u32, u32) -> Result<BenchCandidate, StepAbort>,
{
    if has_passing_hw {
        // Sample strictly two representative x264 presets when hardware already passed cleanly.
        let (w, h, fps) = start_tier;
        for sw in sw_encoders.iter().filter(|sw| matches!(sw.preset.as_deref(), Some("veryfast" | "superfast"))) {
            if cancel.load(Ordering::Relaxed) {
                return Err(StepAbort::Cancelled);
            }
            if !results.is_empty() && Instant::now() >= deadline {
                break;
            }
            let label = format!("{} ({w}×{h} @ {fps} fps)", sw.label);
            on_progress(format!("Stress testing {label}…"), results, *completed_steps);
            match run_step(sw, w, h, fps) {
                Ok(c) => {
                    results.push(c);
                    *completed_steps += 1;
                }
                Err(StepAbort::Cancelled) => return Err(StepAbort::Cancelled),
                Err(StepAbort::Failed(msg)) => {
                    tracing::debug!(preset = ?sw.preset, error = %msg, "x264 preset benchmark step failed");
                }
            }
        }
        return Ok(());
    }

    // CPU-only / no passing hardware: exhaust `faster -> veryfast -> superfast -> ultrafast` at
    // each resolution tier BEFORE stepping resolution/fps down (H5).
    let mut tiers = vec![start_tier];
    for &(nw, nh, nf) in ladder {
        if (u64::from(nw) * u64::from(nh), nf) < (u64::from(start_tier.0) * u64::from(start_tier.1), start_tier.2)
            && !tiers.contains(&(nw, nh, nf))
        {
            tiers.push((nw, nh, nf));
        }
    }

    for (w, h, fps) in tiers {
        for sw in sw_encoders {
            if cancel.load(Ordering::Relaxed) {
                return Err(StepAbort::Cancelled);
            }
            if !results.is_empty() && Instant::now() >= deadline {
                return Ok(());
            }
            let label = format!("{} ({w}×{h} @ {fps} fps)", sw.label);
            on_progress(format!("Stress testing {label}…"), results, *completed_steps);
            match run_step(sw, w, h, fps) {
                Ok(c) => {
                    let passed = c.passed;
                    results.push(c);
                    *completed_steps += 1;
                    if passed {
                        return Ok(());
                    }
                }
                Err(StepAbort::Cancelled) => return Err(StepAbort::Cancelled),
                Err(StepAbort::Failed(msg)) => {
                    tracing::debug!(preset = ?sw.preset, error = %msg, "x264 preset benchmark step failed");
                }
            }
        }
    }
    Ok(())
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
    let mut cap_enc = VideoEncoder::for_benchmark(cfg, cand.ffmpeg_name.as_deref(), Some(1), Some(cancel.clone()))
        .map_err(|e| StepAbort::Failed(format!("{} failed to start: {e}", cand.label)))?;
    let total_burst = timing.burst_frames.max(timing.warmup_frames + 5);
    let frame_us = 1_000_000i64 / i64::from(fps.max(1));

    let (mut frame_times_ms, throughput_avg_ms) = run_phase1_encode_loop(
        &mut cap_enc,
        &cand.label,
        total_burst,
        timing.warmup_frames,
        cancel,
        |enc, i| {
            let key = i % (fps.max(1) * 2) == 0;
            enc.encode(frames_data.frame(i as usize), i64::from(i) * frame_us, key)
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
        |enc| {
            let _ = enc.flush();
        },
    )?;
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
    let skipped_frames = rt_enc.skipped();
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
    let (cpu_pct, gpu_pct) = sampler.window_sample(rt_start, rt_end);

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
    Done(Box<WorkerDone>),
}

struct WorkerDone {
    mode: AutoConfigMode,
    candidates: Vec<BenchCandidate>,
    upload_check: Option<UploadCheck>,
    upload_error: Option<String>,
    picked: BenchRecommendation,
}

/// Derives a persisted encode throughput ceiling (`PersistedStudioBench`) from passing candidates.
pub(crate) fn persisted_bench_from_run(
    candidates: &[BenchCandidate],
    picked: &BenchRecommendation,
    host_sig: &str,
    measured_at: i64,
) -> crate::golive_plan::PersistedStudioBench {
    let want_sw = crate::golive_plan::encoder_is_software(&picked.studio_encoder_id);
    let estimate_mpx = |c: &BenchCandidate| -> f64 {
        let mpx = f64::from(c.width) * f64::from(c.height) * f64::from(c.fps) / 1e6;
        let budget_ms = 1000.0 / f64::from(c.fps.max(1));
        let avg_ms = c.encode_ms_avg.max(0.25);
        let fps_factor =
            if c.fps_achieved > 0.0 { (c.fps_achieved / f64::from(c.fps.max(1))).clamp(0.1, 1.0) } else { 1.0 };
        ((mpx * (budget_ms / avg_ms) * 0.80 * fps_factor).max(mpx)).min(mpx * 4.0).min(1_000.0)
    };
    let preset_matches = |c: &BenchCandidate| !want_sw || picked.preset.is_none() || c.preset == picked.preset;
    let mut best_exact: Option<f64> = None;
    let mut best_class: Option<f64> = None;
    for c in candidates {
        if !c.passed || c.encode_ms_avg <= 0.0 || !preset_matches(c) {
            continue;
        }
        let est = estimate_mpx(c);
        if c.studio_encoder_id == picked.studio_encoder_id {
            best_exact = Some(best_exact.map_or(est, |cur| cur.max(est)));
        } else if crate::golive_plan::encoder_is_software(&c.studio_encoder_id) == want_sw {
            best_class = Some(best_class.map_or(est, |cur| cur.max(est)));
        }
    }
    let picked_mpx = f64::from(picked.width) * f64::from(picked.height) * f64::from(picked.fps) / 1e6;
    let max_mpx_s = best_exact.or(best_class).unwrap_or(picked_mpx).max(picked_mpx);
    crate::golive_plan::PersistedStudioBench {
        encoder_id: picked.studio_encoder_id.clone(),
        is_software: want_sw,
        max_mpx_s: (max_mpx_s * 10.0).round() / 10.0,
        max_sessions_ok: None,
        measured_at,
        host_sig: if host_sig.is_empty() { "default".into() } else { host_sig.to_string() },
    }
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
        let host = self.studio_host()?;
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

            let s = host.settings().await;
            let canvas = [s.width, s.height];

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
        let cancel_flag = params.cancel.clone();
        let done_guard = WorkerDoneGuard(params.done_tx.clone());
        let outcome = self.run_benchmark_task_inner(params).await;
        // All encoders, sampler threads, and temp folders are dropped inside
        // `run_benchmark_task_inner`; apply final state before releasing `done_guard` (C3).
        let outcome = if cancel_flag.load(Ordering::Acquire) { WorkerOutcome::Cancelled } else { outcome };
        match outcome {
            WorkerOutcome::Cancelled => self.finish_bench_cancelled(&job_id),
            WorkerOutcome::Failed(mode, error, candidates) => {
                self.finish_bench_failed(&job_id, mode, error, candidates);
            }
            WorkerOutcome::Done(done) => {
                let host_sig = if let Ok(h) = self.studio_host() {
                    h.host_budget_info()
                        .await
                        .map(|i| {
                            self.note_host_budget_info(i.clone());
                            i.host_sig
                        })
                        .unwrap_or_default()
                } else {
                    self.golive_cached_host_sig().unwrap_or_default()
                };
                let bench = persisted_bench_from_run(&done.candidates, &done.picked, &host_sig, now().0);
                *self.studio().studio_bench.write().unwrap_or_else(|p| p.into_inner()) = Some(bench.clone());
                if let Err(e) = crate::studio::save_setting(&self, crate::golive_plan::STUDIO_BENCH_KEY, &bench).await {
                    tracing::warn!(error = %e, "couldn't persist studio_bench measurement");
                }
                let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                let elapsed_ms = if let MachineState::Running(r) = &slot.state
                    && r.job_id == job_id
                {
                    r.started_mono.elapsed().as_millis() as u64
                } else {
                    drop(done_guard);
                    return;
                };
                slot.state = MachineState::Done(DoneInner {
                    mode: done.mode,
                    candidates: done.candidates,
                    upload_check: done.upload_check,
                    upload_error: done.upload_error,
                    picked: done.picked,
                    duration_ms: elapsed_ms,
                });
                let view = slot.view();
                drop(slot);
                self.emit_auto_config_frame(&view);
            }
        }
        drop(done_guard);
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
        let mut upload_error: Option<String> = None;
        if include_upload && mode == AutoConfigMode::Stream {
            let fut = self.golive_upload_check();
            tokio::pin!(fut);
            let res = tokio::select! {
                _ = cancel_notify.notified() => return WorkerOutcome::Cancelled,
                r = &mut fut => r,
            };
            match res {
                Ok(uc) => upload_check = Some(uc),
                Err(e) => {
                    tracing::warn!(error = %e, "upload check failed during benchmark; continuing encoder benchmark");
                    upload_error = Some(e.to_string());
                }
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
            if cancel_clone.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
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
            let ladder = candidate_ladder_for_mode(canvas, fps_pref, mode);
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

            let target_bitrate = ceil_clone.as_ref().map_or_else(
                || {
                    if mode == AutoConfigMode::Record && canvas[0].min(canvas[1]) >= 1440 {
                        18_000
                    } else {
                        8_000
                    }
                },
                |c| c.bitrate_kbps,
            );
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
                                    && (u64::from(ceil.max_width) * u64::from(ceil.max_height)
                                        < u64::from(w) * u64::from(h)
                                        || ceil.max_fps < fps)
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
                && (u64::from(ceil.max_width) * u64::from(ceil.max_height), ceil.max_fps)
                    < (u64::from(sw_tier.0) * u64::from(sw_tier.1), sw_tier.2)
            {
                sw_tier = (ceil.max_width, ceil.max_height, ceil.max_fps);
            }
            if let Err(StepAbort::Cancelled) = run_software_candidates(
                &sw_encoders,
                &ladder,
                sw_tier,
                has_passing_hw,
                &cancel_clone,
                deadline,
                &mut results,
                &mut completed_steps,
                update_running,
                |sw, w, h, fps| {
                    run_candidate_step(
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
                    )
                },
            ) {
                return Err("cancelled".into());
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

        WorkerOutcome::Done(Box::new(WorkerDone { mode, candidates, upload_check, upload_error, picked }))
    }

    fn golive_cached_host_sig(&self) -> Option<String> {
        self.cached_host_budget_info().map(|i| i.host_sig)
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
        let clean_label = if let Some(raw) = &req.label {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.chars().count() > 100 || trimmed.chars().any(char::is_control) {
                return Err(EngineError::Invalid("label: 1 to 100 characters without control characters.".into()));
            }
            Some(trimmed.to_string())
        } else {
            None
        };

        self.flush_pending_startup_restore_to_host().await;
        let _op = self.studio().auto_config_op.lock().await;

        let (mode, picked, candidates, upload_check, duration_ms, timing, existing_unconfirmed_snap) = {
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
                    None,
                ),
                MachineState::RolledBack(rb) => (
                    rb.mode,
                    rb.picked.clone(),
                    rb.candidates.clone(),
                    rb.upload_check.clone(),
                    rb.duration_ms,
                    slot.timing,
                    None,
                ),
                MachineState::Applied(a) => (
                    a.mode,
                    a.picked.clone(),
                    a.candidates.clone(),
                    a.upload_check.clone(),
                    a.duration_ms,
                    slot.timing,
                    (!a.confirmed).then(|| a.snapshot.clone()),
                ),
                MachineState::Idle | MachineState::Failed(_) | MachineState::Running(_) => {
                    if req.fps.is_none() && req.video_kbps.is_none() && req.encoder.is_none() {
                        return Err(EngineError::Invalid("Run the benchmark or provide settings to apply.".into()));
                    }
                    (AutoConfigMode::Stream, None, Vec::new(), None, None, slot.timing, None)
                }
            }
        };

        let host = self.studio_host()?;
        let current = host.settings().await;
        let snapshot = existing_unconfirmed_snap.unwrap_or_else(|| current.clone());
        let mut next = current;

        if let Some(p) = &picked {
            next.encoder = p.encoder.clone();
            next.video_encoder = p.video_encoder.clone();
            next.encoder_settings = p.encoder_settings.clone();
            next.scale = p.scale;
            next.fps = p.fps;
            next.video_kbps = p.video_kbps;
        }
        let had_explicit_video_encoder = req.video_encoder.is_some();
        if let Some(enc) = req.encoder {
            if enc == "software" && !had_explicit_video_encoder {
                next.video_encoder = None;
            }
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

        let summary =
            AppliedSummary::from_settings(&next, clean_label.or_else(|| picked.as_ref().map(|p| p.label.clone())));
        let pending_path = pending_apply_path(self.data_dir());
        let had_pending_before = pending_path.exists();
        let pending = PendingApply {
            version: 1,
            snapshot: OutputSnapshot::from_settings(&snapshot),
            mode,
            applied_label: summary.label.clone(),
            saved_at_ms: now().0.max(0) as u64,
        };
        if let Err(e) = ferrocast_store::durable::save_json_pretty(&pending_path, &pending) {
            tracing::warn!(error = %e, "couldn't persist auto-config pending journal");
        }

        if let Err(e) = self.studio_set_settings_inner(next.clone(), true).await {
            if !had_pending_before {
                ferrocast_store::durable::remove_with_backup(&pending_path);
            }
            return Err(e);
        }

        let baseline = StatBaseline::default();
        let (watch_id, view) = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            let watch_id = slot.seq.saturating_add(1);
            slot.seq = watch_id;
            let now_t = Instant::now();
            slot.state = MachineState::Applied(Box::new(AppliedInner {
                snapshot,
                summary,
                confirmed: false,
                rolling_back: false,
                applied_mono: now_t,
                confirm_deadline: now_t + timing.confirm_timeout,
                watch_window: timing.watch_window,
                watch_started: None,
                watch_deadline: None,
                was_live: false,
                pending_offer: None,
                rollback_offered: false,
                rollback_trigger: None,
                rollback_reason: None,
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

    /// Confirms applied settings (clearing the 30 s timer and any post-stream rollback offer).
    pub fn auto_config_keep(&self) -> Result<AutoConfigView, EngineError> {
        let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        let MachineState::Applied(a) = &mut slot.state else {
            return Err(EngineError::Conflict("There are no newly applied settings waiting to be kept.".into()));
        };
        if a.rolling_back {
            return Err(EngineError::Conflict("Settings rollback is already in progress.".into()));
        }
        let was_offered = a.rollback_offered;
        a.confirmed = true;
        a.pending_offer = None;
        a.rollback_offered = false;
        a.rollback_trigger = None;
        a.rollback_reason = None;
        let done_watch = a.watch_deadline.is_some_and(|d| Instant::now() >= d);
        if was_offered || done_watch {
            slot.state = MachineState::Idle;
        }
        let view = slot.view();
        drop(slot);
        ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));
        self.emit_auto_config_frame(&view);
        Ok(view)
    }

    fn disarm_auto_config_if(&self, should_disarm: impl FnOnce(&AppliedInner) -> bool) {
        let view = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            let MachineState::Applied(a) = &slot.state else {
                return;
            };
            if !should_disarm(a) {
                return;
            }
            slot.seq = slot.seq.saturating_add(1);
            slot.state = MachineState::Idle;
            slot.view()
        };
        ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));
        self.emit_auto_config_frame(&view);
    }

    /// Disarms any pending auto-rollback when the user manually edits one of the 6 output fields (H6).
    pub(crate) fn disarm_auto_config_if_output_changed(&self, saved: &StudioSettings) {
        self.disarm_auto_config_if(|a| !a.summary.matches_output_fields(saved));
    }

    /// Unconditionally disarms any pending auto-rollback and startup restore (e.g. on profile switch).
    pub(crate) fn disarm_auto_config(&self) {
        self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()).take();
        ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));
        self.disarm_auto_config_if(|_| true);
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
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(id) = expected_watch_id
                && slot.seq != id
            {
                return Err(EngineError::Conflict("Rollback generation changed.".into()));
            }
            let MachineState::Applied(a) = &mut slot.state else {
                return Err(EngineError::Conflict("There are no applied settings to undo.".into()));
            };
            if trigger == RollbackTrigger::Timeout && a.confirmed {
                return Err(EngineError::Conflict("Settings were already confirmed by Keep.".into()));
            }
            if a.rolling_back {
                return Err(EngineError::Conflict("Rollback already in progress.".into()));
            }
            a.rolling_back = true;
            (
                OutputSnapshot::from_settings(&a.snapshot),
                a.mode,
                a.candidates.clone(),
                a.upload_check.clone(),
                a.picked.clone(),
                a.duration_ms,
                AppliedSummary::from_settings(&a.snapshot, None),
            )
        };

        let clear_rolling_back = || {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            if expected_watch_id.is_none_or(|id| slot.seq == id)
                && let MachineState::Applied(a) = &mut slot.state
            {
                a.rolling_back = false;
            }
        };

        let host = match self.studio_host() {
            Ok(h) => h,
            Err(e) => {
                clear_rolling_back();
                return Err(e);
            }
        };
        let mut merged = host.settings().await;
        snapshot.apply_to(&mut merged);
        if let Err(e) = merged.check() {
            clear_rolling_back();
            return Err(e);
        }
        if let Err(e) = self.studio_set_settings_inner(merged, true).await {
            clear_rolling_back();
            return Err(e);
        }
        ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));

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

    /// Startup recovery for C6: if Ferrocast crashed or exited with an unconfirmed auto-config apply
    /// pending on disk, immediately restore the saved pre-apply output fields on the active profile
    /// and queue host restoration for when `StudioHost` attaches.
    pub(crate) fn recover_pending_auto_config_on_startup(&self) {
        let pending_path = pending_apply_path(self.data_dir());
        let Some(loaded) =
            ferrocast_store::durable::load_json_with_backup::<PendingApply>(&pending_path, |p| p.version == 1)
        else {
            return;
        };
        let pending = loaded.value;
        match restore_output_snapshot_to_profile_file(self.data_dir(), &pending.snapshot) {
            Ok(_) => ferrocast_store::durable::remove_with_backup(&pending_path),
            Err(e) => {
                tracing::warn!(error = %e, "failed to restore unconfirmed auto-config settings to profile file on startup");
            }
        }
        *self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()) = Some(pending);
    }

    /// Flushes any pending startup rollback onto the attached `StudioHost` (e.g. `DesktopStudio` or test mock).
    pub(crate) async fn flush_pending_startup_restore_to_host(&self) {
        if self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()).is_none() {
            return;
        }
        let _op = self.studio().auto_config_op.lock().await;
        let pending = {
            let guard = self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner());
            guard.clone()
        };
        let Some(pending) = pending else { return };
        let Ok(host) = self.studio_host() else { return };
        let mut cur = host.settings().await;
        pending.snapshot.apply_to(&mut cur);
        if let Err(e) = cur.check() {
            tracing::warn!(error = %e, "pending startup auto-config snapshot failed validation; discarding");
            self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()).take();
            ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));
            return;
        }
        if let Err(e) = host.set_settings(cur).await {
            tracing::warn!(error = %e, "failed to apply startup auto-config rollback to StudioHost");
            return;
        }
        self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()).take();
        ferrocast_store::durable::remove_with_backup(&pending_apply_path(self.data_dir()));
        let saved = host.settings().await;
        self.note_studio_settings(&saved);
        self.send_golive_pub();
        self.send_live(LiveFrame::new(
            "notice",
            &Notice::new(NoticeLevel::Warn, "Previous encoder settings restored").key("auto_config_rollback").detail(
                format!(
                    "Restored your previous encoder settings because {} wasn't kept before Ferrocast closed.",
                    pending.applied_label
                ),
            ),
        ));
    }

    /// Clean-quit safeguard for C6: any unconfirmed auto-config apply is rolled back synchronously
    /// onto disk (and onto an attached host if responsive) before `Engine::shutdown` completes.
    pub(crate) fn restore_pending_auto_config_on_shutdown(&self) {
        let pending_path = pending_apply_path(self.data_dir());
        let from_slot = {
            let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
            match &slot.state {
                MachineState::Applied(a) if !a.confirmed => {
                    let snap = OutputSnapshot::from_settings(&a.snapshot);
                    let restored = AppliedSummary::from_settings(&a.snapshot, None);
                    let candidates = a.candidates.clone();
                    let upload_check = a.upload_check.clone();
                    let picked = a.picked.clone();
                    let mode = a.mode;
                    let duration_ms = a.duration_ms;
                    slot.seq = slot.seq.saturating_add(1);
                    slot.state = MachineState::RolledBack(RolledBackInner {
                        trigger: RollbackTrigger::Timeout,
                        reason: "Restored your previous settings when Ferrocast closed before Keep.".into(),
                        restored,
                        candidates,
                        upload_check,
                        picked,
                        mode,
                        duration_ms,
                    });
                    Some(snap)
                }
                _ => None,
            }
        };

        let snap = from_slot.or_else(|| {
            ferrocast_store::durable::load_json_with_backup::<PendingApply>(&pending_path, |p| p.version == 1)
                .map(|loaded| loaded.value.snapshot)
        });
        let Some(snap) = snap else {
            return;
        };

        let disk_ok = match restore_output_snapshot_to_profile_file(self.data_dir(), &snap) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't restore unconfirmed auto-config settings to profile file on shutdown");
                false
            }
        };

        if let Ok(host) = self.studio_host() {
            let snap_host = snap.clone();
            if let Ok(handle) =
                std::thread::Builder::new().name("ferrocast-autoconfig-shutdown".into()).spawn(move || {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        rt.block_on(async move {
                            let _ = tokio::time::timeout(Duration::from_millis(250), async {
                                let mut cur = host.settings().await;
                                snap_host.apply_to(&mut cur);
                                if cur.check().is_ok() {
                                    let _ = host.set_settings(cur).await;
                                }
                            })
                            .await;
                        });
                    }
                })
            {
                let _ = handle.join();
            }
        }

        if disk_ok {
            ferrocast_store::durable::remove_with_backup(&pending_path);
        }
    }

    async fn watch_rollback_loop(self, watch_id: u64, poll: Duration) {
        let is_output_encoding_live = |st: &Value| -> bool {
            let active = |k: &str| st[k]["active"].as_bool() == Some(true);
            active("stream") || active("record") || active("vertical_stream") || active("light_stream")
        };

        let mut interval = tokio::time::interval(poll);
        interval.tick().await;
        loop {
            interval.tick().await;

            let st = match self.studio_host() {
                Ok(host) => host.stats().await.ok(),
                Err(_) => None,
            };
            let live_stats = st.as_ref().filter(|s| is_output_encoding_live(s));

            enum LoopStep {
                Continue,
                OfferAfterStream { view: Box<AutoConfigView>, reason: String },
                TimeoutOffline,
                WatchComplete,
                Stop,
            }

            let step = {
                let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                if slot.seq != watch_id {
                    return;
                }
                let MachineState::Applied(a) = &mut slot.state else {
                    return;
                };
                let now_t = Instant::now();

                if let Some(st_ref) = live_stats {
                    let lagged = st_ref["engine"]["lagged"].as_u64().unwrap_or(0);
                    let skipped = st_ref["engine"]["skipped"].as_u64().unwrap_or(0);

                    // Start the first-minute clock only once live encoding begins (C5).
                    if a.watch_started.is_none() {
                        a.watch_started = Some(now_t);
                        a.watch_deadline = Some(now_t + a.watch_window);
                        a.baseline.start_live(skipped, lagged);
                    } else if !a.was_live {
                        a.baseline.start_live(skipped, lagged);
                    }
                    a.was_live = true;

                    // Count only encoder-caused skips while live; never mutate settings mid-stream.
                    if a.watch_deadline.is_some_and(|d| now_t < d) && a.pending_offer.is_none() && !a.rollback_offered {
                        let (drops, _lags) = a.baseline.observe_live(skipped, lagged);
                        if drops >= FIRST_MINUTE_DROP_THRESHOLD {
                            a.pending_offer = Some((
                                RollbackTrigger::DroppedFrames,
                                format!(
                                    "First minute dropped {drops} encoder-skipped frames. Undo to restore your previous settings, or Keep."
                                ),
                            ));
                        }
                    }
                    if !a.confirmed && now_t >= a.confirm_deadline && a.pending_offer.is_none() && !a.rollback_offered {
                        a.pending_offer = Some((
                            RollbackTrigger::Timeout,
                            "Settings weren't confirmed within 30 s while live. Restore your previous settings or Keep."
                                .into(),
                        ));
                    }

                    if a.watch_deadline.is_some_and(|d| now_t >= d)
                        && a.confirmed
                        && a.pending_offer.is_none()
                        && !a.rollback_offered
                    {
                        LoopStep::WatchComplete
                    } else {
                        LoopStep::Continue
                    }
                } else {
                    // Offline or stream/recording just ended.
                    if a.was_live {
                        a.was_live = false;
                        if let Some((trig, reason)) = a.pending_offer.take() {
                            a.rollback_offered = true;
                            a.rollback_trigger = Some(trig);
                            a.rollback_reason = Some(reason.clone());
                            let view = Box::new(slot.view());
                            LoopStep::OfferAfterStream { view, reason }
                        } else if a.watch_deadline.is_some_and(|d| now_t >= d) && a.confirmed && !a.rollback_offered {
                            LoopStep::WatchComplete
                        } else if !a.confirmed && !a.rollback_offered && now_t >= a.confirm_deadline {
                            LoopStep::TimeoutOffline
                        } else {
                            LoopStep::Continue
                        }
                    } else if !a.confirmed
                        && !a.rollback_offered
                        && a.pending_offer.is_none()
                        && now_t >= a.confirm_deadline
                    {
                        LoopStep::TimeoutOffline
                    } else if a.watch_started.is_some()
                        && a.watch_deadline.is_some_and(|d| now_t >= d)
                        && a.confirmed
                        && !a.rollback_offered
                    {
                        LoopStep::WatchComplete
                    } else if a.rollback_offered {
                        LoopStep::Stop
                    } else {
                        LoopStep::Continue
                    }
                }
            };

            match step {
                LoopStep::Continue => {}
                LoopStep::Stop => return,
                LoopStep::OfferAfterStream { view, reason } => {
                    self.emit_auto_config_frame(&view);
                    self.send_live(LiveFrame::new(
                        "notice",
                        &Notice::new(NoticeLevel::Warn, "Encoder dropped frames during your stream — rollback ready")
                            .key("auto_config_rollback_offer")
                            .detail(reason),
                    ));
                    return;
                }
                LoopStep::TimeoutOffline => {
                    let _op = self.studio().auto_config_op.lock().await;
                    if let Err(e) = self
                        .rollback_now(
                            Some(watch_id),
                            RollbackTrigger::Timeout,
                            "Automatically rolled back after 30 s with no Keep confirmation.".into(),
                        )
                        .await
                    {
                        if self.auto_config_status().confirmed {
                            continue;
                        }
                        tracing::warn!(error = %e, "auto-config timeout rollback failed");
                    }
                    return;
                }
                LoopStep::WatchComplete => {
                    let mut slot = self.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.seq != watch_id {
                        return;
                    }
                    slot.state = MachineState::Idle;
                    let view = slot.view();
                    drop(slot);
                    self.emit_auto_config_frame(&view);
                    return;
                }
            }
        }
    }
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

    /// Waits for `cond` to hold, polling. The limit only decides when a broken test gives up.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !cond() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Reads the Applied state's watcher fields (false when nothing is applied).
    fn watch_state(engine: &Engine, read: impl Fn(&AppliedInner) -> bool) -> bool {
        let slot = engine.studio().auto_config.lock().unwrap_or_else(|p| p.into_inner());
        match &slot.state {
            MachineState::Applied(a) => read(a),
            _ => false,
        }
    }

    #[test]
    fn parses_macos_ioreg_gpu_pct() {
        let sample_ioreg = r#"+-o AGXAccelerator <class AGXAccelerator> { "PerformanceStatistics" = {"Device Utilization %"=34,"Renderer Utilization %"=29} }"#;
        assert_eq!(parse_macos_gpu_pct(sample_ioreg), Some(34.0));
        const { assert!(BENCH_WORKLOAD_FRAMES >= 24) };
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

        // Live stream starts cleanly with 0 encoder drops; watch timer starts now and completes cleanly.
        host.streaming.store(true, Ordering::Relaxed);
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
        // A watch window no machine can overrun while the test waits for the watcher.
        engine.set_auto_config_timing_for_test(BenchTiming {
            watch_window: Duration::from_secs(600),
            ..BenchTiming::fast_test()
        });
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

        // Live stream starts. The watcher takes its baseline at its first poll that sees the
        // stream live, so the frames are dropped only once it has: waiting for that, not for a
        // fixed time, keeps a busy machine from letting the drops into the baseline.
        host.streaming.store(true, Ordering::Relaxed);
        wait_for("the watcher to see the stream live", || watch_state(&engine, |a| a.was_live)).await;
        host.skipped.store(6, Ordering::Relaxed);
        wait_for("the watcher to count the dropped frames", || watch_state(&engine, |a| a.pending_offer.is_some()))
            .await;

        // Settings MUST NOT mutate mid-stream!
        assert_eq!(engine.auto_config_status().state, AutoConfigStateKind::Applied);
        assert!(!engine.auto_config_status().rollback_offered, "defer offer presentation until stream ends");
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);

        // End stream -> rollback offer surfaces after stream, user invokes Undo cleanly offline.
        host.streaming.store(false, Ordering::Relaxed);
        wait_for("the offer after the stream ends", || engine.auto_config_status().rollback_offered).await;
        let offered = engine.auto_config_status();
        assert_eq!(offered.state, AutoConfigStateKind::Applied);
        assert!(offered.rollback_offered);
        assert_eq!(offered.rollback_trigger, Some(RollbackTrigger::DroppedFrames));
        assert!(offered.rollback_reason.unwrap_or_default().contains("dropped"));

        let rb = engine.auto_config_undo().await.unwrap();
        assert_eq!(rb.state, AutoConfigStateKind::RolledBack);
        assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);
        assert_eq!(host.settings.lock().unwrap().fps, 30);
    }

    // Runs the Apple encoder and libx264 for real: only a Mac has both here.
    #[cfg(target_os = "macos")]
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

    #[test]
    fn bench_encoder_discovery_matches_live_selection_path() {
        // Off-macOS: only the single live FFmpeg hardware encoder (e.g. h264_nvenc or h264_qsv) is
        // included — never alternative GPU backends that VideoEncoder::new won't open.
        let nv = select_bench_encoders(false, Some("h264_nvenc"), true);
        let hw_nv: Vec<_> = nv.iter().filter(|e| e.family.is_hardware()).collect();
        assert_eq!(hw_nv.len(), 1);
        assert_eq!(hw_nv[0].backend, "h264_nvenc");
        assert_eq!(hw_nv[0].studio_encoder_id, "ffmpeg_hardware");
        assert_eq!(nv.iter().filter(|e| !e.family.is_hardware()).count(), 4);

        let qsv = select_bench_encoders(false, Some("h264_qsv"), true);
        let hw_qsv: Vec<_> = qsv.iter().filter(|e| e.family.is_hardware()).collect();
        assert_eq!(hw_qsv.len(), 1);
        assert_eq!(hw_qsv[0].backend, "h264_qsv");
        assert_eq!(hw_qsv[0].studio_encoder_id, "ffmpeg_hardware");

        // When no hardware FFmpeg encoder works (or live fell back to libx264), never emit ffmpeg_hardware.
        let sw_only = select_bench_encoders(false, None, true);
        assert!(sw_only.iter().all(|e| !e.family.is_hardware() && e.studio_encoder_id == "obs_x264"));
        assert_eq!(sw_only.len(), 4);

        // macOS always uses native VideoToolbox + libx264 (never ffmpeg_hardware).
        let mac = select_bench_encoders(true, Some("h264_videotoolbox"), true);
        let hw_mac: Vec<_> = mac.iter().filter(|e| e.family.is_hardware()).collect();
        assert_eq!(hw_mac.len(), 1);
        assert_eq!(hw_mac[0].studio_encoder_id, "com.apple.videotoolbox.videoencoder.ave.avc");
        assert!(hw_mac[0].ffmpeg_name.is_none());
    }

    #[tokio::test]
    async fn c5_live_stream_never_mutates_ignores_network_drops_and_waits_for_stream() {
        let (engine, host, _dir) = test_engine().await;
        host.settings.lock().unwrap().video_kbps = 3500;
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        let baseline_saves = host.save_count.load(Ordering::Relaxed);
        engine.auto_config_keep().unwrap();

        // 1. Offline preview lags + waiting past watch_window offline must NOT expire the live watch
        // or roll back settings before the stream starts.
        host.lagged.store(12, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(
            engine.auto_config_status().state,
            AutoConfigStateKind::Applied,
            "watch clock must wait until live output starts"
        );
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);

        // 2. Stream goes live; pure network drops (`stream.dropped`) must NOT count as encoder drops
        // and must NEVER mutate live settings.
        host.streaming.store(true, Ordering::Relaxed);
        host.dropped.store(25, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(engine.auto_config_status().first_minute_drops, 0, "network drops must not count");
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);
        assert_eq!(host.save_count.load(Ordering::Relaxed), baseline_saves);

        // 3. Encoder-caused drops (`engine.skipped`) occur while live: settings MUST stay untouched
        // on air, and rollback is offered after the stream ends instead.
        host.skipped.store(7, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            host.settings.lock().unwrap().video_kbps,
            6000,
            "live broadcast settings must NEVER change mid-stream"
        );
        assert_eq!(host.save_count.load(Ordering::Relaxed), baseline_saves);

        // Stream ends -> rollback is now offered (`RolledBack` is NOT auto-forced, user can Undo).
        host.streaming.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let post = engine.auto_config_status();
        assert_eq!(post.state, AutoConfigStateKind::Applied);
        assert!(post.rollback_offered, "rollback_offered should turn true after stream ends");
        assert_eq!(post.rollback_trigger, Some(RollbackTrigger::DroppedFrames));
        assert!(post.first_minute_drops >= 5);
        assert_eq!(host.settings.lock().unwrap().video_kbps, 6000);
        let undone = engine.auto_config_undo().await.unwrap();
        assert_eq!(undone.state, AutoConfigStateKind::RolledBack);
        assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);

        // Also test unconfirmed 30s timeout while LIVE: never mutates settings mid-stream.
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(7000),
                ..Default::default()
            })
            .await
            .unwrap();
        host.streaming.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(260)).await;
        assert_eq!(host.settings.lock().unwrap().video_kbps, 7000, "timeout must not mutate live stream");
        host.streaming.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let offered2 = engine.auto_config_status();
        assert!(offered2.rollback_offered);
        assert_eq!(offered2.rollback_trigger, Some(RollbackTrigger::Timeout));
        let kept = engine.auto_config_keep().unwrap();
        assert!(!kept.rollback_offered);
    }

    #[tokio::test]
    async fn c6_unconfirmed_apply_restored_on_clean_shutdown_unless_kept() {
        use ferrocast_store::durable::{load_json_with_backup, save_json_pretty};
        use ferrocast_store::studio_library::{Kind, load as load_library};

        let dir = tempfile::tempdir().unwrap();
        let initial =
            StudioSettings { encoder: "software".into(), fps: 30, video_kbps: 3500, ..StudioSettings::default() };
        let profile_path = load_library(dir.path()).current_file(dir.path(), Kind::Profiles);
        save_json_pretty(&profile_path, &initial).unwrap();

        let engine = Engine::start(dir.path()).await.unwrap();
        let host = Arc::new(BenchMockHost::default());
        *host.settings.lock().unwrap() = initial.clone();
        engine.set_studio_host(host.clone());
        engine.set_auto_config_timing_for_test(BenchTiming::fast_test());

        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        let pending_file = dir.path().join("auto-config-pending.json");
        assert!(pending_file.exists(), "unconfirmed apply must persist pending rollback journal");

        // Clean quit without clicking Keep must restore pre-apply settings on both disk profile & host.
        engine.shutdown();
        assert!(!pending_file.exists(), "shutdown restore should clear pending rollback journal");
        let on_disk = load_json_with_backup::<StudioSettings>(&profile_path, |s| s.check().is_ok())
            .expect("profile file should exist on disk")
            .value;
        assert_eq!(on_disk.video_kbps, 3500);
        assert_eq!(on_disk.fps, 30);
        assert_eq!(on_disk.encoder, "software");
        assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);

        // Kept apply must clear the pending journal and survive clean shutdown.
        let engine2 = Engine::start(dir.path()).await.unwrap();
        let host2 = Arc::new(BenchMockHost::default());
        *host2.settings.lock().unwrap() = initial;
        engine2.set_studio_host(host2.clone());
        engine2.set_auto_config_timing_for_test(BenchTiming::fast_test());

        engine2
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6200),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(pending_file.exists());
        engine2.auto_config_keep().unwrap();
        assert!(!pending_file.exists(), "Keep must clear pending rollback journal");

        // Simulate desktop flushing kept settings to profile file as well.
        save_json_pretty(&profile_path, &*host2.settings.lock().unwrap()).unwrap();
        engine2.shutdown();
        let kept_disk = load_json_with_backup::<StudioSettings>(&profile_path, |s| s.check().is_ok()).unwrap().value;
        assert_eq!(kept_disk.video_kbps, 6200);
        assert_eq!(host2.settings.lock().unwrap().video_kbps, 6200);
    }

    #[tokio::test]
    async fn c6_unconfirmed_apply_survives_crash_and_restores_on_next_start_unless_kept() {
        use ferrocast_store::durable::{load_json_with_backup, save_json_pretty};
        use ferrocast_store::studio_library::{Kind, load as load_library};

        let dir = tempfile::tempdir().unwrap();
        let initial =
            StudioSettings { encoder: "software".into(), fps: 30, video_kbps: 3500, ..StudioSettings::default() };
        let profile_path = load_library(dir.path()).current_file(dir.path(), Kind::Profiles);
        save_json_pretty(&profile_path, &initial).unwrap();
        let pending_file = dir.path().join("auto-config-pending.json");

        {
            let engine1 = Engine::start(dir.path()).await.unwrap();
            let host1 = Arc::new(BenchMockHost::default());
            *host1.settings.lock().unwrap() = initial.clone();
            engine1.set_studio_host(host1.clone());
            engine1.set_auto_config_timing_for_test(BenchTiming::fast_test());

            engine1
                .auto_config_apply(ApplyReq {
                    encoder: Some("hardware".into()),
                    fps: Some(60),
                    video_kbps: Some(6000),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(pending_file.exists());
            // Simulate desktop having written the unconfirmed settings to the active profile JSON.
            save_json_pretty(&profile_path, &*host1.settings.lock().unwrap()).unwrap();
            // Drop without calling `shutdown()` to simulate a crash / SIGKILL.
        }

        // Next start sees the unconfirmed journal and immediately restores the disk profile AND host.
        let engine2 = Engine::start(dir.path()).await.unwrap();
        assert!(!pending_file.exists(), "startup recovery should clear pending journal after disk restore");
        let restored_disk =
            load_json_with_backup::<StudioSettings>(&profile_path, |s| s.check().is_ok()).unwrap().value;
        assert_eq!(restored_disk.video_kbps, 3500);
        assert_eq!(restored_disk.fps, 30);
        assert_eq!(restored_disk.encoder, "software");

        let host2 = Arc::new(BenchMockHost::default());
        *host2.settings.lock().unwrap() =
            StudioSettings { encoder: "hardware".into(), fps: 60, video_kbps: 6000, ..StudioSettings::default() };
        engine2.set_studio_host(host2.clone());
        let resolved = engine2.studio_settings().await.unwrap();
        assert_eq!(resolved.video_kbps, 3500);
        assert_eq!(host2.settings.lock().unwrap().video_kbps, 3500);

        // Kept apply must survive a crash too without rolling back on next start.
        engine2.set_auto_config_timing_for_test(BenchTiming::fast_test());
        engine2
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6500),
                ..Default::default()
            })
            .await
            .unwrap();
        engine2.auto_config_keep().unwrap();
        save_json_pretty(&profile_path, &*host2.settings.lock().unwrap()).unwrap();
        drop(engine2);

        let engine3 = Engine::start(dir.path()).await.unwrap();
        let disk_after_kept =
            load_json_with_backup::<StudioSettings>(&profile_path, |s| s.check().is_ok()).unwrap().value;
        assert_eq!(disk_after_kept.video_kbps, 6500);
        engine3.shutdown();
    }

    #[test]
    fn h2_phase1_steady_timing_excludes_flush_teardown() {
        // Checked against the test's own timeline, not an absolute ms bound: a slow machine
        // stretches the frames (and so the loop's number) but never the gap between them and the
        // teardown. The steady window starts just before frame 5 is encoded and, when it is
        // measured right, stops before the flush starts. If the flush were inside it, the window
        // would be at least the whole teardown longer than frames-to-flush-start.
        const TEARDOWN_MS: u64 = 500;
        let cancel = AtomicBool::new(false);
        let mut dummy = ();
        let steady_frame_entered = std::cell::Cell::new(None::<Instant>);
        let flush_started = std::cell::Cell::new(None::<Instant>);
        let (frame_times, avg_ms) = run_phase1_encode_loop(
            &mut dummy,
            "x264",
            15,
            5,
            &cancel,
            |_, i| {
                if i == 5 {
                    steady_frame_entered.set(Some(Instant::now()));
                }
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            },
            |_| {
                flush_started.set(Some(Instant::now()));
                // Simulate FFmpeg process teardown + stdin EOF flush latency.
                std::thread::sleep(Duration::from_millis(TEARDOWN_MS));
            },
        )
        .unwrap();
        assert_eq!(frame_times.len(), 10);
        let entered = steady_frame_entered.get().expect("frame 5 was encoded");
        let flushed = flush_started.get().expect("the flush ran after the loop");
        let frames_to_flush_ms = flushed.duration_since(entered).as_secs_f64() * 1000.0;
        let steady_window_ms = avg_ms * 10.0;
        // Half the teardown of room either side: the window may start a little before frame 5 is
        // entered, but it must end before the flush, not after it.
        assert!(
            steady_window_ms < frames_to_flush_ms + TEARDOWN_MS as f64 / 2.0,
            "Phase 1 steady window ({steady_window_ms:.1} ms) was polluted by post-loop flush/teardown! \
             (frames to flush start: {frames_to_flush_ms:.1} ms, teardown {TEARDOWN_MS} ms)"
        );
    }

    #[tokio::test]
    async fn h3_sampler_excludes_stale_samples_outside_window_and_joins_thread_on_drop() {
        let t_old = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let from = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let to = Instant::now();

        let (engine, _host, _dir) = test_engine().await;
        let cancel = Arc::new(AtomicBool::new(false));
        let sampler = BackgroundLoadSampler::start(false, Some(tokio::runtime::Handle::current()), engine, cancel);
        assert!(sampler.handle.is_some(), "sampler must hold worker JoinHandle for Drop join");
        sampler.samples.lock().unwrap().push((t_old, Some(88.0), Some(77.0)));
        let (cpu, gpu) = sampler.window_sample(from, to);
        assert!(
            cpu.is_none() && gpu.is_none(),
            "stale pre-window sample must never bleed into another candidate's window: got ({cpu:?}, {gpu:?})"
        );
        let done_flag = sampler.worker_done.clone();
        drop(sampler);
        assert!(done_flag.load(Ordering::Acquire), "Drop must join the BackgroundLoadSampler worker thread");
    }

    #[test]
    fn h5_software_loop_tests_all_presets_at_tier_and_samples_two_when_hw_passes() {
        let sw_encoders: Vec<DiscoveredEncoder> = select_bench_encoders(false, None, true);
        let ladder = ferrocast_golive::bench::candidate_ladder([1920, 1080], 60);
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(30);

        let fake_cand = |sw: &DiscoveredEncoder, w: u32, h: u32, fps: u32, passed: bool| BenchCandidate {
            encoder_backend: sw.backend.clone(),
            studio_encoder_id: sw.studio_encoder_id.clone(),
            label: sw.label.clone(),
            hardware: false,
            family: sw.family,
            preset: sw.preset.clone(),
            width: w,
            height: h,
            fps,
            bitrate_kbps: 6000,
            frames_tested: 45,
            encode_ms_avg: if passed { 4.0 } else { 20.0 },
            encode_ms_p95: if passed { 6.0 } else { 24.0 },
            encode_ms_max: if passed { 8.0 } else { 28.0 },
            budget_ms: 1000.0 / f64::from(fps),
            headroom_pct: if passed { 75.0 } else { -20.0 },
            fps_achieved: if passed { f64::from(fps) } else { 40.0 },
            skipped_frames: 0,
            lagged_frames: 0,
            cpu_pct: Some(50.0),
            gpu_pct: None,
            passed,
            reason: if passed { "ok".into() } else { "too slow".into() },
            elapsed_ms: 100,
        };

        // 1. CPU-only: `1080p60 faster` fails, `1080p60 veryfast` passes -> must stay at 1080p60 and test `veryfast`, NOT drop to 720p60.
        let mut results = Vec::new();
        let mut steps = 0;
        let mut tried_a = Vec::new();
        run_software_candidates(
            &sw_encoders,
            &ladder,
            ladder[0],
            false,
            &cancel,
            deadline,
            &mut results,
            &mut steps,
            |_, _, _| {},
            |sw, w, h, fps| {
                let p = sw.preset.clone().unwrap();
                tried_a.push((w, h, fps, p.clone()));
                Ok(fake_cand(sw, w, h, fps, w == 1920 && p == "veryfast"))
            },
        )
        .unwrap();
        assert_eq!(
            tried_a,
            vec![(1920, 1080, 60, "faster".into()), (1920, 1080, 60, "veryfast".into())],
            "CPU-only must try lighter presets at 1080p60 before stepping resolution down"
        );

        // 2. CPU-only: all 4 presets fail at 1080p60 -> steps down to 720p60 and finds `veryfast`.
        let mut results_b = Vec::new();
        let mut steps_b = 0;
        let mut tried_b = Vec::new();
        run_software_candidates(
            &sw_encoders,
            &ladder,
            ladder[0],
            false,
            &cancel,
            deadline,
            &mut results_b,
            &mut steps_b,
            |_, _, _| {},
            |sw, w, h, fps| {
                let p = sw.preset.clone().unwrap();
                tried_b.push((w, h, fps, p.clone()));
                Ok(fake_cand(sw, w, h, fps, w == 1280 && p == "veryfast"))
            },
        )
        .unwrap();
        assert_eq!(
            tried_b,
            vec![
                (1920, 1080, 60, "faster".into()),
                (1920, 1080, 60, "veryfast".into()),
                (1920, 1080, 60, "superfast".into()),
                (1920, 1080, 60, "ultrafast".into()),
                (1280, 720, 60, "faster".into()),
                (1280, 720, 60, "veryfast".into()),
            ]
        );

        // 3. Hardware passed: sample strictly 2 software comparison presets (`veryfast`, `superfast`), NOT 3 (`faster`, `veryfast`, `superfast`).
        let mut results_c = Vec::new();
        let mut steps_c = 0;
        let mut tried_c = Vec::new();
        run_software_candidates(
            &sw_encoders,
            &ladder,
            ladder[0],
            true,
            &cancel,
            deadline,
            &mut results_c,
            &mut steps_c,
            |_, _, _| {},
            |sw, w, h, fps| {
                let p = sw.preset.clone().unwrap();
                tried_c.push((w, h, fps, p));
                Ok(fake_cand(sw, w, h, fps, true))
            },
        )
        .unwrap();
        assert_eq!(
            tried_c,
            vec![(1920, 1080, 60, "veryfast".into()), (1920, 1080, 60, "superfast".into())],
            "when hardware already passed, sample only 2 software presets (veryfast, superfast)"
        );
    }

    #[tokio::test]
    async fn h6_manual_settings_change_during_countdown_preserved_and_disarms_rollback() {
        let (engine, host, dir) = test_engine().await;
        // Full folder paths on every OS (a leading slash is not one on Windows).
        let old_rec = dir.path().join("old-rec").to_string_lossy().into_owned();
        let new_rec = dir.path().join("new-rec").to_string_lossy().into_owned();
        {
            let mut s = host.settings.lock().unwrap();
            s.encoder = "software".into();
            s.fps = 30;
            s.video_kbps = 3500;
            s.record_folder = Some(old_rec.clone());
        }

        // 1. Unrelated manual edit during countdown (`record_folder`) must survive Undo/rollback.
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut edited = engine.studio_settings().await.unwrap();
        edited.record_folder = Some(new_rec.clone());
        engine.studio_set_settings(edited).await.unwrap();
        assert_eq!(
            engine.auto_config_status().state,
            AutoConfigStateKind::Applied,
            "unrelated setting change should keep Applied countdown active"
        );

        engine.auto_config_undo().await.unwrap();
        let after_undo = engine.studio_settings().await.unwrap();
        assert_eq!(after_undo.video_kbps, 3500);
        assert_eq!(after_undo.encoder, "software");
        assert_eq!(
            after_undo.record_folder.as_deref(),
            Some(new_rec.as_str()),
            "rollback must only restore the 6 output fields without clobbering unrelated edits"
        );

        // 2. Manual edit to one of the 6 output fields during countdown disarms auto-rollback.
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut manual_out = engine.studio_settings().await.unwrap();
        manual_out.video_kbps = 4500;
        engine.studio_set_settings(manual_out).await.unwrap();
        assert_eq!(
            engine.auto_config_status().state,
            AutoConfigStateKind::Idle,
            "editing an output field manually during countdown must disarm pending rollback"
        );
        tokio::time::sleep(Duration::from_millis(320)).await;
        assert_eq!(
            host.settings.lock().unwrap().video_kbps,
            4500,
            "30s timeout must not overwrite user's manual output edit"
        );
    }

    #[tokio::test]
    async fn h7_second_apply_and_timeout_vs_keep_race_stay_consistent() {
        let (engine, host, _dir) = test_engine().await;
        {
            let mut s = host.settings.lock().unwrap();
            s.encoder = "software".into();
            s.fps = 30;
            s.video_kbps = 3500;
        }

        // 1. Second unconfirmed apply while already Applied must preserve original 3500 snapshot.
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                ..Default::default()
            })
            .await
            .unwrap();
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(7200),
                ..Default::default()
            })
            .await
            .unwrap();
        engine.auto_config_undo().await.unwrap();
        assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);

        // 2. Race Keep against an in-flight Timeout rollback paused inside `host.settings()`.
        engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6400),
                ..Default::default()
            })
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *host.settings_barrier.lock().unwrap() = Some(barrier.clone());

        // Let the confirm_timeout (180 ms in fast_test) expire so TimeoutOffline enters `host.settings()`.
        tokio::time::sleep(Duration::from_millis(240)).await;

        let keep_res = engine.auto_config_keep();
        *host.settings_barrier.lock().unwrap() = None;
        barrier.wait().await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        let st = engine.auto_config_status();
        if keep_res.is_ok() {
            assert_ne!(
                st.state,
                AutoConfigStateKind::RolledBack,
                "Keep succeeded so Timeout must not roll back or report RolledBack"
            );
            assert_eq!(host.settings.lock().unwrap().video_kbps, 6400);
        } else {
            assert_eq!(st.state, AutoConfigStateKind::RolledBack);
            assert_eq!(host.settings.lock().unwrap().video_kbps, 3500);
        }
    }

    #[tokio::test]
    async fn h8_apply_req_json_null_clears_scale_and_video_encoder_and_software_clears_hw_id() {
        let req: ApplyReq = serde_json::from_str(
            r#"{"encoder":"software","fps":60,"video_kbps":6000,"scale":null,"video_encoder":null}"#,
        )
        .unwrap();
        assert_eq!(req.scale, Some(None), "explicit JSON null must deserialize as Some(None) to clear scale");
        assert_eq!(
            req.video_encoder,
            Some(None),
            "explicit JSON null must deserialize as Some(None) to clear video_encoder"
        );

        let omitted: ApplyReq = serde_json::from_str(r#"{"fps":60}"#).unwrap();
        assert_eq!(omitted.scale, None);
        assert_eq!(omitted.video_encoder, None);

        let (engine, host, _dir) = test_engine().await;
        {
            let mut s = host.settings.lock().unwrap();
            s.encoder = "hardware".into();
            s.video_encoder = Some("com.apple.videotoolbox.videoencoder.ave.avc".into());
            s.scale = Some([1280, 720]);
            s.fps = 30;
            s.video_kbps = 4000;
        }
        let clear_req: ApplyReq =
            serde_json::from_str(r#"{"encoder":"software","fps":60,"video_kbps":6000,"scale":null}"#).unwrap();
        engine.auto_config_apply(clear_req).await.unwrap();
        let updated = engine.studio_settings().await.unwrap();
        assert_eq!(updated.scale, None, "explicit null scale in ApplyReq must clear previous downscale");
        assert_eq!(
            updated.video_encoder, None,
            "switching encoder to software must clear stale hardware video_encoder"
        );
    }

    #[tokio::test]
    async fn m2_upload_check_failure_degrades_gracefully_with_upload_error() {
        // The benchmark runs a real encoder. Where none works (a CI runner without FFmpeg), it
        // fails for that reason and there is no upload check to degrade: nothing to test here.
        if discover_encoders_cancellable(&AtomicBool::new(false)).is_empty() {
            eprintln!("skipped: no working video encoder on this computer");
            return;
        }
        let (engine, _host, _dir) = test_engine().await;
        // Hold the Go live busy lock so `golive_upload_check()` fails deterministically offline.
        let _busy = engine.golive().busy.lock().await;
        engine
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("stream".into()),
                fps_preference: Some(60),
                include_upload: Some(true),
                max_seconds: Some(3),
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        let final_view = loop {
            let v = engine.auto_config_status();
            if v.state != AutoConfigStateKind::Running {
                break v;
            }
            assert!(Instant::now() < deadline, "benchmark did not finish in time");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(
            final_view.state,
            AutoConfigStateKind::Done,
            "upload failure must record upload_error and still complete encoder benchmark: {:?}",
            final_view.error
        );
        assert!(final_view.upload_error.is_some());
        assert!(final_view.picked.is_some());
    }

    #[tokio::test]
    async fn m4_benchmark_requires_studio_host_and_apply_rejects_bad_label() {
        // 1. Engine without attached StudioHost must reject benchmark upfront with 422 studio_unavailable.
        let dir = tempfile::tempdir().unwrap();
        let detached = Engine::start(dir.path()).await.unwrap();
        let err = detached
            .auto_config_benchmark(BenchmarkReq {
                mode: Some("record".into()),
                fps_preference: Some(60),
                include_upload: Some(false),
                max_seconds: Some(3),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 422, code: "studio_unavailable", .. }),
            "detached engine must reject benchmark upfront cleanly: {err:?}"
        );

        // 2. ApplyReq::label rejects empty, control characters, and > 100 characters.
        let (engine, _host, _dir) = test_engine().await;
        for bad in ["", "   ", "bad\nline", &"x".repeat(101)] {
            let e = engine
                .auto_config_apply(ApplyReq {
                    encoder: Some("hardware".into()),
                    fps: Some(60),
                    video_kbps: Some(6000),
                    label: Some(bad.to_string()),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert!(matches!(e, EngineError::Invalid(_)), "bad label {bad:?} gave {e:?}");
        }

        // Valid label trims cleanly.
        let ok_view = engine
            .auto_config_apply(ApplyReq {
                encoder: Some("hardware".into()),
                fps: Some(60),
                video_kbps: Some(6000),
                label: Some("  VideoToolbox 1080p60  ".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ok_view.applied.unwrap().label, "VideoToolbox 1080p60");
    }

    #[tokio::test]
    async fn studio_bench_persists_reloads_and_invalidates_on_host_sig_change() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start(dir.path()).await.unwrap();
        let cand = BenchCandidate {
            encoder_backend: "nvenc".into(),
            studio_encoder_id: "ffmpeg_nvenc".into(),
            label: "NVENC 1080p60".into(),
            family: EncoderFamily::Nvenc,
            hardware: true,
            preset: None,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 8_000,
            frames_tested: 120,
            encode_ms_avg: 3.2,
            encode_ms_p95: 4.1,
            encode_ms_max: 5.0,
            budget_ms: 16.67,
            fps_achieved: 60.0,
            headroom_pct: 80.8,
            skipped_frames: 0,
            lagged_frames: 0,
            cpu_pct: None,
            gpu_pct: None,
            elapsed_ms: 200,
            passed: true,
            reason: "ok".into(),
        };
        let picked = BenchRecommendation {
            encoder: "hardware".into(),
            video_encoder: Some("ffmpeg_nvenc".into()),
            studio_encoder_id: "ffmpeg_nvenc".into(),
            encoder_settings: serde_json::Map::new(),
            label: "NVENC".into(),
            preset: None,
            scale: None,
            width: 1920,
            height: 1080,
            fps: 60,
            video_kbps: 8_000,
            headroom_pct: 80.8,
            limited_by: ferrocast_golive::bench::LimitedBy::Upload,
            why: "test".into(),
        };
        let bench = super::persisted_bench_from_run(&[cand], &picked, "windows|nv=551.23", 1_700_000_000_000);
        assert!(bench.max_mpx_s > 400.0, "NVENC 3.2ms@1080p60 headroom should estimate >400 Mpx/s: {bench:?}");
        crate::studio::save_setting(&engine, crate::golive_plan::STUDIO_BENCH_KEY, &bench).await.unwrap();
        drop(engine);

        let reloaded = Engine::start(dir.path()).await.unwrap();
        let saved = reloaded.studio().studio_bench.read().unwrap().clone().expect("studio_bench loaded");
        assert_eq!(saved, bench);

        let info_same = crate::studio_host::HostBudgetInfo {
            os: "windows".into(),
            encoder_id: "ffmpeg_nvenc".into(),
            cpu_cores: 8,
            nvidia_driver_major: Some(551),
            nvidia_driver_minor: Some(23),
            host_sig: "windows|nv=551.23".into(),
            ..Default::default()
        };
        let b_same = crate::golive_plan::PcBudget::from_host_info(&info_same, Some(&saved), true);
        assert_eq!(b_same.source, crate::golive_plan::BudgetSource::Bench);

        let mut info_upgraded = info_same;
        info_upgraded.host_sig = "windows|nv=560.70".into();
        let b_stale = crate::golive_plan::PcBudget::from_host_info(&info_upgraded, Some(&saved), true);
        assert_eq!(b_stale.source, crate::golive_plan::BudgetSource::NvencDefault);
    }

    #[test]
    fn persisted_bench_isolates_x264_presets_and_caps_hw_pipelined_extrapolation() {
        let mk = |enc: &str, preset: Option<&str>, ms: f64| BenchCandidate {
            encoder_backend: if enc == "obs_x264" { "x264".into() } else { "nvenc".into() },
            studio_encoder_id: enc.into(),
            label: enc.into(),
            family: if enc == "obs_x264" { EncoderFamily::X264 } else { EncoderFamily::Nvenc },
            hardware: enc != "obs_x264",
            preset: preset.map(str::to_string),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 6_000,
            frames_tested: 120,
            encode_ms_avg: ms,
            encode_ms_p95: ms * 1.2,
            encode_ms_max: ms * 1.5,
            budget_ms: 16.67,
            fps_achieved: 60.0,
            headroom_pct: 50.0,
            skipped_frames: 0,
            lagged_frames: 0,
            cpu_pct: None,
            gpu_pct: None,
            elapsed_ms: 200,
            passed: true,
            reason: "ok".into(),
        };

        let cands = [mk("obs_x264", Some("ultrafast"), 0.8), mk("obs_x264", Some("veryfast"), 7.0)];
        let picked_sw = BenchRecommendation {
            encoder: "software".into(),
            video_encoder: Some("obs_x264".into()),
            studio_encoder_id: "obs_x264".into(),
            encoder_settings: serde_json::Map::new(),
            label: "x264 veryfast".into(),
            preset: Some("veryfast".into()),
            scale: None,
            width: 1920,
            height: 1080,
            fps: 60,
            video_kbps: 6_000,
            headroom_pct: 58.0,
            limited_by: ferrocast_golive::bench::LimitedBy::Upload,
            why: "test".into(),
        };
        let sw_bench = super::persisted_bench_from_run(&cands, &picked_sw, "win|cores=8", 1);
        assert!(sw_bench.max_mpx_s < 400.0, "ultrafast must not inflate veryfast ceiling: {sw_bench:?}");

        let hw_cands = [mk("ffmpeg_nvenc", None, 0.35)];
        let picked_hw = BenchRecommendation {
            encoder: "hardware".into(),
            video_encoder: Some("ffmpeg_nvenc".into()),
            studio_encoder_id: "ffmpeg_nvenc".into(),
            encoder_settings: serde_json::Map::new(),
            label: "NVENC".into(),
            preset: None,
            scale: None,
            width: 1920,
            height: 1080,
            fps: 60,
            video_kbps: 8_000,
            headroom_pct: 95.0,
            limited_by: ferrocast_golive::bench::LimitedBy::Upload,
            why: "test".into(),
        };
        let hw_bench = super::persisted_bench_from_run(&hw_cands, &picked_hw, "win|nv=551.23", 1);
        assert!(hw_bench.max_mpx_s <= 600.0, "pipelined GPU submit must not extrapolate to 2500 Mpx/s: {hw_bench:?}");
    }
}
