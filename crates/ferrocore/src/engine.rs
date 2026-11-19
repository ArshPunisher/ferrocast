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
    AvError, Blend, Compositor, Content, Effect, Filter, Item, Nv12, PendingNv12, Picture, Placement, Scene,
    Transition, TransitionKind,
};

/// A camera to open: its device, and the size and frame rate to ask it for (its own when
/// none). A camera that can't do them gives the nearest it can.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CameraSpec {
    pub id: String,
    pub size: Option<(u32, u32)>,
    pub fps: Option<u32>,
}

impl From<&str> for CameraSpec {
    fn from(id: &str) -> Self {
        Self { id: id.to_string(), ..Self::default() }
    }
}

/// ffmpeg's options asking a camera for a size and frame rate (Windows' dshow, Linux's v4l2).
pub fn camera_args(spec: &CameraSpec) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((w, h)) = spec.size {
        out.extend(["-video_size".to_string(), format!("{w}x{h}")]);
    }
    if let Some(f) = spec.fps {
        out.extend(["-framerate".to_string(), f.to_string()]);
    }
    out
}

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
    /// Counts down for a set time or to a time of day, then says its end message.
    Countdown(crate::source::countdown::Countdown),
    /// Keys and mouse buttons, lit while held (only the layout's keys are looked at).
    Inputs(crate::source::inputs::InputSpec),
    /// Text read from a web address every so often.
    WebText(crate::source::webtext::WebText),
    /// Bars that move with the sound on air, or one source's.
    Visualizer(crate::source::visualizer::VisualizerSpec),
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
        /// The display's UUID (a Mac's): found by it when it opens, as its number changes
        /// when it's plugged back in or the Mac restarts.
        uuid: Option<String>,
        sound: bool,
        hide: Vec<String>,
        /// The mouse pointer drawn in the picture (OBS's Show Cursor / Capture Cursor).
        cursor: bool,
    },
    /// One window, by its window number, as it looks wherever it is; with `cursor`, the mouse
    /// pointer drawn in it. `app` and `title` (as the window list names them; empty when not
    /// known) find it again once its number is gone: the app quit and opened again.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Window {
        id: u32,
        cursor: bool,
        app: String,
        title: String,
    },
    /// One app's windows on a display (macOS, OBS's Application Capture), by bundle id or
    /// name; with `sound`, its sound too.
    #[cfg(target_os = "macos")]
    App {
        display: u32,
        uuid: Option<String>,
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
    /// A camera, by its id, with the size and frame rate asked of it.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Camera(CameraSpec),
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
    /// A picture as NV12, turned into RGB on the card (a playlist's video where FFmpeg plays it).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    yuv: Mutex<Option<crate::ffsrc::Nv12Picture>>,
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

    /// The same, as NV12 for the card (no RGB made on the CPU).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn yuv(&self, p: crate::ffsrc::Nv12Picture) {
        *self.0.yuv.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
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
    /// More video qualities a stream sends as Enhanced RTMP tracks 1… (Twitch Enhanced
    /// Broadcasting): each width, height and kbps, drawn from the same picture scaled down.
    pub video_tracks: Vec<(u32, u32, u32)>,
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
            video_tracks: Vec::new(),
        }
    }

    /// The canvas an extra video quality of this one is drawn on: the same picture, scaled to
    /// `width`×`height` from the size this one is drawn at.
    fn track(&self, width: u32, height: u32) -> Self {
        let (w, h) = self.scaled_from.map_or((self.width, self.height), |(w, h, _)| (w, h));
        Self {
            width,
            height,
            scaled_from: Some((w, h, Filter::Bicubic)),
            more_tracks: Vec::new(),
            video_tracks: Vec::new(),
            ..self.clone()
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

/// Where a frame tap's pictures go (the virtual camera): each NV12 frame drawn for it, with its
/// time (ns). Called on the frame loop with the engine's state held, so it must be quick (a
/// copy into shared memory).
pub type FrameSink = Box<dyn FnMut(&Nv12, u64) + Send>;

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

/// The loop's stats and error as of its last frame (see `Engine::stats`).
type Seen = Mutex<Shown>;

/// What readers see of the loop without waiting for it: as of its last frame.
#[derive(Debug, Default)]
struct Shown {
    stats: Stats,
    error: Option<String>,
    /// Each output's recording and encoder.
    outputs: Vec<(u64, RecordState, String)>,
    /// Each opened source's load (`Engine::source_load`).
    loads: Vec<SourceLoad>,
    /// Captures shown with no picture, not opening: name, whether it's the screen, and since
    /// when it's had none (`Engine::quiet_captures`).
    quiet: Vec<(String, bool, u64)>,
}

impl Shown {
    fn of(st: &State) -> Self {
        Self {
            stats: st.stats,
            error: st.error.clone(),
            outputs: st.outputs.iter().map(|(id, pipe, ..)| (*id, record_state(pipe), pipe.encoder())).collect(),
            loads: loads_of(st),
            quiet: quiet_of(st),
        }
    }
}

fn loads_of(st: &State) -> Vec<SourceLoad> {
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

/// Captures shown somewhere with no picture yet and not opening: name, screen, since when.
fn quiet_of(st: &State) -> Vec<(String, bool, u64)> {
    let shown = seen(st);
    st.opened
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
            (shown.contains(id) && s.size.is_none() && s.opening.is_none())
                .then(|| (name.clone(), screen, s.watch.since))
        })
        .collect()
}

fn record_state(pipe: &Pipeline) -> RecordState {
    RecordState {
        paused: pipe.paused() || pipe.recording_paused(),
        bytes: pipe.recorded_bytes(),
        splitting: pipe.splitting(),
    }
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
    /// Time the outputs' encoders take for one frame's pictures, all together, in ms (smoothed):
    /// near the frame's whole time, the encoder is what holds the frames back (19 Nov: 4K60
    /// streamed and recorded at once, the Mac's one encoder at its limit, 31 of 60 fps).
    pub encode_ms: f32,
    /// All the time the encoders took since the start, in µs: against the clock, the share of
    /// the frame loop's time spent encoding (an output encoding every other frame counts half).
    pub encoding_us: u64,
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
    /// Sound bars: drawn each frame from what was heard.
    Visualizer(Box<crate::source::visualizer::VisualizerLive>),
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
    /// A file's picture as FFmpeg decodes it, turned into RGB on the card (Windows, Linux).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Yuv(crate::ffsrc::Nv12Picture),
}

impl Fresh {
    /// Width, height, and whether the colours are already multiplied by alpha.
    fn info(&self) -> (u32, u32, bool) {
        match self {
            Fresh::Pic(p) => (p.width, p.height, p.premultiplied),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(s) => (s.size().0, s.size().1, true),
            Fresh::Yuv(p) => (p.width, p.height, false),
        }
    }

    /// Sends it to the card as source `id`'s picture.
    fn send(&self, gpu: &Compositor, id: u64) -> bool {
        match self {
            Fresh::Pic(p) => gpu.upload(id, p).is_ok(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(s) => gpu.upload_surface(id, s).is_ok(),
            Fresh::Yuv(p) => gpu.upload_nv12(id, p).is_ok(),
        }
    }

    /// Converts any pinned zero-copy external surface into a CPU picture before parking in
    /// `Src::pending` so finite capture buffer pools aren't starved while hidden.
    fn into_pending(self) -> Option<Self> {
        match self {
            #[cfg(target_os = "macos")]
            Fresh::Screen(s) => s.to_picture().map(Fresh::Pic),
            #[cfg(target_os = "windows")]
            Fresh::Screen(s) => Some(Fresh::Pic(s.to_picture())),
            other => Some(other),
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
    /// A scene drawn over whatever is on air, transitions included (the top layer).
    top: Option<String>,
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
    /// Sources the last load couldn't open (a camera unplugged…), by name, with the id their
    /// items were given: an edit that leaves them as they were needs no full load again.
    missing: HashMap<String, (u64, SourceKind, String)>,
    /// When the last full load was: sources that couldn't open are tried again by a full load
    /// at most this often ([`MISSING_RETRY`]), not on every slider step.
    loaded_at: Option<Instant>,
    /// The collection's own scenes and groups at the last load, top layer and global sources (by
    /// name): an edit that keeps them all is applied in place ([`Engine::apply_same_sources`]).
    loaded: Option<Loaded>,
    /// Scenes a full load took out while a transition or stinger still played from them: kept
    /// until it ends (Studio Mode's copy on air left the collection right after its fade
    /// started, and an edit then cut the fade off).
    lingering: Vec<String>,
    outputs: Vec<(u64, Pipeline, OutputKind, Canvas)>,
    next_output: u64,
    /// Pictures handed out as drawn, not encoded (the virtual camera): id, canvas, sink.
    taps: Vec<(u64, Canvas, FrameSink)>,
    video: VideoConfig,
    audio: AudioConfig,
    /// Where monitored sound is played, when monitoring is on.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor: Option<crate::monitor::Feeder>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor_epoch: u64,
    stats: Stats,
    error: Option<String>,
    /// The previews windows show, by id.
    previews: HashMap<u64, Preview>,
    /// Frames drawn and on their way back from the card, oldest first (`deliver`).
    in_flight: std::collections::VecDeque<InFlight>,
    /// Cameras just taken out, still running for [`PARK_NS`]: one added again meanwhile (Undo,
    /// a rename, the same camera added back) comes back at once instead of being opened again,
    /// which takes seconds on Windows. What each was, and when it was parked.
    parked: Vec<(SourceKind, Live, u64)>,
}

/// How often an edit applied in place lets a full load try again the sources that couldn't
/// open.
const MISSING_RETRY: Duration = Duration::from_secs(5);

/// What a collection's load put in: compared with an edited collection to see whether only
/// settings changed.
#[derive(Debug, Clone, PartialEq)]
struct Loaded {
    scenes: Vec<String>,
    groups: Vec<String>,
    top: Option<String>,
    globals: Vec<String>,
    /// The scene the collection had on air (an edit in place leaves what's on air alone).
    current: Option<String>,
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
    /// Shared with countdowns, so fonts are loaded once.
    text: Arc<Mutex<TextRenderer>>,
    stop: Arc<AtomicBool>,
    /// The loop's stats and error as of its last frame, for those who only read them (/stats
    /// every second, health): they never wait for the state lock the loop holds a whole frame
    /// (on a slow card, every Studio call through the window's thread waited behind it).
    seen: Arc<Seen>,
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
            top: None,
            moving: None,
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            stinger: None,
            started: std::collections::HashMap::new(),
            groups: Vec::new(),
            globals: std::collections::HashSet::new(),
            opened: HashMap::new(),
            missing: HashMap::new(),
            loaded_at: None,
            loaded: None,
            lingering: Vec::new(),
            outputs: Vec::new(),
            next_output: 1,
            taps: Vec::new(),
            video: VideoConfig { width: config.width, height: config.height, fps_num: config.fps, fps_den: 1, ..video },
            audio: AudioConfig { sample_rate: config.sample_rate, ..audio },
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: None,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor_epoch: 0,
            stats: Stats::default(),
            error: None,
            previews: HashMap::new(),
            in_flight: std::collections::VecDeque::new(),
            parked: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let seen: Arc<Seen> = Arc::default();
        let thread = {
            let (state, gpu, mixer, stop, seen) =
                (state.clone(), gpu.clone(), mixer.clone(), stop.clone(), seen.clone());
            std::thread::Builder::new()
                .name("ferrocast-engine".into())
                .spawn(move || run(config, &state, &gpu, &mixer, &stop, &seen))
                .map_err(|e| AvError::Source(format!("the engine's thread didn't start ({e})")))?
        };
        let watchdog = {
            let (state, mixer, stop) = (state.clone(), mixer.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-watchdog".into())
                .spawn(move || watchdog(config, &state, &mixer, &stop))
                .ok()
        };
        Ok(Self {
            config,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: Mutex::new(None),
            state,
            gpu,
            mixer,
            text: Arc::new(Mutex::new(TextRenderer::new())),
            stop,
            seen,
            thread: Some(thread),
            watchdog,
            preview: Mutex::new(HashMap::new()),
            next_preview: AtomicU64::new(1),
        })
    }

    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// A source (re)placed: in the mix with its settings kept (mute and volume belong to the
    /// source, not to the device it opens; resetting them made a muted source loud, 20 Nov), and
    /// no longer marked as capturing an output until it says so again.
    fn reset_sound(&self, id: u64) {
        let mut mx = self.mixer.lock().unwrap_or_else(|e| e.into_inner());
        mx.ensure(id);
        mx.set_output_capture(id, None);
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
                self.reset_sound(id);
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
                    self.reset_sound(id);
                }
                (live, None)
            }
            #[cfg(target_os = "windows")]
            SourceKind::AppSound(_) => {
                let live = open_capture(&self.config, &kind)
                    .unwrap_or_else(|| Err(AvError::Source("not a capture".into())))?;
                self.reset_sound(id);
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
                    self.reset_sound(id);
                }
                if let Some(live) = self.unpark(&kind) {
                    tracing::info!(source = id, "a camera taken out a moment ago came back without opening again");
                    self.lock().sources.insert(id, Src::new(live, None, Some(kind)));
                    return Ok(());
                }
                // The same camera parked with other settings (a new size or frame rate): let it go
                // first, or two sessions share the device and neither gets a picture (owner's
                // test, 10 Nov).
                self.unpark_device(&kind);
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
                        open_video(&path, looping).map(|m| {
                            let heard = m.has_audio;
                            (Live::Media(m), heard)
                        })
                    }),
                );
            }
            SourceKind::Visualizer(spec) => {
                let (w, h) = (spec.width.clamp(16, 3840), spec.height.clamp(8, 2160));
                let live = crate::source::visualizer::VisualizerLive::new(spec);
                (Live::Visualizer(Box::new(live)), Some(Picture::solid(w, h, [0, 0, 0, 0])))
            }
            SourceKind::Countdown(c) => {
                let feed = Feed::default();
                let worker = crate::source::countdown::countdown(c, feed.clone(), self.text.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            SourceKind::Inputs(spec) => {
                crate::source::inputs::ask_once();
                let (w, h) = crate::source::inputs::size(&spec);
                let feed = Feed::default();
                let keys = Box::new(crate::source::inputs::System);
                let worker = crate::source::inputs::input_overlay(spec, keys, feed.clone(), self.text.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(w.max(1), h.max(1), [0, 0, 0, 0])))
            }
            SourceKind::WebText(w) => {
                let feed = Feed::default();
                let worker = crate::source::webtext::web_text(w, feed.clone(), self.text.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            SourceKind::Slides(s) => {
                let feed = Feed::default();
                let worker = crate::source::series::slideshow(s, feed.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            SourceKind::Playlist(p) => {
                self.reset_sound(id);
                let feed = Feed::default();
                let worker = crate::source::series::playlist(p, feed.clone());
                (Live::Series { feed, _worker: worker }, Some(Picture::solid(1, 1, [0, 0, 0, 0])))
            }
            SourceKind::Fed { width, height } => {
                self.reset_sound(id);
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
        let gone = {
            let mut st = self.lock();
            // A capture opened in place (an App capture handed over) keeps the old picture on
            // the card until its first one comes.
            let size = size.or_else(|| restart.as_ref().and(st.sources.get(&id).and_then(|s| s.size)));
            st.sources.insert(id, Src::new(live, size, restart))
        };
        drop(gone);
        Ok(())
    }

    /// Stops any parked camera on the same device as `kind` (its settings differ), outside the
    /// lock.
    fn unpark_device(&self, kind: &SourceKind) {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        {
            let SourceKind::Camera(want) = kind else { return };
            let gone: Vec<_> = {
                let mut st = self.lock();
                let (same, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.parked)
                    .into_iter()
                    .partition(|(k, ..)| matches!(k, SourceKind::Camera(c) if c.id == want.id));
                st.parked = keep;
                same
            };
            drop(gone);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let _ = kind;
    }

    /// A parked capture of this kind, taken back (see `State::parked`). Disconnected or ended
    /// captures are dropped outside the lock and never handed back.
    fn unpark(&self, kind: &SourceKind) -> Option<Live> {
        let mut st = self.lock();
        let at = st.parked.iter().position(|(k, ..)| k == kind)?;
        let (_, live, _) = st.parked.swap_remove(at);
        drop(st);
        if is_live_ended(&live) {
            drop(live);
            return None;
        }
        Some(live)
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
            let mut st = self.lock();
            match st.sources.get_mut(&id) {
                // Already here with a picture (a screen capture handed over): it keeps running
                // and showing until the new one is open.
                Some(src) if src.size.is_some() => {
                    src.kind = restart;
                    src.opening = Some(token);
                }
                _ => {
                    let mut src = Src::new(Live::Still, None, restart);
                    src.opening = Some(token);
                    // Whatever was there before goes after the lock (stopping it can wait).
                    let gone = st.sources.insert(id, src);
                    drop(st);
                    drop(gone);
                }
            }
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
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    let oc = output_capture_of(&live, src.kind.as_ref());
                    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                    let oc: Option<crate::mix::OutputCapture> = None;
                    // The one it took over from (when handed over) stops outside the lock.
                    leftover = Some(std::mem::replace(&mut src.live, live));
                    src.opening = None;
                    src.watch = Watch { since: now_ns(), ..Watch::default() };
                    // Its sound into the mix (a file with sound), under the same lock so a
                    // removal can't slip in between (state, then mixer: the frame loop's order).
                    if heard || oc.is_some() || matches!(src.live, Live::Screen(_)) {
                        let mut mx = mixer.lock().unwrap_or_else(|e| e.into_inner());
                        if heard {
                            mx.ensure(id);
                        }
                        mx.set_output_capture(id, oc);
                    }
                    Ok(())
                }
                (Err(e), Some(src)) => {
                    src.opening = None;
                    // The new one didn't open: the old one handed over stops too (the watchdog
                    // starts what was asked for, not what was there), and no stale picture.
                    leftover = Some(std::mem::replace(&mut src.live, Live::Still));
                    src.size = None;
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
                // Its slot in the mix too (state, then mixer: the frame loop's order), else
                // settings or filters set while it opened stayed there with nothing to play.
                mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
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
                    drop(st);
                    // A picture left from a capture it took over from.
                    self.gpu.forget(id);
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
                // A running, healthy camera is parked a moment (see `State::parked`).
                Some(src)
                    if src.opening.is_none()
                        && matches!(src.kind, Some(SourceKind::Camera(_)))
                        && !is_live_ended(&src.live) =>
                {
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

    /// Low latency sound (Settings): the mixer waits at most 60 ms past its filters' delay.
    pub fn set_low_latency(&self, on: bool) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_low_latency(on);
    }

    /// Meters read true peak (4x oversampled) or sample peak.
    pub fn set_true_peak(&self, on: bool) {
        self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_true_peak(on);
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
        let feeder = m.as_ref().map(crate::monitor::Monitor::feeder);
        let dev_id = feeder.as_ref().and_then(crate::monitor::Feeder::device_id);
        let epoch = feeder.as_ref().map_or(0, crate::monitor::Feeder::device_epoch);
        {
            let mut st = self.lock();
            st.monitor = feeder;
            st.monitor_epoch = epoch;
            self.mixer.lock().unwrap_or_else(|e| e.into_inner()).set_monitor_device(dev_id);
        }
        // Drop old monitor outside both `state` and `monitor` locks (its drop joins its thread).
        let old = std::mem::replace(&mut *self.monitor.lock().unwrap_or_else(|e| e.into_inner()), m);
        drop(old);
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
    /// A scene's items as the engine has them (None: no such scene).
    pub fn scene(&self, name: &str) -> Option<Vec<SceneItem>> {
        self.lock().scenes.get(name).cloned()
    }

    pub fn set_scene(&self, name: &str, items: Vec<SceneItem>) {
        self.lock().scenes.insert(name.to_string(), items);
    }

    /// Opens an OBS scene collection (the JSON OBS and Ferrocast's Studio save): its sources,
    /// scenes and the scene on air replace what the engine had. Returns what couldn't come over
    /// (sources or filters it doesn't run yet, files that are gone).
    /// Opened again (the Studio changed), a source with the same name and settings keeps
    /// running: a camera doesn't blink when something else moves.
    pub fn load_obs(&self, doc: &serde_json::Value) -> Vec<String> {
        let before: HashMap<String, SourceKind> =
            self.lock().opened.iter().map(|(n, (_, k))| (n.clone(), k.clone())).collect();
        let mut imported =
            crate::obs::import_with(doc, (self.config.width, self.config.height), &|n| before.get(n).cloned());
        let mut skipped = std::mem::take(&mut imported.skipped);
        // Which imported sources are already here, running as they should; the rest get ids
        // after every id in use. A screen capture switched to another method (Screen, Window,
        // App) keeps its id and its last picture until the new one opens: no black between
        // (owner's recording, 10 Nov).
        let (keep, handed, ids) = {
            let st = self.lock();
            // Ids the engine keeps for itself (the stinger's video) aren't counted from.
            let mut next = st.sources.keys().filter(|id| **id != STINGER).max().map_or(1, |m| m + 1);
            let mut ids: HashMap<u64, u64> = HashMap::new();
            let mut keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut handed: std::collections::HashSet<u64> = std::collections::HashSet::new();
            for src in &imported.sources {
                match st.opened.get(&src.name) {
                    Some((id, kind)) if *kind == src.kind && st.sources.contains_key(id) && !keep.contains(id) => {
                        ids.insert(src.id, *id);
                        keep.insert(*id);
                    }
                    Some((id, kind))
                        if hands_over(kind, &src.kind)
                            && st.sources.contains_key(id)
                            && !keep.contains(id)
                            && !handed.contains(id) =>
                    {
                        ids.insert(src.id, *id);
                        handed.insert(*id);
                    }
                    _ => {
                        ids.insert(src.id, next);
                        next += 1;
                    }
                }
            }
            (keep, handed, ids)
        };
        // Sources gone or changed are let go first (a camera parked: its device is free for
        // the new one); a stinger under way plays on.
        type Marked = Vec<(u64, bool)>;
        let (old_devices, old_rest): (Marked, Marked) = {
            let st = self.lock();
            st.sources
                .iter()
                .filter(|(id, _)| !keep.contains(*id) && !handed.contains(*id) && **id != STINGER)
                .map(|(id, src)| (*id, src.kind.as_ref().is_some_and(one_at_a_time)))
                .partition(|(_, device)| *device)
        };
        let old_devices: Vec<u64> = old_devices.into_iter().map(|(id, _)| id).collect();
        let old_rest: Vec<u64> = old_rest.into_iter().map(|(id, _)| id).collect();
        // A device (camera, microphone) is let go before it's opened again (two sessions on one
        // device give no picture); the rest go once the new collection is in, so a changed
        // text or picture doesn't blink out meanwhile.
        for id in old_devices {
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
        let loaded = Loaded {
            scenes: imported.scenes.iter().map(|(n, _)| n.clone()).collect(),
            groups: imported.groups.clone(),
            top: imported.top_layer.clone(),
            globals: imported.sources.iter().filter(|s| s.global).map(|s| s.name.clone()).collect(),
            current: imported.current.clone(),
        };
        // New sources opened, and every source's sound set, while the scenes on air go on
        // drawing (nothing shows a new source until the scenes are swapped in below).
        let (mut opened, mut missing, mut globals) = (HashMap::new(), HashMap::new(), std::collections::HashSet::new());
        for mut s in imported.sources {
            s.id = to(s.id);
            let started = if keep.contains(&s.id) { Ok(()) } else { self.set_source(s.id, s.kind.clone()) };
            match started {
                Ok(()) => {
                    if let Some(sound) = s.sound {
                        self.set_sound(s.id, sound);
                        self.set_sound_effects(s.id, &s.sound_effects);
                    }
                    opened.insert(s.name.clone(), (s.id, s.kind));
                }
                Err(e) => {
                    let why = format!("{}: {e}", s.name);
                    skipped.push(why.clone());
                    missing.insert(s.name.clone(), (s.id, s.kind, why));
                }
            }
            if s.global {
                globals.insert(s.id);
            }
        }
        {
            // Everything at once, in one lock: no frame is ever drawn (or heard) between the old
            // collection and the new one. A full load used to clear the scenes, take the scene
            // off air and put it back in many steps: a frame could go out black and every
            // source's sound in it was thrown away (half a second of a video's, 18 Nov), and
            // the scene's clock and any transition started over.
            let mut st = self.lock();
            let was_loaded = st.loaded.take();
            let ours: Vec<String> = was_loaded.as_ref().map(|l| l.scenes.clone()).unwrap_or_default();
            // Scenes of the engine's own (fade to black, a source recorded alone, Studio Mode's
            // copy on air) aren't in the collection: they stay.
            let new_names: std::collections::HashSet<String> = imported.scenes.iter().map(|(n, _)| n.clone()).collect();
            // A scene a transition or stinger still plays from stays until it ends.
            let mut playing: Vec<String> = st.moving.iter().map(|(from, ..)| from.clone()).collect();
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            playing.extend(st.stinger.iter().map(|(from, ..)| from.clone()));
            let mut lingering = std::mem::take(&mut st.lingering);
            // One back in the collection (an undo mid-fade) is the collection's again.
            lingering.retain(|n| !new_names.contains(n));
            for p in &playing {
                if ours.contains(p) && !new_names.contains(p) && !lingering.contains(p) {
                    lingering.push(p.clone());
                }
            }
            st.scenes.retain(|name, _| {
                (!ours.contains(name) || lingering.contains(name)) && !imported.groups.contains(name)
            });
            st.lingering = lingering;
            for (name, items) in imported.scenes {
                st.scenes.insert(name, items);
            }
            let names: std::collections::HashSet<String> = st.scenes.keys().cloned().collect();
            st.started.retain(|name, _| names.contains(name));
            // The collection's own pick of what's on air changed (another collection, an undo):
            // that goes on air. Otherwise what's on air stays, with its clock, unless it's gone.
            let picked = loaded.current.clone().filter(|c| names.contains(c));
            let changed = was_loaded.as_ref().is_none_or(|l| l.current != loaded.current);
            let stays = st.on_air.as_ref().is_some_and(|n| names.contains(n));
            // A scene of the engine's own on air (fade to black, a copy in Studio Mode) is never
            // replaced by a load: the real scene flashed on stream over black (peer, 18 Nov).
            let own_on_air = stays && st.on_air.as_ref().is_some_and(|n| !new_names.contains(n));
            if (changed && !own_on_air) || !stays {
                let to_air = picked.or_else(|| loaded.scenes.iter().find(|n| !imported.groups.contains(n)).cloned());
                if st.on_air != to_air {
                    st.on_air = to_air.clone();
                    st.moving = None;
                    if let Some(n) = to_air {
                        st.started.insert(n, now_ns());
                    }
                }
            }
            if st.moving.as_ref().is_some_and(|(from, ..)| !names.contains(from)) {
                st.moving = None;
            }
            st.top = imported.top_layer;
            st.groups = imported.groups;
            st.globals = globals;
            st.opened = opened;
            st.missing = missing;
            st.loaded = Some(loaded);
            st.loaded_at = Some(Instant::now());
        }
        for id in old_rest {
            self.remove_source(id);
        }
        self.load_audio_care(doc);
        skipped
    }

    /// Applies an edited collection in place when it has the same sources (by name and kind),
    /// scenes, groups, top layer and global sources as the one loaded: each scene's items
    /// (moves, filters, visibility…) and each source's sound settings and sound filters change,
    /// nothing else. No source is opened or closed, the scene on air stays on air with its
    /// clock, a transition or stinger goes on, and no frame is drawn without its scene (a full
    /// [`Self::load_obs`] clears all that for a moment: a slider dragged while live broke the
    /// sound and could flash black, 18 Nov). None: more changed, and nothing was touched (the
    /// caller loads it in full). Some: what couldn't come over, as `load_obs` says.
    pub fn apply_same_sources(&self, doc: &serde_json::Value) -> Option<Vec<String>> {
        let before: HashMap<String, SourceKind> = {
            let st = self.lock();
            st.loaded.as_ref()?;
            // A source that couldn't open (a camera unplugged) is tried again by a full load
            // now and then, as every edit's full load used to.
            // So is one that opened late and failed then (a file that took a moment and turned
            // out broken): it's gone from the sources, though it was opened (CodeRabbit).
            let gone = st.opened.values().any(|(id, _)| !st.sources.contains_key(id));
            if (!st.missing.is_empty() || gone) && st.loaded_at.is_none_or(|t| t.elapsed() >= MISSING_RETRY) {
                return None;
            }
            st.opened
                .iter()
                .map(|(n, (_, k))| (n.clone(), k.clone()))
                .chain(st.missing.iter().map(|(n, (_, k, _))| (n.clone(), k.clone())))
                .collect()
        };
        // Read outside the lock: the frame loop keeps going meanwhile.
        let imported =
            crate::obs::import_with(doc, (self.config.width, self.config.height), &|n| before.get(n).cloned());
        let loaded = Loaded {
            scenes: imported.scenes.iter().map(|(n, _)| n.clone()).collect(),
            groups: imported.groups.clone(),
            top: imported.top_layer.clone(),
            globals: imported.sources.iter().filter(|s| s.global).map(|s| s.name.clone()).collect(),
            current: imported.current.clone(),
        };
        let mut st = self.lock();
        let same =
            st.loaded.as_ref().is_some_and(|l| Loaded { current: loaded.current.clone(), ..l.clone() } == loaded);
        if !same || imported.sources.len() != st.opened.len() + st.missing.len() {
            return None;
        }
        // Every source the same one by name and kind; its id here (the imported ids are the
        // order the document lists them in).
        let mut ids: HashMap<u64, u64> = HashMap::new();
        for src in &imported.sources {
            let here = st
                .opened
                .get(&src.name)
                .map(|(id, k)| (id, k))
                .or_else(|| st.missing.get(&src.name).map(|(id, k, _)| (id, k)));
            match here {
                Some((id, kind)) if kind == &src.kind => {
                    ids.insert(src.id, *id);
                }
                _ => return None,
            }
        }
        let to = |id: u64| ids.get(&id).copied().unwrap_or(id);
        for (name, mut items) in imported.scenes {
            for it in &mut items {
                it.source = to(it.source);
                for e in &mut it.effects {
                    if let Effect::Mask { image, .. } | Effect::Cutout { mask: image, .. } = e {
                        *image = to(*image);
                    }
                }
            }
            // Unchanged scenes are left as they are (cheap to compare, and nothing is redrawn
            // for them).
            if st.scenes.get(&name) != Some(&items) {
                st.scenes.insert(name, items);
            }
        }
        if let Some(l) = st.loaded.as_mut() {
            l.current = loaded.current;
        }
        let running: std::collections::HashSet<String> = st.opened.keys().cloned().collect();
        // Why each that couldn't open didn't, as the full load said.
        let mut skipped = imported.skipped;
        skipped.extend(st.missing.values().map(|(.., why)| why.clone()));
        drop(st);
        for s in imported.sources {
            if let Some(sound) = s.sound.filter(|_| running.contains(&s.name)) {
                let id = to(s.id);
                self.set_sound(id, sound);
                self.set_sound_effects(id, &s.sound_effects);
            }
        }
        self.load_audio_care(doc);
        Some(skipped)
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
        self.now_or_last(loads_of, |s| s.loads.clone())
    }

    /// Captures shown somewhere (on air, a preview, an output) that have had no picture for at
    /// least `quiet_ns` (and aren't still opening): each one's name, and whether it captures the
    /// screen (a window, a display, an app) rather than a camera. The Studio's black screen
    /// fixer says why and what to do.
    pub fn quiet_captures(&self, quiet_ns: u64) -> Vec<(String, bool)> {
        let now = now_ns();
        let mut out: Vec<(String, bool)> = self
            .now_or_last(quiet_of, |s| s.quiet.clone())
            .into_iter()
            .filter(|(.., since)| now.saturating_sub(*since) >= quiet_ns)
            .map(|(name, screen, _)| (name, screen))
            .collect();
        out.sort();
        out
    }

    /// `now` on the state when its lock is free; else from what the loop showed after its last
    /// frame: read every second from the window's thread (/stats), never waiting for the frame
    /// being made (a whole one on a slow card: 10-48 s on Windows' runner, 19 Nov).
    fn now_or_last<T>(&self, now: impl FnOnce(&State) -> T, last: impl FnOnce(&Shown) -> T) -> T {
        match self.state.try_lock() {
            Ok(st) => now(&st),
            Err(std::sync::TryLockError::Poisoned(p)) => now(&p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => last(&self.seen.lock().unwrap_or_else(|p| p.into_inner())),
        }
    }

    /// What readers see refreshed after a change to the outputs (not only at the next frame: a
    /// split read as not begun would be asked for again; projects-60).
    fn show(&self, st: &State) {
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Shown::of(st);
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

    /// A group's box (its own size, its sources placed inside it), when `name` is a group
    /// some scene shows and it isn't empty.
    pub fn group_size(&self, name: &str) -> Option<(u32, u32)> {
        let st = self.lock();
        if !st.groups.iter().any(|g| g == name) {
            return None;
        }
        st.scenes
            .values()
            .flatten()
            .find_map(|i| (i.scene.as_deref() == Some(name)).then_some(i.group).flatten())
            .filter(|&(w, h)| w > 0 && h > 0)
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
        // A switch made here is the collection's own pick now (a later load doesn't count it
        // as a change and switch over whatever is on air then, such as black).
        if let Some(l) = st.loaded.as_mut()
            && l.scenes.iter().any(|s| s == name)
        {
            l.current = Some(name.to_string());
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
        let m = open_video(video, false)?;
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
        if let Some(l) = st.loaded.as_mut()
            && l.scenes.iter().any(|s| s == name)
        {
            l.current = Some(name.to_string());
        }
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
            // Twitch Enhanced Broadcasting's other qualities: H.264, even sides, a few at most.
            if !canvas.video_tracks.is_empty() {
                if canvas.video_tracks.len() > 4
                    || canvas
                        .video_tracks
                        .iter()
                        .any(|(w, h, k)| *w < 2 || *h < 2 || w % 2 != 0 || h % 2 != 0 || *k == 0)
                {
                    return Err(AvError::Source("extra qualities: at most 4, even sides and a bitrate each".into()));
                }
                let configs: Vec<VideoConfig> = canvas
                    .video_tracks
                    .iter()
                    .map(|&(width, height, bitrate_kbps)| VideoConfig {
                        width,
                        height,
                        bitrate_kbps,
                        codec: crate::video::VideoCodec::H264,
                        ..video
                    })
                    .collect();
                let added = pipe.add_video_tracks(&configs);
                tracing::info!(asked = configs.len(), added, "more qualities in the stream");
            }
        }
        if let Some((seconds, mb)) = replay {
            pipe.keep_replay(seconds, mb);
        }
        // Which encoder this output got: libx264 here means the hardware ones failed their test.
        tracing::info!(encoder = %pipe.encoder(), replay = replay.is_some(), "an output starts");
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
        self.show(&st);
        Ok(id)
    }

    /// Hands every picture drawn on `canvas` to `sink` as NV12, not encoded (the virtual
    /// camera). A recording on the same canvas shares the drawing. → its id, for `stop_tap`.
    pub fn tap_frames(&self, canvas: Canvas, sink: FrameSink) -> Result<u64, AvError> {
        if canvas.every == 0
            || canvas.width < 2
            || canvas.height < 2
            || !canvas.width.is_multiple_of(2)
            || !canvas.height.is_multiple_of(2)
        {
            return Err(AvError::Source("a frame tap needs even sides and a rate divisor of 1 or more".into()));
        }
        let mut st = self.lock();
        if let Some(name) = &canvas.scene
            && !st.scenes.contains_key(name)
        {
            return Err(AvError::Source(format!("there's no scene called \"{name}\"")));
        }
        // Ids shared with outputs, so neither is mistaken for the other.
        let id = st.next_output;
        st.next_output += 1;
        st.taps.push((id, canvas, sink));
        Ok(id)
    }

    /// Stops a frame tap (its sink is dropped). False when there's no such tap.
    pub fn stop_tap(&self, id: u64) -> bool {
        let taken = {
            let mut st = self.lock();
            st.taps.iter().position(|(i, ..)| *i == id).map(|at| st.taps.remove(at))
        };
        // Dropped outside the lock: a sink may close shared memory or wait on a thread.
        taken.is_some()
    }

    /// Which encoder an output's pictures go through ("h264_nvenc", "VideoToolbox", "libx264").
    pub fn output_encoder(&self, id: u64) -> Option<String> {
        self.now_or_shown(id, |pipe| Ok(pipe.encoder()), |(_, _, e)| e.clone())?.ok()
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
        // The same scene: nothing to change (every edit sets it again; flushing would wait
        // for the card each time).
        if st.outputs.iter().any(|(i, .., c)| *i == id && c.scene == scene) {
            return Ok(());
        }
        // Frames drawn for the old scene go out first: matched by canvas, they'd be dropped
        // once it names the new one, while their sound still went (review).
        deliver(&mut st, &self.gpu, true);
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
        self.with_output_flushed(id, |pipe| pipe.chapter(name))
    }

    /// Pauses or resumes output `id`'s recording, no gap in the file: the whole output when it
    /// only records, the file alone when a stream shares its encode (the stream goes on).
    pub fn pause_recording(&self, id: u64, on: bool) -> Result<(), AvError> {
        let mut st = self.lock();
        deliver(&mut st, &self.gpu, true);
        let (_, pipe, kind, _) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        if pipe.recording_paused() || (on && matches!(kind, OutputKind::Stream(_))) {
            pipe.pause_recording(on);
        } else {
            pipe.pause(on, now_ns());
        }
        self.show(&st);
        Ok(())
    }

    /// Pauses or resumes an output (a recording, as OBS pauses one): no gap in the file.
    pub fn pause_output(&self, id: u64, on: bool) -> Result<(), AvError> {
        self.with_output_flushed(id, |pipe| {
            pipe.pause(on, now_ns());
            Ok(())
        })
    }

    /// Goes on recording into a new file at `path` from the next frame ("Split recording").
    pub fn split_output(&self, id: u64, path: &std::path::Path) -> Result<(), AvError> {
        self.with_output_flushed(id, |pipe| pipe.split(path))
    }

    /// Starts recording into `path` on running output `id`, from its own encoders (at its next
    /// keyframe; the file plays from 0): a recording "Same as stream" sharing the stream's
    /// encode, where the Mac's one video encoder can't do two at 4K60 (19 Nov: 31 of 60 fps).
    pub fn record_on(&self, id: u64, path: &std::path::Path) -> Result<(), AvError> {
        self.with_output_flushed(id, |pipe| pipe.start_recording(path))
    }

    /// Ends output `id`'s recording, its file finished; the output goes on (its stream).
    pub fn record_off(&self, id: u64) -> Result<(), AvError> {
        self.with_output_flushed(id, Pipeline::end_recording)
    }

    /// A stream joins running output `id` (a recording "Same as stream" that started first):
    /// its tags go to `send` from the output's next keyframe, timed from there.
    pub fn stream_on(&self, id: u64, send: Box<dyn FnMut(Tag) + Send>) -> Result<(), AvError> {
        let mut st = self.lock();
        // Frames drawn before the join go to the recording alone: the stream starts after them.
        deliver(&mut st, &self.gpu, true);
        let (_, pipe, kind, _) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        if !matches!(kind, OutputKind::Record(_)) {
            return Err(AvError::Source("only a recording can take a stream on".into()));
        }
        pipe.begin_stream()?;
        *kind = OutputKind::Stream(send);
        Ok(())
    }

    /// The stream leaves output `id`, which goes on as a recording (its file and encoders
    /// kept): the stream ended while a recording sharing its encode goes on.
    pub fn stream_off(&self, id: u64) -> Result<(), AvError> {
        let mut st = self.lock();
        let (_, pipe, kind, _) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        // The stream always leaves (a recording that failed, say on a full disk, mustn't keep
        // it: the next Go live would find a stream there; CodeRabbit).
        *kind = OutputKind::Record(PathBuf::new());
        if !pipe.recording() {
            return Err(AvError::Source("the output had no recording left to go on with".into()));
        }
        Ok(())
    }

    /// An output's recording now.
    pub fn output_record_state(&self, id: u64) -> Result<RecordState, AvError> {
        if let Some(r) = self.now_or_shown(id, |pipe| Ok(record_state(pipe)), |(_, r, _)| *r) {
            return r;
        }
        Err(AvError::Source("no such output".into()))
    }

    /// `now` on output `id` when the state lock is free; else what the loop showed after its
    /// last frame (`shown`): read every second (/stats, from the window's thread), it never
    /// waits for the frame being made (a whole one, on a slow card; Windows' runner, 19 Nov).
    fn now_or_shown<T>(
        &self,
        id: u64,
        now: impl FnOnce(&mut Pipeline) -> Result<T, AvError>,
        shown: impl FnOnce(&(u64, RecordState, String)) -> T,
    ) -> Option<Result<T, AvError>> {
        match self.state.try_lock() {
            Ok(mut st) => Some(Self::on_output(&mut st, id, now)),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(Self::on_output(&mut p.into_inner(), id, now)),
            Err(std::sync::TryLockError::WouldBlock) => {
                let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
                seen.outputs.iter().find(|o| o.0 == id).map(|o| Ok(shown(o)))
            }
        }
    }

    fn with_output<T>(&self, id: u64, f: impl FnOnce(&mut Pipeline) -> Result<T, AvError>) -> Result<T, AvError> {
        let mut st = self.lock();
        let r = Self::on_output(&mut st, id, f);
        self.show(&st);
        r
    }

    /// As [`Self::with_output`], after the frames still on the card went in: a pause, split or
    /// chapter lands after every frame drawn before it (a frame delivered after a resume would
    /// carry a time from before it, and the file's time would go backwards).
    fn with_output_flushed<T>(
        &self,
        id: u64,
        f: impl FnOnce(&mut Pipeline) -> Result<T, AvError>,
    ) -> Result<T, AvError> {
        let mut st = self.lock();
        deliver(&mut st, &self.gpu, true);
        let r = Self::on_output(&mut st, id, f);
        self.show(&st);
        r
    }

    fn on_output<T>(
        st: &mut State,
        id: u64,
        f: impl FnOnce(&mut Pipeline) -> Result<T, AvError>,
    ) -> Result<T, AvError> {
        let (_, pipe, ..) = st
            .outputs
            .iter_mut()
            .find(|(i, ..)| *i == id)
            .ok_or_else(|| AvError::Source(format!("there's no output {id}")))?;
        f(pipe)
    }

    /// Stops an output: what's in its encoders is sent and a recording is closed.
    /// A stream going out changes its picture without stopping: its size (drawn from the same
    /// canvas, scaled), how often it takes a frame (`every`: 2 is half the engine's rate) and its
    /// encoder. The clock and the sound go on; the platforms get the new picture's header and a
    /// keyframe, as from a stream whose resolution changed. Streams only (a file can't change
    /// halfway).
    pub fn reshape_output(
        &self,
        id: u64,
        width: u32,
        height: u32,
        every: u32,
        video: VideoConfig,
    ) -> Result<(), AvError> {
        if every == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) || width == 0 || height == 0 {
            return Err(AvError::Source("a stream's picture needs even sides and a rate divisor of 1 or more".into()));
        }
        let refuse = |st: &State| -> Result<(), AvError> {
            let (_, pipe, kind, canvas) =
                st.outputs.iter().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
            if !matches!(kind, OutputKind::Stream(_)) {
                return Err(AvError::Source("only a stream's picture can change while it runs".into()));
            }
            // Twitch Enhanced Broadcasting's qualities were handed out for this size and rate;
            // their encoders would keep the old rate while frames came at the new one.
            if !canvas.video_tracks.is_empty() || pipe.video_tracks() > 0 {
                return Err(AvError::Source(
                    "a stream sending several qualities (Twitch Enhanced Broadcasting) can't change its picture while live"
                        .into(),
                ));
            }
            Ok(())
        };
        refuse(&self.lock())?;
        // Opened before taking the engine's lock: drawing, previews and recordings go on meanwhile.
        let next = crate::video::VideoEncoder::new(video)?;
        let mut st = self.lock();
        refuse(&st)?;
        // Frames drawn at the old size go out first.
        deliver(&mut st, &self.gpu, true);
        let config = self.config;
        let (_, pipe, kind, canvas) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        let tags = pipe.set_video(next)?;
        if let OutputKind::Stream(send) = kind {
            for t in tags {
                send(t);
            }
        }
        // Drawn at what it was drawn from (the canvas, or its own scene's size), then scaled.
        let from = canvas.scaled_from.map_or((canvas.width, canvas.height), |(w, h, _)| (w, h));
        let from = if canvas.scene.is_none() { (config.width, config.height) } else { from };
        canvas.scaled_from = ((width, height) != from).then_some((from.0, from.1, Filter::Bicubic));
        canvas.width = width;
        canvas.height = height;
        canvas.every = every;
        tracing::info!(width, height, every, kbps = video.bitrate_kbps, "a stream's picture changed while live");
        Ok(())
    }

    pub fn stop_output(&self, id: u64) -> Result<(), AvError> {
        // Each step said with its time: stopping a recording on GitHub's Windows runner never
        // finished (18 and 19 Nov) and nothing said where it waited.
        let t = Instant::now();
        tracing::info!(id, "an output stops");
        let taken = {
            let mut st = self.lock();
            tracing::info!(id, waited_ms = t.elapsed().as_millis() as u64, "an output stops: the engine's lock taken");
            // Its last frames still on the card go into it first.
            deliver(&mut st, &self.gpu, true);
            tracing::info!(id, at_ms = t.elapsed().as_millis() as u64, "an output stops: its last frames handed over");
            let taken = st.outputs.iter().position(|(i, ..)| *i == id).map(|at| st.outputs.remove(at));
            self.show(&st);
            taken
        };
        if let Some((_, pipe, mut kind, _)) = taken {
            let tags = pipe.finish()?;
            tracing::info!(id, at_ms = t.elapsed().as_millis() as u64, "an output stopped: its encoders finished");
            if let OutputKind::Stream(send) = &mut kind {
                for t in tags {
                    send(t);
                }
            }
        }
        Ok(())
    }

    /// As of the loop's last frame (it never waits for the frame being made).
    pub fn stats(&self) -> Stats {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).stats
    }

    /// What stopped the loop or an output, if anything did (as of its last frame).
    pub fn error(&self) -> Option<String> {
        let own = self.seen.lock().unwrap_or_else(|p| p.into_inner()).error.clone();
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
            let held = frozen(&st).contains(&id);
            if let Some(src) = st.sources.get_mut(&id)
                && !(held && src.sent > 0)
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

    /// One source's picture with its filters (as any of its items in a scene has them; a
    /// source's filters are the same wherever it's shown), scaled to `size`, RGBA: the drawer's
    /// "with filters" look. As [`Self::snapshot_source_at`] when it's in no scene.
    pub fn snapshot_source_filtered(&self, id: u64, size: (u32, u32)) -> Option<Vec<u8>> {
        let (w, h) = (size.0.max(1), size.1.max(1));
        let item = {
            let mut st = self.lock();
            send_pending(&mut st, &self.gpu);
            st.sources.get(&id)?.size?;
            let effects = st
                .scenes
                .values()
                .flatten()
                .find(|i| i.source == id && i.scene.is_none())
                .map(|i| i.effects.clone())
                .unwrap_or_default();
            let mut one = SceneItem::new(
                id,
                Placement {
                    bounds: crate::Bounds::Fit,
                    bounds_width: w as f32,
                    bounds_height: h as f32,
                    ..Default::default()
                },
            );
            one.filter = Filter::Bilinear;
            one.effects = effects;
            item_of(&st, &one, 0, (w, h))?
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
            // Placed by its size after its filters, as it's drawn (a crop or a shadow).
            (None, _) => st.sources.get(&it.source)?.size.map(|(w, h, _)| Effect::placed_size(&it.effects, w, h))?,
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
        // A picked source inside one of the scene's groups (OBS outlines it on the canvas,
        // through its group, with handles to resize it).
        for g in items.iter().filter(|i| i.group.is_some()) {
            let (Some(gname), Some((gw, gh))) = (&g.scene, g.group) else { continue };
            let Some(kids) = st.scenes.get(gname) else { continue };
            let (gp, _) = animated(g, scene_ms(st, scene, ts));
            let to_canvas = crate::transform::draw_transform(&gp, gw, gh);
            // On the scene's clock, as it's drawn (a group's sources play along with it).
            for it in kids.iter().filter(|i| i.item_id != 0 && ids.contains(&i.item_id)) {
                if let Some(c) = corners(scene, it) {
                    let c = c.map(|(x, y)| to_canvas.apply(x, y));
                    out.push(crate::preview::Outline {
                        corners: c,
                        color: color(it.item_id),
                        width: 2.0,
                        handles: true,
                    });
                }
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

/// A canvas's frame: its own scene, or what's on air (mid-transition, perhaps), with the top
/// layer over it.
fn what_to_draw(st: &State, width: u32, height: u32, scene: Option<&str>, ts: u64) -> Draw {
    if let Some(name) = scene {
        return Draw::Scene(build(st, width, height, Some(name), ts));
    }
    let mut draw = on_air_draw(st, width, height, ts);
    // The top layer over both sides of a transition, so it stays still through it; never
    // over itself.
    if let Some(top) = st.top.as_deref()
        && st.on_air.as_deref() != Some(top)
    {
        let mut over = SceneItem::new(0, Placement::default());
        over.scene = Some(top.to_string());
        let item = || item_of(st, &over, ts, (width, height));
        match &mut draw {
            Draw::Scene(s) => s.items.extend(item()),
            Draw::Moving(t) => {
                t.from.items.extend(item());
                t.to.items.extend(item());
            }
        }
    }
    draw
}

/// What's on air: one scene, or two mid-transition.
fn on_air_draw(st: &State, width: u32, height: u32, ts: u64) -> Draw {
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
    roots.extend(st.top.as_deref());
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
/// Sound bars that are shown: what they heard this frame (the sound on air, or their
/// source's through a tap opened by name, again if that source is opened anew), drawn now.
fn visualize(
    st: &mut State,
    blocks: &[crate::mix::Block],
    seen: &std::collections::HashSet<u64>,
    gpu: &Compositor,
    mixer: &Mutex<Mixer>,
    rate: u32,
) {
    let mut on_air: Option<Vec<f32>> = None;
    let names: HashMap<String, u64> = st.opened.iter().map(|(n, (id, _))| (n.clone(), *id)).collect();
    for (id, src) in st.sources.iter_mut() {
        let Live::Visualizer(v) = &mut src.live else { continue };
        if !seen.contains(id) {
            continue;
        }
        let heard = match v.spec.of.clone() {
            None => on_air
                .get_or_insert_with(|| {
                    blocks
                        .iter()
                        .filter_map(|b| b.tracks.first())
                        .flat_map(|t| t.chunks(2).map(|c| (c[0] + c.get(1).copied().unwrap_or(c[0])) * 0.5))
                        .collect()
                })
                .clone(),
            Some(name) => match names.get(&name) {
                Some(&target) => {
                    if v.tap.as_ref().is_none_or(|(t, _)| *t != target) {
                        let tap = mixer.lock().unwrap_or_else(|e| e.into_inner()).tap(target);
                        v.tap = Some((target, tap));
                    }
                    v.tap.as_ref().map(|(_, t)| t.take()).unwrap_or_default()
                }
                // Its source isn't there (yet): flat bars.
                None => {
                    v.tap = None;
                    Vec::new()
                }
            },
        };
        let picture = v.next(&heard, rate);
        if gpu.upload(*id, &picture).is_ok() {
            src.size = Some((picture.width, picture.height, picture.premultiplied));
            src.sent += 1;
        }
    }
}

/// Sources a Freeze filter holds: one on any of the source's items in any scene (a source's
/// filters are the same wherever it's shown).
fn frozen(st: &State) -> std::collections::HashSet<u64> {
    st.scenes
        .values()
        .flatten()
        .filter(|it| it.scene.is_none() && it.effects.iter().any(|e| matches!(e, Effect::Freeze)))
        .map(|it| it.source)
        .collect()
}

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
    roots.extend(st.top.as_deref());
    roots.extend(st.moving.as_ref().map(|(from, ..)| from.as_str()));
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    roots.extend(st.stinger.as_ref().map(|(from, ..)| from.as_str()));
    roots.extend(st.outputs.iter().filter_map(|(.., c)| c.scene.as_deref()));
    // A frame tap's scene too (the virtual camera showing one source alone), or its sources'
    // new pictures would wait and it would look frozen.
    roots.extend(st.taps.iter().filter_map(|(_, c, _)| c.scene.as_deref()));
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
    let frozen = frozen(st);
    for (id, src) in st.sources.iter_mut() {
        // A frozen source keeps its picture: what came in meanwhile is dropped.
        if frozen.contains(id) && src.sent > 0 {
            src.pending = None;
            continue;
        }
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
                // An empty group is 0×0: nothing to draw, and drawing it would fail the whole
                // frame (owner, 11 Nov: the canvas stayed blank once a group was added).
                Some((0, _) | (_, 0)) => return None,
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

/// macOS: the frame loop (which also mixes the sound) at the "user interactive" quality of
/// service, as libobs starts its video thread. At the default class a busy Mac (browsers
/// drawing, a game) woke it late now and then: about 1% of frames missed under a browser test
/// run at 1440p60 (24 of 2,467 and 26 of 2,829), none with this (0 of 2,356 and of 2,351).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn on_time() {
    // SAFETY: sets the calling thread's own class; it takes no pointers.
    let r = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
    if r != 0 {
        tracing::debug!(r, "the frame loop kept its default priority");
    }
}

/// Windows: the frame loop at a high priority (libobs raises its video thread too), and the
/// thread and the app out of Windows 11's power throttling (EcoQoS: Windows' App Nap, put on
/// apps that are minimized or behind a game, which slows their threads and their timers).
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn on_time() {
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentProcess, GetCurrentThread, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation, SetThreadInformation,
        SetThreadPriority, THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST, ThreadPowerThrottling,
    };
    // How, by `FERROCAST_FRAME_PRIORITY` (to compare on a PC, and a way out if one misbehaves):
    // "highest", "above" (normal + 1), "mmcss" (Windows' multimedia scheduler, as a game) or
    // "normal"; `FERROCAST_FRAME_THROTTLING=on` leaves power throttling to Windows.
    let pick = std::env::var("FERROCAST_FRAME_PRIORITY").unwrap_or_default();
    let mode = match pick.as_str() {
        "above" | "normal" | "mmcss" => pick.as_str(),
        _ => "highest",
    };
    let throttle = std::env::var("FERROCAST_FRAME_THROTTLING").is_ok_and(|v| v == "on");
    // SAFETY: the pseudo-handle of this thread (never closed); MMCSS takes a static task name and
    // a u32 of ours, and lets go of the thread when it ends.
    let priority = unsafe {
        match mode {
            "highest" => SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST),
            "above" => SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL),
            "mmcss" => {
                let mut index = 0u32;
                AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &mut index).map(drop)
            }
            _ => Ok(()),
        }
    };
    // Each in the control mask with its state off: "never throttle this", not "Windows decides".
    let thread = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: if throttle { 0 } else { THREAD_POWER_THROTTLING_EXECUTION_SPEED },
        StateMask: 0,
    };
    let process = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: if throttle {
            0
        } else {
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION
        },
        StateMask: 0,
    };
    // SAFETY: the pseudo-handles of this thread and process (never closed), and structs of the
    // sizes given, alive for each call.
    let (slowed, app) = unsafe {
        (
            SetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw const thread).cast(),
                size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            ),
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const process).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            ),
        )
    };
    tracing::info!(
        priority = mode,
        power_throttling = if throttle { "Windows'" } else { "off" },
        "the frame loop's place"
    );
    // Older Windows 10 lacks power throttling: the priority still counts.
    for (what, r) in [("priority", priority), ("thread throttling", slowed), ("app throttling", app)] {
        if let Err(e) = r {
            tracing::debug!(%e, what, "the frame loop kept Windows' default");
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn on_time() {}

/// Raises the calling thread to the frame loop's priority (macOS: "user interactive"; Windows:
/// high, out of power throttling): for a thread the sound depends on, such as a filter's own
/// worker, so a busy computer wakes it in time.
pub fn raise_this_thread() {
    on_time();
}

/// The frame loop.
fn run(
    config: EngineConfig,
    state: &Mutex<State>,
    gpu: &Compositor,
    mixer: &Mutex<Mixer>,
    stop: &AtomicBool,
    seen: &Seen,
) {
    on_time();
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
            frame(&config, state, gpu, mixer, ts, behind, started, seen);
            // Pictures uploaded this frame are sent even when nothing drew (no preview, no
            // output), and what the card finished with is let go: without this, Direct3D and
            // Vulkan keep it all and run out of memory in minutes.
            gpu.submit(std::iter::empty());
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }));
        // Behind (a frame took longer than its time): a moment before the next one takes the
        // engine's lock again. Windows' lock isn't fair: re-taken at once, frame after frame,
        // the window's thread waiting for it (stats, the preview) got it only every 10-48 s
        // on GitHub's runner, every Studio call stuck behind it (19 Nov). On time, the loop
        // sleeps until the next frame anyway.
        if now_ns() >= next {
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Err(p) = ok {
            // A fault in a frame is that frame lost, not the engine: the next one is drawn.
            faults += 1;
            if faults.is_power_of_two() {
                tracing::error!(why = panic_text(&*p), faults, "the engine caught a fault in a frame and carried on");
            }
            let st = &mut *state.lock().unwrap_or_else(|e| e.into_inner());
            st.stats.recovered += 1;
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = Shown::of(st);
        }
    }
}

/// A video file as its pictures are best drawn: as NV12 turned into RGB on the card where FFmpeg
/// plays it (Windows, Linux), as RGBA where the system does (a Mac).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) fn open_video(path: &std::path::Path, looping: bool) -> Result<crate::media::Media, AvError> {
    #[cfg(target_os = "macos")]
    return crate::media::Media::open(path, looping);
    #[cfg(not(target_os = "macos"))]
    return crate::media::Media::open_for_card(path, looping);
}

/// A source whose device takes one user at a time: closed before it's opened again.
fn one_at_a_time(kind: &SourceKind) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    if matches!(kind, SourceKind::Camera(_) | SourceKind::Microphone(_)) {
        return true;
    }
    let _ = kind;
    false
}

/// Sources the watchdog can start again: captures (a file or a picture doesn't stop by itself).
/// A screen capture changing method or what it shows (Screen, Window, App): the old one keeps
/// showing until the new one opens. Not cameras (two sessions on one device give no picture).
fn hands_over(from: &SourceKind, to: &SourceKind) -> bool {
    fn screenish(k: &SourceKind) -> bool {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        if matches!(k, SourceKind::Screen { .. } | SourceKind::Window { .. }) {
            return true;
        }
        #[cfg(target_os = "macos")]
        if matches!(k, SourceKind::App { .. }) {
            return true;
        }
        let _ = k;
        false
    }
    screenish(from) && screenish(to)
}

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

/// The window to capture now. `id` while that window is open and still its app's (minimized,
/// on another Space or desktop counts; a number can come back for another app's window after
/// a restart). Else the app's open window with the same title (a game or browser opened again
/// gets a new number). Never a window of that app with another title: Chrome reopened on
/// someone's mail would go on air. When none fits, a number no window has (the capture then
/// says it's gone, and the watchdog tries again later). Saved before the app was noted:
/// `id` as it is.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn window_now(id: u32, app: &str, title: &str) -> u32 {
    if app.is_empty() {
        return id;
    }
    match crate::screen::window_owner(id) {
        // Can't be asked: as it is.
        None => return id,
        Some(Some(owner)) if owner.eq_ignore_ascii_case(app) => return id,
        Some(_) => {}
    }
    let found = crate::screen::shareable().ok().and_then(|(_, windows)| {
        let list: Vec<(u32, &str, &str)> = windows.iter().map(|w| (w.id, w.app.as_str(), w.title.as_str())).collect();
        pick_window(app, title, &list)
    });
    match found {
        Some(now) => {
            tracing::info!(was = id, now, app, "a window came back with a new number: captured again");
            now
        }
        None => GONE_WINDOW,
    }
}

/// The display a Display or Application Capture opens on now (a Mac's found by its UUID; a
/// display that isn't connected fails to open, and the watchdog tries again, so a screen
/// plugged back in comes back by itself). Elsewhere displays are numbered by place.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn display_now(display: u32, uuid: Option<&str>) -> u32 {
    #[cfg(target_os = "macos")]
    return crate::screen::display_now(display, uuid).unwrap_or(GONE_DISPLAY);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = uuid;
        display
    }
}

