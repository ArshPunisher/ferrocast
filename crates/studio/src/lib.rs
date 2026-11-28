//! The Studio (2.1, LIVE-STUDIO.md Track B), on Ferrocast's own engine (`ferrocore`): the
//! scene collection is an OBS-format document edited here (`stage::Stage`) and drawn, mixed,
//! encoded and sent by our engine, so collections move between OBS and Ferrocast unchanged.

mod collection;
#[cfg(feature = "deepfilter")]
mod denoiser;
mod ffi;
pub mod import;
pub mod import_preview;
pub mod kinds;
mod naming;
mod ndi_io;
mod netmedia;
pub use ndi_io::NdiOut;
mod own;
pub use own::matching_camera_holds_session;
// Remove background: Vision through objc2 on a Mac (the same unsafe calls as ffi's), RVM on Windows.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod cutout;
pub mod projector;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod segment;
pub mod stage;
// The virtual camera on Linux (v4l2loopback); built for its tests elsewhere too.
pub mod streamlabs;
pub mod system;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod virtualcam;
pub mod webfetch;
// The virtual camera on Windows: Ferrocast Camera (DirectShow, and Windows 11's for every app).
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub mod wincam;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The kind id of Ferrocast's Web page source (macOS; OBS's Browser source keys).
pub const WEB_PAGE_KIND: &str = "ferrocast_web";

pub use collection::{
    AudioInfo, BOUNDS, CHANNEL_KEYS, Captions, DEINTERLACE, Face, FaceFollow, FilterInfo, FollowLook, ItemEdit,
    ItemInfo, LevelInfo, MediaAct, MediaInfo, MissingFile, Monitor, NamedTransition, ResolvedTransition, SceneInfo,
    SoundTap, SourceUse, StudioOptions, TBarState, TapReader, Transform, TransitionInfo, TransitionMatrixRule,
    VERTICAL_SIZE, VerticalInfo, WebAct, ask_captions_access, captions_access, find_busy, find_faces, focus_x,
};
pub use collection::{jpeg_data_url, png_data_url};
pub use projector::{MultiviewLayout, MultiviewLook, ProjectorOf, Screen};

/// What went wrong in the Studio.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StudioError {
    #[error("{0}")]
    Failed(String),
}

/// The canvas: what's composed and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { width: 1920, height: 1080, fps: 30 }
    }
}

/// A kind of source, filter, transition, encoder or output, as OBS names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kind {
    pub id: String,
    pub name: String,
}

/// Every kind the loaded plugins offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Kinds {
    pub inputs: Vec<Kind>,
    pub filters: Vec<Kind>,
    pub transitions: Vec<Kind>,
    pub encoders: Vec<Kind>,
    pub outputs: Vec<Kind>,
}

/// One option of a source or filter, enough to draw it in a settings form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionInfo {
    /// The settings key.
    pub key: String,
    pub label: String,
    /// bool, int, float, text, path, list, color, color_alpha, button, font, editable_list,
    /// frame_rate, group.
    pub kind: &'static str,
    pub help: String,
    pub visible: bool,
    pub enabled: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    /// A number OBS shows as a slider.
    pub slider: bool,
    /// A number's unit as OBS writes it after the value (" dB", " ms", "%"), or "".
    pub suffix: String,
    /// A list's choices, as shown.
    pub choices: Vec<String>,
    /// The value saved for each choice (same order): a number, text or true/false.
    pub values: Vec<serde_json::Value>,
    /// A group's own options.
    pub group: Vec<OptionInfo>,
    /// A group with an on/off switch of its own (its key holds true or false).
    pub checkable: bool,
    /// A path's kind: "file", "save" or "folder"; an editable list's: "files" (files, or
    /// files and addresses) or "" (plain text, as for anything else).
    pub path: &'static str,
    /// The file types of a path or a list of files, as OBS writes them
    /// ("Image Files (*.png *.jpg);;All Files (*.*)").
    pub filter: String,
    /// A text's kind: "multiline" (a box), "password" (hidden), "info" (read only) or ""
    /// (one line).
    pub text_type: &'static str,
    /// A text OBS shows in a fixed-width font (code, CSS).
    pub monospace: bool,
    /// Shown only while another option holds one of some values (OBS hides these in its
    /// "modified" callbacks: a Window Capture has no use for the Display list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shown_when: Option<ShownWhen>,
    /// What a new one starts at (the kind's defaults): what a double-click puts a slider
    /// back to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// The option and values an option is shown for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShownWhen {
    pub key: String,
    pub values: Vec<serde_json::Value>,
}

