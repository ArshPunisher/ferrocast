//! The Studio (2.1, LIVE-STUDIO.md Track B), on Ferrocast's own engine (`ferrocore`): the
//! scene collection is an OBS-format document edited here (`stage::Stage`) and drawn, mixed,
//! encoded and sent by our engine, so collections move between OBS and Ferrocast unchanged.

mod collection;
#[cfg(feature = "deepfilter")]
mod denoiser;
mod ffi;
pub mod import;
pub mod kinds;
mod naming;
#[cfg(target_os = "macos")]
mod netmedia;
mod own;
// Vision through objc2 (the same unsafe calls as ffi's).
#[cfg(target_os = "macos")]
mod cutout;
pub mod projector;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod segment;
pub mod stage;
pub mod streamlabs;
pub mod system;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The kind id of Ferrocast's Web page source (macOS; OBS's Browser source keys).
pub const WEB_PAGE_KIND: &str = "ferrocast_web";

pub use collection::{
    AudioInfo, BOUNDS, CHANNEL_KEYS, Captions, DEINTERLACE, Face, FaceFollow, FilterInfo, FollowLook, ItemEdit,
    ItemInfo, LevelInfo, MediaAct, MediaInfo, MissingFile, Monitor, SceneInfo, SoundTap, SourceUse, StudioOptions,
    TBarState, TapReader, Transform, TransitionInfo, VERTICAL_SIZE, VerticalInfo, WebAct, ask_captions_access,
    captions_access, find_busy, find_faces, focus_x,
};
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
pub(crate) const PREVIEW_EDGE: f64 = 10.0;

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
pub(crate) fn edge_px(scale: f64) -> u32 {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (PREVIEW_EDGE * s).round() as u32
}

/// A rectangle in device pixels (at least 1x1).
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

/// The virtual camera (it needs a signed camera extension, which Ferrocast doesn't have yet):
/// never made, so turning it on says why.
#[derive(Debug)]
pub struct VirtualCamera(());

impl VirtualCamera {
    pub fn active(&self) -> bool {
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
    /// The file extension the format writes.
    pub fn extension(&self) -> &str {
        match self.format.as_str() {
            "fmp4" => "mp4",
            "fmov" => "mov",
            "hls" => "m3u8",
            f => f,
        }
    }
}

/// How a recording is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RecordStats {
    pub active: bool,
    pub paused: bool,
    pub bytes: u64,
    pub frames: u32,
}

/// How video and sound are encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeSettings {
    /// An encoder id from `kinds().encoders` (`obs_x264`, VideoToolbox's…).
    pub video_encoder: String,
    pub video_kbps: u32,
    /// Varying bitrate (VBR) instead of steady (CBR).
    pub vbr: bool,
    /// Constant quality instead of a bitrate (recordings): 0 is lossless, 18 very high.
    pub quality: Option<u32>,
    /// Seconds between keyframes (platforms ask for 2; 0 lets the encoder pick).
    pub keyframe_s: u32,
    /// "high", "main" or "baseline".
    pub profile: String,
    pub bframes: bool,
    /// Sent at this size instead of the canvas's.
    pub scale: Option<(u32, u32)>,
    pub audio_kbps: u32,
    /// The encoder's own options (OBS's keys), applied over the ones above.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl EncodeSettings {
    /// This encoder at 6000 kbps CBR, 2 s keyframes, high profile, AAC 160.
    pub fn with_encoder(id: &str) -> EncodeSettings {
        EncodeSettings {
            video_encoder: id.into(),
            video_kbps: 6000,
            vbr: false,
            quality: None,
            keyframe_s: 2,
            profile: "high".into(),
            bframes: true,
            scale: None,
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
}

/// The engine's own load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
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
    /// Why a direct destination is trying again, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The app in front (its name as the Dock shows it), for a scene per game. None where it
/// can't be told (not a Mac yet) or nothing is.
pub fn front_app() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
        ws.frontmostApplication().and_then(|a| a.localizedName()).map(|n| n.to_string())
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
        let me = objc2_app_kit::NSRunningApplication::currentApplication().localizedName().map(|n| n.to_string());
        let mut names: Vec<String> = ws
            .runningApplications()
            .iter()
            .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
            .filter_map(|a| a.localizedName().map(|n| n.to_string()))
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
