//! The engine (ENGINE.md E7's core): what libobs's video and audio threads do, on our parts.
//! It owns sources, scenes and outputs; a thread of its own makes a frame every 1/fps on the
//! host clock (sources updated, the scene on air drawn, sound mixed, then encoded to every
//! output), and keeps numbers on how it's doing. The caller changes things through `Engine`'s
//! methods, from any thread.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::AudioConfig;
use crate::mix::{Mixer, SourceSettings};
use crate::pipeline::{Pipeline, Tag};
use crate::source::image::ImageSource;
use crate::source::text::{TextRenderer, TextStyle};
use crate::video::{Frame, VideoConfig};
use crate::{
    AvError, Blend, Compositor, Content, Effect, Filter, Item, Nv12, Picture, Placement, Scene, Transition,
    TransitionKind,
};

/// What a source makes.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    Color {
        width: u32,
        height: u32,
        rgba: [u8; 4],
    },
    Image(PathBuf),
    Text {
        text: String,
        style: TextStyle,
    },
    /// A test tone (a beep of `beep_ms` every `every_ms`), for checks and line-ups.
    Tone {
        hz: f32,
        every_ms: u64,
        beep_ms: u64,
    },
    /// A display (macOS); with `sound`, what the Mac plays too (Ferrocast's own left out).
    #[cfg(target_os = "macos")]
    Screen {
        display: u32,
        sound: bool,
    },
    /// What the Mac plays, with no picture (OBS's Desktop Audio; Ferrocast's own left out).
    #[cfg(target_os = "macos")]
    DesktopSound,
    /// A camera (macOS), by its id.
    #[cfg(target_os = "macos")]
    Camera(String),
    /// A microphone (macOS), by its id.
    #[cfg(target_os = "macos")]
    Microphone(String),
    /// A video or sound file (macOS), looping or once.
    #[cfg(target_os = "macos")]
    Media {
        path: PathBuf,
        looping: bool,
    },
}

/// A source in a scene: which, where, and how.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneItem {
    pub source: u64,
    pub visible: bool,
    pub placement: Placement,
    pub filter: Filter,
    pub blend: Blend,
    pub effects: Vec<Effect>,
    /// 0 to 1.
    pub opacity: f32,
    /// Keyframed properties, timed from when the scene went on air.
    pub animation: Vec<Track>,
    /// Another scene shown here (then `source` is unused), drawn at the canvas's size.
    pub scene: Option<String>,
    /// With `scene`: that scene is a group, its items placed in a box of this size.
    pub group: Option<(u32, u32)>,
}

impl SceneItem {
    pub fn new(source: u64, placement: Placement) -> Self {
        Self {
            source,
            visible: true,
            placement,
            filter: Filter::Disable,
            blend: Blend::Normal,
            effects: Vec::new(),
            opacity: 1.0,
            animation: Vec::new(),
            scene: None,
            group: None,
        }
    }
}

/// How a keyframe is reached from the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ease {
    #[default]
    Linear,
    In,
    Out,
    InOut,
    /// Jumps there at the keyframe's time.
    Hold,
}

impl Ease {
    fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::In => t * t * t,
            Self::Out => 1.0 - (1.0 - t).powi(3),
            Self::InOut => crate::cubic_ease_in_out(t),
            Self::Hold => {
                if t >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// What a track animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prop {
    X,
    Y,
    ScaleX,
    ScaleY,
    /// Degrees.
    Rotation,
    Opacity,
}

/// A keyframe: at this time (ms from the scene going on air), this value, reached this way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    pub at_ms: u64,
    pub value: f32,
    pub ease: Ease,
}

/// One property's keyframes, in time order; with `repeat`, it starts over after the last.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub prop: Prop,
    pub keys: Vec<Key>,
    pub repeat: bool,
}

impl Track {
    /// The value `ms` in (None with no keyframes).
    pub fn value_at(&self, ms: u64) -> Option<f32> {
        let first = self.keys.first()?;
        let last = self.keys.last()?;
        let ms = if self.repeat && last.at_ms > 0 { ms % last.at_ms.max(1) } else { ms };
        if ms <= first.at_ms {
            return Some(first.value);
        }
        if ms >= last.at_ms {
            return Some(last.value);
        }
        let i = self.keys.iter().position(|k| k.at_ms > ms)?;
        let (a, b) = (self.keys[i - 1], self.keys[i]);
        let t = (ms - a.at_ms) as f32 / (b.at_ms - a.at_ms).max(1) as f32;
        Some(a.value + (b.value - a.value) * b.ease.apply(t))
    }
}

/// A placement and opacity with an item's tracks applied `ms` in.
fn animated(it: &SceneItem, ms: u64) -> (Placement, f32) {
    let (mut p, mut o) = (it.placement, it.opacity);
    for tr in &it.animation {
        let Some(v) = tr.value_at(ms) else { continue };
        match tr.prop {
            Prop::X => p.x = v,
            Prop::Y => p.y = v,
            Prop::ScaleX => p.scale_x = v,
            Prop::ScaleY => p.scale_y = v,
            Prop::Rotation => p.rotation = v,
            Prop::Opacity => o = v,
        }
    }
    (p, o.clamp(0.0, 1.0))
}

/// An output: where the encoded stream goes.
pub enum OutputKind {
    /// Tags for a stream (Go live's hub takes them: `Hub::video` / `Hub::audio`).
    Stream(Box<dyn FnMut(Tag) + Send>),
    /// A crash-safe MP4 (MKV when the name ends in `.mkv`).
    Record(PathBuf),
    /// A replay buffer: the last `seconds` kept in memory (at most `mb` megabytes, 0: no
    /// limit), saved with `Engine::replay_clip`.
    Replay { seconds: u32, mb: u32 },
}

impl std::fmt::Debug for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stream(_) => f.write_str("Stream"),
            Self::Record(p) => f.debug_tuple("Record").field(p).finish(),
            Self::Replay { seconds, mb } => f.debug_struct("Replay").field("seconds", seconds).field("mb", mb).finish(),
        }
    }
}

/// What an output shows: a canvas of its own size and frame rate, showing the scene on air or a
/// scene of its own, with one of the mixer's tracks. Many outputs can share the engine's
/// sources: a 1080p stream and a vertical recording at once, each drawn once a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// A scene's name; None follows what's on air (with its transitions).
    pub scene: Option<String>,
    /// Every how many engine frames it takes one (1: the engine's rate, 2: half…).
    pub every: u32,
    /// The mixer's track it sends (0 is track 1).
    pub track: usize,
    /// Drawn at this size first, then scaled to `width`×`height` with this filter (OBS's
    /// output resolution: a 720p stream from a 1080p canvas). None draws at `width`×`height`.
    pub scaled_from: Option<(u32, u32, Filter)>,
    /// More of the mixer's tracks a recording carries after `track`, each its own sound track
    /// in the file (OBS's recording tracks 2-6).
    pub more_tracks: Vec<usize>,
}

impl Canvas {
    /// The engine's own canvas: its size and rate, what's on air, track 1.
    pub fn main(config: EngineConfig) -> Self {
        Self {
            width: config.width,
            height: config.height,
            scene: None,
            every: 1,
            track: 0,
            scaled_from: None,
            more_tracks: Vec::new(),
        }
    }

    /// The engine's canvas scaled to `width`×`height` (bicubic, OBS's default downscale filter).
    pub fn scaled(config: EngineConfig, width: u32, height: u32) -> Self {
        let from = (width, height) != (config.width, config.height);
        Self {
            width,
            height,
            scaled_from: from.then_some((config.width, config.height, Filter::Bicubic)),
            ..Self::main(config)
        }
    }
}

/// A recording output's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordState {
    pub paused: bool,
    /// Bytes in the file being written now.
    pub bytes: u64,
    /// A split waits for its first frame (the next file isn't written to yet).
    pub splitting: bool,
}

/// How the engine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    pub frames: u64,
    /// Frames not made on time (the loop fell behind and skipped them).
    pub skipped: u64,
    /// How long the last frame took to make, and the longest so far, in ms.
    pub frame_ms: f32,
    pub worst_ms: f32,
}