/// A box inside a window, in points from its top-left corner (the page's own coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// The page's visible height (`window.innerHeight`), or 0 if unknown. The page can draw
    /// under a see-through title bar, so its top sits that far inside the webview; counting
    /// up from the webview's bottom by this height finds it (2.1: the preview sat a title bar
    /// too high).
    pub vh: f64,
}

/// The margin round a preview's picture, in points, as OBS's own: the outline and handles of
/// an item at the canvas's edge show in it instead of being cut off. The native view is this
/// much bigger than the page's box on every side, so the picture stays exactly on the box.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) const PREVIEW_EDGE: f64 = 10.0;

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn with_edge(r: Rect) -> Rect {
    Rect {
        x: r.x - PREVIEW_EDGE,
        y: r.y - PREVIEW_EDGE,
        w: r.w + 2.0 * PREVIEW_EDGE,
        h: r.h + 2.0 * PREVIEW_EDGE,
        vh: r.vh,
    }
}

/// The margin in device pixels.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn edge_px(scale: f64) -> u32 {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (PREVIEW_EDGE * s).round() as u32
}

/// How far a view snapped outward to whole device pixels reaches past the box the page asked
/// for, on each side, in points.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Overhang {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

/// The page's holes (x, y, w, h, corner radius, in points from the asked-for box's top left) as
/// the snapped view needs them, from the view's own top left. A hole is measured against the
/// box the page asked for, but the view is that box rounded out to whole device pixels: a hole
/// that reaches a side of the box (a dialog covering most of the stage is sent as the whole box)
/// has to reach the view's side too, or a thin strip of picture and shade is left across the
/// dialog along the right and bottom edges. A hole that stops short of a side keeps its place.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn fit_holes(
    holes: &[(f64, f64, f64, f64, f64)],
    over: Overhang,
    box_w: f64,
    box_h: f64,
) -> Vec<(f64, f64, f64, f64, f64)> {
    // Half a device pixel is well under any gap a page leaves on purpose.
    const NEAR: f64 = 0.5;
    holes
        .iter()
        .map(|&(x, y, w, h, r)| {
            let (x1, y1) = (x + w, y + h);
            // A side is only reached out to when the view reaches past the box there: with none,
            // the page's own gap (a half point at 2x is a device pixel) stays as it was sent.
            let nx0 = if x <= NEAR && over.left > 0.0 { (x + over.left).min(0.0) } else { x + over.left };
            let ny0 = if y <= NEAR && over.top > 0.0 { (y + over.top).min(0.0) } else { y + over.top };
            let nx1 = if x1 >= box_w - NEAR && over.right > 0.0 {
                (x1 + over.left).max(box_w + over.left + over.right)
            } else {
                x1 + over.left
            };
            let ny1 = if y1 >= box_h - NEAR && over.bottom > 0.0 {
                (y1 + over.top).max(box_h + over.top + over.bottom)
            } else {
                y1 + over.top
            };
            (nx0, ny0, nx1 - nx0, ny1 - ny0, r)
        })
        .collect()
}

/// A rectangle in device pixels (at least 1x1).
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn px(rect: Rect, scale: f64) -> (u32, u32) {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    ((rect.w * s).round().clamp(1.0, 16_384.0) as u32, (rect.h * s).round().clamp(1.0, 16_384.0) as u32)
}

