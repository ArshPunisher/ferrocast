//! The Studio's engine (2.1, Track B) as the dashboard reaches it: over HTTP like everything
//! else (the window has no IPC permissions). The desktop app runs libobs on its main thread
//! and registers itself here as the [`StudioHost`]; the headless server has none, so its routes
//! answer "not on this computer". The engine itself never loads libobs.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Engine, EngineError};

/// Shared-stream ceiling passed to [`StudioHost::set_stream_cap`] when Twitch, Kick, or WebRTC (WHIP) is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamCap {
    pub max_kbps: Option<u32>,
    pub h264_only: bool,
    pub keyframe_secs: u32,
    pub no_bframes: bool,
}

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
    /// Only on the Start after Ferrocast didn't close normally (a crash): what was on then, and
    /// what came back on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered: Option<Recovered>,
    /// Present when an unclean exit was detected or Safe Mode is active / awaiting confirmation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_mode: Option<crate::safe_mode::SafeModeStatus>,
}

/// What the Studio was doing when Ferrocast last closed without quitting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recovered {
    pub record: bool,
    pub replay: bool,
    /// It was live: the stream isn't started again by itself (the dashboard offers it).
    pub live: bool,
    /// What came back on ("recording", "replay buffer").
    #[serde(default)]
    pub again: Vec<String>,
    /// What didn't, in words.
    #[serde(default)]
    pub failed: Vec<String>,
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
    /// The second colour of the diagonal stripes drawn over the part of a source that runs past
    /// the canvas (0xRRGGBB); `bg` is the first. Missing: no stripes, the surround stays plain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe: Option<u32>,
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

/// At most this many holes at once: menus and panels, and while editing the guide lines, the box
/// round several picked sources and its handles (a dozen on their own).
pub const PREVIEW_HOLES: usize = 64;

impl PreviewRect {
    /// `bg` as libobs wants a display's colour (0xAABBGGRR).
    pub fn bg_abgr(&self) -> Option<u32> {
        Self::abgr(self.bg)
    }

    /// `stripe` the same way.
    pub fn stripe_abgr(&self) -> Option<u32> {
        Self::abgr(self.stripe)
    }