/// Settings fixed for the engine's life.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sample_rate: u32,
    /// How far behind the clock sound is mixed, so late pieces still make it.
    pub audio_latency_ms: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30, sample_rate: 48_000, audio_latency_ms: 40 }
    }
}

enum Live {
    Still,
    Image {
        src: ImageSource,
        since: Instant,
        last: Option<usize>,
    },
    Tone {
        hz: f32,
        every_ns: u64,
        beep_ns: u64,
        pushed_to: Option<u64>,
    },
    #[cfg(target_os = "macos")]
    Screen(crate::screen::ScreenCapture),
    #[cfg(target_os = "macos")]
    Camera(crate::camera::Capture),
    #[cfg(target_os = "macos")]
    Microphone(crate::camera::Capture),
    #[cfg(target_os = "macos")]
    Media(crate::media::Media),
}

struct Src {
    live: Live,
    /// The picture's size once there is one, and whether it's premultiplied.
    size: Option<(u32, u32, bool)>,
}

struct State {
    sources: BTreeMap<u64, Src>,
    scenes: BTreeMap<String, Vec<SceneItem>>,
    on_air: Option<String>,
    /// A transition under way: from which scene, how, when it began and how long it lasts (ns).
    moving: Option<(String, TransitionKind, u64, u64)>,
    /// A stinger under way: the scene it came from, when it started, when the scene changes
    /// underneath, and when it ends (ns).
    #[cfg(target_os = "macos")]
    stinger: Option<(String, u64, u64, u64)>,
    /// When each scene last went on air (its keyframes' time 0).
    started: std::collections::HashMap<String, u64>,
    /// Scenes that are OBS groups (shown inside scenes, not listed as scenes).
    groups: Vec<String>,
    /// Sources heard whatever is on air (OBS's Desktop Audio and Mic/Aux channels).
    globals: std::collections::HashSet<u64>,
    /// Sources a collection brought, by name: their id here and what they are (kept running
    /// when the collection is opened again unchanged).
    opened: HashMap<String, (u64, SourceKind)>,
    outputs: Vec<(u64, Pipeline, OutputKind, Canvas)>,
    next_output: u64,
    video: VideoConfig,
    audio: AudioConfig,
    /// Where monitored sound is played, when monitoring is on.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor: Option<crate::monitor::Feeder>,
    stats: Stats,
    error: Option<String>,
}

/// The engine. Dropping it stops its thread and closes its outputs.
pub struct Engine {
    config: EngineConfig,
    /// The device monitored sound plays on (it stays on this side: its feeder goes to the loop).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor: Mutex<Option<crate::monitor::Monitor>>,
    state: Arc<Mutex<State>>,
    gpu: Arc<Compositor>,
    mixer: Arc<Mutex<Mixer>>,
    text: Mutex<TextRenderer>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("config", &self.config).finish_non_exhaustive()
    }
}

/// The host clock, nanoseconds (the same clock capture times use on macOS).
pub fn now_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        crate::apple::host_ns()
    }
    #[cfg(not(target_os = "macos"))]
    {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1_000_000_000
    }
}

impl Engine {
    /// Starts the engine and its frame loop.
    pub fn new(config: EngineConfig, video: VideoConfig, audio: AudioConfig) -> Result<Self, AvError> {
        if config.fps == 0 || config.fps > 240 || !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) {
            return Err(AvError::BadSize(config.width, config.height));
        }
        let gpu = Arc::new(Compositor::new()?);
        let mixer =
            Arc::new(Mutex::new(Mixer::new(config.sample_rate, u64::from(config.audio_latency_ms) * 1_000_000)));
        let state = Arc::new(Mutex::new(State {
            sources: BTreeMap::new(),
            scenes: BTreeMap::new(),
            on_air: None,
            moving: None,
            #[cfg(target_os = "macos")]
            stinger: None,
            started: std::collections::HashMap::new(),
            groups: Vec::new(),
            globals: std::collections::HashSet::new(),
            opened: HashMap::new(),
            outputs: Vec::new(),
            next_output: 1,
            video: VideoConfig { width: config.width, height: config.height, fps_num: config.fps, fps_den: 1, ..video },
            audio: AudioConfig { sample_rate: config.sample_rate, ..audio },
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: None,
            stats: Stats::default(),
            error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, gpu, mixer, stop) = (state.clone(), gpu.clone(), mixer.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-engine".into())
                .spawn(move || run(config, &state, &gpu, &mixer, &stop))
                .map_err(|e| AvError::Source(format!("the engine's thread didn't start ({e})")))?
        };
        Ok(Self {
            config,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: Mutex::new(None),
            state,
            gpu,
            mixer,
            text: Mutex::new(TextRenderer::new()),
            stop,
            thread: Some(thread),
        })
    }

    pub fn config(&self) -> EngineConfig {
        self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds (or replaces) a source. Errors say what's wrong (a missing file, a camera in use).
    pub fn set_source(&self, id: u64, kind: SourceKind) -> Result<(), AvError> {
        let (live, picture) = match kind {
            SourceKind::Color { width, height, rgba } => {
                (Live::Still, Some(Picture::solid(width.max(1), height.max(1), rgba)))
            }
            SourceKind::Image(path) => {
                let src = ImageSource::open(&path)?;
                let first = src.frame_at(Duration::ZERO).clone();
                (Live::Image { src, since: Instant::now(), last: Some(0) }, Some(first))
            }
            SourceKind::Text { text, style } => {
                let p = self.text.lock().unwrap_or_else(|e| e.into_inner()).render(&text, &style);
                (Live::Still, Some(p))
            }
            SourceKind::Tone { hz, every_ms, beep_ms } => {
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                (
                    Live::Tone {
                        hz,
                        every_ns: every_ms.max(1) * 1_000_000,
                        beep_ns: beep_ms * 1_000_000,
                        pushed_to: None,
                    },
                    None,
                )
            }
            #[cfg(target_os = "macos")]
            SourceKind::Screen { display, sound } => {
                let c = crate::screen::ScreenCapture::start_with(
                    crate::screen::Target::Display(display),
                    self.config.width,
                    self.config.height,
                    self.config.fps,
                    true,
                    sound,
                )?;
                if sound {
                    self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                }
                (Live::Screen(c), None)
            }
            #[cfg(target_os = "macos")]
            SourceKind::DesktopSound => {
                // ScreenCaptureKit gives the sound with a display's picture: the smallest, slowest one.
                let display = crate::screen::shareable()?
                    .0
                    .first()
                    .map(|d| d.id)
                    .ok_or_else(|| AvError::Source("there's no display to take the Mac's sound with".into()))?;
                let c = crate::screen::ScreenCapture::start_with(
                    crate::screen::Target::Display(display),
                    2,
                    2,
                    1,
                    false,
                    true,
                )?;
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                (Live::Screen(c), None)
            }
            #[cfg(target_os = "macos")]
            SourceKind::Camera(dev) => (Live::Camera(crate::camera::Capture::camera(&dev)?), None),
            #[cfg(target_os = "macos")]
            SourceKind::Microphone(dev) => {
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                (Live::Microphone(crate::camera::Capture::microphone(&dev)?), None)
            }
            #[cfg(target_os = "macos")]
            SourceKind::Media { path, looping } => {
                let m = crate::media::Media::open(&path, looping)?;
                if m.has_audio {
                    self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                }
                (Live::Media(m), None)
            }
        };
        let size = match &picture {
            Some(p) => {
                self.gpu.upload(id, p)?;
                Some((p.width, p.height, p.premultiplied))
            }
            None => None,
        };
        self.lock().sources.insert(id, Src { live, size });
        Ok(())
    }

    pub fn remove_source(&self, id: u64) {
        {
            let mut st = self.lock();
            st.sources.remove(&id);
            st.globals.remove(&id);
        }
        self.gpu.forget(id);
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    /// A source's sound settings (volume, mute, balance, tracks…), for sources with sound.
    pub fn set_sound(&self, id: u64, settings: SourceSettings) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, settings);
    }

    /// Plays monitored sound (sources set to `Monitoring::Only` or `AndOutput`) on `device` (a
    /// name from `monitor::devices()`; None is the system's), or stops with `on` false.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_monitoring(&self, on: bool, device: Option<&str>) -> Result<(), AvError> {
        let m = if on { Some(crate::monitor::Monitor::start(device, self.config.sample_rate)?) } else { None };
        self.lock().monitor = m.as_ref().map(|m| m.feeder());
        *self.monitor.lock().unwrap_or_else(|e| e.into_inner()) = m;
        Ok(())
    }