/// A mouse action over the preview on Windows, in the page's CSS pixels from the window's top
/// left: the picture's child window takes clicks there, and the app passes each on to the page
/// (on a Mac they go through to the page by themselves).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PreviewInput {
    /// "down", "up", "move", "dblclick", "wheel" or "leave".
    pub kind: &'static str,
    /// The button pressed or let go: 0 left, 1 middle, 2 right (as DOM events number them).
    pub button: i16,
    /// The buttons held now, as DOM `buttons` (1 left, 2 right, 4 middle).
    pub buttons: u16,
    pub x: f64,
    pub y: f64,
    /// A wheel's turn in CSS pixels (DOM `deltaX`/`deltaY`).
    pub dx: f64,
    pub dy: f64,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Remove background's model on Windows (Robust Video Matting), once it's here → whether it's
/// used (a missing file isn't).
#[cfg(windows)]
pub fn use_matte_model(path: &Path) -> bool {
    ffi::matte::set_model(path)
}

/// The displays a full-screen projector can go on (macOS and Windows; none elsewhere yet). Main
/// thread.
pub fn screens() -> Vec<Screen> {
    #[cfg(any(target_os = "macos", windows))]
    {
        ffi::projector_window::screens()
            .into_iter()
            .enumerate()
            .map(|(index, (name, w, h))| Screen {
                index,
                name,
                width: w.round() as u32,
                height: h.round() as u32,
                main: index == 0,
            })
            .collect()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Vec::new()
    }
}

/// This app may record the screen (macOS's Screen Recording switch; always true elsewhere).
/// macOS answers for the app as it started: a change shows only after a restart.
pub fn screen_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        ffi::mac::screen_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks macOS for screen recording (its prompt, and Ferrocast listed in System Settings).
pub fn request_screen_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        ffi::mac::request_screen_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// A source name: 1 to 100 letters on one line (OBS's own names stay unique per scene).
pub fn check_name(name: &str) -> Result<(), StudioError> {
    let n = name.chars().count();
    if n == 0 || n > 100 || name.trim() != name || name.chars().any(char::is_control) {
        return Err(StudioError::Failed("name: 1 to 100 letters on one line.".into()));
    }
    Ok(())
}

/// Looks for files named like `names` (by file name) under `folder`, a few levels deep, as
/// OBS's Missing Files "Search directory" → each name's first match.
pub fn find_files(folder: &Path, names: &[String]) -> std::collections::HashMap<String, PathBuf> {
    let mut want: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for n in names {
        if let Some(f) = Path::new(n).file_name().and_then(|f| f.to_str()) {
            want.entry(f.to_lowercase()).or_default().push(n.clone());
        }
    }
    let mut found = std::collections::HashMap::new();
    let mut stack = vec![(folder.to_path_buf(), 0u8)];
    let mut looked = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            looked += 1;
            // A whole disk isn't searched: enough for a folder of media.
            if looked > 200_000 || found.len() == names.len() {
                return found;
            }
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                let hidden = p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with('.'));
                if depth < 8 && !hidden {
                    stack.push((p, depth + 1));
                }
            } else if let Some(f) = p.file_name().and_then(|f| f.to_str())
                && let Some(originals) = want.get(&f.to_lowercase())
            {
                for o in originals {
                    found.entry(o.clone()).or_insert_with(|| p.clone());
                }
            }
        }
    }
    found
}

/// What a replay buffer keeps and where it saves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPlan {
    pub folder: PathBuf,
    /// OBS's name pattern for saved replays.
    pub name: String,
    /// mkv, mp4, mov, flv or ts (through OBS's mux helper).
    pub format: String,
    pub spaces: bool,
    /// The last this many seconds are kept (and at most this many megabytes, 0: no limit).
    pub seconds: u32,
    pub mb: u32,
    pub tracks: Vec<u8>,
    pub encode: EncodeSettings,
}

