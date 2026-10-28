//! The Studio's engine (2.1, Track B) as the dashboard reaches it: over HTTP like everything
//! else (the window has no IPC permissions). The desktop app runs libobs on its main thread
//! and registers itself here as the [`StudioHost`]; the headless server has none, so its routes
//! answer "not on this computer". The engine itself never loads libobs.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Engine, EngineError};

/// What `GET /studio/engine` shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StudioStatus {
    /// OBS's engine can be used here (found on this computer, a desktop app).
    pub available: bool,
    pub running: bool,
    /// libobs's version, while running.
    pub version: Option<String>,
    /// "metal", "opengl", "d3d11".
    pub renderer: Option<String>,
    /// Why it isn't available, in plain words with what to do.
    pub detail: Option<String>,
}

/// A box in the dashboard window, in CSS pixels from its top-left corner.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PreviewRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// The page's visible height (`window.innerHeight`); 0 or missing if unknown.
    #[serde(default)]
    pub vh: f64,
    /// Where the canvas is inside the box (points from its top left). The box is then the
    /// whole stage: zoomed out, the outlines of what's off the canvas show round it. Missing:
    /// the canvas fills the box, with a small margin round it for handles.
    #[serde(default)]
    pub canvas: Option<CanvasBox>,
    /// The page's colour round the canvas (0xRRGGBB), so the stage matches the theme;
    /// missing: dark.
    #[serde(default)]
    pub bg: Option<u32>,
    /// Where the page's menus and panels lie over the box (points from its top left): the
    /// picture isn't drawn there, so they show through instead of the whole preview hiding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holes: Vec<PreviewHole>,
    /// How much of the picture shows, 0 to 1 (missing: all of it): dimmed under a dialog's
    /// backdrop like the page round it, hidden while the window is being resized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f64>,
    /// A dark layer over the picture, 0 to 1, drawn by the app: a dialog's backdrop over the
    /// page dims the preview the same (missing: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shade: Option<f64>,
}

/// At most this many holes (menus, panels) at once.
pub const PREVIEW_HOLES: usize = 16;

