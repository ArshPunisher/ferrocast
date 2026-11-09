//! The engine (ENGINE.md E7's core): what libobs's video and audio threads do, on our parts.
//! It owns sources, scenes and outputs; a thread of its own makes a frame every 1/fps on the
//! host clock (sources updated, the scene on air drawn, sound mixed, then encoded to every
//! output), and keeps numbers on how it's doing. The caller changes things through `Engine`'s
//! methods, from any thread.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// A display (macOS); with `sound`, what the Mac plays too (Ferrocast's own left out). The
    /// windows of the apps `hide` names (bundle ids or names) are left out: the privacy guard.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Screen {
        display: u32,
        sound: bool,
        hide: Vec<String>,
        /// The mouse pointer drawn in the picture (OBS's Show Cursor / Capture Cursor).
        cursor: bool,
    },
    /// One window (macOS), by its window number, as it looks wherever it is; with `cursor`,
    /// the mouse pointer drawn in it.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Window {
        id: u32,
        cursor: bool,
    },
    /// One app's windows on a display (macOS, OBS's Application Capture), by bundle id or
    /// name; with `sound`, its sound too.
    #[cfg(target_os = "macos")]
    App {
        display: u32,
        app: String,
        sound: bool,
        cursor: bool,
    },
    /// One app's sound alone (OBS's Application Audio Capture): its bundle id on a Mac, its
    /// program's name ("chrome") on Windows.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    AppSound(String),
    /// What the Mac plays, with no picture (OBS's Desktop Audio; Ferrocast's own left out).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    DesktopSound,
    /// A camera (macOS), by its id.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Camera(String),
    /// A microphone (macOS), by its id.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Microphone(String),
    /// A video or sound file (macOS), looping or once.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Media {
        path: PathBuf,
        looping: bool,
    },
    /// Pictures one after another (OBS's Image Slide Show).
    Slides(crate::source::series::Slides),
    /// Video or sound files one after another (OBS's VLC Video Source).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Playlist(crate::source::series::Playlist),
    /// Pictures and sound handed in from outside (a web page rendered by the app), through
    /// `Engine::fed`; `width`×`height` until the first picture says otherwise.
    Fed {
        width: u32,
        height: u32,
    },
}

/// Where an outside source (`SourceKind::Fed`) hands in its pictures and sound, from any
/// thread: the newest picture wins; sound is stereo, stamped on `now_ns`'s clock.
#[derive(Debug, Clone, Default)]
pub struct Feed(Arc<FeedSlot>);

#[derive(Debug, Default)]
struct FeedSlot {
    picture: Mutex<Option<Picture>>,
    sound: Mutex<std::collections::VecDeque<(Vec<f32>, u32, u64)>>,
}