/// What the virtual camera shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "what", content = "name")]
pub enum CameraShows {
    /// What's on air.
    Program,
    /// One scene or source (named), whatever is on air.
    Source(String),
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub(crate) struct AloneGuard {
    pub(crate) shared: std::sync::Arc<own::Shared>,
    pub(crate) name: String,
}

#[cfg(target_os = "windows")]
impl Drop for AloneGuard {
    fn drop(&mut self) {
        self.shared.release_alone(&self.name);
    }
}

/// The virtual camera: on Linux what's on air sent to a v4l2loopback camera; on Windows
/// Ferrocast Camera (`wincam`); a Mac needs a signed camera extension, not here yet, so
/// turning it on there says why.
#[derive(Debug)]
pub struct VirtualCamera {
    #[cfg(target_os = "linux")]
    pub(crate) cam: virtualcam::Loopback,
    #[cfg(target_os = "windows")]
    pub(crate) cam: wincam::Camera,
    // Declared AFTER `cam` so `wincam::Camera` stops and joins its thread before the solo scene
    // is released (Rust drops fields in declaration order).
    #[cfg(target_os = "windows")]
    pub(crate) _alone: Option<AloneGuard>,
}

impl VirtualCamera {
    pub fn active(&self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        return self.cam.active();
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        false
    }

    /// Windows 11's camera for every app is what's on (not DirectShow's).
    pub fn every_app(&self) -> bool {
        #[cfg(target_os = "windows")]
        return self.cam.every_app;
        #[cfg(not(target_os = "windows"))]
        false
    }
}

/// How a recording is made: its format, file names, tracks, splitting and encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPlan {
    pub folder: PathBuf,
    /// OBS's name pattern: `%CCYY` year, `%MM` month, `%DD` day, `%hh`, `%mm`, `%ss`…
    pub name: String,
    /// mp4, mov (crash-safe), fmp4, fmov (fragmented), mkv, flv or ts.
    pub format: String,
    /// Spaces allowed in the name (else they become underscores).
    pub spaces: bool,
    /// A file of the same name is replaced (else " (2)" is added).
    pub overwrite: bool,
    /// A new file every this many minutes or megabytes (0: never).
    pub split_minutes: u32,
    pub split_mb: u32,
    /// Audio tracks (1 to 6) in the file; empty is track 1.
    pub tracks: Vec<u8>,
    pub encode: EncodeSettings,
    /// ffmpeg muxer settings of the streamer's own (`key=value` pairs, space apart), as OBS's
    /// "Custom Muxer Settings".
    #[serde(default)]
    pub muxer: String,
    /// Kept from when OBS's engine was there too (settings files still carry it).
    #[serde(default)]
    pub own_encoder: bool,
}

impl Default for RecordPlan {
    fn default() -> Self {
        RecordPlan {
            folder: PathBuf::new(),
            name: "%CCYY-%MM-%DD %hh-%mm-%ss".into(),
            format: "mp4".into(),
            spaces: true,
            overwrite: false,
            split_minutes: 0,
            split_mb: 0,
            tracks: vec![1],
            encode: EncodeSettings::with_encoder("obs_x264"),
            muxer: String::new(),
            own_encoder: false,
        }
    }
}

impl RecordPlan {
    /// The file extension the format writes. Ferrocore writes crash-safe MP4 (MOV the same,
    /// named .mov) and MKV; OBS's other formats, saved before, become the nearest of those:
    /// fragmented MP4 and MOV need no fragments (ours never loses a file), FLV, MPEG-TS and HLS
    /// go to MKV (crash-safe too, and remuxed to MP4 when that's on).
    pub fn extension(&self) -> &str {
        match self.format.as_str() {
            "mov" | "fmov" => "mov",
            "mkv" | "flv" | "ts" | "hls" => "mkv",
            _ => "mp4",
        }
    }
}