impl PreviewRect {
    /// `bg` as libobs wants a display's colour (0xAABBGGRR).
    pub fn bg_abgr(&self) -> Option<u32> {
        let rgb = self.bg.filter(|c| *c <= 0xff_ffff)?;
        let (r, g, b) = (rgb >> 16 & 0xff, rgb >> 8 & 0xff, rgb & 0xff);
        Some(0xff00_0000 | b << 16 | g << 8 | r)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CanvasBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A menu or panel over the preview: its box and corner radius (points), so the picture is left
/// out exactly where it lies, rounded corners and all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PreviewHole {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(default)]
    pub r: f64,
}

impl PreviewRect {
    pub fn validate(&self) -> Result<(), String> {
        let ok = |v: f64, lo: f64, hi: f64| v.is_finite() && (lo..=hi).contains(&v);
        if !(ok(self.x, -20_000.0, 20_000.0) && ok(self.y, -20_000.0, 20_000.0)) {
            return Err("rect: the box is off the window.".into());
        }
        if !ok(self.vh, 0.0, 16_384.0) {
            return Err("rect: vh is the page's height, 0 to 16384.".into());
        }
        if !(ok(self.w, 1.0, 16_384.0) && ok(self.h, 1.0, 16_384.0)) {
            return Err("rect: the box must be 1 to 16384 pixels wide and high.".into());
        }
        if let Some(c) = self.canvas
            && !(ok(c.x, -20_000.0, 20_000.0)
                && ok(c.y, -20_000.0, 20_000.0)
                && ok(c.w, 1.0, 16_384.0)
                && ok(c.h, 1.0, 16_384.0))
        {
            return Err("canvas: inside the box, 1 to 16384 pixels wide and high.".into());
        }
        if self.bg.is_some() && self.bg_abgr().is_none() {
            return Err("bg: a colour, 0 to 0xFFFFFF.".into());
        }
        if self.opacity.is_some_and(|o| !(0.0..=1.0).contains(&o)) {
            return Err("opacity: 0 to 1.".into());
        }
        if self.shade.is_some_and(|o| !(0.0..=1.0).contains(&o)) {
            return Err("shade: 0 to 1.".into());
        }
        if self.holes.len() > PREVIEW_HOLES {
            return Err(format!("holes: at most {PREVIEW_HOLES}."));
        }
        if !self.holes.iter().all(|c| {
            ok(c.x, -20_000.0, 20_000.0)
                && ok(c.y, -20_000.0, 20_000.0)
                && ok(c.w, 0.0, 20_000.0)
                && ok(c.h, 0.0, 20_000.0)
                && ok(c.r, 0.0, 200.0)
        }) {
            return Err("holes: boxes of 0 to 20000 pixels near the box, corners 0 to 200.".into());
        }
        Ok(())
    }
}

/// The desktop app's side of the Studio (it owns libobs and the window).
#[async_trait::async_trait]
pub trait StudioHost: Send + Sync + std::fmt::Debug {
    async fn status(&self) -> StudioStatus;
    async fn start(&self) -> Result<StudioStatus, String>;
    async fn stop(&self) -> Result<StudioStatus, String>;
    /// Every kind of source, filter, transition, encoder and output (`ferrocast_studio::Kinds`).
    async fn kinds(&self) -> Result<Value, String>;
    /// A kind's options (`ferrocast_studio::OptionInfo[]`).
    async fn options(&self, kind: String) -> Result<Value, String>;
    /// Shows the preview at the box in the dashboard window, or hides it (None).
    async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String>;
    /// Studio Mode's second picture, what's on air, over its own box (or none).
    async fn program_view(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    /// The vertical picture over its own box (or none).
    async fn vertical_view(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    /// The settings a new source of `kind` starts with.
    async fn defaults(&self, kind: String) -> Result<Value, String>;
    /// Does one thing to the scene collection (checked before it gets here) and answers what
    /// it shows; changes are saved by the host.
    async fn act(&self, act: StudioAct) -> Result<Value, String>;
    /// The Studio's settings (saved by the host; defaults the first time).
    async fn settings(&self) -> StudioSettings;
    /// Saves and applies them (checked before they get here) → what applies only after
    /// Ferrocast restarts, in words, if anything.
    async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String>;
    /// `{ engine: { cpu, fps, frame_ms, lagged, frames, memory_mb }, canvas, stream, record }`.
    async fn stats(&self) -> Result<Value, String>;
    /// Starts or stops recording → the recording's stats (null when stopped) and its file.
    async fn record(&self, on: bool) -> Result<Value, String>;
    async fn record_pause(&self, on: bool) -> Result<(), String>;
    /// Starts or stops the replay buffer → `{ active }`.
    async fn replay(&self, _on: bool) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Saves the replay buffer's last seconds to a file → `{ file }` (null until it's written).
    async fn replay_save(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Starts the virtual camera showing what's on air (`source` None) or one scene or
    /// source, or stops it → `{ active }`.
    async fn virtual_camera(&self, _on: bool, _source: Option<String>) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Starts a new file of the recording now (OBS's Split recording).
    async fn record_split(&self) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Marks a chapter in the recording (MP4 and MOV), named or numbered.
    async fn record_chapter(&self, _name: String) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Remuxes a recording (MKV, FLV, TS) to MP4 beside it, in the background → `{ to }`;
    /// how it goes is in stats as `remux`.
    async fn remux(&self, _file: String) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Looks for the missing files by name under `folder` → `{ found: { path: new path } }`.
    async fn find_missing(&self, _folder: String) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// An encoder's own options for `settings` → `{ options, values }` (values with defaults).
    async fn encoder_options(&self, _id: String, _settings: serde_json::Map<String, Value>) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// `{ devices: { name, id }[], current: { name, id } }` for "Hear it".
    async fn monitoring(&self) -> Result<Value, String>;
    /// Streams the program over RTMP (Ferrocast's own Go live ingest) → stats.
    async fn stream_start(&self, server: String, key: String) -> Result<Value, String>;
    async fn stream_stop(&self) -> Result<(), String>;
    /// `ferrocast_studio::StreamStats` plus `error`, or null when not streaming.
    async fn stream_stats(&self) -> Result<Value, String>;
    /// Also sends the Studio's stream straight to an SRT, RIST or WebRTC (WHIP) server. Fails
    /// when the Studio isn't streaming.
    async fn direct_start(&self, _id: String, _url: String, _key: String) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    async fn direct_stop(&self, _id: String) -> Result<(), String> {
        Ok(())
    }
    /// One direct destination's `ferrocast_studio::StreamStats`, or null.
    async fn direct_stats(&self, _id: String) -> Result<Value, String> {
        Ok(Value::Null)
    }
    /// OBS's scene collections on this computer: `{ id, name, scenes, sources }[]`.
    async fn obs_collections(&self) -> Result<Value, String>;
    /// Replaces the Studio's scenes with OBS's collection `id` (listed by `obs_collections`;
    /// the old scenes are kept in a file first) → what came in.
    async fn import_obs(&self, id: String) -> Result<Value, String>;
    /// Streamlabs Desktop's scene collections on this computer, like `obs_collections`.
    async fn streamlabs_collections(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Adds Streamlabs' collection `id` as a new scene collection (turned into OBS's format,
    /// then imported as OBS's) → what came in.
    async fn import_streamlabs(&self, _id: String) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// A file dialog on this computer → the path picked (a list for "files"), or null when
    /// it's cancelled.
    async fn pick_file(&self, _pick: FilePick) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// The app's log, newest line last: up to `lines` of them (OBS's View Current Log) →
    /// `{ file, lines }`. The engine takes secrets out before anyone sees them.
    async fn log(&self, _lines: usize) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Shows the log file in Finder or Explorer (OBS's Show Log Files).
    async fn show_log(&self) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Opens a web page in a window of its own (a web panel whose site won't be shown inside
    /// another page); the same page again brings its window forward.
    async fn web_window(&self, _name: String, _url: String) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// macOS's Screen Recording switch for Ferrocast: `{ granted }` (true where there's none).
    async fn screen_access(&self) -> Result<Value, String> {
        Ok(serde_json::json!({ "granted": true }))
    }
    /// Asks macOS again, forgetting what it said to an older build of Ferrocast (each unsigned
    /// update is a new app to macOS, while System Settings still shows the old one switched on).
    async fn fix_screen_access(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Quits and opens Ferrocast again (macOS applies a new permission only then).
    async fn restart(&self) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Opens a projector window (OBS's projectors) → `{ id }`.
    async fn projector(&self, _p: ProjectorReq) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Closes every projector.
    async fn close_projectors(&self) -> Result<(), String> {
        Ok(())
    }
    /// The displays a full-screen projector can go on: `{ index, name, width, height, main }[]`.
    async fn screens(&self) -> Result<Value, String> {
        Ok(serde_json::json!([]))
    }
    /// Scene collections or profiles (`kind`): list, make, switch, rename, remove, export,
    /// import.
    async fn shelf(&self, _kind: String, _act: ShelfAct) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
}

/// Something to do with the scene collections or profiles (OBS's menus of those names).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "act")]
pub enum ShelfAct {
    /// `{ current, list: { id, name }[] }`.
    List,
    /// A new one (empty, or a copy of the one in use), switched to → `{ id }`.
    New {
        name: String,
        #[serde(default)]
        copy: bool,
    },
    Switch {
        id: String,
    },
    Rename {
        id: String,
        name: String,
    },
    /// Not the one in use (switch first), nor the last.
    Remove {
        id: String,
    },
    /// The file as OBS writes it (a collection) or the settings (a profile).
    Export {
        id: String,
    },
    /// A collection file (OBS's or Ferrocast's) as a new collection, switched to → what came in.
    Import {
        name: String,
        doc: Value,
    },
}

impl ShelfAct {
    pub fn check(&self, kind: &str) -> Result<(), EngineError> {
        if !matches!(kind, "collections" | "profiles") {
            return Err(EngineError::Invalid("kind: collections or profiles.".into()));
        }
        let name = |n: &str| {
            let t = n.trim();
            if t.is_empty() || t.chars().count() > 100 || t.chars().any(char::is_control) {
                return Err(EngineError::Invalid("name: 1 to 100 characters.".into()));
            }
            Ok(())
        };
        let id = |i: &str| {
            if i.is_empty() || i.len() > 64 || !i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(EngineError::Invalid("id: one from the list.".into()));
            }
            Ok(())
        };
        match self {
            ShelfAct::List => Ok(()),
            ShelfAct::New { name: n, .. } => name(n),
            ShelfAct::Switch { id: i } | ShelfAct::Remove { id: i } | ShelfAct::Export { id: i } => id(i),
            ShelfAct::Rename { id: i, name: n } => {
                id(i)?;
                name(n)
            }
            ShelfAct::Import { name: n, doc } => {
                if kind != "collections" {
                    return Err(EngineError::Invalid("Only scene collections can be imported.".into()));
                }
                name(n)?;
                if !doc.get("sources").is_some_and(Value::is_array) {
                    return Err(EngineError::Invalid(
                        "doc: a scene collection file (OBS's, with a sources list).".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// A projector to open: what's on air, one scene or source, or every scene (a multiview).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProjectorReq {
    /// "program", "source" or "multiview".
    pub what: String,
    /// The scene or source, for "source".
    #[serde(default)]
    pub name: Option<String>,
    /// Full screen on this display (from the screens list), or a window when absent.
    #[serde(default)]
    pub screen: Option<u32>,
    /// A window kept above other apps' windows (OBS's "Always on top").
    #[serde(default)]
    pub on_top: bool,
}

impl ProjectorReq {
    pub fn validate(&self) -> Result<(), EngineError> {
        match (self.what.as_str(), &self.name) {
            ("source", Some(n)) => check_source_name(n)?,
            ("source", None) => return Err(EngineError::Invalid("name: the scene or source to show.".into())),
            ("program" | "multiview", None) => {}
            ("program" | "multiview", Some(_)) => {
                return Err(EngineError::Invalid("name: only for what = source.".into()));
            }
            _ => return Err(EngineError::Invalid("what: program, source or multiview.".into())),
        }
        if self.screen.is_some_and(|s| s > 32) {
            return Err(EngineError::Invalid("screen: one from the list of displays.".into()));
        }
        Ok(())
    }
}

/// Everything OBS's Edit Transform dialog shows for an item (mirrors
/// `ferrocast_studio::Transform`; `width` and `height` are the source's own size, only read).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ItemTransform {
    pub x: f32,
    pub y: f32,
    pub rotation: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub alignment: u32,
    pub bounds: String,
    pub bounds_alignment: u32,
    pub bounds_width: f32,
    pub bounds_height: f32,
    pub crop_left: i32,
    pub crop_top: i32,
    pub crop_right: i32,
    pub crop_bottom: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// The transitions the Studio offers (OBS's own kinds).
pub const TRANSITION_KINDS: [&str; 7] = [
    "fade_transition",
    "cut_transition",
    "swipe_transition",
    "slide_transition",
    "fade_to_color_transition",
    "wipe_transition",
    "obs_stinger_transition",
];

/// The kinds of bounding box, as OBS orders them.
pub const BOUNDS_KINDS: [&str; 7] = ["none", "stretch", "fit", "fill", "width", "height", "max"];

impl ItemTransform {
    pub fn validate(&self) -> Result<(), EngineError> {
        let bad = |m: &str| Err(EngineError::Invalid(m.into()));
        let far = |v: f32| !v.is_finite() || v.abs() > 100_000.0;
        if far(self.x) || far(self.y) {
            return bad("x, y: numbers from -100000 to 100000.");
        }
        if !self.rotation.is_finite() {
            return bad("rotation: a number of degrees.");
        }
        let scale = |v: f32| v.is_finite() && v.abs() >= 0.0001 && v.abs() <= 1000.0;
        if !scale(self.scale_x) || !scale(self.scale_y) {
            return bad("scale_x, scale_y: from 0.0001 to 1000, either sign (minus flips).");
        }
        // Left and right (or top and bottom) at once isn't a point.
        let point = |a: u32| a <= 15 && a & 3 != 3 && a & 12 != 12;
        if !point(self.alignment) || !point(self.bounds_alignment) {
            return bad("alignment, bounds_alignment: OBS's bits (0 middle; 1 left, 2 right, 4 top, 8 bottom).");
        }
        if !BOUNDS_KINDS.contains(&self.bounds.as_str()) {
            return bad("bounds: none, stretch, fit, fill, width, height or max.");
        }
        let size = |v: f32| v.is_finite() && (0.0..=100_000.0).contains(&v);
        if !size(self.bounds_width) || !size(self.bounds_height) {
            return bad("bounds_width, bounds_height: from 0 to 100000.");
        }
        if [self.crop_left, self.crop_top, self.crop_right, self.crop_bottom].iter().any(|c| !(0..=100_000).contains(c))
        {
            return bad("crop_left, crop_top, crop_right, crop_bottom: from 0 to 100000 pixels.");
        }
        Ok(())
    }
}

/// Hosts say this for what only the Ferrocast app on this computer can do.
pub const NOT_HERE: &str = "That needs the Ferrocast app on this computer.";

/// What a file dialog asks for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FilePick {
    /// "file" (open one), "files" (open several), "save" (a new file's name) or "folder".
    pub kind: String,
    /// File types as OBS writes them: `Image Files (*.png *.jpg);;All Files (*.*)`.
    #[serde(default)]
    pub filter: String,
}

impl FilePick {
    pub fn validate(&self) -> Result<(), EngineError> {
        if !matches!(self.kind.as_str(), "file" | "files" | "save" | "folder") {
            return Err(EngineError::Invalid("kind: file, files, save or folder.".into()));
        }
        if self.filter.len() > 2000 {
            return Err(EngineError::Invalid("filter: at most 2000 characters.".into()));
        }
        Ok(())
    }

    /// The filter as (name, extensions) pairs; "*.*" (any file) and bad patterns are left out.
    pub fn filters(&self) -> Vec<(String, Vec<String>)> {
        self.filter
            .split(";;")
            .filter_map(|part| {
                let (name, rest) = part.split_once('(')?;
                let exts: Vec<String> = rest
                    .trim_end_matches(')')
                    .split_whitespace()
                    .filter_map(|p| p.strip_prefix("*."))
                    .filter(|e| *e != "*" && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
                    .map(str::to_ascii_lowercase)
                    .collect();
                (!exts.is_empty()).then(|| (name.trim().to_string(), exts))
            })
            .collect()
    }
}

/// Shortcut actions with no name in them (OBS's hotkeys). Others: scene:, mute:,
/// push_to_talk:, push_to_mute:, screenshot:, media_*: <name>; show:, hide: <scene>\n<source>;
/// filter: <source>\n<filter>; quick: <1 to 10>.
pub const HOTKEY_ACTIONS: [&str; 16] = [
    "go_live",
    "end_stream",
    "record",
    "pause_record",
    "split_record",
    "chapter",
    "replay",
    "save_replay",
    "virtualcam",
    "studio_mode",
    "transition",
    "swap_scenes",
    "fade_to_black",
    "screenshot",
    "undo",
    "redo",
];

/// Recording formats: crash-safe MP4 and MOV, fragmented ones, and MKV, FLV and TS.
pub const RECORD_FORMATS: [&str; 8] = ["mp4", "mov", "fmp4", "fmov", "mkv", "flv", "ts", "hls"];

/// How the Studio streams, records and runs. Every field has a default, so an older or
/// partial file still reads.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct StudioSettings {
    /// "hardware" (Apple VideoToolbox, NVENC…) or "software" (x264).
    pub encoder: String,
    pub video_kbps: u32,
    /// Varying bitrate instead of steady (CBR is what platforms ask for).
    pub vbr: bool,
    /// Seconds between keyframes; 0 lets the encoder pick.
    pub keyframe_s: u32,
    /// "high", "main" or "baseline".
    pub profile: String,
    pub bframes: bool,
    /// Send at this size instead of the canvas's.
    pub scale: Option<[u32; 2]>,
    pub audio_kbps: u32,
    /// The canvas: what's composed.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Where recordings go (absolute); None is the Movies folder's Ferrocast folder.
    pub record_folder: Option<String>,
    /// "mp4" or "mov" (both crash-safe).
    pub record_format: String,
    /// "stream" (same as the stream), "high" (near-lossless), "lossless".
    pub record_quality: String,
    pub reconnect: bool,
    pub reconnect_every_s: u32,
    pub reconnect_tries: u32,
    /// Viewers see everything this many seconds later (0: off).
    pub delay_s: u32,
    /// 44100 or 48000 (applies after a restart).
    pub sample_rate: u32,
    pub stereo: bool,
    /// The device "Hear it" plays on: (name, id); None is the system's.
    pub monitoring: Option<(String, String)>,
    /// What each shortcut does: action → keys ("CmdOrCtrl+Shift+L"). Actions: go_live,
    /// end_stream, record, pause_record, scene:<name>, mute:<sound>, push_to_talk:<sound>.
    pub hotkeys: std::collections::BTreeMap<String, String>,
    /// A particular video encoder (an id from the Studio's encoders, e.g. HEVC or AV1) instead
    /// of `encoder`'s pick.
    pub video_encoder: Option<String>,
    /// That encoder's own options (OBS's keys: preset, tune, x264opts, lookahead…), applied
    /// over the ones above, as OBS's Advanced output mode.
    pub encoder_settings: serde_json::Map<String, Value>,
    /// The recording's encoder when `record_quality` is "custom", and its options.
    pub record_encoder: Option<String>,
    pub record_encoder_settings: serde_json::Map<String, Value>,
    /// OBS's file name pattern (`%CCYY-%MM-%DD %hh-%mm-%ss`…).
    pub record_name: String,
    /// Spaces allowed in file names (else underscores).
    pub record_spaces: bool,
    /// A file of the same name is replaced (else " (2)" is added).
    pub record_overwrite: bool,
    /// A new file every this many minutes or megabytes (0: never).
    pub record_split_minutes: u32,
    pub record_split_mb: u32,
    /// Audio tracks (1 to 6) the recording keeps.
    pub record_tracks: Vec<u8>,
    /// An MKV, FLV or TS recording becomes an MP4 when it ends (the original is kept).
    pub record_remux: bool,
    /// ffmpeg muxer settings of the streamer's own for recordings (OBS's Custom Muxer Settings).
    pub record_muxer: String,
    /// The replay buffer keeps the last this many seconds (5 to 21600), at most this many
    /// megabytes (0: no limit), saved as `replay_format` ("mp4", "mkv", "mov", "flv", "ts").
    pub replay_seconds: u32,
    pub replay_mb: u32,
    pub replay_format: String,
    /// A surround layout instead of `stereo` ("2.1", "4.0", "4.1", "5.1", "7.1"; applies after
    /// a restart); None follows `stereo`.
    pub speakers: Option<String>,
    /// Meters read true peak instead of sample peak (OBS's Peak Meter Type).
    pub true_peak: bool,
    /// OBS's Low Latency Audio Buffering (after a restart).
    pub low_latency_audio: bool,
    /// Multiviews: what's on air "top", "bottom", "left" or "right" of the scenes, or
    /// "scenes_only"; names under each box; EBU R95 safe areas.
    pub multiview_layout: String,
    pub multiview_labels: bool,
    pub multiview_safe_areas: bool,
    /// Moving sources in the preview snaps (OBS's Snapping): within `snap_px` screen pixels,
    /// to the screen's edges, other sources, and the screen's middle.
    pub snapping: bool,
    pub snap_px: u32,
    pub snap_screen: bool,
    pub snap_sources: bool,
    pub snap_center: bool,
    /// Asked "are you sure?" before going live, ending the stream, stopping a recording.
    pub confirm_go_live: bool,
    pub confirm_end_stream: bool,
    pub confirm_stop_record: bool,
    /// OBS's General › Output: recording and the replay buffer start with the stream, and stop
    /// with it unless kept.
    pub record_when_streaming: bool,
    pub keep_recording: bool,
    pub replay_when_streaming: bool,
    pub keep_replay: bool,
}

impl Default for StudioSettings {
    fn default() -> Self {
        StudioSettings {
            encoder: "hardware".into(),
            video_kbps: 6000,
            vbr: false,
            keyframe_s: 2,
            profile: "high".into(),
            bframes: true,
            scale: None,
            audio_kbps: 160,
            width: 1920,
            height: 1080,
            fps: 30,
            record_folder: None,
            record_format: "mp4".into(),
            record_quality: "stream".into(),
            reconnect: true,
            reconnect_every_s: 2,
            reconnect_tries: 25,
            delay_s: 0,
            sample_rate: 48000,
            stereo: true,
            monitoring: None,
            hotkeys: Default::default(),
            video_encoder: None,
            encoder_settings: Default::default(),
            record_encoder: None,
            record_encoder_settings: Default::default(),
            record_name: "%CCYY-%MM-%DD %hh-%mm-%ss".into(),
            record_spaces: true,
            record_overwrite: false,
            record_split_minutes: 0,
            record_split_mb: 0,
            record_tracks: vec![1],
            record_remux: true,
            record_muxer: String::new(),
            replay_seconds: 30,
            replay_mb: 0,
            replay_format: "mp4".into(),
            speakers: None,
            true_peak: false,
            low_latency_audio: false,
            multiview_layout: "top".into(),
            multiview_labels: true,
            multiview_safe_areas: false,
            // OBS's defaults.
            snapping: true,
            snap_px: 10,
            snap_screen: true,
            snap_sources: false,
            snap_center: false,
            confirm_go_live: false,
            confirm_end_stream: true,
            confirm_stop_record: false,
            record_when_streaming: false,
            keep_recording: false,
            replay_when_streaming: false,
            keep_replay: false,
        }
    }
}

impl StudioSettings {
    /// Refuses a value outside what OBS takes, naming the field and what's allowed.
    pub fn check(&self) -> Result<(), EngineError> {
        let bad = |m: &str| Err(EngineError::Invalid(m.into()));
        if !["hardware", "software"].contains(&self.encoder.as_str()) {
            return bad("encoder: hardware or software.");
        }
        if !(500..=60_000).contains(&self.video_kbps) {
            return bad("video_kbps: 500 to 60000.");
        }
        if self.keyframe_s > 10 {
            return bad("keyframe_s: 0 (auto) to 10.");
        }
        if !["high", "main", "baseline"].contains(&self.profile.as_str()) {
            return bad("profile: high, main or baseline.");
        }
        if let Some([w, h]) = self.scale
            && !((128..=7680).contains(&w) && (72..=4320).contains(&h) && w.is_multiple_of(2) && h.is_multiple_of(2))
        {
            return bad("scale: even sizes from 128x72 to 7680x4320.");
        }
        if ![64, 96, 128, 160, 192, 256, 320].contains(&self.audio_kbps) {
            return bad("audio_kbps: 64, 96, 128, 160, 192, 256 or 320.");
        }
        if !((320..=7680).contains(&self.width) && (240..=4320).contains(&self.height))
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return bad("width, height: even sizes from 320x240 to 7680x4320.");
        }
        if ![24, 25, 30, 48, 50, 60, 120].contains(&self.fps) {
            return bad("fps: 24, 25, 30, 48, 50, 60 or 120.");
        }
        if let Some(f) = &self.record_folder
            && (f.len() > 1024 || !std::path::Path::new(f).is_absolute())
        {
            return bad("record_folder: a full folder path.");
        }
        if !RECORD_FORMATS.contains(&self.record_format.as_str()) {
            return bad("record_format: mp4, mov, fmp4, fmov, mkv, flv, ts or hls.");
        }
        if !["stream", "high", "lossless", "custom"].contains(&self.record_quality.as_str()) {
            return bad("record_quality: stream, high, lossless or custom.");
        }
        for (field, id) in [("video_encoder", &self.video_encoder), ("record_encoder", &self.record_encoder)] {
            if let Some(id) = id
                && (id.is_empty()
                    || id.len() > 128
                    || !id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
            {
                return Err(EngineError::Invalid(format!("{field}: an encoder id from the Studio's list.")));
            }
        }
        for (field, map) in
            [("encoder_settings", &self.encoder_settings), ("record_encoder_settings", &self.record_encoder_settings)]
        {
            if map.len() > 64 || serde_json::to_string(map).map_or(true, |j| j.len() > 16_384) {
                return Err(EngineError::Invalid(format!("{field}: up to 64 of the encoder's own options.")));
            }
        }
        let name = self.record_name.trim();
        if name.is_empty() || name.chars().count() > 120 || name.chars().any(|c| c.is_control() || "/\\:".contains(c)) {
            return bad("record_name: a file name pattern without / \\ or :, up to 120 characters.");
        }
        if self.record_muxer.len() > 512 || self.record_muxer.chars().any(char::is_control) {
            return bad("record_muxer: up to 512 characters of ffmpeg's key=value settings.");
        }
        if self.record_split_minutes > 24 * 60 || self.record_split_mb > 1_000_000 {
            return bad("record_split: up to 1440 minutes or 1000000 MB (0: never).");
        }
        if !(5..=21_600).contains(&self.replay_seconds) || self.replay_mb > 1_000_000 {
            return bad("replay_seconds: 5 to 21600 (replay_mb: 0 to 1000000).");
        }
        if let Some(l) = &self.speakers
            && !["2.1", "4.0", "4.1", "5.1", "7.1"].contains(&l.as_str())
        {
            return bad("speakers: 2.1, 4.0, 4.1, 5.1 or 7.1 (or null for stereo or mono).");
        }
        if !(1..=50).contains(&self.snap_px) {
            return bad("snap_px: 1 to 50 screen pixels.");
        }
        if !["top", "bottom", "left", "right", "scenes_only"].contains(&self.multiview_layout.as_str()) {
            return bad("multiview_layout: top, bottom, left, right or scenes_only.");
        }
        if !["mp4", "mkv", "mov", "flv", "ts"].contains(&self.replay_format.as_str()) {
            return bad("replay_format: mp4, mkv, mov, flv or ts.");
        }
        if self.record_tracks.is_empty()
            || self.record_tracks.len() > 6
            || self.record_tracks.iter().any(|t| !(1..=6).contains(t))
        {
            return bad("record_tracks: 1 to 6 track numbers, 1 to 6.");
        }
        if !(1..=60).contains(&self.reconnect_every_s) || !(1..=10_000).contains(&self.reconnect_tries) {
            return bad("reconnect: every 1 to 60 s, 1 to 10000 tries.");
        }
        if self.delay_s > 1800 {
            return bad("delay_s: 0 to 1800.");
        }
        if ![44_100, 48_000].contains(&self.sample_rate) {
            return bad("sample_rate: 44100 or 48000.");
        }
        if let Some((n, i)) = &self.monitoring
            && (n.len() > 256 || i.len() > 512)
        {
            return bad("monitoring: a device from the list.");
        }
        if self.hotkeys.len() > 200 {
            return bad("hotkeys: up to 200.");
        }
        for (action, keys) in &self.hotkeys {
            let known = HOTKEY_ACTIONS.contains(&action.as_str())
                || ["scene:", "mute:", "push_to_talk:", "push_to_mute:", "media_play_pause:", "media_restart:", "media_stop:", "media_next:", "media_previous:", "screenshot:"]
                    .iter()
                    .any(|p| action.strip_prefix(p).is_some_and(|n| check_source_name(n).is_ok()))
                // A source in a scene, a filter on a source: two names, a line apart.
                || ["show:", "hide:", "filter:"].iter().any(|p| {
                    action.strip_prefix(p).and_then(|r| r.split_once('\n')).is_some_and(|(x, y)| {
                        check_source_name(x).is_ok() && check_source_name(y).is_ok()
                    })
                })
                || action.strip_prefix("quick:").and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=10).contains(&n));
            if !known {
                return bad("hotkeys: one of the Studio's shortcut actions (see API.md).");
            }
            if keys.is_empty()
                || keys.len() > 64
                // Named keys (Space, Comma, F9…) joined with +: nothing else.
                || !keys.chars().all(|c| c.is_ascii_alphanumeric() || c == '+')
            {
                return bad("hotkeys: keys like CmdOrCtrl+Shift+L.");
            }
        }
        Ok(())
    }
}

/// A change to one source in a scene (`POST …/items/{id}/edit` `{ "do": "lock" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemEdit {
    Show,
    Hide,
    Lock,
    Unlock,
    Fit,
    Remove,
    Up,
    Down,
    Top,
    Bottom,
    Stretch,
    Center,
    Reset,
    FlipH,
    FlipV,
    RotateCw,
    RotateCcw,
    Rotate180,
    Duplicate,
    BlendNormal,
    BlendAdditive,
    BlendSubtract,
    BlendScreen,
    BlendMultiply,
    BlendLighten,
    BlendDarken,
    BlendMethodDefault,
    BlendMethodSrgbOff,
    ScaleOff,
    ScalePoint,
    ScaleBicubic,
    ScaleBilinear,
    ScaleLanczos,
    ScaleArea,
}

/// A change to a filter (`POST …/filters/{filter}/edit` `{ "do": "disable" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterEdit {
    Enable,
    Disable,
    Up,
    Down,
    Top,
    Bottom,
}

/// A transition: its kind and how long it takes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SceneTransition {
    pub kind: String,
    pub ms: u32,
}

/// Something to do with a Web page source (`POST …/sources/{name}/web` `{ "do": "reload" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAct {
    Interact,
    Stop,
    Reload,
}

/// What to do to a video, playlist or slideshow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAct {
    Play,
    Pause,
    PlayPause,
    Restart,
    Stop,
    Next,
    Previous,
}

/// OBS's deinterlacing modes.
pub const DEINTERLACE_MODES: [&str; 9] =
    ["off", "discard", "retro", "blend", "blend_2x", "linear", "linear_2x", "yadif", "yadif_2x"];

/// Studio Mode's options, as OBS has them under the Transition button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct StudioModeOptions {
    /// After a transition, the preview shows what was on air.
    pub swap: bool,
    /// What goes on air is a copy: moving or hiding sources in the preview doesn't touch it.
    pub duplicate_scene: bool,
    /// The copy has its own copies of the sources too.
    pub duplicate_sources: bool,
    /// A double-click on a scene sends it on air.
    pub double_click: bool,
}

/// What to change in a sound's mixer settings (anything left out stays).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct AudioChange {
    pub volume_db: Option<f32>,
    pub muted: Option<bool>,
    pub monitor: Option<Monitor>,
    /// Left to right, 0 to 1.
    pub balance: Option<f32>,
    /// Heard this much later (negative: earlier), -950 to 20000 ms.
    pub sync_ms: Option<i64>,
    pub mono: Option<bool>,
    /// The audio tracks (1 to 6) it's mixed into.
    pub tracks: Option<Vec<u8>>,
    /// Sent by a shortcut (mute, push-to-talk): not an undo step, as in OBS. Never from the API.
    #[serde(skip)]
    pub by_shortcut: bool,
}

/// Who hears a sound besides the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Monitor {
    Off,
    OnlyMe,
    MeAndStream,
}

/// A new source: its kind, name and OBS settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct NewSource {
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub settings: Value,
}

/// One thing to do to the scene collection. The routes build these; [`StudioAct::check`]
/// refuses bad input before the host sees it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "act")]
pub enum StudioAct {
    /// `{ scenes, current, transition, vertical }` (`vertical` null while it's off).
    Scenes,
    AddScene {
        name: String,
        /// In the vertical picture.
        #[serde(default)]
        vertical: bool,
    },
    /// Turns the vertical picture on, with a first scene (nothing happens when it's on).
    StartVertical {
        scene: String,
    },
    /// Turns the vertical picture off: its scenes go; sources stay where else they're used.
    StopVertical,
    DuplicateScene {
        scene: String,
        name: String,
    },
    RemoveScene {
        scene: String,
    },
    MoveScene {
        scene: String,
        to: usize,
    },
    /// Puts a scene on air through the transition.
    SetCurrent {
        scene: String,
    },
    SetTransition {
        kind: String,
        ms: u32,
    },
    /// A scene's own transition (OBS's Transition Override), or none to use the Studio's.
    SetSceneTransition {
        scene: String,
        transition: Option<SceneTransition>,
    },
    /// A scene's sources, top first.
    Items {
        scene: String,
    },
    AddSource {
        scene: String,
        source: NewSource,
        fit: bool,
    },
    /// Adds a source from another scene (the same source, shown in both).
    AddExisting {
        scene: String,
        name: String,
    },
    EditItem {
        scene: String,
        id: i64,
        edit: ItemEdit,
    },
    /// Every source, with the scenes it's in.
    Sources,
    Rename {
        name: String,
        new_name: String,
    },
    Settings {
        name: String,
    },
    Options {
        name: String,
    },
    Update {
        name: String,
        settings: Value,
    },
    /// The mixer for a scene: the global devices, then its own sounds.
    Audio {
        scene: String,
    },
    SetAudio {
        name: String,
        change: AudioChange,
    },
    /// A global device on channel 1 to 6 (1, 2 desktop sound; 3 to 6 mics), or none.
    SetChannel {
        channel: u32,
        device: Option<NewSource>,
    },
    /// Levels now, dBFS, for these sounds.
    Levels {
        names: Vec<String>,
    },
    Filters {
        name: String,
    },
    AddFilter {
        name: String,
        filter: NewSource,
    },
    RemoveFilter {
        name: String,
        filter: String,
    },
    EditFilter {
        name: String,
        filter: String,
        edit: FilterEdit,
    },
    FilterSettings {
        name: String,
        filter: String,
    },
    FilterOptions {
        name: String,
        filter: String,
    },
    UpdateFilter {
        name: String,
        filter: String,
        settings: Value,
    },
    /// A PNG of a scene or source, saved where recordings go → `{ file }`.
    Screenshot {
        name: String,
    },
    /// A PNG of exactly what's on air (black when faded, mid-transition as it is) → `{ file }`.
    ScreenshotProgram,
    /// Clicks into, stops clicking or reloads a Web page source.
    Web {
        name: String,
        what: WebAct,
    },
    /// Copies every filter of `from` onto `name` (OBS's Copy / Paste Filters).
    PasteFilters {
        name: String,
        from: String,
    },
    /// Moves and sizes an item from the preview: its box's first corner to (`x`, `y`), its
    /// sides `w` and `h` long, in canvas pixels → the item. `save` at the end of a drag.
    SetItemBox {
        scene: String,
        id: i64,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        save: bool,
    },
    /// Crops an item from the preview (Alt-drag on a handle): left, top, right, bottom in the
    /// source's own pixels, keeping what's left where it is on screen → the item. `save` at the
    /// end of a drag.
    SetItemCrop {
        scene: String,
        id: i64,
        crop: [i32; 4],
        #[serde(default)]
        save: bool,
    },
    /// An item's transform, as OBS's Edit Transform shows it.
    Transform {
        scene: String,
        id: i64,
    },
    /// Sets all of an item's transform → the item.
    SetTransform {
        scene: String,
        id: i64,
        transform: ItemTransform,
    },
    /// A transition kind's own settings (a stinger's video, a swipe's direction).
    TransitionSettings {
        kind: String,
    },
    /// Changes the keys given of a transition kind's settings.
    SetTransitionSettings {
        kind: String,
        settings: Value,
    },
    /// Studio Mode on (its preview starts as what's on air) or off.
    StudioMode {
        on: bool,
    },
    /// The scene Studio Mode's preview shows, to edit before it goes on air.
    SetPreview {
        scene: String,
    },
    /// Sends the preview on air: with a quick transition, or as a click on the scene would.
    TransitionNow {
        #[serde(default)]
        quick: Option<SceneTransition>,
    },
    /// OBS's quick transitions (up to 10).
    SetQuickTransitions {
        list: Vec<SceneTransition>,
    },
    /// How an item appears when shown (`show`) or goes when hidden, or at once (None).
    SetItemTransition {
        scene: String,
        id: i64,
        show: bool,
        transition: Option<SceneTransition>,
    },
    /// How video from a device or file is deinterlaced: off, discard, retro, blend, blend_2x,
    /// linear, linear_2x, yadif or yadif_2x.
    SetDeinterlace {
        name: String,
        mode: String,
        top_field_first: bool,
    },
    /// A new, empty group on top of a scene → the item.
    AddGroup {
        scene: String,
        name: String,
    },
    /// A small picture of a scene now, `width` pixels wide → `{ image }` (a PNG data URL).
    Thumbnail {
        scene: String,
        width: u32,
    },
    /// Puts these sources of a scene into a new group, each where it is on screen (OBS's
    /// Group Selected Items) → the group.
    GroupItems {
        scene: String,
        ids: Vec<i64>,
        name: String,
    },
    /// Moves an item to place `to` in its list (0 is the top: on top of everything).
    ItemOrder {
        scene: String,
        id: i64,
        to: usize,
    },
    /// Takes a group apart (its sources stay, in the scene).
    Ungroup {
        scene: String,
        id: i64,
    },
    /// Moves an item into a group of the scene, or out of its group (`group` None).
    SetGroup {
        scene: String,
        id: i64,
        group: Option<i64>,
    },
    /// A new, independent copy of a source on top of a scene (OBS's Paste (Duplicate)) → the
    /// item.
    PasteCopy {
        scene: String,
        name: String,
    },
    /// A video, playlist or slideshow's play controls → `{ state, time_ms, duration_ms }`;
    /// `what` None only reads them, `seek_ms` jumps a video.
    Media {
        name: String,
        what: Option<MediaAct>,
        #[serde(default)]
        seek_ms: Option<i64>,
    },
    /// Files sources, filters and transitions use that aren't on this computer →
    /// `[{ source, parent, kind, key, label, path }]`.
    MissingFiles,
    /// Points a missing file's setting at `to`, or clears it (`to` empty).
    ReplaceFile {
        source: String,
        #[serde(default)]
        parent: Option<String>,
        kind: String,
        key: String,
        path: String,
        to: String,
    },
    /// How Studio Mode sends scenes on air (swap, duplicate scene and sources, double-click).
    SetStudioOptions {
        options: StudioModeOptions,
    },
    /// Swaps the preview and what's on air, with a cut.
    SwapScenes,
    /// Fades what's on air to black, or back → `{ black }`.
    FadeToBlack,
    /// Moves the T-bar to `at` (0 on air, 1 preview); `release` when let go → `{ active, at }`.
    TBar {
        at: f32,
        #[serde(default)]
        release: bool,
    },
    /// Undoes the newest change to sources → `{ done, undo, redo }` (labels, or null).
    Undo,
    /// Does again what was undone → `{ done, undo, redo }`.
    Redo,
    /// Every step Undo and Redo can take → `{ undo: [labels], redo: [labels] }`, newest first.
    History,
    /// Outlines an item in the preview, or nothing (`scene` and `id` null); with `frames`,
    /// every source of `scene` gets an outline in its own colour too.
    Select {
        scene: Option<String>,
        id: Option<i64>,
        #[serde(default)]
        frames: bool,
        /// More sources picked with it (⌘-click, Shift-click), each outlined with handles.
        #[serde(default)]
        also: Vec<i64>,
    },
}

impl StudioAct {
    /// Refuses bad input with the field and what's allowed.
    pub fn check(&mut self) -> Result<(), EngineError> {
        use StudioAct as A;
        let new = |n: &mut NewSource| -> Result<(), EngineError> {
            check_kind(&n.kind)?;
            check_source_name(&n.name)?;
            check_settings(&n.settings)?;
            if n.settings.is_null() {
                n.settings = serde_json::json!({});
            }
            Ok(())
        };
        let object = |v: &Value| -> Result<(), EngineError> {
            if !v.is_object() {
                return Err(EngineError::Invalid("settings: an object of OBS setting keys and values.".into()));
            }
            check_settings(v)
        };
        match self {
            A::Scenes | A::Sources | A::StopVertical => Ok(()),
            A::AddScene { name, .. } | A::StartVertical { scene: name } => check_source_name(name),
            A::RemoveScene { scene }
            | A::MoveScene { scene, .. }
            | A::SetCurrent { scene }
            | A::Items { scene }
            | A::Audio { scene } => check_source_name(scene),
            A::DuplicateScene { scene, name } | A::AddExisting { scene, name } => {
                check_source_name(scene)?;
                check_source_name(name)
            }
            A::SetSceneTransition { scene, transition } => {
                check_source_name(scene)?;
                if let Some(t) = transition {
                    if !TRANSITION_KINDS.contains(&t.kind.as_str()) {
                        return Err(EngineError::Invalid(
                            "kind: fade, cut, swipe, slide, fade to colour, wipe or stinger transition.".into(),
                        ));
                    }
                    if t.ms > 20_000 {
                        return Err(EngineError::Invalid("ms: a transition lasts 0 to 20000 ms.".into()));
                    }
                }
                Ok(())
            }
            A::SetTransition { kind, ms } => {
                check_kind(kind)?;
                if *ms > 20_000 {
                    return Err(EngineError::Invalid("ms: a transition lasts 0 to 20000 ms.".into()));
                }
                Ok(())
            }
            A::AddSource { scene, source, .. } => {
                check_source_name(scene)?;
                new(source)
            }
            A::EditItem { scene, .. } => check_source_name(scene),
            A::Rename { name, new_name } => {
                check_source_name(name)?;
                check_source_name(new_name)
            }
            A::Settings { name } | A::Options { name } | A::Filters { name } => check_source_name(name),
            A::Update { name, settings } => {
                check_source_name(name)?;
                object(settings)
            }
            A::SetAudio { name, change: c } => {
                check_source_name(name)?;
                if let Some(v) = c.volume_db
                    && !(v.is_finite() && (-100.0..=26.0).contains(&v))
                {
                    return Err(EngineError::Invalid("volume_db: -100 to 26.".into()));
                }
                if let Some(b) = c.balance
                    && !(b.is_finite() && (0.0..=1.0).contains(&b))
                {
                    return Err(EngineError::Invalid("balance: 0 (left) to 1 (right).".into()));
                }
                if let Some(ms) = c.sync_ms
                    && !(-950..=20_000).contains(&ms)
                {
                    return Err(EngineError::Invalid("sync_ms: -950 to 20000 ms.".into()));
                }
                if let Some(t) = &c.tracks
                    && (t.len() > 6 || t.iter().any(|n| !(1..=6).contains(n)))
                {
                    return Err(EngineError::Invalid("tracks: numbers 1 to 6.".into()));
                }
                Ok(())
            }
            A::SetChannel { channel, device } => {
                if !(1..=6).contains(channel) {
                    return Err(EngineError::Invalid("channel: 1 to 6.".into()));
                }
                device.as_mut().map(new).transpose().map(|_| ())
            }
            A::Levels { names } => {
                if names.len() > 64 {
                    return Err(EngineError::Invalid("names: up to 64 sounds at once.".into()));
                }
                names.iter().try_for_each(|n| check_source_name(n))
            }
            A::AddFilter { name, filter } => {
                check_source_name(name)?;
                new(filter)
            }
            A::RemoveFilter { name, filter }
            | A::EditFilter { name, filter, .. }
            | A::FilterSettings { name, filter }
            | A::FilterOptions { name, filter } => {
                check_source_name(name)?;
                check_source_name(filter)
            }
            A::UpdateFilter { name, filter, settings } => {
                check_source_name(name)?;
                check_source_name(filter)?;
                object(settings)
            }
            A::Web { name, .. } | A::Screenshot { name } => check_source_name(name),
            A::PasteFilters { name, from } => {
                check_source_name(name)?;
                check_source_name(from)?;
                if name == from {
                    return Err(EngineError::Invalid("from: another source than the one pasted onto.".into()));
                }
                Ok(())
            }
            A::SetItemCrop { scene, crop, .. } => {
                check_source_name(scene)?;
                if crop.iter().any(|c| !(0..=100_000).contains(c)) {
                    return Err(EngineError::Invalid("crop: left, top, right, bottom, 0 to 100000 pixels.".into()));
                }
                Ok(())
            }
            A::SetItemBox { scene, x, y, w, h, .. } => {
                check_source_name(scene)?;
                const MAX: f32 = 100_000.0;
                if ![*x, *y, *w, *h].iter().all(|v| v.is_finite() && v.abs() <= MAX) {
                    return Err(EngineError::Invalid("x, y, w, h: numbers from -100000 to 100000.".into()));
                }
                if *w < 1.0 || *h < 1.0 {
                    return Err(EngineError::Invalid("w, h: at least 1 pixel.".into()));
                }
                Ok(())
            }
            A::Transform { scene, .. } => check_source_name(scene),
            A::Undo | A::Redo | A::History | A::StudioMode { .. } | A::SetStudioOptions { .. } => Ok(()),
            A::SwapScenes | A::FadeToBlack | A::MissingFiles | A::ScreenshotProgram => Ok(()),
            A::AddGroup { scene, name } | A::PasteCopy { scene, name } => {
                check_source_name(scene)?;
                check_source_name(name)
            }
            A::Ungroup { scene, .. } | A::SetGroup { scene, .. } => check_source_name(scene),
            A::Thumbnail { scene, width } => {
                check_source_name(scene)?;
                if !(64..=640).contains(width) {
                    return Err(EngineError::Invalid("w: 64 to 640 pixels.".into()));
                }
                Ok(())
            }
            A::GroupItems { scene, ids, name } => {
                check_source_name(scene)?;
                check_source_name(name)?;
                if ids.is_empty() || ids.len() > 500 {
                    return Err(EngineError::Invalid("ids: the sources to group, 1 to 500.".into()));
                }
                Ok(())
            }
            A::ItemOrder { scene, to, .. } => {
                check_source_name(scene)?;
                if *to > 10_000 {
                    return Err(EngineError::Invalid("to: a place in the list, 0 to 10000.".into()));
                }
                Ok(())
            }
            A::Media { name, seek_ms, .. } => {
                check_source_name(name)?;
                if seek_ms.is_some_and(|ms| !(0..=24 * 3_600_000).contains(&ms)) {
                    return Err(EngineError::Invalid("seek_ms: 0 to 24 hours.".into()));
                }
                Ok(())
            }
            A::ReplaceFile { source, parent, key, path, to, .. } => {
                if !source.is_empty() {
                    check_source_name(source)?;
                }
                if let Some(p) = parent {
                    check_source_name(p)?;
                }
                if key.is_empty() || key.len() > 128 || path.len() > 4096 {
                    return Err(EngineError::Invalid("key, path: the missing file as listed.".into()));
                }
                if !to.is_empty() && (to.len() > 4096 || !std::path::Path::new(to).is_absolute()) {
                    return Err(EngineError::Invalid("to: a whole path to a file, or empty to clear it.".into()));
                }
                Ok(())
            }
            A::SetItemTransition { scene, transition, .. } => {
                check_source_name(scene)?;
                transition.iter().try_for_each(check_transition)
            }
            A::SetDeinterlace { name, mode, .. } => {
                check_source_name(name)?;
                if !DEINTERLACE_MODES.contains(&mode.as_str()) {
                    return Err(EngineError::Invalid(format!("mode: one of {}.", DEINTERLACE_MODES.join(", "))));
                }
                Ok(())
            }
            A::TBar { at, .. } => {
                if !(at.is_finite() && (0.0..=1.0).contains(at)) {
                    return Err(EngineError::Invalid("at: 0 to 1.".into()));
                }
                Ok(())
            }
            A::SetPreview { scene } => check_source_name(scene),
            A::TransitionNow { quick } => quick.iter().try_for_each(check_transition),
            A::SetQuickTransitions { list } => {
                if list.len() > 10 {
                    return Err(EngineError::Invalid("list: up to 10 quick transitions.".into()));
                }
                list.iter().try_for_each(check_transition)
            }
            A::TransitionSettings { kind } | A::SetTransitionSettings { kind, .. }
                if !TRANSITION_KINDS.contains(&kind.as_str()) =>
            {
                Err(EngineError::Invalid("kind: one of the Studio's transitions.".into()))
            }
            A::TransitionSettings { .. } => Ok(()),
            A::SetTransitionSettings { settings, .. } => check_settings(settings),
            A::SetTransform { scene, transform, .. } => {
                check_source_name(scene)?;
                transform.validate()
            }
            A::Select { also, .. } if also.len() > 500 => {
                Err(EngineError::Invalid("also: at most 500 more sources.".into()))
            }
            A::Select { scene, id, frames, .. } => match (scene, id) {
                (Some(s), Some(_)) => check_source_name(s),
                (Some(s), None) if *frames => check_source_name(s),
                (None, None) => Ok(()),
                _ => Err(EngineError::Invalid(
                    "scene, id: both, or neither to pick nothing (a scene alone only with frames).".into(),
                )),
            },
        }
    }

