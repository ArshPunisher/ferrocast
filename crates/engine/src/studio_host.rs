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
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct PreviewRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// The page's visible height (`window.innerHeight`); 0 or missing if unknown.
    #[serde(default)]
    pub vh: f64,
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
    /// `{ devices: { name, id }[], current: { name, id } }` for "Hear it".
    async fn monitoring(&self) -> Result<Value, String>;
    /// Streams the program over RTMP (Ferrocast's own Go live ingest) → stats.
    async fn stream_start(&self, server: String, key: String) -> Result<Value, String>;
    async fn stream_stop(&self) -> Result<(), String>;
    /// `ferrocast_studio::StreamStats` plus `error`, or null when not streaming.
    async fn stream_stats(&self) -> Result<Value, String>;
    /// OBS's scene collections on this computer: `{ id, name, scenes, sources }[]`.
    async fn obs_collections(&self) -> Result<Value, String>;
    /// Replaces the Studio's scenes with OBS's collection `id` (listed by `obs_collections`;
    /// the old scenes are kept in a file first) → what came in.
    async fn import_obs(&self, id: String) -> Result<Value, String>;
}

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
        if !["mp4", "mov"].contains(&self.record_format.as_str()) {
            return bad("record_format: mp4 or mov.");
        }
        if !["stream", "high", "lossless"].contains(&self.record_quality.as_str()) {
            return bad("record_quality: stream, high or lossless.");
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
            let known = ["go_live", "end_stream", "record", "pause_record"].contains(&action.as_str())
                || ["scene:", "mute:", "push_to_talk:"]
                    .iter()
                    .any(|p| action.strip_prefix(p).is_some_and(|n| check_source_name(n).is_ok()));
            if !known {
                return bad(
                    "hotkeys: go_live, end_stream, record, pause_record, scene:, mute: or push_to_talk: a name.",
                );
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
    /// `{ scenes, current, transition }`.
    Scenes,
    AddScene {
        name: String,
    },
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
        volume_db: Option<f32>,
        muted: Option<bool>,
        monitor: Option<Monitor>,
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
            A::Scenes | A::Sources => Ok(()),
            A::AddScene { name } => check_source_name(name),
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
                    const KINDS: [&str; 6] = [
                        "fade_transition",
                        "cut_transition",
                        "swipe_transition",
                        "slide_transition",
                        "fade_to_color_transition",
                        "wipe_transition",
                    ];
                    if !KINDS.contains(&t.kind.as_str()) {
                        return Err(EngineError::Invalid(
                            "kind: fade, cut, swipe, slide, fade to colour or wipe transition.".into(),
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
            A::SetAudio { name, volume_db, .. } => {
                check_source_name(name)?;
                if let Some(v) = volume_db
                    && !(v.is_finite() && (-100.0..=26.0).contains(v))
                {
                    return Err(EngineError::Invalid("volume_db: -100 to 26.".into()));
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
        }
    }

    /// It changes the collection (the host saves it after).
    pub fn changes(&self) -> bool {
        use StudioAct as A;
        !matches!(
            self,
            A::Scenes
                | A::Items { .. }
                | A::Sources
                | A::Settings { .. }
                | A::Options { .. }
                | A::Audio { .. }
                | A::Levels { .. }
                | A::Filters { .. }
                | A::Screenshot { .. }
                | A::FilterSettings { .. }
                | A::FilterOptions { .. }
        )
    }
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

    fn studio_host(&self) -> Result<Arc<dyn StudioHost>, EngineError> {
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

    /// OBS's scene collections, to import one.
    pub async fn studio_obs_collections(&self) -> Result<Value, EngineError> {
        self.studio_host()?.obs_collections().await.map_err(failed)
    }

    /// Brings in OBS's scene collection `id` in place of the Studio's scenes.
    pub async fn studio_import_obs(&self, id: &str) -> Result<Value, EngineError> {
        let ok = id.len() <= 128
            && !id.is_empty()
            && !id.starts_with('.')
            && id.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'));
        if !ok {
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
        if !on {
            host.stream_stop().await.map_err(failed)?;
            return Ok(Value::Null);
        }
        let (server, key) = self.golive_ingest().await?;
        host.stream_start(server, key).await.map_err(failed)
    }

    pub async fn studio_stream_stats(&self) -> Result<Value, EngineError> {
        self.studio_host()?.stream_stats().await.map_err(failed)
    }

    pub async fn studio_preview(&self, rect: Option<PreviewRect>) -> Result<(), EngineError> {
        if let Some(r) = &rect {
            r.validate().map_err(EngineError::Invalid)?;
        }
        self.studio_host()?.preview(rect).await.map_err(failed)
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
    if message.contains("already a source") || message.contains("already a filter") {
        return refused(409, "studio_name_taken", message, "Pick another name.");
    }
    if message.contains("isn't here yet") {
        return refused(422, "studio_not_supported", message, "Use Reload, or OBS for now.");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Fake {
        rects: std::sync::Mutex<Vec<Option<PreviewRect>>>,
        acts: std::sync::Mutex<Vec<StudioAct>>,
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
            Ok(serde_json::json!({ "on": on }))
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
        let r = PreviewRect { x: 10.0, y: 20.0, w: 640.0, h: 360.0, vh: 800.0 };
        e.studio_preview(Some(r)).await.unwrap();
        e.studio_preview(None).await.unwrap();
        for bad in [
            PreviewRect { w: 0.0, ..r },
            PreviewRect { h: f64::NAN, ..r },
            PreviewRect { x: f64::INFINITY, ..r },
            PreviewRect { w: 99_999.0, ..r },
            PreviewRect { vh: -1.0, ..r },
        ] {
            assert!(matches!(e.studio_preview(Some(bad)).await.unwrap_err(), EngineError::Invalid(_)));
        }
        assert_eq!(*fake.rects.lock().unwrap(), vec![Some(r), None], "only good boxes reach the app");

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
                StudioAct::SetAudio { name: "Mic".into(), volume_db: Some(f32::NAN), muted: None, monitor: None },
                "volume_db",
            ),
            (
                StudioAct::SetAudio { name: "Mic".into(), volume_db: Some(40.0), muted: None, monitor: None },
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
        assert!(hk.check().is_ok(), "{:?}", hk.check());
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
            ("record_format", StudioSettings { record_format: "mkv".into(), ..d.clone() }),
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