/// How a recording is going.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordStats {
    /// False once the recording stopped, by itself too (a full disk, a drive taken away).
    pub active: bool,
    pub paused: bool,
    pub bytes: u64,
    pub frames: u32,
    /// Why it stopped, in one line, when it did by itself ("the disk is full").
    pub error: Option<String>,
    /// Times it went on in a new file because the video encoder was rebuilt (display sleep, the
    /// system taking the session back).
    pub auto_splits: u32,
}

/// How video and sound are encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeSettings {
    /// An encoder id from `kinds().encoders` (`obs_x264`, VideoToolbox's…).
    pub video_encoder: String,
    pub video_kbps: u32,
    /// Varying bitrate (VBR) instead of steady (CBR).
    pub vbr: bool,
    /// Strict 1× VBV / 1 s DataRateLimits CBR used when capped for a platform limit.
    #[serde(default)]
    pub strict_cbr: bool,
    /// Constant quality instead of a bitrate (recordings): 0 is lossless, 18 very high.
    pub quality: Option<u32>,
    /// Seconds between keyframes (platforms ask for 2; 0 lets the encoder pick).
    pub keyframe_s: u32,
    /// "high", "main" or "baseline".
    pub profile: String,
    /// B-frames in the Mac's encoder (off by default).
    pub bframes: bool,
    /// Sent at this size instead of the canvas's.
    pub scale: Option<(u32, u32)>,
    /// How a picture sent at another size is scaled: "bilinear", "bicubic" (OBS's default),
    /// "lanczos" or "area".
    #[serde(default = "default_scale_filter")]
    pub scale_filter: String,
    pub audio_kbps: u32,
    /// The encoder's own options (OBS's keys), applied over the ones above.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_scale_filter() -> String {
    "bicubic".into()
}

impl EncodeSettings {
    /// This encoder at 6000 kbps CBR, 2 s keyframes, high profile, AAC 160.
    pub fn with_encoder(id: &str) -> EncodeSettings {
        EncodeSettings {
            video_encoder: id.into(),
            video_kbps: 6000,
            vbr: false,
            strict_cbr: false,
            quality: None,
            keyframe_s: 2,
            profile: "high".into(),
            bframes: false,
            scale: None,
            scale_filter: default_scale_filter(),
            audio_kbps: 160,
            extra: Default::default(),
        }
    }
}

/// What a stream sends, and what it does when the connection drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSettings {
    pub encode: EncodeSettings,
    /// Tries again (retries, every n seconds) when the connection drops; None gives up.
    pub reconnect: Option<(u32, u32)>,
    /// Viewers see it this many seconds later (0: no delay).
    pub delay_s: u32,
    /// Twitch's VOD track: this audio track (2 to 6) sent too, as a second track (OBS's own
    /// way, Enhanced RTMP multitrack audio). Twitch keeps it for the VOD (music left out there)
    /// while viewers hear track 1. None: one track.
    #[serde(default)]
    pub vod_track: Option<u8>,
    /// Kept from when OBS's engine was there too (settings files still carry it); Ferrocore
    /// always encodes now.
    #[serde(default)]
    pub own_encoder: bool,
    /// More qualities sent as extra video tracks (width, height, kbps): Twitch Enhanced
    /// Broadcasting's plan, set for one stream, never saved.
    #[serde(skip)]
    pub video_tracks: Vec<(u32, u32, u32)>,
}