    /// It changes the collection (the host saves it after).
    pub fn changes(&self) -> bool {
        use StudioAct as A;
        if let A::SetItemBox { save: last, .. } | A::SetItemCrop { save: last, .. } | A::TBar { release: last, .. } =
            self
        {
            return *last;
        }
        !matches!(
            self,
            A::Scenes
                | A::Thumbnail { .. }
                | A::Items { .. }
                | A::Sources
                | A::Settings { .. }
                | A::Options { .. }
                | A::Audio { .. }
                | A::Levels { .. }
                | A::Filters { .. }
                | A::Screenshot { .. }
                | A::ScreenshotProgram
                | A::FilterSettings { .. }
                | A::FilterOptions { .. }
                | A::Select { .. }
                | A::Transform { .. }
                | A::TransitionSettings { .. }
                | A::History
                | A::MissingFiles
                | A::Media { .. }
        )
    }
}

/// One of the Studio's transitions, 0 to 20 s long.
fn check_transition(t: &SceneTransition) -> Result<(), EngineError> {
    if !TRANSITION_KINDS.contains(&t.kind.as_str()) {
        return Err(EngineError::Invalid("kind: one of the Studio's transitions.".into()));
    }
    if t.ms > 20_000 {
        return Err(EngineError::Invalid("ms: a transition lasts 0 to 20000 ms.".into()));
    }
    Ok(())
}

/// A source or filter kind id from OBS (`color_source`, `com.example.thing`).
fn check_kind(kind: &str) -> Result<(), EngineError> {
    if kind.is_empty() || kind.len() > 128 || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(EngineError::Invalid("kind: an OBS source or filter id, like color_source.".into()));
    }
    Ok(())
}

/// A source name: 1 to 100 letters on one line.
fn check_source_name(name: &str) -> Result<(), EngineError> {
    let n = name.chars().count();
    if n == 0 || n > 100 || name.trim() != name || name.chars().any(char::is_control) {
        return Err(EngineError::Invalid("name: 1 to 100 letters on one line.".into()));
    }
    Ok(())
}

fn check_settings(v: &Value) -> Result<(), EngineError> {
    if !(v.is_object() || v.is_null()) {
        return Err(EngineError::Invalid("settings: an object of OBS setting keys and values.".into()));
    }
    if v.to_string().len() > 256 * 1024 {
        return Err(EngineError::Invalid("settings: up to 256 KB.".into()));
    }
    Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct Slot {
    host: std::sync::RwLock<Option<Arc<dyn StudioHost>>>,
}

const NONE: &str = "The Studio runs in the Ferrocast app on your computer. Open Ferrocast there.";

impl Engine {
    /// The desktop app registers itself once its window exists.
    pub fn set_studio_host(&self, host: Arc<dyn StudioHost>) {
        *self.inner.studio_host.host.write().unwrap_or_else(|p| p.into_inner()) = Some(host);
    }

    pub(crate) fn studio_host(&self) -> Result<Arc<dyn StudioHost>, EngineError> {
        self.inner.studio_host.host.read().unwrap_or_else(|p| p.into_inner()).clone().ok_or(EngineError::Refused {
            status: 422,
            code: "studio_unavailable",
            message: NONE.into(),
            hint: "The Studio needs the desktop app; this is the server or a browser.".into(),
            retry_after: None,
        })
    }

    pub async fn studio_status(&self) -> StudioStatus {
        match self.studio_host() {
            Ok(h) => h.status().await,
            Err(_) => StudioStatus { detail: Some(NONE.into()), ..Default::default() },
        }
    }

    pub async fn studio_start(&self) -> Result<StudioStatus, EngineError> {
        self.studio_host()?.start().await.map_err(failed)
    }

    pub async fn studio_stop(&self) -> Result<StudioStatus, EngineError> {
        self.studio_host()?.stop().await.map_err(failed)
    }

    pub async fn studio_kinds(&self) -> Result<Value, EngineError> {
        self.studio_host()?.kinds().await.map_err(failed)
    }

    pub async fn studio_options(&self, kind: &str) -> Result<Value, EngineError> {
        check_kind(kind)?;
        self.studio_host()?.options(kind.to_string()).await.map_err(failed)
    }

    pub async fn studio_defaults(&self, kind: &str) -> Result<Value, EngineError> {
        check_kind(kind)?;
        self.studio_host()?.defaults(kind.to_string()).await.map_err(failed)
    }

    pub async fn studio_settings(&self) -> Result<StudioSettings, EngineError> {
        Ok(self.studio_host()?.settings().await)
    }

    /// Saves and applies the Studio's settings → `{ settings, restart: string | null }`.
    pub async fn studio_set_settings(&self, s: StudioSettings) -> Result<Value, EngineError> {
        s.check()?;
        let host = self.studio_host()?;
        let restart = host.set_settings(s).await.map_err(failed)?;
        Ok(serde_json::json!({ "settings": host.settings().await, "restart": restart }))
    }

    pub async fn studio_stats(&self) -> Result<Value, EngineError> {
        self.studio_host()?.stats().await.map_err(failed)
    }

    pub async fn studio_record(&self, on: bool) -> Result<Value, EngineError> {
        self.studio_host()?.record(on).await.map_err(failed)
    }

    pub async fn studio_record_pause(&self, on: bool) -> Result<(), EngineError> {
        self.studio_host()?.record_pause(on).await.map_err(failed)
    }

    pub async fn studio_replay(&self, on: bool) -> Result<Value, EngineError> {
        self.studio_host()?.replay(on).await.map_err(failed)
    }

    pub async fn studio_replay_save(&self) -> Result<Value, EngineError> {
        self.studio_host()?.replay_save().await.map_err(failed)
    }

    pub async fn studio_virtual_camera(&self, on: bool, source: Option<String>) -> Result<Value, EngineError> {
        if let Some(s) = &source {
            check_source_name(s)?;
        }
        self.studio_host()?.virtual_camera(on, source).await.map_err(failed)
    }

    /// Searches a folder for the missing files (by file name).
    pub async fn studio_find_missing(&self, folder: String) -> Result<Value, EngineError> {
        if folder.len() > 4096 || !std::path::Path::new(&folder).is_absolute() {
            return Err(EngineError::Invalid("folder: a whole folder path.".into()));
        }
        self.studio_host()?.find_missing(folder).await.map_err(failed)
    }

    pub async fn studio_record_split(&self) -> Result<(), EngineError> {
        self.studio_host()?.record_split().await.map_err(failed)
    }

    pub async fn studio_record_chapter(&self, name: String) -> Result<(), EngineError> {
        if name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(EngineError::Invalid("name: up to 100 characters, or empty for a number.".into()));
        }
        self.studio_host()?.record_chapter(name).await.map_err(failed)
    }

    /// Remuxes a recording to MP4 beside it (OBS's Remux), in the background.
    pub async fn studio_remux(&self, file: String) -> Result<Value, EngineError> {
        let p = std::path::Path::new(&file);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if file.len() > 4096 || !p.is_absolute() || !["mkv", "flv", "ts", "mov", "mp4", "m3u8"].contains(&ext.as_str())
        {
            return Err(EngineError::Invalid("file: a whole path to an MKV, FLV, TS, MOV or MP4 recording.".into()));
        }
        self.studio_host()?.remux(file).await.map_err(failed)
    }

    pub async fn studio_encoder_options(
        &self,
        id: String,
        settings: serde_json::Map<String, Value>,
    ) -> Result<Value, EngineError> {
        if id.is_empty() || id.len() > 128 || !id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            return Err(EngineError::Invalid("id: an encoder id from the Studio's list.".into()));
        }
        if settings.len() > 64 {
            return Err(EngineError::Invalid("settings: up to 64 of the encoder's options.".into()));
        }
        self.studio_host()?.encoder_options(id, settings).await.map_err(failed)
    }