/// A display number none has: opening it says it isn't connected.
#[cfg(target_os = "macos")]
const GONE_DISPLAY: u32 = 0;

/// A window number no window has (macOS and Windows never hand out 0): opening it says the
/// window is gone.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
const GONE_WINDOW: u32 = 0;

/// [`window_now`]'s choice from a window list of (number, app, title): the app's window with
/// that title.
fn pick_window(app: &str, title: &str, windows: &[(u32, &str, &str)]) -> Option<u32> {
    if app.is_empty() || title.is_empty() {
        return None;
    }
    windows.iter().find(|w| w.1.eq_ignore_ascii_case(app) && w.2 == title).map(|w| w.0)
}

/// Starts a capture (None: `kind` isn't one).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn open_capture(config: &EngineConfig, kind: &SourceKind) -> Option<Result<Live, AvError>> {
    use crate::screen::{ScreenCapture, Target};
    Some(match kind {
        SourceKind::Screen { display, uuid, sound, hide, cursor } => ScreenCapture::start_with(
            Target::Display(display_now(*display, uuid.as_deref())),
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
        SourceKind::Window { id, cursor, app, title } => ScreenCapture::start_with(
            Target::Window(window_now(*id, app, title)),
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
        SourceKind::App { display, uuid, app, sound, cursor } => ScreenCapture::start_app(
            display_now(*display, uuid.as_deref()),
            app,
            config.width,
            config.height,
            config.fps,
            *cursor,
            *sound,
        )
        .map(Live::Screen),
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
        SourceKind::Camera(spec) => crate::camera::Capture::camera(spec).map(Live::Camera),
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

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn output_capture_of(live: &Live, kind: Option<&SourceKind>) -> Option<crate::mix::OutputCapture> {
    let is_loopback = matches!(kind, Some(SourceKind::DesktopSound | SourceKind::Screen { sound: true, .. }));
    if !is_loopback {
        return None;
    }
    let Live::Screen(cap) = live else { return None };
    Some(crate::mix::OutputCapture {
        device: cap.loopback_device().map(|d| d.to_string()).unwrap_or_else(|| "default".into()),
        excludes_self: cap.excludes_self_audio(),
    })
}

fn is_live_ended(live: &Live) -> bool {
    signs(live).is_some_and(|(_, _, e)| e.is_some())
}

fn poll_hotplug(seq: &mut u64) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        crate::camera::take_hotplug_connected(seq)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = seq;
        None
    }
}

/// Clears watchdog reconnect backoff for disconnected/stopped camera and microphone sources whose
/// device just reconnected so replug recovers immediately instead of waiting for backoff.
fn apply_hotplug_reconnects(sources: &mut BTreeMap<u64, Src>, connected: &[String], now: u64) -> usize {
    let mut woke = 0;
    for src in sources.values_mut() {
        if src.opening.is_some() {
            continue;
        }
        let dead = matches!(src.live, Live::Still) || src.watch.faulted || is_live_ended(&src.live);
        if !dead {
            continue;
        }
        let matches_device = match &src.kind {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Some(SourceKind::Camera(spec)) => {
                connected.is_empty() || spec.id.is_empty() || connected.iter().any(|id| id == &spec.id)
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Some(SourceKind::Microphone(dev)) => {
                connected.is_empty() || dev.is_empty() || dev == "default" || connected.iter().any(|id| id == dev)
            }
            _ => false,
        };
        if matches_device {
            src.watch.next_try = now;
            src.watch.tries = 0;
            woke += 1;
        }
    }
    woke
}

/// The watchdog: once a second (or right away when a device connects) it looks at every capture,
/// and one that faulted, ended or went quiet is started again (waiting longer after each try that
/// didn't hold).
fn watchdog(config: EngineConfig, state: &Mutex<State>, mixer: &Mutex<Mixer>, stop: &AtomicBool) {
    let mut hotplug_seq = 0u64;
    let _ = poll_hotplug(&mut hotplug_seq);
    while !stop.load(Ordering::Acquire) {
        let mut connected = None;
        for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if let Some(ids) = poll_hotplug(&mut hotplug_seq) {
                connected = Some(ids);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let now = now_ns();
        let mut todo = Vec::new();
        let mut expired = Vec::new();
        {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ids) = &connected {
                apply_hotplug_reconnects(&mut st.sources, ids, now);
            }
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
                        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                        let oc = output_capture_of(&live, src.kind.as_ref());
                        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                        let oc: Option<crate::mix::OutputCapture> = None;
                        src.live = live;
                        src.watch.count = 0;
                        src.watch.since = now_ns();
                        st.stats.restarts += 1;
                        mixer.lock().unwrap_or_else(|e| e.into_inner()).set_output_capture(id, oc);
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

/// Frames a canvas waits for at most before the loop waits for the card: two (33 ms at 60
/// fps), as OBS keeps its staging surfaces.
pub(crate) const IN_FLIGHT: usize = 2;

/// One frame time's work on its way out: its pictures coming back from the card, its sound, and
/// the outputs and taps there were then (one started later doesn't get this frame's sound
/// without its picture).
struct InFlight {
    ts: u64,
    blocks: Vec<crate::mix::Block>,
    outputs: Vec<u64>,
    taps: Vec<u64>,
    drawn: Vec<(Canvas, Result<PendingNv12, String>)>,
}

/// Hands the frames that came back to their outputs and taps, oldest first: those ready, and
/// any past [`IN_FLIGHT`] (waiting for the card); every one with `flush`.
fn deliver(st: &mut State, gpu: &Compositor, flush: bool) {
    let _ = gpu.device.poll(wgpu::PollType::Poll);
    if st.outputs.is_empty() {
        // Nothing encodes: the next output's time starts afresh.
        st.stats.encode_ms = 0.0;
    }
    while let Some(front) = st.in_flight.front() {
        let ready = front.drawn.iter().all(|(_, f)| f.as_ref().map_or(true, PendingNv12::ready));
        if !(ready || flush || st.in_flight.len() > IN_FLIGHT) {
            break;
        }
        let Some(e) = st.in_flight.pop_front() else { break };
        let InFlight { ts, blocks, outputs, taps, drawn } = e;
        let drawn: Vec<(Canvas, Result<Nv12, String>)> = drawn
            .into_iter()
            .map(|(c, f)| (c, f.and_then(|p| gpu.finish_nv12(p).map_err(|e| e.to_string()))))
            .collect();
        let mut failed = None;
        let mut encoding = Duration::ZERO;
        for (id, pipe, kind, canvas) in st.outputs.iter_mut() {
            if !outputs.contains(id) {
                continue;
            }
            let mut tags = Vec::new();
            if let Some((_, frame)) = drawn.iter().find(|(c, _)| c == canvas) {
                match frame {
                    Ok(f) => {
                        let w = canvas.width as usize;
                        let t0 = Instant::now();
                        match pipe.video(Frame::Nv12 { y: &f.y, y_stride: w, uv: &f.uv, uv_stride: w }, ts) {
                            Ok(t) => tags = t,
                            Err(e) => failed = Some(e.to_string()),
                        }
                        encoding += t0.elapsed();
                    }
                    Err(e) => failed = Some(e.clone()),
                }
                for (i, &(w, h, _)) in canvas.video_tracks.iter().enumerate() {
                    let c = canvas.track(w, h);
                    if let Some((_, Ok(f))) = drawn.iter().find(|(d, _)| *d == c) {
                        let s = w as usize;
                        let t0 = Instant::now();
                        match pipe.video_track(i, Frame::Nv12 { y: &f.y, y_stride: s, uv: &f.uv, uv_stride: s }, ts) {
                            Ok(t) => tags.extend(t),
                            Err(e) => failed = Some(e.to_string()),
                        }
                        encoding += t0.elapsed();
                    }
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
        // Only frames that went through an encoder count (a canvas drawn every other frame, or
        // one that failed to draw, would pull the average down by half).
        st.stats.encoding_us += encoding.as_micros() as u64;
        if encoding > Duration::ZERO {
            let ms = encoding.as_secs_f32() * 1000.0;
            st.stats.encode_ms = if st.stats.encode_ms == 0.0 { ms } else { st.stats.encode_ms * 0.9 + ms * 0.1 };
        }
        // A tap's picture that failed to draw is left out (the camera repeats its last one); it
        // doesn't stop the outputs.
        for (id, canvas, sink) in st.taps.iter_mut() {
            if !taps.contains(id) {
                continue;
            }
            if let Some((_, Ok(f))) = drawn.iter().find(|(c, _)| c == canvas) {
                sink(f, ts);
            }
        }
    }
}

/// What a panic said.
fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(String::as_str)).unwrap_or("a fault")
}

/// One frame: sources taken in, sound mixed, each canvas drawn and its outputs fed.
#[allow(clippy::too_many_arguments)]
fn frame(
    config: &EngineConfig,
    state: &Mutex<State>,
    gpu: &Compositor,
    mixer: &Mutex<Mixer>,
    ts: u64,
    behind: u64,
    started: Instant,
    shown: &Seen,
) {
    let rate = config.sample_rate;
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.stats.skipped += behind;
    let mut mx = mixer.lock().unwrap_or_else(|e| e.into_inner());
    // Only what's on air is heard (and monitored), as libobs mixes its program's tree; the
    // rest still has its sound taken, so it doesn't pile up.
    let heard = heard(&st);
    mx.sync_active(&heard);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some((epoch, dev_id)) = st.monitor.as_ref().and_then(|f| {
        let epoch = f.device_epoch();
        (epoch != st.monitor_epoch).then(|| (epoch, f.device_id()))
    }) {
        st.monitor_epoch = epoch;
        mx.set_monitor_device(dev_id);
    }
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
            // Drawn after the sound is mixed (`visualize`).
            Live::Still | Live::Visualizer(_) => {}
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
                // As NV12 for the card where FFmpeg plays it (`open_video`), else RGBA.
                #[cfg(not(target_os = "macos"))]
                let fresh = m.frame_nv12().map(Fresh::Yuv).or_else(|| m.frame_at(ts).map(Fresh::Pic));
                #[cfg(target_os = "macos")]
                let fresh = m.frame_at(ts).map(Fresh::Pic);
                if let Some(f) = fresh {
                    uploads.push((*id, f));
                }
                for (pcm, t) in m.take_sound() {
                    if heard.contains(id) {
                        mx.push(*id, &pcm, 2, 48_000, t);
                    }
                }
            }
            Live::Fed(feed) | Live::Series { feed, .. } => {
                // The newest of the two kinds handed in (a playlist moves between them only at
                // a file's start, so one is always empty then).
                if let Some(p) = feed.0.yuv.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    uploads.push((*id, Fresh::Yuv(p)));
                } else if let Some(p) = feed.0.picture.lock().unwrap_or_else(|e| e.into_inner()).take() {
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
    let frozen = frozen(&st);
    for (id, f) in uploads {
        let Some(s) = st.sources.get_mut(&id) else { continue };
        // A frozen source keeps the picture it has (its first one still comes in).
        if frozen.contains(&id) && s.sent > 0 {
            s.pending = None;
            continue;
        }
        if !seen.contains(&id) && s.size.is_some() {
            s.pending = f.into_pending();
            continue;
        }
        if f.send(gpu, id) {
            s.size = Some(f.info());
            s.sent += 1;
        }
    }
    // Any hidden or frozen source that still holds a pinned zero-copy GPU texture copies it
    // onto a GPU-owned texture now so CoreVideo buffer pools aren't starved while hidden/frozen.
    for (id, s) in st.sources.iter_mut() {
        let keep_frozen = frozen.contains(id) && s.sent > 0;
        if keep_frozen {
            s.pending = None;
        }
        if !seen.contains(id) || keep_frozen {
            gpu.unpin_surface(*id);
        }
    }
    let blocks = mx.mix_until(now_ns());
    let mon_active = mx.has_monitored();
    drop(mx);
    visualize(&mut st, &blocks, &seen, gpu, mixer, rate);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(f) = &st.monitor {
        if mon_active {
            for b in &blocks {
                f.push(&b.monitor);
            }
        } else {
            f.idle();
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
    if !st.lingering.is_empty() {
        let mut playing: Vec<String> = st.moving.iter().map(|(from, ..)| from.clone()).collect();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        playing.extend(st.stinger.iter().map(|(from, ..)| from.clone()));
        playing.extend(st.on_air.clone());
        let gone: Vec<String> = st.lingering.iter().filter(|n| !playing.contains(n)).cloned().collect();
        for n in gone {
            st.scenes.remove(&n);
            st.started.remove(&n);
            st.lingering.retain(|l| *l != n);
        }
    }
    if !st.outputs.is_empty() || !st.taps.is_empty() {
        // Each distinct canvas drawn once this frame (if it takes this one). The card reads them
        // back while the next frames are drawn (`deliver`): waiting for it here stalled the loop
        // whenever a game kept the card busy (owner, 15 Nov: 35 of 60 fps, 46% skipped).
        let index = st.stats.frames;
        let mut drawn: Vec<(Canvas, Result<PendingNv12, String>)> = Vec::new();
        for canvas in st.outputs.iter().map(|(.., c)| c).chain(st.taps.iter().map(|(_, c, _)| c)) {
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
                Draw::Scene(s) => gpu.begin_nv12(&s),
                Draw::Moving(t) => gpu.begin_transition_nv12(&t),
            };
            drawn.push((canvas.clone(), frame.map_err(|e| e.to_string())));
        }
        // Each extra quality's picture (Twitch Enhanced Broadcasting), drawn once like the rest.
        let mut extra = Vec::new();
        for (.., canvas) in &st.outputs {
            if !index.is_multiple_of(u64::from(canvas.every)) {
                continue;
            }
            for &(w, h, _) in &canvas.video_tracks {
                let c = canvas.track(w, h);
                if drawn.iter().chain(extra.iter()).any(|(d, _)| *d == c) {
                    continue;
                }
                let (bw, bh) = c.scaled_from.map_or((w, h), |(bw, bh, _)| (bw, bh));
                let frame = match what_to_draw(&st, bw, bh, c.scene.as_deref(), ts) {
                    Draw::Scene(s) => gpu.begin_nv12(&scaled(s, w, h, Filter::Bicubic)),
                    Draw::Moving(mut t) => {
                        t.from = scaled(t.from, w, h, Filter::Bicubic);
                        t.to = scaled(t.to, w, h, Filter::Bicubic);
                        gpu.begin_transition_nv12(&t)
                    }
                };
                extra.push((c, frame.map_err(|e| e.to_string())));
            }
        }
        drawn.extend(extra);
        let entry = InFlight {
            ts,
            blocks,
            outputs: st.outputs.iter().map(|(id, ..)| *id).collect(),
            taps: st.taps.iter().map(|(id, ..)| *id).collect(),
            drawn,
        };
        st.in_flight.push_back(entry);
    }
    deliver(&mut st, gpu, false);
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    st.stats.frames += 1;
    st.stats.frame_ms = ms;
    st.stats.worst_ms = st.stats.worst_ms.max(ms);
    // Shown before the state's lock goes: a change made right after (a split, a pause) shows
    // itself, never overwritten by this older picture (projects-60).
    *shown.lock().unwrap_or_else(|e| e.into_inner()) = Shown::of(&st);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioConfig;
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    #[test]
    fn a_window_whose_app_opened_again_is_found_by_its_app_and_title() {
        let list = [(40, "Google Chrome", "Mail"), (41, "Google Chrome", "Stream - YouTube"), (50, "Game", "Game")];
        assert_eq!(pick_window("Google Chrome", "Stream - YouTube", &list), Some(41));
        assert_eq!(pick_window("game", "Game", &list), Some(50));
        // Another title of the same app is never taken, even as its only window (19 Nov,
        // projects-60: Chrome reopened on someone's mail would go on air).
        assert_eq!(pick_window("Google Chrome", "Another tab", &list), None);
        assert_eq!(pick_window("Game", "Menu", &list), None);
        // Nothing to go on, or the app isn't open.
        assert_eq!(pick_window("", "Mail", &list), None);
        assert_eq!(pick_window("Discord", "Mail", &list), None);
    }

    /// Whether an output can encode here: the Mac always; elsewhere only with a working FFmpeg
    /// H.264 encoder (GitHub's Windows runner has none: these tests failed there, not skipped).
    fn encodes_here() -> bool {
        let ok = cfg!(target_os = "macos") || crate::ffenc::h264_encoder().is_some();
        if !ok {
            eprintln!("skipped: no H.264 encoder here");
        }
        ok
    }

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

    /// The frame loop's thread runs high and isn't throttled (Windows 11's EcoQoS).
    #[cfg(target_os = "windows")]
    #[test]
    #[allow(unsafe_code)]
    fn the_frame_thread_is_raised() {
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_HIGHEST};
        let got = std::thread::spawn(|| {
            on_time();
            // SAFETY: this thread's pseudo-handle.
            unsafe { GetThreadPriority(GetCurrentThread()) }
        })
        .join()
        .unwrap_or_default();
        assert_eq!(got, THREAD_PRIORITY_HIGHEST.0);
    }

    /// Frame pacing at 1080p60 for a while: the share of frame times the loop woke too late for
    /// (the Stats panel's "Missed"). Ignored by default (it takes `FERROCAST_PACE_SECS`, 30 s);
    /// run it with the machine busy to see the loop hold its clock, as OBS's does:
    /// `cargo test -p ferrocore --release pacing_holds -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing measurement; run by hand, ideally with the CPU loaded"]
    fn pacing_holds() {
        let secs = std::env::var("FERROCAST_PACE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            bitrate_kbps: 6000,
            rate: RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
        };
        let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let config = EngineConfig { width: 1920, height: 1080, fps: 60, ..Default::default() };
        let Ok(e) = Engine::new(config, video, audio) else { return };
        e.set_source(1, SourceKind::Color { width: 1920, height: 1080, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_secs(1));
        let before = e.stats();
        std::thread::sleep(Duration::from_secs(secs));
        let s = e.stats();
        let (drawn, missed) = (s.frames - before.frames, s.skipped - before.skipped);
        let share = missed as f64 * 100.0 / (drawn + missed).max(1) as f64;
        eprintln!("pacing over {secs} s at 60 fps: {drawn} drawn, {missed} missed ({share:.2}%)");
        assert!(share < 1.0, "{missed} of {} frame times missed", drawn + missed);
    }

    /// Frames are read back from the card while the next ones are drawn: a stopped output still
    /// gets the ones on their way (a recording keeps its last frames), its pictures come in
    /// time order, and sound keeps flowing with them.
    #[test]
    fn frames_still_on_the_card_reach_an_output_that_stops() {
        if !encodes_here() {
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [10, 200, 30, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let tags: Arc<Mutex<Vec<(bool, u32)>>> = Arc::default();
        let sink = {
            let tags = tags.clone();
            move |t: Tag| {
                let v = match t {
                    Tag::Video { ms, .. } => (true, ms),
                    Tag::Audio { ms, .. } => (false, ms),
                };
                tags.lock().unwrap_or_else(|p| p.into_inner()).push(v);
            }
        };
        let out = e.start_output(OutputKind::Stream(Box::new(sink))).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1500));
        let drawn_before_stop = e.stats().frames;
        e.stop_output(out).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.lock().in_flight.is_empty(), "nothing left on the way");
        let tags = tags.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let video: Vec<u32> = tags.iter().filter(|(v, _)| *v).map(|(_, ms)| *ms).collect();
        let audio = tags.iter().filter(|(v, _)| !*v).count();
        // About 1.5 s at 30 fps; the frames made before the stop all came out (the encoder's
        // header and a frame or two of its own delay aside).
        // (Counted against what the engine drew, not 30 a second: GitHub's Mac VM is slower.)
        assert!(video.len() >= 10, "{} video tags", video.len());
        assert!(video.len() as u64 + 3 >= drawn_before_stop.min(45), "{} of {drawn_before_stop}", video.len());
        assert!(video.windows(2).all(|w| w[0] <= w[1]), "in time order: {video:?}");
        assert!(audio > 20, "sound came too: {audio}");
    }

    /// Optimize while live: a stream's picture goes from the canvas at full rate to a smaller
    /// one at half the rate without stopping. A new header comes where the stream is (never
    /// back at 0), time only goes forward, and frames come half as often after.
    #[test]
    fn a_live_stream_changes_its_picture_without_stopping() {
        if !encodes_here() {
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [10, 200, 30, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        type Tags = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;
        let tags: Tags = Arc::default();
        let sink = {
            let tags = tags.clone();
            move |t: Tag| {
                if let Tag::Video { ms, body } = t {
                    tags.lock().unwrap_or_else(|p| p.into_inner()).push((ms, body));
                }
            }
        };
        let out = e.start_output(OutputKind::Stream(Box::new(sink))).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1200));
        let video = VideoConfig {
            codec: crate::video::VideoCodec::H264,
            input: crate::video::PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 15,
            fps_den: 1,
            bitrate_kbps: 800,
            rate: crate::video::RateControl::Cbr,
            keyframe_secs: 1,
            software: false,
        };
        e.reshape_output(out, 320, 180, 2, video).unwrap_or_else(|x| panic!("{x}"));
        let switched_at = tags.lock().unwrap_or_else(|p| p.into_inner()).len();
        std::thread::sleep(Duration::from_millis(1500));
        e.stop_output(out).unwrap_or_else(|x| panic!("{x}"));
        let tags = tags.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // AVC sequence headers: 0x17 then 0x00.
        let headers: Vec<u32> =
            tags.iter().filter(|(_, b)| b.get(..2) == Some(&[0x17, 0x00])).map(|(ms, _)| *ms).collect();
        assert_eq!(headers.len(), 2, "one header at the start, one when the picture changed: {headers:?}");
        assert!(headers[1] >= 1000, "the second header is where the stream is, not 0: {}", headers[1]);
        let times: Vec<u32> = tags.iter().map(|(ms, _)| *ms).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "time only goes forward: {times:?}");
        // After the change: about 15 frames a second, not 30.
        let after: Vec<u32> =
            tags[switched_at..].iter().filter(|(_, b)| b.get(1) == Some(&1)).map(|(ms, _)| *ms).collect();
        let gaps: Vec<u32> = after.windows(2).map(|w| w[1] - w[0]).collect();
        // The median (it took the middle gap unsorted: one late frame on GitHub's Mac VM failed it).
        let mut sorted = gaps.clone();
        sorted.sort_unstable();
        let typical = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        assert!((55..=80).contains(&typical), "a frame about every 66 ms after: {gaps:?}");
        // A recording refuses.
        let rec = std::env::temp_dir().join(format!("ferrocast-reshape-{}.mp4", std::process::id()));
        let r = e.start_output(OutputKind::Record(rec.clone())).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.reshape_output(r, 320, 180, 2, video).is_err());
        let _ = e.stop_output(r);
        let _ = std::fs::remove_file(rec);
    }

    /// A capture that can't open says so to whoever added it, and isn't kept (macOS and Linux:
    /// on Windows looking for a camera through FFmpeg can take longer than the wait).
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn a_capture_that_cant_open_says_so_at_once() {
        let Some(e) = engine() else { return };
        // The system's first camera lookup in a process can take longer than the engine waits
        // (OPEN_WAIT): such a capture is kept and tried again later, by design. Warmed first,
        // so the check is of the answer, not of a cold start (it failed ~1 run in 3).
        let _ = e.set_source(6, SourceKind::Camera("/no/such/camera".into()));
        let warm = Instant::now() + Duration::from_secs(5);
        while e.opening() > 0 && Instant::now() < warm {
            std::thread::sleep(Duration::from_millis(20));
        }
        e.remove_source(6);
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

    /// A collection edited with the same sources (a move, a filter's setting) is applied in
    /// place: the same sources keep running, the scene on air stays on air with its clock, and
    /// at no moment is nothing on air (a full load clears it for a moment: a slider dragged
    /// while live broke the sound, 18 Nov).
    #[test]
    fn settings_only_edits_apply_in_place() {
        let Some(e) = engine() else { return };
        let doc = |x: f64, radius: u32, extra: bool| {
            let mut sources = vec![
                serde_json::json!({ "id": "color_source", "name": "Back", "settings": { "color": 0xFF00_00FFu32 } }),
                serde_json::json!({ "id": "color_source", "name": "Box",
                    "settings": { "color": 0xFFFF_0000u32, "width": 64, "height": 64 },
                    "filters": [{ "id": "ferrocast_rounded_corners", "name": "Round", "settings": { "radius": radius } }] }),
                serde_json::json!({ "id": "ffmpeg_source", "name": "Gone", "settings": { "local_file": "/no/such/clip.mp4" } }),
                serde_json::json!({ "id": "scene", "name": "Main", "settings": { "items": [
                    { "name": "Back" }, { "name": "Box", "pos": { "x": x, "y": 0.0 } }, { "name": "Gone" } ] } }),
            ];
            if extra {
                sources.push(
                    serde_json::json!({ "id": "color_source", "name": "New", "settings": { "color": 0xFF00_FF00u32 } }),
                );
            }
            serde_json::json!({ "current_program_scene": "Main", "sources": sources })
        };
        let skipped = e.load_obs(&doc(0.0, 4, false));
        assert_eq!(skipped.len(), 1, "the missing file is said: {skipped:?}");
        let ids = |e: &Engine| {
            let st = e.lock();
            let id = |n: &str| st.opened.get(n).map(|(i, _)| *i);
            (id("Back"), id("Box"))
        };
        let before = ids(&e);
        let started = e.lock().started.get("Main").copied();
        // Something watches the whole time: Main is on air with its items at every moment.
        let stop = Arc::new(AtomicBool::new(false));
        let empty = Arc::new(AtomicU64::new(0));
        let watcher = {
            let (state, stop, empty) = (e.state.clone(), stop.clone(), empty.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let st = state.lock().unwrap_or_else(|p| p.into_inner());
                    if st.on_air.as_deref() != Some("Main") || st.scenes.get("Main").is_none_or(Vec::is_empty) {
                        empty.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        for i in 0..200 {
            let r = e.apply_same_sources(&doc(f64::from(i), 4 + i % 30, false));
            assert!(r.is_some(), "the same sources: applied in place");
        }
        stop.store(true, Ordering::Release);
        let _ = watcher.join();
        assert_eq!(empty.load(Ordering::Relaxed), 0, "never nothing on air");
        assert_eq!(ids(&e), before, "the same sources keep running");
        assert_eq!(e.lock().started.get("Main").copied(), started, "the scene's clock goes on");
        {
            let st = e.lock();
            let items = &st.scenes["Main"];
            let bx = items.iter().find(|it| Some(it.source) == before.1).unwrap_or_else(|| panic!("the box"));
            assert_eq!(bx.placement.x, 199.0, "the last move");
            assert_eq!(bx.effects, vec![Effect::RoundedCorners { radius: 23.0 }], "the last setting");
        }
        // A new source: not applied here, nothing touched (the caller loads it in full).
        assert!(e.apply_same_sources(&doc(5.0, 4, true)).is_none());
        assert_eq!(
            e.lock().scenes["Main"].iter().find(|it| Some(it.source) == before.1).map(|it| it.placement.x),
            Some(199.0)
        );
        // A scene of the engine's own on air (fade to black) stays on air through an edit.
        e.set_scene("black", Vec::new());
        e.go("black").unwrap_or_else(|x| panic!("{x}"));
        assert!(e.apply_same_sources(&doc(7.0, 4, false)).is_some());
        assert_eq!(e.lock().on_air.as_deref(), Some("black"));
        assert!(e.lock().scenes.contains_key("black"), "the engine's own scene is kept");
    }

    /// A full load (a source added or removed) swaps the collection in at once: nothing is
    /// ever off air in between, the scene on air keeps its clock, the engine's own scenes (fade
    /// to black) stay, a stinger's video plays on and new ids never run into its id; a
    /// collection whose own scene on air changed puts that one on air.
    #[test]
    fn a_full_load_swaps_in_at_once() {
        let Some(e) = engine() else { return };
        let doc = |extra: bool, current: &str| {
            let mut sources = vec![
                serde_json::json!({ "id": "color_source", "name": "Back", "settings": { "color": 0xFF00_00FFu32 } }),
                serde_json::json!({ "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Back" }] } }),
                serde_json::json!({ "id": "scene", "name": "Other", "settings": { "items": [{ "name": "Back" }] } }),
            ];
            if extra {
                sources.push(
                    serde_json::json!({ "id": "color_source", "name": "New", "settings": { "color": 0xFF00_FF00u32 } }),
                );
            }
            serde_json::json!({ "current_program_scene": current, "sources": sources })
        };
        e.load_obs(&doc(false, "Main"));
        let started = e.lock().started.get("Main").copied();
        assert!(started.is_some());
        // The stinger's video, as `go_stinger` adds it.
        e.set_source(STINGER, SourceKind::Color { width: 8, height: 8, rgba: [1, 2, 3, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let stop = Arc::new(AtomicBool::new(false));
        let empty = Arc::new(AtomicU64::new(0));
        let watcher = {
            let (state, stop, empty) = (e.state.clone(), stop.clone(), empty.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let st = state.lock().unwrap_or_else(|p| p.into_inner());
                    if st.on_air.as_deref() != Some("Main") || st.scenes.get("Main").is_none_or(Vec::is_empty) {
                        empty.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        for i in 0..100 {
            e.load_obs(&doc(i % 2 == 0, "Main"));
        }
        stop.store(true, Ordering::Release);
        let _ = watcher.join();
        assert_eq!(empty.load(Ordering::Relaxed), 0, "never nothing on air");
        assert_eq!(e.lock().started.get("Main").copied(), started, "the scene's clock goes on");
        assert!(e.lock().sources.contains_key(&STINGER), "the stinger plays on");
        assert!(e.lock().sources.keys().all(|id| *id < 1000 || *id == STINGER), "ids count on from the collection's");
        // Fade to black (a scene of the engine's own) stays on air through full loads.
        e.set_scene("black", Vec::new());
        e.go("black").unwrap_or_else(|x| panic!("{x}"));
        e.load_obs(&doc(true, "Main"));
        assert_eq!(e.lock().on_air.as_deref(), Some("black"));
        // A switch made with `go`, then black, then a load: black stays (the switch isn't
        // taken for the collection's own change; peer's R3).
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        e.go("black").unwrap_or_else(|x| panic!("{x}"));
        e.load_obs(&doc(false, "Other"));
        assert_eq!(e.lock().on_air.as_deref(), Some("black"), "black stays on air");
        // Another collection's own pick goes on air (when black isn't on).
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        e.load_obs(&doc(true, "Main"));
        e.load_obs(&doc(false, "Other"));
        assert_eq!(e.lock().on_air.as_deref(), Some("Other"));
        // A fade from a scene the load takes out plays to its end, then the scene goes.
        let with_third = serde_json::json!({ "current_program_scene": "Other", "sources": [
            { "id": "color_source", "name": "Back", "settings": { "color": 0xFF00_00FFu32 } },
            { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Back" }] } },
            { "id": "scene", "name": "Other", "settings": { "items": [{ "name": "Back" }] } },
            { "id": "scene", "name": "Copy", "settings": { "items": [{ "name": "Back" }] } } ] });
        e.load_obs(&with_third);
        e.go("Copy").unwrap_or_else(|x| panic!("{x}"));
        e.go_with("Main", TransitionKind::Fade, 400).unwrap_or_else(|x| panic!("{x}"));
        e.load_obs(&doc(false, "Main"));
        assert!(e.lock().moving.is_some(), "the fade goes on");
        assert!(e.lock().scenes.contains_key("Copy"), "from the scene it started from");
        std::thread::sleep(Duration::from_millis(700));
        assert!(e.lock().moving.is_none());
        assert!(!e.lock().scenes.contains_key("Copy"), "then that scene goes");
        // Taken out mid-fade and put back (an undo) before it ends: it stays.
        e.load_obs(&with_third);
        e.go("Copy").unwrap_or_else(|x| panic!("{x}"));
        e.go_with("Main", TransitionKind::Fade, 400).unwrap_or_else(|x| panic!("{x}"));
        e.load_obs(&doc(false, "Main"));
        e.load_obs(&with_third);
        std::thread::sleep(Duration::from_millis(700));
        assert!(e.lock().scenes.contains_key("Copy"), "back in the collection: kept");
    }

    #[test]
    fn a_handed_over_capture_keeps_its_picture_until_the_new_one_opens() {
        let Some(e) = engine() else { return };
        e.set_source(7, SourceKind::Color { width: 8, height: 8, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let slow: Opener = Box::new(|| {
            std::thread::sleep(Duration::from_millis(600));
            Ok((Live::Still, false))
        });
        e.open_soon(7, None, slow).unwrap_or_else(|x| panic!("{x}"));
        {
            let st = e.lock();
            let src = &st.sources[&7];
            assert!(src.opening.is_some(), "still opening");
            assert!(src.size.is_some(), "the old picture stays meanwhile");
        }
        let until = Instant::now() + Duration::from_secs(5);
        while e.opening() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(e.opening(), 0);
        // One that fails straight away lets the old one go with it.
        let bad: Opener = Box::new(|| Err(AvError::Source("no".into())));
        e.set_source(8, SourceKind::Color { width: 8, height: 8, rgba: [0, 255, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        assert!(e.open_soon(8, None, bad).is_err());
        assert!(!e.lock().sources.contains_key(&8));
    }

    /// A source with sound that finishes opening after its settings were set keeps them: it
    /// stays muted (it came back unmuted at full volume: muted media blasting on air; 20 Nov).
    #[test]
    fn a_slow_source_keeps_the_sound_settings_set_while_it_opened() {
        let Some(e) = engine() else { return };
        let slow: Opener = Box::new(|| {
            std::thread::sleep(Duration::from_millis(400));
            Ok((Live::Still, true))
        });
        e.open_soon(9, None, slow).unwrap_or_else(|x| panic!("{x}"));
        let muted = SourceSettings { muted: true, volume: 0.1, ..SourceSettings::default() };
        e.set_sound(9, muted);
        let until = Instant::now() + Duration::from_secs(5);
        while e.opening() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(e.opening(), 0);
        let kept = e.mixer.lock().unwrap_or_else(|p| p.into_inner()).settings(9);
        assert_eq!(kept, Some(muted), "still muted after it opened");
    }

    /// The virtual camera's way: pictures handed out as NV12 at the tap's size and rate, each
    /// distinct canvas drawn once, nothing after the tap stops.
    #[test]
    fn frame_taps_get_nv12_at_their_size_and_rate() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        assert!(e.go("Main").is_ok());
        /// Each frame's size, first luma byte and chroma length.
        type Got = Arc<Mutex<Vec<(u32, u32, u8, usize)>>>;
        let got: Got = Arc::new(Mutex::new(Vec::new()));
        let sink = |got: &Got| -> FrameSink {
            let got = got.clone();
            Box::new(move |f: &Nv12, _ts| {
                got.lock().unwrap_or_else(|p| p.into_inner()).push((f.width, f.height, f.y[0], f.uv.len()));
            })
        };
        let small = Canvas::scaled(e.config, 320, 180);
        let half = Canvas { every: 2, ..Canvas::scaled(e.config, 160, 90) };
        let slow = Arc::new(Mutex::new(Vec::new()));
        let from = e.stats().frames;
        let a = e.tap_frames(small, sink(&got)).unwrap_or_else(|x| panic!("{x}"));
        let b = e.tap_frames(half, sink(&slow)).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(700));
        let drawn = e.stats().frames - from;
        assert!(e.stop_tap(a) && e.stop_tap(b));
        assert!(!e.stop_tap(a), "stopped once");
        let fast = got.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let slow = slow.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // Every frame drawn meanwhile (counted against what the engine drew: GitHub's Mac VM draws
        // a fraction of 30 a second, and a fixed "8" failed there).
        assert!(fast.len() >= 3 && fast.len() as u64 + 3 >= drawn, "{} of {drawn} drawn", fast.len());
        assert!(fast.iter().all(|&(w, h, y, uv)| (w, h) == (320, 180) && uv == 320 * 90 && y > 200), "white, 320x180");
        assert!(!slow.is_empty() && slow.len() < fast.len(), "every other frame: {} of {}", slow.len(), fast.len());
        assert!(slow.iter().all(|&(w, h, ..)| (w, h) == (160, 90)));
        let after = fast.len();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(got.lock().unwrap_or_else(|p| p.into_inner()).len(), after, "nothing after stopping");
        // Odd sides, no rate, or a scene that isn't there are refused.
        assert!(e.tap_frames(Canvas::scaled(e.config, 321, 180), sink(&got)).is_err());
        assert!(e.tap_frames(Canvas { every: 0, ..Canvas::main(e.config) }, sink(&got)).is_err());
        assert!(e.tap_frames(Canvas { scene: Some("Nope".into()), ..Canvas::main(e.config) }, sink(&got)).is_err());
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn only_screen_captures_hand_over() {
        let screen = SourceKind::Screen { display: 1, uuid: None, sound: false, hide: Vec::new(), cursor: true };
        let window = SourceKind::Window { id: 4, cursor: true, app: String::new(), title: String::new() };
        assert!(hands_over(&screen, &window));
        assert!(hands_over(&window, &screen));
        let cam = SourceKind::Camera("a".into());
        assert!(!hands_over(&cam, &SourceKind::Camera("b".into())));
        assert!(!hands_over(&screen, &cam));
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
        if crate::monitor::devices().is_empty() || e.set_monitoring(true, None).is_err() {
            return; // no speakers here (a CI machine)
        }
        std::thread::sleep(Duration::from_millis(1200));
        let (short, played) = e.monitoring_stats().unwrap_or_default();
        e.set_monitoring(false, None).unwrap_or_else(|x| panic!("{x}"));
        assert!(played > 40_000, "about a second played: {played}");
        assert!(short < 10, "the device seldom ran short: {short}");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn monitoring_with_missing_device_succeeds_and_drops_cleanly() {
        let Some(e) = engine() else { return };
        let r = e.set_monitoring(true, Some("__ferrocast_missing_monitor_device__"));
        assert!(r.is_ok(), "missing monitor device must retry instead of failing: {r:?}");
        assert!(e.monitoring_stats().is_some());
        let t = std::time::Instant::now();
        e.set_monitoring(false, None).unwrap_or_else(|x| panic!("{x}"));
        assert!(t.elapsed() < Duration::from_millis(300), "stopped promptly: {:?}", t.elapsed());
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

    /// An outside Shadow spills past the box: the outline is the picture's own box, where it's
    /// drawn, moved or not; an older one (not outside) is outlined with its shadow's room.
    #[test]
    fn an_outside_shadow_leaves_the_outline_on_the_picture() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 100, height: 50, rgba: [200, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let marks = crate::preview::Marks {
            scene: None,
            picked: Some(("Main".into(), vec![7])),
            colors: vec![[1.0, 0.0, 0.0, 1.0]],
        };
        let corners = |e: &Engine, outside: bool, at: (f32, f32)| {
            let mut it = SceneItem::new(1, Placement { x: at.0, y: at.1, ..Default::default() });
            it.item_id = 7;
            it.effects = vec![Effect::Shadow { rgba: [0.0, 0.0, 0.0, 0.5], blur: 10.0, x: 0.0, y: 0.0, outside }];
            e.set_scene("Main", vec![it]);
            e.go("Main").unwrap_or_else(|x| panic!("{x}"));
            let st = e.lock();
            outlines(&st, &marks, (640, 360), now_ns())[0].corners
        };
        assert_eq!(corners(&e, true, (10.0, 20.0)), [(10.0, 20.0), (110.0, 20.0), (110.0, 70.0), (10.0, 70.0)]);
        assert_eq!(corners(&e, true, (300.0, 100.0)), [(300.0, 100.0), (400.0, 100.0), (400.0, 150.0), (300.0, 150.0)]);
        assert_eq!(corners(&e, false, (10.0, 20.0))[2], (130.0, 90.0), "grown by 10 on every side");
    }

    /// A source picked inside a group is outlined on the canvas, through its group, with
    /// handles (owner, 11 Nov: it had no frame, so it couldn't be resized).
    #[test]
    fn a_picked_source_in_a_group_is_outlined_through_it() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 100, height: 50, rgba: [200, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut kid = SceneItem::new(1, Placement { x: 10.0, y: 20.0, ..Default::default() });
        kid.item_id = 9;
        e.set_scene("Box", vec![kid]);
        let mut group =
            SceneItem::new(0, Placement { x: 100.0, y: 100.0, scale_x: 2.0, scale_y: 2.0, ..Default::default() });
        group.item_id = 3;
        group.scene = Some("Box".into());
        group.group = Some((200, 100));
        e.set_scene("Main", vec![group]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let marks = crate::preview::Marks {
            scene: None,
            picked: Some(("Main".into(), vec![9])),
            colors: vec![[1.0, 0.0, 0.0, 1.0]],
        };
        let st = e.lock();
        let o = outlines(&st, &marks, (640, 360), now_ns());
        assert_eq!(o.len(), 1, "the source inside, outlined");
        assert!(o[0].handles);
        assert_eq!(o[0].corners, [(120.0, 140.0), (320.0, 140.0), (320.0, 240.0), (120.0, 240.0)]);
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
        // Frames still on the card at the pause went in before it: the picture's time only goes
        // forward through the resume (they came after it, with times from before, 17 Nov review).
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=dts_time", "-of", "csv=p=0"])
            .arg(&one)
            .output()
            .unwrap_or_else(|x| panic!("{x}"));
        let times: Vec<f64> =
            String::from_utf8_lossy(&probe.stdout).lines().filter_map(|l| l.trim_matches(',').parse().ok()).collect();
        assert!(times.len() > 60, "{} frames", times.len());
        assert!(times.windows(2).all(|w| w[1] > w[0]), "the time went back: {times:?}");
    }

    #[cfg(target_os = "macos")]
    fn has_ffprobe() -> bool {
        std::process::Command::new("ffprobe").arg("-version").output().is_ok_and(|o| o.status.success())
    }

    /// What a stream sent: (video?, ms, keyframe?, codec header?).
    #[cfg(target_os = "macos")]
    type Sent = Arc<Mutex<Vec<(bool, u32, bool, bool)>>>;
    #[cfg(target_os = "macos")]
    fn sink_into(sent: &Sent) -> Box<dyn FnMut(Tag) + Send> {
        let sent = sent.clone();
        Box::new(move |t: Tag| {
            let v = match t {
                // FLV video: the frame type in the top nibble (1 key), then AVC packet type
                // (0 the sequence header).
                Tag::Video { ms, body } => {
                    (true, ms, body.first().is_some_and(|b| b >> 4 == 1), body.get(1) == Some(&0))
                }
                Tag::Audio { ms, body } => (false, ms, false, body.get(1) == Some(&0)),
            };
            sent.lock().unwrap_or_else(|p| p.into_inner()).push(v);
        })
    }

    #[cfg(target_os = "macos")]
    fn shared_scene(e: &Engine) {
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 90, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 500, beep_ms: 100 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
    }

    /// A recording "Same as stream" joins the running stream's encode (one encode, 19 Nov: the
    /// Mac's one encoder made 31 of 60 fps doing two at 4K60): the file plays from 0 with
    /// picture and sound, and the stream never stops sending.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_recording_joins_a_running_streams_encode() {
        let Some(e) = engine() else { return };
        shared_scene(&e);
        let sent: Sent = Arc::default();
        let id = e.start_output(OutputKind::Stream(sink_into(&sent))).unwrap_or_else(|x| panic!("{x}"));
        let file = std::env::temp_dir().join(format!("ferrocore-joined-{}.mp4", std::process::id()));
        std::thread::sleep(Duration::from_millis(1500));
        e.record_on(id, &file).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.record_on(id, &file).is_err(), "one recording at a time");
        std::thread::sleep(Duration::from_millis(2000));
        e.record_off(id).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(500));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.error(), None);
        let video: Vec<u32> =
            sent.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|t| t.0).map(|t| t.1).collect();
        assert!(video.len() > 100, "the stream went on throughout: {} video tags", video.len());
        assert!(video.windows(2).all(|w| w[1] >= w[0]), "the stream's time never went back");
        if !has_ffprobe() {
            eprintln!("skipped the check: no ffprobe");
            return;
        }
        let (secs, av) = probed(&file).unwrap_or_else(|| panic!("ffprobe couldn't read {}", file.display()));
        assert!(av, "the file has picture and sound");
        assert!((1.5..2.6).contains(&secs), "about 2 s, from 0: {secs:.2}");
        let _ = std::fs::remove_file(file);
    }

    /// The recording sharing a stream's encode pauses alone: the stream goes on sending, the
    /// file leaves the time paused out (CodeRabbit, 19 Nov: pause was refused once both ran).
    #[test]
    #[cfg(target_os = "macos")]
    fn a_shared_recording_pauses_while_the_stream_goes_on() {
        let Some(e) = engine() else { return };
        shared_scene(&e);
        let sent: Sent = Arc::default();
        let id = e.start_output(OutputKind::Stream(sink_into(&sent))).unwrap_or_else(|x| panic!("{x}"));
        let file = std::env::temp_dir().join(format!("ferrocore-shared-pause-{}.mp4", std::process::id()));
        let plain = std::env::temp_dir().join(format!("ferrocore-shared-plain-{}.mp4", std::process::id()));
        let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
        sleep(500);
        // One without pauses first, for how much shorter the sound ends anyway: it reaches the
        // encoder after the picture, so its last moments miss any recording's stop.
        e.record_on(id, &plain).unwrap_or_else(|x| panic!("{x}"));
        sleep(2500);
        e.record_off(id).unwrap_or_else(|x| panic!("{x}"));
        e.record_on(id, &file).unwrap_or_else(|x| panic!("{x}"));
        sleep(1500);
        e.pause_recording(id, true).unwrap_or_else(|x| panic!("{x}"));
        let before = sent.lock().unwrap_or_else(|p| p.into_inner()).len();
        sleep(1500);
        assert!(e.output_record_state(id).is_ok_and(|r| r.paused), "the recording shows paused");
        let during = sent.lock().unwrap_or_else(|p| p.into_inner()).len() - before;
        assert!(during > 60, "the stream went on while the recording paused: {during} tags");
        e.pause_recording(id, false).unwrap_or_else(|x| panic!("{x}"));
        sleep(1500);
        // More pauses: the sound mustn't drift from the picture across them (projects-60).
        for _ in 0..3 {
            e.pause_recording(id, true).unwrap_or_else(|x| panic!("{x}"));
            sleep(500);
            e.pause_recording(id, false).unwrap_or_else(|x| panic!("{x}"));
            sleep(800);
        }
        e.record_off(id).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.error(), None);
        let video: Vec<u32> =
            sent.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|t| t.0).map(|t| t.1).collect();
        assert!(video.windows(2).all(|w| w[1] >= w[0]), "the stream's time never went back");
        if !has_ffprobe() {
            eprintln!("skipped the check: no ffprobe");
            return;
        }
        let (secs, av) = probed(&file).unwrap_or_else(|| panic!("ffprobe couldn't read {}", file.display()));
        assert!(av, "the file has picture and sound");
        // 1.5 s, paused 1.5 s, 1.5 s, then 3 × (paused 0.5 s, 0.8 s): about 5.4 s, not 7.4
        // (each resume waits up to a keyframe).
        assert!((4.4..5.8).contains(&secs), "the pauses left out of the file: {secs:.2} s");
        // (start, end) of the picture and the sound.
        let ends = |f: &std::path::Path| {
            let out = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", "stream=codec_type,start_time,duration", "-of", "csv=p=0"])
                .arg(f)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            let of = |kind: &str| -> (f64, f64) {
                let l = out.lines().find(|l| l.starts_with(kind)).unwrap_or_else(|| panic!("no {kind}: {out}"));
                let v: Vec<f64> = l.split(',').skip(1).filter_map(|x| x.trim().parse().ok()).collect();
                (v[0], v[0] + v[1])
            };
            (of("video"), of("audio"))
        };
        let ((vs, ve), (as_, ae)) = ends(&file);
        let ((_, pve), (_, pae)) = ends(&plain);
        assert!((vs - as_).abs() < 0.05, "sound starts with the picture: {vs:.3} {as_:.3}");
        // Cut by time, the 4 pauses take as much sound as picture out (an AAC frame, 21 ms, at
        // most each side of a cut); cut by when the encoder handed them back, it was 0.35 s.
        let (short, plain_short) = (ve - ae, pve - pae);
        assert!(
            (short - plain_short).abs() < 0.1,
            "the sound ends {short:.3} s before the picture after 4 pauses, {plain_short:.3} s without"
        );
        let _ = std::fs::remove_file(plain);
        let gaps = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
            .arg(&file)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let mut ts: Vec<f64> = gaps.lines().filter_map(|l| l.trim().parse().ok()).collect();
        ts.sort_by(f64::total_cmp);
        let worst = ts.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(worst < 0.2, "no hole in the picture where it paused: {worst:.3} s");
        let _ = std::fs::remove_file(file);
    }

    /// An encoder that doesn't make a keyframe when asked (FFmpeg's: x264, NVENC, QSV keep
    /// their own interval): a shared recording paused and resumed goes on only at its next
    /// real keyframe; frames before it lean on frames left out, and would show smeared
    /// (projects-60). With keyframes 4 s apart, nothing after the pause is written here.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_shared_recording_resumes_only_at_a_real_keyframe() {
        if crate::ffenc::h264_encoder().is_none() {
            eprintln!("skipped: no FFmpeg here");
            return;
        }
        let video = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 640,
            height: 360,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 2000,
            rate: RateControl::Cbr,
            keyframe_secs: 4,
            software: true,
        };
        let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let config = EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() };
        let e = Engine::new(config, video, audio).unwrap_or_else(|x| panic!("{x}"));
        shared_scene(&e);
        let file = std::env::temp_dir().join(format!("ferrocore-x264-pause-{}.mp4", std::process::id()));
        let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
        // The recording from the start (the encoder's first frame, always a keyframe, starts the
        // file; joined later, it would wait for the next one, 4 s on), the stream joining it.
        let id = e.start_output(OutputKind::Record(file.clone())).unwrap_or_else(|x| panic!("{x}"));
        let sent: Sent = Arc::default();
        e.stream_on(id, sink_into(&sent)).unwrap_or_else(|x| panic!("{x}"));
        sleep(1500);
        e.pause_recording(id, true).unwrap_or_else(|x| panic!("{x}"));
        sleep(500);
        e.pause_recording(id, false).unwrap_or_else(|x| panic!("{x}"));
        // Up to 3.5 s on the output's clock: still before its next keyframe, at 4 s.
        sleep(1300);
        assert!(e.output_record_state(id).is_ok_and(|r| r.paused), "still waiting for a keyframe to go on");
        e.record_off(id).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        if !has_ffprobe() {
            eprintln!("skipped the check: no ffprobe");
            return;
        }
        let (secs, _) = probed(&file).unwrap_or_else(|| panic!("ffprobe couldn't read {}", file.display()));
        assert!(secs < 1.8, "nothing after the pause without a keyframe: {secs:.2} s (2.8 s when written)");
        let _ = std::fs::remove_file(file);
    }

    /// A recording joined and stopped before its first frame leaves no empty file behind.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_recording_stopped_before_its_first_frame_leaves_no_file() {
        let Some(e) = engine() else { return };
        shared_scene(&e);
        let sent: Sent = Arc::default();
        let id = e.start_output(OutputKind::Stream(sink_into(&sent))).unwrap_or_else(|x| panic!("{x}"));
        let file = std::env::temp_dir().join(format!("ferrocore-unborn-{}.mp4", std::process::id()));
        // Straight after: most often no frame came between (a frame that did starts the file,
        // which is then a real one); never an empty file.
        e.record_on(id, &file).unwrap_or_else(|x| panic!("{x}"));
        e.record_off(id).unwrap_or_else(|x| panic!("{x}"));
        let left = std::fs::metadata(&file).map(|m| m.len()).ok();
        assert_ne!(left, Some(0), "an empty file was left at {}", file.display());
        let _ = std::fs::remove_file(&file);
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
    }

    /// A stream joins a running recording's encode: it opens with the codec headers and a
    /// keyframe at 0, no sound from before it; when it leaves, the recording goes on.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_stream_joins_a_running_recordings_encode() {
        let Some(e) = engine() else { return };
        shared_scene(&e);
        let file = std::env::temp_dir().join(format!("ferrocore-joined-rec-{}.mp4", std::process::id()));
        let id = e.start_output(OutputKind::Record(file.clone())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1500));
        let sent: Sent = Arc::default();
        e.stream_on(id, sink_into(&sent)).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.stream_on(id, sink_into(&sent)).is_err(), "one stream at a time");
        std::thread::sleep(Duration::from_millis(1500));
        e.stream_off(id).unwrap_or_else(|x| panic!("{x}"));
        let at_off = sent.lock().unwrap_or_else(|p| p.into_inner()).len();
        std::thread::sleep(Duration::from_millis(1000));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let sent = sent.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(sent.len(), at_off, "nothing sent after the stream left");
        let video: Vec<_> = sent.iter().filter(|t| t.0).collect();
        assert!(video.len() > 40, "{} video tags", video.len());
        assert!(video[0].3, "the stream opens with the video header: {:?}", &video[..2]);
        assert!(video[1].2 && video[1].1 < 50, "then a keyframe at 0: {:?}", video[1]);
        assert!(sent.iter().filter(|t| !t.0 && !t.3).all(|t| t.1 < 10_000), "no sound timed from before the stream");
        if !has_ffprobe() {
            eprintln!("skipped the check: no ffprobe");
            return;
        }
        let (secs, av) = probed(&file).unwrap_or_else(|| panic!("ffprobe couldn't read {}", file.display()));
        assert!(av && (3.4..4.6).contains(&secs), "the recording went on throughout: {secs:.2} s");
        let _ = std::fs::remove_file(file);
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

    /// Picture and sound stay together through the engine, as OBS's "Sync Test Pair" sources
    /// check by eye (test/test-input/sync-pair-*.c): a clip that flashes white and beeps at
    /// the same instant every second is played, recorded, and each flash in the recording is
    /// matched with its beep. Needs ffmpeg (skipped without it). `FERROCAST_SYNC_SECS` records
    /// longer than the 6 s default, to see drift.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn a_flash_and_its_beep_stay_together() {
        let secs: u64 = std::env::var("FERROCAST_SYNC_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(6);
        let dir = std::env::temp_dir();
        let (clip, rec) = (dir.join("ferrocore-sync-clip.mp4"), dir.join("ferrocore-sync-rec.mp4"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!(
                "color=c=black:s=640x360:r=30:d={},drawbox=c=white:t=fill:enable='lt(mod(t\\,1)\\,0.1)'",
                secs + 2
            ))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=f=1000:r=48000:d={},volume='if(lt(mod(t\\,1)\\,0.1)\\,1\\,0)':eval=frame", secs + 2))
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&clip)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("skipped the check: no ffmpeg");
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Media { path: clip.clone(), looping: false }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let id = e.start_output(OutputKind::Record(rec.clone())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_secs(secs));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));

        let flashes = flash_times(&rec);
        let beeps = beep_times(&rec);
        let offsets: Vec<f64> = flashes
            .iter()
            .filter_map(|f| beeps.iter().map(|b| b - f).min_by(|a, b| a.abs().total_cmp(&b.abs())))
            .filter(|d| d.abs() < 0.5)
            .collect();
        eprintln!("flashes {flashes:.3?}\nbeeps   {beeps:.3?}\nsound minus picture {offsets:.3?} s");
        assert!(offsets.len() >= 3, "too few flash and beep pairs: {flashes:?} {beeps:?}");
        // Within about a frame and a half at 30 fps: well inside what anyone can see (~45 ms).
        assert!(offsets.iter().all(|d| d.abs() <= 0.05), "out of sync: {offsets:.3?} s");
    }

    /// When the recording's picture turns bright (a flash starts), in seconds.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn flash_times(file: &std::path::Path) -> Vec<f64> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-an", "-vf", "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-", "-f", "null", "-"])
            .output()
            .unwrap_or_else(|x| panic!("{x}"));
        let text = String::from_utf8_lossy(&out.stdout);
        let (mut at, mut bright, mut times) = (0.0, false, Vec::new());
        for line in text.lines() {
            if let Some(t) = line.split("pts_time:").nth(1) {
                at = t.trim().parse().unwrap_or(at);
            } else if let Some(y) = line.strip_prefix("lavfi.signalstats.YAVG=") {
                let now = y.trim().parse::<f64>().unwrap_or(0.0) > 128.0;
                if now && !bright {
                    times.push(at);
                }
                bright = now;
            }
        }
        times
    }

    /// When the recording's sound starts after a silence (a beep starts), in seconds.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn beep_times(file: &std::path::Path) -> Vec<f64> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "info", "-i"])
            .arg(file)
            .args(["-vn", "-af", "silencedetect=n=-30dB:d=0.2", "-f", "null", "-"])
            .output()
            .unwrap_or_else(|x| panic!("{x}"));
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter_map(|l| l.split("silence_end: ").nth(1))
            .filter_map(|t| t.split_whitespace().next()?.parse().ok())
            .collect()
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

    /// Twitch Enhanced Broadcasting: one stream carries the main picture and two smaller ones,
    /// each with its own header and frames under its track number (1, 2), the main track's
    /// packets as before.
    #[test]
    // Needs the system's own encoder (GitHub's Windows runner has none that opens here).
    #[cfg(target_os = "macos")]
    fn a_stream_sends_more_qualities_as_tracks() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [200, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let into = got.clone();
        let send: Box<dyn FnMut(Tag) + Send> = Box::new(move |t| {
            if let Tag::Video { body, .. } = t {
                into.lock().unwrap_or_else(|e| e.into_inner()).push(body);
            }
        });
        let canvas = Canvas { video_tracks: vec![(320, 180, 600), (256, 144, 300)], ..Canvas::main(e.config()) };
        let id = e.start_output_on(OutputKind::Stream(send), canvas).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1500));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let tags = got.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Multitrack video: 0x80 | frame type | 6, then one track + packet type, avc1, track id.
        let track = |t: &[u8]| (t.len() > 6 && t[0] & 0x8F == 0x86 && &t[2..6] == b"avc1").then(|| (t[6], t[1] & 0x0F));
        // Every quality that started (an encoder that won't open at a size is left out: GitHub's
        // Windows runner has none for small ones), and on a Mac both.
        let mut started: Vec<u8> = tags.iter().filter_map(|t| track(t)).map(|(k, _)| k).collect();
        started.sort_unstable();
        started.dedup();
        #[cfg(target_os = "macos")]
        assert_eq!(started, [1, 2]);
        for n in started {
            let mine: Vec<u8> =
                tags.iter().filter_map(|t| track(t)).filter(|(k, _)| *k == n).map(|(_, kind)| kind).collect();
            assert_eq!(mine.first(), Some(&0), "track {n} starts with its header");
            assert!(mine.iter().filter(|k| **k == 1 || **k == 3).count() > 10, "track {n} frames: {}", mine.len());
        }
        // The main track is still plain AVC (0x17 header, then frames).
        assert!(tags.iter().any(|t| t.first() == Some(&0x17)), "the main track");
        // A wrong quality is refused before anything starts.
        let bad = Canvas { video_tracks: vec![(321, 180, 600)], ..Canvas::main(e.config()) };
        assert!(e.start_output_on(OutputKind::Stream(Box::new(|_| {})), bad).is_err());
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
            add(
                2,
                "Game",
                SourceKind::Window { id: 7, cursor: true, app: String::new(), title: String::new() },
                None,
                None,
            );
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

    /// A scene showing fed source 1 (with `effects`) and fed source 2 beside it.
    fn freeze_scene(e: &Engine, effects: Vec<Effect>) {
        let mut one = SceneItem::new(1, Placement::default());
        one.effects = effects;
        let two = SceneItem::new(2, Placement { x: 320.0, ..Placement::default() });
        e.set_scene("Main", vec![one, two]);
    }

    /// A shadow drawn outside the box leaves the picture its size; an older one (not outside)
    /// is fitted into the box with the picture, which shrinks.
    #[test]
    fn a_shadow_outside_keeps_the_picture_its_size() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        let corner = |outside: bool| {
            let mut it = SceneItem::new(
                1,
                Placement {
                    x: 100.0,
                    y: 100.0,
                    bounds: crate::Bounds::Fit,
                    bounds_width: 320.0,
                    bounds_height: 180.0,
                    ..Placement::default()
                },
            );
            it.effects = vec![Effect::Shadow { rgba: [0.0, 0.0, 0.0, 0.0], blur: 40.0, x: 0.0, y: 0.0, outside }];
            e.set_scene("Main", vec![it]);
            e.go("Main").unwrap_or_else(|x| panic!("{x}"));
            e.fed(1).unwrap_or_else(|| panic!("fed")).picture(Picture::solid(64, 36, [255, 0, 0, 255]));
            std::thread::sleep(Duration::from_millis(150));
            let px = e.snapshot().unwrap_or_default();
            // Just inside the box's top-left corner (canvas 640 wide).
            let at = 4 * (103 * 640 + 103);
            px[at..at + 4].to_vec()
        };
        assert_eq!(corner(true)[0], 255, "the picture fills its box");
        assert!(corner(false)[0] < 128, "fitted with its shadow, it starts further in");
    }

    /// The drawer's "with filters" picture has the source's filters on; the plain one doesn't.
    #[test]
    fn a_source_picture_with_its_filters() {
        let Some(e) = engine() else { return };
        for id in [1, 2] {
            e.set_source(id, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        }
        // Black and white, fully.
        freeze_scene(&e, vec![Effect::Look { look: 3, strength: 100.0 }]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        e.fed(1).unwrap_or_else(|| panic!("fed")).picture(Picture::solid(64, 36, [220, 30, 30, 255]));
        std::thread::sleep(Duration::from_millis(150));
        let plain = e.snapshot_source_at(1, (16, 9)).unwrap_or_default();
        let with = e.snapshot_source_filtered(1, (16, 9)).unwrap_or_default();
        let at = 4 * (4 * 16 + 8);
        assert!(plain[at] > 150 && plain[at + 1] < 80, "plain is red: {:?}", &plain[at..at + 4]);
        let (r, g, b) = (i32::from(with[at]), i32::from(with[at + 1]), i32::from(with[at + 2]));
        assert!((r - g).abs() < 12 && (g - b).abs() < 12, "grey with its filter: {r} {g} {b}");
        assert!(e.snapshot_source_filtered(9, (16, 9)).is_none(), "no such source");
    }

    #[test]
    fn freeze_holds_the_picture_until_it_is_off() {
        let Some(e) = engine() else { return };
        for id in [1, 2] {
            e.set_source(id, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        }
        freeze_scene(&e, vec![Effect::Freeze]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let (one, two) = (e.fed(1).unwrap_or_else(|| panic!("fed")), e.fed(2).unwrap_or_else(|| panic!("fed")));
        let px = |e: &Engine, x: usize| e.snapshot().unwrap_or_default()[x * 4..x * 4 + 4].to_vec();
        // Its first picture still comes in (the placeholder doesn't count).
        one.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        two.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(px(&e, 0), [0, 0, 255, 255], "the first picture of a frozen source");
        // New pictures: the frozen one keeps blue, the other one moves on.
        one.picture(Picture::solid(64, 36, [0, 255, 0, 255]));
        two.picture(Picture::solid(64, 36, [255, 0, 0, 255]));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(px(&e, 0), [0, 0, 255, 255], "held");
        assert_eq!(px(&e, 330), [255, 0, 0, 255], "only the frozen source is held");
        // A thumbnail doesn't slip the new picture in either.
        one.picture(Picture::solid(64, 36, [0, 255, 0, 255]));
        let thumb = e.snapshot_scene("Main").unwrap_or_default();
        assert_eq!(&thumb[0..4], &[0, 0, 255, 255], "a thumbnail shows it frozen");
        assert_eq!(e.snapshot_source_at(1, (8, 8)).unwrap_or_default()[0..4], [0, 0, 255, 255]);
        // Off: new pictures come in again.
        freeze_scene(&e, Vec::new());
        one.picture(Picture::solid(64, 36, [0, 255, 0, 255]));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(px(&e, 0), [0, 255, 0, 255], "moving again");
    }

    #[test]
    fn freeze_comes_from_either_filter_name_and_only_when_on() {
        assert_eq!(crate::obs::effect_from("ferrocast_freeze", &serde_json::Value::Null), Some(Effect::Freeze));
        assert_eq!(crate::obs::effect_from("freeze_filter", &serde_json::Value::Null), Some(Effect::Freeze));
        // A freeze switched off in the collection holds nothing.
        let Some(e) = engine() else { return };
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": "color_source", "versioned_id": "color_source_v3", "name": "Red",
                  "settings": { "width": 64, "height": 36, "color": 4278190335u32 },
                  "filters": [{ "id": "ferrocast_freeze", "name": "Freeze", "enabled": false, "settings": {} }] },
                { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Red", "id": 1 }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty(), "nothing left out");
        let st = e.lock();
        assert!(frozen(&st).is_empty(), "a switched-off freeze holds nothing");
    }

    /// A ready-made effect from the gallery draws: a full vignette darkens the corners and
    /// leaves the middle; an unknown one is left out with why; a filter saved before the
    /// gallery (no "preset") keeps its own code.
    #[test]
    fn a_ready_made_effect_draws() {
        let doc = |shader: serde_json::Value| {
            serde_json::json!({
                "current_program_scene": "Main",
                "sources": [
                    { "id": "color_source", "versioned_id": "color_source_v3", "name": "White",
                      "settings": { "width": 640, "height": 360, "color": 4294967295u32 },
                      "filters": [{ "id": "ferrocast_shader", "name": "Look", "settings": shader }] },
                    { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "White", "id": 1 }] } }
                ]
            })
        };
        let Some(e) = engine() else { return };
        let skipped = e.load_obs(&doc(serde_json::json!({ "preset": "vignette", "param1": 1.0, "param2": 0.0 })));
        assert!(skipped.is_empty(), "{skipped:?}");
        std::thread::sleep(Duration::from_millis(200));
        let px = e.snapshot().unwrap_or_default();
        let at = |x: usize, y: usize| px[(y * 640 + x) * 4];
        assert!(at(320, 180) > 240, "the middle stays white: {}", at(320, 180));
        assert!(at(2, 2) < 60, "the corner goes dark: {}", at(2, 2));
        let skipped = e.load_obs(&doc(serde_json::json!({ "preset": "nope" })));
        assert!(skipped.iter().any(|s| s.contains("no ready-made effect called nope")), "{skipped:?}");
        // Saved before the gallery: its own code (invert), no "preset".
        let own = "fn effect(uv: vec2<f32>) -> vec4<f32> { let c = pic(uv); return vec4<f32>(1.0 - c.rgb, c.a); }";
        assert!(e.load_obs(&doc(serde_json::json!({ "shader_text": own }))).is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert!(e.snapshot().unwrap_or_default()[0] < 10, "its own code still runs (white inverted)");
    }

    /// Sound bars on air rise with a tone heard on air, and sit as slivers in silence.
    #[test]
    fn sound_bars_move_with_the_sound_on_air() {
        let Some(e) = engine() else { return };
        let spec = |of: Option<String>| crate::source::visualizer::VisualizerSpec {
            of,
            bars: 16,
            width: 320,
            height: 100,
            rgba: [255, 255, 255, 255],
            mirror: false,
        };
        e.set_source(1, SourceKind::Visualizer(spec(None))).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let lit = |e: &Engine| {
            let px = e.snapshot().unwrap_or_default();
            (0..100usize).filter(|y| (0..320usize).any(|x| px[(y * 640 + x) * 4 + 3] > 0)).count()
        };
        std::thread::sleep(Duration::from_millis(400));
        assert!(lit(&e) <= 3, "silence: slivers only ({} rows lit)", lit(&e));
        // A steady tone heard everywhere.
        e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 100, beep_ms: 100 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_global(2, true);
        std::thread::sleep(Duration::from_millis(1200));
        assert!(lit(&e) > 40, "a tone lifts its bars ({} rows lit)", lit(&e));
        // Listening to a source that isn't there: flat.
        e.set_source(1, SourceKind::Visualizer(spec(Some("Nope".into())))).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(lit(&e) <= 3, "nothing to hear: slivers ({} rows lit)", lit(&e));
    }

    /// A countdown draws its words where it's placed.
    #[test]
    fn a_countdown_draws() {
        let Some(e) = engine() else { return };
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": "ferrocast_countdown", "name": "Timer",
                  "settings": { "minutes": 5, "before": "Starting in ", "end": "Now",
                                "font": { "size": 48 }, "color1": 4294967295u32 } },
                { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Timer", "id": 1 }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty());
        std::thread::sleep(Duration::from_millis(400));
        let px = e.snapshot().unwrap_or_default();
        let lit = px.chunks(4).take(640 * 80).filter(|p| p[3] > 0 && p[0] > 200).count();
        assert!(lit > 200, "white words in the top left: {lit} lit pixels");
    }

    /// A Source Clone draws its source's picture a second time, where it's placed.
    #[test]
    fn a_clone_draws_the_same_picture_again() {
        let Some(e) = engine() else { return };
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": "color_source", "versioned_id": "color_source_v3", "name": "Red",
                  "settings": { "width": 64, "height": 36, "color": 4278190335u32 } },
                { "id": "ferrocast_clone", "name": "Copy", "settings": { "clone": "Red" } },
                { "id": "scene", "name": "Main", "settings": { "items": [
                    { "name": "Red", "id": 1 },
                    { "name": "Copy", "id": 2, "pos": { "x": 320.0, "y": 0.0 } }
                ] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty());
        std::thread::sleep(Duration::from_millis(150));
        let px = e.snapshot().unwrap_or_default();
        assert_eq!(&px[0..4], &[255, 0, 0, 255], "the source");
        assert_eq!(&px[330 * 4..330 * 4 + 4], &[255, 0, 0, 255], "its clone, beside it");
        assert_eq!(&px[200 * 4..200 * 4 + 4], &[0, 0, 0, 0], "nothing between");
    }

    /// A collection: "Main" all blue, "Other" all green, and "Logo" with a small red box at the
    /// top left; `top` names the top layer.
    fn top_doc(top: serde_json::Value) -> serde_json::Value {
        let colour = |name: &str, c: u32, w: u32, h: u32| {
            serde_json::json!({ "id": "color_source", "versioned_id": "color_source_v3", "name": name,
                                "settings": { "width": w, "height": h, "color": c } })
        };
        let scene = |name: &str, item: &str| serde_json::json!({ "id": "scene", "name": name, "settings": { "items": [{ "name": item, "id": 1 }] } });
        serde_json::json!({
            "current_program_scene": "Main",
            "ferrocast_top_layer": top,
            "sources": [
                colour("Blue", 0xFFFF_0000, 640, 360), colour("Green", 0xFF00_FF00, 640, 360),
                colour("Red", 0xFF00_00FF, 32, 32),
                scene("Main", "Blue"), scene("Other", "Green"), scene("Logo", "Red"),
                { "id": "group", "name": "Box", "settings": { "items": [], "cx": 10, "cy": 10 } }
            ]
        })
    }

    #[test]
    fn the_top_layer_is_over_whatever_is_on_air() {
        let Some(e) = engine() else { return };
        assert!(e.load_obs(&top_doc(serde_json::json!("Logo"))).is_empty());
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(150));
        let px = |e: &Engine, x: usize| e.snapshot().unwrap_or_default()[x * 4..x * 4 + 4].to_vec();
        assert_eq!(px(&e, 5), [255, 0, 0, 255], "the logo over Main");
        assert_eq!(px(&e, 100), [0, 0, 255, 255], "Main under it");
        // Mid-fade the logo stays where it is.
        e.go_with("Other", TransitionKind::Fade, 4000).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(px(&e, 5), [255, 0, 0, 255], "still there mid-transition");
        let mid = px(&e, 100);
        assert!(mid[1] > 0 && mid[2] > 0, "Main and Other mixing under it: {mid:?}");
        // Not over itself, not in a thumbnail of a scene.
        e.go("Logo").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(px(&e, 5), [255, 0, 0, 255]);
        assert_eq!(px(&e, 100), [0, 0, 0, 0], "drawn once, not twice over a black canvas");
        let thumb = e.snapshot_scene("Other").unwrap_or_default();
        assert_eq!(&thumb[20..24], &[0, 255, 0, 255], "a thumbnail is the scene alone");
    }

    #[test]
    fn a_colour_mask_hides_where_its_picture_is_black() {
        // Owner's filter check, 11 Nov. Its colour kind reads black and white (the alpha kind
        // reads the picture's own see-through parts).
        let Some(e) = engine() else { return };
        let dir = std::env::temp_dir().join(format!("fc-mask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|x| panic!("{x}"));
        let mask = dir.join("mask.png");
        // Left half black, right half white, as a PNG.
        let (w, h) = (64u32, 36u32);
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0 } else { 255 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        image::save_buffer(&mask, &px, w, h, image::ColorType::Rgba8).unwrap_or_else(|x| panic!("{x}"));
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": "color_source", "versioned_id": "color_source_v3", "name": "Blue",
                  "settings": { "width": 640, "height": 360, "color": 0xFFFF_0000u32 },
                  "filters": [{ "id": "mask_filter", "versioned_id": "mask_filter_v2", "name": "Mask", "enabled": true,
                                "settings": { "image_path": mask.to_string_lossy(), "type": "mask_color_filter.effect", "stretch": true } }] },
                { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Blue", "id": 1 }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty(), "nothing left out");
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(400));
        let shot = e.snapshot().unwrap_or_default();
        let at = |x: usize| shot[x * 4..x * 4 + 4].to_vec();
        assert_eq!(at(600), [0, 0, 255, 255], "the white side shows");
        assert_eq!(at(40)[3], 0, "the black side is hidden: {:?}", at(40));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_group_doesnt_stop_the_picture() {
        let Some(e) = engine() else { return };
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": "color_source", "versioned_id": "color_source_v3", "name": "Blue",
                  "settings": { "width": 640, "height": 360, "color": 0xFFFF_0000u32 } },
                { "id": "group", "name": "Empty", "settings": { "items": [], "cx": 0, "cy": 0 } },
                { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Blue", "id": 1 }, { "name": "Empty", "id": 2 }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty());
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(&e.snapshot().unwrap_or_default()[20..24], &[0, 0, 255, 255], "the scene still shows");
    }

    #[test]
    fn a_top_layer_that_isnt_a_scene_is_left_out() {
        let Some(e) = engine() else { return };
        for top in [serde_json::json!("Nope"), serde_json::json!("Box"), serde_json::json!(7), serde_json::json!(null)]
        {
            assert!(e.load_obs(&top_doc(top.clone())).is_empty());
            assert_eq!(e.lock().top, None, "{top}");
            e.go("Main").unwrap_or_else(|x| panic!("{x}"));
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(&e.snapshot().unwrap_or_default()[20..24], &[0, 0, 255, 255], "{top}: Main alone");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_or_frozen_source_releases_pinned_cv_image_buffer() {
        let Some(e) = engine() else { return };
        let Some(buf) = crate::apple::screen::tests::bgra_buffer(64, 36, [10, 20, 200, 255]) else { return };
        let surface = crate::screen::Surface::of(buf.clone()).unwrap_or_else(|| panic!("BGRA surface"));
        assert!(surface.io_surface().is_some(), "test buffer must have an IOSurface");
        e.gpu.upload_surface(1, &surface).unwrap_or_else(|x| panic!("{x}"));
        {
            let mut st = e.lock();
            let mut src = Src::new(Live::Still, Some((64, 36, true)), None);
            src.sent = 1;
            st.sources.insert(1, src);
        }
        e.set_source(2, SourceKind::Color { width: 64, height: 36, rgba: [0, 255, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Other", vec![SceneItem::new(2, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        // Freeze source 1 while on air, drop local Surface handle, and let frame loop run.
        let mut frozen_item = SceneItem::new(1, Placement::default());
        frozen_item.effects = vec![Effect::Freeze];
        e.set_scene("Main", vec![frozen_item]);
        drop(surface);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            crate::apple::screen::tests::buffer_retain_count(&buf),
            1,
            "frozen source must release its CVImageBuffer"
        );
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[200, 20, 10, 255], "frozen frame still renders");

        // Re-pin a fresh surface on air, then hide it by switching to Other.
        let surface2 = crate::screen::Surface::of(buf.clone()).unwrap_or_else(|| panic!("BGRA surface"));
        e.gpu.upload_surface(1, &surface2).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        drop(surface2);
        std::thread::sleep(Duration::from_millis(120));
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            crate::apple::screen::tests::buffer_retain_count(&buf),
            1,
            "hidden source must release its CVImageBuffer"
        );
        assert_eq!(
            &e.snapshot_scene("Main").unwrap_or_default()[0..4],
            &[200, 20, 10, 255],
            "hidden scene still renders its last frame"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fresh_into_pending_releases_cv_image_buffer() {
        let Some(buf) = crate::apple::screen::tests::bgra_buffer(32, 16, [30, 60, 90, 255]) else { return };
        let surface = crate::screen::Surface::of(buf.clone()).unwrap_or_else(|| panic!("BGRA surface"));
        let fresh = Fresh::Screen(surface);
        assert_eq!(crate::apple::screen::tests::buffer_retain_count(&buf), 2);
        let pending = fresh.into_pending().unwrap_or_else(|| panic!("converted to Pic"));
        assert_eq!(
            crate::apple::screen::tests::buffer_retain_count(&buf),
            1,
            "into_pending must not keep CVImageBuffer pinned while hidden"
        );
        assert!(
            matches!(pending, Fresh::Pic(ref p) if p.width == 32 && p.height == 16 && p.rgba[..4] == [90, 60, 30, 255])
        );
    }

    #[test]
    fn hotplug_reconnect_wakes_only_matching_stopped_sources() {
        let now = 50_000_000_000u64;
        let mut sources = BTreeMap::new();
        let mut dead_cam = Src::new(Live::Still, None, Some(SourceKind::Camera(CameraSpec::from("cam-usb-1"))));
        dead_cam.watch.next_try = now + 16_000_000_000;
        dead_cam.watch.tries = 3;
        sources.insert(1, dead_cam);

        let mut other_cam = Src::new(Live::Still, None, Some(SourceKind::Camera(CameraSpec::from("cam-other"))));
        other_cam.watch.next_try = now + 16_000_000_000;
        other_cam.watch.tries = 3;
        sources.insert(2, other_cam);

        let mut dead_mic = Src::new(Live::Still, None, Some(SourceKind::Microphone("mic-usb-1".into())));
        dead_mic.watch.next_try = now + 8_000_000_000;
        dead_mic.watch.tries = 2;
        sources.insert(3, dead_mic);

        let woke = apply_hotplug_reconnects(&mut sources, &["cam-usb-1".into()], now);
        assert_eq!(woke, 1);
        assert_eq!(sources[&1].watch.next_try, now);
        assert_eq!(sources[&1].watch.tries, 0);
        assert_eq!(sources[&2].watch.next_try, now + 16_000_000_000);
        assert_eq!(sources[&3].watch.next_try, now + 8_000_000_000);

        // Empty list acts as wildcard wake (e.g. generic hotplug reset).
        let woke = apply_hotplug_reconnects(&mut sources, &[], now);
        assert_eq!(woke, 3);
        assert_eq!(sources[&2].watch.next_try, now);
        assert_eq!(sources[&3].watch.next_try, now);
    }
}