/// The engine's own load.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineStats {
    /// Ferrocast's CPU use, % of one machine.
    pub cpu: f64,
    pub fps: f64,
    /// Average time to render a frame.
    pub frame_ms: f64,
    /// Frames rendered late since start, and all frames.
    pub lagged: u32,
    pub frames: u32,
    pub memory_mb: u64,
    /// Frames the encoders skipped because they fell behind (encoding lag), and all frames
    /// given to them.
    pub skipped: u32,
    pub encoded: u32,
    /// Faults the engine caught and carried on from, and sources its watchdog started again.
    pub recovered: u32,
    pub restarts: u32,
    /// Time the encoders take for one frame, all outputs together (smoothed), in ms.
    pub encode_ms: f64,
    /// All the time the encoders took since the engine started, in ms.
    pub encoding_ms: u64,
    /// Missed frames attributed to rendering vs encoding, when any occurred.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub missed_render: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub missed_encode: u32,
    /// Few-word explanation of why frames were missed or skipped, shown on Stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_reason: Option<String>,
    /// Counts and recovery hint shown behind Stats' `(i)` tooltip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_detail: Option<String>,
}

/// Builds a short Stats explanation and `(i)` detail for missed/skipped frames.
pub fn describe_missed_frames(
    missed_render: u32,
    missed_encode: u32,
    skipped_encode: u32,
) -> (Option<String>, Option<String>) {
    let encode_total = missed_encode.saturating_add(skipped_encode);
    match (missed_render > 0, encode_total > 0) {
        (false, false) => (None, None),
        (true, false) => (
            Some("render fell behind".into()),
            Some(format!(
                "{missed_render} missed in render (scene took too long to draw). Use fewer sources or a lower frame rate."
            )),
        ),
        (false, true) => {
            let mut parts = Vec::new();
            if missed_encode > 0 {
                parts.push(format!("{missed_encode} held the frame loop back"));
            }
            if skipped_encode > 0 {
                parts.push(format!("{skipped_encode} skipped by the encoder"));
            }
            (
                Some("encoder fell behind".into()),
                Some(format!(
                    "Encoder backlog ({}). Lower the quality or resolution, or use the hardware encoder.",
                    parts.join(", ")
                )),
            )
        }
        (true, true) => (
            Some("render and encoder fell behind".into()),
            Some(format!(
                "{missed_render} missed in render, {encode_total} missed or skipped in encoding. Lower the quality, resolution, or source load."
            )),
        ),
    }
}

/// How a source's picture looks now (`Stage::look`): for catching a black or frozen camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Look {
    /// Average brightness, 0 (black) to 255.
    pub luma: u8,
    /// How much the brightness varies across the picture (0: one flat colour).
    pub spread: u8,
    /// Changes whenever the picture does (a frozen one keeps it).
    pub sig: u64,
}

impl Look {
    /// From an RGBA picture.
    pub fn of(rgba: &[u8]) -> Look {
        let lumas: Vec<u32> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| (u32::from(p[0]) * 54 + u32::from(p[1]) * 183 + u32::from(p[2]) * 19) >> 8)
            .collect();
        let n = lumas.len().max(1) as u32;
        let mean = lumas.iter().sum::<u32>() / n;
        let var = lumas.iter().map(|l| l.abs_diff(mean).pow(2)).sum::<u32>() / n;
        // FNV-1a over the brightness in steps of 4 (sensor noise doesn't change it much).
        let sig =
            lumas.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, l| (h ^ u64::from(*l >> 2)).wrapping_mul(0x100_0000_01b3));
        Look { luma: mean.min(255) as u8, spread: (var as f32).sqrt().min(255.0) as u8, sig }
    }

    /// Black: dark and flat (a covered lens, a camera another app holds).
    pub fn black(&self) -> bool {
        self.luma < 12 && self.spread < 6
    }
}

/// How a stream is going.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamStats {
    pub active: bool,
    pub reconnecting: bool,
    pub bytes: u64,
    pub frames: u32,
    pub dropped: u32,
    /// A direct destination's network (ping, loss, resends), once it's connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<ferrocast_golive::Link>,
    /// Why a direct destination is trying again or failed, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// True when the sender gave up for this stream (`PushState::Failed`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    /// Attempts made before giving up.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempts: u32,
    /// Full cause and what to do, behind an (i).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when the server/platform refused the stream rather than a network outage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    /// What the stream delay holds now, in megabytes (left out without a delay).
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub delay_mb: f32,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