    /// OBS's scene collections, to import one.
    pub async fn studio_obs_collections(&self) -> Result<Value, EngineError> {
        self.studio_host()?.obs_collections().await.map_err(failed)
    }

    /// Streamlabs Desktop's scene collections, to import one.
    pub async fn studio_streamlabs_collections(&self) -> Result<Value, EngineError> {
        self.studio_host()?.streamlabs_collections().await.map_err(failed)
    }

    /// Brings in Streamlabs' scene collection `id` as a new scene collection.
    pub async fn studio_import_streamlabs(&self, id: &str) -> Result<Value, EngineError> {
        if !collection_id_ok(id) {
            return Err(EngineError::Invalid("id: one of Streamlabs' scene collections, as listed.".into()));
        }
        let host = self.studio_host()?;
        let list = host.streamlabs_collections().await.map_err(failed)?;
        if !list.as_array().is_some_and(|l| l.iter().any(|c| c["id"] == id)) {
            return Err(EngineError::Refused {
                status: 404,
                code: "studio_no_obs_collection",
                message: "Streamlabs has no such scene collection.".into(),
                hint: "Pick one from the list.".into(),
                retry_after: None,
            });
        }
        host.import_streamlabs(id.to_string()).await.map_err(failed)
    }

    /// Brings in OBS's scene collection `id` in place of the Studio's scenes.
    pub async fn studio_import_obs(&self, id: &str) -> Result<Value, EngineError> {
        if !collection_id_ok(id) {
            return Err(EngineError::Invalid("id: one of OBS's scene collections, as listed.".into()));
        }
        let host = self.studio_host()?;
        let list = host.obs_collections().await.map_err(failed)?;
        let known = list.as_array().is_some_and(|l| l.iter().any(|c| c["id"] == id));
        if !known {
            return Err(EngineError::Refused {
                status: 404,
                code: "studio_no_obs_collection",
                message: format!("OBS has no scene collection \"{id}\"."),
                hint: "Pick one from the list.".into(),
                retry_after: None,
            });
        }
        host.import_obs(id.to_string()).await.map_err(failed)
    }