    /// Monitoring's device so far: times it ran short of sound, and samples played.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn monitoring_stats(&self) -> Option<(u64, u64)> {
        self.monitor.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.stats())
    }

    /// A source's sound filters, in order (gate, compressor, EQ…), replacing any before.
    pub fn set_sound_effects(&self, id: u64, effects: &[crate::afx::AudioEffect]) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_effects(id, effects);
    }

    /// A source's sound levels over the last block (for meters).
    pub fn levels(&self, id: u64) -> Option<crate::mix::Levels> {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).levels(id)
    }

    /// A scene's items, bottom first (replacing what it had).
    pub fn set_scene(&self, name: &str, items: Vec<SceneItem>) {
        self.lock().scenes.insert(name.to_string(), items);
    }

    /// Opens an OBS scene collection (the JSON OBS and Ferrocast's Studio save): its sources,
    /// scenes and the scene on air replace what the engine had. Returns what couldn't come over
    /// (sources or filters it doesn't run yet, files that are gone).
    /// Opened again (the Studio changed), a source with the same name and settings keeps
    /// running: a camera doesn't blink when something else moves.
    pub fn load_obs(&self, doc: &serde_json::Value) -> Vec<String> {
        let mut imported = crate::obs::import(doc, (self.config.width, self.config.height));
        let mut skipped = std::mem::take(&mut imported.skipped);
        // Which imported sources are already here, running as they should; the rest get ids
        // after every id in use.
        let (keep, ids) = {
            let st = self.lock();
            let mut next = st.sources.keys().max().map_or(1, |m| m + 1);
            let mut ids: HashMap<u64, u64> = HashMap::new();
            let mut keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
            for src in &imported.sources {
                match st.opened.get(&src.name) {
                    Some((id, kind)) if *kind == src.kind && st.sources.contains_key(id) && !keep.contains(id) => {
                        ids.insert(src.id, *id);
                        keep.insert(*id);
                    }
                    _ => {
                        ids.insert(src.id, next);
                        next += 1;
                    }
                }
            }
            (keep, ids)
        };
        let old: Vec<u64> = self.lock().sources.keys().copied().filter(|id| !keep.contains(id)).collect();
        for id in old {
            self.remove_source(id);
        }
        let to = |id: u64| ids.get(&id).copied().unwrap_or(id);
        for (_, items) in &mut imported.scenes {
            for it in items.iter_mut() {
                it.source = to(it.source);
                for e in &mut it.effects {
                    if let Effect::Mask { image, .. } = e {
                        *image = to(*image);
                    }
                }
            }
        }
        {
            // A fresh start: nothing on air, every scene's clock unset.
            let mut st = self.lock();
            st.scenes.clear();
            st.started.clear();
            st.on_air = None;
            st.moving = None;
            st.groups = imported.groups;
            st.globals.clear();
            st.opened.clear();
        }
        for mut s in imported.sources {
            s.id = to(s.id);
            let started = if keep.contains(&s.id) { Ok(()) } else { self.set_source(s.id, s.kind.clone()) };
            match started {
                Ok(()) => {
                    if let Some(sound) = s.sound {
                        self.set_sound(s.id, sound);
                        self.set_sound_effects(s.id, &s.sound_effects);
                    }
                    self.lock().opened.insert(s.name.clone(), (s.id, s.kind));
                }
                Err(e) => skipped.push(format!("{}: {e}", s.name)),
            }
            if s.global {
                self.set_global(s.id, true);
            }
        }
        for (name, items) in imported.scenes {
            self.set_scene(&name, items);
        }
        let first = self.scene_names().into_iter().next();
        if let Some(cur) = imported.current.or(first) {
            let _ = self.go(&cur);
        }
        skipped
    }

    /// Makes a source heard whatever is on air (OBS's Desktop Audio and Mic/Aux), or not.
    pub fn set_global(&self, id: u64, global: bool) {
        let mut st = self.lock();
        if global {
            st.globals.insert(id);
        } else {
            st.globals.remove(&id);
        }
    }

    /// The scenes there are (groups left out).
    pub fn scene_names(&self) -> Vec<String> {
        let st = self.lock();
        st.scenes.keys().filter(|n| !st.groups.contains(n)).cloned().collect()
    }

    /// Puts a scene on air (a cut).
    pub fn go(&self, name: &str) -> Result<(), AvError> {
        self.go_with(name, TransitionKind::Cut, 0)
    }

    /// Puts a scene on air through a transition lasting `ms`.
    pub fn go_with(&self, name: &str, kind: TransitionKind, ms: u64) -> Result<(), AvError> {
        let mut st = self.lock();
        if !st.scenes.contains_key(name) {
            return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
        }
        let now = now_ns();
        let from = st.on_air.replace(name.to_string());
        if from.as_deref() != Some(name) {
            st.started.insert(name.to_string(), now);
        }
        st.moving = match from {
            Some(f) if kind != TransitionKind::Cut && ms > 0 && f != name => Some((f, kind, now, ms * 1_000_000)),
            _ => None,
        };
        Ok(())
    }

    /// Puts a scene on air behind a stinger: `video` plays over everything, and the scene
    /// changes underneath `cut_ms` in. Its sound plays too.
    #[cfg(target_os = "macos")]
    pub fn go_stinger(&self, name: &str, video: &std::path::Path, cut_ms: u64) -> Result<(), AvError> {
        if !self.lock().scenes.contains_key(name) {
            return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
        }
        let m = crate::media::Media::open(video, false)?;
        if !m.has_video {
            return Err(AvError::Source("a stinger needs a video with a picture".into()));
        }
        let dur = m.duration_ns.max(1);
        if m.has_audio {
            self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(STINGER, SourceSettings::default());
        }
        let now = now_ns();
        let mut st = self.lock();
        st.sources.insert(STINGER, Src { live: Live::Media(m), size: None });
        let from = st.on_air.replace(name.to_string()).unwrap_or_default();
        st.started.insert(name.to_string(), now + cut_ms * 1_000_000);
        st.moving = None;
        // The media's first frame shows about 100 ms after it opens.
        let start = now + 100_000_000;
        st.stinger = Some((from, start, start + cut_ms * 1_000_000, start + dur));
        Ok(())
    }

    /// Whether a transition (or stinger) is under way.
    pub fn transitioning(&self) -> bool {
        let st = self.lock();
        #[cfg(target_os = "macos")]
        if st.stinger.is_some() {
            return true;
        }
        st.moving.is_some()
    }

    /// Starts an output (stream or recording) on the engine's own canvas → its id.
    pub fn start_output(&self, kind: OutputKind) -> Result<u64, AvError> {
        self.start_output_on(kind, Canvas::main(self.config))
    }

    /// Starts an output on a canvas of its own (another size, scene, rate or track).
    pub fn start_output_on(&self, kind: OutputKind, canvas: Canvas) -> Result<u64, AvError> {
        if canvas.every == 0
            || canvas.track >= crate::mix::TRACKS
            || canvas.more_tracks.iter().any(|t| *t >= crate::mix::TRACKS)
            || !canvas.width.is_multiple_of(2)
            || !canvas.height.is_multiple_of(2)
        {
            return Err(AvError::Source(
                "a canvas needs even sides, a rate divisor of 1 or more, and a track from 1 to 6".into(),
            ));
        }
        let mut st = self.lock();
        if let Some(name) = &canvas.scene
            && !st.scenes.contains_key(name)
        {
            return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
        }
        let record = match &kind {
            OutputKind::Record(p) => Some(p.clone()),
            OutputKind::Stream(_) | OutputKind::Replay { .. } => None,
        };
        let video = VideoConfig {
            width: canvas.width,
            height: canvas.height,
            fps_num: (self.config.fps / canvas.every).max(1),
            fps_den: 1,
            ..st.video
        };
        // Extra tracks only mean something in a file (a recording, or a saved replay).
        let replay = match &kind {
            OutputKind::Replay { seconds, mb } => Some((*seconds, *mb)),
            _ => None,
        };
        let more = if record.is_some() || replay.is_some() { canvas.more_tracks.len() } else { 0 };
        let mut pipe = Pipeline::with_tracks(video, st.audio, record.as_deref(), more)?;
        if let Some((seconds, mb)) = replay {
            pipe.keep_replay(seconds, mb);
        }
        let id = st.next_output;
        st.next_output += 1;
        st.outputs.push((id, pipe, kind, canvas));
        Ok(id)
    }

    /// A new video bitrate for an output from now on; false when its encoder takes it only at
    /// the next start (FFmpeg's).
    pub fn set_output_kbps(&self, id: u64, kbps: u32) -> Result<bool, AvError> {
        self.with_output(id, |pipe| pipe.set_video_kbps(kbps))
    }

    /// Shows another scene on an output with a canvas of its own (None: what's on air), cut.
    pub fn set_output_scene(&self, id: u64, scene: Option<String>) -> Result<(), AvError> {
        let mut st = self.lock();
        if let Some(name) = &scene
            && !st.scenes.contains_key(name)
        {
            return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
        }
        let (.., canvas) = st
            .outputs
            .iter_mut()
            .find(|(i, ..)| *i == id)
            .ok_or_else(|| AvError::Source(format!("there's no output {id}")))?;
        canvas.scene = scene;
        Ok(())
    }

    /// What a replay buffer output holds now, to write with `ReplayClip::write` (off this
    /// thread: it can be tens of megabytes).
    pub fn replay_clip(&self, id: u64) -> Result<crate::pipeline::ReplayClip, AvError> {
        self.with_output(id, |pipe| {
            pipe.replay_clip().ok_or_else(|| AvError::Encoder("the replay buffer has nothing kept yet".into()))
        })
    }

    /// Pauses or resumes an output (a recording, as OBS pauses one): no gap in the file.
    pub fn pause_output(&self, id: u64, on: bool) -> Result<(), AvError> {
        self.with_output(id, |pipe| {
            pipe.pause(on, now_ns());
            Ok(())
        })
    }

    /// Goes on recording into a new file at `path` from the next frame ("Split recording").
    pub fn split_output(&self, id: u64, path: &std::path::Path) -> Result<(), AvError> {
        self.with_output(id, |pipe| pipe.split(path))
    }

    /// An output's recording now.
    pub fn output_record_state(&self, id: u64) -> Result<RecordState, AvError> {
        self.with_output(id, |pipe| {
            Ok(RecordState { paused: pipe.paused(), bytes: pipe.recorded_bytes(), splitting: pipe.splitting() })
        })
    }

    fn with_output<T>(&self, id: u64, f: impl FnOnce(&mut Pipeline) -> Result<T, AvError>) -> Result<T, AvError> {
        let mut st = self.lock();
        let (_, pipe, ..) = st
            .outputs
            .iter_mut()
            .find(|(i, ..)| *i == id)
            .ok_or_else(|| AvError::Source(format!("there's no output {id}")))?;
        f(pipe)
    }

    /// Stops an output: what's in its encoders is sent and a recording is closed.
    pub fn stop_output(&self, id: u64) -> Result<(), AvError> {
        let taken = {
            let mut st = self.lock();
            st.outputs.iter().position(|(i, ..)| *i == id).map(|at| st.outputs.remove(at))
        };
        if let Some((_, pipe, mut kind, _)) = taken {
            let tags = pipe.finish()?;
            if let OutputKind::Stream(send) = &mut kind {
                for t in tags {
                    send(t);
                }
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    /// What stopped the loop or an output, if anything did.
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// Any scene drawn now at the engine's size, RGBA: Studio Mode's preview, the Scenes grid.
    pub fn snapshot_scene(&self, name: &str) -> Result<Vec<u8>, AvError> {
        let scene = {
            let st = self.lock();
            if !st.scenes.contains_key(name) {
                return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
            }
            build(&st, self.config.width, self.config.height, Some(name), now_ns())
        };
        self.gpu.render(&scene)
    }

    /// The scene on air drawn now, RGBA (a preview or a screenshot).
    pub fn snapshot(&self) -> Result<Vec<u8>, AvError> {
        let frame = {
            let st = self.lock();
            what_to_draw(&st, self.config.width, self.config.height, None, now_ns())
        };
        match frame {
            Draw::Scene(s) => self.gpu.render(&s),
            Draw::Moving(t) => self.gpu.render_transition(&t),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let ids: Vec<u64> = self.lock().outputs.iter().map(|(i, ..)| *i).collect();
        for id in ids {
            let _ = self.stop_output(id);
        }
    }
}

/// What a frame shows: the scene on air, or two scenes mid-transition.
enum Draw {
    Scene(Scene),
    Moving(Box<Transition>),
}

/// The stinger's own source id (it plays as a source while it runs).
#[cfg(target_os = "macos")]
const STINGER: u64 = u64::MAX - 1;

/// A canvas's frame: its own scene, or what's on air (mid-transition, perhaps).
fn what_to_draw(st: &State, width: u32, height: u32, scene: Option<&str>, ts: u64) -> Draw {
    if let Some(name) = scene {
        return Draw::Scene(build(st, width, height, Some(name), ts));
    }
    #[cfg(target_os = "macos")]
    if let Some((from, _, cut, _)) = &st.stinger {
        let under = if ts < *cut { Some(from.as_str()) } else { st.on_air.as_deref() };
        let mut s = build(st, width, height, under, ts);
        // Over everything, stretched to the canvas.
        let over = SceneItem::new(
            STINGER,
            Placement {
                bounds: crate::Bounds::Stretch,
                bounds_width: width as f32,
                bounds_height: height as f32,
                ..Default::default()
            },
        );
        s.items.extend(item_of(st, &over, 0, (width, height)));
        return Draw::Scene(s);
    }
    match &st.moving {
        Some((from, TransitionKind::Move, start, dur)) if ts < start + dur => {
            let t = crate::cubic_ease_in_out(ts.saturating_sub(*start) as f32 / *dur as f32);
            Draw::Scene(moved(st, width, height, from, st.on_air.as_deref().unwrap_or_default(), t, ts))
        }
        Some((from, kind, start, dur)) if ts < start + dur => {
            let t = ts.saturating_sub(*start) as f32 / *dur as f32;
            let to = build(st, width, height, st.on_air.as_deref(), ts);
            Draw::Moving(Box::new(Transition {
                kind: kind.clone(),
                t,
                from: build(st, width, height, Some(from), ts),
                to,
            }))
        }
        _ => Draw::Scene(build(st, width, height, st.on_air.as_deref(), ts)),
    }
}

/// A scene drawn at its own size, then stretched to `width`×`height` with `filter`.
fn scaled(scene: Scene, width: u32, height: u32, filter: Filter) -> Scene {
    let item = Item {
        content: Content::Scene(scene),
        effects: Vec::new(),
        placement: Placement {
            bounds: crate::Bounds::Stretch,
            bounds_width: width as f32,
            bounds_height: height as f32,
            ..Default::default()
        },
        filter,
        blend: crate::Blend::Normal,
        srgb_off: false,
        linear: true,
        opacity: 1.0,
    };
    Scene { width, height, items: vec![item] }
}

/// The sources heard now: the global ones, and every source shown (scenes inside scenes and
/// groups too) by the scene on air, the one a transition or stinger leaves, and each output's
/// own scene; the stinger itself.
fn heard(st: &State) -> std::collections::HashSet<u64> {
    fn walk(st: &State, scene: &str, depth: u32, out: &mut std::collections::HashSet<u64>) {
        let Some(items) = st.scenes.get(scene) else { return };
        for it in items.iter().filter(|i| i.visible) {
            match &it.scene {
                Some(inner) if depth < MAX_NESTING => walk(st, inner, depth + 1, out),
                Some(_) => {}
                None => {
                    out.insert(it.source);
                }
            }
        }
    }
    let mut out = st.globals.clone();
    let mut roots: Vec<&str> = Vec::new();
    roots.extend(st.on_air.as_deref());
    roots.extend(st.moving.as_ref().map(|(from, ..)| from.as_str()));
    #[cfg(target_os = "macos")]
    roots.extend(st.stinger.as_ref().map(|(from, ..)| from.as_str()));
    roots.extend(st.outputs.iter().filter_map(|(.., c)| c.scene.as_deref()));
    for r in roots {
        walk(st, r, 0, &mut out);
    }
    #[cfg(target_os = "macos")]
    out.insert(STINGER);
    out
}

/// An item as the compositor takes it, `ms` into its scene; None without a picture yet.
fn item_of(st: &State, it: &SceneItem, ms: u64, canvas: (u32, u32)) -> Option<Item> {
    item_at(st, it, ms, 0, canvas)
}

/// Scenes inside scenes go at most this deep (a scene showing itself stops there).
const MAX_NESTING: u32 = 8;

fn item_at(st: &State, it: &SceneItem, ms: u64, depth: u32, canvas: (u32, u32)) -> Option<Item> {
    let (placement, opacity) = animated(it, ms);
    let content = match &it.scene {
        Some(name) if depth < MAX_NESTING => {
            let scene = st.scenes.get(name)?;
            // Its items keep the time of the scene showing it (as OBS: a nested scene plays along).
            let items =
                scene.iter().filter(|i| i.visible).filter_map(|i| item_at(st, i, ms, depth + 1, canvas)).collect();
            match it.group {
                Some((width, height)) => Content::Group { width, height, items },
                None => Content::Scene(Scene { width: canvas.0, height: canvas.1, items }),
            }
        }
        Some(_) => return None,
        None => {
            let src = st.sources.get(&it.source)?;
            let (width, height, premultiplied) = src.size?;
            Content::Uploaded { id: it.source, width, height, premultiplied }
        }
    };
    // Filters that move with time are told how long the scene has been on; a mask whose picture
    // hasn't loaded (or won't) is left out rather than stopping the frame.
    let effects = it
        .effects
        .iter()
        .filter(|e| match e {
            Effect::Mask { image, .. } => st.sources.get(image).is_some_and(|s| s.size.is_some()),
            _ => true,
        })
        .map(|e| {
            let mut e = e.clone();
            if let Effect::Scroll { elapsed_ms, .. } = &mut e {
                *elapsed_ms = ms;
            }
            e
        })
        .collect();
    Some(Item {
        content,
        effects,
        placement,
        filter: it.filter,
        blend: it.blend,
        srgb_off: false,
        linear: true,
        opacity,
    })
}

fn scene_ms(st: &State, name: &str, ts: u64) -> u64 {
    st.started.get(name).map_or(0, |s| ts.saturating_sub(*s) / 1_000_000)
}

/// A scene as the compositor takes it at `ts` (sources without a picture yet are left out).
fn build(st: &State, width: u32, height: u32, name: Option<&str>, ts: u64) -> Scene {
    let mut items = Vec::new();
    if let Some(name) = name
        && let Some(scene) = st.scenes.get(name)
    {
        let ms = scene_ms(st, name, ts);
        items.extend(scene.iter().filter(|i| i.visible).filter_map(|it| item_of(st, it, ms, (width, height))));
    }
    Scene { width, height, items }
}

/// The Move transition `t` of the way (eased): a source in both scenes glides from its old
/// place to its new one; one only in the old scene fades out underneath, one only in the new
/// fades in.
fn moved(st: &State, width: u32, height: u32, from: &str, to: &str, t: f32, ts: u64) -> Scene {
    let (empty, empty2) = (Vec::new(), Vec::new());
    let a = st.scenes.get(from).unwrap_or(&empty);
    let b = st.scenes.get(to).unwrap_or(&empty2);
    let (ma, mb) = (scene_ms(st, from, ts), scene_ms(st, to, ts));
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let canvas = (width, height);
    // The same thing in both scenes: one source, or one nested scene.
    let same = |x: &SceneItem, y: &SceneItem| x.source == y.source && x.scene == y.scene;
    let mut items = Vec::new();
    for old in a.iter().filter(|i| i.visible && !b.iter().any(|n| n.visible && same(n, i))) {
        if let Some(mut it) = item_of(st, old, ma, canvas) {
            it.opacity *= 1.0 - t;
            items.push(it);
        }
    }
    for new in b.iter().filter(|i| i.visible) {
        let Some(mut it) = item_of(st, new, mb, canvas) else { continue };
        match a.iter().find(|o| o.visible && same(o, new)).and_then(|o| item_of(st, o, ma, canvas)) {
            Some(o) => {
                let (p, q) = (o.placement, it.placement);
                it.placement.x = lerp(p.x, q.x);
                it.placement.y = lerp(p.y, q.y);
                it.placement.scale_x = lerp(p.scale_x, q.scale_x);
                it.placement.scale_y = lerp(p.scale_y, q.scale_y);
                it.placement.rotation = lerp(p.rotation, q.rotation);
                it.placement.bounds_width = lerp(p.bounds_width, q.bounds_width);
                it.placement.bounds_height = lerp(p.bounds_height, q.bounds_height);
                it.opacity = lerp(o.opacity, it.opacity);
            }
            None => it.opacity *= t,
        }
        items.push(it);
    }
    Scene { width, height, items }
}

/// The frame loop.
fn run(config: EngineConfig, state: &Mutex<State>, gpu: &Compositor, mixer: &Mutex<Mixer>, stop: &AtomicBool) {
    let frame_ns = 1_000_000_000 / u64::from(config.fps);
    let mut next = now_ns();
    let rate = config.sample_rate;
    while !stop.load(Ordering::Acquire) {
        let now = now_ns();
        if now < next {
            std::thread::sleep(Duration::from_nanos((next - now).min(5_000_000)));
            continue;
        }
        // Fell behind by more than a frame: skip ahead instead of rushing (as libobs does).
        let behind = (now - next) / frame_ns;
        let ts = next + behind * frame_ns;
        next = ts + frame_ns;
        let started = Instant::now();

        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.stats.skipped += behind;
        let mut mx = mixer.lock().unwrap_or_else(|e| e.into_inner());
        // Only what's on air is heard (and monitored), as libobs mixes its program's tree; the
        // rest still has its sound taken, so it doesn't pile up.
        let heard = heard(&st);
        // Sources: new pictures to the GPU, new sound to the mixer.
        let mut uploads = Vec::new();
        for (id, src) in st.sources.iter_mut() {
            match &mut src.live {
                Live::Still => {}
                Live::Image { src: img, since, last } => {
                    if img.animated() {
                        let p = img.frame_at(since.elapsed());
                        uploads.push((*id, p.clone()));
                        *last = None;
                    }
                }
                Live::Tone { hz, every_ns, beep_ns, pushed_to } => {
                    // Beeps scheduled up to half a second ahead, on the frame clock.
                    let mut at = pushed_to.unwrap_or(ts - ts % *every_ns);
                    while at < ts + 500_000_000 {
                        let n = (u128::from(*beep_ns) * u128::from(rate) / 1_000_000_000) as usize;
                        let pcm: Vec<f32> = (0..n)
                            .flat_map(|i| {
                                let v = (i as f32 * *hz * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                                [v, v]
                            })
                            .collect();
                        if heard.contains(id) {
                            mx.push(*id, &pcm, 2, rate, at);
                        }
                        at += *every_ns;
                    }
                    *pushed_to = Some(at);
                }
                #[cfg(target_os = "macos")]
                Live::Screen(cap) => {
                    if let Some(p) = cap.take() {
                        uploads.push((*id, p));
                    }
                    for (pcm, t) in cap.take_sound() {
                        if heard.contains(id) {
                            mx.push(*id, &pcm, 2, 48_000, t);
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                Live::Camera(cap) => {
                    if let Some((p, _)) = cap.take_frame() {
                        uploads.push((*id, p));
                    }
                }
                #[cfg(target_os = "macos")]
                Live::Microphone(cap) => {
                    for (pcm, t) in cap.take_sound() {
                        if heard.contains(id) {
                            mx.push(*id, &pcm, 2, 48_000, t);
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                Live::Media(m) => {
                    if let Some(p) = m.frame_at(ts) {
                        uploads.push((*id, p));
                    }
                    for (pcm, t) in m.take_sound() {
                        if heard.contains(id) {
                            mx.push(*id, &pcm, 2, 48_000, t);
                        }
                    }
                }
            }
        }
        for (id, p) in uploads {
            if gpu.upload(id, &p).is_ok()
                && let Some(s) = st.sources.get_mut(&id)
            {
                s.size = Some((p.width, p.height, p.premultiplied));
            }
        }
        let blocks = mx.mix_until(now_ns());
        drop(mx);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(f) = &st.monitor {
            for b in &blocks {
                f.push(&b.monitor);
            }
        }

        #[cfg(target_os = "macos")]
        if st.stinger.as_ref().is_some_and(|(.., end)| ts >= *end) {
            st.stinger = None;
            st.sources.remove(&STINGER);
            gpu.forget(STINGER);
            mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(STINGER);
        }
        if st.moving.as_ref().is_some_and(|(_, _, start, dur)| ts >= start + dur) {
            st.moving = None;
        }
        if !st.outputs.is_empty() {
            // Each distinct canvas drawn once this frame (if it takes this one), then every output
            // on it encodes the same pixels.
            let index = st.stats.frames;
            let mut drawn: Vec<(Canvas, Result<Nv12, String>)> = Vec::new();
            for (.., canvas) in &st.outputs {
                if !index.is_multiple_of(u64::from(canvas.every)) || drawn.iter().any(|(c, _)| c == canvas) {
                    continue;
                }
                let (w, h) = canvas.scaled_from.map_or((canvas.width, canvas.height), |(w, h, _)| (w, h));
                let draw = match (what_to_draw(&st, w, h, canvas.scene.as_deref(), ts), canvas.scaled_from) {
                    (d, None) => d,
                    (Draw::Scene(s), Some((.., f))) => Draw::Scene(scaled(s, canvas.width, canvas.height, f)),
                    (Draw::Moving(mut t), Some((.., f))) => {
                        t.from = scaled(t.from, canvas.width, canvas.height, f);
                        t.to = scaled(t.to, canvas.width, canvas.height, f);
                        Draw::Moving(t)
                    }
                };
                let frame = match draw {
                    Draw::Scene(s) => gpu.render_nv12(&s),
                    Draw::Moving(t) => gpu.render_transition_nv12(&t),
                };
                drawn.push((canvas.clone(), frame.map_err(|e| e.to_string())));
            }
            let mut failed = None;
            for (_, pipe, kind, canvas) in st.outputs.iter_mut() {
                let mut tags = Vec::new();
                if let Some((_, frame)) = drawn.iter().find(|(c, _)| c == canvas) {
                    match frame {
                        Ok(f) => {
                            let w = canvas.width as usize;
                            match pipe.video(Frame::Nv12 { y: &f.y, y_stride: w, uv: &f.uv, uv_stride: w }, ts) {
                                Ok(t) => tags = t,
                                Err(e) => failed = Some(e.to_string()),
                            }
                        }
                        Err(e) => failed = Some(e.clone()),
                    }
                }
                for b in &blocks {
                    match pipe.audio(&b.tracks[canvas.track], b.ts_ns) {
                        Ok(t) => tags.extend(t),
                        Err(e) => failed = Some(e.to_string()),
                    }
                    for (i, t) in canvas.more_tracks.iter().enumerate() {
                        if let Err(e) = pipe.audio_extra(i, &b.tracks[*t], b.ts_ns) {
                            failed = Some(e.to_string());
                        }
                    }
                }
                if let OutputKind::Stream(send) = kind {
                    for t in tags {
                        send(t);
                    }
                }
            }
            if failed.is_some() {
                st.error = failed;
            }
        }
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        st.stats.frames += 1;
        st.stats.frame_ms = ms;
        st.stats.worst_ms = st.stats.worst_ms.max(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioConfig;
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    fn engine() -> Option<Engine> {
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 640,
            height: 360,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 2000,
            rate: RateControl::Cbr,
            keyframe_secs: 1,
            software: false,
        };
        let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        Engine::new(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() }, video, audio).ok()
    }

    #[test]
    fn scenes_draw_what_is_on_air() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 100, height: 100, rgba: [0, 0, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene(
            "A",
            vec![
                SceneItem::new(1, Placement::default()),
                SceneItem::new(2, Placement { x: 10.0, y: 10.0, ..Default::default() }),
            ],
        );
        e.set_scene("B", vec![SceneItem::new(2, Placement::default())]);
        assert!(e.go("C").is_err());
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        let px = e.snapshot().unwrap_or_default();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        let i = (50 * 640 + 50) * 4;
        assert_eq!(&px[i..i + 4], &[0, 0, 255, 255]);
        e.go("B").unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[0, 0, 255, 255]);
        // Studio Mode: A previewed while B is on air.
        assert_eq!(&e.snapshot_scene("A").unwrap_or_default()[0..4], &[255, 0, 0, 255]);
        assert!(e.snapshot_scene("Nope").is_err());
    }

    #[test]
    fn a_mask_without_its_picture_is_left_out() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 255, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut it = SceneItem::new(1, Placement::default());
        it.effects = vec![Effect::Mask { kind: crate::MaskKind::Alpha, image: 99, rgba: [1.0; 4], stretch: true }];
        e.set_scene("A", vec![it]);
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        let px = e.snapshot().unwrap_or_else(|x| panic!("the frame still draws: {x}"));
        assert_eq!(&px[0..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn only_what_is_on_air_is_heard() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        // A steady tone, shown only in B (inside a nested scene, as OBS counts that too).
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 50, beep_ms: 50 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("A", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Inner", vec![SceneItem::new(2, Placement::default())]);
        let mut nested = SceneItem::new(0, Placement::default());
        nested.scene = Some("Inner".into());
        e.set_scene("B", vec![SceneItem::new(1, Placement::default()), nested]);
        // Its peak in dB (the tone is at -6).
        let peak = |e: &Engine| e.levels(2).map_or(f32::NEG_INFINITY, |l| l.peak[0]);
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(400));
        assert!(peak(&e) < -60.0, "not on air, not heard: {}", peak(&e));
        e.go("B").unwrap_or_else(|x| panic!("{x}"));
        // The tone is scheduled half a second ahead.
        std::thread::sleep(Duration::from_millis(900));
        assert!(peak(&e) > -12.0, "on air inside a nested scene: {}", peak(&e));
        // Global sound is heard whatever is on air.
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        std::thread::sleep(Duration::from_millis(400));
        assert!(peak(&e) > -12.0, "global: {}", peak(&e));
    }

    #[test]
    fn reopening_keeps_what_did_not_change() {
        let Some(e) = engine() else { return };
        let doc = |x: f64, blue: u32| {
            serde_json::json!({
                "current_program_scene": "Main",
                "sources": [
                    { "id": "color_source", "name": "Back", "settings": { "color": 0xFF00_00FFu32 } },
                    { "id": "color_source", "name": "Box", "settings": { "color": blue, "width": 64, "height": 64 } },
                    { "id": "scene", "name": "Main", "settings": { "items": [
                        { "name": "Back" }, { "name": "Box", "pos": { "x": x, "y": 0.0 } } ] } }
                ]
            })
        };
        let ids = |e: &Engine| {
            let st = e.lock();
            let id = |n: &str| st.opened.get(n).map(|(i, _)| *i);
            (id("Back"), id("Box"))
        };
        assert!(e.load_obs(&doc(0.0, 0xFFFF_0000)).is_empty());
        let (back, bx) = ids(&e);
        // Only the box moved: both keep running as they were.
        e.load_obs(&doc(100.0, 0xFFFF_0000));
        assert_eq!(ids(&e), (back, bx));
        // The box's colour changed: it's made again, the background stays.
        e.load_obs(&doc(100.0, 0xFF00_FF00));
        let (back2, bx2) = ids(&e);
        assert_eq!(back2, back);
        assert_ne!(bx2, bx);
        let px = e.snapshot_scene("Main").unwrap_or_else(|x| panic!("{x}"));
        let at = (10 * 640 + 110) * 4;
        assert_eq!(&px[at..at + 4], &[0, 255, 0, 255], "the new box, where it moved");
    }

    #[test]
    fn keyframes_move_items_over_time() {
        let track = Track {
            prop: Prop::X,
            keys: vec![
                Key { at_ms: 0, value: 0.0, ease: Ease::Linear },
                Key { at_ms: 1000, value: 100.0, ease: Ease::Linear },
            ],
            repeat: false,
        };
        assert_eq!(track.value_at(0), Some(0.0));
        assert_eq!(track.value_at(500), Some(50.0));
        assert_eq!(track.value_at(5000), Some(100.0));
        let looped = Track { repeat: true, ..track.clone() };
        assert_eq!(looped.value_at(1250), Some(25.0));
        let held = Track { keys: vec![track.keys[0], Key { ease: Ease::Hold, ..track.keys[1] }], ..track };
        assert_eq!(held.value_at(999), Some(0.0));
        // An eased move is slow at both ends.
        assert!(Ease::InOut.apply(0.1) < 0.1 && Ease::InOut.apply(0.9) > 0.9);
    }

    #[test]
    fn move_glides_shared_sources() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 40, height: 40, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene(
            "Left",
            vec![
                SceneItem::new(1, Placement::default()),
                SceneItem::new(2, Placement { x: 0.0, y: 160.0, ..Default::default() }),
            ],
        );
        e.set_scene(
            "Right",
            vec![
                SceneItem::new(1, Placement::default()),
                SceneItem::new(2, Placement { x: 600.0, y: 160.0, ..Default::default() }),
            ],
        );
        e.go("Left").unwrap_or_else(|x| panic!("{x}"));
        // A long glide, so a slow machine still looks during it.
        e.go_with("Right", TransitionKind::Move, 4000).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(2000));
        let px = e.snapshot().unwrap_or_default();
        // Part way the square is between its two places, at neither.
        let white_at = (0..640).find(|x| px[(180 * 640 + x) * 4] > 200).unwrap_or(0);
        assert!((20..580).contains(&white_at), "the square is part way: x {white_at}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn stingers_cover_the_cut_then_go() {
        let Some(e) = engine() else { return };
        let clip = std::env::temp_dir().join("ferrocast-av-stinger.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=320x180:r=30:d=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            return; // no FFmpeg here
        }
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Black", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("White", vec![SceneItem::new(2, Placement::default())]);
        e.go("Black").unwrap_or_else(|x| panic!("{x}"));
        e.go_stinger("White", &clip, 500).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(450));
        assert!(e.transitioning());
        let mid = e.snapshot().unwrap_or_default();
        assert!(mid[0] > 200 && mid[1] < 60, "the stinger covers the cut: {:?}", &mid[..4]);
        std::thread::sleep(Duration::from_millis(1000));
        assert!(!e.transitioning(), "it ends with its video");
        assert_eq!(&e.snapshot().unwrap_or_default()[..3], &[255, 255, 255], "the new scene shows");
    }

    #[test]
    fn transitions_run_for_their_time() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Black", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("White", vec![SceneItem::new(2, Placement::default())]);
        e.go("Black").unwrap_or_else(|x| panic!("{x}"));
        e.go_with("White", TransitionKind::Fade, 600).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(300));
        assert!(e.transitioning());
        let mid = e.snapshot().unwrap_or_default()[0];
        assert!((60..200).contains(&mid), "half way through a fade: {mid}");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!e.transitioning());
        assert_eq!(e.snapshot().unwrap_or_default()[0], 255);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn canvases_of_their_own_at_once() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 360, height: 640, rgba: [200, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Wide", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Tall", vec![SceneItem::new(2, Placement::default())]);
        e.go("Wide").unwrap_or_else(|x| panic!("{x}"));
        let (wide, tall) =
            (std::env::temp_dir().join("ferrocast-av-wide.mp4"), std::env::temp_dir().join("ferrocast-av-tall.mp4"));
        let a = e.start_output(OutputKind::Record(wide.clone())).unwrap_or_else(|x| panic!("{x}"));
        let b = e
            .start_output_on(
                OutputKind::Record(tall.clone()),
                Canvas { width: 360, height: 640, scene: Some("Tall".into()), every: 2, ..Canvas::main(e.config()) },
            )
            .unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_secs(2));
        e.stop_output(a).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(b).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.error(), None);
        let probe = |f: &std::path::Path| {
            std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-count_frames",
                    "-show_entries",
                    "stream=width,height,nb_read_frames",
                    "-of",
                    "csv=p=0",
                ])
                .arg(f)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let (pw, pt) = (probe(&wide), probe(&tall));
        if !pw.is_empty() {
            assert!(pw.starts_with("640,360,"), "{pw}");
            assert!(pt.starts_with("360,640,"), "{pt}");
            let n = |s: &str| s.rsplit(',').next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            // Half the rate: about half the frames.
            assert!((n(&pt) * 2).abs_diff(n(&pw)) <= 4, "{pw} / {pt}");
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn monitoring_plays_on_the_speakers() {
        use crate::mix::{Monitoring, SourceSettings};
        let Some(e) = engine() else { return };
        // A quiet beep heard here only (left out of every output).
        e.set_source(1, SourceKind::Tone { hz: 660.0, every_ms: 400, beep_ms: 60 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_sound(1, SourceSettings { volume: 0.1, monitoring: Monitoring::Only, ..Default::default() });
        e.set_global(1, true);
        if e.set_monitoring(true, None).is_err() {
            return; // no speakers here (a CI machine)
        }
        std::thread::sleep(Duration::from_millis(1200));
        let (short, played) = e.monitoring_stats().unwrap_or_default();
        e.set_monitoring(false, None).unwrap_or_else(|x| panic!("{x}"));
        assert!(played > 40_000, "about a second played: {played}");
        assert!(short < 10, "the device seldom ran short: {short}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn records_on_its_own_clock() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Tone { hz: 1000.0, every_ms: 500, beep_ms: 50 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let file = std::env::temp_dir().join("ferrocast-av-engine.mp4");
        let id = e.start_output(OutputKind::Record(file.clone())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_secs(3));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let s = e.stats();
        // On the clock: every frame time is drawn or counted as skipped (a slow CI machine
        // skips some while its encoder warms up), and most are drawn.
        assert!(s.frames + s.skipped >= 85 && s.frames >= 45, "{s:?}");
        assert_eq!(e.error(), None);
        if let Ok(out) = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration:stream=codec_name", "-of", "csv=p=0"])
            .arg(&file)
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(text.contains("h264") && text.contains("aac"), "{text}");
        }
    }

    /// The length ffprobe reads, and whether it has picture and sound.
    #[cfg(target_os = "macos")]
    fn probed(file: &std::path::Path) -> Option<(f64, bool)> {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration:stream=codec_name", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let secs = text.lines().filter_map(|l| l.trim().parse::<f64>().ok()).next()?;
        Some((secs, text.contains("h264") && text.contains("aac")))
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_recording_pauses_and_splits() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Tone { hz: 1000.0, every_ms: 500, beep_ms: 50 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let dir = std::env::temp_dir();
        let (one, two) = (dir.join("ferrocast-av-split-1.mp4"), dir.join("ferrocast-av-split-2.mp4"));
        let id = e.start_output(OutputKind::Record(one.clone())).unwrap_or_else(|x| panic!("{x}"));
        let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
        // 2 s, 2 s paused, 1 s, then a new file for 2 s.
        sleep(2000);
        e.pause_output(id, true).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.output_record_state(id).map(|s| s.paused).ok(), Some(true));
        sleep(2000);
        e.pause_output(id, false).unwrap_or_else(|x| panic!("{x}"));
        sleep(1000);
        let written = e.output_record_state(id).map(|s| s.bytes).unwrap_or(0);
        assert!(written > 10_000, "the first file grows: {written} bytes");
        e.split_output(id, &two).unwrap_or_else(|x| panic!("{x}"));
        // One split at a time: another waits until this one has its first frame.
        assert!(e.split_output(id, &dir.join("ferrocast-av-split-3.mp4")).is_err());
        sleep(2000);
        assert_eq!(e.output_record_state(id).map(|s| s.splitting).ok(), Some(false));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.error(), None);
        let (Some((a, a_av)), Some((b, b_av))) = (probed(&one), probed(&two)) else {
            eprintln!("skipped the check: no ffprobe");
            return;
        };
        assert!(a_av && b_av, "both files have picture and sound");
        // The pause left no gap: about 3 s in the first file, not 5.
        assert!((2.6..3.6).contains(&a), "first file {a:.2} s");
        assert!((1.6..2.5).contains(&b), "second file {b:.2} s, from 0");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn records_mkv_that_decodes_cleanly() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [200, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 500, beep_ms: 100 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let dir = std::env::temp_dir();
        let (one, two) = (dir.join("ferrocast-av-rec-1.mkv"), dir.join("ferrocast-av-rec-2.mkv"));
        let id = e.start_output(OutputKind::Record(one.clone())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(2500));
        e.split_output(id, &two).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1500));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let (Some((a, a_av)), Some((b, b_av))) = (probed(&one), probed(&two)) else {
            eprintln!("skipped the check: no ffprobe");
            return;
        };
        assert!(a_av && b_av, "both files have picture and sound");
        assert!((2.1..3.0).contains(&a) && (1.1..1.9).contains(&b), "{a:.2} s, then {b:.2} s");
        for f in [&one, &two] {
            let out = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(f)
                .args(["-f", "null", "-"])
                .output()
                .unwrap_or_else(|x| panic!("{x}"));
            let errors = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(out.status.success() && errors.trim().is_empty(), "{}: {errors}", f.display());
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn an_output_scales_the_whole_canvas() {
        let Some(e) = engine() else { return };
        // Red on the left half, blue on the right, of a 640×360 canvas.
        e.set_source(1, SourceKind::Color { width: 320, height: 360, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 320, height: 360, rgba: [0, 0, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene(
            "Main",
            vec![
                SceneItem::new(1, Placement::default()),
                SceneItem::new(2, Placement { x: 320.0, ..Default::default() }),
            ],
        );
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let file = std::env::temp_dir().join("ferrocast-av-scaled.mp4");
        let canvas = Canvas::scaled(e.config(), 320, 180);
        let id = e.start_output_on(OutputKind::Record(file.clone()), canvas).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1500));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&file)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
        else {
            eprintln!("skipped the check: no ffmpeg");
            return;
        };
        let px = out.stdout;
        assert_eq!(px.len(), 320 * 180 * 3, "the whole canvas, at 320×180");
        let at = |x: usize, y: usize| &px[(y * 320 + x) * 3..(y * 320 + x) * 3 + 3];
        // Both halves are there, scaled (not the top-left corner cut out).
        assert!(at(40, 90)[0] > 200 && at(40, 90)[2] < 60, "red left: {:?}", at(40, 90));
        assert!(at(280, 90)[2] > 200 && at(280, 90)[0] < 60, "blue right: {:?}", at(280, 90));
    }

    /// ffmpeg's mean volume (dB) of one sound stream in a file.
    #[cfg(target_os = "macos")]
    fn mean_db(file: &std::path::Path, stream: usize) -> Option<f32> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "info", "-nostats", "-i"])
            .arg(file)
            .args(["-map", &format!("0:a:{stream}"), "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stderr).to_string();
        let line = text.lines().find(|l| l.contains("mean_volume:"))?;
        line.split("mean_volume:").nth(1)?.trim().trim_end_matches("dB").trim().parse().ok()
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_recording_carries_more_tracks() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        // A short beep on track 1 only, a long one on track 2 only.
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 500, beep_ms: 40 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_source(3, SourceKind::Tone { hz: 880.0, every_ms: 500, beep_ms: 300 }).unwrap_or_else(|x| panic!("{x}"));
        for (id, tracks) in [(2, 0b01), (3, 0b10)] {
            e.set_global(id, true);
            e.set_sound(id, SourceSettings { tracks, ..Default::default() });
        }
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        for name in ["ferrocast-av-tracks.mp4", "ferrocast-av-tracks.mkv"] {
            let file = std::env::temp_dir().join(name);
            let canvas = Canvas { more_tracks: vec![1], ..Canvas::main(e.config()) };
            let id = e.start_output_on(OutputKind::Record(file.clone()), canvas).unwrap_or_else(|x| panic!("{x}"));
            std::thread::sleep(Duration::from_millis(2500));
            e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
            let (Some(one), Some(two)) = (mean_db(&file, 0), mean_db(&file, 1)) else {
                eprintln!("skipped the check of {name}: no ffmpeg, or a track is missing");
                assert!(mean_db(&file, 0).is_none(), "{name}: track 2 is missing");
                continue;
            };
            // The long beep holds about 7 times the sound of the short one: about 8 dB more.
            assert!(two > one + 5.0, "{name}: track 1 {one} dB, track 2 {two} dB");
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn an_output_switches_its_own_scene() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 360, height: 640, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 360, height: 640, rgba: [0, 0, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Tall red", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Tall blue", vec![SceneItem::new(2, Placement::default())]);
        let file = std::env::temp_dir().join("ferrocast-av-vertical.mp4");
        let canvas = Canvas { width: 360, height: 640, scene: Some("Tall red".into()), ..Canvas::main(e.config()) };
        let id = e.start_output_on(OutputKind::Record(file.clone()), canvas).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(e.set_output_scene(id, Some("Nope".into())).is_err());
        e.set_output_scene(id, Some("Tall blue".into())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1200));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let frame = |at: &str| -> Option<Vec<u8>> {
            let out = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-ss", at, "-i"])
                .arg(&file)
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .output()
                .ok()?;
            (out.stdout.len() == 360 * 640 * 3).then_some(out.stdout)
        };
        let (Some(a), Some(b)) = (frame("0.3"), frame("2.0")) else {
            eprintln!("skipped the check: no ffmpeg");
            return;
        };
        assert!(a[0] > 200 && a[2] < 60, "red first: {:?}", &a[..3]);
        assert!(b[2] > 200 && b[0] < 60, "then blue: {:?}", &b[..3]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_replay_buffer_keeps_the_last_seconds() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 500, beep_ms: 100 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let id = e.start_output(OutputKind::Replay { seconds: 2, mb: 0 }).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(4500));
        let clip = e.replay_clip(id).unwrap_or_else(|x| panic!("{x}"));
        // From a keyframe (one a second here): 2 s, up to a second more.
        assert!((1.9..3.1).contains(&clip.seconds()), "{} s kept", clip.seconds());
        let file = std::env::temp_dir().join("ferrocast-av-replay.mkv");
        clip.write(&file).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let Some((secs, av)) = probed(&file) else {
            eprintln!("skipped the check: no ffprobe");
            return;
        };
        assert!(av && (1.9..3.2).contains(&secs), "{secs:.2} s");
        assert!(mean_db(&file, 0).is_some_and(|db| db > -40.0), "the sound came along");
    }
}