/// A delayed stream's tail being sent after it ended: seconds to go, megabytes left.
pub fn delay_drain() -> Option<(f64, f64)> {
    own::delay_drain()
}

/// The most memory a stream delay holds, in megabytes (from the next stream).
pub fn set_delay_cap_mb(mb: u32) {
    own::set_delay_cap_mb(mb);
}

/// The sound's format from the settings: the rate the engine mixes at (44100 or 48000, from the next
/// start) and the names of the six audio tracks in a recording's file.
pub fn set_audio_format(rate: u32, track_names: Vec<String>) {
    own::set_audio_format(rate, track_names);
}

/// Broadcast timing (29.97, 59.94, 23.976 for a canvas of 30, 60, 24 fps), from the next time the
/// engine opens.
pub fn set_ntsc(on: bool) {
    own::set_ntsc(on);
}

/// Ends any delayed tail being sent (the streamer chose Stop now).
pub fn discard_delay() {
    own::discard_delay();
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// The app in front (its name as the Dock shows it), for a scene per game. None where it
/// can't be told (not a Mac yet) or nothing is.
pub fn front_app() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
        ws.frontmostApplication().and_then(|a| a.localizedName()).map(|n| ferrocore::obs::app_name(&n.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Apps open now with a window in the Dock (not this one), by name, sorted: what a scene per
/// game can be set for.
pub fn running_apps() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSApplicationActivationPolicy;
        let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
        let me = objc2_app_kit::NSRunningApplication::currentApplication()
            .localizedName()
            .map(|n| ferrocore::obs::app_name(&n.to_string()));
        let mut names: Vec<String> = ws
            .runningApplications()
            .iter()
            .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
            .filter_map(|a| a.localizedName().map(|n| ferrocore::obs::app_name(&n.to_string())))
            .filter(|n| !n.is_empty())
            .filter(|n| Some(n) != me.as_ref())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hole_reaching_the_box_reaches_the_snapped_view() {
        // A 400.4 x 300.3 box on a 2x screen: the view is out to whole pixels, half a pixel
        // more on the left and top, 0.3 on the right and bottom.
        let over = Overhang { left: 0.4, top: 0.1, right: 0.3, bottom: 0.4 };
        // A dialog covering most of the stage is sent as the whole box.
        let whole = [(0.0, 0.0, 400.4, 300.3, 0.0)];
        let fit = fit_holes(&whole, over, 400.4, 300.3);
        let (x, y, w, h, _) = fit[0];
        assert_eq!((x, y), (0.0, 0.0), "from the view's own corner");
        assert!((x + w - (400.4 + 0.4 + 0.3)).abs() < 1e-9, "to the view's right edge: {fit:?}");
        assert!((y + h - (300.3 + 0.1 + 0.4)).abs() < 1e-9, "to the view's bottom edge: {fit:?}");
        // A hole that stops short of the sides moves with the box and keeps its size.
        let inner = [(50.0, 40.0, 100.0, 80.0, 12.0)];
        assert_eq!(fit_holes(&inner, over, 400.4, 300.3), vec![(50.4, 40.1, 100.0, 80.0, 12.0)]);
        // One reaching past a side already covers the view's side: it stays at least that far.
        let past = [(300.0, 200.0, 200.0, 200.0, 8.0)];
        let (x, y, w, h, r) = fit_holes(&past, over, 400.4, 300.3)[0];
        assert_eq!((x, y, r), (300.4, 200.1, 8.0));
        assert!(x + w >= 401.1 && y + h >= 300.8, "{x} {y} {w} {h}");
        // No overhang (a box already on whole pixels) changes nothing.
        assert_eq!(fit_holes(&whole, Overhang::default(), 400.4, 300.3), whole.to_vec());
    }

    #[test]
    fn a_near_edge_hole_keeps_its_gap_on_a_side_with_no_overhang() {
        // Half a point from the left and top, and from the right and bottom, with nothing to
        // reach out to: the gaps the page sent stay (at 2x each is one device pixel).
        let near = [(0.5, 0.5, 399.4, 299.3, 0.0)];
        assert_eq!(fit_holes(&near, Overhang::default(), 400.4, 300.3), near.to_vec());
        let small = [(0.5, 40.0, 100.0, 80.0, 0.0)];
        assert_eq!(fit_holes(&small, Overhang::default(), 400.4, 300.3), small.to_vec());
        // Each side on its own: the left reaches out, the others keep their gap.
        let over = Overhang { left: 0.4, ..Overhang::default() };
        let (x, y, w, h, _) = fit_holes(&near, over, 400.4, 300.3)[0];
        assert_eq!(x, 0.0, "the side with overhang reaches the view's edge");
        assert_eq!(y, 0.5, "the side without keeps its gap");
        assert!((x + w - 399.9 - 0.4).abs() < 1e-9 && (y + h - 299.8).abs() < 1e-9, "{x} {y} {w} {h}");
        // And the far sides the same way.
        let over = Overhang { right: 0.3, bottom: 0.0, ..Overhang::default() };
        let (x, y, w, h, _) = fit_holes(&near, over, 400.4, 300.3)[0];
        assert_eq!((x, y), (0.5, 0.5));
        assert!((x + w - 400.7).abs() < 1e-9, "the right reaches the view's edge: {w}");
        assert!((y + h - 299.8).abs() < 1e-9, "the bottom keeps its gap: {h}");
    }

    #[test]
    fn preview_sizes_are_device_pixels() {
        let r = Rect { x: 10.0, y: 20.0, w: 640.0, h: 360.0, vh: 800.0 };
        assert_eq!(px(r, 2.0), (1280, 720));
        assert_eq!(px(r, f64::NAN), (640, 360), "a bad scale is 1");
        assert_eq!(px(Rect { w: 0.0, h: -5.0, ..r }, 1.0), (1, 1), "never zero");
    }

    #[test]
    fn source_names_are_checked() {
        assert!(check_name("Webcam").is_ok());
        for bad in ["", " lead", "a\nb", &"x".repeat(101)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn missed_frame_reasons_explain_render_vs_encode_with_counts() {
        assert_eq!(describe_missed_frames(0, 0, 0), (None, None));
        let (r, d) = describe_missed_frames(42, 0, 0);
        assert_eq!(r.as_deref(), Some("render fell behind"));
        assert!(d.as_deref().is_some_and(|s| s.contains("42 missed in render")), "{d:?}");

        let (r, d) = describe_missed_frames(0, 7, 12);
        assert_eq!(r.as_deref(), Some("encoder fell behind"));
        assert!(d.as_deref().is_some_and(|s| s.contains("7 held") && s.contains("12 skipped")), "{d:?}");

        let (r, d) = describe_missed_frames(5, 3, 2);
        assert_eq!(r.as_deref(), Some("render and encoder fell behind"));
        assert!(
            d.as_deref().is_some_and(|s| s.contains("5 missed in render") && s.contains("5 missed or skipped")),
            "{d:?}"
        );
    }
}

#[cfg(test)]
mod look_tests {
    use super::Look;

    #[test]
    fn a_black_flat_picture_is_black_and_a_moving_one_changes() {
        let black = Look::of(&[0u8, 0, 0, 255].repeat(32 * 18));
        assert!(black.black());
        let mut grad = Vec::new();
        for i in 0..(32 * 18) {
            let v = (i % 32 * 8) as u8;
            grad.extend_from_slice(&[v, v, v, 255]);
        }
        let a = Look::of(&grad);
        assert!(!a.black() && a.spread > 20, "{a:?}");
        grad[0..4].copy_from_slice(&[255, 255, 255, 255]);
        assert_ne!(Look::of(&grad).sig, a.sig, "a change shows");
    }
}