    pub async fn studio_monitoring(&self) -> Result<Value, EngineError> {
        self.studio_host()?.monitoring().await.map_err(failed)
    }

    /// Does one thing to the scene collection, after checking it.
    pub async fn studio_act(&self, mut act: StudioAct) -> Result<Value, EngineError> {
        act.check()?;
        self.studio_host()?.act(act).await.map_err(failed)
    }

    /// Starts or stops streaming the Studio into this PC's Go live ingest, which sends it on
    /// to every destination switched on, as a stream from OBS would.
    pub async fn studio_stream(&self, on: bool) -> Result<Value, EngineError> {
        let host = self.studio_host()?;
        let s = host.settings().await;
        if !on {
            host.stream_stop().await.map_err(failed)?;
            // The stream has ended either way; what didn't stop with it comes back in words.
            return Ok(follow_stream(host.as_ref(), false, &s)
                .await
                .map_or(Value::Null, |also| serde_json::json!({ "also": also })));
        }
        let (server, key) = self.golive_ingest().await?;
        let mut v = host.stream_start(server, key).await.map_err(failed)?;
        if let (Some(also), Some(o)) = (follow_stream(host.as_ref(), true, &s).await, v.as_object_mut()) {
            o.insert("also".into(), Value::String(also));
        }
        Ok(v)
    }