impl Feed {
    /// The newest picture handed in, taken.
    pub fn take_picture(&self) -> Option<Picture> {
        self.0.picture.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn picture(&self, p: Picture) {
        *self.0.picture.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    }

    /// Stereo, interleaved, at `rate` Hz; its first sample at `ts_ns` (`now_ns`'s clock). At
    /// most a few seconds wait (a stalled engine can't grow memory).
    pub fn sound(&self, pcm: Vec<f32>, rate: u32, ts_ns: u64) {
        let mut q = self.0.sound.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back((pcm, rate, ts_ns));
        while q.len() > 400 {
            q.pop_front();
        }
    }
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
    /// The item's id in the scene collection (OBS's), for outlining it in the preview; 0 none.
    pub item_id: i64,
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
            item_id: 0,
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

/// One source's load on the engine (see [`Engine::source_load`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLoad {
    pub name: String,
    /// Pictures sent to the card since it started.
    pub sent: u64,
    pub width: u32,
    pub height: u32,
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
    /// Faults the loop caught and carried on from (a source's or a whole frame's).
    pub recovered: u64,
    /// Sources the watchdog started again (a camera that stopped sending, a capture that died).
    pub restarts: u64,
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
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Screen(crate::screen::ScreenCapture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Camera(crate::camera::Capture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Microphone(crate::camera::Capture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Media(crate::media::Media),
    Fed(Feed),
    /// Files one after another, on a worker that hands them in as a fed source would.
    Series {
        feed: Feed,
        _worker: crate::source::series::Worker,
    },
}

struct Src {
    live: Live,
    /// The picture's size once there is one, and whether it's premultiplied.
    size: Option<(u32, u32, bool)>,
    /// What it was started as, when the watchdog can start it again (captures).
    kind: Option<SourceKind>,
    watch: Watch,
    /// Its newest picture while nothing drawn shows it: kept, not sent to the GPU, until it is.
    pending: Option<Fresh>,
    /// Being opened on its own thread (that open's token): the watchdog leaves it alone.
    opening: Option<u64>,
    /// Pictures sent to the card so far (how busy it keeps the engine; Stats shows the rate).
    sent: u64,
}

/// A new picture for the card: pixels of ours, or (on a Mac) a frame of the screen as the
/// system holds it, which goes up without a copy of ours.
enum Fresh {
    Pic(Picture),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Screen(crate::screen::Surface),
}

impl Fresh {
    /// Width, height, and whether the colours are already multiplied by alpha.
    fn info(&self) -> (u32, u32, bool) {
        match self {
            Fresh::Pic(p) => (p.width, p.height, p.premultiplied),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(s) => (s.size().0, s.size().1, true),
        }
    }

    /// Sends it to the card as source `id`'s picture.
    fn send(&self, gpu: &Compositor, id: u64) -> bool {
        match self {
            Fresh::Pic(p) => gpu.upload(id, p).is_ok(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(s) => gpu.upload_surface(id, s).is_ok(),
        }
    }
}

impl Src {
    fn new(live: Live, size: Option<(u32, u32, bool)>, kind: Option<SourceKind>) -> Self {
        Src {
            live,
            size,
            kind,
            watch: Watch { since: now_ns(), ..Watch::default() },
            pending: None,
            opening: None,
            sent: 0,
        }
    }
}

/// The watchdog's look at a capture: what it had received last, since when, and its tries.
#[derive(Debug, Default)]
struct Watch {
    count: u64,
    since: u64,
    tries: u32,
    next_try: u64,
    /// It panicked in the loop (and was stopped there).
    faulted: bool,
}

/// A camera or microphone with nothing new for this long has stopped.
const STALL_NS: u64 = 5_000_000_000;

/// The wait before try `n` at starting a source again: 2 s, doubling, a minute at most.
fn backoff_ns(n: u32) -> u64 {
    (2_000_000_000u64 << n.min(5)).min(60_000_000_000)
}

struct State {
    sources: BTreeMap<u64, Src>,
    scenes: BTreeMap<String, Vec<SceneItem>>,
    on_air: Option<String>,
    /// A transition under way: from which scene, how, when it began and how long it lasts (ns).
    moving: Option<(String, TransitionKind, u64, u64)>,
    /// A stinger under way: the scene it came from, when it started, when the scene changes
    /// underneath, and when it ends (ns).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
    /// The previews windows show, by id.
    previews: HashMap<u64, Preview>,
    /// Cameras just taken out, still running for [`PARK_NS`]: one added again meanwhile (Undo,
    /// a rename, the same camera added back) comes back at once instead of being opened again,
    /// which takes seconds on Windows. What each was, and when it was parked.
    parked: Vec<(SourceKind, Live, u64)>,
}

/// How a preview looks, and what it shows (its window is with its own thread).
#[derive(Debug)]
struct Preview {
    look: crate::preview::PreviewLook,
    /// A scene instead of what's on air (Studio Mode's preview, the vertical picture's).
    scene: Option<String>,
    /// The canvas's size.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    size: (u32, u32),
    /// Every scene in boxes instead (a multiview projector).
    multiview: Option<Multiview>,
}

/// One box of a multiview: a scene (None: what's on air) stretched into `area` (the
/// multiview's pixels: x, y, w, h), framed in `frame` (straight RGBA; clear for none) and
/// named under it.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiviewBox {
    pub scene: Option<String>,
    pub area: (f32, f32, f32, f32),
    pub frame: [u8; 4],
    /// Framed in this instead while its scene is the one on air (a scene box, as OBS marks it).
    pub live: Option<[u8; 4]>,
    /// Thin lines round these boxes over the picture (the safe areas).
    pub marks: Vec<(f32, f32, f32, f32)>,
    pub label: Option<String>,
}

/// A multiview a preview shows: its size, the canvas its scenes are drawn at, and its boxes
/// with their names drawn once.
#[derive(Debug)]
struct Multiview {
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    size: (u32, u32),
    canvas: (u32, u32),
    boxes: Vec<(MultiviewBox, Option<Picture>)>,
}

/// The preview's own thread and its window: a window that's slow to take a frame (hidden,
/// covered) only ever holds this thread up, never the outputs.
struct PreviewThread {
    surface: Arc<Mutex<crate::preview::PreviewSurface>>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
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
    watchdog: Option<JoinHandle<()>>,
    preview: Mutex<HashMap<u64, PreviewThread>>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    next_preview: AtomicU64,
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
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
            previews: HashMap::new(),
            parked: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, gpu, mixer, stop) = (state.clone(), gpu.clone(), mixer.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-engine".into())
                .spawn(move || run(config, &state, &gpu, &mixer, &stop))
                .map_err(|e| AvError::Source(format!("the engine's thread didn't start ({e})")))?
        };
        let watchdog = {
            let (state, stop) = (state.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-watchdog".into())
                .spawn(move || watchdog(config, &state, &stop))
                .ok()
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
            watchdog,
            preview: Mutex::new(HashMap::new()),
            next_preview: AtomicU64::new(1),
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
        let restart = is_capture(&kind).then(|| kind.clone());
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
            SourceKind::App { .. } | SourceKind::AppSound(_) => {
                let live = open_capture(&self.config, &kind)
                    .unwrap_or_else(|| Err(AvError::Source("not a capture".into())))?;
                if matches!(kind, SourceKind::App { sound: true, .. } | SourceKind::AppSound(_)) {
                    self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                }
                (live, None)
            }
            #[cfg(target_os = "windows")]
            SourceKind::AppSound(_) => {
                let live = open_capture(&self.config, &kind)
                    .unwrap_or_else(|| Err(AvError::Source("not a capture".into())))?;
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                (live, None)
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            SourceKind::Screen { .. }
            | SourceKind::Window { .. }
            | SourceKind::DesktopSound
            | SourceKind::Camera(_)
            | SourceKind::Microphone(_) => {
                let heard = match &kind {
                    SourceKind::Screen { sound, .. } => *sound,
                    SourceKind::DesktopSound | SourceKind::Microphone(_) => true,
                    _ => false,
                };
                if heard {
                    self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                }
                if let Some(live) = self.unpark(&kind) {
                    tracing::info!(source = id, "a camera taken out a moment ago came back without opening again");
                    self.lock().sources.insert(id, Src::new(live, None, Some(kind)));
                    return Ok(());
                }
                let config = self.config;
                let open = kind.clone();
                return self.open_soon(
                    id,
                    Some(kind),
                    Box::new(move || {
                        open_capture(&config, &open)
                            .unwrap_or_else(|| Err(AvError::Source("not a capture".into())))
                            .map(|live| (live, false))
                    }),
                );
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            SourceKind::Media { path, looping } => {
                // Opening a file can take a while (FFmpeg looks at it, then tries the GPU
                // decoder): on its own thread like a capture. A missing file still says so at once.
                return self.open_soon(
                    id,
                    None,
                    Box::new(move || {
                        crate::media::Media::open(&path, looping).map(|m| {
                            let heard = m.has_audio;
                            (Live::Media(m), heard)
                        })
                    }),
                );
            }
            SourceKind::Slides(s) => {
                let feed = Feed::default();
                let worker = crate::source::series::slideshow(s, feed.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            SourceKind::Playlist(p) => {
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                let feed = Feed::default();
                let worker = crate::source::series::playlist(p, feed.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            SourceKind::Fed { width, height } => {
                self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                // Clear until the first picture comes.
                (Live::Fed(Feed::default()), Some(Picture::solid(width.max(1), height.max(1), [0, 0, 0, 0])))
            }
        };
        let size = match &picture {
            Some(p) => {
                self.gpu.upload(id, p)?;
                Some((p.width, p.height, p.premultiplied))
            }
            None => None,
        };
        self.lock().sources.insert(id, Src::new(live, size, restart));
        Ok(())
    }

    /// A parked capture of this kind, taken back (see `State::parked`).
    fn unpark(&self, kind: &SourceKind) -> Option<Live> {
        let mut st = self.lock();
        let at = st.parked.iter().position(|(k, ..)| k == kind)?;
        Some(st.parked.swap_remove(at).1)
    }

    /// Opens capture `kind` as source `id` on its own thread. Opening one can take seconds (on
    /// Windows FFmpeg looks at a camera before reading it), and whoever adds a source is often
    /// the window's own thread: it froze, and the live picture couldn't move out from under a
    /// dialog meanwhile (owner, 8 Nov). Waits [`OPEN_WAIT`] so a quick failure (a camera in
    /// use) still comes back as the error; a slower open lands by itself, and one that fails
    /// then is tried again by the watchdog (a capture) or let go (a file: the next opening of the
    /// collection tries it again).
    fn open_soon(&self, id: u64, restart: Option<SourceKind>, open: Opener) -> Result<(), AvError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let token = NEXT.fetch_add(1, Ordering::Relaxed);
        {
            let mut src = Src::new(Live::Still, None, restart);
            src.opening = Some(token);
            self.lock().sources.insert(id, src);
        }
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), AvError>>();
        let state = self.state.clone();
        let mixer = self.mixer.clone();
        let spawned = std::thread::Builder::new().name("ferrocore-open".into()).spawn(move || {
            // A panic opening it is an error like any other: the token is cleared below and the
            // watchdog tries again (CodeRabbit), instead of a source stuck "opening".
            let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open))
                .unwrap_or_else(|p| Err(AvError::Source(format!("opening it failed: {}", panic_text(&*p)))));
            let mut leftover = None;
            let mut discard = false;
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            // Still the same source (not removed or replaced while it opened)?
            let mine = st.sources.get_mut(&id).filter(|s| s.opening == Some(token));
            let answer = match (opened, mine) {
                (Ok((live, heard)), Some(src)) => {
                    src.live = live;
                    src.opening = None;
                    src.watch = Watch { since: now_ns(), ..Watch::default() };
                    // Its sound into the mix (a file with sound), under the same lock so a
                    // removal can't slip in between (state, then mixer: the frame loop's order).
                    if heard {
                        mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, SourceSettings::default());
                    }
                    Ok(())
                }
                (Err(e), Some(src)) => {
                    src.opening = None;
                    if src.kind.is_some() {
                        // A capture: the watchdog tries again later, waiting longer each time.
                        src.watch.next_try = now_ns() + backoff_ns(0);
                        src.watch.tries = 1;
                    } else {
                        // Nothing restarts a file: let it go, so the next opening of the
                        // collection tries again and says why it's left out (CodeRabbit).
                        discard = true;
                    }
                    Err(e)
                }
                // Gone meanwhile: let go below, outside the lock (stopping a camera can wait).
                (Ok((live, _)), None) => {
                    leftover = Some(live);
                    Ok(())
                }
                (Err(e), None) => Err(e),
            };
            if discard {
                st.sources.remove(&id);
            }
            drop(st);
            drop(leftover);
            let failed = answer.as_ref().err().map(ToString::to_string);
            // Told to whoever added it when they're still waiting; logged when it came too late
            // for that (otherwise a missing file was logged again at every edit).
            if tx.send(answer).is_err()
                && let Some(e) = failed
            {
                tracing::warn!(source = id, error = %e, "a source didn't open");
            }
        });
        if let Err(e) = spawned {
            self.lock().sources.remove(&id);
            return Err(AvError::Source(format!("couldn't start opening it: {e}")));
        }
        match rx.recv_timeout(OPEN_WAIT) {
            Ok(Err(e)) => {
                // Said straight away: not kept as a source that never opened (nor its sound).
                let mut st = self.lock();
                if st.sources.get(&id).is_some_and(|s| matches!(s.live, Live::Still) && s.opening.is_none()) {
                    st.sources.remove(&id);
                    self.mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
                }
                Err(e)
            }
            // Opened, or still opening: it lands by itself.
            _ => Ok(()),
        }
    }

    /// Sources still opening (captures and files started by [`Engine::set_source`], not in yet).
    pub fn opening(&self) -> usize {
        self.lock().sources.values().filter(|s| s.opening.is_some()).count()
    }

    pub fn remove_source(&self, id: u64) {
        let gone = {
            let mut st = self.lock();
            st.globals.remove(&id);
            match st.sources.remove(&id) {
                // A running camera is parked a moment (see `State::parked`).
                Some(src) if src.opening.is_none() && matches!(src.kind, Some(SourceKind::Camera(_))) => {
                    if let (Some(kind), live @ Live::Camera(_)) = (src.kind, src.live) {
                        st.parked.push((kind, live, now_ns()));
                    }
                    None
                }
                other => other,
            }
        };
        // Let go outside the lock (stopping a capture can wait).
        drop(gone);
        self.gpu.forget(id);
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    /// A source's sound settings (volume, mute, balance, tracks…), for sources with sound.
    pub fn set_sound(&self, id: u64, settings: SourceSettings) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set(id, settings);
    }

    /// Music quieter while the voices talk (None: off).
    pub fn set_ducking(&self, ducking: Option<crate::level::Ducking>) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_ducking(ducking);
    }

    /// Every track steered to `target_lufs` and kept under -1 dBFS (None: off).
    pub fn set_loudness(&self, target_lufs: Option<f32>) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_loudness(target_lufs);
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

    /// Plays, pauses, restarts, stops or seeks a media source (OBS's media controls).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn media_control(&self, id: u64, c: crate::media::Control) -> Result<(), AvError> {
        match self.lock().sources.get(&id).map(|s| &s.live) {
            Some(Live::Media(m)) => {
                m.control(c);
                Ok(())
            }
            _ => Err(AvError::Source(format!("source {id} isn't a media file"))),
        }
    }

    /// A media source's state, where it is and how long it lasts (ns).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn media_state(&self, id: u64) -> Option<(crate::media::MediaState, u64, u64)> {
        match self.lock().sources.get(&id).map(|s| &s.live) {
            Some(Live::Media(m)) => {
                let (state, at) = m.state();
                Some((state, at, m.duration_ns))
            }
            _ => None,
        }
    }

    /// Where an outside source (`SourceKind::Fed`) is handed its pictures and sound.
    pub fn fed(&self, id: u64) -> Option<Feed> {
        match self.lock().sources.get(&id).map(|s| &s.live) {
            Some(Live::Fed(f)) => Some(f.clone()),
            _ => None,
        }
    }

    /// Listens to one source's sound (mono, 48 kHz, as it comes in): live captions.
    pub fn tap_sound(&self, id: u64) -> crate::mix::Tap {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).tap(id)
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
                    if let Effect::Mask { image, .. } | Effect::Cutout { mask: image, .. } = e {
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
        self.load_audio_care(doc);
        let first = self.scene_names().into_iter().next();
        if let Some(cur) = imported.current.or(first) {
            let _ = self.go(&cur);
        }
        skipped
    }

    /// Ferrocast's own sound care in the document (`ferrocast.audio`): ducking (`ducking`:
    /// `{ voices, music }` source names and `depth_db`) and loudness (`loudness_lufs`).
    fn load_audio_care(&self, doc: &serde_json::Value) {
        let a = &doc["ferrocast"]["audio"];
        let ids = |key: &str| -> Vec<u64> {
            let st = self.lock();
            a["ducking"][key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|n| st.opened.get(n).map(|(id, _)| *id))
                .collect()
        };
        let ducking = a["ducking"].is_object().then(|| crate::level::Ducking {
            voices: ids("voices"),
            music: ids("music"),
            depth_db: a["ducking"]["depth_db"].as_f64().map_or(-12.0, |d| (d as f32).clamp(-40.0, -3.0)),
            ..Default::default()
        });
        self.set_ducking(ducking);
        self.set_loudness(a["loudness_lufs"].as_f64().map(|l| (l as f32).clamp(-30.0, -8.0)));
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

    /// Every source a collection brought, by name: pictures sent to the card so far and their
    /// size. Read twice, the difference is each one's rate (what keeps the engine busiest).
    pub fn source_load(&self) -> Vec<SourceLoad> {
        let st = self.lock();
        let mut out: Vec<SourceLoad> = st
            .opened
            .iter()
            .filter_map(|(name, (id, _))| {
                let s = st.sources.get(id)?;
                let (width, height) = s.size.map_or((0, 0), |(w, h, _)| (w, h));
                Some(SourceLoad { name: name.clone(), sent: s.sent, width, height })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Captures shown somewhere (on air, a preview, an output) that have had no picture for at
    /// least `quiet_ns` (and aren't still opening): each one's name, and whether it captures the
    /// screen (a window, a display, an app) rather than a camera. The Studio's black screen
    /// fixer says why and what to do.
    pub fn quiet_captures(&self, quiet_ns: u64) -> Vec<(String, bool)> {
        let st = self.lock();
        let shown = seen(&st);
        let now = now_ns();
        let mut out: Vec<(String, bool)> = st
            .opened
            .iter()
            .filter_map(|(name, (id, kind))| {
                let screen = match kind {
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    SourceKind::Camera(_) => false,
                    #[cfg(target_os = "macos")]
                    SourceKind::App { .. } => true,
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    SourceKind::Screen { .. } => true,
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    SourceKind::Window { .. } => true,
                    _ => return None,
                };
                let s = st.sources.get(id)?;
                (shown.contains(id)
                    && s.size.is_none()
                    && s.opening.is_none()
                    && now.saturating_sub(s.watch.since) >= quiet_ns)
                    .then(|| (name.clone(), screen))
            })
            .collect();
        out.sort();
        out
    }

    /// A source an opened collection brought, by name: its id, and its picture's size once
    /// it has one.
    pub fn source_named(&self, name: &str) -> Option<(u64, Option<(u32, u32)>)> {
        let st = self.lock();
        let (id, _) = st.opened.get(name)?;
        Some((*id, st.sources.get(id).and_then(|s| s.size).map(|(w, h, _)| (w, h))))
    }

    /// Places item `item_id` (its id in the scene collection) of `scene` at `p` from the next
    /// frame. False when there's no such item (the caller then opens the collection again).
    pub fn set_item_placement(&self, scene: &str, item_id: i64, p: Placement) -> bool {
        if item_id == 0 {
            return false;
        }
        let mut st = self.lock();
        match st.scenes.get_mut(scene).and_then(|items| items.iter_mut().find(|i| i.item_id == item_id)) {
            Some(it) => {
                it.placement = p;
                true
            }
            None => false,
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
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
        st.sources.insert(STINGER, Src::new(Live::Media(m), None, None));
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
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
        let (video, audio) = {
            let st = self.lock();
            (st.video, st.audio)
        };
        self.start_output_with(kind, canvas, video, audio)
    }

    /// As `start_output_on`, encoded its own way (each output its encoder, bitrate and quality:
    /// a stream at 6 Mbps and a recording at high quality from one engine). The canvas sets the
    /// picture's size and rate.
    pub fn start_output_with(
        &self,
        kind: OutputKind,
        canvas: Canvas,
        video: VideoConfig,
        audio: AudioConfig,
    ) -> Result<u64, AvError> {
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
        let known = |st: &State| canvas.scene.as_ref().is_none_or(|name| st.scenes.contains_key(name));
        if !known(&self.lock()) {
            return Err(AvError::Source(format!(
                "there's no scene called \"{}\"",
                canvas.scene.as_deref().unwrap_or("")
            )));
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
            ..video
        };
        // Extra tracks go in a file (a recording, or a saved replay), or with a stream as its
        // further tracks (Twitch's VOD track).
        let replay = match &kind {
            OutputKind::Replay { seconds, mb } => Some((*seconds, *mb)),
            _ => None,
        };
        let more = canvas.more_tracks.len();
        let mut pipe = Pipeline::with_tracks(video, audio, record.as_deref(), more)?;
        if matches!(kind, OutputKind::Stream(_)) {
            pipe.stream_extra_tracks();
        }
        if let Some((seconds, mb)) = replay {
            pipe.keep_replay(seconds, mb);
        }
        // Made above without the engine's lock: starting FFmpeg (and, the first time, trying
        // each encoder) takes a second or more, and the frame loop waits on that lock every
        // frame, so every recording started with a burst of skipped frames.
        let mut st = self.lock();
        if !known(&st) {
            return Err(AvError::Source(format!(
                "there's no scene called \"{}\"",
                canvas.scene.as_deref().unwrap_or("")
            )));
        }
        let id = st.next_output;
        st.next_output += 1;
        st.outputs.push((id, pipe, kind, canvas));
        Ok(id)
    }

    /// A new video bitrate for an output from now on; false when its encoder has none to change
    /// (constant quality).
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

    /// Shows a preview in a Mac view (`NSView *`, `width`×`height` device pixels): what's on
    /// air at the engine's size, drawn each frame from now on → its id. Several can show at
    /// once (Studio Mode's two, the vertical picture's, projectors).
    ///
    /// # Safety
    /// `ns_view` must be a live `NSView` that outlives the preview (`stop_preview` first).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[allow(unsafe_code)]
    pub unsafe fn start_preview(
        &self,
        ns_view: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Result<u64, AvError> {
        let size = (self.config.width, self.config.height);
        // SAFETY: passed on from the caller's promise.
        unsafe { self.start_preview_of(ns_view, width, height, size, None) }
    }

    /// As `start_preview`, of a canvas of `size` showing `scene` (the vertical picture's).
    ///
    /// # Safety
    /// As `start_preview`.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[allow(unsafe_code)]
    pub unsafe fn start_preview_of(
        &self,
        ns_view: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        size: (u32, u32),
        scene: Option<String>,
    ) -> Result<u64, AvError> {
        // SAFETY: passed on from the caller's promise.
        let surface = unsafe { self.gpu.preview_surface(ns_view, width, height) }?;
        let id = self.next_preview.fetch_add(1, Ordering::Relaxed);
        self.lock().previews.insert(id, Preview { look: Default::default(), scene, size, multiview: None });
        let surface = Arc::new(Mutex::new(surface));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, gpu, surface, stop) = (self.state.clone(), self.gpu.clone(), surface.clone(), stop.clone());
            let every = Duration::from_secs(1) / self.config.fps.max(1);
            std::thread::Builder::new()
                .name("ferrocast-preview".into())
                .spawn(move || preview_loop(id, &state, &gpu, &surface, &stop, every))
                .map_err(|e| AvError::Source(format!("the preview's thread didn't start ({e})")))?
        };
        self.preview_threads().insert(id, PreviewThread { surface, stop, thread });
        Ok(id)
    }

    fn preview_threads(&self) -> std::sync::MutexGuard<'_, HashMap<u64, PreviewThread>> {
        self.preview.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stops drawing a preview; once it returns, its window may go.
    pub fn stop_preview(&self, id: u64) {
        let running = self.preview_threads().remove(&id);
        if let Some(p) = running {
            p.stop.store(true, Ordering::Release);
            let _ = p.thread.join();
        }
        self.lock().previews.remove(&id);
    }

    /// A preview's view changed size (device pixels).
    pub fn resize_preview(&self, id: u64, width: u32, height: u32) {
        if let Some(p) = self.preview_threads().get(&id) {
            self.gpu.resize_preview(&mut p.surface.lock().unwrap_or_else(|e| e.into_inner()), width, height);
        }
    }

    /// How a preview looks round and over the picture (outlines, where the canvas sits…).
    pub fn set_preview_look(&self, id: u64, look: crate::preview::PreviewLook) {
        if let Some(p) = self.lock().previews.get_mut(&id) {
            p.look = look;
        }
    }

    /// A scene a preview shows instead of what's on air (Studio Mode), or None.
    pub fn set_preview_scene(&self, id: u64, scene: Option<String>) {
        if let Some(p) = self.lock().previews.get_mut(&id) {
            p.scene = scene;
        }
    }

    /// Makes a preview a multiview `size` big, its boxes' scenes drawn at `canvas` (OBS's
    /// multiview projector); no boxes makes it a plain preview again.
    pub fn set_preview_multiview(&self, id: u64, size: (u32, u32), canvas: (u32, u32), boxes: Vec<MultiviewBox>) {
        let mv = (!boxes.is_empty()).then(|| {
            let mut text = self.text.lock().unwrap_or_else(|e| e.into_inner());
            let boxes = boxes
                .into_iter()
                .map(|b| {
                    // White on a dark band, sized to the box (OBS's labels).
                    let style = TextStyle {
                        size: (b.area.3 * 0.07).clamp(12.0, 48.0),
                        color: [255, 255, 255, 255],
                        background: Some([0, 0, 0, 170]),
                        padding: (b.area.3 * 0.015).clamp(2.0, 10.0) as u32,
                        ..Default::default()
                    };
                    let label = b.label.as_deref().filter(|l| !l.is_empty()).map(|l| text.render(l, &style));
                    (b, label)
                })
                .collect();
            Multiview { size, canvas, boxes }
        });
        if let Some(p) = self.lock().previews.get_mut(&id) {
            p.multiview = mv;
        }
    }

    /// A chapter mark in an output's recording here (named, or "Chapter n").
    pub fn chapter_output(&self, id: u64, name: &str) -> Result<(), AvError> {
        self.with_output(id, |pipe| pipe.chapter(name))
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
        let own = self.lock().error.clone();
        own.or_else(|| self.gpu.gpu_error.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// Any scene drawn now at the engine's size, RGBA: Studio Mode's preview, the Scenes grid.
    pub fn snapshot_scene(&self, name: &str) -> Result<Vec<u8>, AvError> {
        let scene = {
            let mut st = self.lock();
            if !st.scenes.contains_key(name) {
                return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
            }
            send_pending(&mut st, &self.gpu);
            build(&st, self.config.width, self.config.height, Some(name), now_ns())
        };
        self.gpu.render(&scene)
    }

    /// A scene drawn on a canvas of `canvas` size and scaled down whole to `size` (bicubic),
    /// RGBA: the Scenes grid's tiles, a vertical scene's at its own shape.
    pub fn snapshot_scene_at(&self, name: &str, canvas: (u32, u32), size: (u32, u32)) -> Result<Vec<u8>, AvError> {
        let scene = {
            let mut st = self.lock();
            if !st.scenes.contains_key(name) {
                return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
            }
            send_pending(&mut st, &self.gpu);
            build(&st, canvas.0.max(1), canvas.1.max(1), Some(name), now_ns())
        };
        self.gpu.render(&scaled(scene, size.0.max(1), size.1.max(1), Filter::Bicubic))
    }

    /// One source's picture alone, scaled to `size`, RGBA (face follow looks at it). None until
    /// it has a picture.
    pub fn snapshot_source_at(&self, id: u64, size: (u32, u32)) -> Option<Vec<u8>> {
        let (w, h) = (size.0.max(1), size.1.max(1));
        let item = {
            let mut st = self.lock();
            if let Some(src) = st.sources.get_mut(&id)
                && let Some(f) = src.pending.take()
                && f.send(&self.gpu, id)
            {
                src.size = Some(f.info());
            }
            let (sw, sh, premultiplied) = st.sources.get(&id)?.size?;
            let mut it = Item::picture(Picture::solid(1, 1, [0, 0, 0, 0]), Placement::default());
            it.content = Content::Uploaded { id, width: sw, height: sh, premultiplied };
            it.placement = Placement {
                bounds: crate::Bounds::Stretch,
                bounds_width: w as f32,
                bounds_height: h as f32,
                ..Default::default()
            };
            it.filter = Filter::Bilinear;
            it
        };
        self.gpu.render(&Scene { width: w, height: h, items: vec![item] }).ok()
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
        let ids: Vec<u64> = self.preview_threads().keys().copied().collect();
        for id in ids {
            self.stop_preview(id);
        }
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.watchdog.take() {
            let _ = t.join();
        }
        let ids: Vec<u64> = self.lock().outputs.iter().map(|(i, ..)| *i).collect();
        for id in ids {
            let _ = self.stop_output(id);
        }
    }
}

/// The preview's thread: what's on air (or Studio Mode's scene) at the engine's rate, taken
/// from the state in a moment and drawn into the window outside its lock.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn preview_loop(
    id: u64,
    state: &Mutex<State>,
    gpu: &Compositor,
    surface: &Mutex<crate::preview::PreviewSurface>,
    stop: &AtomicBool,
    every: Duration,
) {
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let next = {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            st.previews.get(&id).map(|p| {
                let draw = match &p.multiview {
                    Some(mv) => Draw::Scene(multiview_scene(&st, mv, now_ns())),
                    None => what_to_draw(&st, p.size.0, p.size.1, p.scene.as_deref(), now_ns()),
                };
                // The outlines from the same items, at the same moment, as the picture: drawn
                // together they can't drift apart while a source is dragged (as OBS draws its
                // selection boxes in the frame it draws the scene).
                let mut look = p.look.clone();
                if let Some(m) = &look.marks {
                    look.outlines = outlines(&st, m, p.size, now_ns());
                }
                (draw, look)
            })
        };
        if let Some((draw, look)) = next {
            let shown = match &draw {
                Draw::Scene(s) => crate::preview::Shown::Scene(s),
                Draw::Moving(t) => crate::preview::Shown::Moving(t),
            };
            let mut s = surface.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = gpu.present(&mut s, shown, &look) {
                tracing::warn!("the preview couldn't be drawn: {e}");
            }
        }
        std::thread::sleep(every.saturating_sub(started.elapsed()));
    }
}

/// The preview's outlines (canvas pixels) for `m`, from the engine's own items at `ts`: every
/// shown source of `m.scene` in its colour, then the picked ones with handles on top.
fn outlines(st: &State, m: &crate::preview::Marks, canvas: (u32, u32), ts: u64) -> Vec<crate::preview::Outline> {
    let corners = |scene: &str, it: &SceneItem| -> Option<[(f32, f32); 4]> {
        let (placement, _) = animated(it, scene_ms(st, scene, ts));
        let (w, h) = match (&it.scene, it.group) {
            (Some(_), Some(size)) => size,
            (Some(_), None) => canvas,
            (None, _) => st.sources.get(&it.source)?.size.map(|(w, h, _)| (w, h))?,
        };
        let a = crate::transform::draw_transform(&placement, w, h);
        let (cw, ch) = crate::transform::cropped(w, h, placement.crop);
        let (cw, ch) = (cw as f32, ch as f32);
        Some([a.apply(0.0, 0.0), a.apply(cw, 0.0), a.apply(cw, ch), a.apply(0.0, ch)])
    };
    let color = |id: i64| {
        let n = m.colors.len().max(1) as i64;
        m.colors.get(id.rem_euclid(n) as usize).copied().unwrap_or([1.0, 1.0, 1.0, 1.0])
    };
    let picked = |scene: &str, id: i64| m.picked.as_ref().is_some_and(|(s, p)| s == scene && p.contains(&id));
    let mut out = Vec::new();
    if let Some(scene) = &m.scene
        && let Some(items) = st.scenes.get(scene)
    {
        for it in items.iter().filter(|i| i.visible && i.item_id != 0 && !picked(scene, i.item_id)) {
            if let Some(c) = corners(scene, it) {
                out.push(crate::preview::Outline { corners: c, color: color(it.item_id), width: 1.5, handles: false });
            }
        }
    }
    if let Some((scene, ids)) = &m.picked
        && let Some(items) = st.scenes.get(scene)
    {
        for it in items.iter().filter(|i| i.item_id != 0 && ids.contains(&i.item_id)) {
            if let Some(c) = corners(scene, it) {
                out.push(crate::preview::Outline { corners: c, color: color(it.item_id), width: 2.0, handles: true });
            }
        }
    }
    out
}

/// What a frame shows: the scene on air, or two scenes mid-transition.
enum Draw {
    Scene(Scene),
    Moving(Box<Transition>),
}

/// The stinger's own source id (it plays as a source while it runs).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
const STINGER: u64 = u64::MAX - 1;

/// A canvas's frame: its own scene, or what's on air (mid-transition, perhaps).
fn what_to_draw(st: &State, width: u32, height: u32, scene: Option<&str>, ts: u64) -> Draw {
    if let Some(name) = scene {
        return Draw::Scene(build(st, width, height, Some(name), ts));
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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

/// A multiview's frame: each box's scene stretched into it (what's on air for the program box,
/// past any transition), its frame round it and its name along its bottom.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn multiview_scene(st: &State, mv: &Multiview, ts: u64) -> Scene {
    let at = |content: Content, (x, y, w, h): (f32, f32, f32, f32)| Item {
        content,
        effects: Vec::new(),
        placement: Placement {
            x,
            y,
            bounds: crate::Bounds::Stretch,
            bounds_width: w.max(1.0),
            bounds_height: h.max(1.0),
            ..Default::default()
        },
        filter: Filter::Bilinear,
        blend: crate::Blend::Normal,
        srgb_off: false,
        linear: true,
        opacity: 1.0,
    };
    let solid =
        |rgba: [u8; 4]| Content::Picture(Picture { width: 1, height: 1, rgba: rgba.to_vec(), premultiplied: false });
    let t = (mv.size.1 as f32 * 0.004).max(2.0);
    let mut items = Vec::new();
    for (b, label) in &mv.boxes {
        let (x, y, w, h) = b.area;
        let name = b.scene.as_deref().or(st.on_air.as_deref());
        items.push(at(Content::Scene(build(st, mv.canvas.0, mv.canvas.1, name, ts)), b.area));
        let frame = match b.live {
            Some(live) if b.scene.is_some() && b.scene.as_deref() == st.on_air.as_deref() => live,
            _ => b.frame,
        };
        let mut ring = |(x, y, w, h): (f32, f32, f32, f32), t: f32, rgba: [u8; 4]| {
            for bar in
                [(x - t, y - t, w + 2.0 * t, t), (x - t, y + h, w + 2.0 * t, t), (x - t, y, t, h), (x + w, y, t, h)]
            {
                items.push(at(solid(rgba), bar));
            }
        };
        for m in &b.marks {
            ring(*m, (t / 2.0).max(1.0), [255, 255, 255, 140]);
        }
        if frame[3] > 0 {
            ring(b.area, t, frame);
        }
        if let Some(p) = label {
            let (lw, lh) = (p.width as f32, p.height as f32);
            let k = (w * 0.95 / lw.max(1.0)).min(1.0);
            let (lw, lh) = (lw * k, lh * k);
            items.push(at(Content::Picture(p.clone()), (x + (w - lw) / 2.0, y + h - lh - t, lw, lh)));
        }
    }
    Scene { width: mv.size.0, height: mv.size.1, items }
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
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    roots.extend(st.stinger.as_ref().map(|(from, ..)| from.as_str()));
    roots.extend(st.outputs.iter().filter_map(|(.., c)| c.scene.as_deref()));
    for r in roots {
        walk(st, r, 0, &mut out);
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    out.insert(STINGER);
    out
}

/// Sources something drawn shows now: on air, a transition's or stinger's, an output's own
/// scene, a preview's or a multiview's, with the scenes inside them and the masks they use.
fn seen(st: &State) -> std::collections::HashSet<u64> {
    fn walk(st: &State, scene: &str, depth: u32, out: &mut std::collections::HashSet<u64>) {
        let Some(items) = st.scenes.get(scene) else { return };
        for it in items.iter().filter(|i| i.visible) {
            for e in &it.effects {
                if let Effect::Mask { image, .. } | Effect::Cutout { mask: image, .. } = e {
                    out.insert(*image);
                }
            }
            match &it.scene {
                Some(inner) if depth < MAX_NESTING => walk(st, inner, depth + 1, out),
                Some(_) => {}
                None => {
                    out.insert(it.source);
                }
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    let mut roots: Vec<&str> = Vec::new();
    roots.extend(st.on_air.as_deref());
    roots.extend(st.moving.as_ref().map(|(from, ..)| from.as_str()));
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    roots.extend(st.stinger.as_ref().map(|(from, ..)| from.as_str()));
    roots.extend(st.outputs.iter().filter_map(|(.., c)| c.scene.as_deref()));
    for p in st.previews.values() {
        roots.extend(p.scene.as_deref());
        if let Some(mv) = &p.multiview {
            roots.extend(mv.boxes.iter().filter_map(|(b, _)| b.scene.as_deref()));
        }
    }
    for r in roots {
        walk(st, r, 0, &mut out);
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    out.insert(STINGER);
    out
}

/// Pictures kept back while unseen, sent to the GPU now (something is about to draw them
/// another way: a thumbnail, a snapshot).
fn send_pending(st: &mut State, gpu: &Compositor) {
    for (id, src) in st.sources.iter_mut() {
        if let Some(f) = src.pending.take()
            && f.send(gpu, *id)
        {
            src.size = Some(f.info());
        }
    }
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
            // A mask not here yet (Remove background's first one too): the picture as it is.
            Effect::Mask { image, .. } | Effect::Cutout { mask: image, .. } => {
                st.sources.get(image).is_some_and(|s| s.size.is_some())
            }
            _ => true,
        })
        .map(|e| {
            let mut e = e.clone();
            if let Effect::Scroll { elapsed_ms, .. } | Effect::Custom { elapsed_ms, .. } = &mut e {
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
    let mut faults = 0u64;
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

        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            frame(&config, state, gpu, mixer, ts, behind, started);
            // Pictures uploaded this frame are sent even when nothing drew (no preview, no
            // output), and what the card finished with is let go: without this, Direct3D and
            // Vulkan keep it all and run out of memory in minutes.
            gpu.submit(std::iter::empty());
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }));
        if let Err(p) = ok {
            // A fault in a frame is that frame lost, not the engine: the next one is drawn.
            faults += 1;
            if faults.is_power_of_two() {
                tracing::error!(why = panic_text(&*p), faults, "the engine caught a fault in a frame and carried on");
            }
            state.lock().unwrap_or_else(|e| e.into_inner()).stats.recovered += 1;
        }
    }
}

/// Sources the watchdog can start again: captures (a file or a picture doesn't stop by itself).
fn is_capture(kind: &SourceKind) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    if matches!(
        kind,
        SourceKind::Screen { .. }
            | SourceKind::Window { .. }
            | SourceKind::DesktopSound
            | SourceKind::Camera(_)
            | SourceKind::Microphone(_)
    ) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if matches!(kind, SourceKind::App { .. } | SourceKind::AppSound(_)) {
        return true;
    }
    #[cfg(target_os = "windows")]
    if matches!(kind, SourceKind::AppSound(_)) {
        return true;
    }
    let _ = kind;
    false
}

/// Starts a capture (None: `kind` isn't one).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn open_capture(config: &EngineConfig, kind: &SourceKind) -> Option<Result<Live, AvError>> {
    use crate::screen::{ScreenCapture, Target};
    Some(match kind {
        SourceKind::Screen { display, sound, hide, cursor } => ScreenCapture::start_with(
            Target::Display(*display),
            config.width,
            config.height,
            config.fps,
            *cursor,
            *sound,
            hide,
        )
        .map(Live::Screen),
        // The window's app's sound comes with it (ScreenCaptureKit filters it to the window on
        // a Mac, WASAPI's process loopback hears its app on Windows); none on Linux.
        SourceKind::Window { id, cursor } => ScreenCapture::start_with(
            Target::Window(*id),
            config.width,
            config.height,
            config.fps,
            *cursor,
            cfg!(any(target_os = "macos", target_os = "windows")),
            &[],
        )
        .map(Live::Screen),
        // Linux reads the speakers' monitor on its own, no display needed.
        #[cfg(target_os = "linux")]
        SourceKind::DesktopSound => {
            ScreenCapture::start_with(Target::Display(0), 2, 2, 1, false, true, &[]).map(Live::Screen)
        }
        // ScreenCaptureKit gives the sound with a display's picture: the smallest, slowest one.
        #[cfg(not(target_os = "linux"))]
        SourceKind::DesktopSound => crate::screen::shareable()
            .and_then(|s| {
                s.0.first()
                    .map(|d| d.id)
                    .ok_or_else(|| AvError::Source("there's no display to take the computer's sound with".into()))
            })
            .and_then(|d| ScreenCapture::start_with(Target::Display(d), 2, 2, 1, false, true, &[]))
            .map(Live::Screen),
        #[cfg(target_os = "macos")]
        SourceKind::App { display, app, sound, cursor } => {
            ScreenCapture::start_app(*display, app, config.width, config.height, config.fps, *cursor, *sound)
                .map(Live::Screen)
        }
        #[cfg(target_os = "macos")]
        SourceKind::AppSound(app) => crate::screen::shareable()
            .and_then(|s| {
                s.0.first()
                    .map(|d| d.id)
                    .ok_or_else(|| AvError::Source("there's no display to take the app's sound with".into()))
            })
            .and_then(|d| ScreenCapture::start_app(d, app, 2, 2, 1, false, true))
            .map(Live::Screen),
        #[cfg(target_os = "windows")]
        SourceKind::AppSound(app) => ScreenCapture::start_app_sound(app).map(Live::Screen),
        SourceKind::Camera(dev) => crate::camera::Capture::camera(dev).map(Live::Camera),
        SourceKind::Microphone(dev) => crate::camera::Capture::microphone(dev).map(Live::Microphone),
        _ => return None,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn open_capture(_: &EngineConfig, _: &SourceKind) -> Option<Result<Live, AvError>> {
    None
}

/// What a capture has received so far, whether it should keep receiving (a camera or a
/// microphone does; a screen only sends when something changes), and why it ended if it did.
fn signs(live: &Live) -> Option<(u64, bool, Option<String>)> {
    match live {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        Live::Screen(c) => Some((c.frames(), false, c.ended())),
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        Live::Camera(c) | Live::Microphone(c) => Some((c.received(), c.steady(), c.ended())),
        _ => None,
    }
}

/// Why a capture should be started again now, if it should: it faulted, isn't running (a
/// restart that failed), ended, or (one that sends all the time) has been quiet too long.
fn look(w: &mut Watch, running: bool, signs: Option<(u64, bool, Option<String>)>, now: u64) -> Option<String> {
    if w.faulted {
        return Some("it faulted".into());
    }
    if !running {
        return Some("it didn't start again last time".into());
    }
    let (count, steady, ended) = signs?;
    if count != w.count {
        w.count = count;
        w.since = now;
        w.tries = 0;
    }
    ended.or_else(|| {
        (steady && (count > 0 || w.tries > 0) && now.saturating_sub(w.since) > STALL_NS)
            .then(|| "it stopped sending".into())
    })
}

/// Opens a slow source on its own thread: what it plays, and whether it has sound to mix.
type Opener = Box<dyn FnOnce() -> Result<(Live, bool), AvError> + Send>;

/// How long a removed camera stays open in case it's added back (Undo, a rename).
const PARK_NS: u64 = 8_000_000_000;

/// How long adding a capture waits for it to open before handing back (the rest happens on
/// its own thread): short enough that the window never feels stuck.
const OPEN_WAIT: Duration = Duration::from_millis(250);

/// The watchdog: once a second it looks at every capture, and one that faulted, ended or
/// went quiet is started again (waiting longer after each try that didn't hold).
fn watchdog(config: EngineConfig, state: &Mutex<State>, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let now = now_ns();
        let mut todo = Vec::new();
        let mut expired = Vec::new();
        {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            // Parked cameras not wanted back: closed (outside the lock, below).
            let mut i = 0;
            while i < st.parked.len() {
                if now.saturating_sub(st.parked[i].2) >= PARK_NS {
                    expired.push(st.parked.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            for (id, src) in st.sources.iter_mut() {
                let Some(kind) = &src.kind else { continue };
                if src.opening.is_some() {
                    continue;
                }
                let running = !matches!(src.live, Live::Still);
                let w = &mut src.watch;
                let why = look(w, running, signs(&src.live), now);
                if let Some(why) = why
                    && now >= w.next_try
                {
                    w.next_try = now + backoff_ns(w.tries);
                    w.tries += 1;
                    w.faulted = false;
                    w.since = now;
                    // Taken out now (a camera is let go before it's opened again), dropped
                    // outside the lock.
                    let old = std::mem::replace(&mut src.live, Live::Still);
                    todo.push((*id, kind.clone(), why, old));
                }
            }
        }
        drop(expired);
        for (id, kind, why, old) in todo {
            drop(old);
            tracing::warn!(source = id, %why, "the watchdog starts a source again");
            match open_capture(&config, &kind) {
                Some(Ok(live)) => {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(src) = st.sources.get_mut(&id).filter(|s| s.kind.as_ref() == Some(&kind)) {
                        src.live = live;
                        src.watch.count = 0;
                        src.watch.since = now_ns();
                        st.stats.restarts += 1;
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!(source = id, error = %e, "the source didn't start again; it's tried later")
                }
                None => {}
            }
        }
    }
}

/// What a panic said.
fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(String::as_str)).unwrap_or("a fault")
}

/// One frame: sources taken in, sound mixed, each canvas drawn and its outputs fed.
fn frame(
    config: &EngineConfig,
    state: &Mutex<State>,
    gpu: &Compositor,
    mixer: &Mutex<Mixer>,
    ts: u64,
    behind: u64,
    started: Instant,
) {
    let rate = config.sample_rate;
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.stats.skipped += behind;
    let mut mx = mixer.lock().unwrap_or_else(|e| e.into_inner());
    // Only what's on air is heard (and monitored), as libobs mixes its program's tree; the
    // rest still has its sound taken, so it doesn't pile up.
    let heard = heard(&st);
    // Pictures go to the GPU only for sources something drawn shows (and a source's first,
    // for its size); the rest keep their newest until they're shown.
    let seen = seen(&st);
    // Sources: new pictures to the GPU, new sound to the mixer.
    let mut uploads = Vec::new();
    for (id, src) in st.sources.iter_mut() {
        if seen.contains(id)
            && let Some(f) = src.pending.take()
        {
            uploads.push((*id, f));
        }
    }
    let mut st_recovered = 0;
    for (id, src) in st.sources.iter_mut() {
        let took = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &mut src.live {
            Live::Still => {}
            Live::Image { src: img, since, last } => {
                if img.animated() {
                    let p = img.frame_at(since.elapsed());
                    uploads.push((*id, Fresh::Pic(p.clone())));
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
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Screen(cap) => {
                #[cfg(target_os = "macos")]
                if let Some(s) = cap.take_surface() {
                    uploads.push((*id, Fresh::Screen(s)));
                }
                // Windows' own capture hands BGRA over as it is; FFmpeg's gives pictures.
                #[cfg(target_os = "windows")]
                if let Some(s) = cap.take_surface() {
                    uploads.push((*id, Fresh::Screen(s)));
                } else if let Some(p) = cap.take() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                if let Some(p) = cap.take() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                for (pcm, t) in cap.take_sound() {
                    if heard.contains(id) {
                        mx.push(*id, &pcm, 2, 48_000, t);
                    }
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Camera(cap) => {
                // A Mac camera's frames go to the card as the camera holds them (no copy).
                #[cfg(target_os = "macos")]
                if let Some(s) = cap.take_surface() {
                    uploads.push((*id, Fresh::Screen(s)));
                }
                if let Some((p, _)) = cap.take_frame() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Microphone(cap) => {
                for (pcm, t) in cap.take_sound() {
                    if heard.contains(id) {
                        mx.push(*id, &pcm, 2, 48_000, t);
                    }
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Media(m) => {
                if let Some(p) = m.frame_at(ts) {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                for (pcm, t) in m.take_sound() {
                    if heard.contains(id) {
                        mx.push(*id, &pcm, 2, 48_000, t);
                    }
                }
            }
            Live::Fed(feed) | Live::Series { feed, .. } => {
                if let Some(p) = feed.0.picture.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                let sound: Vec<_> = feed.0.sound.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
                if heard.contains(id) {
                    for (pcm, rate, t) in sound {
                        mx.push(*id, &pcm, 2, rate, t);
                    }
                }
            }
        }));
        if let Err(p) = took {
            // This source alone stops (its last picture stays); the watchdog starts a capture again.
            tracing::error!(source = id, why = panic_text(&*p), "a source faulted: stopped");
            src.live = Live::Still;
            src.watch.faulted = true;
            st_recovered += 1;
        }
    }
    st.stats.recovered += st_recovered;
    for (id, f) in uploads {
        let Some(s) = st.sources.get_mut(&id) else { continue };
        if !seen.contains(&id) && s.size.is_some() {
            s.pending = Some(f);
            continue;
        }
        if f.send(gpu, id) {
            s.size = Some(f.info());
            s.sent += 1;
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

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
                    match pipe.audio_extra(i, &b.tracks[*t], b.ts_ns) {
                        Ok(t) => tags.extend(t),
                        Err(e) => failed = Some(e.to_string()),
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

    /// A capture that can't open says so to whoever added it, and isn't kept (macOS and Linux:
    /// on Windows looking for a camera through FFmpeg can take longer than the wait).
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn a_capture_that_cant_open_says_so_at_once() {
        let Some(e) = engine() else { return };
        let t = Instant::now();
        let r = e.set_source(7, SourceKind::Camera("/no/such/camera".into()));
        assert!(r.is_err(), "the error comes back: {r:?}");
        assert!(t.elapsed() < Duration::from_secs(2), "without waiting long");
        assert_eq!(e.opening(), 0);
        assert!(!e.lock().sources.contains_key(&7), "not kept");
    }

    /// A file that isn't there says so at once too (it opens on its own thread like a camera).
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn a_missing_media_file_says_so_at_once() {
        let Some(e) = engine() else { return };
        let r = e.set_source(9, SourceKind::Media { path: "/no/such/clip.mp4".into(), looping: false });
        assert!(r.is_err(), "the error comes back: {r:?}");
        assert!(!e.lock().sources.contains_key(&9), "not kept");
    }

    #[test]
    fn the_watchdog_starts_stuck_captures_again() {
        let s = 1_000_000_000;
        let mut w = Watch { since: 0, ..Watch::default() };
        // A camera sending: nothing to do; quiet 5 s: started again.
        assert_eq!(look(&mut w, true, Some((30, true, None)), s), None);
        assert_eq!(look(&mut w, true, Some((30, true, None)), 5 * s), None);
        assert!(look(&mut w, true, Some((30, true, None)), 7 * s).is_some());
        // A screen sends only on change: quiet isn't stuck; ended is.
        let mut w = Watch::default();
        assert_eq!(look(&mut w, true, Some((3, false, None)), 60 * s), None);
        assert!(look(&mut w, true, Some((3, false, Some("closed".into()))), 61 * s).is_some());
        // Faulted, or a restart that didn't take: at once.
        assert!(look(&mut Watch { faulted: true, ..Watch::default() }, true, None, 0).is_some());
        assert!(look(&mut Watch::default(), false, None, 0).is_some());
        // A camera that never sent isn't stuck (it may be warming up)…
        let mut w = Watch::default();
        assert_eq!(look(&mut w, true, Some((0, true, None)), 60 * s), None);
        // …unless it's one the watchdog restarted; sending again resets the tries.
        w.tries = 2;
        assert!(look(&mut w, true, Some((0, true, None)), 60 * s).is_some());
        assert_eq!(look(&mut w, true, Some((5, true, None)), 61 * s), None);
        assert_eq!(w.tries, 0);
        // Tries wait 2, 4, 8… seconds, a minute at most.
        assert_eq!((backoff_ns(0), backoff_ns(2), backoff_ns(9)), (2 * s, 8 * s, 60 * s));
    }

    #[test]
    fn a_fault_is_named() {
        let p = std::panic::catch_unwind(|| panic!("boom {}", 1)).unwrap_err();
        assert_eq!(panic_text(&*p), "boom 1");
        let p = std::panic::catch_unwind(|| std::panic::panic_any(7u8)).unwrap_err();
        assert_eq!(panic_text(&*p), "a fault");
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
        // A tile: the whole scene scaled to 64x36 (the blue square at 10,10 now at 1,1).
        let tile = e.snapshot_scene_at("A", (640, 360), (64, 36)).unwrap_or_default();
        assert_eq!(tile.len(), 64 * 36 * 4);
        assert_eq!(&tile[(30 * 64 + 50) * 4..(30 * 64 + 50) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&tile[(5 * 64 + 5) * 4..(5 * 64 + 5) * 4 + 4], &[0, 0, 255, 255]);
    }

    #[test]
    fn a_multiview_boxes_frames_and_names_the_scenes() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("A", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("B", vec![SceneItem::new(2, Placement::default())]);
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        let on_air = MultiviewBox {
            scene: None,
            area: (20.0, 20.0, 160.0, 90.0),
            frame: [0, 255, 0, 255],
            live: None,
            marks: Vec::new(),
            label: Some("Program".into()),
        };
        let b = MultiviewBox {
            scene: Some("B".into()),
            area: (220.0, 20.0, 160.0, 90.0),
            frame: [0; 4],
            live: Some([255, 255, 0, 255]),
            marks: Vec::new(),
            label: None,
        };
        let label = e.text.lock().unwrap_or_else(|x| x.into_inner()).render(
            "Program",
            &TextStyle {
                size: 14.0,
                color: [255, 255, 255, 255],
                background: Some([0, 0, 0, 170]),
                ..Default::default()
            },
        );
        let mv = Multiview { size: (400, 300), canvas: (640, 360), boxes: vec![(on_air, Some(label)), (b, None)] };
        let px = e.gpu.render(&multiview_scene(&e.lock(), &mv, now_ns())).unwrap_or_else(|x| panic!("{x}"));
        let at = |x: usize, y: usize| px[(y * 400 + x) * 4..(y * 400 + x) * 4 + 3].to_vec();
        assert_eq!(at(100, 40), [255, 0, 0], "what's on air in the first box");
        assert_eq!(at(300, 40), [0, 0, 255], "scene B in its own");
        assert_eq!(at(19, 60), [0, 255, 0], "the first box's frame");
        assert_eq!(at(219, 60), [0, 0, 0], "no frame round the second");
        assert_ne!(at(100, 103), [255, 0, 0], "the name along the first box's bottom");
        assert_eq!(at(200, 200), [0, 0, 0], "nothing outside the boxes");
        // B goes on air: its box is framed as live.
        e.go("B").unwrap_or_else(|x| panic!("{x}"));
        let px = e.gpu.render(&multiview_scene(&e.lock(), &mv, now_ns())).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(&px[(60 * 400 + 219) * 4..(60 * 400 + 219) * 4 + 3], &[255, 255, 0], "B framed as on air");
    }

    #[test]
    fn remove_background_keeps_the_person_and_blurs_or_drops_the_rest() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 255, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Fed { width: 4, height: 2 }).unwrap_or_else(|x| panic!("{x}"));
        // The person on the left half.
        let mut mask = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 255 } else { 0 };
                mask.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let feed = e.fed(2).unwrap_or_else(|| panic!("fed"));
        feed.picture(Picture { width: 4, height: 2, rgba: mask, premultiplied: false });
        let mut it = SceneItem::new(1, Placement::default());
        it.effects = vec![Effect::Cutout { mask: 2, blur: false, blur_px: 24.0, soft: 0.1 }];
        e.set_scene("A", vec![it]);
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        let mut px = Vec::new();
        for _ in 0..50 {
            px = e.snapshot().unwrap_or_else(|x| panic!("{x}"));
            if px[(180 * 640 + 600) * 4 + 1] < 10 && px[(180 * 640 + 40) * 4 + 1] > 245 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let at = |x: usize| px[(180 * 640 + x) * 4 + 1];
        assert!(at(40) > 245, "the person stays: {}", at(40));
        assert!(at(600) < 10, "the background goes: {}", at(600));
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
        let clip = std::env::temp_dir().join("ferrocore-stinger.mp4");
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
            (std::env::temp_dir().join("ferrocore-wide.mp4"), std::env::temp_dir().join("ferrocore-tall.mp4"));
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
        // GitHub's Macs open a sound device that never asks for sound: nothing to hear there.
        if std::env::var_os("CI").is_some() {
            return;
        }
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

    /// A dragged source moves in the engine's own item, and the preview's outline is worked out
    /// from that same item: picture and box can't drift apart.
    #[test]
    fn a_moved_item_and_its_outline_move_together() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 100, height: 50, rgba: [200, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut it = SceneItem::new(1, Placement { x: 10.0, y: 20.0, ..Default::default() });
        it.item_id = 7;
        e.set_scene("Main", vec![it]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let marks = crate::preview::Marks {
            scene: None,
            picked: Some(("Main".into(), vec![7])),
            colors: vec![[1.0, 0.0, 0.0, 1.0]],
        };
        let corners = |e: &Engine| {
            let st = e.lock();
            let o = outlines(&st, &marks, (640, 360), now_ns());
            assert_eq!(o.len(), 1);
            assert!(o[0].handles);
            o[0].corners
        };
        assert_eq!(corners(&e), [(10.0, 20.0), (110.0, 20.0), (110.0, 70.0), (10.0, 70.0)]);
        let to = Placement { x: 300.0, y: 100.0, scale_x: 2.0, scale_y: 2.0, ..Default::default() };
        assert!(e.set_item_placement("Main", 7, to));
        assert_eq!(corners(&e), [(300.0, 100.0), (500.0, 100.0), (500.0, 200.0), (300.0, 200.0)]);
        assert!(!e.set_item_placement("Main", 8, Placement::default()), "no such item: the long way");
        assert!(!e.set_item_placement("Other", 7, Placement::default()));
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
        let file = std::env::temp_dir().join("ferrocore-engine.mp4");
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
        let (one, two) = (dir.join("ferrocore-split-1.mp4"), dir.join("ferrocore-split-2.mp4"));
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
        assert!(e.split_output(id, &dir.join("ferrocore-split-3.mp4")).is_err());
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
        let (one, two) = (dir.join("ferrocore-rec-1.mkv"), dir.join("ferrocore-rec-2.mkv"));
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
        let file = std::env::temp_dir().join("ferrocore-scaled.mp4");
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
        for name in ["ferrocore-tracks.mp4", "ferrocore-tracks.mkv"] {
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
        let file = std::env::temp_dir().join("ferrocore-vertical.mp4");
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
        let file = std::env::temp_dir().join("ferrocore-replay.mkv");
        clip.write(&file).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let Some((secs, av)) = probed(&file) else {
            eprintln!("skipped the check: no ffprobe");
            return;
        };
        assert!(av && (1.9..3.2).contains(&secs), "{secs:.2} s");
        assert!(mean_db(&file, 0).is_some_and(|db| db > -40.0), "the sound came along");
    }

    #[test]
    fn a_fed_source_shows_what_it_is_handed() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Fed { width: 640, height: 360 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[0, 0, 0, 0], "clear until a picture comes");
        let feed = e.fed(1).unwrap_or_else(|| panic!("a fed source"));
        feed.picture(Picture::solid(320, 180, [255, 0, 0, 255]));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[255, 0, 0, 255], "the picture handed in");
        assert!(e.fed(2).is_none());
    }

    /// Stats' "what keeps the engine busiest": pictures each named source sent, and their size.
    #[test]
    fn each_source_counts_the_pictures_it_sends() {
        let Some(e) = engine() else { return };
        let kind = SourceKind::Fed { width: 640, height: 360 };
        e.set_source(1, kind.clone()).unwrap_or_else(|x| panic!("{x}"));
        e.lock().opened.insert("Phone".into(), (1, kind));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let feed = e.fed(1).unwrap_or_else(|| panic!("a fed source"));
        for _ in 0..3 {
            feed.picture(Picture::solid(320, 180, [0, 0, 255, 255]));
            std::thread::sleep(Duration::from_millis(80));
        }
        let load = e.source_load();
        assert_eq!(load.len(), 1);
        assert_eq!((load[0].name.as_str(), load[0].width, load[0].height), ("Phone", 320, 180));
        assert!(load[0].sent >= 2, "pictures counted: {}", load[0].sent);
    }

    /// The black screen fixer's list: a capture on air with no picture yet, not one still
    /// opening, nor one that's off air or has a picture.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn captures_on_air_with_no_picture_are_named() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            let mut add = |id: u64, name: &str, kind: SourceKind, size: Option<(u32, u32, bool)>, opening| {
                let mut s = Src::new(Live::Still, size, Some(kind.clone()));
                s.watch.since = 0;
                s.opening = opening;
                st.sources.insert(id, s);
                st.opened.insert(name.into(), (id, kind));
            };
            add(1, "Webcam", SourceKind::Camera("x".into()), None, None);
            add(2, "Game", SourceKind::Window { id: 7, cursor: true }, None, None);
            add(3, "Opening", SourceKind::Camera("y".into()), None, Some(9));
            add(4, "Fine", SourceKind::Camera("z".into()), Some((640, 360, true)), None);
            add(5, "Off air", SourceKind::Camera("w".into()), None, None);
        }
        let items = [1, 2, 3, 4].map(|id| SceneItem::new(id, Placement::default()));
        e.set_scene("Main", items.to_vec());
        e.set_scene("Other", vec![SceneItem::new(5, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.quiet_captures(1_000), vec![("Game".to_string(), true), ("Webcam".to_string(), false)]);
    }

    #[test]
    fn a_hidden_source_waits_off_the_gpu_until_shown() {
        let Some(e) = engine() else { return };
        for id in [1, 2] {
            e.set_source(id, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        }
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Other", vec![SceneItem::new(2, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let feed = e.fed(2).unwrap_or_else(|| panic!("a fed source"));
        feed.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        std::thread::sleep(Duration::from_millis(150));
        // Off air: kept back, not sent.
        assert!(e.lock().sources[&2].pending.is_some(), "kept, not uploaded");
        // A thumbnail of its scene sends it first.
        assert_eq!(&e.snapshot_scene("Other").unwrap_or_default()[0..4], &[0, 0, 255, 255]);
        assert!(e.lock().sources[&2].pending.is_none());
        // Another picture while hidden, then its scene goes on air: shown at once.
        feed.picture(Picture::solid(64, 36, [0, 255, 0, 255]));
        std::thread::sleep(Duration::from_millis(150));
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[0, 255, 0, 255], "the newest, on air");
    }
}