    fn abgr(rgb: Option<u32>) -> Option<u32> {
        let rgb = rgb.filter(|c| *c <= 0xff_ffff)?;
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
        if self.stripe.is_some() && self.stripe_abgr().is_none() {
            return Err("stripe: a colour, 0 to 0xFFFFFF.".into());
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

/// Local Studio capture state inspected synchronously by update guards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalCaptureBlock {
    pub recording: bool,
    pub replay: bool,
}

/// The desktop app's side of the Studio (it owns libobs and the window).
#[async_trait::async_trait]
pub trait StudioHost: Send + Sync + std::fmt::Debug {
    async fn status(&self) -> StudioStatus;
    async fn start(&self) -> Result<StudioStatus, String>;
    /// Starts the Studio, deferring saved collection load when `load_collection` is `false` (Safe Mode).
    async fn start_with_mode(&self, _load_collection: bool) -> Result<StudioStatus, String> {
        self.start().await
    }
    /// Confirms loading the saved scene collection while running in Safe Mode or switching to Normal Mode.
    async fn confirm_collection(&self) -> Result<StudioStatus, String> {
        Ok(self.status().await)
    }
    async fn stop(&self) -> Result<StudioStatus, String>;
    /// Every kind of source, filter, transition, encoder and output (`ferrocast_studio::Kinds`).
    async fn kinds(&self) -> Result<Value, String>;
    /// A kind's options (`ferrocast_studio::OptionInfo[]`).
    async fn options(&self, kind: String) -> Result<Value, String>;
    /// Shows the preview at the box in the dashboard window, or hides it (None).
    async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String>;
    /// Performance mode: stops (`paused`) or resumes drawing the preview, Studio Mode's second
    /// picture and the vertical picture, to save GPU and CPU. Never touches a stream, recording,
    /// replay buffer or virtual camera, which draw their own frames. Kept by the host while it
    /// runs (the pictures made again after a canvas change stay as they were set).
    async fn set_preview_paused(&self, _paused: bool) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Whether performance mode has the preview paused now.
    async fn preview_paused(&self) -> bool {
        false
    }
    /// Studio Mode's second picture, what's on air, over its own box (or none).
    async fn program_view(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    /// The vertical picture over its own box (or none).
    async fn vertical_view(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    /// The cursor over the pictures, by its CSS name: on Windows they take the mouse
    /// themselves and pass it on to the page, which says what it shows there.
    async fn preview_cursor(&self, _cursor: String) -> Result<(), String> {
        Ok(())
    }
    /// The settings a new source of `kind` starts with.
    async fn defaults(&self, kind: String) -> Result<Value, String>;
    /// Does one thing to the scene collection (checked before it gets here) and answers what
    /// it shows; changes are saved by the host.
    async fn act(&self, act: StudioAct) -> Result<Value, String>;
    /// The meters' levels (the 300 ms window, or `fast`: a few blocks), read without waiting for the Studio's queue
    /// (`act` runs one thing at a time, in order). None: this host can't (or no engine runs),
    /// ask `act` with `StudioAct::Levels` instead.
    fn levels_now(&self, _names: &[String], _fast: bool) -> Option<Value> {
        None
    }
    /// A shortcut field is waiting for a key (`on`) or stopped waiting: the host's global
    /// shortcuts are let go for the wait, so the page gets every key (those already in use too)
    /// and no action runs from it. Asked again within seconds, it keeps them off; it puts them
    /// back by itself if the page goes away without saying so. Not queued behind other work.
    fn capture_keys(&self, _on: bool) {}
    /// Active local Studio capture state checked synchronously before installing/restarting updates.
    fn local_capture_block(&self) -> LocalCaptureBlock {
        LocalCaptureBlock::default()
    }
    /// The Studio's settings (saved by the host; defaults the first time).
    async fn settings(&self) -> StudioSettings;
    /// Saves and applies them (checked before they get here) → what applies only after
    /// Ferrocast restarts, in words, if anything.
    async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String>;
    /// `{ engine: { cpu, fps, frame_ms, lagged, frames, memory_mb }, canvas, stream, record }`.
    async fn stats(&self) -> Result<Value, String>;
    /// Cheap, read-only Studio snapshot for obs-websocket event polling: never saves to disk or
    /// gathers CPU/disk stats. Returning None falls back to individual `act` calls for test fakes.
    async fn obsws_snapshot(&self) -> Option<Result<Value, String>> {
        None
    }
    /// Starts or stops recording → the recording's stats (null when stopped) and its file.
    async fn record(&self, on: bool) -> Result<Value, String>;
    /// The recording's stats now (`active`, `bytes`, `seconds`, `file`, `error`, `error_detail`,
    /// `disk_free_mb`), or null without one. Looked at every couple of seconds.
    async fn record_stats(&self) -> Result<Value, String> {
        Ok(Value::Null)
    }
    /// Web page sources and whether each stopped drawing (its browser gave up being brought back).
    async fn stalled_sources(&self) -> Vec<(String, bool)> {
        Vec::new()
    }
    /// One-line things the Studio's captures want said (key, line, detail), since the last call.
    async fn take_notices(&self) -> Vec<(String, String, String)> {
        Vec::new()
    }
    async fn record_pause(&self, on: bool) -> Result<(), String>;
    /// Starts or stops the replay buffer → `{ active }`.
    async fn replay(&self, _on: bool) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Saves the replay buffer's last seconds to a file → `{ file }` (null until it's written).
    async fn replay_save(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Records one scene or source on its own (OBS's Source Record), or stops it → `{ file }`
    /// (null when stopped).
    async fn record_source(&self, _name: String, _on: bool) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// A guest asked to join (crates/guest): answers their browser's offer (its JSON) and puts
    /// their camera in the Studio as a source → the answer's JSON.
    async fn guest_join(&self, _name: String, _offer: String) -> Result<String, String> {
        Err(NOT_HERE.into())
    }
    /// The guests now: `[{ name, source, connected, video, audio }]`.
    async fn guests(&self) -> Result<Value, String> {
        Ok(Value::Array(Vec::new()))
    }
    /// Hangs up on a guest (their source stays, showing nothing).
    async fn guest_leave(&self, _name: String) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Starts the virtual camera showing what's on air (`source` None) or one scene or
    /// source, or stops it → `{ active }`.
    async fn virtual_camera(&self, _on: bool, _source: Option<String>) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// The virtual camera's setup on this computer → `{ every_app_possible, every_app_on, store }`
    /// (Windows: whether Windows 11's camera for every app can be, and is, set up).
    async fn virtual_camera_setup(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Sets up (or takes away) the camera for every app (Windows 11; asks for admin once) →
    /// the setup as `virtual_camera_setup` gives it.
    async fn set_virtual_camera_every_app(&self, _on: bool) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Sends what's on air out as the NDI source `name`, or stops → `{ active, name }`.
    async fn ndi_output(&self, _on: bool, _name: Option<String>) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Starts a new file of the recording now (OBS's Split recording).
    async fn record_split(&self) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Where the eye goes in each picture (RGBA, `w`×`h`), across from 0 to 1: a face that
    /// fills enough of it, else what stands out; None where nothing is found.
    async fn focus(&self, _pictures: Vec<Vec<u8>>, _w: u32, _h: u32) -> Result<Vec<Option<f32>>, String> {
        Err(NOT_HERE.into())
    }
    /// Tells the Studio the active shared-stream ceiling imposed by enabled main destinations
    /// (e.g. Twitch 6,000 kbps H.264 / 2 s keyframes) without altering saved user settings.
    async fn set_stream_cap(&self, _cap: Option<StreamCap>) -> Result<(), String> {
        Ok(())
    }
    /// Auto quality: the stream on air (and the vertical one) at `pct` percent of the bitrate
    /// set, from now on; what's saved doesn't change.
    async fn set_live_quality(&self, _pct: u8) -> Result<(), String> {
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
    /// Starts the Studio's stream into Go live; `tracks` are more qualities (width, height,
    /// kbps) it sends as extra video tracks (Twitch Enhanced Broadcasting), usually none.
    async fn stream_start(&self, server: String, key: String, tracks: Vec<(u32, u32, u32)>) -> Result<Value, String>;
    /// Stops the stream (and the vertical picture's with it).
    async fn stream_stop(&self) -> Result<(), String>;
    /// A delayed stream's tail still being sent after Stop: seconds to go and megabytes left.
    fn delay_drain(&self) -> Option<(f64, f64)> {
        None
    }
    /// Ends the delayed tail at once (what is left of the delay is dropped).
    async fn discard_delay(&self) {}
    /// Also streams the vertical picture, while it's on, over RTMP to `key` → whether it
    /// started (false: there's no vertical picture).
    async fn vertical_stream_start(&self, _server: String, _key: String) -> Result<bool, String> {
        Ok(false)
    }
    /// Stops the vertical picture's stream (the wide one goes on).
    async fn vertical_stream_stop(&self) -> Result<(), String> {
        Ok(())
    }
    /// Streams a lighter copy of what's on air (720p, 3,000 kbps at most) to `server`/`key`
    /// beside the full one, for platforms that take it. Running already: nothing changes.
    async fn light_stream_start(&self, _server: String, _key: String) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    /// Stops the lighter stream (the full one goes on).
    async fn light_stream_stop(&self) -> Result<(), String> {
        Ok(())
    }
    /// `ferrocast_studio::StreamStats` plus `error`, or null when not streaming.
    async fn stream_stats(&self) -> Result<Value, String>;
    /// Also sends the Studio's stream straight to an SRT, RIST or WebRTC (WHIP) server. Fails
    /// when the Studio isn't streaming. `simulcast` (WebRTC): the lighter stream goes beside it
    /// as a second layer, while it runs.
    async fn direct_start(&self, _id: String, _url: String, _key: String, _simulcast: bool) -> Result<(), String> {
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
    /// Previews OBS's collection `id` without changing anything in the Studio.
    async fn preview_obs(&self, _id: String) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Replaces the Studio's scenes with OBS's collection `id` (listed by `obs_collections`;
    /// the old scenes are kept in a file first) → what came in.
    async fn import_obs(&self, id: String) -> Result<Value, String>;
    /// Streamlabs Desktop's scene collections on this computer, like `obs_collections`.
    async fn streamlabs_collections(&self) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Previews Streamlabs' collection `id` without changing anything in the Studio.
    async fn preview_streamlabs(&self, _id: String) -> Result<Value, String> {
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
    /// Extra Studio hardware/encoder/renderer telemetry for support diagnostics.
    async fn diagnostics_studio(&self) -> Option<Value> {
        None
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
    /// What streaming and recording would ask of this computer's video encoder, against what
    /// it does (see docs/API.md, `/studio/engine/encode-budget`): for `draft` settings when
    /// given (the page asks while they're picked), else the saved ones; `enhanced` and `light`
    /// say Go live adds Twitch's qualities or the lighter stream. Elsewhere: no figure.
    async fn encode_budget(&self, _draft: Option<Value>, _enhanced: bool, _light: bool) -> Result<Value, String> {
        Ok(serde_json::json!({ "budget_mpx": null, "level": "unknown" }))
    }
    /// Current total machine CPU (%) and GPU (%) load for auto-config benchmarking.
    async fn machine_load(&self) -> (Option<f64>, Option<f64>) {
        (None, None)
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
    /// Live captions (plan item 7): `None` reads where they stand; `Some` asks macOS for
    /// speech recognition, or turns them on (from a sound) or off → `{ access, on, source, text, box }`.
    async fn captions(&self, _set: Option<CaptionsSet>) -> Result<Value, String> {
        Ok(serde_json::json!({ "access": "unsupported", "on": false, "source": null, "text": "", "box": false }))
    }
    /// Smart vertical (plan item 8): `None` reads; `Some` makes an item of a vertical scene
    /// follow the face in it, or stops → `{ on, scene, item, face }`.
    async fn follow_face(&self, _set: Option<FollowSet>) -> Result<Value, String> {
        Err(NOT_HERE.into())
    }
    /// Whisper over a whole recording's sound (16 kHz mono) → what was said and when, for a
    /// clip's subtitles.
    async fn transcribe(&self, _sound: Vec<f32>, _how: Transcribe) -> Result<Vec<Said>, String> {
        Err(NOT_HERE.into())
    }
    /// The app in front, and (with `running`) every app open now → `{ front, running }`, for a
    /// scene per game. None and [] where it can't be told.
    async fn apps(&self, _running: bool) -> Result<Value, String> {
        Ok(serde_json::json!({ "front": null, "running": [] }))
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

/// The transitions the Studio offers (OBS's own kinds + Ferrocast's Move).
pub const TRANSITION_KINDS: [&str; 8] = [
    "fade_transition",
    "cut_transition",
    "swipe_transition",
    "slide_transition",
    "fade_to_color_transition",
    "wipe_transition",
    "obs_stinger_transition",
    "ferrocast_move",
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
/// A change to live captions: ask for macOS's permission, or turn them on from `source` (a
/// Studio sound) or off.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct CaptionsSet {
    #[serde(default)]
    pub ask: bool,
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub source: Option<String>,
    /// "apple" (macOS's own, the default on a Mac) or "whisper" (every system; a downloaded
    /// model).
    #[serde(default)]
    pub engine: Option<String>,
    /// Whisper's model: "base" (the default) or "small".
    #[serde(default)]
    pub model: Option<String>,
    /// Whisper: the language spoken ("auto", "en", "hi"…; the default is auto).
    #[serde(default)]
    pub language: Option<String>,
    /// Whisper: captions in English whatever is spoken.
    #[serde(default)]
    pub translate: bool,
    /// The model's file, found by the engine (never from the request).
    #[serde(skip_deserializing)]
    pub model_path: Option<std::path::PathBuf>,
}

/// Languages Whisper is offered in (it knows more; these are the ones the dashboard lists).
pub const WHISPER_LANGUAGES: [&str; 13] =
    ["auto", "en", "hi", "es", "pt", "fr", "de", "ja", "ko", "ru", "ar", "id", "tr"];

impl CaptionsSet {
    pub fn check(&self) -> Result<(), String> {
        if let Some(e) = &self.engine
            && !["apple", "whisper"].contains(&e.as_str())
        {
            return Err("engine: apple or whisper.".into());
        }
        if let Some(m) = &self.model
            && crate::speech_model::model(m).is_none()
        {
            return Err("model: base or small.".into());
        }
        if let Some(l) = &self.language
            && !WHISPER_LANGUAGES.contains(&l.as_str())
        {
            return Err(format!("language: one of {}.", WHISPER_LANGUAGES.join(", ")));
        }
        if self.on && !self.ask {
            let s = self.source.as_deref().unwrap_or("").trim();
            if s.is_empty() || s.chars().count() > 100 || s.contains('\n') {
                return Err("source: the sound to listen to, 1 to 100 characters on one line.".into());
            }
        }
        Ok(())
    }
}

/// How to transcribe: the model's file, the language ("auto", "en"…), English or not.
#[derive(Debug, Clone)]
pub struct Transcribe {
    pub model: std::path::PathBuf,
    pub language: String,
    pub translate: bool,
    /// Turns true when whoever asked has gone (a cancelled job): the transcription stops at its next
    /// step instead of running to the end of the recording on the CPU.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A stretch of speech: when (ms from the start) and what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Which item follows faces: a vertical scene and an item in it, or neither to stop.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FollowSet {
    #[serde(default)]
    pub scene: Option<String>,
    #[serde(default)]
    pub item: Option<i64>,
    /// "face" (the default) or "action": what stands out, as in a game.
    #[serde(default)]
    pub what: Option<String>,
}

impl FollowSet {
    pub fn check(&self) -> Result<(), String> {
        match (&self.scene, self.item) {
            (None, None) => Ok::<(), String>(()),
            (Some(s), Some(i)) => {
                let s = s.trim();
                if s.is_empty() || s.chars().count() > 100 || s.contains('\n') {
                    Err("scene: a vertical scene's name, 1 to 100 characters on one line.".into())
                } else if i < 0 {
                    Err("item: an item id in that scene.".into())
                } else {
                    Ok(())
                }
            }
            _ => Err("scene and item: both to follow a face, neither to stop.".into()),
        }?;
        match self.what.as_deref() {
            None | Some("face") | Some("action") => Ok(()),
            Some(_) => Err("what: face or action.".into()),
        }
    }
}

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

/// Shortcut actions with no name in them (OBS's hotkeys and Ferrocast's vertical canvas). Others:
/// scene:, mute:, push_to_talk:, push_to_mute:, screenshot:, media_*: <name>; show:, hide:
/// <scene>\n<source>; filter: <source>\n<filter>; quick: <1 to 10>.
pub const HOTKEY_ACTIONS: [&str; 30] = [
    "go_live",
    "end_stream",
    "record",
    "start_record",
    "stop_record",
    "pause_record",
    "unpause_record",
    "split_record",
    "chapter",
    "replay",
    "start_replay",
    "stop_replay",
    "save_replay",
    "virtualcam",
    "start_virtualcam",
    "stop_virtualcam",
    "studio_mode",
    "transition",
    "swap_scenes",
    "fade_to_black",
    "screenshot",
    "vertical:show_preview",
    "vertical:hide_preview",
    "vertical:toggle_preview",
    "vertical:screenshot",
    "performance_mode",
    "performance_mode_on",
    "performance_mode_off",
    "undo",
    "redo",
];

/// How many keys one hotkey action takes (OBS's "+" on a hotkey row).
pub const MAX_KEYS_PER_ACTION: usize = 4;

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
    /// B-frames in the Mac's encoder. Its own key: the old `bframes` (always saved as true, and
    /// ignored) must not switch them on.
    #[serde(rename = "b_frames")]
    pub bframes: bool,
    /// Send at this size instead of the canvas's.
    pub scale: Option<[u32; 2]>,
    /// How a stream sent at another size is scaled: "bilinear", "bicubic" (default), "lanczos" or
    /// "area".
    pub scale_filter: String,
    pub audio_kbps: u32,
    /// The canvas: what's composed.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Broadcast timing: 24 → 23.976, 30 → 29.97, 60 → 59.94 (from the next start).
    pub fps_ntsc: bool,
    /// Where recordings go (absolute); None is the Movies folder's Ferrocast folder.
    pub record_folder: Option<String>,
    /// That Movies (Videos) folder's Ferrocast folder on this computer, so Settings can show
    /// the real path when none is set. Filled when read, never saved or taken from a save.
    #[serde(skip_deserializing, skip_serializing_if = "String::is_empty")]
    pub record_folder_default: String,
    /// "mp4" or "mov" (both crash-safe).
    pub record_format: String,
    /// "stream" (same as the stream), "high" (near-lossless, the default), "balanced" (about half
    /// its size), "lossless", "custom".
    pub record_quality: String,
    pub reconnect: bool,
    pub reconnect_every_s: u32,
    pub reconnect_tries: u32,
    /// Viewers see everything this many seconds later (0: off).
    pub delay_s: u32,
    /// The most memory the delay holds, in megabytes: past it the oldest seconds are dropped
    /// (16 to 4096; 256).
    pub delay_max_mb: u32,
    /// Twitch's VOD track: this audio track (2 to 6) also goes to Twitch for the VOD (leave
    /// music off it); 0: off.
    pub twitch_vod_track: u8,
    /// 44100 or 48000 (applies after a restart).
    pub sample_rate: u32,
    /// The names of the recording's six audio tracks in its file (MP4 and MKV), each up to 40
    /// characters; fewer than six leave the rest unnamed.
    pub audio_track_names: Vec<String>,
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
    /// megabytes (0: the default cap, 512; 1000000 is as good as none), saved as `replay_format` ("mp4", "mkv", "mov", "flv", "ts").
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
    /// Kept from when OBS's engine was there too (settings files still carry them): Ferrocore
    /// always encodes, draws and mixes now.
    pub own_encoder: bool,
    pub own_drawing: bool,
    /// How a Mac draws the picture: "metal" (Apple's own, the default from 29 Oct) or
    /// "opengl" (the older way, for a filter or plugin that draws wrong with Metal). After a
    /// restart; other systems ignore it.
    pub renderer: String,
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
    /// OBS's General › Projectors: new projector windows stay above other apps' windows.
    pub projector_on_top: bool,
    /// Opening a full-screen projector on a display closes the one already there.
    pub projector_one_per_screen: bool,
    /// The projectors open when Ferrocast quits come back at the next Start.
    pub projector_restore: bool,
    /// OBS's "Hide cursor over projectors": the pointer is hidden while it is over a projector
    /// window and comes back when it leaves.
    pub projector_hide_cursor: bool,
}

/// The replay buffer's memory cap when none is set: it kept encoded video in RAM without a limit.
pub const DEFAULT_REPLAY_MB: u32 = 512;

impl StudioSettings {
    /// The megabytes the replay buffer may hold: `replay_mb`, or the default cap for 0 (profiles
    /// saved before the cap existed carry 0).
    pub fn replay_cap_mb(&self) -> u32 {
        if self.replay_mb == 0 { DEFAULT_REPLAY_MB } else { self.replay_mb }
    }
}

impl Default for StudioSettings {
    fn default() -> Self {
        StudioSettings {
            encoder: "hardware".into(),
            video_kbps: 6000,
            vbr: false,
            keyframe_s: 2,
            profile: "high".into(),
            bframes: false,
            scale: None,
            scale_filter: "bicubic".into(),
            audio_kbps: 160,
            width: 1920,
            height: 1080,
            fps: 30,
            fps_ntsc: false,
            record_folder: None,
            record_folder_default: String::new(),
            record_format: "mp4".into(),
            // A recording is a file here, not held to a platform's bitrate cap: constant
            // quality spends bits where the picture needs them (a 6000 kbps "same as stream"
            // at 1440p60 was blurry next to OBS, owner 12 Nov).
            record_quality: "high".into(),
            reconnect: true,
            reconnect_every_s: 2,
            reconnect_tries: 25,
            delay_s: 0,
            delay_max_mb: 256,
            twitch_vod_track: 0,
            sample_rate: 48000,
            audio_track_names: Vec::new(),
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
            replay_mb: DEFAULT_REPLAY_MB,
            replay_format: "mp4".into(),
            speakers: None,
            true_peak: false,
            low_latency_audio: false,
            own_encoder: false,
            own_drawing: false,
            renderer: "metal".into(),
            multiview_layout: "top".into(),
            multiview_labels: true,
            multiview_safe_areas: false,
            // On, 10 pixels, and everything it can line up with (OBS's own default is the canvas's
            // edges only): the preview draws a guide line for each thing a source snaps to.
            snapping: true,
            snap_px: 10,
            snap_screen: true,
            snap_sources: true,
            snap_center: true,
            confirm_go_live: false,
            confirm_end_stream: true,
            confirm_stop_record: false,
            record_when_streaming: false,
            keep_recording: false,
            replay_when_streaming: false,
            keep_replay: false,
            projector_on_top: false,
            projector_one_per_screen: false,
            projector_restore: false,
            projector_hide_cursor: false,
        }
    }
}

/// x264, OBS's software encoder (`obs_x264`).
fn is_x264(id: &str) -> bool {
    id.to_ascii_lowercase().contains("x264")
}

impl StudioSettings {
    /// Whether streaming uses x264 (the software encoder), which Ferrocast's own engine runs
    /// through FFmpeg; with "record when streaming", the recording's encoder counts too.
    pub fn stream_needs_ffmpeg(&self) -> bool {
        let x264 = self.encoder == "software" || self.video_encoder.as_deref().is_some_and(is_x264);
        x264 || (self.record_when_streaming && self.record_needs_ffmpeg())
    }

    /// Whether recording uses x264: Lossless, a custom x264, or the stream's encoder when that's x264.
    pub fn record_needs_ffmpeg(&self) -> bool {
        match (self.record_quality.as_str(), self.record_encoder.as_deref()) {
            ("lossless", _) => true,
            ("custom", Some(id)) => is_x264(id),
            _ => self.encoder == "software" || self.video_encoder.as_deref().is_some_and(is_x264),
        }
    }

    /// Whether any of it does (fetched as soon as such settings are saved).
    pub fn needs_ffmpeg(&self) -> bool {
        self.stream_needs_ffmpeg() || self.record_needs_ffmpeg()
    }

    /// Refuses a value outside what OBS takes, naming the field and what's allowed.
    pub fn check(&self) -> Result<(), EngineError> {
        let bad = |m: &str| Err(EngineError::Invalid(m.into()));
        if !["hardware", "software"].contains(&self.encoder.as_str()) {
            return bad("encoder: hardware or software.");
        }
        if self.twitch_vod_track == 1 || self.twitch_vod_track > 6 {
            return bad("twitch_vod_track: 0 (off) or an audio track from 2 to 6.");
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
        if ferrocore::Filter::downscale_named(&self.scale_filter).is_none() {
            return bad("scale_filter: bilinear, bicubic, lanczos or area.");
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
        if self.fps_ntsc && ![24, 30, 60].contains(&self.fps) {
            return bad("fps_ntsc: only for 24 (23.976), 30 (29.97) or 60 (59.94).");
        }
        if let Some(f) = &self.record_folder
            && (f.len() > 1024 || !std::path::Path::new(f).is_absolute())
        {
            return bad("record_folder: a full folder path.");
        }
        if !RECORD_FORMATS.contains(&self.record_format.as_str()) {
            return bad("record_format: mp4, mov, fmp4, fmov, mkv, flv, ts or hls.");
        }
        if !["stream", "high", "balanced", "lossless", "custom"].contains(&self.record_quality.as_str()) {
            return bad("record_quality: stream, high, balanced, lossless or custom.");
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
            return bad("replay_seconds: 5 to 21600 (replay_mb: 0 to 1000000, 0 is 512).");
        }
        if !["metal", "opengl"].contains(&self.renderer.as_str()) {
            return bad("renderer: metal or opengl.");
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
        if !(16..=4096).contains(&self.delay_max_mb) {
            return bad("delay_max_mb: 16 to 4096 megabytes.");
        }
        if self.audio_track_names.len() > 6
            || self.audio_track_names.iter().any(|n| n.chars().count() > 40 || n.chars().any(char::is_control))
        {
            return bad("audio_track_names: up to 6 names of up to 40 characters.");
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
        for (action, value) in &self.hotkeys {
            let known = HOTKEY_ACTIONS.contains(&action.as_str())
                || ["scene:", "mute:", "push_to_talk:", "push_to_mute:", "media_play_pause:", "media_restart:", "media_stop:", "media_next:", "media_previous:", "screenshot:"]
                    .iter()
                    .any(|p| action.strip_prefix(p).is_some_and(|n| check_source_name(n).is_ok()))
                // A source in a scene, a filter on a source: two names, a line apart.
                || ["show:", "hide:", "filter:", "push_show:", "push_hide:", "push_filter_on:", "push_filter_off:"].iter().any(|p| {
                    action.strip_prefix(p).and_then(|r| r.split_once('\n')).is_some_and(|(x, y)| {
                        check_source_name(x).is_ok() && check_source_name(y).is_ok()
                    })
                })
                || action.strip_prefix("quick:").and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=10).contains(&n));
            if !known {
                return bad("hotkeys: one of the Studio's shortcut actions (see API.md).");
            }
            // One or more keys (up to 4), each named keys (Space, Comma, F9…) joined with +,
            // the keys of one action joined with |: nothing else.
            let all: Vec<&str> = value.split('|').collect();
            if all.len() > MAX_KEYS_PER_ACTION
                || all.iter().any(|keys| {
                    keys.is_empty() || keys.len() > 64 || !keys.chars().all(|c| c.is_ascii_alphanumeric() || c == '+')
                })
            {
                return bad("hotkeys: keys like CmdOrCtrl+Shift+L, up to 4 for one action, joined with |.");
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
    /// Where an item is shown: both (the default), only on the stream, or only in the recording.
    StreamAndRecording,
    StreamOnly,
    RecordingOnly,
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

/// Input to create a saved named transition.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct NamedTransitionInput {
    pub name: String,
    pub kind: String,
    pub ms: u32,
    pub settings: Value,
    pub default: bool,
}

impl Default for NamedTransitionInput {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: "fade_transition".into(),
            ms: 300,
            settings: serde_json::json!({}),
            default: false,
        }
    }
}

/// Fields to update on an existing named transition.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NamedTransitionPatch {
    pub kind: Option<String>,
    pub ms: Option<u32>,
    pub settings: Option<Value>,
    pub default: Option<bool>,
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
    /// Push-to-talk / push-to-mute on or off, and their release delays (0 to 5000 ms).
    pub push_to_talk: Option<bool>,
    pub push_to_talk_ms: Option<u32>,
    pub push_to_mute: Option<bool>,
    pub push_to_mute_ms: Option<u32>,
    /// "This is a mic": whether the live watch judges it as a microphone.
    pub mic: Option<bool>,
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

/// Optional scene-item target for item-scoped video filter operations (`{ scene, id }`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ItemTarget {
    pub scene: String,
    pub id: i64,
}

/// The most boxes one `SetItemBoxes` takes.
pub const MAX_BOXES: usize = 500;

/// One item's box in a batch (`SetItemBoxes`): its first corner and its sides, canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct ItemBoxIn {
    pub id: i64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Why several boxes are sent: a move or resize (a drag, the arrow keys), or one click of Align or
/// Space out, which are named steps of their own in the undo list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxReason {
    #[default]
    Move,
    Align,
    Distribute,
}

/// A box a preview edit sends: numbers on the canvas, sides of at least a pixel.
fn check_box(x: f32, y: f32, w: f32, h: f32) -> Result<(), EngineError> {
    const MAX: f32 = 100_000.0;
    if ![x, y, w, h].iter().all(|v| v.is_finite() && v.abs() <= MAX) {
        return Err(EngineError::Invalid("x, y, w, h: numbers from -100000 to 100000.".into()));
    }
    if w < 1.0 || h < 1.0 {
        return Err(EngineError::Invalid("w, h: at least 1 pixel.".into()));
    }
    Ok(())
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
    /// The scene shown over every scene on air (the top layer), or none.
    SetTopLayer {
        scene: Option<String>,
    },
    /// A scene's own transition (OBS's Transition Override), or none to use the Studio's.
    SetSceneTransition {
        scene: String,
        transition: Option<SceneTransition>,
    },
    /// Adds a saved named transition → `NamedTransition`.
    AddNamedTransition {
        transition: NamedTransitionInput,
    },
    /// Updates kind, duration, settings, or default status on a named transition → `NamedTransition`.
    UpdateNamedTransition {
        name: String,
        patch: NamedTransitionPatch,
    },
    /// Renames a named transition, keeping default and matrix references → `NamedTransition`.
    RenameNamedTransition {
        name: String,
        new_name: String,
    },
    /// Duplicates a named transition (`new_name` optional) → `NamedTransition`.
    DuplicateNamedTransition {
        name: String,
        #[serde(default)]
        new_name: Option<String>,
    },
    /// Deletes a named transition (cleans up matrix references, promotes default if needed).
    DeleteNamedTransition {
        name: String,
    },
    /// Sets the default named transition.
    SetDefaultNamedTransition {
        name: String,
    },
    /// Sets or clears a rule in the from-scene / to-scene transition matrix.
    SetTransitionMatrixRule {
        from: Option<String>,
        to: Option<String>,
        transition: Option<String>,
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
    /// One audio source or global channel by name -> `AudioInfo`.
    AudioByName {
        name: String,
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
    /// A push-to-talk (`talk`) or push-to-mute key went down or up for a sound → whether the sound
    /// has that switched on (false: the key then toggles its mute as before). Never saved.
    PushKey {
        name: String,
        talk: bool,
        down: bool,
    },
    /// One look for the live watch's two-second tick, taken together on the Studio's thread:
    /// `{ scenes, audio, items }`: the scenes (as `Scenes`), the sounds of the scene on air (as
    /// `Audio`, null when none) and the items of that scene and of every scene shown inside it
    /// (as `Items`, by scene name). Replaces a handful of separate reads every tick. Never saved.
    WatchSnapshot,
    /// A push-to-show / push-to-hide key: an item shown or hidden while its key is held and put back
    /// on release (`studio_push_hold`). Never saved and never an undo step.
    PushItem {
        scene: String,
        id: i64,
        visible: bool,
        /// The key is let go: the held view is dropped (`visible` is what it was before).
        #[serde(default)]
        release: bool,
    },
    /// A push-to-enable / push-to-disable key on a source's own filter; as `PushItem`.
    PushFilter {
        name: String,
        filter: String,
        enabled: bool,
        #[serde(default)]
        release: bool,
    },
    /// How these sources' pictures look now: `{ name: { luma, spread, sig } }` (a black or
    /// frozen camera).
    Look {
        names: Vec<String>,
    },
    /// The sound care: `{ ducking: { voices, music, depth_db } | null, loudness_lufs }`.
    AudioCare,
    /// Sets it (as `AudioCare` gives it).
    SetAudioCare {
        care: Value,
    },
    /// The privacy guard: `{ hide_notifications, hide_apps }`, what a display capture leaves out.
    Privacy,
    /// Sets it (as `Privacy` gives it).
    SetPrivacy {
        privacy: Value,
    },
    Filters {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    AddFilter {
        name: String,
        filter: NewSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    RemoveFilter {
        name: String,
        filter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    EditFilter {
        name: String,
        filter: String,
        edit: FilterEdit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    FilterSettings {
        name: String,
        filter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    FilterOptions {
        name: String,
        filter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    UpdateFilter {
        name: String,
        filter: String,
        settings: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    /// Switches one scene item between `"This picture only"` (`own: true`, copies source video
    /// filters to an item override) and `"Everywhere this source is used"` (`own: false`).
    SetItemFilterScope {
        scene: String,
        id: i64,
        own: bool,
    },
    /// A PNG of a scene, source, or scene item (with its filters and transform), saved where
    /// recordings go → `{ file }`.
    Screenshot {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    /// A PNG of exactly what's on air (black when faded, mid-transition as it is) → `{ file }`.
    ScreenshotProgram,
    /// The scenes opened again in the engine, as they are (after a plugin changed).
    Refresh,
    /// Clicks into, stops clicking or reloads a Web page source.
    Web {
        name: String,
        what: WebAct,
    },
    /// Copies every filter of `from` (or `from_item`) onto `name` or `item` (OBS's Copy / Paste Filters).
    PasteFilters {
        name: String,
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_item: Option<ItemTarget>,
    },
    /// Copies an item and its source (a group with every source in it) out as plain data
    /// (`{ item, sources }`), for the clipboard.
    CopySource {
        scene: String,
        id: i64,
    },
    /// Copies a source's (or an item's own) filters out as plain data (`{ filters, from }`).
    CopyFilters {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    /// Puts a copied source into `scene` of the open collection → `{ item, notes }`.
    PasteSource {
        scene: String,
        clip: Value,
    },
    /// Puts copied filters on a source or an item → `{ notes }`.
    PasteFilterClip {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
        clip: Value,
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
    /// Moves and sizes several items at once, each as `SetItemBox` does: aligning, spacing out, or
    /// moving and resizing a group of picked items, as one undo step → the items, in the order
    /// given. All of them or none. `save` at the end of a drag.
    SetItemBoxes {
        scene: String,
        items: Vec<ItemBoxIn>,
        #[serde(default)]
        save: bool,
        /// `align` or `distribute` for one click of those buttons (their own undo step, named
        /// for it); missing: a move.
        #[serde(default)]
        reason: BoxReason,
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
    /// One source's own picture now, before its filters, `width` pixels wide → `{ image,
    /// width, height }` (its real size, for the crop editor), or `{ image: null }` before it
    /// has a picture.
    SourcePicture {
        name: String,
        width: u32,
        /// With its filters on (the drawer's result), as it shows in a scene.
        #[serde(default)]
        filters: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<ItemTarget>,
    },
    /// How a source is doing, for its drawer → `{ web_text?: { at_ms, value, error } | null }`
    /// for a Web text (its last read), `{ page?: { ok, error } }` for a Browser source on the
    /// web (whether its address answers); `{}` for the rest.
    SourceStatus {
        name: String,
    },
    /// A still of a screen (`window` false: a display by its id) or a window, at most `width`
    /// wide, for the cards a capture is picked from → `{ image: data URL | null }` (null where
    /// it can't be taken: not on a Mac yet, or no permission).
    ScreenStill {
        id: u32,
        #[serde(default)]
        window: bool,
        width: u32,
        #[serde(default)]
        fresh: bool,
    },
    /// A still of a display, window, camera, or capture card for Add-source picker cards, at most
    /// `width` wide -> `{ image: data URL | null }`.
    SourceStill {
        kind: String,
        key: String,
        value: Value,
        width: u32,
        #[serde(default)]
        fresh: bool,
    },
    /// The VST plugins on this computer → `{ available, why, plugins: [{ path, cid, name,
    /// vendor, kinds }] }` (looked for once; `rescan` looks again).
    VstPlugins {
        rescan: bool,
    },
    /// A VST filter's plugin, by `what`: "status" → `{ running, problem, changed }`; "open" or
    /// "close" its own window; "save" its state into the filter's settings → `{ saved }`.
    Vst {
        name: String,
        filter: String,
        what: String,
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
        /// The picked source's distances to the canvas edges, in pixels (while it's dragged).
        #[serde(default)]
        guides: bool,
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
            A::Scenes | A::Sources | A::StopVertical | A::AudioCare | A::Privacy | A::WatchSnapshot => Ok(()),
            A::SetPrivacy { privacy } => {
                if !privacy.is_object() || privacy.to_string().len() > 8_000 {
                    return Err(EngineError::Invalid("privacy: { hide_notifications, hide_apps }.".into()));
                }
                Ok(())
            }
            // The Studio checks the names and numbers (it knows the sources); here only its size.
            A::SetAudioCare { care } => {
                if !care.is_object() || care.to_string().len() > 8_000 {
                    return Err(EngineError::Invalid("care: { ducking, loudness_lufs }.".into()));
                }
                Ok(())
            }
            A::AddScene { name, .. } | A::StartVertical { scene: name } => check_source_name(name),
            A::RemoveScene { scene }
            | A::MoveScene { scene, .. }
            | A::SetCurrent { scene }
            | A::Items { scene }
            | A::Audio { scene }
            | A::AudioByName { name: scene } => check_source_name(scene),
            A::DuplicateScene { scene, name } | A::AddExisting { scene, name } => {
                check_source_name(scene)?;
                check_source_name(name)
            }
            A::SetSceneTransition { scene, transition } => {
                check_source_name(scene)?;
                if let Some(t) = transition {
                    check_transition(t)?;
                }
                Ok(())
            }
            A::AddNamedTransition { transition } => {
                check_source_name(&transition.name)?;
                if !TRANSITION_KINDS.contains(&transition.kind.as_str()) {
                    return Err(EngineError::Invalid("kind: one of the Studio's transitions.".into()));
                }
                if transition.ms > 20_000 {
                    return Err(EngineError::Invalid("ms: a transition lasts 0 to 20000 ms.".into()));
                }
                if !transition.settings.is_object() {
                    return Err(EngineError::Invalid("settings: an object of OBS transition settings.".into()));
                }
                check_settings(&transition.settings)
            }
            A::UpdateNamedTransition { name, patch } => {
                check_source_name(name)?;
                if let Some(kind) = &patch.kind
                    && !TRANSITION_KINDS.contains(&kind.as_str())
                {
                    return Err(EngineError::Invalid("kind: one of the Studio's transitions.".into()));
                }
                if let Some(ms) = patch.ms
                    && ms > 20_000
                {
                    return Err(EngineError::Invalid("ms: a transition lasts 0 to 20000 ms.".into()));
                }
                if let Some(settings) = &patch.settings {
                    if !settings.is_object() {
                        return Err(EngineError::Invalid("settings: an object of OBS transition settings.".into()));
                    }
                    check_settings(settings)?;
                }
                Ok(())
            }
            A::RenameNamedTransition { name, new_name } => {
                check_source_name(name)?;
                check_source_name(new_name)
            }
            A::DuplicateNamedTransition { name, new_name } => {
                check_source_name(name)?;
                if let Some(n) = new_name {
                    check_source_name(n)?;
                }
                Ok(())
            }
            A::DeleteNamedTransition { name } | A::SetDefaultNamedTransition { name } => check_source_name(name),
            A::SetTransitionMatrixRule { from, to, transition } => {
                if from.is_none() && to.is_none() {
                    return Err(EngineError::Invalid("from, to: pick a From scene, a To scene, or both.".into()));
                }
                if let Some(f) = from {
                    check_source_name(f)?;
                }
                if let Some(t) = to {
                    check_source_name(t)?;
                }
                if let Some(tr) = transition {
                    check_source_name(tr)?;
                }
                Ok(())
            }
            A::SetTopLayer { scene } => scene.as_deref().map_or(Ok(()), check_source_name),
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
            A::Settings { name } | A::Options { name } => check_source_name(name),
            A::Filters { name, item } => check_filter_target(name, item.as_ref()),
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
                if c.push_to_talk_ms.is_some_and(|ms| ms > 5_000) || c.push_to_mute_ms.is_some_and(|ms| ms > 5_000) {
                    return Err(EngineError::Invalid("push_to_talk_ms, push_to_mute_ms: 0 to 5000 ms.".into()));
                }
                Ok(())
            }
            A::SetChannel { channel, device } => {
                if !(1..=6).contains(channel) {
                    return Err(EngineError::Invalid("channel: 1 to 6.".into()));
                }
                device.as_mut().map(new).transpose().map(|_| ())
            }
            A::PushKey { name, .. } => check_source_name(name),
            A::PushItem { scene, .. } => check_source_name(scene),
            A::PushFilter { name, filter, .. } => {
                check_source_name(name)?;
                check_source_name(filter)
            }
            A::Levels { names } | A::Look { names } => {
                if names.len() > 64 {
                    return Err(EngineError::Invalid("names: up to 64 sources at once.".into()));
                }
                names.iter().try_for_each(|n| check_source_name(n))
            }
            A::AddFilter { name, filter, item } => {
                check_filter_target(name, item.as_ref())?;
                new(filter)
            }
            A::RemoveFilter { name, filter, item }
            | A::EditFilter { name, filter, item, .. }
            | A::FilterSettings { name, filter, item }
            | A::FilterOptions { name, filter, item } => {
                check_filter_target(name, item.as_ref())?;
                check_source_name(filter)
            }
            A::UpdateFilter { name, filter, settings, item } => {
                check_filter_target(name, item.as_ref())?;
                check_source_name(filter)?;
                object(settings)
            }
            A::SetItemFilterScope { scene, .. } => check_source_name(scene),
            A::Web { name, .. } => check_source_name(name),
            A::Screenshot { name, item } => check_filter_target(name, item.as_ref()),
            A::PasteFilters { name, from, item, from_item } => {
                check_filter_target(name, item.as_ref())?;
                check_filter_target(from, from_item.as_ref())?;
                if item.is_none() && from_item.is_none() && name == from {
                    return Err(EngineError::Invalid("from: another source than the one pasted onto.".into()));
                }
                Ok(())
            }
            A::CopySource { scene, .. } => check_source_name(scene),
            A::CopyFilters { name, item } => check_filter_target(name, item.as_ref()),
            A::PasteSource { scene, clip } => {
                check_source_name(scene)?;
                check_clip(clip, "sources")
            }
            A::PasteFilterClip { name, item, clip } => {
                check_filter_target(name, item.as_ref())?;
                check_clip(clip, "filters")
            }
            A::SetItemCrop { scene, crop, .. } => {
                check_source_name(scene)?;
                if crop.iter().any(|c| !(0..=100_000).contains(c)) {
                    return Err(EngineError::Invalid("crop: left, top, right, bottom, 0 to 100000 pixels.".into()));
                }
                Ok(())
            }
            A::SetItemBoxes { scene, items, .. } => {
                check_source_name(scene)?;
                if items.is_empty() || items.len() > MAX_BOXES {
                    return Err(EngineError::Invalid(format!("items: 1 to {MAX_BOXES} boxes.")));
                }
                for (n, b) in items.iter().enumerate() {
                    if items[..n].iter().any(|o| o.id == b.id) {
                        return Err(EngineError::Invalid(format!("items: item {} twice.", b.id)));
                    }
                    check_box(b.x, b.y, b.w, b.h)?;
                }
                Ok(())
            }
            A::SetItemBox { scene, x, y, w, h, .. } => {
                check_source_name(scene)?;
                check_box(*x, *y, *w, *h)
            }
            A::Transform { scene, .. } => check_source_name(scene),
            A::Undo | A::Redo | A::History | A::StudioMode { .. } | A::SetStudioOptions { .. } => Ok(()),
            A::SwapScenes | A::FadeToBlack | A::MissingFiles | A::ScreenshotProgram | A::Refresh => Ok(()),
            A::AddGroup { scene, name } | A::PasteCopy { scene, name } => {
                check_source_name(scene)?;
                check_source_name(name)
            }
            A::Ungroup { scene, .. } | A::SetGroup { scene, .. } => check_source_name(scene),
            A::SourceStatus { name } => check_source_name(name),
            A::ScreenStill { width, .. } if !(64..=1280).contains(width) => {
                Err(EngineError::Invalid("width: 64 to 1280.".into()))
            }
            A::ScreenStill { .. } => Ok(()),
            A::SourceStill { kind, key, value, width, .. } => {
                if kind.trim().is_empty() || kind.len() > 64 {
                    return Err(EngineError::Invalid("kind: a source kind id.".into()));
                }
                if key.trim().is_empty() || key.len() > 64 {
                    return Err(EngineError::Invalid("key: a source setting key.".into()));
                }
                if value.is_null() || matches!(value, Value::String(s) if s.trim().is_empty()) {
                    return Err(EngineError::Invalid("value: a device, screen, or window id.".into()));
                }
                if !(64..=640).contains(width) {
                    return Err(EngineError::Invalid("width: 64 to 640.".into()));
                }
                Ok(())
            }
            A::VstPlugins { .. } => Ok(()),
            A::Vst { name, filter, what } => {
                check_source_name(name)?;
                check_source_name(filter)?;
                if !["status", "open", "close", "save"].contains(&what.as_str()) {
                    return Err(EngineError::Invalid("what: status, open, close or save.".into()));
                }
                Ok(())
            }
            A::Thumbnail { scene: name, width } => {
                check_source_name(name)?;
                if !(64..=640).contains(width) {
                    return Err(EngineError::Invalid("w: 64 to 640 pixels.".into()));
                }
                Ok(())
            }
            A::SourcePicture { name, width, item, .. } => {
                check_filter_target(name, item.as_ref())?;
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
        if let A::SetItemBox { save: last, .. }
        | A::SetItemBoxes { save: last, .. }
        | A::SetItemCrop { save: last, .. }
        | A::TBar { release: last, .. } = self
        {
            return *last;
        }
        !matches!(
            self,
            A::Scenes
                | A::Thumbnail { .. }
                | A::SourcePicture { .. }
                | A::SourceStatus { .. }
                | A::ScreenStill { .. }
                | A::SourceStill { .. }
                | A::VstPlugins { .. }
                | A::Vst { .. }
                | A::Items { .. }
                | A::Sources
                | A::Settings { .. }
                | A::Options { .. }
                | A::Audio { .. }
                | A::AudioByName { .. }
                | A::Levels { .. }
                | A::PushKey { .. }
                | A::PushItem { .. }
                | A::PushFilter { .. }
                | A::WatchSnapshot
                | A::Look { .. }
                | A::AudioCare
                | A::Privacy
                | A::Filters { .. }
                | A::CopySource { .. }
                | A::CopyFilters { .. }
                | A::Screenshot { .. }
                | A::ScreenshotProgram
                | A::Refresh
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
/// A copied list (`key` an array) is at most 200 entries: it comes from the clipboard, but the
/// route is the same one a script could call.
fn check_clip(clip: &Value, key: &str) -> Result<(), EngineError> {
    match clip.get(key).and_then(Value::as_array) {
        Some(l) if !l.is_empty() && l.len() <= 200 => Ok(()),
        _ => Err(EngineError::Invalid(format!("clip: a copy with 1 to 200 {key}."))),
    }
}

fn check_filter_target(name: &str, item: Option<&ItemTarget>) -> Result<(), EngineError> {
    match item {
        Some(it) => {
            if !name.is_empty() {
                check_source_name(name)?;
            }
            check_source_name(&it.scene)
        }
        None => check_source_name(name),
    }
}

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

impl Slot {
    pub(crate) fn optional(&self) -> Option<Arc<dyn StudioHost>> {
        self.host.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

const NONE: &str = "The Studio runs in the Ferrocast app on your computer. Open Ferrocast there.";

impl Engine {
    /// The desktop app registers itself once its window exists.
    pub fn set_studio_host(&self, host: Arc<dyn StudioHost>) {
        *self.inner.studio_host.host.write().unwrap_or_else(|p| p.into_inner()) = Some(host);
        if self.studio().pending_startup_restore.lock().unwrap_or_else(|p| p.into_inner()).is_some()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let e = self.clone();
            rt.spawn(async move {
                e.flush_pending_startup_restore_to_host().await;
            });
        }
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
        let mut status = match self.studio_host() {
            Ok(h) => h.status().await,
            Err(_) => StudioStatus { detail: Some(NONE.into()), ..Default::default() },
        };
        status.safe_mode = self.safe_mode_option();
        status
    }

    pub async fn studio_start(&self) -> Result<StudioStatus, EngineError> {
        self.studio_start_with(crate::safe_mode::StartOptions::default()).await
    }

    pub async fn studio_start_with(&self, opts: crate::safe_mode::StartOptions) -> Result<StudioStatus, EngineError> {
        // Off a Mac, cameras, video files and some screen capture go through FFmpeg: fetched
        // now, in the background, so a new streamer never installs anything or waits later.
        if !cfg!(target_os = "macos") && self.ffmpeg_path().is_none() {
            let _ = self.get_ffmpeg().await;
        }
        let host = self.studio_host()?;
        let was_running = host.status().await.running;
        // A running Studio reloads its scene collection only to put the held-back saved scenes in
        // place of the temporary ones. That is a collection switch, refused while a show or any
        // output is on (it used to run on every Start, wiping the undo history and stopping NDI,
        // the virtual camera and the vertical picture), and it is asked before any choice is made,
        // so a refused one leaves Safe Mode as it was.
        let confirming = was_running
            && self.safe_mode_status().collection_pending
            && (opts.confirm_collection || opts.safe_mode == Some(false));
        if confirming {
            self.ensure_idle_for_switch("scene collections").await?;
        }
        let (load_collection, allow_auto_start, _enabled_normal) = self.apply_start_options(&opts)?;
        let mut status = if was_running {
            if confirming && load_collection {
                self.studio_release_holds().await;
                host.confirm_collection().await.map_err(failed)?
            } else {
                host.status().await
            }
        } else {
            host.start_with_mode(load_collection).await.map_err(failed)?
        };
        // After a crash: the replay buffer and the recording come back on as they were (a new
        // file) unless Safe Mode has auto-start disabled; the stream always waits for the streamer.
        if let Some(r) = status.recovered.as_mut() {
            if allow_auto_start {
                if r.replay {
                    match self.studio_replay(true).await {
                        Ok(_) => r.again.push("replay buffer".into()),
                        Err(e) => r.failed.push(format!("The replay buffer didn't start: {e}")),
                    }
                }
                if r.record {
                    match self.studio_record(true).await {
                        Ok(_) => r.again.push("recording".into()),
                        Err(e) => r.failed.push(format!("Recording didn't start: {e}")),
                    }
                }
                tracing::info!(?r, "the Studio came back after Ferrocast closed unexpectedly");
            } else {
                tracing::info!(?r, "Safe Mode kept recording and replay buffer off after crash");
            }
        }
        status.safe_mode = self.safe_mode_option();
        Ok(status)
    }

    pub async fn studio_stop(&self) -> Result<StudioStatus, EngineError> {
        let host = self.studio_host()?;
        // Keys held down put their items back before the scenes are saved and let go.
        self.studio_release_holds().await;
        let was_running = host.status().await.running;
        // Cancel any active Show, countdown, or pending delayed stop immediately (even before
        // acquiring output_lock) so an in-flight start_show / go_live_now sees the generation
        // bump at once and never posts Discord/chat or resurrects Live after Studio stops.
        // Even when it had already gone (a crash): a show with no Studio under it is over. Not
        // when OBS is there to run a show of its own with the Studio off: that one is not ours.
        if was_running || !matches!(self.rig().await, Ok(crate::studio::rig::Rig::Obs(_))) {
            self.cancel_show_on_studio_stop("Studio stopped.");
        }
        let _out = self.studio().output_lock.lock().await;
        if was_running {
            *crate::golive::lock_enhanced(self) = None;
            self.set_studio_live(false);
        }
        let status = host.stop().await.map_err(failed)?;
        self.send_update_frame();
        Ok(status)
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
        self.flush_pending_startup_restore_to_host().await;
        Ok(self.studio_host()?.settings().await)
    }

    /// Saves and applies the Studio's settings → `{ settings, restart: string | null }`.
    pub async fn studio_set_settings(&self, s: StudioSettings) -> Result<Value, EngineError> {
        self.studio_set_settings_inner(s, false).await
    }

    pub(crate) async fn studio_set_settings_inner(
        &self,
        s: StudioSettings,
        from_auto_config: bool,
    ) -> Result<Value, EngineError> {
        if !from_auto_config {
            self.flush_pending_startup_restore_to_host().await;
        }
        s.check()?;
        // x264 runs through FFmpeg in Ferrocast's own engine: fetched now, so it's here by the
        // time Record or Go live is pressed.
        if s.needs_ffmpeg() && self.ffmpeg_path().is_none() {
            let _ = self.get_ffmpeg().await;
        }
        let host = self.studio_host()?;
        let restart = host.set_settings(s).await.map_err(failed)?;
        let saved = host.settings().await;
        if !from_auto_config {
            self.disarm_auto_config_if_output_changed(&saved);
        }
        self.note_studio_settings(&saved);
        self.send_golive_pub();
        // One short line (`restart`), the full words behind the page's (i) (`restart_detail`).
        let (restart, detail) = one_line(restart, "Some changes apply after a restart");
        let mut out = serde_json::json!({ "settings": saved, "restart": restart });
        if let (Some(d), Some(o)) = (detail, out.as_object_mut()) {
            o.insert("restart_detail".into(), Value::String(d));
        }
        Ok(out)
    }

    /// The Studio's stats, with the stream's health while it's live.
    pub async fn studio_stats(&self) -> Result<Value, EngineError> {
        let mut v = self.studio_host()?.stats().await.map_err(failed)?;
        self.observe_sidechain_cycles(&v);
        let health = self.studio().live_health.lock().unwrap_or_else(|p| p.into_inner()).1.clone();
        if let (Some(o), Some(h)) = (v.as_object_mut(), health) {
            o.insert("health".into(), serde_json::json!(h));
            // Some platforms down while the others are fine: the score says so too, not 100.
            if let Some(cap) = crate::golive::partial_failure_cap(&self.golive_main_states())
                && let Some(score) = o.get_mut("health").and_then(|h| h.get_mut("score"))
                && let Some(now) = score.as_u64()
            {
                *score = Value::from(now.min(cap));
            }
        }
        // The upload itself struggling (most platforms behind at once): the status bar says
        // so, and not when only the drawing is slow or one platform has trouble of its own.
        let struggling =
            self.studio().auto_quality.lock().unwrap_or_else(|p| p.into_inner()).struggling(crate::now().0);
        // The stream delay: what it holds while live, or how long its tail still has to go after Stop.
        let drain = self.studio_host().ok().and_then(|h| h.delay_drain());
        let held_mb = v.pointer("/stream/delay_mb").and_then(Value::as_f64).unwrap_or(0.0);
        if let Some(o) = v.as_object_mut()
            && (drain.is_some() || held_mb > 0.0)
        {
            let (draining, remaining_s, mb) = drain.map_or((false, 0.0, held_mb), |(r, mb)| (true, r, mb));
            o.insert(
                "delay".into(),
                serde_json::json!({ "draining": draining, "remaining_s": remaining_s.round(), "mb": (mb * 10.0).round() / 10.0 }),
            );
        }
        if let Some(s) = v.get_mut("stream").and_then(Value::as_object_mut) {
            s.insert("upload_struggling".into(), Value::Bool(struggling));
            if let Some(reason) = self.golive_main_failed() {
                s.insert("active".into(), Value::Bool(false));
                s.insert("reconnecting".into(), Value::Bool(false));
                s.insert("error".into(), Value::String(reason));
            } else if let Some(reason) = self.golive_main_reconnecting() {
                // Still running, and honest: nothing is reaching any platform.
                s.insert("reconnecting".into(), Value::Bool(true));
                s.insert("reconnecting_reason".into(), Value::String(reason));
            }
        }
        Ok(v)
    }

    /// Refuses once `request_update_restart` has admitted restart under `output_gate` so a late
    /// capture or stream cannot race ahead of clean exit.
    pub(crate) fn ensure_not_restarting_for_update(&self) -> Result<(), EngineError> {
        if self.studio().update_committing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::golive::refused_pub(
                "updating",
                "Ferrocast is restarting to finish an update.",
                "Try again once Ferrocast reopens.",
            ));
        }
        Ok(())
    }

    pub async fn studio_record(&self, on: bool) -> Result<Value, EngineError> {
        let host = self.studio_host()?;
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            self.ensure_not_restarting_for_update()?;
            let s = host.settings().await;
            self.ffmpeg_first(s.record_needs_ffmpeg()).await?;
            Some(g)
        } else {
            None
        };
        let out = host.record(on).await.map_err(failed)?;
        self.send_update_frame();
        Ok(out)
    }

    /// Refuses (and starts fetching FFmpeg) when x264 is `needed` and FFmpeg isn't here yet.
    async fn ffmpeg_first(&self, needed: bool) -> Result<(), EngineError> {
        if !needed || self.ffmpeg_path().is_some() {
            return Ok(());
        }
        self.get_ffmpeg().await?;
        Err(crate::golive::refused_pub(
            "ffmpeg_downloading",
            "Downloading FFmpeg for x264 and Lossless (about 30 MB).",
            "Try again in a minute, or pick the hardware encoder.",
        ))
    }

    pub async fn studio_record_source(&self, name: &str, on: bool) -> Result<Value, EngineError> {
        if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(EngineError::Invalid("name: a scene or source's name.".into()));
        }
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            self.ensure_not_restarting_for_update()?;
            Some(g)
        } else {
            None
        };
        let out = self.studio_host()?.record_source(name.to_string(), on).await.map_err(failed)?;
        self.send_update_frame();
        Ok(out)
    }

    pub async fn studio_record_pause(&self, on: bool) -> Result<(), EngineError> {
        self.studio_host()?.record_pause(on).await.map_err(failed)
    }

    pub async fn studio_replay(&self, on: bool) -> Result<Value, EngineError> {
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            self.ensure_not_restarting_for_update()?;
            Some(g)
        } else {
            None
        };
        let out = self.studio_host()?.replay(on).await.map_err(failed)?;
        self.send_update_frame();
        Ok(out)
    }

    pub async fn studio_replay_save(&self) -> Result<Value, EngineError> {
        self.studio_host()?.replay_save().await.map_err(failed)
    }

    pub async fn studio_virtual_camera(&self, on: bool, source: Option<String>) -> Result<Value, EngineError> {
        if let Some(s) = &source {
            check_source_name(s)?;
        }
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            Some(g)
        } else {
            None
        };
        self.studio_host()?.virtual_camera(on, source).await.map_err(failed)
    }

    pub async fn studio_virtual_camera_setup(&self) -> Result<Value, EngineError> {
        self.studio_host()?.virtual_camera_setup().await.map_err(failed)
    }

    pub async fn studio_set_virtual_camera_every_app(&self, on: bool) -> Result<Value, EngineError> {
        self.studio_host()?.set_virtual_camera_every_app(on).await.map_err(failed)
    }

    /// NDI output: what's on air sent on the network as `name` (default "Ferrocast"), or stopped.
    pub async fn studio_ndi_output(&self, on: bool, name: Option<String>) -> Result<Value, EngineError> {
        if let Some(n) = &name {
            check_source_name(n)?;
        }
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            Some(g)
        } else {
            None
        };
        self.studio_host()?.ndi_output(on, name).await.map_err(failed)
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

    /// Previews OBS's scene collection `id` without modifying Studio state.
    pub async fn studio_preview_obs(&self, id: &str) -> Result<Value, EngineError> {
        if !collection_id_ok(id) {
            return Err(EngineError::Invalid("id: one of OBS's scene collections, as listed.".into()));
        }
        self.studio_host()?.preview_obs(id.to_string()).await.map_err(failed)
    }

    /// Streamlabs Desktop's scene collections, to import one.
    pub async fn studio_streamlabs_collections(&self) -> Result<Value, EngineError> {
        self.studio_host()?.streamlabs_collections().await.map_err(failed)
    }

    /// Previews Streamlabs' scene collection `id` without modifying Studio state.
    pub async fn studio_preview_streamlabs(&self, id: &str) -> Result<Value, EngineError> {
        if !collection_id_ok(id) {
            return Err(EngineError::Invalid("id: one of Streamlabs' scene collections, as listed.".into()));
        }
        self.studio_host()?.preview_streamlabs(id.to_string()).await.map_err(failed)
    }

    /// Brings in Streamlabs' scene collection `id` as a new scene collection.
    pub async fn studio_import_streamlabs(&self, id: &str) -> Result<Value, EngineError> {
        if !collection_id_ok(id) {
            return Err(EngineError::Invalid("id: one of Streamlabs' scene collections, as listed.".into()));
        }
        self.ensure_idle_for_switch("scene collections").await?;
        // Keys held now belong to the collection being left.
        self.studio_release_holds().await;
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
        self.ensure_idle_for_switch("scene collections").await?;
        self.studio_release_holds().await;
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

    /// The meters' levels (`POST /studio/engine/levels`): read straight from the engine's
    /// wait-free meters over a short window, so thirty polls a second never queue behind (or in
    /// front of) the user's edits; the Studio's own queue answers when no engine runs.
    pub async fn studio_levels(&self, names: Vec<String>, fast: bool) -> Result<Value, EngineError> {
        let mut act = StudioAct::Levels { names };
        act.check()?;
        let StudioAct::Levels { names } = &act else { unreachable!() };
        if let Some(v) = self.studio_host()?.levels_now(names, fast) {
            return Ok(v);
        }
        self.studio_act(act).await
    }

    /// Does one thing to the scene collection, after checking it.
    pub async fn studio_act(&self, mut act: StudioAct) -> Result<Value, EngineError> {
        act.check()?;
        // Undo and redo go back through snapshots: a key held now would be in them, and its
        // release would then undo the undo. Held keys are let go first.
        if matches!(act, StudioAct::Undo | StudioAct::Redo) {
            self.studio_release_holds().await;
        }
        let turns_vertical = matches!(act, StudioAct::StartVertical { .. } | StudioAct::StopVertical);
        let out = self.studio_host()?.act(act).await.map_err(failed)?;
        if turns_vertical {
            // Turned on or off while live: the vertical stream follows.
            self.studio_sync_vertical().await;
        }
        Ok(out)
    }

    /// A shortcut field of the dashboard is waiting for a key (`on`) or has stopped: the desktop
    /// lets its global shortcuts go meanwhile (see [`StudioHost::capture_keys`]). Without a
    /// desktop (the server, a browser) there is nothing to let go: fine.
    pub fn studio_capture_keys(&self, on: bool) {
        if let Ok(h) = self.studio_host() {
            h.capture_keys(on);
        }
    }

    /// Starts or stops streaming the Studio into this PC's Go live ingest, which sends it on
    /// to every destination switched on, as a stream from OBS would.
    pub async fn studio_stream(&self, on: bool) -> Result<Value, EngineError> {
        let _out = self.studio().output_lock.lock().await;
        self.studio_stream_locked(on).await
    }

    /// Stops the stream; with `discard_delay` the delay's tail (what the stream delay still holds)
    /// isn't sent either, at once. Without, it goes on being sent for the delay (`stats.delay`).
    pub async fn studio_stream_stop(&self, discard_delay: bool) -> Result<Value, EngineError> {
        let out = self.studio_stream(false).await?;
        if discard_delay {
            self.studio_host()?.discard_delay().await;
        }
        Ok(out)
    }

    /// Runs `studio_stream` while the caller already holds `studio().output_lock`.
    pub(crate) async fn studio_stream_locked(&self, on: bool) -> Result<Value, EngineError> {
        let _gate = if on {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            self.ensure_not_restarting_for_update()?;
            Some(g)
        } else {
            None
        };
        let host = self.studio_host()?;
        let s = host.settings().await;
        self.note_studio_settings(&s);
        if on {
            self.ffmpeg_first(s.stream_needs_ffmpeg()).await?;
        }
        if !on {
            self.set_studio_live(false);
            // Its Enhanced Broadcasting plan ends with it: OBS going live later sends as usual.
            *crate::golive::lock_enhanced(self) = None;
            crate::golive::clear_applied_cap(self);
            host.stream_stop().await.map_err(failed)?;
            // A countdown (or a show waiting for its live scene) with no stream is over: Go
            // live again must start a new show, not say one is running.
            self.end_show_whose_stream_stopped();
            self.send_golive_pub();
            // The stream has ended either way; what didn't stop with it comes back in words.
            return Ok(follow_stream(host.as_ref(), false, &s)
                .await
                .map_or(Value::Null, |also| serde_json::json!({ "also": also })));
        }
        self.sync_stream_cap().await;
        let (server, key) = self.golive_ingest().await?;
        // Twitch Enhanced Broadcasting: its other qualities, when Twitch's destination wants it.
        // Not while one already runs: its plan stays with it (Start then only says how it is).
        let running = host.stream_stats().await.ok().is_some_and(|v| !v.is_null() && v["error"].is_null());
        if !running {
            self.clear_golive_failures(crate::golive::Canvas::Main);
        }
        let out = s.scale.map_or((s.width, s.height), |[w, h]| (w, h));
        let (tracks, eb_note) =
            if running { (Vec::new(), None) } else { self.enhanced_tracks(out, s.fps, s.twitch_vod_track > 0).await };
        let mut v = match host.stream_start(server, key, tracks).await {
            Ok(v) => v,
            Err(e) => {
                if !running {
                    *crate::golive::lock_enhanced(self) = None;
                    self.set_studio_live(false);
                }
                return Err(failed(e));
            }
        };
        // Mark a new stream session (even when `running` was still true during a prior Show's
        // stop delay) so no earlier Show's delayed stop can kill this stream.
        self.on_stream_started();
        self.set_studio_live(true);
        self.send_golive_pub();
        let mut also = if running { None } else { follow_stream(host.as_ref(), true, &s).await };
        if let Some(note) = eb_note {
            also = Some(also.map_or(note.clone(), |a| format!("{a} {note}")));
        }
        if let Some(note) = self.studio_sync_vertical().await {
            also = Some(also.map_or(note.clone(), |a| format!("{a} {note}")));
        }
        if let Some(note) = self.studio_sync_light().await {
            also = Some(also.map_or(note.clone(), |a| format!("{a} {note}")));
        }
        // One short line (`also`), the full words behind the page's (i) (`also_detail`).
        let (also, detail) = one_line(also, "Part of what goes with the stream didn't start");
        if let (Some(also), Some(o)) = (also, v.as_object_mut()) {
            o.insert("also".into(), Value::String(also));
            if let Some(d) = detail {
                o.insert("also_detail".into(), Value::String(d));
            }
        }
        Ok(v)
    }

    /// Starts or stops the Studio's vertical stream to match: while the Studio streams, its
    /// vertical picture goes to Go live's vertical key (as OBS's vertical canvas does) when a
    /// vertical platform is ticked and the picture comes from a canvas. Called when the stream
    /// starts and whenever Go live's platforms, the source or the vertical picture change →
    /// a note when it should send but can't.
    pub(crate) async fn studio_sync_vertical(&self) -> Option<String> {
        let host = self.studio_host().ok()?;
        // The wide stream is on (connecting or live) unless it isn't there or failed.
        let streaming = host.stream_stats().await.ok().is_some_and(|v| !v.is_null() && v["error"].is_null());
        if !streaming || !self.studio_vertical_wanted() {
            if let Err(e) = host.vertical_stream_stop().await {
                tracing::warn!(error = %e, "the Studio's vertical stream didn't stop");
            }
            return None;
        }
        let started = match self.golive_vertical_ingest().await {
            Ok((server, key)) => host.vertical_stream_start(server, key).await,
            Err(e) => Err(e.to_string()),
        };
        match started {
            Ok(true) => None,
            Ok(false) => Some("A vertical platform is on, but there's no vertical picture: add one in Go live.".into()),
            Err(e) => {
                tracing::warn!(error = %e, "the Studio's vertical picture didn't start streaming");
                Some("The vertical stream didn't start; the wide one did.".into())
            }
        }
    }

    /// Starts or stops the Studio's lighter stream to match: while the Studio streams, it goes
    /// to Go live's light key when a ticked platform takes it → a note when it can't.
    pub(crate) async fn studio_sync_light(&self) -> Option<String> {
        let host = self.studio_host().ok()?;
        let streaming = host.stream_stats().await.ok().is_some_and(|v| !v.is_null() && v["error"].is_null());
        // For a platform that takes it, or as a WebRTC server's simulcast layer.
        let wanted = self
            .golive_config()
            .destinations
            .iter()
            .any(|d| d.enabled && (self.wants_light_now(d) || self.simulcast(d)));
        if !streaming || !wanted {
            if let Err(e) = host.light_stream_stop().await {
                tracing::warn!(error = %e, "the Studio's lighter stream didn't stop");
            }
            return None;
        }
        let started = match self.golive_light_ingest().await {
            Ok((server, key)) => host.light_stream_start(server, key).await,
            Err(e) => Err(e.to_string()),
        };
        started.err().map(|e| {
            tracing::warn!(error = %e, "the Studio's lighter stream didn't start");
            "The lighter stream didn't start; the full one did.".to_string()
        })
    }

    /// A vertical platform is ticked and the vertical picture comes from a canvas (the Studio's
    /// or OBS's), not cut from the stream.
    fn studio_vertical_wanted(&self) -> bool {
        use crate::golive::{Canvas, VerticalSource};
        let cfg = self.golive_config();
        cfg.vertical_source == VerticalSource::Canvas
            && cfg.destinations.iter().any(|d| d.enabled && d.canvas == Canvas::Vertical)
    }

    pub async fn studio_stream_stats(&self) -> Result<Value, EngineError> {
        let mut v = self.studio_host()?.stream_stats().await.map_err(failed)?;
        if let Some(s) = v.as_object_mut()
            && let Some(reason) = self.golive_main_failed()
        {
            s.insert("active".into(), Value::Bool(false));
            s.insert("reconnecting".into(), Value::Bool(false));
            s.insert("error".into(), Value::String(reason));
        } else if let Some(s) = v.as_object_mut()
            && let Some(reason) = self.golive_main_reconnecting()
        {
            s.insert("reconnecting".into(), Value::Bool(true));
            s.insert("reconnecting_reason".into(), Value::String(reason));
        }
        Ok(v)
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

    pub async fn studio_encode_budget(&self, draft: Option<Value>) -> Result<Value, EngineError> {
        // Settings that couldn't be saved get the field's error, not a budget (CodeRabbit).
        if let Some(d) = &draft {
            let s: StudioSettings =
                serde_json::from_value(d.clone()).map_err(|e| EngineError::Invalid(format!("settings: {e}")))?;
            s.check()?;
        }
        let config = self.golive_config();
        let on = || config.destinations.iter().filter(|d| d.enabled);
        let enhanced = on().any(|d| d.enhanced);
        let light = on().any(crate::golive::DestinationConfig::wants_light);
        self.studio_host()?.encode_budget(draft, enhanced, light).await.map_err(failed)
    }

    pub async fn studio_screen_access(&self) -> Result<Value, EngineError> {
        self.studio_host()?.screen_access().await.map_err(failed)
    }

    pub async fn studio_fix_screen_access(&self) -> Result<Value, EngineError> {
        self.studio_host()?.fix_screen_access().await.map_err(failed)
    }

    /// Live captions → `{ access, on, source, text, box }`.
    pub async fn studio_captions(&self, mut set: Option<CaptionsSet>) -> Result<Value, EngineError> {
        if let Some(s) = &mut set {
            s.check().map_err(EngineError::Invalid)?;
            // Whisper needs its model here first.
            if s.on && !s.ask && s.engine.as_deref() == Some("whisper") {
                let id = s.model.clone().unwrap_or_else(|| "base".into());
                s.model_path = Some(self.speech_model_path(&id).ok_or_else(|| EngineError::Refused {
                    status: 409,
                    code: "captions_model_missing",
                    message: "The speech model isn't downloaded yet.".into(),
                    hint: "Download it in Live captions first (it stays on this computer).".into(),
                    retry_after: None,
                })?);
            }
        }
        self.studio_host()?.captions(set).await.map_err(captions_failed)
    }

    /// The app in front and the apps open → `{ front, running }`.
    pub async fn studio_apps(&self, running: bool) -> Result<Value, EngineError> {
        self.studio_host()?.apps(running).await.map_err(failed)
    }

    /// Smart vertical → `{ on, scene, item, face }`.
    pub async fn studio_follow_face(&self, set: Option<FollowSet>) -> Result<Value, EngineError> {
        if let Some(s) = &set {
            s.check().map_err(EngineError::Invalid)?;
        }
        self.studio_host()?.follow_face(set).await.map_err(failed)
    }

    /// Opens a projector (a window, or full screen on a display) → `{ id }`.
    pub async fn studio_projector(&self, p: ProjectorReq) -> Result<Value, EngineError> {
        p.validate()?;
        self.studio_host()?.projector(p).await.map_err(failed)
    }

    pub async fn studio_close_projectors(&self) -> Result<(), EngineError> {
        self.studio_host()?.close_projectors().await.map_err(failed)
    }

    /// Refuses scene-collection/profile switches, imports, or encoder benchmarks while a Show or
    /// any live Studio output (stream including reconnecting, vertical/light stream, delay drain,
    /// recording, source record, replay, virtual camera, NDI) is active, failing closed on status
    /// errors so outputs are never disrupted mid-broadcast (M9-01, C1).
    pub(crate) async fn ensure_outputs_idle(
        &self,
        show_action: &str,
        busy_message: String,
        busy_hint: String,
    ) -> Result<(), EngineError> {
        let busy_err = |message: String, hint: String| EngineError::Refused {
            status: 409,
            code: "studio_busy",
            message,
            hint,
            retry_after: None,
        };
        if self.is_show_active() {
            return Err(busy_err(
                format!("Stop the show before {show_action}."),
                "End the show or cancel the countdown first.".into(),
            ));
        }
        let gl = self.golive();
        if gl.hub.live().borrow().on || gl.vhub.live().borrow().on || gl.lhub.live().borrow().on {
            return Err(busy_err(busy_message, busy_hint));
        }
        let Ok(host) = self.studio_host() else { return Ok(()) };
        if host.delay_drain().is_some() {
            return Err(busy_err(busy_message, busy_hint));
        }
        if !host.status().await.running {
            return Ok(());
        }
        let fail_closed = |what: &str, err: String| {
            tracing::warn!(what, error = %err, "studio output status unavailable; failing closed");
            busy_err(
                format!("Couldn't check {what} before {show_action}: {err}"),
                "Wait a moment or restart Studio before retrying.".into(),
            )
        };
        let stream = host.stream_stats().await.map_err(|e| fail_closed("stream status", e))?;
        if stream["active"].as_bool() == Some(true) || stream["reconnecting"].as_bool() == Some(true) {
            return Err(busy_err(busy_message, busy_hint));
        }
        let rec = host.record_stats().await.map_err(|e| fail_closed("recording status", e))?;
        if rec["active"].as_bool() == Some(true) {
            return Err(busy_err(busy_message, busy_hint));
        }
        let v = host.stats().await.map_err(|e| fail_closed("studio stats", e))?;
        let active = |k: &str| v[k]["active"].as_bool() == Some(true);
        let reconnecting = |k: &str| v[k]["reconnecting"].as_bool() == Some(true);
        let ndi_active = v["ndi"].is_object() && v["ndi"]["active"].as_bool() != Some(false);
        let stats_busy = active("stream")
            || reconnecting("stream")
            || active("vertical_stream")
            || reconnecting("vertical_stream")
            || active("light_stream")
            || reconnecting("light_stream")
            || active("record")
            || active("replay")
            || active("virtualcam")
            || ndi_active
            || v["source_records"].as_array().is_some_and(|a| !a.is_empty());
        if stats_busy {
            return Err(busy_err(busy_message, busy_hint));
        }
        Ok(())
    }

    pub(crate) async fn ensure_idle_for_switch(&self, noun: &str) -> Result<(), EngineError> {
        self.ensure_outputs_idle(
            &format!("changing {noun}"),
            format!("Stop streaming and recording before changing {noun}."),
            "Stop every active output first.".into(),
        )
        .await
    }

    /// Scene collections or profiles: one act on that list. Falls back to file-backed storage on
    /// disk when no desktop [`StudioHost`] is attached (such as in `ferrocast-headless`).
    pub async fn studio_shelf(&self, kind: &str, act: ShelfAct) -> Result<Value, EngineError> {
        act.check(kind)?;
        let host = self.inner.studio_host.host.read().unwrap_or_else(|p| p.into_inner()).clone();
        // Swapping what the Studio shows is refused during a show, benchmark, or while it streams
        // or records; with no desktop host attached (headless) the show/benchmark still count.
        let _gate = if matches!(act, ShelfAct::Switch { .. } | ShelfAct::New { .. } | ShelfAct::Import { .. }) {
            let g = self.studio().output_gate.lock().await;
            self.ensure_no_benchmark_running()?;
            let noun = if kind == "profiles" { "profiles" } else { "scene collections" };
            self.ensure_idle_for_switch(noun).await?;
            // A held key's item is in the collection being left: put it back first.
            self.studio_release_holds().await;
            Some(g)
        } else {
            None
        };
        if let Some(host) = host {
            let out = host.shelf(kind.to_string(), act).await.map_err(failed)?;
            // Another collection may have its own vertical picture (or none): the stream follows.
            self.studio_sync_vertical().await;
            return Ok(out);
        }

        let data = self.store().dir().to_path_buf();
        let kind_owned = kind.to_string();
        let res = tokio::task::spawn_blocking(move || {
            use ferrocast_store::studio_library as lib;
            match act {
                ShelfAct::List => lib::headless_shelf_list(&data, &kind_owned),
                ShelfAct::New { name, copy } => lib::headless_shelf_new(&data, &kind_owned, &name, copy),
                ShelfAct::Switch { id } => lib::headless_shelf_switch(&data, &kind_owned, &id),
                ShelfAct::Rename { id, name } => lib::headless_shelf_rename(&data, &kind_owned, &id, &name),
                ShelfAct::Remove { id } => lib::headless_shelf_remove(&data, &kind_owned, &id),
                ShelfAct::Export { id } => lib::headless_shelf_export(&data, &kind_owned, &id),
                ShelfAct::Import { name, doc } => {
                    let (id, _prev, saved_name) = lib::import_collection_doc(&data, &name, doc)?;
                    Ok(serde_json::json!({ "id": id, "name": saved_name }))
                }
            }
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))?;
        res.map_err(failed)
    }

    pub async fn studio_screens(&self) -> Result<Value, EngineError> {
        self.studio_host()?.screens().await.map_err(failed)
    }

    /// Opens Ferrocast again; refused while the Studio streams, records, or benchmarks.
    pub async fn studio_restart(&self) -> Result<(), EngineError> {
        let _gate = self.studio().output_gate.lock().await;
        self.ensure_no_benchmark_running()?;
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

    /// Performance mode now: whether the preview is paused.
    pub async fn studio_preview_paused(&self) -> Result<bool, EngineError> {
        Ok(self.studio_host()?.preview_paused().await)
    }

    /// Performance mode: pauses (`paused`) or resumes drawing the preview → whether it is paused
    /// now. The outputs are never touched, so it is allowed while streaming or recording (that
    /// is what it is for). Said again, it changes nothing.
    pub async fn studio_set_preview_paused(&self, paused: bool) -> Result<bool, EngineError> {
        let host = self.studio_host()?;
        host.set_preview_paused(paused).await.map_err(failed)?;
        Ok(host.preview_paused().await)
    }

    /// The cursor over the pictures (a CSS cursor name, as the page's stage shows it there).
    pub async fn studio_preview_cursor(&self, cursor: String) -> Result<(), EngineError> {
        if cursor.is_empty() || cursor.len() > 32 || !cursor.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            return Err(EngineError::Invalid("cursor: a CSS cursor name, like move or nwse-resize.".into()));
        }
        self.studio_host()?.preview_cursor(cursor).await.map_err(failed)
    }
}

/// Hosts say "Start the Studio first." when it's stopped; anything else is a failure.
pub const STOPPED: &str = "Start the Studio first.";

/// Speech recognition's own refusals say what to do; the rest are the Studio's.
fn captions_failed(message: String) -> EngineError {
    // The Studio's own (stopped, an unknown sound) keep their mapping, whatever the name says.
    if message == STOPPED || message.starts_with("There's no") {
        return failed(message);
    }
    let lower = message.to_lowercase();
    let hint = if message.starts_with("Allow speech recognition") {
        "Press Allow speech recognition in Live captions."
    } else if message.contains("on a Mac for now") {
        "Windows captions come in a later update."
    } else if lower.contains("speech") || lower.contains("dictation") {
        "Then try again."
    } else {
        return failed(message);
    };
    EngineError::Refused { status: 422, code: "captions_unavailable", message, hint: hint.into(), retry_after: None }
}

pub(crate) fn failed(message: String) -> EngineError {
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
    // An item id that isn't in the scene (a box, a crop, a batch): like the other item routes.
    // Its exact shape (`"Scene" has no item 9.`), so a scene or source named "has no item" doesn't
    // read as one.
    if message.starts_with('"') && message.contains("\" has no item ") && !message.starts_with("There's no") {
        return refused(404, "studio_no_source", message, "Refresh the list of sources.");
    }
    // Several boxes at once: a group and one of its own sources can't be in one request.
    if message.contains("can't be moved together") {
        return EngineError::Invalid(message);
    }
    if message.starts_with("There's no scene called") {
        return refused(404, "studio_no_scene", message, "Refresh the list of scenes.");
    }
    if message.starts_with("There's no filter called") {
        return refused(404, "studio_no_filter", message, "Refresh the list of filters.");
    }
    if message.starts_with("There's no transition called") {
        return refused(404, "studio_no_transition", message, "Refresh the list of transitions.");
    }
    if message.contains("already a transition called") {
        return refused(409, "studio_name_taken", message, "Pick another transition name.");
    }
    if message.starts_with("At least one transition must remain") {
        return refused(409, "studio_last_transition", message, "Keep at least one transition in the collection.");
    }
    // A collection's own name is in these messages: it must not decide the answer ("in use now").
    if message.contains("is damaged and can't be read") && message.contains("scene collection") {
        return refused(
            422,
            "studio_collection_broken",
            message,
            "Pick another scene collection or start fresh in Ferrocast.",
        );
    }
    if message.contains("is too large to import") {
        return refused(
            413,
            "studio_collection_too_large",
            message,
            "Split very large collections in OBS or Streamlabs, or start fresh.",
        );
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
    if (message.contains("scene collection") && message.contains("isn't there any more"))
        || message.contains("scene collections aren't on this computer")
        || message.contains("has no such scene collection")
    {
        return refused(404, "studio_no_obs_collection", message, "Pick one from the list.");
    }
    if message.contains("JSON is damaged")
        || message.contains("is damaged and can't be read")
        || message.contains("didn't read")
        || message.contains("isn't an OBS")
        || message.contains("isn't a Streamlabs")
    {
        return refused(
            422,
            "studio_collection_broken",
            message,
            "Pick another scene collection or start fresh in Ferrocast.",
        );
    }
    if message.contains("too large to") {
        return refused(
            413,
            "studio_collection_too_large",
            message,
            "Split very large collections in OBS or Streamlabs, or start fresh.",
        );
    }
    if message.contains("already a source") || message.contains("already a filter") {
        return refused(409, "studio_name_taken", message, "Pick another name.");
    }
    // The message already says what to do; scenes and sources share one set of names.
    if message.contains("is taken: pick another name") {
        return refused(409, "studio_name_taken", message, "Scenes and sources share one set of names.");
    }
    if message.contains("already has a filter called") {
        return refused(409, "studio_name_taken", message, "Each filter on a source needs its own name.");
    }
    if message.contains("already in this scene") {
        return refused(409, "studio_already_here", message, "Pick it in the Sources list instead.");
    }
    if message.contains("isn't here yet") {
        return refused(422, "studio_not_supported", message, "Use Reload for now.");
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
    refused(422, "studio_failed", message, "Check the settings and try again.")
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
/// A hint as one short line plus the rest: a note already short enough stays as it is; a longer
/// one (or several joined) becomes `short`, and its full words are the detail. (The page shows
/// the line and puts the detail behind an (i); an older page shows the line.)
pub(crate) fn one_line(full: Option<String>, short: &str) -> (Option<String>, Option<String>) {
    match full {
        Some(f) if f.chars().count() > 60 => (Some(short.to_string()), Some(f)),
        other => (other, None),
    }
}

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
    #[test]
    fn the_replay_buffer_has_a_memory_cap_unless_one_is_set() {
        let mut s = StudioSettings::default();
        assert_eq!(s.replay_cap_mb(), 512, "the default is capped");
        s.replay_mb = 0;
        assert_eq!(s.replay_cap_mb(), 512, "an older profile's 0 is capped too");
        s.replay_mb = 2048;
        assert_eq!(s.replay_cap_mb(), 2048);
    }

    use super::*;

    #[test]
    fn a_hint_is_one_short_line_with_the_rest_kept_as_its_detail() {
        assert_eq!(one_line(None, "x"), (None, None));
        let short = "Applies after a restart.".to_string();
        assert_eq!(one_line(Some(short.clone()), "x"), (Some(short), None));
        let long = "The replay buffer didn't start: No disk space. The recording didn't start: busy.";
        assert_eq!(
            one_line(Some(long.into()), "Part didn't start"),
            (Some("Part didn't start".into()), Some(long.into()))
        );
    }

    #[derive(Debug, Default)]
    struct Fake {
        rects: std::sync::Mutex<Vec<Option<PreviewRect>>>,
        acts: std::sync::Mutex<Vec<StudioAct>>,
        outputs: std::sync::Mutex<Vec<String>>,
        streaming: std::sync::atomic::AtomicBool,
        said: std::sync::Mutex<Vec<(String, String, String)>>,
        /// What `stats` answers (null when none), and the delay's tail being sent.
        stats: std::sync::Mutex<Option<Value>>,
        drain: std::sync::Mutex<Option<(f64, f64)>>,
        discarded: std::sync::atomic::AtomicBool,
        /// While set, `act` waits (the Studio's queue is busy with a long edit).
        busy: std::sync::atomic::AtomicBool,
        /// Whether `levels_now` answers (an engine is running).
        direct: std::sync::atomic::AtomicBool,
        /// Each `capture_keys(on)` the page asked for.
        captured: std::sync::Mutex<Vec<bool>>,
        /// Performance mode: the preview is paused.
        paused: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl StudioHost for Fake {
        fn capture_keys(&self, on: bool) {
            self.captured.lock().unwrap_or_else(|p| p.into_inner()).push(on);
        }
        fn levels_now(&self, names: &[String], fast: bool) -> Option<Value> {
            self.direct
                .load(std::sync::atomic::Ordering::SeqCst)
                .then(|| serde_json::json!({ names[0].clone(): { "peak": [if fast { -6.0 } else { -20.0 }, -6.0] } }))
        }
        fn delay_drain(&self) -> Option<(f64, f64)> {
            *self.drain.lock().unwrap()
        }
        async fn discard_delay(&self) {
            self.discarded.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        async fn take_notices(&self) -> Vec<(String, String, String)> {
            std::mem::take(&mut *self.said.lock().unwrap())
        }
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
        async fn set_preview_paused(&self, paused: bool) -> Result<(), String> {
            self.paused.store(paused, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn preview_paused(&self) -> bool {
            self.paused.load(std::sync::atomic::Ordering::SeqCst)
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
            Ok(self.stats.lock().unwrap().clone().unwrap_or(Value::Null))
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
            while self.busy.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            self.acts.lock().unwrap_or_else(|p| p.into_inner()).push(act.clone());
            // Copies answer as the desktop's Stage does; everything else echoes the act.
            match &act {
                StudioAct::CopySource { .. } => {
                    return Ok(serde_json::json!({
                        "item": { "name": "Cam", "id": 4 },
                        "sources": [{ "name": "Cam" }, { "name": "Mic" }],
                    }));
                }
                StudioAct::CopyFilters { name, .. } if name == "Empty" => {
                    return Ok(serde_json::json!({ "filters": [], "from": "Empty" }));
                }
                StudioAct::CopyFilters { name, .. } => {
                    return Ok(serde_json::json!({ "filters": [{ "name": "Gate" }], "from": name }));
                }
                _ => {}
            }
            Ok(serde_json::to_value(act).unwrap_or_default())
        }
        async fn stream_start(
            &self,
            server: String,
            key: String,
            _tracks: Vec<(u32, u32, u32)>,
        ) -> Result<Value, String> {
            self.streaming.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(serde_json::json!({ "server": server, "key_len": key.len() }))
        }
        async fn stream_stop(&self) -> Result<(), String> {
            self.streaming.store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        async fn vertical_stream_start(&self, _server: String, key: String) -> Result<bool, String> {
            self.outputs.lock().unwrap_or_else(|p| p.into_inner()).push(format!("vstream on {}", key.len()));
            Ok(true)
        }
        async fn vertical_stream_stop(&self) -> Result<(), String> {
            self.outputs.lock().unwrap_or_else(|p| p.into_inner()).push("vstream off".into());
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(if self.streaming.load(std::sync::atomic::Ordering::Relaxed) {
                serde_json::json!({ "active": true, "error": null })
            } else {
                Value::Null
            })
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(serde_json::json!([{ "id": "Untitled", "name": "Untitled", "scenes": 2, "sources": 3 }]))
        }
        async fn preview_obs(&self, id: String) -> Result<Value, String> {
            match id.as_str() {
                "Untitled" => Ok(serde_json::json!({
                    "source": "obs",
                    "id": "Untitled",
                    "name": "Untitled",
                    "scenes": 2,
                    "sources": 3,
                    "web_pages": 0,
                    "scene_names": ["Main", "BRB"],
                    "truncated_scenes": 0,
                    "device_resets": [],
                    "skipped": [],
                    "skipped_total": 0,
                    "missing": []
                })),
                "broken" => {
                    Err("OBS's scene collection \"broken\" couldn't be read because the JSON is damaged.".into())
                }
                "huge" => Err("The collection file is too large to preview.".into()),
                _ => Err(format!("OBS's scene collection \"{id}\" isn't there any more.")),
            }
        }
        async fn import_obs(&self, id: String) -> Result<Value, String> {
            Ok(
                serde_json::json!({ "id": "untitled-2", "previous_id": "untitled", "name": id, "scenes": 2, "sources": 3, "web_pages": 0, "missing": [] }),
            )
        }
        async fn preview_streamlabs(&self, id: String) -> Result<Value, String> {
            if id == "sl-main" {
                Ok(serde_json::json!({
                    "source": "streamlabs",
                    "id": id,
                    "name": "SL Main",
                    "scenes": 1,
                    "sources": 2,
                    "web_pages": 1,
                    "scene_names": ["Live"],
                    "truncated_scenes": 0,
                    "device_resets": [],
                    "skipped": [],
                    "skipped_total": 0,
                    "missing": []
                }))
            } else {
                Err("That Streamlabs scene collection isn't there any more.".into())
            }
        }
    }

    #[test]
    fn vertical_acts_read_and_check() {
        let add: StudioAct = serde_json::from_value(serde_json::json!({ "act": "add_scene", "name": "Tall" })).unwrap();
        assert_eq!(add, StudioAct::AddScene { name: "Tall".into(), vertical: false });
        let add: StudioAct =
            serde_json::from_value(serde_json::json!({ "act": "add_scene", "name": "Tall", "vertical": true }))
                .unwrap();
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

    /// A shortcut field waiting for a key reaches the desktop's host, in order, and without a
    /// desktop (the server, a browser) it is fine and does nothing.
    #[tokio::test]
    async fn a_shortcut_field_waiting_for_a_key_reaches_the_host() {
        let (e, _d) = engine().await;
        e.studio_capture_keys(true);
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        e.studio_capture_keys(true);
        e.studio_capture_keys(false);
        assert_eq!(*fake.captured.lock().unwrap_or_else(|p| p.into_inner()), vec![true, false]);
    }

    fn pasted(fake: &Fake) -> Vec<StudioAct> {
        fake.acts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|a| matches!(a, StudioAct::PasteSource { .. } | StudioAct::PasteFilterClip { .. }))
            .cloned()
            .collect()
    }

    /// Copy and paste: the engine keeps the copy, hands it to the host on paste, says plainly
    /// when nothing (or the wrong thing) is copied, and keeps it for another collection.
    #[tokio::test]
    async fn the_clipboard_keeps_a_copy_for_a_paste_in_any_collection() {
        let (e, _d) = engine().await;
        assert!(matches!(e.studio_clipboard().unwrap_err(), EngineError::Missing { code: "clipboard_empty", .. }));
        assert!(matches!(
            e.studio_paste_source("Main".into()).await.unwrap_err(),
            EngineError::Missing { code: "clipboard_empty", .. }
        ));
        assert!(matches!(
            e.studio_copy_source("Main".into(), 4).await.unwrap_err(),
            EngineError::Refused { code: "studio_unavailable", .. }
        ));
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        let c = e.studio_copy_source("Main".into(), 4).await.unwrap();
        assert_eq!(c, serde_json::json!({ "kind": "source", "name": "Cam", "from": "Main", "count": 2 }));
        assert_eq!(e.studio_clipboard().unwrap(), c);
        // Another collection opens: the host's state changes, the copy stays.
        e.studio_act(StudioAct::Refresh).await.unwrap();
        assert_eq!(e.studio_clipboard().unwrap(), c);
        e.studio_paste_source("Scene 2".into()).await.unwrap();
        e.studio_paste_source("Scene 2".into()).await.unwrap();
        let p = pasted(&fake);
        assert_eq!(p.len(), 2, "a paste can be repeated");
        let StudioAct::PasteSource { scene, clip } = &p[0] else { panic!("{p:?}") };
        assert_eq!((scene.as_str(), clip["sources"].as_array().map(Vec::len)), ("Scene 2", Some(2)));
        // The wrong kind says what is held; clearing empties it.
        let wrong = e.studio_paste_filters("Mic".into(), None).await.unwrap_err();
        assert!(matches!(&wrong, EngineError::Invalid(m) if m.contains("a source")), "{wrong:?}");
        e.studio_clipboard_clear();
        assert!(e.studio_clipboard().is_err());
    }

    #[tokio::test]
    async fn copied_filters_paste_onto_a_source_or_an_item() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        let none = e.studio_copy_filters("Empty".into(), None).await.unwrap_err();
        assert!(matches!(&none, EngineError::Invalid(m) if m.contains("Empty")), "{none:?}");
        assert!(e.studio_clipboard().is_err(), "a failed copy leaves the clipboard as it was");
        let c = e.studio_copy_filters("Mic".into(), None).await.unwrap();
        assert_eq!(c, serde_json::json!({ "kind": "filters", "name": "Mic", "from": "Mic", "count": 1 }));
        let wrong = e.studio_paste_source("Main".into()).await.unwrap_err();
        assert!(matches!(&wrong, EngineError::Invalid(m) if m.contains("filters")), "{wrong:?}");
        e.studio_paste_filters("Voice".into(), None).await.unwrap();
        e.studio_paste_filters(String::new(), Some(ItemTarget { scene: "Main".into(), id: 3 })).await.unwrap();
        assert_eq!(pasted(&fake).len(), 2);
    }

    #[test]
    fn clipboard_acts_check_their_input_and_only_pastes_change_the_collection() {
        let ok_clip = serde_json::json!({ "sources": [{ "name": "A" }], "filters": [{ "name": "F" }] });
        assert!(StudioAct::PasteSource { scene: "Main".into(), clip: ok_clip.clone() }.check().is_ok());
        assert!(StudioAct::PasteFilterClip { name: "Mic".into(), item: None, clip: ok_clip.clone() }.check().is_ok());
        for bad in [serde_json::json!({}), serde_json::json!({ "sources": [] }), serde_json::json!({ "sources": 3 })] {
            assert!(StudioAct::PasteSource { scene: "Main".into(), clip: bad }.check().is_err());
        }
        let many: Vec<_> = (0..201).map(|_| serde_json::json!({ "name": "x" })).collect();
        assert!(
            StudioAct::PasteFilterClip { name: "Mic".into(), item: None, clip: serde_json::json!({ "filters": many }) }
                .check()
                .is_err()
        );
        assert!(StudioAct::PasteSource { scene: String::new(), clip: ok_clip.clone() }.check().is_err());
        assert!(!StudioAct::CopySource { scene: "Main".into(), id: 1 }.changes());
        assert!(!StudioAct::CopyFilters { name: "Mic".into(), item: None }.changes());
        assert!(StudioAct::PasteSource { scene: "Main".into(), clip: ok_clip.clone() }.changes());
        assert!(StudioAct::PasteFilterClip { name: "Mic".into(), item: None, clip: ok_clip }.changes());
    }

    /// Performance mode: the engine asks the host to pause, answers what the host says, and
    /// saying the same thing twice changes nothing. Without the desktop app it says so (not a
    /// crash, not a pretend success).
    #[tokio::test]
    async fn performance_mode_goes_to_the_host_and_is_idempotent() {
        let (e, _d) = engine().await;
        assert!(matches!(
            e.studio_set_preview_paused(true).await.unwrap_err(),
            EngineError::Refused { code: "studio_unavailable", .. }
        ));
        assert!(e.studio_preview_paused().await.is_err());
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        assert!(!e.studio_preview_paused().await.unwrap());
        assert!(e.studio_set_preview_paused(true).await.unwrap());
        assert!(e.studio_set_preview_paused(true).await.unwrap());
        assert!(e.studio_preview_paused().await.unwrap());
        assert!(!e.studio_set_preview_paused(false).await.unwrap());
        // It never asks the host to do anything to the scenes, the stream or a recording.
        assert!(fake.acts.lock().unwrap_or_else(|p| p.into_inner()).is_empty());
        assert!(fake.outputs.lock().unwrap_or_else(|p| p.into_inner()).is_empty());
    }

    #[test]
    fn the_performance_mode_keys_are_known_shortcut_actions() {
        for a in ["performance_mode", "performance_mode_on", "performance_mode_off"] {
            assert!(HOTKEY_ACTIONS.contains(&a), "{a}");
            let mut s = StudioSettings::default();
            s.hotkeys.insert(a.into(), "Alt+P".into());
            assert!(s.check().is_ok(), "{a}");
        }
    }

    #[tokio::test]
    async fn the_vertical_picture_streams_with_the_wide_one_when_a_vertical_platform_is_ticked() {
        let (e, _d) = engine().await;
        // Go live's local server, on a free port.
        e.start_golive(0).await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        let taken = || std::mem::take(&mut *fake.outputs.lock().unwrap_or_else(|p| p.into_inner()));
        let sent = |o: &Vec<String>| o.iter().any(|o| o.starts_with("vstream on"));
        // No vertical platform ticked: only the wide stream.
        e.studio_stream(true).await.unwrap();
        assert!(!sent(&taken()));
        // Ticked while live: the vertical stream starts then (Go live's platforms changed).
        e.add_golive_destination("Phone test", "rtmp://127.0.0.1:1/live", "k", crate::golive::Canvas::Vertical)
            .await
            .unwrap();
        assert!(sent(&taken()));
        // Cut from the stream instead, while live: Ferrocast makes it, the Studio's stops.
        e.set_vertical_source(crate::golive::VerticalSource::Cut).await.unwrap();
        let out = taken();
        assert!(!sent(&out) && out.iter().any(|o| o == "vstream off"), "{out:?}");
        // Not streaming: nothing is sent, whatever is ticked.
        e.set_vertical_source(crate::golive::VerticalSource::Canvas).await.unwrap();
        e.studio_stream(false).await.unwrap();
        taken();
        e.studio_sync_vertical().await;
        assert!(!sent(&taken()));
    }

    /// The stream delay in the stats: what it holds while live, then how long its tail has to go after
    /// Stop; Stop now (discard_delay) ends the tail, plain Stop doesn't.
    #[tokio::test]
    async fn the_stream_delay_shows_in_the_stats_and_stop_now_discards_the_tail() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        // Live with a delay holding 3.2 MB.
        *fake.stats.lock().unwrap() = Some(serde_json::json!({ "stream": { "active": true, "delay_mb": 3.2 } }));
        let v = e.studio_stats().await.unwrap();
        assert_eq!(v["delay"], serde_json::json!({ "draining": false, "remaining_s": 0.0, "mb": 3.2 }));
        // Without a delay: no "delay".
        *fake.stats.lock().unwrap() = Some(serde_json::json!({ "stream": { "active": true } }));
        assert!(e.studio_stats().await.unwrap().get("delay").is_none());
        // Stopped, the tail still going: 12 s to go, 1.5 MB.
        *fake.stats.lock().unwrap() = Some(serde_json::json!({}));
        *fake.drain.lock().unwrap() = Some((11.6, 1.46));
        let v = e.studio_stats().await.unwrap();
        assert_eq!(v["delay"], serde_json::json!({ "draining": true, "remaining_s": 12.0, "mb": 1.5 }));
        // Stop leaves the tail; Stop now ends it.
        e.studio_stream_stop(false).await.unwrap();
        assert!(!fake.discarded.load(std::sync::atomic::Ordering::SeqCst));
        e.studio_stream_stop(true).await.unwrap();
        assert!(fake.discarded.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Twitch Enhanced Broadcasting's plan belongs to the Studio stream that fetched it: gone
    /// What a Studio's capture says reaches the dashboard once, through the host that owns that
    /// engine (not a process-wide queue another engine could drain).
    #[tokio::test]
    async fn a_capture_notice_comes_through_its_own_host_once() {
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        fake.said.lock().unwrap().push(("mic-bluetooth".into(), "Using KREO KAST.".into(), "why".into()));
        e.set_studio_host(fake);
        let mut live = e.subscribe_live();
        e.record_watch_tick().await;
        e.record_watch_tick().await;
        let mut seen = Vec::new();
        while let Ok(f) = live.try_recv() {
            let v: Value = serde_json::from_str(&f.text).unwrap();
            if v["type"] == "notice" && v["data"]["key"] == "mic-bluetooth" {
                seen.push(v["data"].clone());
            }
        }
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!((seen[0]["level"].as_str(), seen[0]["detail"].as_str()), (Some("info"), Some("why")));
    }

    /// when that stream stops, so OBS going live later never sends to an old address and key.
    #[tokio::test]
    async fn an_enhanced_broadcasting_plan_ends_with_its_stream() {
        let (e, _d) = engine().await;
        e.start_golive(0).await;
        e.set_studio_host(Arc::new(Fake::default()));
        e.studio_stream(true).await.unwrap();
        *crate::golive::lock_enhanced(&e) = Some(ferrocast_golive::twitch_eb::Plan {
            config_id: "c".into(),
            url: "rtmps://ingest.test/app/old_key".into(),
            encodes: Vec::new(),
            warning: None,
        });
        e.studio_stream(false).await.unwrap();
        assert!(crate::golive::lock_enhanced(&e).is_none());
    }

    #[tokio::test]
    async fn levels_are_served_while_the_studio_queue_is_busy() {
        use std::sync::atomic::Ordering::SeqCst;
        let (e, _d) = engine().await;
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake.clone());
        fake.busy.store(true, SeqCst);
        // A long edit holds the Studio's queue.
        let held = tokio::spawn({
            let e = e.clone();
            async move { e.studio_act(StudioAct::AudioCare).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // With an engine running, the meters answer at once, not behind it.
        fake.direct.store(true, SeqCst);
        let t0 = std::time::Instant::now();
        let v = tokio::time::timeout(std::time::Duration::from_millis(50), e.studio_levels(vec!["Mic".into()], true))
            .await
            .expect("levels waited for the busy queue")
            .unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(v["Mic"]["peak"][0], -6.0);
        // The default window is the long one; only `fast` asks for the short one.
        fake.direct.store(true, SeqCst);
        let slow = e.studio_levels(vec!["Mic".into()], false).await.unwrap();
        assert_eq!(slow["Mic"]["peak"][0], -20.0);
        // With none running, the queue answers (as before).
        fake.direct.store(false, SeqCst);
        fake.busy.store(false, SeqCst);
        let v = e.studio_levels(vec!["Mic".into()], false).await.unwrap();
        assert!(v.get("Mic").is_none(), "the fast path answered with no engine: {v}");
        held.await.unwrap().unwrap();
    }

    /// The readers' words for a file that went bad after it was listed map to the same plain error.
    #[test]
    fn a_damaged_or_big_import_file_has_its_own_error() {
        for (m, status, code) in [
            (
                "OBS's scene collection \"x\" is damaged and can't be read. Open it in OBS to check it, or start fresh.",
                422,
                "studio_collection_broken",
            ),
            (
                "That Streamlabs scene collection is damaged and can't be read. Open it in Streamlabs to check it, or start fresh.",
                422,
                "studio_collection_broken",
            ),
            ("OBS's scene collections aren't on this computer.", 404, "studio_no_obs_collection"),
            ("Streamlabs' scene collections aren't on this computer.", 404, "studio_no_obs_collection"),
            ("OBS's scene collection \"x\" is too large to import.", 413, "studio_collection_too_large"),
            ("OBS's scene collection \"in use now\" is too large to import.", 413, "studio_collection_too_large"),
        ] {
            assert!(
                matches!(failed(m.into()), EngineError::Refused { status: s, code: c, .. } if s == status && c == code),
                "{m}"
            );
        }
    }

    #[test]
    fn captions_need_a_sound_only_to_switch_on() {
        let set = |ask, on, source: Option<&str>| CaptionsSet {
            ask,
            on,
            source: source.map(String::from),
            ..Default::default()
        };
        assert!(set(false, false, None).check().is_ok());
        assert!(set(true, false, None).check().is_ok());
        // Asking for permission needs no sound, even with on set.
        assert!(set(true, true, None).check().is_ok());
        assert!(set(false, true, Some("Mic")).check().is_ok());
        for bad in [None, Some(""), Some("   "), Some("a\nb")] {
            assert!(set(false, true, bad).check().is_err(), "{bad:?}");
        }
        assert!(set(false, true, Some(&"m".repeat(101))).check().is_err());
        assert!(set(false, true, Some(&"m".repeat(100))).check().is_ok());
        let w = |engine: &str, model: Option<&str>, language: Option<&str>| CaptionsSet {
            engine: Some(engine.into()),
            model: model.map(String::from),
            language: language.map(String::from),
            ..set(false, true, Some("Mic"))
        };
        assert!(w("whisper", Some("small"), Some("hi")).check().is_ok());
        assert!(w("apple", None, None).check().is_ok());
        assert!(w("siri", None, None).check().is_err());
        assert!(w("whisper", Some("huge"), None).check().is_err());
        assert!(w("whisper", Some("../x"), None).check().is_err());
        assert!(w("whisper", None, Some("klingon")).check().is_err());
        // The model's path never comes from the request.
        let sneaky: CaptionsSet =
            serde_json::from_str(r#"{"on":true,"source":"Mic","model_path":"/etc/passwd"}"#).unwrap();
        assert!(sneaky.model_path.is_none());
    }

    #[test]
    fn settings_saved_before_the_renderer_choice_draw_with_metal() {
        let old: StudioSettings = serde_json::from_str(r#"{"video_kbps": 6000}"#).unwrap();
        assert_eq!(old.renderer, "metal");
        assert!(old.check().is_ok());
        let gl = StudioSettings { renderer: "opengl".into(), ..StudioSettings::default() };
        assert!(gl.check().is_ok());
    }

    /// A new profile snaps a moved source to the canvas's edges and middle and to other sources'
    /// edges and middles, 10 pixels near (the guide lines in the preview show what it caught).
    #[test]
    fn the_preview_snaps_to_everything_by_default() {
        let s = StudioSettings::default();
        assert!(s.snapping && s.snap_screen && s.snap_center && s.snap_sources);
        assert_eq!(s.snap_px, 10);
    }

    /// The preview's holes: the box round several picked sources alone is a dozen, with a few
    /// guide lines and a menu that is more than the old 16 (Neon: the app answered 400 and kept
    /// painting the old holes).
    #[test]
    fn an_item_that_is_not_in_the_scene_is_a_404_like_the_other_item_routes() {
        let e = failed("\"Main\" has no item 999.".into());
        assert!(matches!(&e, EngineError::Refused { status: 404, code: "studio_no_source", .. }), "{e:?}");
        // A scene called that is still a missing scene.
        let e = failed("There's no scene called \"has no item\".".into());
        assert!(matches!(&e, EngineError::Refused { status: 404, code: "studio_no_scene", .. }), "{e:?}");
    }

    #[test]
    fn a_group_with_its_own_source_in_one_batch_is_a_400() {
        let e = failed("a group and a source inside it can't be moved together.".into());
        assert!(matches!(&e, EngineError::Invalid(m) if m.contains("together")), "{e:?}");
    }

    #[test]
    fn the_preview_takes_enough_holes_for_the_edit_guides() {
        let rect = |n: usize| PreviewRect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
            vh: 0.0,
            canvas: None,
            bg: None,
            stripe: None,
            holes: vec![PreviewHole { x: 0.0, y: 0.0, w: 4.0, h: 4.0, r: 0.0 }; n],
            opacity: None,
            shade: None,
        };
        assert!(rect(20).validate().is_ok());
        assert!(rect(PREVIEW_HOLES).validate().is_ok());
        assert!(rect(PREVIEW_HOLES + 1).validate().is_err());
    }

    #[test]
    fn x264_and_lossless_need_ffmpeg() {
        let s = StudioSettings::default();
        assert!(!s.needs_ffmpeg(), "the hardware encoder doesn't");
        assert!(StudioSettings { encoder: "software".into(), ..s.clone() }.needs_ffmpeg());
        assert!(StudioSettings { record_quality: "lossless".into(), ..s.clone() }.needs_ffmpeg());
        let custom =
            StudioSettings { record_quality: "custom".into(), record_encoder: Some("obs_x264".into()), ..s.clone() };
        assert!(custom.record_needs_ffmpeg() && !custom.stream_needs_ffmpeg());
        // Lossless recordings don't hold up a hardware stream, unless it records too.
        let lossless = StudioSettings { record_quality: "lossless".into(), ..s.clone() };
        assert!(!lossless.stream_needs_ffmpeg());
        assert!(StudioSettings { record_when_streaming: true, ..lossless }.stream_needs_ffmpeg());
        // "Same as stream" recordings use the stream's x264.
        let software = StudioSettings { encoder: "software".into(), ..s };
        assert!(software.record_needs_ffmpeg() && software.stream_needs_ffmpeg());
    }

    #[test]
    fn following_needs_both_scene_and_item_or_neither() {
        let set = |scene: Option<&str>, item| FollowSet { scene: scene.map(String::from), item, what: None };
        assert!(set(None, None).check().is_ok());
        assert!(set(Some("Vertical"), Some(3)).check().is_ok());
        for bad in
            [set(Some("Vertical"), None), set(None, Some(3)), set(Some(" "), Some(3)), set(Some("a\nb"), Some(1))]
        {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert!(set(Some("V"), Some(-1)).check().is_err());
        assert!(set(Some(&"v".repeat(101)), Some(1)).check().is_err());
        assert!(serde_json::from_str::<FollowSet>(r#"{"scene":"V","item":1,"x":2}"#).is_err());
        let action = FollowSet { what: Some("action".into()), ..set(Some("V"), Some(1)) };
        assert!(action.check().is_ok());
        assert!(FollowSet { what: Some("cat".into()), ..set(Some("V"), Some(1)) }.check().is_err());
    }

    #[tokio::test]
    async fn captions_are_checked_before_the_host_and_default_to_unsupported() {
        let (e, _d) = engine().await;
        // No desktop app: refused like the rest of the Studio.
        assert!(e.studio_captions(None).await.is_err());
        let fake = Arc::new(Fake::default());
        e.set_studio_host(fake);
        let bad = CaptionsSet { ask: false, on: true, source: None, ..Default::default() };
        assert!(matches!(e.studio_captions(Some(bad)).await, Err(EngineError::Invalid(_))));
        let e1 = captions_failed("Allow speech recognition first.".into());
        assert!(matches!(e1, EngineError::Refused { status: 422, code: "captions_unavailable", .. }), "{e1:?}");
        let e2 = captions_failed("There's no source called \"X\"".into());
        assert!(matches!(e2, EngineError::Refused { status: 404, code: "studio_no_source", .. }), "{e2:?}");
        assert!(matches!(captions_failed(STOPPED.into()), EngineError::Refused { status: 409, .. }));
        let e3 = captions_failed("Speech recognition isn't available on this Mac.".into());
        assert!(matches!(e3, EngineError::Refused { code: "captions_unavailable", .. }), "{e3:?}");
        let e4 = captions_failed("There's no source called \"speech\"".into());
        assert!(matches!(e4, EngineError::Refused { status: 404, code: "studio_no_source", .. }), "{e4:?}");
        // A host that has no captions says so, rather than failing.
        // Whisper without its model downloaded: a 409 saying so.
        let no_model = CaptionsSet {
            engine: Some("whisper".into()),
            ..CaptionsSet { ask: false, on: true, source: Some("Mic".into()), ..Default::default() }
        };
        assert!(matches!(
            e.studio_captions(Some(no_model)).await,
            Err(EngineError::Refused { status: 409, code: "captions_model_missing", .. })
        ));
        let v = e.studio_captions(None).await.unwrap();
        assert_eq!(v["access"], "unsupported");
        assert_eq!(v["on"], false);
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
        let mut same = StudioAct::PasteFilters { name: "Mic".into(), from: "Mic".into(), item: None, from_item: None };
        assert!(matches!(same.check(), Err(EngineError::Invalid(_))));
        assert!(matches!(
            failed("\"Cam\" isn't a web page.".into()),
            EngineError::Refused { status: 400, code: "studio_not_web_page", .. }
        ));
        assert!(matches!(
            failed("Stop streaming and recording before bringing in scenes.".into()),
            EngineError::Refused { status: 409, code: "studio_busy", .. }
        ));
        assert_eq!(e.studio_preview_obs("Untitled").await.unwrap()["scenes"], 2);
        assert_eq!(e.studio_preview_streamlabs("sl-main").await.unwrap()["source"], "streamlabs");
        for bad in ["../Untitled", "", ".x", "a/b"] {
            assert!(matches!(e.studio_preview_obs(bad).await.unwrap_err(), EngineError::Invalid(_)), "{bad}");
            assert!(matches!(e.studio_preview_streamlabs(bad).await.unwrap_err(), EngineError::Invalid(_)), "{bad}");
        }
        assert!(matches!(
            e.studio_preview_obs("Other").await.unwrap_err(),
            EngineError::Refused { status: 404, code: "studio_no_obs_collection", .. }
        ));
        assert!(matches!(
            e.studio_preview_obs("broken").await.unwrap_err(),
            EngineError::Refused { status: 422, code: "studio_collection_broken", .. }
        ));
        assert!(matches!(
            e.studio_preview_obs("huge").await.unwrap_err(),
            EngineError::Refused { status: 413, code: "studio_collection_too_large", .. }
        ));
        assert!(matches!(
            e.studio_preview_streamlabs("missing").await.unwrap_err(),
            EngineError::Refused { status: 404, code: "studio_no_obs_collection", .. }
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
            StudioAct::Select { scene: Some("Main".into()), id: None, frames: false, also: vec![], guides: false },
        ] {
            assert!(matches!(e.studio_act(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }
        // Several boxes at once: one act, checked as a whole.
        let one = |id, x| ItemBoxIn { id, x, y: 0.0, w: 10.0, h: 10.0 };
        let many = |items: Vec<ItemBoxIn>, save| StudioAct::SetItemBoxes {
            scene: "Main".into(),
            items,
            save,
            reason: BoxReason::default(),
        };
        assert!(!many(vec![one(3, 0.0)], false).changes());
        assert!(many(vec![one(3, 0.0)], true).changes());
        e.studio_act(many(vec![one(3, 1.0), one(4, 2.0)], true)).await.unwrap();
        for bad in [
            many(vec![], true),
            many((0..=MAX_BOXES as i64).map(|i| one(i, 0.0)).collect(), true),
            many(vec![one(3, 0.0), one(3, 5.0)], true),
            many(vec![one(3, f32::NAN)], true),
            many(vec![ItemBoxIn { w: 0.5, ..one(3, 0.0) }], true),
            StudioAct::SetItemBoxes {
                scene: String::new(),
                items: vec![one(3, 0.0)],
                save: true,
                reason: BoxReason::default(),
            },
        ] {
            assert!(matches!(e.studio_act(bad.clone()).await.unwrap_err(), EngineError::Invalid(_)), "{bad:?}");
        }
        let pick =
            StudioAct::Select { scene: Some("Main".into()), id: Some(3), frames: false, also: vec![4], guides: false };
        assert!(!pick.changes());
        e.studio_act(pick).await.unwrap();
        e.studio_act(StudioAct::Select {
            scene: Some("Main".into()),
            id: None,
            frames: true,
            also: vec![],
            guides: false,
        })
        .await
        .unwrap();
        e.studio_act(StudioAct::Select { scene: None, id: None, frames: false, also: vec![], guides: false })
            .await
            .unwrap();
        assert_eq!(fake.acts.lock().unwrap().len(), 5);

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
            ("\"Main\" is taken: pick another name.", "studio_name_taken"),
            ("\"Cam\" already has a filter called \"Crop\".", "studio_name_taken"),
            ("This device is already in this scene, as \"Cam\".", "studio_already_here"),
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
            stripe: None,
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
        // The second colour of the stripes over what runs past the canvas, as 0xRRGGBB.
        let striped = PreviewRect { bg: Some(0xe6e8ed), stripe: Some(0xdcdfe6), ..r.clone() };
        assert!(striped.validate().is_ok());
        assert_eq!(striped.stripe_abgr(), Some(0xffe6_dfdc));
        assert_eq!(r.stripe_abgr(), None, "none unless the page says");
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
            PreviewRect { stripe: Some(0x0100_0000), ..r.clone() },
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
                    change: AudioChange { push_to_talk_ms: Some(6_000), ..Default::default() },
                },
                "push_to_talk_ms",
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
            (
                StudioAct::AddFilter { name: "Mic".into(), filter: src("bad id", "Gate", Value::Null), item: None },
                "kind",
            ),
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
            // Several keys for one action; OBS's separate start and stop.
            ("record", "Alt+R|F9|Cmd+Shift+KeyR"),
            ("stop_record", "F10"),
            ("start_replay", "F11"),
            // Vertical canvas preview & screenshot, plus per-scene hotkeys targeting vertical scenes.
            ("vertical:show_preview", "Alt+1"),
            ("vertical:hide_preview", "Alt+4"),
            ("vertical:toggle_preview", "Alt+V"),
            ("vertical:screenshot", "Alt+P"),
            ("scene:Tall", "Alt+5"),
            ("screenshot:Tall", "Alt+6"),
            ("show:Tall\nCam", "Alt+7"),
            ("hide:Tall\nCam", "Alt+8"),
        ] {
            hk.hotkeys.insert(action.into(), keys.into());
        }
        assert!(hk.check().is_ok(), "{:?}", hk.check());
        for action in [
            "quick:11",
            "quick:0",
            "show:only one name",
            "media_explode:Intro",
            "filter:\nGate",
            // Unwired Aitum Vertical outputs must stay rejected until standalone engine support exists.
            "vertical:record",
            "vertical:stream",
            "vertical:clip",
            "vertical:virtualcam",
            "vertical:unknown",
        ] {
            let mut bad = d.clone();
            bad.hotkeys.insert(action.into(), "F8".into());
            assert!(bad.check().is_err(), "{action} is refused");
        }
        for (field, bad) in [
            ("renderer", StudioSettings { renderer: "vulkan".into(), ..d.clone() }),
            ("encoder", StudioSettings { encoder: "gpu".into(), ..d.clone() }),
            ("video_kbps", StudioSettings { video_kbps: 90_000, ..d.clone() }),
            ("keyframe_s", StudioSettings { keyframe_s: 30, ..d.clone() }),
            ("profile", StudioSettings { profile: "ultra".into(), ..d.clone() }),
            ("scale", StudioSettings { scale: Some([1281, 720]), ..d.clone() }),
            ("scale_filter", StudioSettings { scale_filter: "sharp".into(), ..d.clone() }),
            ("audio_kbps", StudioSettings { audio_kbps: 100, ..d.clone() }),
            ("width", StudioSettings { width: 100, ..d.clone() }),
            ("fps", StudioSettings { fps: 59, ..d.clone() }),
            ("fps_ntsc", StudioSettings { fps: 50, fps_ntsc: true, ..d.clone() }),
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
            ("delay_max_mb", StudioSettings { delay_max_mb: 8, ..d.clone() }),
            ("sample_rate", StudioSettings { sample_rate: 96_000, ..d.clone() }),
            ("audio_track_names", StudioSettings { audio_track_names: vec!["x".repeat(41)], ..d.clone() }),
        ] {
            let err = e.studio_set_settings(bad).await.unwrap_err();
            assert!(matches!(&err, EngineError::Invalid(m) if m.starts_with(field)), "{field}: {err:?}");
        }
        for keys in ["F9|", "|F9", "F9||F8", "A|B|C|D|E", "F9|F8;x"] {
            let mut bad = d.clone();
            bad.hotkeys.insert("record".into(), keys.into());
            assert!(bad.check().is_err(), "{keys:?} is refused");
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
        // The projector options are off until asked for: today's behaviour.
        assert!(
            !old.projector_on_top
                && !old.projector_one_per_screen
                && !old.projector_restore
                && !old.projector_hide_cursor
        );
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

    #[test]
    fn item_filter_acts_validate_and_track_changes() {
        let tgt = ItemTarget { scene: "Vertical".into(), id: 2 };
        let mut good = [
            StudioAct::Filters { name: String::new(), item: Some(tgt.clone()) },
            StudioAct::AddFilter {
                name: String::new(),
                filter: NewSource {
                    kind: "ferrocast_rounded_corners".into(),
                    name: "Round".into(),
                    settings: Value::Null,
                },
                item: Some(tgt.clone()),
            },
            StudioAct::UpdateFilter {
                name: String::new(),
                filter: "Round".into(),
                settings: serde_json::json!({ "radius": 8 }),
                item: Some(tgt.clone()),
            },
            StudioAct::PasteFilters {
                name: String::new(),
                from: "Cam".into(),
                item: Some(tgt.clone()),
                from_item: None,
            },
            StudioAct::SourcePicture { name: "Cam".into(), width: 256, filters: true, item: Some(tgt.clone()) },
            StudioAct::SetItemFilterScope { scene: "Vertical".into(), id: 2, own: true },
        ];
        for a in &mut good {
            assert!(a.check().is_ok(), "{a:?}");
        }
        assert!(!good[0].changes());
        assert!(!good[4].changes());
        assert!(good[1].changes());
        assert!(good[2].changes());
        assert!(good[3].changes());
        assert!(good[5].changes());

        for mut shot in [
            StudioAct::Screenshot { name: "Cam".into(), item: Some(tgt.clone()) },
            StudioAct::Screenshot { name: String::new(), item: Some(tgt.clone()) },
            StudioAct::Screenshot { name: "Main".into(), item: None },
        ] {
            assert!(shot.check().is_ok(), "{shot:?}");
            assert!(!shot.changes());
        }
        let mut bad_shot = StudioAct::Screenshot { name: String::new(), item: None };
        assert!(matches!(bad_shot.check(), Err(EngineError::Invalid(_))));
        let mut bad_shot_name =
            StudioAct::Screenshot { name: "bad\nname".into(), item: Some(ItemTarget { scene: "Main".into(), id: 1 }) };
        assert!(matches!(bad_shot_name.check(), Err(EngineError::Invalid(_))));

        let mut bad_scope = StudioAct::SetItemFilterScope { scene: "bad\nscene".into(), id: 2, own: true };
        assert!(matches!(bad_scope.check(), Err(EngineError::Invalid(_))));
        let mut bad_target =
            StudioAct::Filters { name: String::new(), item: Some(ItemTarget { scene: "".into(), id: 1 }) };
        assert!(matches!(bad_target.check(), Err(EngineError::Invalid(_))));
        let mut bad_pic_name =
            StudioAct::SourcePicture { name: "a\nb".into(), width: 256, filters: true, item: Some(tgt) };
        assert!(matches!(bad_pic_name.check(), Err(EngineError::Invalid(_))));
    }

    #[test]
    fn named_transition_and_matrix_acts_validate_and_map_errors() {
        let mut good = [
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput {
                    name: "Quick Fade".into(),
                    kind: "fade_transition".into(),
                    ms: 200,
                    settings: serde_json::json!({}),
                    default: false,
                },
            },
            StudioAct::UpdateNamedTransition {
                name: "Quick Fade".into(),
                patch: NamedTransitionPatch {
                    kind: Some("ferrocast_move".into()),
                    ms: Some(400),
                    settings: Some(serde_json::json!({})),
                    default: Some(true),
                },
            },
            StudioAct::RenameNamedTransition { name: "Quick Fade".into(), new_name: "Slow Move".into() },
            StudioAct::DuplicateNamedTransition { name: "Slow Move".into(), new_name: None },
            StudioAct::DeleteNamedTransition { name: "Slow Move 2".into() },
            StudioAct::SetDefaultNamedTransition { name: "Fade".into() },
            StudioAct::SetTransitionMatrixRule {
                from: Some("Main".into()),
                to: None,
                transition: Some("Slow Move".into()),
            },
        ];
        for a in &mut good {
            assert!(a.check().is_ok(), "{a:?}");
            assert!(a.changes(), "{a:?}");
        }

        let mut bad = [
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput { name: " ".into(), ..Default::default() },
            },
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput {
                    name: "BadKind".into(),
                    kind: "explode".into(),
                    ..Default::default()
                },
            },
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput { name: "TooLong".into(), ms: 20_001, ..Default::default() },
            },
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput {
                    name: "BadSettings".into(),
                    settings: serde_json::json!(42),
                    ..Default::default()
                },
            },
            StudioAct::SetTransitionMatrixRule { from: None, to: None, transition: Some("Fade".into()) },
            StudioAct::SetTransitionMatrixRule {
                from: Some("".into()),
                to: Some("Main".into()),
                transition: Some("Fade".into()),
            },
        ];
        for b in &mut bad {
            assert!(matches!(b.check(), Err(EngineError::Invalid(_))), "{b:?}");
        }

        assert!(matches!(
            failed("There's no transition called \"Gone\".".into()),
            EngineError::Refused { status: 404, code: "studio_no_transition", .. }
        ));
        assert!(matches!(
            failed("There's already a transition called \"Fade\".".into()),
            EngineError::Refused { status: 409, code: "studio_name_taken", .. }
        ));
        assert!(matches!(
            failed("At least one transition must remain.".into()),
            EngineError::Refused { status: 409, code: "studio_last_transition", .. }
        ));
    }
}