    pub async fn studio_stream_stats(&self) -> Result<Value, EngineError> {
        self.studio_host()?.stream_stats().await.map_err(failed)
    }

    /// A file dialog on the computer Ferrocast runs on → the path picked, or null.
    pub async fn studio_pick_file(&self, pick: FilePick) -> Result<Value, EngineError> {
        pick.validate()?;
        self.studio_host()?.pick_file(pick).await.map_err(failed)
    }

    /// The log's last `lines` lines (1 to 5000), stream keys, tokens and passwords taken out,
    /// so it can be read on screen or pasted into a bug report.
    pub async fn studio_log(&self, lines: usize) -> Result<Value, EngineError> {
        if !(1..=5000).contains(&lines) {
            return Err(EngineError::Invalid("lines: 1 to 5000.".into()));
        }
        let mut v = self.studio_host()?.log(lines).await.map_err(failed)?;
        if let Some(list) = v.get_mut("lines").and_then(Value::as_array_mut) {
            for l in list.iter_mut() {
                if let Some(s) = l.as_str() {
                    *l = Value::String(redact_log_line(s));
                }
            }
        }
        Ok(v)
    }

    pub async fn studio_show_log(&self) -> Result<(), EngineError> {
        self.studio_host()?.show_log().await.map_err(failed)
    }

    /// A web panel's page in a window of its own, after checking it's a web page.
    pub async fn studio_web_window(&self, name: String, url: String) -> Result<(), EngineError> {
        let (name, url) = web_window_check(&name, &url)?;
        self.studio_host()?.web_window(name, url).await.map_err(failed)
    }

    pub async fn studio_screen_access(&self) -> Result<Value, EngineError> {
        self.studio_host()?.screen_access().await.map_err(failed)
    }

    pub async fn studio_fix_screen_access(&self) -> Result<Value, EngineError> {
        self.studio_host()?.fix_screen_access().await.map_err(failed)
    }

    /// Opens a projector (a window, or full screen on a display) → `{ id }`.
    pub async fn studio_projector(&self, p: ProjectorReq) -> Result<Value, EngineError> {
        p.validate()?;
        self.studio_host()?.projector(p).await.map_err(failed)
    }

    pub async fn studio_close_projectors(&self) -> Result<(), EngineError> {
        self.studio_host()?.close_projectors().await.map_err(failed)
    }

    /// Scene collections or profiles: one act on that list.
    pub async fn studio_shelf(&self, kind: &str, act: ShelfAct) -> Result<Value, EngineError> {
        act.check(kind)?;
        self.studio_host()?.shelf(kind.to_string(), act).await.map_err(failed)
    }

    pub async fn studio_screens(&self) -> Result<Value, EngineError> {
        self.studio_host()?.screens().await.map_err(failed)
    }

    /// Opens Ferrocast again; refused while the Studio streams or records.
    pub async fn studio_restart(&self) -> Result<(), EngineError> {
        self.studio_host()?.restart().await.map_err(failed)
    }

    /// The preview over the page's box, in Studio Mode what's on air over a second one, and
    /// the vertical picture over a third.
    pub async fn studio_preview(
        &self,
        rect: Option<PreviewRect>,
        program: Option<PreviewRect>,
        vertical: Option<PreviewRect>,
    ) -> Result<(), EngineError> {
        for r in rect.iter().chain(program.iter()).chain(vertical.iter()) {
            r.validate().map_err(EngineError::Invalid)?;
        }
        let host = self.studio_host()?;
        host.preview(rect).await.map_err(failed)?;
        host.program_view(program).await.map_err(failed)?;
        host.vertical_view(vertical).await.map_err(failed)
    }
}

/// Hosts say "Start the Studio first." when it's stopped; anything else is a failure.
pub const STOPPED: &str = "Start the Studio first.";

fn failed(message: String) -> EngineError {
    let refused = |status, code, message: String, hint: &str| EngineError::Refused {
        status,
        code,
        message,
        hint: hint.into(),
        retry_after: None,
    };
    if message == STOPPED {
        return refused(409, "studio_stopped", message, "Press Start in Studio, Go live.");
    }
    // OBS's errors come as sentence fragments ("there's no source…"): start them as sentences.
    let mut message = message.strip_prefix("name: ").map(str::to_string).unwrap_or(message);
    if let Some(c) = message.chars().next().filter(char::is_ascii_lowercase) {
        message.replace_range(..1, &c.to_ascii_uppercase().to_string());
    }
    if !message.ends_with('.') {
        message.push('.');
    }
    if message.starts_with("There's no source") {
        return refused(404, "studio_no_source", message, "Refresh the list of sources.");
    }
    if message.starts_with("There's no scene called") {
        return refused(404, "studio_no_scene", message, "Refresh the list of scenes.");
    }
    if message.starts_with("There's no filter called") {
        return refused(404, "studio_no_filter", message, "Refresh the list of filters.");
    }
    if message.contains("already a scene collection called") || message.contains("already a profile called") {
        return refused(409, "studio_name_taken", message, "Pick another name.");
    }
    if message.contains("in use now") {
        return refused(409, "studio_in_use", message, "Switch to another one first.");
    }
    if message.contains("There's no scene collection") || message.contains("There's no profile") {
        return refused(404, "studio_no_such", message, "Refresh the list.");
    }
    if message.contains("already a source") || message.contains("already a filter") {
        return refused(409, "studio_name_taken", message, "Pick another name.");
    }
    if message.contains("isn't here yet") {
        return refused(422, "studio_not_supported", message, "Use Reload, or OBS for now.");
    }
    if message.starts_with("The replay buffer is off") {
        return refused(
            422,
            "studio_replay_off",
            message,
            "Turn on Replay buffer in Studio, Go live (the menu beside Record).",
        );
    }
    if message.contains("display isn't there") {
        return refused(404, "studio_no_screen", message, "Pick a display from the list again.");
    }
    if message == NOT_HERE {
        return refused(
            422,
            "studio_not_here",
            message,
            "Do it in the Ferrocast app on the computer it runs on (or type the file's path).",
        );
    }
    if message.contains("isn't a web page") {
        return refused(400, "studio_not_web_page", message, "Pick a Web page source.");
    }
    if message.contains("Stop streaming and recording") {
        return refused(409, "studio_busy", message, "Stop the stream and the recording, then try again.");
    }
    if message.contains("at least one scene") {
        return refused(409, "studio_last_scene", message, "Add another scene first.");
    }
    refused(
        422,
        "studio_failed",
        message,
        "Check the settings and try again. If it keeps failing, make sure OBS 32 or newer is installed.",
    )
}

/// Where a secret value written after a marker ends.
fn value_end(s: &str) -> usize {
    s.find(|c: char| c.is_whitespace() || "&\"',;)]}>".contains(c)).unwrap_or(s.len())
}

/// A word that looks like a key: long, letters and digits mixed (stream keys, tokens).
fn looks_secret(w: &str) -> bool {
    w.len() >= 16
        && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && w.chars().any(|c| c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_alphabetic())
}

const HIDDEN: &str = "•••";

/// One log line with what could open an account taken out: stream keys, tokens, passwords,
/// and the secret parts of stream addresses. A log pasted into a bug report stays safe.
pub fn redact_log_line(line: &str) -> String {
    const MARKERS: [&str; 10] =
        ["key=", "token=", "passphrase=", "streamid=", "password=", "secret=", "pass=", "key:", "bearer ", "auth="];
    let mut out = line.to_string();
    // Values after a marker (case doesn't matter; the marker itself stays).
    for m in MARKERS {
        let mut from = 0;
        loop {
            let lower = out.to_ascii_lowercase();
            let Some(i) = lower[from..].find(m).map(|i| i + from) else { break };
            let start = i + m.len();
            let rest = &out[start..];
            let rest_trim = rest.trim_start_matches(['"', '\'', ' ']);
            let skip = rest.len() - rest_trim.len();
            let end = value_end(rest_trim);
            if end == 0 || rest_trim[..end] == *HIDDEN {
                from = start;
                continue;
            }
            out.replace_range(start + skip..start + skip + end, HIDDEN);
            from = start + skip + HIDDEN.len();
        }
    }
    // Key-like path parts of addresses (rtmp://host/app/<key>) and key-like words on their own.
    let mut words: Vec<String> = Vec::new();
    for word in out.split(' ') {
        let w = if word.contains("://") {
            let (scheme, rest) = word.split_once("://").unwrap_or((word, ""));
            let mut parts = rest.split('/');
            let host = parts.next().unwrap_or("");
            let mut path: Vec<&str> = vec![host];
            // RTMP: rtmp://host/app/<key>, so whatever follows the app is the key, however short.
            let rtmp = scheme.to_ascii_lowercase().starts_with("rtmp");
            path.extend(parts.enumerate().map(|(i, p)| {
                if (rtmp && i >= 1 && !p.is_empty()) || looks_secret(p) || p.starts_with("live_") { HIDDEN } else { p }
            }));
            format!("{scheme}://{}", path.join("/"))
        } else if looks_secret(word.trim_matches(|c: char| !c.is_ascii_alphanumeric())) || word.starts_with("live_") {
            HIDDEN.to_string()
        } else {
            word.to_string()
        };
        words.push(w);
    }
    words.join(" ")
}

/// Another app's collection id is a plain file name: no path, no leading dot.
fn collection_id_ok(id: &str) -> bool {
    id.len() <= 128
        && !id.is_empty()
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
}

/// A web window's title and address, checked: only http(s) pages, never a local file or script.
fn web_window_check(name: &str, url: &str) -> Result<(String, String), EngineError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(EngineError::Invalid("name: 1 to 100 characters.".into()));
    }
    let u = reqwest::Url::parse(url.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some() && url.len() <= 4096)
        .ok_or_else(|| EngineError::Invalid("url: a web address starting with https://.".into()))?;
    Ok((name.to_string(), u.to_string()))
}

/// What starts and stops with the stream (OBS's "Automatically record when streaming" and its
/// friends). The stream starts or ends either way, so what didn't follow comes back in words.
async fn follow_stream(host: &dyn StudioHost, on: bool, s: &StudioSettings) -> Option<String> {
    let mut failed = Vec::new();
    if on {
        if s.record_when_streaming
            && let Err(e) = host.record(true).await
        {
            failed.push(format!("The recording didn't start: {e}"));
        }
        if s.replay_when_streaming
            && let Err(e) = host.replay(true).await
        {
            failed.push(format!("The replay buffer didn't start: {e}"));
        }
    } else {
        if s.record_when_streaming
            && !s.keep_recording
            && let Err(e) = host.record(false).await
        {
            failed.push(format!("The recording didn't stop: {e}"));
        }
        if s.replay_when_streaming
            && !s.keep_replay
            && let Err(e) = host.replay(false).await
        {
            failed.push(format!("The replay buffer didn't stop: {e}"));
        }
    }
    (!failed.is_empty()).then(|| failed.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Fake {
        rects: std::sync::Mutex<Vec<Option<PreviewRect>>>,
        acts: std::sync::Mutex<Vec<StudioAct>>,
        outputs: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl StudioHost for Fake {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(StudioStatus { available: true, running: true, version: Some("32.2.2".into()), ..Default::default() })
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            Err("not running".into())
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "inputs": [] }))
        }
        async fn options(&self, kind: String) -> Result<Value, String> {
            Ok(serde_json::json!([{ "key": kind }]))
        }
        async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String> {
            self.rects.lock().unwrap_or_else(|p| p.into_inner()).push(rect);
            Ok(())
        }
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn settings(&self) -> StudioSettings {
            StudioSettings::default()
        }
        async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn record(&self, on: bool) -> Result<Value, String> {
            self.outputs.lock().unwrap_or_else(|p| p.into_inner()).push(format!("record {on}"));
            Ok(serde_json::json!({ "on": on }))
        }
        async fn replay(&self, on: bool) -> Result<Value, String> {
            if on {
                return Err("No disk space.".into());
            }
            self.outputs.lock().unwrap_or_else(|p| p.into_inner()).push(format!("replay {on}"));
            Ok(serde_json::json!({ "active": on }))
        }
        async fn record_pause(&self, _on: bool) -> Result<(), String> {
            Ok(())
        }
        async fn monitoring(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "devices": [] }))
        }
        async fn act(&self, act: StudioAct) -> Result<Value, String> {
            self.acts.lock().unwrap_or_else(|p| p.into_inner()).push(act.clone());
            Ok(serde_json::to_value(act).unwrap_or_default())
        }
        async fn stream_start(&self, server: String, key: String) -> Result<Value, String> {
            Ok(serde_json::json!({ "server": server, "key_len": key.len() }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(serde_json::json!([{ "id": "Untitled", "name": "Untitled", "scenes": 2, "sources": 3 }]))
        }
        async fn import_obs(&self, id: String) -> Result<Value, String> {
            Ok(serde_json::json!({ "name": id, "scenes": 2, "sources": 3, "web_pages": 0, "missing": [] }))
        }
    }

    #[test]
    fn vertical_acts_read_and_check() {
        let add: StudioAct = serde_json::from_value(serde_json::json!({ "act": "add_scene", "name": "Tall" })).unwrap();
        assert_eq!(add, StudioAct::AddScene { name: "Tall".into(), vertical: false });
        let add: StudioAct =
            serde_json::from_value(serde_json::json!({ "act": "add_scene", "name": "Tall", "vertical": true })).unwrap();
        assert_eq!(add, StudioAct::AddScene { name: "Tall".into(), vertical: true });
        assert!(StudioAct::StartVertical { scene: "Gameplay".into() }.check().is_ok());
        assert!(matches!(StudioAct::StartVertical { scene: "  ".into() }.check(), Err(EngineError::Invalid(_))));
        assert!(StudioAct::StopVertical.check().is_ok());
        // Turning it on or off is saved with the scenes.
        assert!(StudioAct::StartVertical { scene: "Gameplay".into() }.changes());
        assert!(StudioAct::StopVertical.changes());
    }

    #[test]
    fn logs_lose_their_secrets() {
        let cases = [
            (
                "connect rtmp://live.twitch.tv/app/live_123456789_AbCdEfGhIjKlMnOp ok",
                "connect rtmp://live.twitch.tv/app/••• ok",
            ),
            ("srt://1.2.3.4:9000?streamid=publish:abc123 latency=200", "srt://1.2.3.4:9000?streamid=••• latency=200"),
            ("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.x.y sent", "Authorization: Bearer ••• sent"),
            ("whip token=abc&x=1 key=\"zz9\"", "whip token=•••&x=1 key=\"•••\""),
            ("youtube key: abcd-efgh-ijkl-mnop-1234 used", "youtube key: ••• used"),
            ("password=hunter2", "password=•••"),
            // A short key in an RTMP address is still a key; the app part stays.
            ("to rtmp://a.example/app/abc123def456", "to rtmp://a.example/app/•••"),
            ("RTMPS://x.example:443/live/k1/extra", "RTMPS://x.example:443/live/•••/•••"),
        ];
        for (raw, want) in cases {
            assert_eq!(redact_log_line(raw), want, "{raw}");
        }
        // Everyday lines stay as they are.
        let plain = "INFO libobs: [ mac-avcapture ]: Selected device 'FaceTime HD Camera' 1920x1080 @ 30 fps";
        assert_eq!(redact_log_line(plain), plain);
        assert_eq!(redact_log_line(""), "");
        // Already hidden values aren't hidden twice, and nothing loops.
        assert_eq!(redact_log_line("key=••• key=x"), "key=••• key=•••");
    }

    #[test]
    fn web_windows_open_web_pages_only() {
        let ok = web_window_check(" Chat ", "https://www.twitch.tv/popout/x/chat").unwrap();
        assert_eq!(ok, ("Chat".to_string(), "https://www.twitch.tv/popout/x/chat".to_string()));
        assert!(web_window_check("Local tool", "http://127.0.0.1:8080/").is_ok());
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "not a url", "https://"] {
            assert!(web_window_check("x", bad).is_err(), "{bad}");
        }
        assert!(web_window_check("  ", "https://example.com").is_err());
    }

    #[tokio::test]
    async fn recording_and_the_replay_buffer_follow_the_stream_when_asked() {
        let fake = Fake::default();
        let taken = |f: &Fake| std::mem::take(&mut *f.outputs.lock().unwrap_or_else(|p| p.into_inner()));
        // Off by default: nothing else moves.
        let mut s = StudioSettings::default();
        assert_eq!(follow_stream(&fake, true, &s).await, None);
        follow_stream(&fake, false, &s).await;
        assert!(taken(&fake).is_empty());
        // Both on: a replay buffer that fails says so; both stop with the stream.
        s.record_when_streaming = true;
        s.replay_when_streaming = true;
        let also = follow_stream(&fake, true, &s).await.unwrap_or_default();
        assert!(also.contains("replay buffer didn't start") && also.contains("No disk space"), "{also}");
        follow_stream(&fake, false, &s).await;
        assert_eq!(taken(&fake), ["record true", "record false", "replay false"]);
        // Kept: they go on after the stream.
        s.keep_recording = true;
        s.keep_replay = true;
        follow_stream(&fake, false, &s).await;
        assert!(taken(&fake).is_empty());
    }

    async fn engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        (e, dir)
    }

    #[tokio::test]
    async fn without_the_desktop_app_it_says_where_it_runs() {
        let (e, _d) = engine().await;
        let s = e.studio_status().await;
        assert!(!s.available && s.detail.as_deref() == Some(NONE));
        let err = e.studio_start().await.unwrap_err();
        assert!(matches!(err, EngineError::Refused { status: 422, code: "studio_unavailable", .. }), "{err:?}");
    }

    #[tokio::test]
    async fn obs_collections_import_only_by_a_listed_id() {
        let (e, _d) = engine().await;
        e.set_studio_host(Arc::new(Fake::default()));
        assert_eq!(e.studio_obs_collections().await.unwrap()[0]["id"], "Untitled");
        assert_eq!(e.studio_import_obs("Untitled").await.unwrap()["scenes"], 2);
        let gone = e.studio_import_obs("Other").await.unwrap_err();
        assert!(matches!(gone, EngineError::Refused { status: 404, code: "studio_no_obs_collection", .. }));
        for bad in ["../Untitled", "", ".x", "a/b"] {
            assert!(matches!(e.studio_import_obs(bad).await.unwrap_err(), EngineError::Invalid(_)), "{bad}");
        }
        let mut same = StudioAct::PasteFilters { name: "Mic".into(), from: "Mic".into() };
        assert!(matches!(same.check(), Err(EngineError::Invalid(_))));
        assert!(matches!(
            failed("\"Cam\" isn't a web page.".into()),
            EngineError::Refused { status: 400, code: "studio_not_web_page", .. }
        ));
        assert!(matches!(
            failed("Stop streaming and recording before bringing in scenes.".into()),
            EngineError::Refused { status: 409, code: "studio_busy", .. }
        ));
    }

    #[tokio::test]
    async fn preview_editing_file_dialogs_and_screen_access() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        // Moving and sizing: numbers on the canvas, sizes of at least a pixel.
        let bx = |scene: &str, x: f32, y: f32, w: f32, save: bool| StudioAct::SetItemBox {
            scene: scene.into(),
            id: 3,
            x,
            y,
            w,
            h: 360.0,
            save,
        };
        assert!(!bx("Main", -20.0, 40.5, 640.0, false).changes());
        assert!(bx("Main", -20.0, 40.5, 640.0, true).changes());
        e.studio_act(bx("Main", -20.0, 40.5, 640.0, false)).await.unwrap();
        for bad in [
            bx("Main", 0.0, 0.0, 0.5, false),
            bx("Main", f32::NAN, 0.0, 10.0, false),
            bx("Main", 0.0, 1e9, 10.0, false),
            bx("", 0.0, 0.0, 10.0, false),
            StudioAct::Select { scene: Some("Main".into()), id: None, frames: false, also: vec![] },
        ] {
            assert!(matches!(e.studio_act(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }
        let pick = StudioAct::Select { scene: Some("Main".into()), id: Some(3), frames: false, also: vec![4] };
        assert!(!pick.changes());
        e.studio_act(pick).await.unwrap();
        e.studio_act(StudioAct::Select { scene: Some("Main".into()), id: None, frames: true, also: vec![] })
            .await
            .unwrap();
        e.studio_act(StudioAct::Select { scene: None, id: None, frames: false, also: vec![] }).await.unwrap();
        assert_eq!(fake.acts.lock().unwrap().len(), 4);

        // OBS's filter strings become dialog filters; "any file" is left to the dialog.
        let f = FilePick {
            kind: "file".into(),
            filter: "Image Files (*.bmp *.PNG *.jpeg);;Bad (*.x/y);;All Files (*.*)".into(),
        };
        assert_eq!(f.filters(), vec![("Image Files".to_string(), vec!["bmp".into(), "png".into(), "jpeg".into()])]);
        assert!(f.validate().is_ok());
        let bad = FilePick { kind: "run".into(), filter: String::new() };
        assert!(matches!(e.studio_pick_file(bad).await.unwrap_err(), EngineError::Invalid(_)));
        // Hosts without dialogs (or a fix for macOS) say so plainly.
        let not_here = e.studio_pick_file(f).await.unwrap_err();
        assert!(matches!(not_here, EngineError::Refused { status: 422, code: "studio_not_here", .. }));
        assert_eq!(e.studio_screen_access().await.unwrap()["granted"], true);
        assert!(matches!(e.studio_restart().await.unwrap_err(), EngineError::Refused { code: "studio_not_here", .. }));

        // Edit Transform: every number is checked before the host.
        let t = ItemTransform {
            x: 10.0,
            y: 20.0,
            rotation: 450.0,
            scale_x: -0.5,
            scale_y: 0.5,
            alignment: 5,
            bounds: "fit".into(),
            bounds_alignment: 0,
            bounds_width: 1920.0,
            bounds_height: 1080.0,
            crop_left: 0,
            crop_top: 10,
            crop_right: 0,
            crop_bottom: 0,
            width: 0,
            height: 0,
        };
        assert!(t.validate().is_ok());
        let set = |t: ItemTransform| StudioAct::SetTransform { scene: "Main".into(), id: 3, transform: t };
        assert!(set(t.clone()).changes());
        assert!(!StudioAct::Transform { scene: "Main".into(), id: 3 }.changes());
        e.studio_act(set(t.clone())).await.unwrap();
        for bad in [
            ItemTransform { scale_x: 0.0, ..t.clone() },
            ItemTransform { alignment: 3, ..t.clone() },
            ItemTransform { bounds_alignment: 12, ..t.clone() },
            ItemTransform { bounds: "huge".into(), ..t.clone() },
            ItemTransform { crop_left: -1, ..t.clone() },
            ItemTransform { x: f32::INFINITY, ..t.clone() },
            ItemTransform { rotation: f32::NAN, ..t.clone() },
            ItemTransform { bounds_width: -5.0, ..t.clone() },
        ] {
            assert!(matches!(e.studio_act(set(bad.clone())).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }

        // Transition settings: only the Studio's transitions, with an object of settings.
        let ts = |kind: &str, settings| StudioAct::SetTransitionSettings { kind: kind.into(), settings };
        e.studio_act(ts("obs_stinger_transition", serde_json::json!({ "path": "/a.webm" }))).await.unwrap();
        assert!(ts("swipe_transition", serde_json::json!({})).changes());
        for bad in [ts("explode_transition", serde_json::json!({})), ts("fade_transition", serde_json::json!(3))] {
            assert!(matches!(e.studio_act(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }

        // Collections and profiles: names, ids and import files are checked first.
        let bad_shelf = [
            ("collections", ShelfAct::New { name: " ".into(), copy: false }),
            ("collections", ShelfAct::Switch { id: "../x".into() }),
            ("profiles", ShelfAct::Import { name: "x".into(), doc: serde_json::json!({ "sources": [] }) }),
            ("collections", ShelfAct::Import { name: "x".into(), doc: serde_json::json!({ "scenes": 1 }) }),
            ("things", ShelfAct::List),
        ];
        for (kind, act) in bad_shelf {
            assert!(matches!(e.studio_shelf(kind, act.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{act:?}");
        }
        assert!(matches!(
            e.studio_shelf("collections", ShelfAct::List).await.unwrap_err(),
            EngineError::Refused { code: "studio_not_here", .. }
        ));
        for (m, code) in [
            ("There's already a scene collection called \"Show\".", "studio_name_taken"),
            ("\"Show\" is in use now.", "studio_in_use"),
            ("There's no profile with that id.", "studio_no_such"),
        ] {
            assert!(matches!(failed(m.into()), EngineError::Refused { code: c, .. } if c == code), "{m}");
        }

        // Studio Mode: quick transitions are the Studio's own, 10 at most.
        let q = |kind: &str, ms| SceneTransition { kind: kind.into(), ms };
        e.studio_act(StudioAct::TransitionNow { quick: Some(q("cut_transition", 0)) }).await.unwrap();
        for bad in [
            StudioAct::TransitionNow { quick: Some(q("boom", 0)) },
            StudioAct::SetQuickTransitions { list: vec![q("fade_transition", 300); 11] },
            StudioAct::SetQuickTransitions { list: vec![q("fade_transition", 90_000)] },
            StudioAct::SetPreview { scene: String::new() },
        ] {
            assert!(matches!(e.studio_act(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }

        // Projectors: what to show is checked first; hosts without windows say so.
        let p = |what: &str, name: Option<&str>, screen: Option<u32>| ProjectorReq {
            what: what.into(),
            name: name.map(str::to_string),
            screen,
            on_top: false,
        };
        for bad in [
            p("source", None, None),
            p("program", Some("Main"), None),
            p("wall", None, None),
            p("multiview", None, Some(99)),
            p("source", Some(""), None),
        ] {
            assert!(matches!(e.studio_projector(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }
        let none = e.studio_projector(p("multiview", None, Some(1))).await.unwrap_err();
        assert!(matches!(none, EngineError::Refused { code: "studio_not_here", .. }));
        assert_eq!(e.studio_screens().await.unwrap(), serde_json::json!([]));
        e.studio_close_projectors().await.unwrap();
        assert!(matches!(
            failed("That display isn't there any more.".into()),
            EngineError::Refused { status: 404, code: "studio_no_screen", .. }
        ));
    }

    #[tokio::test]
    async fn the_host_answers_and_bad_input_never_reaches_it() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        assert!(e.studio_start().await.unwrap().running);
        assert!(matches!(e.studio_stop().await.unwrap_err(), EngineError::Refused { code: "studio_failed", .. }));
        assert!(matches!(failed(STOPPED.into()), EngineError::Refused { status: 409, code: "studio_stopped", .. }));
        assert_eq!(e.studio_options("color_source").await.unwrap()[0]["key"], "color_source");
        for bad in ["", "a b", "../x", &"x".repeat(200)] {
            assert!(matches!(e.studio_options(bad).await.unwrap_err(), EngineError::Invalid(_)), "{bad}");
        }
        let r = PreviewRect {
            x: 10.0,
            y: 20.0,
            w: 640.0,
            h: 360.0,
            vh: 800.0,
            canvas: None,
            bg: None,
            holes: vec![],
            opacity: None,
            shade: None,
        };
        e.studio_preview(Some(r.clone()), Some(r.clone()), Some(r.clone())).await.unwrap();
        e.studio_preview(None, None, None).await.unwrap();
        // The whole stage, with the canvas zoomed out inside it.
        let staged = PreviewRect { canvas: Some(CanvasBox { x: 80.0, y: 45.0, w: 480.0, h: 270.0 }), ..r.clone() };
        assert!(staged.validate().is_ok());
        // A menu over part of it: a hole there, the rest still shows.
        let holed =
            PreviewRect { holes: vec![PreviewHole { x: 400.0, y: -10.0, w: 260.0, h: 500.0, r: 10.0 }], ..r.clone() };
        assert!(holed.validate().is_ok());
        let light = PreviewRect { bg: Some(0xe7e9ee), ..r.clone() };
        assert_eq!(light.bg_abgr(), Some(0xffee_e9e7));
        let canvas = |w: f64, h: f64| PreviewRect { canvas: Some(CanvasBox { x: 0.0, y: 0.0, w, h }), ..r.clone() };
        for bad in [
            PreviewRect { w: 0.0, ..r.clone() },
            PreviewRect { h: f64::NAN, ..r.clone() },
            PreviewRect { x: f64::INFINITY, ..r.clone() },
            PreviewRect { w: 99_999.0, ..r.clone() },
            PreviewRect { vh: -1.0, ..r.clone() },
            canvas(0.0, 270.0),
            canvas(480.0, f64::NAN),
            PreviewRect { bg: Some(0x0100_0000), ..r.clone() },
            PreviewRect { holes: vec![PreviewHole { x: 0.0, y: 0.0, w: f64::NAN, h: 1.0, r: 0.0 }], ..r.clone() },
            PreviewRect { holes: vec![PreviewHole { x: 0.0, y: 0.0, w: 1.0, h: 1.0, r: 500.0 }], ..r.clone() },
            PreviewRect {
                holes: vec![PreviewHole { x: 0.0, y: 0.0, w: 1.0, h: 1.0, r: 0.0 }; PREVIEW_HOLES + 1],
                ..r.clone()
            },
            PreviewRect { opacity: Some(1.5), ..r.clone() },
            PreviewRect { shade: Some(-0.1), ..r.clone() },
        ] {
            assert!(matches!(e.studio_preview(None, None, Some(bad)).await.unwrap_err(), EngineError::Invalid(_)));
        }
        assert_eq!(*fake.rects.lock().unwrap(), vec![Some(r.clone()), None], "only good boxes reach the app");

        let src =
            |kind: &str, name: &str, settings: Value| NewSource { kind: kind.into(), name: name.into(), settings };
        let add = |source| StudioAct::AddSource { scene: "Gameplay".into(), source, fit: true };
        let made = e.studio_act(add(src("color_source", "Background", Value::Null))).await.unwrap();
        assert_eq!(made["source"]["settings"], serde_json::json!({}), "no settings is an empty object");
        for (bad, field) in [
            (add(src("../x", "A", serde_json::json!({}))), "kind"),
            (add(src("color_source", "", serde_json::json!({}))), "name"),
            (add(src("color_source", " lead", serde_json::json!({}))), "name"),
            (add(src("color_source", "A", serde_json::json!([1]))), "settings"),
            (add(src("color_source", "A", serde_json::json!({ "x": "y".repeat(300_000) }))), "settings"),
            (StudioAct::Update { name: "A".into(), settings: serde_json::json!("red") }, "settings"),
            (StudioAct::SetTransition { kind: "fade_transition".into(), ms: 60_000 }, "ms"),
            (
                StudioAct::SetAudio {
                    name: "Mic".into(),
                    change: AudioChange { volume_db: Some(f32::NAN), ..Default::default() },
                },
                "volume_db",
            ),
            (
                StudioAct::SetAudio {
                    name: "Mic".into(),
                    change: AudioChange { volume_db: Some(40.0), ..Default::default() },
                },
                "volume_db",
            ),
            (StudioAct::SetChannel { channel: 7, device: None }, "channel"),
            (StudioAct::Levels { names: vec!["x".into(); 65] }, "names"),
            (StudioAct::Items { scene: "a\nb".into() }, "name"),
            (StudioAct::AddFilter { name: "Mic".into(), filter: src("bad id", "Gate", Value::Null) }, "kind"),
        ] {
            let err = e.studio_act(bad).await.unwrap_err();
            assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with(field)), "{err:?}");
        }
        e.studio_act(StudioAct::EditItem { scene: "Gameplay".into(), id: 3, edit: ItemEdit::Lock }).await.unwrap();
        e.studio_act(StudioAct::SetChannel {
            channel: 3,
            device: Some(src("coreaudio_input_capture", "Mic", Value::Null)),
        })
        .await
        .unwrap();
        assert_eq!(fake.acts.lock().unwrap().len(), 3, "only good acts reach the app");
        assert!(StudioAct::Levels { names: vec![] }.check().is_ok());
        // Settings: the defaults pass, and every field outside what OBS takes is refused.
        let d = StudioSettings::default();
        assert!(d.check().is_ok());
        assert!(e.studio_set_settings(d.clone()).await.unwrap()["restart"].is_null());
        let mut hk = d.clone();
        hk.hotkeys.insert("scene:Just chatting".into(), "Alt+2".into());
        hk.hotkeys.insert("push_to_talk:Mic".into(), "Alt+Space".into());
        // OBS's other hotkeys: Studio Mode, quick transitions, show/hide, filters, media.
        for (action, keys) in [
            ("transition", "Alt+T"),
            ("quick:3", "Alt+3"),
            ("show:Gameplay\nCamera", "Alt+C"),
            ("hide:Gameplay\nCamera", "Alt+Shift+C"),
            ("filter:Mic\nGate", "Alt+G"),
            ("media_restart:Intro", "Alt+R"),
            ("push_to_mute:Mic", "Alt+M"),
            ("split_record", "Alt+S"),
            ("undo", "Alt+Z"),
        ] {
            hk.hotkeys.insert(action.into(), keys.into());
        }
        assert!(hk.check().is_ok(), "{:?}", hk.check());
        for action in ["quick:11", "quick:0", "show:only one name", "media_explode:Intro", "filter:\nGate"] {
            let mut bad = d.clone();
            bad.hotkeys.insert(action.into(), "F8".into());
            assert!(bad.check().is_err(), "{action} is refused");
        }
        for (field, bad) in [
            ("encoder", StudioSettings { encoder: "gpu".into(), ..d.clone() }),
            ("video_kbps", StudioSettings { video_kbps: 90_000, ..d.clone() }),
            ("keyframe_s", StudioSettings { keyframe_s: 30, ..d.clone() }),
            ("profile", StudioSettings { profile: "ultra".into(), ..d.clone() }),
            ("scale", StudioSettings { scale: Some([1281, 720]), ..d.clone() }),
            ("audio_kbps", StudioSettings { audio_kbps: 100, ..d.clone() }),
            ("width", StudioSettings { width: 100, ..d.clone() }),
            ("fps", StudioSettings { fps: 59, ..d.clone() }),
            ("record_folder", StudioSettings { record_folder: Some("relative/path".into()), ..d.clone() }),
            ("record_format", StudioSettings { record_format: "avi".into(), ..d.clone() }),
            ("record_name", StudioSettings { record_name: "a/b".into(), ..d.clone() }),
            ("record_split", StudioSettings { record_split_minutes: 5000, ..d.clone() }),
            ("record_tracks", StudioSettings { record_tracks: vec![0], ..d.clone() }),
            ("record_tracks", StudioSettings { record_tracks: Vec::new(), ..d.clone() }),
            ("video_encoder", StudioSettings { video_encoder: Some("bad id!".into()), ..d.clone() }),
            ("record_quality", StudioSettings { record_quality: "ultra".into(), ..d.clone() }),
            ("reconnect", StudioSettings { reconnect_every_s: 0, ..d.clone() }),
            ("delay_s", StudioSettings { delay_s: 4000, ..d.clone() }),
            ("sample_rate", StudioSettings { sample_rate: 96_000, ..d.clone() }),
        ] {
            let err = e.studio_set_settings(bad).await.unwrap_err();
            assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with(field)), "{field}: {err:?}");
        }
        let mut bad = d.clone();
        bad.hotkeys.insert("explode".into(), "F9".into());
        assert!(bad.check().is_err());
        let mut bad = d.clone();
        bad.hotkeys.insert("record".into(), "F9; rm".into());
        assert!(bad.check().is_err());
        // A file from an older version (fields missing) still reads, with defaults.
        let old: StudioSettings = serde_json::from_str(r#"{ "video_kbps": 4500 }"#).unwrap();
        assert_eq!((old.video_kbps, old.fps), (4500, 30));
        assert!(
            !StudioAct::Audio { scene: "x".into() }.changes() && StudioAct::RemoveScene { scene: "x".into() }.changes()
        );

        for (msg, status) in [
            ("there's no source called \"X\" here", 404),
            ("there's no source with id 4 in \"Gameplay\"", 404),
            ("there's no scene called \"X\"", 404),
            ("there's no filter called \"Gate\" on this source", 404),
            ("name: there's already a source or scene called \"X\".", 409),
            ("name: there's already a filter called \"X\" here.", 409),
            ("A collection keeps at least one scene. Add another first.", 409),
            ("the encoder \"x\" isn't available", 422),
        ] {
            let EngineError::Refused { status: got, message, .. } = failed(msg.into()) else { panic!() };
            assert_eq!(got, status, "{msg}");
            assert!(message.ends_with('.') && !message.starts_with(char::is_lowercase), "{message}");
            assert!(!message.starts_with("Name:"), "{message}");
        }
    }
}
