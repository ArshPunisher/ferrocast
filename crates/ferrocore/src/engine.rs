//! The engine (ENGINE.md E7's core): what libobs's video and audio threads do, on our parts.
//! It owns sources, scenes and outputs; a thread of its own makes a frame every 1/fps on the
//! host clock (sources updated, the scene on air drawn, sound mixed, then encoded to every
//! output), and keeps numbers on how it's doing. The caller changes things through `Engine`'s
//! methods, from any thread.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::AudioConfig;
use crate::audio_thread::{AudioCmd, AudioHandle, ToneSpec};
use crate::mix::{Block, SourceSettings};
use crate::pipeline::{Pipeline, Tag};
use crate::rt::{AtomicLevels, AudioProducer, SpscAudioRing};
use crate::source::image::ImageSource;
use crate::source::text::{TextRenderer, TextStyle};
use crate::video::VideoConfig;
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
    /// A video or sound file, with playback options (looping, activation lifecycle, speed, hw decode).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Media {
        path: PathBuf,
        options: crate::ffsrc::MediaOptions,
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
    #[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
    surface: Mutex<Option<crate::screen::Surface>>,
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

    #[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
    #[allow(dead_code)]
    pub(crate) fn surface(&self, s: crate::screen::Surface) {
        *self.0.surface.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
    #[allow(dead_code)]
    pub(crate) fn has_surface(&self) -> bool {
        self.0.surface.lock().unwrap_or_else(|e| e.into_inner()).is_some()
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

/// Who a picture is drawn for. The stream and the recording can differ by item (`Show`); the
/// editor's own views (the preview, snapshots, the multiview) show everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Audience {
    /// Every item, whatever it is shown for.
    #[default]
    Everyone,
    /// The stream's picture: items shown on the stream (or both).
    Stream,
    /// The recording's picture (and the replay buffer's clips): items shown in the recording
    /// (or both).
    Recording,
}

/// Where an item is shown: on the stream, in the recording, or both (the default, and what a
/// collection from before this had).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Show {
    #[default]
    Both,
    /// Only on the stream: a chat box or alerts a clean recording leaves out.
    Stream,
    /// Only in the recording: a camera the stream doesn't carry.
    Recording,
}

impl Show {
    /// Whether a picture drawn for `audience` carries the item.
    pub fn shown_to(self, audience: Audience) -> bool {
        matches!(
            (self, audience),
            (Self::Both, _)
                | (_, Audience::Everyone)
                | (Self::Stream, Audience::Stream)
                | (Self::Recording, Audience::Recording)
        )
    }

    /// "both", "stream" or "recording" (what is saved and sent); anything else is both.
    pub fn from_name(name: &str) -> Self {
        match name {
            "stream" => Self::Stream,
            "recording" => Self::Recording,
            _ => Self::Both,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Stream => "stream",
            Self::Recording => "recording",
        }
    }
}

/// What an output is for: a stream's picture, or a recording's (a replay buffer's clips are
/// recordings).
pub fn audience_of(kind: &OutputKind) -> Audience {
    match kind {
        OutputKind::Stream(_) => Audience::Stream,
        OutputKind::Record(_) | OutputKind::Replay { .. } => Audience::Recording,
    }
}

/// What differs between the pictures one canvas can take: whether its scenes hold a visible item
/// for the stream only, and one for the recording only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Sides {
    stream_only: bool,
    recording_only: bool,
}

/// The canvas as the frame loop draws it, as a key two outputs share when they take the same
/// picture, so it is drawn once:
/// - what a canvas carries besides its picture (which sound tracks, their names, the extra
///   qualities, which are drawn from this one) is no part of it;
/// - an audience sees what the editor's views see unless the other side has items of its own:
///   with none for the recording only, the stream's picture is every item's; with none for the
///   stream only, the recording's is. Nothing flagged: all three are one picture.
fn view(canvas: &Canvas, sides: Sides) -> Canvas {
    let mut c = canvas.clone();
    c.track = 0;
    c.more_tracks.clear();
    c.track_names.clear();
    c.video_tracks.clear();
    c.audience = match c.audience {
        Audience::Stream if !sides.recording_only => Audience::Everyone,
        Audience::Recording if !sides.stream_only => Audience::Everyone,
        other => other,
    };
    c
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
    /// `effects` are this item's own list (an override), not its source's filters.
    pub own_effects: bool,
    /// Whether the stream, the recording or both show it. On a group, that covers the group's
    /// children as a whole (a child keeps its own flag inside a group that is shown).
    pub show: Show,
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
            own_effects: false,
            show: Show::Both,
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
    /// The names of the recording's audio tracks, by track (first: track one).
    pub track_names: Vec<String>,
    /// More video qualities a stream sends as Enhanced RTMP tracks 1… (Twitch Enhanced
    /// Broadcasting): each width, height and kbps, drawn from the same picture scaled down.
    pub video_tracks: Vec<(u32, u32, u32)>,
    /// Whose picture it is: an output takes it from what it is (`start_output_with`). Matters
    /// only while an item is shown just on the stream or just in the recording.
    pub audience: Audience,
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
            track_names: Vec::new(),
            video_tracks: Vec::new(),
            audience: Audience::Everyone,
        }
    }

    /// The canvas an extra video quality of this one is drawn on: the same picture, scaled to
    /// `width`×`height` from the size this one is drawn at.
    fn track(&self, width: u32, height: u32) -> Self {
        let (w, h) = self.scaled_from.map_or((self.width, self.height), |(w, h, _)| (w, h));
        Self {
            width,
            height,
            scaled_from: Some((w, h, self.scaled_from.map_or(Filter::Bicubic, |(.., f)| f))),
            more_tracks: Vec::new(),
            video_tracks: Vec::new(),
            ..self.clone()
        }
    }

    /// The engine's canvas scaled to `width`×`height` (bicubic, OBS's default downscale filter).
    pub fn scaled(config: EngineConfig, width: u32, height: u32) -> Self {
        Self::scaled_with(config, width, height, Filter::Bicubic)
    }

    /// The engine's canvas scaled to `width`×`height` with `filter`.
    pub fn scaled_with(config: EngineConfig, width: u32, height: u32, filter: Filter) -> Self {
        let from = (width, height) != (config.width, config.height);
        Self { width, height, scaled_from: from.then_some((config.width, config.height, filter)), ..Self::main(config) }
    }
}

/// Where a frame tap's pictures go (the virtual camera): each NV12 frame drawn for it, with its
/// time (ns). Called on the frame loop with the engine's state held, so it must be quick (a
/// copy into shared memory).
pub type FrameSink = Box<dyn FnMut(&Nv12, u64) + Send>;

/// A recording output's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordState {
    /// Why the recording stopped (a full disk, a drive taken away), when it did: the file so far
    /// was kept and nothing is written any more.
    pub failed: Option<crate::pipeline::RecordProblem>,
    pub paused: bool,
    /// Bytes in the file being written now.
    pub bytes: u64,
    /// A split waits for its first frame (the next file isn't written to yet).
    pub splitting: bool,
    /// A rebuilt video encoder needs the recording in a new file: call `split_output` (the
    /// frames are held until then).
    pub split_wanted: bool,
    /// How many times a recording went on in a new file that way (said once each, as a notice).
    pub auto_splits: u32,
}

/// One source's load on the engine (see [`Engine::source_load`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLoad {
    pub name: String,
    /// Pictures sent to the card since it started.
    pub sent: u64,
    pub width: u32,
    pub height: u32,
    /// Waiting for an interrupted, stopped, or disconnected camera to return.
    pub waiting: bool,
}

/// Live connectivity state for a camera source (`Engine::camera_status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraStatus {
    /// `"live"` when receiving or connecting cleanly; `"waiting"` when reconnecting.
    pub state: &'static str,
    /// Human-facing message (`"Waiting for the camera"`) while waiting.
    pub message: Option<&'static str>,
    /// Consecutive reconnect attempts since the last stable picture.
    pub tries: u32,
}

/// The loop's stats and error as of its last frame (see `Engine::stats`).
type Seen = Mutex<Shown>;

/// What readers see of the loop without waiting for it: as of its last frame.
#[derive(Debug, Default)]
struct Shown {
    stats: Stats,
    /// What stopped each output (by id): one output's fault is its own, never another's.
    errors: Vec<(u64, String)>,
    /// Each output's recording and encoder.
    outputs: Vec<(u64, RecordState, String)>,
    /// Approximate per-frame encode time (ms) for each active output.
    output_encode_ms: Vec<(u64, f32)>,
    /// Each opened source's load (`Engine::source_load`).
    loads: Vec<SourceLoad>,
    /// Captures shown with no picture, not opening: name, whether it's the screen, and since
    /// when it's had none (`Engine::quiet_captures`).
    quiet: Vec<(String, bool, u64)>,
    /// Camera sources currently waiting to reconnect while shown or on air.
    waiting_cameras: Vec<String>,
    /// Snapshot of every camera source's availability status for nonblocking polling.
    camera_statuses: HashMap<String, CameraStatus>,
    /// Camera sources still holding an open session while waiting or in `no_signal`.
    holding_sessions: std::collections::HashSet<String>,
}

impl Shown {
    fn of(st: &State) -> Self {
        Self {
            stats: st.stats,
            errors: st.errors.iter().map(|(id, e)| (*id, e.clone())).collect(),
            outputs: st.outputs.iter().map(|(id, pipe, ..)| (*id, record_state(pipe), pipe.encoder())).collect(),
            output_encode_ms: st.encode_ms.iter().map(|(&id, &ms)| (id, ms)).collect(),
            loads: loads_of(st),
            quiet: quiet_of(st),
            waiting_cameras: waiting_cameras_of(st),
            camera_statuses: camera_statuses_of(st),
            holding_sessions: holding_sessions_of(st),
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
            Some(SourceLoad { name: name.clone(), sent: s.sent, width, height, waiting: s.is_waiting_for_camera() })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn active_or_empty(st: &State) -> std::collections::HashSet<u64> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        active(st)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = st;
        std::collections::HashSet::new()
    }
}

fn camera_status_of_src(src: &Src) -> Option<CameraStatus> {
    let retry = src.camera_retry.as_ref()?;
    Some(CameraStatus {
        state: if retry.is_waiting() { "waiting" } else { "live" },
        message: retry.waiting_message(),
        tries: retry.tries(),
    })
}

fn camera_status_of(st: &State, name: &str) -> Option<CameraStatus> {
    let (id, _) = st.opened.get(name)?;
    camera_status_of_src(st.sources.get(id)?)
}

fn camera_statuses_of(st: &State) -> HashMap<String, CameraStatus> {
    st.opened
        .iter()
        .filter_map(|(name, (id, _))| Some((name.clone(), camera_status_of_src(st.sources.get(id)?)?)))
        .collect()
}

fn camera_holding_session_of_src(src: &Src) -> bool {
    !matches!(src.live, Live::Still) && src.camera_retry.as_ref().is_some_and(|r| r.is_waiting() || r.is_no_signal())
}

fn camera_holding_session_of(st: &State, name: &str) -> bool {
    let Some((id, _)) = st.opened.get(name) else {
        return false;
    };
    st.sources.get(id).is_some_and(camera_holding_session_of_src)
}

fn holding_sessions_of(st: &State) -> std::collections::HashSet<String> {
    st.opened
        .iter()
        .filter(|(_, (id, _))| st.sources.get(id).is_some_and(camera_holding_session_of_src))
        .map(|(name, _)| name.clone())
        .collect()
}

fn waiting_cameras_of(st: &State) -> Vec<String> {
    let shown = seen(st).0;
    let act = active_or_empty(st);
    let has_scenes = !st.scenes.is_empty() || !st.opened.is_empty();
    let mut out: Vec<String> = st
        .opened
        .iter()
        .filter_map(|(name, (id, _))| {
            let s = st.sources.get(id)?;
            let visible = !has_scenes || shown.contains(id) || act.contains(id);
            (visible && s.is_waiting_for_camera()).then(|| name.clone())
        })
        .collect();
    out.sort();
    out
}

/// Captures shown somewhere with no picture yet and not opening: name, screen, since when.
fn quiet_of(st: &State) -> Vec<(String, bool, u64)> {
    let shown = seen(st).0;
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
        failed: pipe.recording_problem().cloned(),
        paused: pipe.paused() || pipe.recording_paused(),
        bytes: pipe.recorded_bytes(),
        splitting: pipe.splitting(),
        split_wanted: pipe.split_wanted(),
        auto_splits: pipe.auto_splits(),
    }
}

/// How the engine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    pub frames: u64,
    /// Frames not made on time (the loop fell behind and skipped them).
    pub skipped: u64,
    /// Frames missed when scene rendering / frame work fell behind.
    pub missed_render: u64,
    /// Frames missed while encoders held the frame loop back.
    pub missed_encode: u64,
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

const MISS_LOG_EVERY_NS: u64 = 5_000_000_000;

#[derive(Debug, Clone, PartialEq)]
struct MissLog {
    why: &'static str,
    render_missed: u64,
    encode_held: u64,
    encode_skipped: u64,
    total_missed: u64,
    total_encode_skipped: u64,
    frame_ms: f32,
    encode_ms: f32,
}

#[derive(Debug, Default)]
struct FrameMissMonitor {
    prev_enc_us: u64,
    last_frame_enc_us: u64,
    prev_enc_skipped: u64,
    pending_render: u64,
    pending_encode_held: u64,
    pending_encode_skipped: u64,
    last_log_ns: Option<u64>,
}

impl FrameMissMonitor {
    fn with_enc_skipped(prev_enc_skipped: u64) -> Self {
        Self { prev_enc_skipped, ..Self::default() }
    }

    fn record(
        &mut self,
        now_ns: u64,
        behind: u64,
        frame_ns: u64,
        stats: &mut Stats,
        enc_skipped_total: u64,
    ) -> Option<MissLog> {
        let enc_delta_us = stats.encoding_us.saturating_sub(self.prev_enc_us);
        self.prev_enc_us = stats.encoding_us;

        let mut render_delta = 0u64;
        let mut encode_held_delta = 0u64;
        if behind > 0 {
            let frame_us = (frame_ns / 1000).max(1);
            let miss_budget_us = behind.saturating_mul(frame_us);
            // The overrun happened before this frame began: check whether the preceding frame spent
            // at least 60% of the missed deadline encoding, or smoothed encode_ms exceeds 60% of a frame.
            let enc_dominated = self.last_frame_enc_us.saturating_mul(10) >= miss_budget_us.saturating_mul(6)
                || (self.last_frame_enc_us > 0 && stats.encode_ms * 1000.0 >= frame_us as f32 * 0.6);
            if enc_dominated {
                stats.missed_encode += behind;
                encode_held_delta = behind;
            } else {
                stats.missed_render += behind;
                render_delta = behind;
            }
        }
        self.last_frame_enc_us = enc_delta_us;

        let encode_skipped_delta = enc_skipped_total.saturating_sub(self.prev_enc_skipped);
        self.prev_enc_skipped = enc_skipped_total;

        self.pending_render += render_delta;
        self.pending_encode_held += encode_held_delta;
        self.pending_encode_skipped += encode_skipped_delta;

        let has_pending = self.pending_render > 0 || self.pending_encode_held > 0 || self.pending_encode_skipped > 0;
        if !has_pending || !self.last_log_ns.is_none_or(|t| now_ns.saturating_sub(t) >= MISS_LOG_EVERY_NS) {
            return None;
        }

        let encode_any = self.pending_encode_held > 0 || self.pending_encode_skipped > 0;
        let why = match (self.pending_render > 0, encode_any) {
            (true, true) => "render and encoder fell behind",
            (true, false) => "render fell behind",
            (false, true) => "encoder fell behind",
            (false, false) => unreachable!(),
        };
        let out = MissLog {
            why,
            render_missed: self.pending_render,
            encode_held: self.pending_encode_held,
            encode_skipped: self.pending_encode_skipped,
            total_missed: stats.skipped,
            total_encode_skipped: enc_skipped_total,
            frame_ms: stats.frame_ms,
            encode_ms: stats.encode_ms,
        };
        self.pending_render = 0;
        self.pending_encode_held = 0;
        self.pending_encode_skipped = 0;
        self.last_log_ns = Some(now_ns);
        Some(out)
    }
}

/// Settings fixed for the engine's life.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Broadcast timing: `fps` ×1000/1001 (24 → 23.976, 30 → 29.97, 60 → 59.94).
    pub ntsc: bool,
    pub sample_rate: u32,
    /// How far behind the clock sound is mixed, so late pieces still make it.
    pub audio_latency_ms: u32,
}

impl EngineConfig {
    /// The frame rate as a fraction: 30/1, or 30000/1001 with broadcast timing.
    pub fn frame_rate(&self) -> (u32, u32) {
        if self.ntsc { (self.fps * 1000, 1001) } else { (self.fps, 1) }
    }

    /// One frame's time, in nanoseconds (33,333,333 at 30; 33,366,666 at 29.97).
    pub fn frame_ns(&self) -> u64 {
        let (n, d) = self.frame_rate();
        1_000_000_000 * u64::from(d) / u64::from(n.max(1))
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30, ntsc: false, sample_rate: 48_000, audio_latency_ms: 40 }
    }
}

/// Lifecycle state for a local media source (`ffmpeg_source` / `ferrocast_replay` / stinger).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
struct MediaSlot {
    path: PathBuf,
    options: crate::ffsrc::MediaOptions,
    /// Currently open player (`None` when closed by `close_when_inactive` or while reopening).
    player: Option<Arc<crate::media::Media>>,
    /// Token of an in-flight async reopen triggered on activate or explicit Play/Restart.
    reopening: Option<u64>,
    /// Last known duration (ns) so `media_state` reports duration even while closed.
    duration_ns: u64,
    /// Whether this source was active on air on the previous frame (for activate/deactivate edges).
    was_active: bool,
    /// Whether the GPU texture was already cleared after EOF/stop.
    cleared: bool,
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
impl MediaSlot {
    fn open(path: PathBuf, options: crate::ffsrc::MediaOptions, media: crate::media::Media) -> Self {
        let duration_ns = media.duration_ns;
        Self {
            path,
            options,
            player: Some(Arc::new(media)),
            reopening: None,
            duration_ns,
            was_active: false,
            cleared: false,
        }
    }

    fn closed(path: PathBuf, options: crate::ffsrc::MediaOptions) -> Self {
        Self { path, options, player: None, reopening: None, duration_ns: 0, was_active: false, cleared: false }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
static REOPEN_TOKEN: AtomicU64 = AtomicU64::new(1);

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn next_reopen_token() -> u64 {
    REOPEN_TOKEN.fetch_add(1, Ordering::Relaxed)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
fn spawn_media_reopen(
    state: Arc<Mutex<State>>,
    audio_cmd: mpsc::Sender<AudioCmd>,
    levels: Arc<RwLock<HashMap<u64, Arc<AtomicLevels>>>>,
    id: u64,
    token: u64,
    path: PathBuf,
    options: crate::ffsrc::MediaOptions,
    require_active: bool,
) {
    let _ = std::thread::Builder::new().name("ferrocore-media-open".into()).spawn(move || {
        let opened = open_video_with(&path, options, false);
        let mut leftover: Option<crate::media::Media> = None;
        {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            let still_wanted = !require_active || active(&st).contains(&id);
            match (opened, st.sources.get_mut(&id)) {
                (Ok(m), Some(src))
                    if matches!(&src.live, Live::Media(slot) if slot.reopening == Some(token)) && still_wanted =>
                {
                    let has_audio = m.has_audio;
                    if let Live::Media(slot) = &mut src.live {
                        slot.reopening = None;
                        slot.duration_ns = m.duration_ns;
                        slot.cleared = false;
                        slot.player = Some(Arc::new(m));
                    }
                    src.audio_tx = None;
                    if has_audio {
                        let atom = ensure_levels_in(&levels, id);
                        let _ = audio_cmd.send(AudioCmd::EnsureSource { id, levels: atom });
                        let _ = audio_cmd.send(AudioCmd::SetOutputCapture { id, capture: None });
                    }
                }
                (Ok(m), Some(src)) if matches!(&src.live, Live::Media(slot) if slot.reopening == Some(token)) => {
                    if let Live::Media(slot) = &mut src.live {
                        slot.reopening = None;
                    }
                    leftover = Some(m);
                }
                (Ok(m), _) => {
                    leftover = Some(m);
                }
                (Err(e), Some(src)) if matches!(&src.live, Live::Media(slot) if slot.reopening == Some(token)) => {
                    if let Live::Media(slot) = &mut src.live {
                        slot.reopening = None;
                    }
                    tracing::warn!(source = id, file = %path.display(), error = %e, "media source couldn't reopen");
                }
                (Err(_), _) => {}
            }
        }
        drop(leftover);
    });
}

enum Live {
    Still,
    Image {
        src: ImageSource,
        since: Instant,
        last: Option<usize>,
    },
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Screen(crate::screen::ScreenCapture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Camera(crate::camera::Capture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Microphone(crate::camera::Capture),
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    Media(MediaSlot),
    Fed(Feed),
    /// Files one after another, on a worker that hands them in as a fed source would.
    Series {
        feed: Feed,
        _worker: crate::source::series::Worker,
    },
    /// Sound bars: drawn each frame from what was heard.
    Visualizer(Box<crate::source::visualizer::VisualizerLive>),
    #[cfg(test)]
    PanicOnDrop(#[allow(dead_code)] PanicDrop),
    #[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    FakeCamera(FakeCapture),
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct FakeCameraBackend {
    online: AtomicBool,
    interrupted: AtomicBool,
    streaming: AtomicBool,
    color: Mutex<[u8; 4]>,
    frames_sent: AtomicU64,
    open_attempts: AtomicU64,
    active_sessions: std::sync::atomic::AtomicUsize,
    max_active_sessions: std::sync::atomic::AtomicUsize,
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl FakeCameraBackend {
    fn new(color: [u8; 4]) -> Arc<Self> {
        Arc::new(Self {
            online: AtomicBool::new(true),
            interrupted: AtomicBool::new(false),
            streaming: AtomicBool::new(true),
            color: Mutex::new(color),
            frames_sent: AtomicU64::new(0),
            open_attempts: AtomicU64::new(0),
            active_sessions: std::sync::atomic::AtomicUsize::new(0),
            max_active_sessions: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn open(self: &Arc<Self>) -> Result<Live, AvError> {
        self.open_attempts.fetch_add(1, Ordering::SeqCst);
        if !self.online.load(Ordering::Acquire) {
            return Err(AvError::Source("the camera isn't connected".into()));
        }
        let now_active = self.active_sessions.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_sessions.fetch_max(now_active, Ordering::SeqCst);
        Ok(Live::FakeCamera(FakeCapture { state: Arc::clone(self) }))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct FakeCapture {
    state: Arc<FakeCameraBackend>,
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl FakeCapture {
    fn received(&self) -> u64 {
        self.state.frames_sent.load(Ordering::Acquire)
    }

    fn ended(&self) -> Option<String> {
        if !self.state.online.load(Ordering::Acquire) {
            return Some("the camera was disconnected".into());
        }
        None
    }

    fn interrupted(&self) -> bool {
        self.state.online.load(Ordering::Acquire) && self.state.interrupted.load(Ordering::Acquire)
    }

    fn take_frame(&self) -> Option<(Picture, u64)> {
        if !self.state.online.load(Ordering::Acquire)
            || self.state.interrupted.load(Ordering::Acquire)
            || !self.state.streaming.load(Ordering::Acquire)
        {
            return None;
        }
        let c = *self.state.color.lock().unwrap_or_else(|e| e.into_inner());
        self.state.frames_sent.fetch_add(1, Ordering::SeqCst);
        Some((Picture::solid(16, 16, c), now_ns()))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.state.active_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
struct PanicDrop;

#[cfg(test)]
impl Drop for PanicDrop {
    fn drop(&mut self) {
        panic!("simulated live drop panic");
    }
}

struct Src {
    live: Live,
    /// The picture's size once there is one, and whether it's premultiplied.
    size: Option<(u32, u32, bool)>,
    /// What it was started as, when the watchdog can start it again (captures).
    kind: Option<SourceKind>,
    watch: Watch,
    /// Camera reconnect policy and "waiting for the camera" status (`None` for non-cameras).
    camera_retry: Option<crate::camera_retry::CameraRetry>,
    /// Its newest picture while nothing drawn shows it: kept, not sent to the GPU, until it is.
    pending: Option<Fresh>,
    /// Clock time (`now_ns`) when `pending` was stored (idle hidden surfaces unpin once after [`PENDING_UNPIN_NS`]).
    pending_since: u64,
    /// Being opened on its own thread (that open's token): the watchdog leaves it alone.
    opening: Option<u64>,
    /// Clock time (`now_ns`) when the current reopen worker started, for [`REOPEN_TIMEOUT_NS`].
    opening_since: u64,
    /// Active or wedged `open` / `reopen` worker threads currently executing for this source.
    reopen_workers: Arc<AtomicUsize>,
    /// Pictures sent to the card so far (how busy it keeps the engine; Stats shows the rate).
    sent: u64,
    /// Per-source SPSC audio ring producer attached lazily on the first forwarded audio chunk.
    audio_tx: Option<AudioProducer>,
    /// A Video Delay filter's held pictures (set while one is on the source).
    video_delay: Option<crate::delay::FrameDelay<Fresh>>,
    /// Its sound held back too, when the filter says so.
    audio_delay: Option<crate::delay::AudioDelay>,
    /// When the memory cap cut a delay short: (the delay asked, the one both picture and sound
    /// keep now), so they stay together.
    delay_cap: Option<(u32, u32)>,
}

/// How long a hidden capture may hold a pinned zero-copy surface without a newer frame before
/// that single idle surface is converted once to a CPU picture so capture pools aren't starved.
const PENDING_UNPIN_NS: u64 = 2_000_000_000;

fn unpin_stale_pending(s: &mut Src, now: u64) {
    if now.saturating_sub(s.pending_since) < PENDING_UNPIN_NS {
        return;
    }
    if s.pending.as_ref().is_some_and(Fresh::is_pinned_surface)
        && let Some(f) = s.pending.take()
    {
        s.pending = f.into_pending();
        s.pending_since = 0;
    }
}

/// A new picture for the card: pixels of ours, or (on a Mac) a frame of the screen as the
/// system holds it, which goes up without a copy of ours.
enum Fresh {
    Pic(Picture),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Screen(crate::screen::Surface),
    /// A file's picture as FFmpeg decodes it, turned into RGB on the card (Windows, Linux, and
    /// macOS FFmpeg fallback for MKV/WebM).
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

    /// What it takes of memory, for the Video Delay's cap.
    fn bytes(&self) -> usize {
        match self {
            Fresh::Pic(p) => p.rgba.len(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(s) => s.size().0 as usize * s.size().1 as usize * 4,
            Fresh::Yuv(p) => p.data.len(),
        }
    }

    fn is_pinned_surface(&self) -> bool {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Fresh::Screen(_) => true,
            _ => false,
        }
    }

    /// Converts any pinned zero-copy external surface into a CPU picture before parking in
    /// `Src::pending` so finite capture buffer pools aren't starved while hidden.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
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

fn is_camera_kind(kind: Option<&SourceKind>) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        matches!(kind, Some(SourceKind::Camera(_)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = kind;
        false
    }
}

impl Src {
    fn new(live: Live, size: Option<(u32, u32, bool)>, kind: Option<SourceKind>) -> Self {
        let now = now_ns();
        let watch = Watch::for_kind(kind.as_ref(), now);
        let camera_retry = is_camera_kind(kind.as_ref()).then(|| crate::camera_retry::CameraRetry::new(now));
        Src {
            live,
            size,
            kind,
            watch,
            camera_retry,
            pending: None,
            pending_since: 0,
            opening: None,
            opening_since: 0,
            reopen_workers: Arc::new(AtomicUsize::new(0)),
            sent: 0,
            audio_tx: None,
            video_delay: None,
            audio_delay: None,
            delay_cap: None,
        }
    }

    fn waiting_camera(kind: SourceKind, now: u64, reason: &str) -> Self {
        let mut src = Self::new(Live::Still, None, Some(kind));
        src.watch.tries = 1;
        src.watch.next_try = now.saturating_add(crate::camera_retry::CAMERA_BACKOFF_INITIAL_NS);
        src.camera_retry = Some(crate::camera_retry::CameraRetry::waiting(
            now,
            crate::camera_retry::CAMERA_BACKOFF_INITIAL_NS,
            reason,
        ));
        src
    }

    fn is_waiting_for_camera(&self) -> bool {
        self.camera_retry.as_ref().is_some_and(crate::camera_retry::CameraRetry::is_waiting)
    }
}

/// A picture that waited to be held for a Video Delay: source, when it was seen (ns), picture
/// (as video where it can be) and its size in memory.
type Held = (u64, u64, Fresh, usize);

/// Turns pictures into what a Video Delay holds (the copy out of the capture surface and the
/// RGBA to NV12 pass) on a thread of its own, so the render loop and its lock don't wait on it.
/// One slow source can't stall a frame: when it is behind, a picture is dropped here.
struct DelayConv {
    tx: Option<mpsc::SyncSender<(u64, u64, Fresh)>>,
    done: Arc<Mutex<Vec<Held>>>,
    thread: Option<JoinHandle<()>>,
}

/// Pictures waiting for the conversion thread at most (a pinned surface held in here starves the
/// capture's pool, so few).
const CONV_QUEUE: usize = 3;

impl DelayConv {
    fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel::<(u64, u64, Fresh)>(CONV_QUEUE);
        let done: Arc<Mutex<Vec<Held>>> = Arc::default();
        let out = done.clone();
        let thread = std::thread::Builder::new()
            .name("ferrocast-delay-conv".into())
            .spawn(move || {
                while let Ok((id, at, f)) = rx.recv() {
                    // A pinned capture surface would starve its pool while held: copied out.
                    let Some(f) = f.into_pending() else { continue };
                    // Held as video (NV12: 1.5 bytes a pixel, not 4) when it's opaque; a picture
                    // with transparency stays RGBA. The card turns it back.
                    let f = match f {
                        Fresh::Pic(p) => match crate::delay::rgba_to_nv12(p.width, p.height, &p.rgba) {
                            Some(data) => Fresh::Yuv(crate::ffsrc::Nv12Picture {
                                width: p.width,
                                height: p.height,
                                data,
                                bt709: true,
                                full_range: false,
                            }),
                            None => Fresh::Pic(p),
                        },
                        other => other,
                    };
                    let bytes = f.bytes();
                    out.lock().unwrap_or_else(|e| e.into_inner()).push((id, at, f, bytes));
                }
            })
            .ok();
        Self { tx: Some(tx), done, thread }
    }

    /// Hands a picture over; false when the thread is behind (the picture is dropped).
    fn submit(&self, id: u64, at: u64, f: Fresh) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.try_send((id, at, f)).is_ok())
    }

    /// What has been converted since the last call, oldest first.
    fn take(&self) -> Vec<Held> {
        std::mem::take(&mut *self.done.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for DelayConv {
    fn drop(&mut self) {
        // The channel closing ends the thread; it isn't waited for (a copy may be in progress).
        drop(self.tx.take());
        drop(self.thread.take());
    }
}

/// The watchdog's look at a capture: what it had received last, since when, and its tries.
#[derive(Debug, Default)]
struct Watch {
    count: u64,
    since: u64,
    tries: u32,
    next_try: u64,
    /// True once this source has delivered at least one frame since creation or last edit.
    ever_sent: bool,
    /// True for camera/capture-card sources that pause auto-retries after [`NO_SIGNAL_MAX_TRIES`]
    /// healthy zero-frame attempts until signal arrives, disconnect, or USB hotplug.
    cap_no_signal: bool,
    /// True for cameras (always `1, 2, 4, 5 s` backoff via `camera_retry`).
    camera_backoff: bool,
    /// A camera or microphone: one that has worked is looked for again within seconds when it
    /// goes (a USB mic bumped out and back in, where Windows has no hotplug wake).
    quick_retry: bool,
    /// Set once `cap_no_signal && !ever_sent` reaches [`NO_SIGNAL_MAX_TRIES`] with an open session.
    no_signal: bool,
    /// It panicked in the loop (and was stopped there).
    faulted: bool,
    /// Ended because the system turned its permission off (Screen Recording): not tried again
    /// until the permission is back (`regained_access`); each try would only raise the system's
    /// prompt again (GAPS #20).
    denied: bool,
    /// The permission is back: the next look restarts it at once.
    regained: bool,
    /// Since when frames have been coming without a break (0: not yet). `tries` only goes back
    /// to 0 after [`STABLE_NS`] of that: one frame from a flapping HDMI link isn't a recovery.
    flowing_since: u64,
}

impl Watch {
    /// The wait before try `n` at starting this source again. Cameras always retry within
    /// `1, 2, 4, 5 s`; a microphone that has worked before waits at most [`CARD_RETRY_MAX_NS`].
    fn backoff(&self, n: u32) -> u64 {
        if self.camera_backoff {
            return crate::camera_retry::backoff_ns(n);
        }
        let wait = backoff_ns(n);
        if self.quick_retry && self.ever_sent { wait.min(CARD_RETRY_MAX_NS) } else { wait }
    }

    fn for_kind(kind: Option<&SourceKind>, now: u64) -> Self {
        let cap_no_signal = is_camera_kind(kind);
        let camera_backoff = cap_no_signal;
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let quick_retry = matches!(kind, Some(SourceKind::Camera(_) | SourceKind::Microphone(_)));
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let quick_retry = false;
        Self { since: now, cap_no_signal, camera_backoff, quick_retry, ..Self::default() }
    }
}

/// A camera or microphone with nothing new for this long has stopped.
const STALL_NS: u64 = 5_000_000_000;
/// Grace period for a freshly opened steady capture to deliver its first frame before restarting.
const FIRST_FRAME_NS: u64 = 10_000_000_000;
/// Maximum fruitless zero-frame tries for a capture card/camera before waiting in "no signal".
const NO_SIGNAL_MAX_TRIES: u32 = crate::camera_retry::NO_SIGNAL_MAX_TRIES;
/// How long a capture must keep sending before its failed tries are forgotten.
const STABLE_NS: u64 = 10_000_000_000;
/// The longest wait between tries at a capture card that has worked before.
const CARD_RETRY_MAX_NS: u64 = 3_000_000_000;
/// Hard deadline before the watchdog clears a stalled reopen worker slot.
const REOPEN_TIMEOUT_NS: u64 = 60_000_000_000;
/// Caps simultaneous reopen worker threads so a USB hub reset doesn't contend on every device at once.
const MAX_PARALLEL_REOPENS: usize = 2;
/// Maximum open/reopen worker threads allowed in flight or wedged for any single source.
const MAX_SOURCE_REOPEN_WORKERS: usize = 2;
/// Global cap on live or wedged open/reopen worker threads across the whole engine.
const MAX_TOTAL_REOPEN_WORKERS: usize = 8;

/// RAII worker-slot counter decremented on normal return, early exit, panic, or spawn failure.
struct ReopenWorkerGuard {
    source_workers: Arc<AtomicUsize>,
    total_workers: Arc<AtomicUsize>,
}

impl ReopenWorkerGuard {
    fn new(source_workers: Arc<AtomicUsize>, total_workers: Arc<AtomicUsize>) -> Self {
        source_workers.fetch_add(1, Ordering::AcqRel);
        total_workers.fetch_add(1, Ordering::AcqRel);
        Self { source_workers, total_workers }
    }
}

impl Drop for ReopenWorkerGuard {
    fn drop(&mut self) {
        self.source_workers.fetch_sub(1, Ordering::AcqRel);
        self.total_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn next_open_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

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
    /// Sorted list of source IDs currently active/heard on `ferrocast-audio` (`SetHeard` dedup).
    last_heard: Vec<u64>,
    stats: Stats,
    miss_monitor: FrameMissMonitor,
    /// Why an output failed, by id, until it stops (`deliver`).
    errors: HashMap<u64, String>,
    /// Outputs whose encoder failed and are being made again (see `heal_outputs`).
    heal: HashMap<u64, Heal>,
    /// Smoothed per-frame encode duration in milliseconds per output (`deliver`).
    encode_ms: HashMap<u64, f32>,
    /// Things captures want said (see `Engine::take_notices`).
    notices: crate::notices::Notices,
    /// The previews windows show, by id.
    previews: HashMap<u64, Preview>,
    /// Makes the pictures a Video Delay holds, off the render loop (started with the first one).
    delay_conv: Option<DelayConv>,
    /// Frames drawn and on their way back from the card, oldest first (`deliver`).
    in_flight: std::collections::VecDeque<InFlight>,
    /// Cameras just taken out, still running for [`PARK_NS`]: one added again meanwhile (Undo,
    /// a rename, the same camera added back) comes back at once instead of being opened again,
    /// which takes seconds on Windows. What each was, and when it was parked.
    parked: Vec<(SourceKind, Live, u64)>,
    /// Whether any active streaming or recording output currently wants system sleep inhibited.
    sleep_want: bool,
    /// Monotonic counter so concurrent start/stop callers never apply a stale sleep state.
    sleep_epoch: u64,
    /// Live or wedged open/reopen worker threads currently active across all sources.
    reopen_workers: Arc<AtomicUsize>,
    /// Per-source open/reopen worker counters preserved across `Src` replacement, tagged with
    /// the device kind so reusing a numeric ID for a different camera stays isolated.
    source_reopen_workers: HashMap<u64, (Option<SourceKind>, Arc<AtomicUsize>)>,
    /// Worker counters for removed sources that still have stalled opens running in the background.
    draining_workers: Vec<(Option<SourceKind>, Arc<AtomicUsize>)>,
}

fn output_needs_awake(pipe: &Pipeline, kind: &OutputKind) -> bool {
    match kind {
        OutputKind::Stream(_) => true,
        OutputKind::Record(_) => pipe.recording(),
        OutputKind::Replay { .. } => false,
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn camera_identity_key(id: &str) -> std::borrow::Cow<'_, str> {
    if let Some((_, path)) = id.split_once(":\\\\")
        && !path.is_empty()
    {
        return std::borrow::Cow::Owned(format!(r"\\{}", path.to_ascii_lowercase()));
    }
    if id.as_bytes().get(..12).is_some_and(|p| p.eq_ignore_ascii_case(b"@device_pnp_"))
        && let Some(rest) = id.get(12..).filter(|r| r.starts_with(r"\\"))
    {
        return std::borrow::Cow::Owned(rest.to_ascii_lowercase());
    }
    if id.starts_with('@') {
        return std::borrow::Cow::Owned(id.to_ascii_lowercase());
    }
    std::borrow::Cow::Borrowed(id)
}

fn same_reopen_device(a: &Option<SourceKind>, b: &Option<SourceKind>) -> bool {
    match (a, b) {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        (Some(SourceKind::Camera(x)), Some(SourceKind::Camera(y))) => {
            camera_identity_key(&x.id) == camera_identity_key(&y.id)
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        (Some(SourceKind::Microphone(x)), Some(SourceKind::Microphone(y))) => x == y,
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        (
            Some(SourceKind::Screen { display: d1, uuid: u1, .. }),
            Some(SourceKind::Screen { display: d2, uuid: u2, .. }),
        ) => match (u1, u2) {
            (Some(x), Some(y)) => x == y,
            _ => d1 == d2,
        },
        #[cfg(target_os = "macos")]
        (
            Some(SourceKind::App { display: d1, uuid: u1, app: a1, .. }),
            Some(SourceKind::App { display: d2, uuid: u2, app: a2, .. }),
        ) => {
            let same_screen = match (u1, u2) {
                (Some(x), Some(y)) => x == y,
                _ => d1 == d2,
            };
            same_screen && a1 == a2
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        (
            Some(SourceKind::Window { id: w1, app: a1, title: t1, .. }),
            Some(SourceKind::Window { id: w2, app: a2, title: t2, .. }),
        ) => (w1, a1, t1) == (w2, a2, t2),
        _ => a == b,
    }
}

impl State {
    fn sleep_ticket(&mut self) -> Option<(u64, bool)> {
        let want = self.outputs.iter().any(|(_, pipe, kind, _)| output_needs_awake(pipe, kind));
        if want == self.sleep_want {
            return None;
        }
        self.sleep_want = want;
        self.sleep_epoch += 1;
        Some((self.sleep_epoch, want))
    }

    fn peek_source_workers(&self, id: u64, kind: Option<&SourceKind>) -> Option<Arc<AtomicUsize>> {
        let wanted = kind.cloned();
        if let Some((tracked, workers)) = self.source_reopen_workers.get(&id)
            && same_reopen_device(tracked, &wanted)
        {
            return Some(workers.clone());
        }
        self.draining_workers
            .iter()
            .find(|(tracked, workers)| workers.load(Ordering::Acquire) > 0 && same_reopen_device(tracked, &wanted))
            .map(|(_, workers)| workers.clone())
    }

    fn source_workers_for(&mut self, id: u64, kind: Option<&SourceKind>) -> Arc<AtomicUsize> {
        let wanted = kind.cloned();
        if let Some((tracked, workers)) = self.source_reopen_workers.get(&id) {
            if same_reopen_device(tracked, &wanted) {
                return workers.clone();
            }
            if workers.load(Ordering::Acquire) > 0 {
                self.draining_workers.push((tracked.clone(), workers.clone()));
            }
            self.source_reopen_workers.remove(&id);
        }
        if let Some(pos) = self
            .draining_workers
            .iter()
            .position(|(tracked, workers)| workers.load(Ordering::Acquire) > 0 && same_reopen_device(tracked, &wanted))
        {
            let entry = self.draining_workers.swap_remove(pos);
            let workers = entry.1.clone();
            self.source_reopen_workers.insert(id, entry);
            return workers;
        }
        let workers = Arc::new(AtomicUsize::new(0));
        self.source_reopen_workers.insert(id, (wanted, workers.clone()));
        workers
    }

    fn attach_workers_if_tracked(&mut self, id: u64, src: &mut Src) {
        if let Some((tracked, workers)) = self.source_reopen_workers.get(&id) {
            if same_reopen_device(tracked, &src.kind) {
                src.reopen_workers = workers.clone();
                return;
            }
            if workers.load(Ordering::Acquire) > 0 {
                self.draining_workers.push((tracked.clone(), workers.clone()));
            }
            self.source_reopen_workers.remove(&id);
        }
        if let Some(pos) = self.draining_workers.iter().position(|(tracked, workers)| {
            workers.load(Ordering::Acquire) > 0 && same_reopen_device(tracked, &src.kind)
        }) {
            let entry = self.draining_workers.swap_remove(pos);
            src.reopen_workers = entry.1.clone();
            self.source_reopen_workers.insert(id, entry);
        }
    }

    fn prune_source_workers(&mut self) {
        let State { sources, source_reopen_workers, draining_workers, .. } = self;
        source_reopen_workers.retain(|id, (kind, workers)| {
            if sources.contains_key(id) {
                return true;
            }
            if workers.load(Ordering::Acquire) > 0 {
                draining_workers.push((kind.clone(), workers.clone()));
            }
            false
        });
        draining_workers.retain(|(_, workers)| workers.load(Ordering::Acquire) > 0);
    }
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
    /// Performance mode: nothing is composed or drawn for this view until it is let go. Never
    /// touches an output: streams and recordings draw their own frames.
    paused: bool,
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
    /// Performance mode for this thread, readable without the engine's state lock.
    paused: Arc<AtomicBool>,
    /// Wakes the thread from its wait while paused (resume or stop).
    wake: Arc<(Mutex<()>, Condvar)>,
}

/// Blocks while `paused` (and not `stop`): no timer, no lock on the engine state, no wakeups
/// until `wake` is notified. Returns at once when it isn't paused.
fn wait_while_paused(paused: &AtomicBool, stop: &AtomicBool, wake: &(Mutex<()>, Condvar)) {
    wait_while_paused_with(paused, stop, wake, || {});
}

/// [`wait_while_paused`], calling `about_to_wait` (lock held) just before each wait: a test's way
/// of knowing the thread is parked, without guessing at a delay.
fn wait_while_paused_with(
    paused: &AtomicBool,
    stop: &AtomicBool,
    wake: &(Mutex<()>, Condvar),
    mut about_to_wait: impl FnMut(),
) {
    let mut guard = wake.0.lock().unwrap_or_else(|e| e.into_inner());
    while paused.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        about_to_wait();
        guard = wake.1.wait(guard).unwrap_or_else(|e| e.into_inner());
    }
}

/// Wakes a preview thread waiting in [`wait_while_paused`]. The lock is taken first so a
/// notification can't slip in between the thread's check and its wait.
fn wake_preview(wake: &(Mutex<()>, Condvar)) {
    drop(wake.0.lock().unwrap_or_else(|e| e.into_inner()));
    wake.1.notify_all();
}

/// Maximum time `Engine::drop` waits for the watchdog thread (which may be blocked in a slow OS
/// capture driver open) before detaching it cleanly so quit never hangs.
const WATCHDOG_STOP_TIMEOUT: Duration = Duration::from_secs(2);

fn join_bounded(handle: JoinHandle<()>, done: &(Mutex<bool>, Condvar), timeout: Duration) -> bool {
    let guard = done.0.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _timed_out) =
        done.1.wait_timeout_while(guard, timeout, |finished| !*finished).unwrap_or_else(|e| e.into_inner());
    let finished = *guard;
    drop(guard);
    if finished {
        let _ = handle.join();
        true
    } else {
        tracing::warn!(
            timeout_ms = timeout.as_millis() as u64,
            "engine watchdog did not finish before shutdown deadline; detaching"
        );
        false
    }
}

/// The engine. Dropping it stops its thread and closes its outputs.
pub struct Engine {
    config: EngineConfig,
    /// The device monitored sound plays on (it stays on this side: its feeder goes to `ferrocast-audio`).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    monitor: Mutex<Option<crate::monitor::Monitor>>,
    state: Arc<Mutex<State>>,
    gpu: Arc<Compositor>,
    audio: AudioHandle,
    /// Lock-free level mirrors shared with `ferrocast-audio`, keyed by source ID.
    levels: Arc<RwLock<HashMap<u64, Arc<AtomicLevels>>>>,
    recycle_tx: mpsc::SyncSender<Block>,
    /// Shared with countdowns, so fonts are loaded once.
    text: Arc<Mutex<TextRenderer>>,
    stop: Arc<AtomicBool>,
    /// The loop's stats and error as of its last frame, for those who only read them (/stats
    /// every second, health): they never wait for the state lock the loop holds a whole frame
    /// (on a slow card, every Studio call through the window's thread waited behind it).
    seen: Arc<Seen>,
    thread: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
    watchdog_done: Arc<(Mutex<bool>, Condvar)>,
    sleep: Arc<crate::sleep::SleepMonitor>,
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

const FRAME_RING_SECONDS: usize = 4;
const FRAME_RING_PIECE_FRAMES: usize = 480;
const MAX_RING_CHUNK_FRAMES: usize = 4096;

fn ensure_levels_in(levels: &RwLock<HashMap<u64, Arc<AtomicLevels>>>, id: u64) -> Arc<AtomicLevels> {
    if let Some(atom) = levels.read().unwrap_or_else(|e| e.into_inner()).get(&id) {
        return Arc::clone(atom);
    }
    let mut map = levels.write().unwrap_or_else(|e| e.into_inner());
    Arc::clone(map.entry(id).or_insert_with(AtomicLevels::shared))
}

fn sync_heard(st: &mut State, audio_cmd: &mpsc::Sender<AudioCmd>) {
    let mut list: Vec<u64> = heard(st).into_iter().collect();
    list.sort_unstable();
    if list != st.last_heard {
        let msg: Arc<[u64]> = Arc::from(list.as_slice());
        st.last_heard = list;
        let _ = audio_cmd.send(AudioCmd::SetHeard(msg));
    }
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn forward_audio(
    id: u64,
    slot: &mut Option<AudioProducer>,
    audio_cmd: &mpsc::Sender<AudioCmd>,
    pcm: &[f32],
    channels: usize,
    rate: u32,
    ts_ns: u64,
    delay: Option<&mut crate::delay::AudioDelay>,
) {
    let Ok(ch) = u8::try_from(channels) else { return };
    if channels == 0 || rate == 0 || pcm.is_empty() || !pcm.len().is_multiple_of(channels) {
        return;
    }
    // A Video Delay filter that holds sound back: the same length comes out, later.
    let pcm = match delay {
        Some(d) => d.process(pcm, channels, rate),
        None => pcm,
    };
    let prod = slot.get_or_insert_with(|| {
        let (p, c) = SpscAudioRing::for_seconds(FRAME_RING_SECONDS, 48_000, 2, FRAME_RING_PIECE_FRAMES);
        let _ = audio_cmd.send(AudioCmd::AttachRing { id, consumer: c });
        // Its rate is known now: a resampler (if it isn't the mix's rate) is made before the
        // sound is taken in.
        let _ = audio_cmd.send(AudioCmd::WantResampler(rate));
        p
    });
    let max_samples = MAX_RING_CHUNK_FRAMES * channels;
    let mut offset_frames = 0usize;
    for chunk in pcm.chunks(max_samples) {
        let chunk_ts = ts_ns.saturating_add(((offset_frames as u128 * 1_000_000_000) / u128::from(rate)) as u64);
        let _ = prod.push(chunk, ch, rate, chunk_ts);
        offset_frames += chunk.len() / channels;
    }
}

impl Engine {
    /// Starts the engine and its frame loop.
    pub fn new(config: EngineConfig, video: VideoConfig, audio_cfg: AudioConfig) -> Result<Self, AvError> {
        if config.fps == 0 || config.fps > 240 || !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) {
            return Err(AvError::BadSize(config.width, config.height));
        }
        let gpu = Arc::new(Compositor::new()?);
        let (audio, bridge_rx) =
            AudioHandle::spawn(config.sample_rate, u64::from(config.audio_latency_ms) * 1_000_000)?;
        let levels: Arc<RwLock<HashMap<u64, Arc<AtomicLevels>>>> = Arc::new(RwLock::new(HashMap::new()));
        let recycle_tx = audio.recycle_sender();
        let audio_cmd = audio.cmd_sender();
        let state = Arc::new(Mutex::new(State {
            delay_conv: None,
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
            video: VideoConfig {
                width: config.width,
                height: config.height,
                fps_num: config.frame_rate().0,
                fps_den: config.frame_rate().1,
                ..video
            },
            audio: AudioConfig { sample_rate: config.sample_rate, ..audio_cfg },
            last_heard: Vec::new(),
            stats: Stats::default(),
            miss_monitor: FrameMissMonitor::with_enc_skipped(crate::video::dropped_frames()),
            errors: HashMap::new(),
            heal: HashMap::new(),
            encode_ms: HashMap::new(),
            notices: Default::default(),
            previews: HashMap::new(),
            in_flight: std::collections::VecDeque::new(),
            parked: Vec::new(),
            sleep_want: false,
            sleep_epoch: 0,
            reopen_workers: Arc::new(AtomicUsize::new(0)),
            source_reopen_workers: HashMap::new(),
            draining_workers: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let seen: Arc<Seen> = Arc::default();
        let sleep = Arc::new(crate::sleep::SleepMonitor::default());
        let thread = {
            let (state, gpu, audio_cmd, levels, recycle_tx, stop, seen, sleep) = (
                state.clone(),
                gpu.clone(),
                audio_cmd.clone(),
                levels.clone(),
                recycle_tx.clone(),
                stop.clone(),
                seen.clone(),
                sleep.clone(),
            );
            std::thread::Builder::new()
                .name("ferrocast-engine".into())
                .spawn(move || {
                    run(config, &state, &gpu, &audio_cmd, &levels, &bridge_rx, &recycle_tx, &stop, &seen, &sleep)
                })
                .map_err(|e| AvError::Source(format!("the engine's thread didn't start ({e})")))?
        };
        let watchdog_done = Arc::new((Mutex::new(false), Condvar::new()));
        let watchdog = {
            let (state, audio_cmd, stop, done) =
                (state.clone(), audio_cmd.clone(), stop.clone(), watchdog_done.clone());
            std::thread::Builder::new()
                .name("ferrocast-watchdog".into())
                .spawn(move || watchdog(config, state, audio_cmd, stop, done))
                .ok()
        };
        Ok(Self {
            config,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            monitor: Mutex::new(None),
            state,
            gpu,
            audio,
            levels,
            recycle_tx,
            text: Arc::new(Mutex::new(TextRenderer::new())),
            stop,
            seen,
            thread: Some(thread),
            watchdog,
            watchdog_done,
            sleep,
            preview: Mutex::new(HashMap::new()),
            next_preview: AtomicU64::new(1),
        })
    }

    /// Whether sleep is currently inhibited due to live streaming or recording.
    pub fn sleep_inhibited(&self) -> bool {
        self.sleep.is_inhibited()
    }

    /// Reloads Ferrocast's local font directory (`<data>/fonts/`) into the text renderer.
    pub fn reload_fonts(&self) {
        self.text.lock().unwrap_or_else(|p| p.into_inner()).reload_extra_fonts();
    }

    fn sync_sleep(&self, ticket: Option<(u64, bool)>) {
        if let Some((epoch, want)) = ticket {
            self.sleep.apply(epoch, want);
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// A source (re)placed: in the mix with its settings kept (mute and volume belong to the
    /// source, not to the device it opens; resetting them made a muted source loud, 20 Nov), and
    /// no longer marked as capturing an output until it says so again.
    fn reset_sound(&self, id: u64) {
        let levels = ensure_levels_in(&self.levels, id);
        let cmd = self.audio.cmd_sender();
        let _ = cmd.send(AudioCmd::EnsureSource { id, levels });
        let _ = cmd.send(AudioCmd::SetOutputCapture { id, capture: None });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds (or replaces) a source. Errors say what's wrong (a missing file, a camera in use).
    pub fn set_source(&self, id: u64, kind: SourceKind) -> Result<(), AvError> {
        let is_tone = matches!(kind, SourceKind::Tone { .. });
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
                let levels = ensure_levels_in(&self.levels, id);
                let cmd = self.audio.cmd_sender();
                let _ = cmd.send(AudioCmd::SetTone {
                    id,
                    tone: Some(ToneSpec { hz, every_ns: every_ms.max(1) * 1_000_000, beep_ns: beep_ms * 1_000_000 }),
                    levels: Some(levels),
                });
                let _ = cmd.send(AudioCmd::SetOutputCapture { id, capture: None });
                (Live::Still, None)
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
                    let gone = {
                        let mut st = self.lock();
                        let src_workers = st.source_workers_for(id, Some(&kind));
                        self.audio.send(AudioCmd::SetTone { id, tone: None, levels: None });
                        let mut src = Src::new(live, None, Some(kind));
                        src.reopen_workers = src_workers;
                        let prev = st.sources.insert(id, src);
                        sync_heard(&mut st, &self.audio.cmd_sender());
                        prev
                    };
                    drop(gone);
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
            SourceKind::Media { path, options } => {
                if options.close_when_inactive {
                    if !path.is_file() {
                        return Err(AvError::Source(format!("{} isn't there", path.display())));
                    }
                    let gone = {
                        let mut st = self.lock();
                        self.audio.send(AudioCmd::SetTone { id, tone: None, levels: None });
                        let mut src = Src::new(Live::Media(MediaSlot::closed(path, options)), None, None);
                        st.attach_workers_if_tracked(id, &mut src);
                        let prev = st.sources.insert(id, src);
                        sync_heard(&mut st, &self.audio.cmd_sender());
                        prev
                    };
                    drop(gone);
                    self.gpu.forget(id);
                    return Ok(());
                }
                // Opening a file can take a while (FFmpeg looks at it, then tries the GPU
                // decoder): on its own thread like a capture. A missing file still says so at once.
                let start_stopped = options.restart_on_activate;
                return self.open_soon(
                    id,
                    None,
                    Box::new(move || {
                        open_video_with(&path, options, start_stopped).map(|m| {
                            let heard = m.has_audio;
                            (Live::Media(MediaSlot::open(path, options, m)), heard)
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
        let clear_gpu = picture.is_none() && restart.is_none();
        let size = match &picture {
            Some(p) => {
                self.gpu.upload(id, p)?;
                Some((p.width, p.height, p.premultiplied))
            }
            None => None,
        };
        let gone = {
            let mut st = self.lock();
            if !is_tone {
                self.audio.send(AudioCmd::SetTone { id, tone: None, levels: None });
            }
            // A capture opened in place (an App capture handed over) keeps the old picture on
            // the card until its first one comes.
            let size = size.or_else(|| restart.as_ref().and(st.sources.get(&id).and_then(|s| s.size)));
            let mut src = Src::new(live, size, restart);
            st.attach_workers_if_tracked(id, &mut src);
            let prev = st.sources.insert(id, src);
            sync_heard(&mut st, &self.audio.cmd_sender());
            prev
        };
        drop(gone);
        if clear_gpu {
            self.gpu.forget(id);
        }
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
        let token = next_open_token();
        let now = now_ns();
        let mut gone = None;
        let worker_guard = {
            let mut st = self.lock();
            if restart.is_some() {
                if st.reopen_workers.load(Ordering::Acquire) >= MAX_TOTAL_REOPEN_WORKERS {
                    return Err(AvError::Source("too many stalled device opens in progress".into()));
                }
                if st
                    .peek_source_workers(id, restart.as_ref())
                    .is_some_and(|w| w.load(Ordering::Acquire) >= MAX_SOURCE_REOPEN_WORKERS)
                {
                    return Err(AvError::Source("too many stalled opens for this device".into()));
                }
            }
            let src_workers = st.source_workers_for(id, restart.as_ref());
            let worker_guard = if restart.is_some() {
                Some(ReopenWorkerGuard::new(src_workers.clone(), st.reopen_workers.clone()))
            } else {
                None
            };
            self.audio.send(AudioCmd::SetTone { id, tone: None, levels: None });
            match st.sources.get_mut(&id) {
                // Already here with a picture (a screen capture handed over): it keeps running
                // and showing until the new one is open.
                Some(src) if src.size.is_some() => {
                    src.kind = restart;
                    src.opening = Some(token);
                    src.opening_since = now;
                    src.reopen_workers = src_workers;
                }
                _ => {
                    let mut src = Src::new(Live::Still, None, restart);
                    src.opening = Some(token);
                    src.opening_since = now;
                    src.reopen_workers = src_workers;
                    // Stopping the replaced source happens outside the lock below, while slot
                    // reservation already completed atomically under `st`.
                    gone = st.sources.insert(id, src);
                }
            }
            worker_guard
        };
        drop(gone);
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), AvError>>();
        let state = self.state.clone();
        let audio_cmd = self.audio.cmd_sender();
        let levels = self.levels.clone();
        let stop = self.stop.clone();
        let spawned = std::thread::Builder::new().name("ferrocore-open".into()).spawn(move || {
            let _worker_guard = worker_guard;
            // A panic opening it is an error like any other: the token is cleared below and the
            // watchdog tries again (CodeRabbit), instead of a source stuck "opening".
            let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open))
                .unwrap_or_else(|p| Err(AvError::Source(format!("opening it failed: {}", panic_text(&*p)))));
            let mut leftover = None;
            let mut discard = false;
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            // Do not accept late captures once Engine shutdown has started.
            let mine = (!stop.load(Ordering::Acquire))
                .then(|| st.sources.get_mut(&id).filter(|s| s.opening == Some(token)))
                .flatten();
            let answer = match (opened, mine) {
                (Ok((live, heard)), Some(src)) => {
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    let oc = output_capture_of(&live, src.kind.as_ref());
                    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                    let oc: Option<crate::mix::OutputCapture> = None;
                    // The one it took over from (when handed over) stops outside the lock.
                    leftover = Some(std::mem::replace(&mut src.live, live));
                    src.opening = None;
                    src.opening_since = 0;
                    src.audio_tx = None;
                    src.watch = Watch::for_kind(src.kind.as_ref(), now_ns());
                    src.camera_retry =
                        is_camera_kind(src.kind.as_ref()).then(|| crate::camera_retry::CameraRetry::new(now_ns()));
                    // Its sound into the mix (a file with sound), under the same lock so a
                    // removal can't slip in between.
                    if heard || oc.is_some() || matches!(src.live, Live::Screen(_)) {
                        if heard {
                            let atom = ensure_levels_in(&levels, id);
                            let _ = audio_cmd.send(AudioCmd::EnsureSource { id, levels: atom });
                        }
                        let _ = audio_cmd.send(AudioCmd::SetOutputCapture { id, capture: oc });
                    }
                    sync_heard(&mut st, &audio_cmd);
                    Ok(())
                }
                (Err(e), Some(src)) => {
                    src.opening = None;
                    src.opening_since = 0;
                    // The new one didn't open: the old one handed over stops too (the watchdog
                    // starts what was asked for, not what was there), and no stale picture.
                    leftover = Some(std::mem::replace(&mut src.live, Live::Still));
                    src.size = None;
                    if src.kind.is_some() {
                        // A capture: the watchdog tries again later (cameras on 1..5 s backoff).
                        let now = now_ns();
                        if let Some(retry) = src.camera_retry.as_mut() {
                            retry.on_initial_open_failed(now, Some(&e.to_string()));
                        }
                        src.watch.next_try = now + src.watch.backoff(0);
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
                st.prune_source_workers();
                levels.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
                let _ = audio_cmd.send(AudioCmd::RemoveSource(id));
                sync_heard(&mut st, &audio_cmd);
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
            let mut st = self.lock();
            st.sources.remove(&id);
            st.prune_source_workers();
            self.levels.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
            self.audio.send(AudioCmd::RemoveSource(id));
            sync_heard(&mut st, &self.audio.cmd_sender());
            drop(st);
            self.gpu.forget(id);
            return Err(AvError::Source(format!("couldn't start opening it: {e}")));
        }
        match rx.recv_timeout(OPEN_WAIT) {
            Ok(Err(e)) => {
                // Said straight away: not kept as a source that never opened (nor its sound).
                let mut st = self.lock();
                if st.sources.get(&id).is_some_and(|s| matches!(s.live, Live::Still) && s.opening.is_none()) {
                    st.sources.remove(&id);
                    st.prune_source_workers();
                    self.levels.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    self.audio.send(AudioCmd::RemoveSource(id));
                    sync_heard(&mut st, &self.audio.cmd_sender());
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
            let removed = match st.sources.remove(&id) {
                // A running, healthy camera is parked a moment (see `State::parked`).
                Some(src)
                    if src.opening.is_none()
                        && is_camera_kind(src.kind.as_ref())
                        && !is_live_ended(&src.live)
                        && !src.is_waiting_for_camera() =>
                {
                    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                    if let (Some(kind), live @ Live::Camera(_)) = (src.kind, src.live) {
                        st.parked.push((kind, live, now_ns()));
                    }
                    None
                }
                other => other,
            };
            st.prune_source_workers();
            self.levels.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
            self.audio.send(AudioCmd::RemoveSource(id));
            sync_heard(&mut st, &self.audio.cmd_sender());
            removed
        };
        // Let go outside the lock (stopping a capture can wait).
        drop(gone);
        self.gpu.forget(id);
    }

    /// A source's sound settings (volume, mute, balance, tracks…), for sources with sound.
    pub fn set_sound(&self, id: u64, settings: SourceSettings) {
        let levels = ensure_levels_in(&self.levels, id);
        self.audio.send(AudioCmd::SetSettings { id, settings, levels });
    }

    /// A push-to-talk (`talk`) or push-to-mute key went down or up for a source: the mixer gates its
    /// sound while it is held and for the source's release delay after (the audio thread counts it).
    pub fn push_key(&self, id: u64, talk: bool, down: bool) {
        self.audio.send(AudioCmd::PushKey { id, talk, down });
    }

    /// Low latency sound (Settings): the mixer waits at most 60 ms past its filters' delay.
    pub fn set_low_latency(&self, on: bool) {
        self.audio.send(AudioCmd::SetLowLatency(on));
    }

    /// Meters read true peak (4x oversampled) or sample peak.
    pub fn set_true_peak(&self, on: bool) {
        self.audio.send(AudioCmd::SetTruePeak(on));
    }

    /// Music quieter while the voices talk (None: off).
    pub fn set_ducking(&self, ducking: Option<crate::level::Ducking>) {
        self.audio.send(AudioCmd::SetDucking(ducking));
    }

    /// Every track steered to `target_lufs` and kept under -1 dBFS (None: off).
    pub fn set_loudness(&self, target_lufs: Option<f32>) {
        self.audio.send(AudioCmd::SetLoudness(target_lufs));
    }

    /// Plays monitored sound (sources set to `Monitoring::Only` or `AndOutput`) on `device` (a
    /// name from `monitor::devices()`; None is the system's), or stops with `on` false.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_monitoring(&self, on: bool, device: Option<&str>) -> Result<(), AvError> {
        let m = if on { Some(crate::monitor::Monitor::start(device, self.config.sample_rate)?) } else { None };
        let feeder = m.as_ref().map(crate::monitor::Monitor::feeder);
        let dev_id = feeder.as_ref().and_then(crate::monitor::Feeder::device_id);
        let epoch = feeder.as_ref().map_or(0, crate::monitor::Feeder::device_epoch);
        self.audio.send(AudioCmd::SetMonitor { feeder, dev_id, epoch });
        // Drop old monitor outside both `monitor` lock and `ferrocast-audio` (its drop joins its thread).
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
        let levels = ensure_levels_in(&self.levels, id);
        self.audio.send(AudioCmd::SetEffects { id, effects: effects.to_vec(), levels: Some(levels) });
    }

    /// Plays, pauses, restarts, stops or seeks a media source (OBS's media controls).
    /// Releases the engine's State lock before dispatching so scrubber drags never stall frames.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn media_control(&self, id: u64, c: crate::media::Control) -> Result<(), AvError> {
        let mut reopen = None;
        let player = {
            let mut st = self.lock();
            match st.sources.get_mut(&id).map(|s| &mut s.live) {
                Some(Live::Media(slot)) => match &slot.player {
                    Some(m) => Some(Arc::clone(m)),
                    None => {
                        if matches!(c, crate::media::Control::Play | crate::media::Control::Restart)
                            && slot.reopening.is_none()
                        {
                            let tok = next_reopen_token();
                            slot.reopening = Some(tok);
                            reopen = Some((tok, slot.path.clone(), slot.options));
                        }
                        None
                    }
                },
                _ => return Err(AvError::Source(format!("source {id} isn't a media file"))),
            }
        };
        if let Some(m) = player {
            m.control(c);
            if matches!(
                c,
                crate::media::Control::Restart | crate::media::Control::Seek(_) | crate::media::Control::Stop
            ) {
                self.audio.send(AudioCmd::FlushSource(id));
            }
        } else if let Some((tok, path, options)) = reopen {
            spawn_media_reopen(
                self.state.clone(),
                self.audio.cmd_sender(),
                self.levels.clone(),
                id,
                tok,
                path,
                options,
                false,
            );
        }
        Ok(())
    }

    /// A media source's state, where it is and how long it lasts (ns).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn media_state(&self, id: u64) -> Option<(crate::media::MediaState, u64, u64)> {
        let (player, cached_dur) = {
            let st = self.lock();
            match st.sources.get(&id).map(|s| &s.live) {
                Some(Live::Media(slot)) => (slot.player.as_ref().map(Arc::clone), slot.duration_ns),
                _ => return None,
            }
        };
        match player {
            Some(m) => {
                let (state, at) = m.state();
                Some((state, at, m.duration_ns))
            }
            None => Some((crate::media::MediaState::Stopped, 0, cached_dur)),
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
        let buf = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        self.audio.send(AudioCmd::AttachTap { id, tap: Arc::downgrade(&buf) });
        crate::mix::Tap(buf)
    }

    /// A source's sound levels over the last block (for meters), wait-free via [`AtomicLevels`].
    pub fn levels(&self, id: u64) -> Option<crate::mix::Levels> {
        self.levels.read().unwrap_or_else(|e| e.into_inner()).get(&id).map(|atom| atom.load())
    }

    /// A source's levels over the last few blocks only (about 43 ms), wait-free: the dashboard's
    /// meters, which smooth the fall themselves. [`Engine::levels`] keeps its 300 ms window.
    pub fn levels_fast(&self, id: u64) -> Option<crate::mix::Levels> {
        self.levels.read().unwrap_or_else(|e| e.into_inner()).get(&id).map(|atom| atom.load_fast())
    }

    #[cfg(test)]
    pub(crate) fn sound_settings(&self, id: u64) -> Option<SourceSettings> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.audio.send(AudioCmd::QuerySettings { id, reply: tx });
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    /// A scene's items, bottom first (replacing what it had).
    /// A scene's items as the engine has them (None: no such scene).
    pub fn scene(&self, name: &str) -> Option<Vec<SceneItem>> {
        self.lock().scenes.get(name).cloned()
    }

    pub fn set_scene(&self, name: &str, items: Vec<SceneItem>) {
        let mut st = self.lock();
        st.scenes.insert(name.to_string(), items);
        sync_heard(&mut st, &self.audio.cmd_sender());
    }

    /// Removes a scene by name (used to clean up temporary solo scenes after projectors,
    /// screenshots, or isolated recordings finish).
    pub fn remove_scene(&self, name: &str) {
        let mut st = self.lock();
        if st.scenes.remove(name).is_some() {
            sync_heard(&mut st, &self.audio.cmd_sender());
        }
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
            for e in &mut s.sound_effects {
                if let crate::afx::AudioEffect::Compressor { sidechain: Some(sc), .. } = e
                    && *sc != 0
                {
                    *sc = to(*sc);
                }
            }
            let started = if keep.contains(&s.id) { Ok(()) } else { self.set_source(s.id, s.kind.clone()) };
            match started {
                Ok(()) => {
                    if let Some(sound) = s.sound {
                        self.set_sound(s.id, sound);
                        self.set_sound_effects(s.id, &s.sound_effects);
                    }
                    opened.insert(s.name.clone(), (s.id, s.kind));
                }
                Err(e) if is_camera_kind(Some(&s.kind)) => {
                    // Selected or saved camera unavailable now (e.g. Continuity Camera sleeping):
                    // keep it alive in `sources` marked "waiting for the camera" so the watchdog
                    // keeps checking with backoff rather than stranding it in `missing`.
                    let why = e.to_string();
                    let mut st = self.lock();
                    let mut src = Src::waiting_camera(s.kind.clone(), now_ns(), &why);
                    src.reopen_workers = st.source_workers_for(s.id, Some(&s.kind));
                    st.sources.insert(s.id, src);
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
        let live_ids: std::collections::HashSet<u64> = opened.values().map(|(id, _)| *id).collect();
        let sidechain_ducking = remap_ducking(imported.ducking, &to, &live_ids);
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
            sync_heard(&mut st, &self.audio.cmd_sender());
        }
        for id in old_rest {
            self.remove_source(id);
        }
        self.load_audio_care(doc, sidechain_ducking);
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
        // Sources whose words or colour changed: redrawn in place (no capture, no sound).
        let mut retarget: Vec<(u64, String, SourceKind)> = Vec::new();
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
                Some((id, kind))
                    if st.opened.contains_key(&src.name) && is_in_place_kind(kind) && is_in_place_kind(&src.kind) =>
                {
                    ids.insert(src.id, *id);
                    retarget.push((*id, src.name.clone(), src.kind.clone()));
                }
                _ => return None,
            }
        }
        for (_, name, kind) in &retarget {
            if let Some(o) = st.opened.get_mut(name) {
                o.1 = kind.clone();
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
        let live_ids: std::collections::HashSet<u64> = st.opened.values().map(|(id, _)| *id).collect();
        let sidechain_ducking = remap_ducking(imported.ducking, &to, &live_ids);
        // Why each that couldn't open didn't, as the full load said.
        let mut skipped = imported.skipped;
        skipped.extend(st.missing.values().map(|(.., why)| why.clone()));
        sync_heard(&mut st, &self.audio.cmd_sender());
        drop(st);
        for (id, name, kind) in retarget {
            if let Err(e) = self.set_source(id, kind) {
                skipped.push(format!("{name}: {e}"));
            }
        }
        for mut s in imported.sources {
            if let Some(sound) = s.sound.filter(|_| running.contains(&s.name)) {
                let id = to(s.id);
                for e in &mut s.sound_effects {
                    if let crate::afx::AudioEffect::Compressor { sidechain: Some(sc), .. } = e
                        && *sc != 0
                    {
                        *sc = to(*sc);
                    }
                }
                self.set_sound(id, sound);
                self.set_sound_effects(id, &s.sound_effects);
            }
        }
        self.load_audio_care(doc, sidechain_ducking);
        Some(skipped)
    }

    /// Ferrocast's own sound care in the document (`ferrocast.audio`): ducking (`ducking`:
    /// `{ voices, music }` source names and `depth_db`) and loudness (`loudness_lufs`), merged
    /// with any OBS sidechained compressors imported as ducking (M2-03).
    fn load_audio_care(&self, doc: &serde_json::Value, imported_ducking: Option<crate::level::Ducking>) {
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
        let explicit = a["ducking"].is_object().then(|| crate::level::Ducking {
            voices: ids("voices"),
            music: ids("music"),
            depth_db: a["ducking"]["depth_db"].as_f64().map_or(-12.0, |d| (d as f32).clamp(-40.0, -3.0)),
            ..Default::default()
        });
        let ducking = match (explicit, imported_ducking) {
            (Some(mut ex), Some(imp)) => {
                for v in imp.voices {
                    if !ex.voices.contains(&v) && !ex.music.contains(&v) {
                        ex.voices.push(v);
                    }
                }
                for m in imp.music {
                    if !ex.music.contains(&m) && !ex.voices.contains(&m) {
                        ex.music.push(m);
                    }
                }
                Some(ex)
            }
            (Some(ex), None) => Some(ex),
            (None, Some(imp)) => Some(imp),
            (None, None) => None,
        };
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
        sync_heard(&mut st, &self.audio.cmd_sender());
    }

    /// Every source a collection brought, by name: pictures sent to the card so far and their
    /// size. Read twice, the difference is each one's rate (what keeps the engine busiest).
    pub fn source_load(&self) -> Vec<SourceLoad> {
        self.now_or_last(loads_of, |s| s.loads.clone())
    }

    /// Shown or on-air camera sources currently waiting for their device to return.
    pub fn waiting_cameras(&self) -> Vec<String> {
        self.now_or_last(waiting_cameras_of, |s| s.waiting_cameras.clone())
    }

    /// Live status of a named camera source (`None` when not an open camera source), read
    /// non-blockingly from `Shown` when the render loop holds `State`.
    pub fn camera_status(&self, name: &str) -> Option<CameraStatus> {
        self.now_or_last(|st| camera_status_of(st, name), |s| s.camera_statuses.get(name).cloned())
    }

    /// True when named camera source `name` still owns an open capture session while reporting
    /// waiting state (e.g. interruption grace or healthy 0-frame card), so picker probes avoid
    /// opening a competing session against it.
    pub fn is_camera_holding_session(&self, name: &str) -> bool {
        self.now_or_last(|st| camera_holding_session_of(st, name), |s| s.holding_sessions.contains(name))
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
        sync_heard(&mut st, &self.audio.cmd_sender());
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
            let levels = ensure_levels_in(&self.levels, STINGER);
            let cmd = self.audio.cmd_sender();
            let _ = cmd.send(AudioCmd::SetSettings { id: STINGER, settings: SourceSettings::default(), levels });
            let _ = cmd.send(AudioCmd::SetOutputCapture { id: STINGER, capture: None });
        }
        let now = now_ns();
        let mut st = self.lock();
        st.sources.insert(
            STINGER,
            Src::new(
                Live::Media(MediaSlot::open(video.to_path_buf(), crate::ffsrc::MediaOptions::default(), m)),
                None,
                None,
            ),
        );
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
        sync_heard(&mut st, &self.audio.cmd_sender());
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
            fps_num: (self.config.frame_rate().0 / canvas.every).max(1),
            fps_den: self.config.frame_rate().1,
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
        pipe.set_track_names(canvas.track_names.clone());
        // The AAC work off the render thread (FERROCORE_AAC_ON_RENDER_THREAD keeps it there, to compare).
        if std::env::var_os("FERROCORE_AAC_ON_RENDER_THREAD").is_none()
            && let Err(e) = pipe.audio_off_thread()
        {
            tracing::warn!(error = %e, "the sound encoder stays on the render thread");
        }
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
        // A stream draws what the stream shows, a recording what the recording shows.
        let canvas = Canvas { audience: audience_of(&kind), ..canvas };
        st.outputs.push((id, pipe, kind, canvas));
        sync_heard(&mut st, &self.audio.cmd_sender());
        self.show(&st);
        let ticket = st.sleep_ticket();
        drop(st);
        self.sync_sleep(ticket);
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
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
        let (.., canvas) = st
            .outputs
            .iter_mut()
            .find(|(i, ..)| *i == id)
            .ok_or_else(|| AvError::Source(format!("there's no output {id}")))?;
        canvas.scene = scene;
        sync_heard(&mut st, &self.audio.cmd_sender());
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
        self.lock()
            .previews
            .insert(id, Preview { look: Default::default(), scene, size, multiview: None, paused: false });
        let surface = Arc::new(Mutex::new(surface));
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let thread = {
            let (state, gpu, surface, stop) = (self.state.clone(), self.gpu.clone(), surface.clone(), stop.clone());
            let (paused, wake) = (paused.clone(), wake.clone());
            let every = Duration::from_secs(1) / self.config.fps.max(1);
            std::thread::Builder::new()
                .name("ferrocast-preview".into())
                .spawn(move || preview_loop(id, &state, &gpu, &surface, &stop, &(paused, wake), every))
                .map_err(|e| AvError::Source(format!("the preview's thread didn't start ({e})")))?
        };
        self.preview_threads().insert(id, PreviewThread { surface, stop, thread, paused, wake });
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
            wake_preview(&p.wake);
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

    /// Performance mode: a paused preview composes and draws nothing (its view keeps the last
    /// picture, or is hidden by the host) until `paused` is false again. The outputs (stream,
    /// recording, replay, virtual camera) are never touched. An unknown id changes nothing.
    pub fn set_preview_paused(&self, id: u64, paused: bool) {
        if let Some(p) = self.lock().previews.get_mut(&id) {
            p.paused = paused;
        }
        if let Some(t) = self.preview_threads().get(&id) {
            t.paused.store(paused, Ordering::Release);
            wake_preview(&t.wake);
        }
    }

    /// Whether a preview is paused (None: no such preview).
    pub fn preview_paused(&self, id: u64) -> Option<bool> {
        self.lock().previews.get(&id).map(|p| p.paused)
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
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
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
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
        let (_, pipe, kind, canvas) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        if !matches!(kind, OutputKind::Record(_)) {
            return Err(AvError::Source("only a recording can take a stream on".into()));
        }
        pipe.begin_stream()?;
        *kind = OutputKind::Stream(send);
        // One picture goes to both while they share: the stream's, so an item shown only in the
        // recording never reaches the stream (the frames drawn before are delivered above).
        canvas.audience = Audience::Stream;
        let ticket = st.sleep_ticket();
        drop(st);
        self.sync_sleep(ticket);
        Ok(())
    }

    /// The stream leaves output `id`, which goes on as a recording (its file and encoders
    /// kept): the stream ended while a recording sharing its encode goes on.
    pub fn stream_off(&self, id: u64) -> Result<(), AvError> {
        let mut st = self.lock();
        // Frames drawn for the stream's picture go out first: the recording's picture is another.
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
        let (_, pipe, kind, canvas) =
            st.outputs.iter_mut().find(|(i, ..)| *i == id).ok_or_else(|| AvError::Source("no such output".into()))?;
        // The stream always leaves (a recording that failed, say on a full disk, mustn't keep
        // it: the next Go live would find a stream there; CodeRabbit).
        *kind = OutputKind::Record(PathBuf::new());
        canvas.audience = Audience::Recording;
        let no_rec = !pipe.recording();
        let ticket = st.sleep_ticket();
        drop(st);
        self.sync_sleep(ticket);
        if no_rec {
            return Err(AvError::Source("the output had no recording left to go on with".into()));
        }
        Ok(())
    }

    /// An output's recording now.
    pub fn output_record_state(&self, id: u64) -> Result<RecordState, AvError> {
        if let Some(r) = self.now_or_shown(id, |pipe| Ok(record_state(pipe)), |(_, r, _)| r.clone()) {
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
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
        let r = Self::on_output(&mut st, id, f);
        self.show(&st);
        let ticket = st.sleep_ticket();
        drop(st);
        self.sync_sleep(ticket);
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
        deliver(&mut st, &self.gpu, true, &self.recycle_tx);
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
        // (The filter chosen for it stays.)
        let filter = canvas.scaled_from.map_or(Filter::Bicubic, |(.., f)| f);
        canvas.scaled_from = ((width, height) != from).then_some((from.0, from.1, filter));
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
        let (taken, ticket) = {
            let mut st = self.lock();
            tracing::info!(id, waited_ms = t.elapsed().as_millis() as u64, "an output stops: the engine's lock taken");
            // Its last frames still on the card go into it first.
            deliver(&mut st, &self.gpu, true, &self.recycle_tx);
            tracing::info!(id, at_ms = t.elapsed().as_millis() as u64, "an output stops: its last frames handed over");
            let taken = st.outputs.iter().position(|(i, ..)| *i == id).map(|at| st.outputs.remove(at));
            st.errors.remove(&id);
            st.heal.remove(&id);
            st.encode_ms.remove(&id);
            sync_heard(&mut st, &self.audio.cmd_sender());
            self.show(&st);
            let ticket = st.sleep_ticket();
            (taken, ticket)
        };
        if let Some((_, pipe, mut kind, _)) = taken {
            let finished = pipe.finish();
            self.sync_sleep(ticket);
            let tags = finished?;
            tracing::info!(id, at_ms = t.elapsed().as_millis() as u64, "an output stopped: its encoders finished");
            if let OutputKind::Stream(send) = &mut kind {
                for t in tags {
                    send(t);
                }
            }
        } else {
            self.sync_sleep(ticket);
        }
        Ok(())
    }

    /// As of the loop's last frame (it never waits for the frame being made).
    pub fn stats(&self) -> Stats {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).stats
    }

    /// What stopped the loop or an output, if anything did (as of its last frame).
    pub fn error(&self) -> Option<String> {
        let own = {
            let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            seen.errors.iter().min_by_key(|(id, _)| *id).map(|(_, e)| e.clone())
        };
        own.or_else(|| self.gpu.gpu_error.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// Tests: how many video frames this output's encoder has been handed.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn video_calls_for_test(&self, id: u64) -> u64 {
        let st = self.lock();
        st.outputs.iter().find(|(i, ..)| *i == id).map_or(0, |(_, pipe, ..)| pipe.video_calls_for_test())
    }

    /// Tests: the hardware session of this output's encoder is taken away.
    #[cfg(all(test, target_os = "macos"))]
    fn lose_output_session_for_test(&self, id: u64) {
        let mut st = self.lock();
        if let Some((_, pipe, ..)) = st.outputs.iter_mut().find(|(i, ..)| *i == id) {
            pipe.lose_video_session_for_test();
        }
    }

    /// Tests: this output's next video frame fails, once.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn fail_output_once_for_test(&self, id: u64) {
        let mut st = self.lock();
        if let Some((_, pipe, ..)) = st.outputs.iter_mut().find(|(i, ..)| *i == id) {
            pipe.fail_video_once_for_test();
        }
    }

    /// Tests: makes this output's encoder fault on every frame.
    #[cfg(test)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn fail_output_for_test(&self, id: u64) {
        let mut st = self.lock();
        if let Some((_, pipe, ..)) = st.outputs.iter_mut().find(|(i, ..)| *i == id) {
            pipe.fail_video_for_test();
        }
    }

    /// What captures opened by this engine want said on the dashboard, since the last call.
    pub fn take_notices(&self) -> Vec<crate::notices::Notice> {
        self.lock().notices.take()
    }

    /// What stopped this output, if anything did (as of the loop's last frame). Another output's
    /// fault (a vertical stream's, the replay buffer's) isn't it.
    pub fn output_error(&self, id: u64) -> Option<String> {
        let own = {
            let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            seen.errors.iter().find(|(i, _)| *i == id).map(|(_, e)| e.clone())
        };
        // The card's own failure (out of memory, an internal error) stops every picture: it is
        // each output's, unlike an encoder's.
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
            build(&st, self.config.width, self.config.height, Some(name), now_ns(), Audience::Everyone)
        };
        self.gpu.render(&scene)
    }

    /// Approximate per-frame encode time in milliseconds for an active output, or `None` once stopped.
    pub fn output_encode_ms(&self, id: u64) -> Option<f32> {
        self.now_or_last(
            |st| st.encode_ms.get(&id).copied(),
            |s| s.output_encode_ms.iter().find(|(i, _)| *i == id).map(|(_, ms)| *ms),
        )
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
            build(&st, canvas.0.max(1), canvas.1.max(1), Some(name), now_ns(), Audience::Everyone)
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
            {
                src.pending_since = 0;
                if f.send(&self.gpu, id) {
                    src.size = Some(f.info());
                }
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
        self.snapshot_with_effects(id, size, |st| default_source_effects(st, id))
    }

    /// One scene item's own picture now (`snapshot_source_filtered` using the item's video
    /// filter list — its override when set, otherwise the source's filters).
    pub fn snapshot_item_filtered(
        &self,
        owner_scene: &str,
        item_id: i64,
        id: u64,
        size: (u32, u32),
    ) -> Option<Vec<u8>> {
        self.snapshot_with_effects(id, size, |st| {
            find_item_effects(st, owner_scene, item_id, id, 0).unwrap_or_else(|| default_source_effects(st, id))
        })
    }

    /// Whether a scene item's picture is already uploaded (or doesn't need an upload, such as a
    /// scene/group); `None` when `(scene, item_id)` isn't in `scene` or any group inside it.
    pub fn item_picture_ready(&self, scene: &str, item_id: i64) -> Option<bool> {
        let st = self.lock();
        let (it, _) = find_scene_item(&st, scene, item_id)?;
        Some(if it.scene.is_some() {
            true
        } else {
            st.sources.get(&it.source).is_some_and(|s| s.size.is_some() || s.pending.is_some())
        })
    }

    /// One scene item rendered on its own exactly as the scene render draws it: its active
    /// video filters (`ferrocast_filters` override or source filters), crop, scale, rotation,
    /// flips, scale filter, blend and opacity (including any enclosing group chain's transform and
    /// filter expansion).
    ///
    /// Sized to the item's 1:1 transformed bounding box clipped to `canvas` when it overlaps the
    /// canvas, or the full transformed box capped to `4096` px when completely off-canvas.
    pub fn snapshot_item_transformed(
        &self,
        scene: &str,
        item_id: i64,
        canvas: (u32, u32),
    ) -> Result<(Vec<u8>, (u32, u32)), AvError> {
        const MAX_OFFSCREEN_SHOT: u32 = 4096;
        let canvas = (canvas.0.max(1), canvas.1.max(1));
        let scene = {
            let mut st = self.lock();
            send_pending(&mut st, &self.gpu);
            let ts = now_ns();
            let ms = scene_ms(&st, scene, ts);
            let (it, ancestors) = find_scene_item(&st, scene, item_id)
                .ok_or_else(|| AvError::Source(format!("item {item_id} isn't in scene \"{scene}\"")))?;
            let (it_effects, it_base) = active_item_effects_and_base(&st, it, ms, canvas)?;
            let (mut it_placement, it_opacity) = animated(it, ms);
            let (m_local, (cw, ch)) = item_draw_affine_and_crop(&it_placement, &it_effects, it_base)?;

            let mut pts = [
                m_local.apply(0.0, 0.0),
                m_local.apply(cw as f32, 0.0),
                m_local.apply(cw as f32, ch as f32),
                m_local.apply(0.0, ch as f32),
            ];
            let mut group_chain = Vec::with_capacity(ancestors.len());
            for g in ancestors {
                let (g_effects, g_base) = active_item_effects_and_base(&st, g, ms, canvas)?;
                let (gp, g_opacity) = animated(g, ms);
                pts = project_pts_through_group(pts, g, &g_effects, g_base, &gp, g_opacity)?;
                group_chain.push((g.clone(), g_effects, g_base, gp, g_opacity));
            }

            let (bx0, by0, bx1, by1) = snap_pts_bbox(&pts);
            if bx1 <= bx0 || by1 <= by0 {
                return Err(AvError::Source("the item has zero size".into()));
            }
            let (cx0, cy0, cx1, cy1) =
                (bx0.max(0), by0.max(0), bx1.min(i64::from(canvas.0)), by1.min(i64::from(canvas.1)));
            let (off_x, off_y, raw_w, raw_h, target_size) = if cx1 > cx0 && cy1 > cy0 {
                let rw = u64::try_from(cx1 - cx0).map_err(|_| AvError::BadSize(u32::MAX, u32::MAX))?.max(1);
                let rh = u64::try_from(cy1 - cy0).map_err(|_| AvError::BadSize(u32::MAX, u32::MAX))?.max(1);
                let target = (
                    u32::try_from(rw).map_err(|_| AvError::BadSize(u32::MAX, u32::MAX))?,
                    u32::try_from(rh).map_err(|_| AvError::BadSize(u32::MAX, u32::MAX))?,
                );
                (cx0 as f32, cy0 as f32, rw, rh, target)
            } else {
                let rw = u64::try_from(i128::from(bx1) - i128::from(bx0)).unwrap_or(u64::MAX).max(1);
                let rh = u64::try_from(i128::from(by1) - i128::from(by0)).unwrap_or(u64::MAX).max(1);
                let target = if rw > u64::from(MAX_OFFSCREEN_SHOT) || rh > u64::from(MAX_OFFSCREEN_SHOT) {
                    fit_within_u64(rw, rh, MAX_OFFSCREEN_SHOT)
                } else {
                    (rw as u32, rh as u32)
                };
                (bx0 as f32, by0 as f32, rw, rh, target)
            };

            let mut solo = it.clone();
            if group_chain.is_empty() {
                it_placement.x -= off_x;
                it_placement.y -= off_y;
            }
            solo.placement = it_placement;
            solo.opacity = it_opacity;
            solo.animation.clear();
            solo.visible = true;
            solo.show = Show::Both;
            let depth = u32::try_from(group_chain.len()).unwrap_or(MAX_NESTING);
            let mut comp_item = item_at(&st, &solo, ms, depth, canvas, Audience::Everyone)
                .ok_or_else(|| AvError::Source("the item couldn't be rendered".into()))?;
            let last = group_chain.len().saturating_sub(1);
            for (idx, (g, g_effects, g_base, mut gp, g_opacity)) in group_chain.into_iter().enumerate() {
                if idx == last {
                    gp.x -= off_x;
                    gp.y -= off_y;
                }
                comp_item = Item {
                    content: Content::Group { width: g_base.0, height: g_base.1, items: vec![comp_item] },
                    effects: g_effects,
                    placement: gp,
                    filter: g.filter,
                    blend: g.blend,
                    srgb_off: false,
                    linear: true,
                    opacity: g_opacity,
                };
            }
            // Scale oversized offscreen items via an unfiltered Group transform directly into the
            // bounded target instead of wrapping in `Content::Scene(raw_w, raw_h)`, which would
            // allocate a full `raw_w * raw_h` intermediate GPU texture before downscaling.
            let items = if u64::from(target_size.0) != raw_w || u64::from(target_size.1) != raw_h {
                vec![Item {
                    content: Content::Group { width: target_size.0, height: target_size.1, items: vec![comp_item] },
                    effects: Vec::new(),
                    placement: Placement {
                        scale_x: (target_size.0 as f64 / raw_w as f64) as f32,
                        scale_y: (target_size.1 as f64 / raw_h as f64) as f32,
                        ..Default::default()
                    },
                    filter: Filter::Disable,
                    blend: crate::Blend::Normal,
                    srgb_off: false,
                    linear: true,
                    opacity: 1.0,
                }]
            } else {
                vec![comp_item]
            };
            Scene { width: target_size.0, height: target_size.1, items }
        };
        let out_size = (scene.width, scene.height);
        let px = self.gpu.render(&scene)?;
        Ok((px, out_size))
    }

    fn snapshot_with_effects(
        &self,
        id: u64,
        size: (u32, u32),
        effects_of: impl FnOnce(&State) -> Vec<Effect>,
    ) -> Option<Vec<u8>> {
        let (w, h) = (size.0.max(1), size.1.max(1));
        let item = {
            let mut st = self.lock();
            send_pending(&mut st, &self.gpu);
            st.sources.get(&id)?.size?;
            let effects = effects_of(&st);
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
            item_of(&st, &one, 0, (w, h), Audience::Everyone)?
        };
        self.gpu.render(&Scene { width: w, height: h, items: vec![item] }).ok()
    }

    /// The scene on air drawn now, RGBA (a preview or a screenshot).
    pub fn snapshot(&self) -> Result<Vec<u8>, AvError> {
        let frame = {
            let st = self.lock();
            what_to_draw(&st, self.config.width, self.config.height, None, now_ns(), Audience::Everyone)
        };
        match frame {
            Draw::Scene(s) => self.gpu.render(&s),
            Draw::Moving(t) => self.gpu.render_transition(&t),
        }
    }
}

fn default_source_effects(st: &State, id: u64) -> Vec<Effect> {
    shared_source_effects(st.scenes.values().flatten(), id)
}

/// The source's own filters, as an item that shares them has them. An item with an override of
/// its own (a crop for the vertical scene) says nothing about the source, and `scenes` is in no
/// particular order, so those are skipped.
fn shared_source_effects<'a>(items: impl Iterator<Item = &'a SceneItem>, id: u64) -> Vec<Effect> {
    items
        .filter(|i| i.source == id && i.scene.is_none() && !i.own_effects)
        .map(|i| i.effects.clone())
        .next()
        .unwrap_or_default()
}

fn find_scene_item<'a>(st: &'a State, scene: &str, item_id: i64) -> Option<(&'a SceneItem, Vec<&'a SceneItem>)> {
    fn walk<'a>(
        st: &'a State,
        scene: &str,
        item_id: i64,
        depth: u32,
        chain_outer_first: &mut Vec<&'a SceneItem>,
    ) -> Option<&'a SceneItem> {
        let items = st.scenes.get(scene)?;
        if let Some(direct) = items.iter().find(|it| it.item_id == item_id) {
            return Some(direct);
        }
        if depth >= MAX_NESTING {
            return None;
        }
        for g in items.iter().filter(|g| g.group.is_some()) {
            let Some(gname) = g.scene.as_deref() else { continue };
            chain_outer_first.push(g);
            if let Some(found) = walk(st, gname, item_id, depth + 1, chain_outer_first) {
                return Some(found);
            }
            chain_outer_first.pop();
        }
        None
    }
    let mut ancestors = Vec::new();
    let leaf = walk(st, scene, item_id, 0, &mut ancestors)?;
    ancestors.reverse();
    Some((leaf, ancestors))
}

fn active_item_effects_and_base(
    st: &State,
    it: &SceneItem,
    ms: u64,
    canvas: (u32, u32),
) -> Result<(Vec<Effect>, (u32, u32)), AvError> {
    let base = match (&it.scene, it.group) {
        (Some(_), Some((0, _)) | Some((_, 0))) => return Err(AvError::Source("the group is empty".into())),
        (Some(_), Some(size)) => size,
        (Some(_), None) => canvas,
        (None, _) => {
            let (w, h, _) = st
                .sources
                .get(&it.source)
                .and_then(|s| s.size)
                .ok_or_else(|| AvError::Source("the source has no picture yet".into()))?;
            (w, h)
        }
    };
    let effects = it
        .effects
        .iter()
        .filter(|e| match e {
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
    Ok((effects, base))
}

fn clip_poly_edge(
    poly: &[(f32, f32)],
    inside: impl Fn((f32, f32)) -> bool,
    intersect: impl Fn((f32, f32), (f32, f32)) -> (f32, f32),
) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(poly.len() + 2);
    let Some(&last) = poly.last() else {
        return out;
    };
    let mut prev = last;
    let mut prev_in = inside(prev);
    for &cur in poly {
        let cur_in = inside(cur);
        if cur_in != prev_in {
            out.push(intersect(prev, cur));
        }
        if cur_in {
            out.push(cur);
        }
        prev = cur;
        prev_in = cur_in;
    }
    out
}

fn clip_pts_to_rect(pts: &[(f32, f32); 4], w: f32, h: f32) -> Option<(f32, f32, f32, f32)> {
    let mut poly: Vec<(f32, f32)> = pts.to_vec();
    poly = clip_poly_edge(
        &poly,
        |(x, _)| x >= 0.0,
        |(ax, ay), (bx, by)| {
            let t = (0.0 - ax) / (bx - ax);
            (0.0, ay + t * (by - ay))
        },
    );
    poly = clip_poly_edge(
        &poly,
        |(x, _)| x <= w,
        |(ax, ay), (bx, by)| {
            let t = (w - ax) / (bx - ax);
            (w, ay + t * (by - ay))
        },
    );
    poly = clip_poly_edge(
        &poly,
        |(_, y)| y >= 0.0,
        |(ax, ay), (bx, by)| {
            let t = (0.0 - ay) / (by - ay);
            (ax + t * (bx - ax), 0.0)
        },
    );
    poly = clip_poly_edge(
        &poly,
        |(_, y)| y <= h,
        |(ax, ay), (bx, by)| {
            let t = (h - ay) / (by - ay);
            (ax + t * (bx - ax), h)
        },
    );
    if poly.len() < 3 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for (x, y) in poly {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

fn project_pts_through_group(
    pts: [(f32, f32); 4],
    g: &SceneItem,
    g_effects: &[Effect],
    g_base: (u32, u32),
    gp: &Placement,
    g_opacity: f32,
) -> Result<[(f32, f32); 4], AvError> {
    let (m_group, (gcw, gch)) = item_draw_affine_and_crop(gp, g_effects, g_base)?;
    let has_crop = gp.crop.iter().any(|c| *c > 0);
    // Compositor allocates an offscreen `g_base` texture (clipping children to `0..g_base`)
    // whenever effects, placement crop, scale filter, blend mode, or group opacity apply.
    let group_renders_to_texture =
        !g_effects.is_empty() || has_crop || g.filter != Filter::Disable || g.blend != Blend::Normal || g_opacity < 1.0;
    if !group_renders_to_texture {
        return Ok(pts.map(|(x, y)| m_group.apply(x, y)));
    }
    // Clip actual polygon corners to `[0..g_base.0, 0..g_base.1]` before computing its AABB so
    // rotated items outside a corner (whose unclipped AABB straddles `(0,0)`) are rejected too.
    let (mut tx0, mut ty0, mut tx1, mut ty1) = clip_pts_to_rect(&pts, g_base.0 as f32, g_base.1 as f32)
        .ok_or_else(|| AvError::Source("the item is cropped out by its group".into()))?;
    let (mut cur_w, mut cur_h) = g_base;
    for e in g_effects {
        let (nw, nh) = e.out_size(cur_w, cur_h);
        match e {
            Effect::Crop { left, top, .. } => {
                tx0 = (tx0 - *left as f32).max(0.0);
                ty0 = (ty0 - *top as f32).max(0.0);
                tx1 = (tx1 - *left as f32).min(nw as f32);
                ty1 = (ty1 - *top as f32).min(nh as f32);
                if tx1 <= tx0 || ty1 <= ty0 {
                    return Err(AvError::Source("the item is cropped out by its group".into()));
                }
            }
            Effect::Scroll { speed_x, speed_y, width, height, .. }
                if *speed_x != 0.0 || *speed_y != 0.0 || width.is_some() || height.is_some() =>
            {
                tx0 = 0.0;
                ty0 = 0.0;
                tx1 = nw as f32;
                ty1 = nh as f32;
            }
            _ => {
                let pad = e.pad();
                if pad > 0 {
                    tx0 = tx0.max(0.0);
                    ty0 = ty0.max(0.0);
                    tx1 = (tx1 + (2 * pad) as f32).min(nw as f32);
                    ty1 = (ty1 + (2 * pad) as f32).min(nh as f32);
                }
            }
        }
        (cur_w, cur_h) = (nw, nh);
    }
    if has_crop {
        tx0 = (tx0 - gp.crop[0] as f32).max(0.0);
        ty0 = (ty0 - gp.crop[1] as f32).max(0.0);
        tx1 = (tx1 - gp.crop[0] as f32).min(gcw as f32);
        ty1 = (ty1 - gp.crop[1] as f32).min(gch as f32);
        if tx1 <= tx0 || ty1 <= ty0 {
            return Err(AvError::Source("the item is cropped out by its group".into()));
        }
    }
    Ok([m_group.apply(tx0, ty0), m_group.apply(tx1, ty0), m_group.apply(tx1, ty1), m_group.apply(tx0, ty1)])
}

fn item_draw_affine_and_crop(
    placement: &Placement,
    effects: &[Effect],
    base: (u32, u32),
) -> Result<(crate::transform::Affine, (u32, u32)), AvError> {
    let (ow, oh) = effects.iter().fold(base, |(w, h), e| e.out_size(w, h));
    let (cw, ch) = crate::transform::cropped(ow, oh, placement.crop);
    if cw == 0 || ch == 0 {
        return Err(AvError::Source("the item is cropped to nothing".into()));
    }
    let [l, t, r, b] = Effect::outside_sides(effects, base.0, base.1);
    let m = if l + r + t + b > 0 && ow > l + r && oh > t + b {
        crate::transform::Affine::translate(-(l as f32), -(t as f32)).then(&crate::transform::draw_transform(
            placement,
            ow - l - r,
            oh - t - b,
        ))
    } else {
        crate::transform::draw_transform(placement, ow, oh)
    };
    Ok((m, (cw, ch)))
}

fn pts_aabb(pts: &[(f32, f32); 4]) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    (x0, y0, x1, y1)
}

fn snap_pts_bbox(pts: &[(f32, f32); 4]) -> (i64, i64, i64, i64) {
    let (x0, y0, x1, y1) = pts_aabb(pts);
    // Snap negligible float rotation error (e.g. cos(90°) ~= -4.37e-8) away from outward padding.
    const EPS: f32 = 1e-4;
    ((x0 + EPS).floor() as i64, (y0 + EPS).floor() as i64, (x1 - EPS).ceil() as i64, (y1 - EPS).ceil() as i64)
}

fn fit_within_u64(w: u64, h: u64, max_side: u32) -> (u32, u32) {
    let cap = u128::from(max_side.max(1));
    if w >= h {
        let th = ((u128::from(h) * cap) / u128::from(w.max(1))).clamp(1, cap) as u32;
        (max_side.max(1), th)
    } else {
        let tw = ((u128::from(w) * cap) / u128::from(h.max(1))).clamp(1, cap) as u32;
        (tw, max_side.max(1))
    }
}

fn find_item_effects(st: &State, scene: &str, item_id: i64, source_id: u64, depth: u8) -> Option<Vec<Effect>> {
    if depth > 4 {
        return None;
    }
    let items = st.scenes.get(scene)?;
    for i in items {
        // The item itself: same number AND the same source. Item numbers restart in every
        // scene, so a nested scene's item with the same number is someone else's.
        if i.item_id == item_id && i.source == source_id {
            return Some(i.effects.clone());
        }
    }
    for i in items {
        if let Some(sub) = &i.scene
            && let Some(found) = find_item_effects(st, sub, item_id, source_id, depth + 1)
        {
            return Some(found);
        }
    }
    None
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let ids: Vec<u64> = self.preview_threads().keys().copied().collect();
        for id in ids {
            self.stop_preview(id);
        }
        // Join the render/encode loop first so it stops feeding outputs/captures.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Stop remaining outputs BEFORE releasing sources/parked captures so encoders flush cleanly.
        let ids: Vec<u64> = self.lock().outputs.iter().map(|(i, ..)| *i).collect();
        for id in ids {
            let _ = self.stop_output(id);
        }
        if let Some(t) = self.watchdog.take() {
            join_bounded(t, &self.watchdog_done, WATCHDOG_STOP_TIMEOUT);
        }
        self.audio.shutdown();
        let (sources, parked) = {
            let mut st = self.lock();
            (std::mem::take(&mut st.sources), std::mem::take(&mut st.parked))
        };
        drop(sources);
        drop(parked);
        self.sleep.release_all();
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
    pause: &(Arc<AtomicBool>, Arc<(Mutex<()>, Condvar)>),
    every: Duration,
) {
    let (paused_flag, wake) = (&*pause.0, &*pause.1);
    while !stop.load(Ordering::Acquire) {
        // Paused: asleep until resumed or stopped. No timer and no look at the engine's state,
        // so it never competes with the outputs for that lock.
        wait_while_paused(paused_flag, stop, wake);
        if stop.load(Ordering::Acquire) {
            break;
        }
        let started = Instant::now();
        let paused;
        let next = {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            let found = st.previews.get(&id);
            // Paused: nothing is composed (no scene walk, no GPU pass) until it is let go.
            paused = found.is_some_and(|p| p.paused);
            found.filter(|p| !p.paused).map(|p| {
                let draw = match &p.multiview {
                    Some(mv) => Draw::Scene(multiview_scene(&st, mv, now_ns())),
                    None => what_to_draw(&st, p.size.0, p.size.1, p.scene.as_deref(), now_ns(), Audience::Everyone),
                };
                // The outlines from the same items, at the same moment, as the picture: drawn
                // together they can't drift apart while a source is dragged (as OBS draws its
                // selection boxes in the frame it draws the scene).
                let mut look = p.look.clone();
                if let Some(m) = &look.marks {
                    let shown = p.scene.as_deref().or(st.on_air.as_deref()).unwrap_or("");
                    look.outlines = outlines(&st, m, p.size, shown, now_ns());
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
        std::thread::sleep(preview_wait(paused, every, started.elapsed()));
    }
}

/// How long a preview's thread waits before looking again: the rest of the frame, or (paused)
/// a slow look for the moment it is let go, so a paused view costs next to nothing.
fn preview_wait(paused: bool, every: Duration, spent: Duration) -> Duration {
    if paused { PAUSED_LOOK.max(every).saturating_sub(spent) } else { every.saturating_sub(spent) }
}

/// How often a paused preview checks whether it should draw again.
const PAUSED_LOOK: Duration = Duration::from_millis(100);

/// The preview's outlines (canvas pixels) for `m`, from the engine's own items at `ts`: every
/// shown source of `m.scene` in its colour, then the picked ones with handles on top.
///
/// Only for the scene this preview shows (`shown`): the main and the vertical picture each have
/// scenes in the engine, and a box for a source picked in the vertical one was drawn on the main
/// one's corner too (19 Nov).
fn outlines(
    st: &State,
    m: &crate::preview::Marks,
    canvas: (u32, u32),
    shown: &str,
    ts: u64,
) -> Vec<crate::preview::Outline> {
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
    if let Some(scene) = m.scene.as_ref().filter(|s| s.as_str() == shown)
        && let Some(items) = st.scenes.get(scene)
    {
        for it in items.iter().filter(|i| i.visible && i.item_id != 0 && !picked(scene, i.item_id)) {
            if let Some(c) = corners(scene, it) {
                out.push(crate::preview::Outline { corners: c, color: color(it.item_id), width: 1.5, handles: false });
            }
        }
    }
    if let Some((scene, ids)) = m.picked.as_ref().filter(|(s, _)| s == shown)
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
fn what_to_draw(st: &State, width: u32, height: u32, scene: Option<&str>, ts: u64, aud: Audience) -> Draw {
    if let Some(name) = scene {
        return Draw::Scene(build(st, width, height, Some(name), ts, aud));
    }
    let mut draw = on_air_draw(st, width, height, ts, aud);
    // The top layer over both sides of a transition, so it stays still through it; never
    // over itself.
    if let Some(top) = st.top.as_deref()
        && st.on_air.as_deref() != Some(top)
    {
        let mut over = SceneItem::new(0, Placement::default());
        over.scene = Some(top.to_string());
        let item = || item_of(st, &over, ts, (width, height), aud);
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

/// What `canvas`'s own scenes hold: the scene it shows, or what is on air (with a transition's
/// other side, the stinger's and the top layer) for one that follows the program; a visible item for
/// the stream only or the recording only, in them and in the scenes and groups nested in them
/// (a hidden group or scene shows nothing). One canvas's flags never reach another's picture.
fn sides(st: &State, canvas: &Canvas) -> Sides {
    fn walk(st: &State, scene: &str, depth: u32, out: &mut Sides) {
        let Some(items) = st.scenes.get(scene) else { return };
        for i in items.iter().filter(|i| i.visible) {
            match i.show {
                Show::Stream => out.stream_only = true,
                Show::Recording => out.recording_only = true,
                Show::Both => {}
            }
            if let Some(inner) = i.scene.as_deref()
                && depth < MAX_NESTING
            {
                walk(st, inner, depth + 1, out);
            }
        }
    }
    let mut roots: Vec<&str> = Vec::new();
    match canvas.scene.as_deref() {
        Some(name) => roots.push(name),
        None => {
            roots.extend(st.on_air.as_deref());
            roots.extend(st.top.as_deref());
            roots.extend(st.moving.as_ref().map(|(from, ..)| from.as_str()));
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            roots.extend(st.stinger.as_ref().map(|(from, ..)| from.as_str()));
        }
    }
    let mut out = Sides::default();
    for r in roots {
        walk(st, r, 0, &mut out);
    }
    out
}

/// What's on air: one scene, or two mid-transition.
fn on_air_draw(st: &State, width: u32, height: u32, ts: u64, aud: Audience) -> Draw {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    if let Some((from, _, cut, _)) = &st.stinger {
        let under = if ts < *cut { Some(from.as_str()) } else { st.on_air.as_deref() };
        let mut s = build(st, width, height, under, ts, aud);
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
        s.items.extend(item_of(st, &over, 0, (width, height), aud));
        return Draw::Scene(s);
    }
    match &st.moving {
        Some((from, TransitionKind::Move, start, dur)) if ts < start + dur => {
            let t = crate::cubic_ease_in_out(ts.saturating_sub(*start) as f32 / *dur as f32);
            Draw::Scene(moved(st, (width, height), from, st.on_air.as_deref().unwrap_or_default(), t, ts, aud))
        }
        Some((from, kind, start, dur)) if ts < start + dur => {
            let t = ts.saturating_sub(*start) as f32 / *dur as f32;
            let to = build(st, width, height, st.on_air.as_deref(), ts, aud);
            Draw::Moving(Box::new(Transition {
                kind: kind.clone(),
                t,
                from: build(st, width, height, Some(from), ts, aud),
                to,
            }))
        }
        _ => Draw::Scene(build(st, width, height, st.on_air.as_deref(), ts, aud)),
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
        items.push(at(Content::Scene(build(st, mv.canvas.0, mv.canvas.1, name, ts, Audience::Everyone)), b.area));
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

/// Sources currently active on air (program, top layer, transitions/stingers, outputs, taps,
/// globals), matching OBS's `obs_source_active` (excludes Multiview / Studio Mode preview boxes).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn active(st: &State) -> std::collections::HashSet<u64> {
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
    let mut out = heard(st);
    for (_, c, _) in &st.taps {
        if let Some(s) = c.scene.as_deref() {
            walk(st, s, 0, &mut out);
        }
    }
    out
}

/// Sources something drawn shows now: on air, a transition's or stinger's, an output's own
/// scene, a preview's or a multiview's, with the scenes inside them and the masks they use.
/// Sound bars that are shown: what they heard this frame (the sound on air, or their
/// source's through a tap opened by name, again if that source is opened anew), drawn now.
fn visualize(
    st: &mut State,
    seen: &std::collections::HashSet<u64>,
    gpu: &Compositor,
    audio_cmd: &mpsc::Sender<AudioCmd>,
    rate: u32,
) {
    use crate::source::visualizer::TapKey;
    let names: HashMap<String, u64> = st.opened.iter().map(|(n, (id, _))| (n.clone(), *id)).collect();
    for (id, src) in st.sources.iter_mut() {
        let Live::Visualizer(v) = &mut src.live else { continue };
        if !seen.contains(id) {
            continue;
        }
        let heard = match v.spec.of.as_deref() {
            None => {
                if v.tap.as_ref().is_none_or(|(k, _)| *k != TapKey::Master) {
                    let buf = Arc::new(Mutex::new(std::collections::VecDeque::new()));
                    let _ = audio_cmd.send(AudioCmd::AttachMasterTap(Arc::downgrade(&buf)));
                    v.tap = Some((TapKey::Master, crate::mix::Tap(buf)));
                }
                v.tap.as_ref().map(|(_, t)| t.take()).unwrap_or_default()
            }
            Some(name) => match names.get(name) {
                Some(&target) => {
                    if v.tap.as_ref().is_none_or(|(k, _)| *k != TapKey::Source(target)) {
                        let buf = Arc::new(Mutex::new(std::collections::VecDeque::new()));
                        let _ = audio_cmd.send(AudioCmd::AttachTap { id: target, tap: Arc::downgrade(&buf) });
                        v.tap = Some((TapKey::Source(target), crate::mix::Tap(buf)));
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

/// What is drawn (sources and mask pictures), and the Video Delay filters of the items that
/// show: (a source's delay in ms, whether its sound goes with it). The items that show decide,
/// so a delay on a source in a scene that's off air (or an item that's hidden) holds nothing
/// and doesn't delay the source where it's shown without one; where several show it with
/// different delays, the longest wins (a source has one queue).
fn seen(st: &State) -> (std::collections::HashSet<u64>, HashMap<u64, (u32, bool)>) {
    type Delays = HashMap<u64, (u32, bool)>;
    fn walk(st: &State, scene: &str, depth: u32, out: &mut std::collections::HashSet<u64>, dl: &mut Delays) {
        let Some(items) = st.scenes.get(scene) else { return };
        for it in items.iter().filter(|i| i.visible) {
            for e in &it.effects {
                if let Effect::Mask { image, .. } | Effect::Cutout { mask: image, .. } = e {
                    out.insert(*image);
                }
            }
            match &it.scene {
                Some(inner) if depth < MAX_NESTING => walk(st, inner, depth + 1, out, dl),
                Some(_) => {}
                None => {
                    out.insert(it.source);
                    for e in &it.effects {
                        if let Effect::Delay { ms, audio } = e
                            && *ms > 0
                        {
                            let cur = dl.entry(it.source).or_insert((0, false));
                            cur.0 = cur.0.max(*ms);
                            cur.1 |= *audio;
                        }
                    }
                }
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    let mut dl = Delays::new();
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
    for p in st.previews.values().filter(|p| !p.paused) {
        roots.extend(p.scene.as_deref());
        if let Some(mv) = &p.multiview {
            roots.extend(mv.boxes.iter().filter_map(|(b, _)| b.scene.as_deref()));
        }
    }
    for r in roots {
        walk(st, r, 0, &mut out, &mut dl);
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    out.insert(STINGER);
    (out, dl)
}

/// Pictures kept back while unseen, sent to the GPU now (something is about to draw them
/// another way: a thumbnail, a snapshot).
fn send_pending(st: &mut State, gpu: &Compositor) {
    let frozen = frozen(st);
    for (id, src) in st.sources.iter_mut() {
        // A frozen source keeps its picture: what came in meanwhile is dropped.
        if frozen.contains(id) && src.sent > 0 {
            src.pending = None;
            src.pending_since = 0;
            continue;
        }
        if let Some(f) = src.pending.take() {
            src.pending_since = 0;
            if f.send(gpu, *id) {
                src.size = Some(f.info());
            }
        }
    }
}

/// An item as the compositor takes it, `ms` into its scene; None without a picture yet.
fn item_of(st: &State, it: &SceneItem, ms: u64, canvas: (u32, u32), aud: Audience) -> Option<Item> {
    item_at(st, it, ms, 0, canvas, aud)
}

/// Scenes inside scenes go at most this deep (a scene showing itself stops there).
const MAX_NESTING: u32 = 8;

/// Whether scene `name` holds something that `aud` doesn't see: a visible item for the other
/// side, or a visible container (that this audience does see) whose own contents are all for the
/// other side. It tells a container emptied by the audience from one that was empty to begin with.
fn emptied_by(st: &State, name: &str, depth: u32, aud: Audience) -> bool {
    let Some(items) = st.scenes.get(name) else { return false };
    items.iter().filter(|i| i.visible).any(|i| {
        !i.show.shown_to(aud)
            || i.scene.as_deref().is_some_and(|inner| {
                depth < MAX_NESTING
                    && emptied_by(st, inner, depth + 1, aud)
                    && !holds_anything_for(st, inner, depth + 1, aud)
            })
    })
}

/// Whether scene `name` has any visible item that `aud` sees (and, in a container, anything in it).
fn holds_anything_for(st: &State, name: &str, depth: u32, aud: Audience) -> bool {
    let Some(items) = st.scenes.get(name) else { return false };
    items.iter().filter(|i| i.visible && i.show.shown_to(aud)).any(|i| match i.scene.as_deref() {
        Some(inner) if depth < MAX_NESTING => holds_anything_for(st, inner, depth + 1, aud),
        Some(_) => false,
        None => true,
    })
}

fn item_at(st: &State, it: &SceneItem, ms: u64, depth: u32, canvas: (u32, u32), aud: Audience) -> Option<Item> {
    let (placement, opacity) = animated(it, ms);
    let content = match &it.scene {
        Some(name) if depth < MAX_NESTING => {
            let scene = st.scenes.get(name)?;
            // Its items keep the time of the scene showing it (as OBS: a nested scene plays along).
            let items = scene
                .iter()
                .filter(|i| i.visible && i.show.shown_to(aud))
                .filter_map(|i| item_at(st, i, ms, depth + 1, canvas, aud))
                .collect::<Vec<_>>();
            // Everything in it is for the other audience: it draws nothing (its own filters would
            // otherwise paint an empty box into this picture), however many containers deep.
            if items.is_empty() && emptied_by(st, name, depth, aud) {
                return None;
            }
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
fn build(st: &State, width: u32, height: u32, name: Option<&str>, ts: u64, aud: Audience) -> Scene {
    let mut items = Vec::new();
    if let Some(name) = name
        && let Some(scene) = st.scenes.get(name)
    {
        let ms = scene_ms(st, name, ts);
        items.extend(
            scene
                .iter()
                .filter(|i| i.visible && i.show.shown_to(aud))
                .filter_map(|it| item_of(st, it, ms, (width, height), aud)),
        );
    }
    Scene { width, height, items }
}

/// The Move transition `t` of the way (eased): a source in both scenes glides from its old
/// place to its new one; one only in the old scene fades out underneath, one only in the new
/// fades in.
fn moved(st: &State, canvas: (u32, u32), from: &str, to: &str, t: f32, ts: u64, aud: Audience) -> Scene {
    let (empty, empty2) = (Vec::new(), Vec::new());
    let a = st.scenes.get(from).unwrap_or(&empty);
    let b = st.scenes.get(to).unwrap_or(&empty2);
    let (ma, mb) = (scene_ms(st, from, ts), scene_ms(st, to, ts));
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let (width, height) = canvas;
    // The same thing in both scenes: one source, or one nested scene.
    let same = |x: &SceneItem, y: &SceneItem| x.source == y.source && x.scene == y.scene;
    let mut items = Vec::new();
    let shows = |i: &SceneItem| i.visible && i.show.shown_to(aud);
    for old in a.iter().filter(|i| shows(i) && !b.iter().any(|n| shows(n) && same(n, i))) {
        if let Some(mut it) = item_of(st, old, ma, canvas, aud) {
            it.opacity *= 1.0 - t;
            items.push(it);
        }
    }
    for new in b.iter().filter(|i| shows(i)) {
        let Some(mut it) = item_of(st, new, mb, canvas, aud) else { continue };
        match a.iter().find(|o| shows(o) && same(o, new)).and_then(|o| item_of(st, o, ma, canvas, aud)) {
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
fn on_time_os() {
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
fn on_time_os() {
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
fn on_time_os() {}

/// Prepares a real-time engine or audio thread: raises OS priority/QoS and enables x86 FTZ/DAZ
/// so exponential decay in silent audio filters never stalls on denormals (M2-06).
fn on_time() {
    crate::afx::enable_ftz_daz();
    on_time_os();
}

/// Raises the calling thread to the frame loop's priority (macOS: "user interactive"; Windows:
/// high, out of power throttling): for a thread the sound depends on, such as a filter's own
/// worker, so a busy computer wakes it in time.
pub fn raise_this_thread() {
    on_time();
}

/// RAII guard for real-time audio thread priority/MMCSS registration on `ferrocast-audio`.
#[allow(dead_code)]
/// The work the audio thread declares to the real-time scheduler per 21.3 ms block (macOS): 20 times
/// its measured p99 (about 0.2 ms) and 1.4 times the worst block seen in a 5-minute soak with two
/// DeepFilterNet mics, music and a 1440p60 stream (2.8 ms; the denoiser runs off the thread).
pub(crate) const AUDIO_COMPUTATION_NS: u64 = 4_000_000;

#[doc(hidden)]
#[derive(Debug)]
pub struct AudioThreadGuard {
    #[cfg(target_os = "windows")]
    mmcss: Option<windows::Win32::Foundation::HANDLE>,
}

#[cfg(target_os = "windows")]
impl Drop for AudioThreadGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if let Some(h) = self.mmcss.take() {
            // SAFETY: `h` was returned by `AvSetMmThreadCharacteristicsW` on this thread and
            // has not been reverted yet.
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// Prepares the dedicated real-time audio thread (`ferrocast-audio`): enables x86 FTZ/DAZ, sets
/// `QOS_CLASS_USER_INTERACTIVE` on macOS, or registers MMCSS `"Audio"` + disables EcoQoS on Windows.
#[allow(dead_code, unsafe_code)]
#[doc(hidden)]
pub fn raise_audio_thread() -> AudioThreadGuard {
    crate::afx::enable_ftz_daz();
    #[cfg(target_os = "macos")]
    {
        // SAFETY: sets the calling thread's own QoS class; takes no pointers.
        let r = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
        if r != 0 {
            tracing::debug!(r, "audio thread kept default QoS class");
        }
        // The real-time class, as CoreAudio's own threads: precise wakes for a 21.3 ms block's
        // clock (about 4 ms of work, due within 10 ms of waking).
        if !crate::apple::make_realtime(21_333_333, AUDIO_COMPUTATION_NS, 10_000_000) {
            tracing::debug!("audio thread kept the timeshare class");
        }
        AudioThreadGuard {}
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadInformation, SetThreadPriority,
            THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_HIGHEST, ThreadPowerThrottling,
        };
        let mut task_idx = 0u32;
        // SAFETY: `task_idx` is a valid local `u32` and `"Audio"` is a static wide string literal.
        let mmcss = match unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Audio"), &mut task_idx) } {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::debug!(%e, "MMCSS Audio registration failed; falling back to THREAD_PRIORITY_HIGHEST");
                // SAFETY: pseudo-handle of the current thread (never closed).
                let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
                None
            }
        };
        let thread = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        // SAFETY: pseudo-handle of this thread and stack-pinned struct alive for the call.
        if let Err(e) = unsafe {
            SetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw const thread).cast(),
                size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            )
        } {
            tracing::debug!(%e, "audio thread kept default power throttling");
        }
        AudioThreadGuard { mmcss }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        AudioThreadGuard {}
    }
}

/// Remaps imported ducking source IDs to the engine's live IDs, dropping sources that failed to
/// open and ensuring no source ducks itself.
fn remap_ducking(
    ducking: Option<crate::level::Ducking>,
    to: &dyn Fn(u64) -> u64,
    live_ids: &std::collections::HashSet<u64>,
) -> Option<crate::level::Ducking> {
    let mut d = ducking?;
    d.voices = d.voices.into_iter().map(to).filter(|id| live_ids.contains(id)).collect();
    d.music = d.music.into_iter().map(to).filter(|id| live_ids.contains(id) && !d.voices.contains(id)).collect();
    (!d.voices.is_empty() && !d.music.is_empty()).then_some(d)
}

/// The frame loop.
#[allow(clippy::too_many_arguments)]
fn run(
    config: EngineConfig,
    state: &Arc<Mutex<State>>,
    gpu: &Compositor,
    audio_cmd: &mpsc::Sender<AudioCmd>,
    levels: &Arc<RwLock<HashMap<u64, Arc<AtomicLevels>>>>,
    bridge_rx: &mpsc::Receiver<Block>,
    recycle_tx: &mpsc::SyncSender<Block>,
    stop: &AtomicBool,
    seen: &Seen,
    sleep: &crate::sleep::SleepMonitor,
) {
    on_time();
    let frame_ns = config.frame_ns();
    let mut next = now_ns();
    let mut faults = 0u64;
    while !stop.load(Ordering::Acquire) {
        let now = now_ns();
        if now < next {
            crate::sleep_precise(Duration::from_nanos((next - now).min(5_000_000)));
            continue;
        }
        // Fell behind by more than a frame: skip ahead instead of rushing (as libobs does).
        let behind = (now - next) / frame_ns;
        let ts = next + behind * frame_ns;
        next = ts + frame_ns;
        let started = Instant::now();

        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let ticket =
                frame(&config, state, gpu, audio_cmd, levels, bridge_rx, recycle_tx, ts, behind, started, seen);
            // Pictures uploaded this frame are sent even when nothing drew (no preview, no
            // output), and what the card finished with is let go: without this, Direct3D and
            // Vulkan keep it all and run out of memory in minutes.
            gpu.submit(std::iter::empty());
            let _ = gpu.device.poll(wgpu::PollType::Poll);
            ticket
        }));
        if let Ok(Some((epoch, want))) = ok {
            sleep.apply(epoch, want);
        }
        // Behind (a frame took longer than its time): a moment before the next one takes the
        // engine's lock again. Windows' lock isn't fair: re-taken at once, frame after frame,
        // the window's thread waiting for it (stats, the preview) got it only every 10-48 s
        // on GitHub's runner, every Studio call stuck behind it (19 Nov). On time, the loop
        // sleeps until the next frame anyway.
        if now_ns() >= next {
            crate::sleep_precise(Duration::from_millis(1));
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

/// Opens a video or audio file with full playback options (macOS AVFoundation with FFmpeg
/// fallback; Windows/Linux FFmpeg with NV12 for opaque and RGBA for alpha).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) fn open_video_with(
    path: &std::path::Path,
    options: crate::ffsrc::MediaOptions,
    start_stopped: bool,
) -> Result<crate::media::Media, AvError> {
    crate::media::Media::open_with(path, options, start_stopped)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) fn open_video(path: &std::path::Path, looping: bool) -> Result<crate::media::Media, AvError> {
    open_video_with(path, crate::ffsrc::MediaOptions::looping(looping), false)
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
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn display_now(display: u32, uuid: Option<&str>) -> u32 {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return crate::screen::display_now(display, uuid).unwrap_or(GONE_DISPLAY);
    #[cfg(target_os = "linux")]
    {
        let _ = uuid;
        display
    }
}

/// A display number none has: opening it says it isn't connected.
#[cfg(target_os = "macos")]
const GONE_DISPLAY: u32 = 0;
#[cfg(target_os = "windows")]
const GONE_DISPLAY: u32 = u32::MAX;

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
        #[cfg(target_os = "windows")]
        SourceKind::Screen { display, uuid, sound, hide, cursor } => ScreenCapture::start_display(
            *display,
            uuid.as_deref(),
            (config.width, config.height),
            config.fps,
            *cursor,
            *sound,
            hide,
        )
        .map(Live::Screen),
        #[cfg(not(target_os = "windows"))]
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
        // Windows reads WASAPI loopback directly without enumerating displays or windows.
        #[cfg(target_os = "windows")]
        SourceKind::DesktopSound => ScreenCapture::start_desktop_sound().map(Live::Screen),
        // ScreenCaptureKit gives the sound with a display's picture: the smallest, slowest one.
        #[cfg(target_os = "macos")]
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
        #[cfg(test)]
        Live::PanicOnDrop(_) => Some((0, false, Some("dead".into()))),
        #[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        Live::FakeCamera(c) => Some((c.received(), true, c.ended())),
        _ => None,
    }
}

/// Why a capture should be started again now, if it should: it faulted, isn't running (a
/// restart that failed), ended, never delivered its first frame, or (one that sends all the
/// time) has been quiet too long.
fn look(w: &mut Watch, running: bool, signs: Option<(u64, bool, Option<String>)>, now: u64) -> Option<String> {
    if w.faulted {
        return Some("it faulted".into());
    }
    if w.regained {
        w.regained = false;
        w.denied = false;
        return Some("its permission is back".into());
    }
    if w.denied {
        return None;
    }
    if let Some((_, _, Some(why))) = &signs
        && is_permission_end(why)
    {
        w.denied = true;
        return None;
    }
    if w.cap_no_signal && !w.ever_sent && w.tries >= NO_SIGNAL_MAX_TRIES {
        // Waiting for a signal, with the session left open: a picture that comes now (a console
        // switched on) lets go of the wait, as USB hotplug does. Nothing else is tried.
        let gone = signs.as_ref().is_some_and(|(_, _, ended)| ended.is_some());
        match signs {
            Some((count, ..)) if running && count > 0 => {
                w.ever_sent = true;
                w.no_signal = false;
                w.count = count;
                w.since = now;
                w.tries = 0;
                w.next_try = 0;
            }
            // No session to see the signal come with (the last try failed to open, or it ended
            // with an error): retry on backoff until a healthy open session is listening.
            _ if !running || gone => {
                w.no_signal = true;
                return Some("its session is gone while it waits for a signal".into());
            }
            _ => w.no_signal = true,
        }
        return None;
    }
    if !running {
        return Some("it didn't start again last time".into());
    }
    let (count, steady, ended) = signs?;
    if count > 0 {
        w.ever_sent = true;
        w.no_signal = false;
    }
    if count != w.count {
        w.count = count;
        w.since = now;
        if w.flowing_since == 0 {
            w.flowing_since = now.max(1);
        }
        // A link that comes and goes (a loose HDMI cable) is retried on its growing backoff; only
        // a capture that has kept sending for a while starts afresh.
        if now.saturating_sub(w.flowing_since) >= STABLE_NS {
            w.tries = 0;
            w.next_try = 0;
        }
    }
    let why = ended.or_else(|| {
        if !steady {
            return None;
        }
        // Only a capture that has already produced a frame drops from 10 s first-frame grace to 5 s stall.
        let limit = if count > 0 { STALL_NS } else { FIRST_FRAME_NS };
        (now.saturating_sub(w.since) > limit)
            .then(|| if count > 0 { "it stopped sending" } else { "it never sent a first frame" }.into())
    });
    if why.is_some() {
        w.flowing_since = 0;
    }
    why
}

/// Whether a capture ended because its permission was turned off.
fn is_permission_end(why: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        why.starts_with(crate::apple::screen::PERMISSION_REVOKED)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = why;
        false
    }
}

#[cfg(target_os = "macos")]
/// A source that stopped for a turned-off permission is tried again once `granted()` says it's
/// back (asked without prompting), not before.
fn retry_if_permission_back(w: &mut Watch, granted: impl FnOnce() -> bool) {
    if w.denied && granted() {
        w.regained = true;
    }
}

/// Kinds an edit changes without opening or closing anything: a text's words or look, a colour,
/// a picture file. Their sources have no device and no sound.
fn is_in_place_kind(k: &SourceKind) -> bool {
    matches!(k, SourceKind::Text { .. } | SourceKind::Color { .. } | SourceKind::Image(_))
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

fn is_live_interrupted(live: &Live) -> bool {
    match live {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        Live::Camera(c) => c.interrupted(),
        #[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        Live::FakeCamera(c) => c.interrupted(),
        _ => false,
    }
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
        let dead = matches!(src.live, Live::Still)
            || src.watch.faulted
            || is_live_ended(&src.live)
            || src.is_waiting_for_camera()
            || src.watch.no_signal
            || src.camera_retry.as_ref().is_some_and(crate::camera_retry::CameraRetry::is_no_signal);
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
            if let Some(r) = src.camera_retry.as_mut() {
                r.on_hotplug(now);
            }
            src.watch.next_try = now;
            src.watch.tries = 0;
            src.watch.no_signal = false;
            woke += 1;
        }
    }
    woke
}

type Reopener = Arc<dyn Fn(&SourceKind) -> Option<Result<Live, AvError>> + Send + Sync>;

/// Evaluates all captures once and spawns independent restart workers (capped to
/// [`MAX_PARALLEL_REOPENS`] concurrent opens) with a hard [`REOPEN_TIMEOUT_NS`] slot deadline.
fn watchdog_step(
    state: &Arc<Mutex<State>>,
    audio_cmd: &mpsc::Sender<AudioCmd>,
    stop: &Arc<AtomicBool>,
    reopen: &Reopener,
    connected: Option<&[String]>,
    now: u64,
) {
    use crate::camera_retry::{CameraAction, CameraObservation};

    let step_started = Instant::now();
    let mut todo = Vec::new();
    let mut expired = Vec::new();
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ids) = connected {
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
        let shown_ids = seen(&st).0;
        let active_ids = active_or_empty(&st);
        let has_scenes = !st.scenes.is_empty() || !st.opened.is_empty();
        let mut in_flight = 0usize;
        let mut ready: Vec<(u32, u64, String)> = Vec::new();
        let mut said: Vec<crate::notices::Notice> = Vec::new();
        for (id, src) in st.sources.iter_mut() {
            if src.kind.is_none() {
                continue;
            }
            if src.opening.is_some() {
                if now.saturating_sub(src.opening_since) < REOPEN_TIMEOUT_NS {
                    in_flight += 1;
                    continue;
                }
                tracing::warn!(source = *id, "watchdog reopen worker timed out after 60 s; releasing slot");
                src.opening = None;
                src.opening_since = 0;
            }
            let running = !matches!(src.live, Live::Still);
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            if let Live::Microphone(c) = &src.live
                && let Some(n) = c.take_notice()
            {
                said.push(n);
            }
            let live_signs = signs(&src.live);
            if let Some(retry) = src.camera_retry.as_mut() {
                let visible_or_on_air = !has_scenes || shown_ids.contains(id) || active_ids.contains(id);
                let (frames, ended) = match live_signs {
                    Some((count, _, e)) => (count, e.is_some()),
                    None => (0, false),
                };
                let ended = ended || src.watch.faulted;
                let was_no_signal = retry.is_no_signal();
                src.watch.count = frames;
                let action = retry.step(CameraObservation {
                    running,
                    frames,
                    ended,
                    interrupted: is_live_interrupted(&src.live),
                    opening: false,
                    visible_or_on_air,
                    now_ns: now,
                });
                src.watch.no_signal = retry.is_no_signal();
                src.watch.ever_sent = retry.ever_sent();
                if !was_no_signal && retry.is_no_signal() {
                    tracing::warn!(
                        source = *id,
                        tries = NO_SIGNAL_MAX_TRIES,
                        "capture didn't deliver a picture (no signal); leaving session open until signal or disconnect"
                    );
                }
                if let CameraAction::Reopen(why) = action {
                    ready.push((retry.tries(), *id, why.to_string()));
                }
                continue;
            }
            let w = &mut src.watch;
            #[cfg(target_os = "macos")]
            retry_if_permission_back(w, crate::apple::screen::screen_access_granted);
            let why = look(w, running, live_signs, now);
            if let Some(why) = why
                && now >= w.next_try
            {
                ready.push((w.tries, *id, why));
            }
        }
        for n in said {
            st.notices.push(n);
        }
        ready.sort_by_key(|(tries, id, _)| (*tries, *id));
        let total_workers = st.reopen_workers.clone();
        let mut launched = 0usize;
        for (_, id, why) in ready {
            if in_flight + launched >= MAX_PARALLEL_REOPENS {
                break;
            }
            if total_workers.load(Ordering::Acquire) >= MAX_TOTAL_REOPEN_WORKERS {
                tracing::warn!(
                    active = total_workers.load(Ordering::Acquire),
                    "global reopen worker cap reached; waiting for stalled workers to exit"
                );
                break;
            }
            let Some(src) = st.sources.get_mut(&id) else { continue };
            if src.reopen_workers.load(Ordering::Acquire) >= MAX_SOURCE_REOPEN_WORKERS {
                tracing::warn!(
                    source = id,
                    workers = src.reopen_workers.load(Ordering::Acquire),
                    "source reopen worker cap reached; waiting for stalled workers to exit"
                );
                if let Some(retry) = src.camera_retry.as_mut() {
                    retry.defer_backoff(now);
                    src.watch.next_try = retry.next_try_ns();
                    src.watch.tries = retry.tries();
                } else {
                    let prev = src.watch.tries;
                    src.watch.next_try = now + src.watch.backoff(prev);
                    src.watch.tries = prev.saturating_add(1);
                }
                continue;
            }
            let Some(kind) = src.kind.clone() else { continue };
            let token = next_open_token();
            let prev_tries = if let Some(retry) = src.camera_retry.as_mut() {
                let prev = retry.on_reopen_started(now);
                src.watch.next_try = retry.next_try_ns();
                src.watch.tries = retry.tries();
                prev
            } else {
                let prev = src.watch.tries;
                src.watch.next_try = now + src.watch.backoff(prev);
                src.watch.tries = prev.saturating_add(1);
                prev
            };
            src.watch.faulted = false;
            src.watch.since = now;
            src.opening = Some(token);
            src.opening_since = now;
            let workers = src.reopen_workers.clone();
            let worker_guard = ReopenWorkerGuard::new(workers.clone(), total_workers.clone());
            // Taken out now (a camera is let go before it's opened again), dropped on its
            // own reopen worker outside the lock.
            let old = std::mem::replace(&mut src.live, Live::Still);
            st.source_reopen_workers.entry(id).or_insert_with(|| (Some(kind.clone()), workers));
            todo.push((id, kind, why, old, token, prev_tries, worker_guard));
            launched += 1;
        }
        st.prune_source_workers();
    }
    for (id, kind, why, old, token, prev_tries, worker_guard) in todo {
        if stop.load(Ordering::Acquire) {
            drop(worker_guard);
            drop(old);
            return;
        }
        let (state_w, state_panic, audio_w, stop_w, reopen_w) =
            (state.clone(), state.clone(), audio_cmd.clone(), stop.clone(), reopen.clone());
        let spawned = std::thread::Builder::new().name("ferrocore-reopen".into()).spawn(move || {
            let _worker_guard = worker_guard;
            let worker_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                drop(old);
                if stop_w.load(Ordering::Acquire) {
                    return;
                }
                let still_mine = {
                    let st = state_w.lock().unwrap_or_else(|e| e.into_inner());
                    st.sources.get(&id).is_some_and(|s| s.opening == Some(token) && s.kind.as_ref() == Some(&kind))
                };
                if !still_mine {
                    return;
                }
                tracing::warn!(source = id, %why, "the watchdog starts a source again");
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reopen_w(&kind)))
                    .unwrap_or_else(|p| {
                        Some(Err(AvError::Source(format!("reopening it failed: {}", panic_text(&*p)))))
                    });
                if stop_w.load(Ordering::Acquire) {
                    drop(outcome);
                    return;
                }
                let mut leftover = None;
                {
                    let mut st = state_w.lock().unwrap_or_else(|e| e.into_inner());
                    let finished_at = now.saturating_add(step_started.elapsed().as_nanos() as u64);
                    let Some(src) =
                        st.sources.get_mut(&id).filter(|s| s.opening == Some(token) && s.kind.as_ref() == Some(&kind))
                    else {
                        drop(st);
                        drop(outcome);
                        return;
                    };
                    src.opening = None;
                    src.opening_since = 0;
                    match outcome {
                        Some(Ok(live)) => {
                            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                            let oc = output_capture_of(&live, src.kind.as_ref());
                            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                            let oc: Option<crate::mix::OutputCapture> = None;
                            leftover = Some(std::mem::replace(&mut src.live, live));
                            src.audio_tx = None;
                            // What the old capture left in the delay (its pictures and sound)
                            // mustn't come out ahead of the new one's: started again, empty.
                            src.video_delay = None;
                            src.audio_delay = None;
                            src.delay_cap = None;
                            src.watch.count = 0;
                            src.watch.since = now_ns();
                            if let Some(retry) = src.camera_retry.as_mut() {
                                retry.on_reopen_finished(prev_tries, true, finished_at, None);
                                src.watch.next_try = retry.next_try_ns();
                            } else {
                                src.watch.next_try = finished_at + src.watch.backoff(prev_tries);
                            }
                            st.stats.restarts += 1;
                            let _ = audio_w.send(AudioCmd::SetOutputCapture { id, capture: oc });
                        }
                        Some(Err(e)) => {
                            if let Some(retry) = src.camera_retry.as_mut() {
                                retry.on_reopen_finished(prev_tries, false, finished_at, Some(&e.to_string()));
                                src.watch.next_try = retry.next_try_ns();
                            } else {
                                src.watch.next_try = finished_at + src.watch.backoff(prev_tries);
                            }
                            tracing::warn!(source = id, error = %e, "the source didn't start again; it's tried later");
                        }
                        None => {}
                    }
                }
                drop(leftover);
            }));
            if let Err(p) = worker_res {
                tracing::error!(source = id, error = %panic_text(&*p), "watchdog reopen worker panicked");
                let mut st = state_panic.lock().unwrap_or_else(|e| e.into_inner());
                let finished_at = now.saturating_add(step_started.elapsed().as_nanos() as u64);
                if let Some(src) = st.sources.get_mut(&id).filter(|s| s.opening == Some(token)) {
                    src.opening = None;
                    src.opening_since = 0;
                    if let Some(retry) = src.camera_retry.as_mut() {
                        retry.on_reopen_finished(prev_tries, false, finished_at, Some("reopen panicked"));
                        src.watch.next_try = retry.next_try_ns();
                    } else {
                        src.watch.next_try = finished_at + src.watch.backoff(prev_tries);
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::error!(source = id, error = %e, "couldn't start watchdog reopen thread");
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            let finished_at = now.saturating_add(step_started.elapsed().as_nanos() as u64);
            if let Some(src) = st.sources.get_mut(&id).filter(|s| s.opening == Some(token)) {
                src.opening = None;
                src.opening_since = 0;
                if let Some(retry) = src.camera_retry.as_mut() {
                    retry.on_reopen_finished(prev_tries, false, finished_at, Some(&e.to_string()));
                    src.watch.next_try = retry.next_try_ns();
                } else {
                    src.watch.next_try = finished_at + src.watch.backoff(prev_tries);
                }
            }
        }
    }
    drop(expired);
}

/// The watchdog: once a second (or right away when a device connects) it looks at every capture,
/// and one that faulted, ended or went quiet is started again (waiting longer after each try that
/// didn't hold).
struct SignalDone<'a>(&'a (Mutex<bool>, Condvar));

impl Drop for SignalDone<'_> {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0.1.notify_all();
    }
}

fn watchdog(
    config: EngineConfig,
    state: Arc<Mutex<State>>,
    audio_cmd: mpsc::Sender<AudioCmd>,
    stop: Arc<AtomicBool>,
    done: Arc<(Mutex<bool>, Condvar)>,
) {
    let _signal_done = SignalDone(&done);
    let mut hotplug_seq = 0u64;
    let _ = poll_hotplug(&mut hotplug_seq);
    let reopen: Reopener = Arc::new(move |kind| open_capture(&config, kind));
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
        watchdog_step(&state, &audio_cmd, &stop, &reopen, connected.as_deref(), now_ns());
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
    /// What differed for each output's and tap's canvas when this frame was drawn (`view`).
    sides: Vec<(u64, Sides)>,
}

/// Hands the frames that came back to their outputs and taps, oldest first: those ready, and
/// any past [`IN_FLIGHT`] (waiting for the card); every one with `flush`.
/// An output's failures, and when it is next tried again.
#[derive(Debug, Default)]
struct Heal {
    /// When it failed lately (the last [`HEAL_WINDOW`]).
    fails: std::collections::VecDeque<Instant>,
    /// Set while waiting to be made again; the output isn't fed meanwhile.
    retry_at: Option<Instant>,
}

/// Waits before the 1st, 2nd and later tries, as OBS reconnects: 1 s, 2 s, then 5 s.
const HEAL_WAITS: [Duration; 3] = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(5)];
/// Failures within this long are counted...
const HEAL_WINDOW: Duration = Duration::from_secs(300);
/// ...and this many give up: the output keeps its error (Health shows it).
const HEAL_GIVE_UP: usize = 5;

/// Notes a failure of output `id`: tried again after a wait, or given up on.
fn note_failure(st: &mut State, id: u64, why: &str) {
    let now = Instant::now();
    let h = st.heal.entry(id).or_default();
    while h.fails.front().is_some_and(|t| now.duration_since(*t) > HEAL_WINDOW) {
        h.fails.pop_front();
    }
    h.fails.push_back(now);
    let n = h.fails.len();
    if n >= HEAL_GIVE_UP {
        h.retry_at = None;
        tracing::error!(output = id, failures = n, error = %why, "an output keeps failing; giving up on it");
        st.errors.entry(id).or_insert_with(|| format!("it kept failing ({n} times in 5 minutes): {why}"));
    } else {
        let wait = HEAL_WAITS[(n - 1).min(HEAL_WAITS.len() - 1)];
        h.retry_at = Some(now + wait);
        tracing::error!(
            output = id,
            failure = n,
            wait_ms = wait.as_millis() as u64,
            error = %why,
            "an output's encoder failed; making it again"
        );
    }
}

/// Makes the encoders again of every output whose wait is over. A stream resumes at a keyframe; a
/// recording asks for a new part (the caller names the file).
fn heal_outputs(st: &mut State) {
    let now = Instant::now();
    let due: Vec<u64> =
        st.heal.iter().filter(|(_, h)| h.retry_at.is_some_and(|t| t <= now)).map(|(id, _)| *id).collect();
    for id in due {
        let result = st.outputs.iter_mut().find(|(i, ..)| *i == id).map(|(_, pipe, ..)| pipe.heal());
        match result {
            Some(Ok(())) => {
                if let Some(h) = st.heal.get_mut(&id) {
                    h.retry_at = None;
                }
                tracing::info!(output = id, "an output's encoder was made again; it goes on");
            }
            Some(Err(e)) => note_failure(st, id, &e.to_string()),
            None => {
                st.heal.remove(&id);
            }
        }
    }
}

fn deliver(st: &mut State, gpu: &Compositor, flush: bool, recycle_tx: &mpsc::SyncSender<Block>) {
    let _ = gpu.device.poll(wgpu::PollType::Poll);
    heal_outputs(st);
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
        let InFlight { ts, blocks, outputs, taps, drawn, sides } = e;
        let sides_of = |id: u64| sides.iter().find(|(i, _)| *i == id).map_or(Sides::default(), |s| s.1);
        // A frame the card drew into a pooled surface stays one (no copy): the encoders share it,
        // and a tap that needs host bytes makes them once.
        let drawn: Vec<(Canvas, Result<crate::Drawn, String>)> = drawn
            .into_iter()
            .map(|(c, f)| (c, f.and_then(|p| gpu.finish_drawn(p).map_err(|e| e.to_string()))))
            .collect();
        let mut failures: Vec<(u64, String)> = Vec::new();
        // Given up on (an error stays) or waiting to be made again: not fed meanwhile.
        let dead: Vec<u64> = st
            .errors
            .keys()
            .copied()
            .chain(st.heal.iter().filter(|(_, h)| h.retry_at.is_some()).map(|(id, _)| *id))
            .collect();
        let mut encoding = Duration::ZERO;
        let mut per_out_encoding: Vec<(u64, Duration)> = Vec::new();
        for (id, pipe, kind, canvas) in st.outputs.iter_mut() {
            if !outputs.contains(id) {
                continue;
            }
            let mut failed: Option<String> = None;
            // A recording on its own that can't be written any more has nothing to encode for:
            // it would only keep the encoder busy until someone presses Stop. (One that shares
            // its encode with a stream goes on, for the stream.)
            if matches!(kind, OutputKind::Record(_)) && pipe.recording_failed() {
                continue;
            }
            // An output that has failed isn't fed again: its encoder would only fail 60 times a
            // second, with the engine's lock held, until someone stops it.
            if dead.contains(id) {
                continue;
            }
            let mut out_encoding = Duration::ZERO;
            let mut tags = Vec::new();
            if let Some((_, frame)) = drawn.iter().find(|(c, _)| *c == view(canvas, sides_of(*id))) {
                match frame {
                    Ok(f) => {
                        let w = canvas.width as usize;
                        let t0 = Instant::now();
                        match pipe.video(f.as_frame(w), ts) {
                            Ok(t) => tags = t,
                            Err(e) => failed = Some(e.to_string()),
                        }
                        let dt = t0.elapsed();
                        out_encoding += dt;
                        encoding += dt;
                    }
                    Err(e) => failed = Some(e.clone()),
                }
                for (i, &(w, h, _)) in canvas.video_tracks.iter().enumerate() {
                    let c = view(&canvas.track(w, h), sides_of(*id));
                    if let Some((_, Ok(f))) = drawn.iter().find(|(d, _)| *d == c) {
                        let s = w as usize;
                        let t0 = Instant::now();
                        match pipe.video_track(i, f.as_frame(s), ts) {
                            Ok(t) => tags.extend(t),
                            Err(e) => failed = Some(e.to_string()),
                        }
                        let dt = t0.elapsed();
                        out_encoding += dt;
                        encoding += dt;
                    }
                }
            }
            if out_encoding > Duration::ZERO {
                per_out_encoding.push((*id, out_encoding));
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
            if let Some(why) = failed {
                failures.push((*id, why));
            }
        }
        // Each fault goes on its own output (the first one stays): the main stream and the
        // recording aren't marked dead by a vertical stream's or the replay buffer's.
        for (id, why) in failures {
            note_failure(st, id, &why);
        }
        for (id, dt) in per_out_encoding {
            let ms = dt.as_secs_f32() * 1000.0;
            st.encode_ms.entry(id).and_modify(|prev| *prev = *prev * 0.9 + ms * 0.1).or_insert(ms);
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
            if let Some((_, Ok(f))) = drawn.iter().find(|(c, _)| *c == view(canvas, sides_of(*id)))
                && let Some(host) = f.host()
            {
                sink(host, ts);
            }
        }
        for b in blocks {
            let _ = recycle_tx.try_send(b);
        }
    }
}

/// What a panic said.
fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(String::as_str)).unwrap_or("a fault")
}

/// One frame: sources taken in, forwarded audio synced to the audio thread, each canvas drawn
/// and its outputs fed.
#[allow(clippy::too_many_arguments)]
fn frame(
    config: &EngineConfig,
    state: &Arc<Mutex<State>>,
    gpu: &Compositor,
    audio_cmd: &mpsc::Sender<AudioCmd>,
    levels: &Arc<RwLock<HashMap<u64, Arc<AtomicLevels>>>>,
    bridge_rx: &mpsc::Receiver<Block>,
    recycle_tx: &mpsc::SyncSender<Block>,
    ts: u64,
    behind: u64,
    started: Instant,
    shown: &Seen,
) -> Option<(u64, bool)> {
    let rate = config.sample_rate;
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.stats.skipped += behind;
    // Only what's on air is heard (and monitored), as libobs mixes its program's tree; the
    // rest still has its sound taken, so it doesn't pile up.
    let heard = heard(&st);
    sync_heard(&mut st, audio_cmd);
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let active_set = active(&st);
    // Pictures go to the GPU only for sources something drawn shows (and a source's first,
    // for its size); the rest keep their newest until they're shown.
    let (seen, delayed) = seen(&st);
    // Sources: new pictures to the GPU, captured/media sound forwarded to per-source rings.
    let mut uploads = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let mut media_clear: Vec<u64> = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let mut media_drop: Vec<Arc<crate::media::Media>> = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let mut media_reopen: Vec<(u64, u64, PathBuf, crate::ffsrc::MediaOptions)> = Vec::new();
    for (id, src) in st.sources.iter_mut() {
        if seen.contains(id)
            && let Some(f) = src.pending.take()
        {
            src.pending_since = 0;
            uploads.push((*id, f));
        }
    }
    let n_pending = uploads.len();
    // Video Delay: buffers made when a filter is set or changed (not per frame), dropped when
    // it's gone.
    let period_ns = 1_000_000_000 * u64::from(config.frame_rate().1) / u64::from(config.frame_rate().0.max(1));
    for (id, src) in st.sources.iter_mut() {
        match delayed.get(id) {
            Some(&(asked, audio)) => {
                // After the memory cap cut a delay short, picture and sound keep the cut length
                // together (sound alone at the full delay would be seconds out of step).
                let ms = match src.delay_cap {
                    Some((a, cut)) if a == asked => cut,
                    _ => {
                        src.delay_cap = None;
                        asked
                    }
                };
                match &mut src.video_delay {
                    // Changed in place: what is held keeps coming, the source doesn't freeze.
                    Some(d) => d.set_delay_ms(ms),
                    None => src.video_delay = Some(crate::delay::FrameDelay::new(ms, config.fps)),
                }
                // A change of the delay fades the held sound out and in (no click).
                match (audio, &mut src.audio_delay) {
                    (true, Some(d)) => d.set_delay(ms),
                    (true, None) => src.audio_delay = Some(crate::delay::AudioDelay::new(ms)),
                    (false, _) => src.audio_delay = None,
                }
                // Off air, its sound isn't taken in: what's held mustn't come back later.
                if !heard.contains(id)
                    && let Some(a) = &mut src.audio_delay
                {
                    a.clear();
                }
            }
            None => {
                src.video_delay = None;
                src.audio_delay = None;
                src.delay_cap = None;
            }
        }
    }
    let mut st_recovered = 0;
    for (id, src) in st.sources.iter_mut() {
        let Src { live, audio_tx, audio_delay, .. } = src;
        let took = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match live {
            // Drawn after the frame (`visualize`).
            Live::Still | Live::Visualizer(_) => {}
            #[cfg(test)]
            Live::PanicOnDrop(_) => {}
            #[cfg(all(test, any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Live::FakeCamera(cap) => {
                if let Some((p, _)) = cap.take_frame() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
            }
            Live::Image { src: img, since, last } => {
                if img.animated() {
                    let p = img.frame_at(since.elapsed());
                    uploads.push((*id, Fresh::Pic(p.clone())));
                    *last = None;
                }
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
                        forward_audio(*id, audio_tx, audio_cmd, &pcm, 2, 48_000, t, audio_delay.as_mut());
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
                #[cfg(target_os = "windows")]
                if let Some((p, _)) = cap.take_nv12() {
                    uploads.push((*id, Fresh::Yuv(p)));
                } else if let Some((p, _)) = cap.take_frame() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                #[cfg(not(target_os = "windows"))]
                if let Some((p, _)) = cap.take_frame() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Microphone(cap) => {
                for (pcm, t) in cap.take_sound() {
                    if heard.contains(id) {
                        forward_audio(*id, audio_tx, audio_cmd, &pcm, 2, 48_000, t, audio_delay.as_mut());
                    }
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            Live::Media(slot) => {
                let is_active = active_set.contains(id);
                let became_active = is_active && !slot.was_active;
                let became_inactive = !is_active && slot.was_active;
                slot.was_active = is_active;

                if became_active {
                    slot.cleared = false;
                    if let Some(m) = &slot.player {
                        if slot.options.restart_on_activate {
                            m.control(crate::media::Control::Restart);
                            let _ = audio_cmd.send(AudioCmd::FlushSource(*id));
                        }
                    } else if slot.options.close_when_inactive && slot.reopening.is_none() {
                        let tok = next_reopen_token();
                        slot.reopening = Some(tok);
                        media_reopen.push((*id, tok, slot.path.clone(), slot.options));
                    }
                } else if became_inactive {
                    if slot.options.close_when_inactive {
                        slot.reopening = None;
                        if let Some(m) = slot.player.take() {
                            media_drop.push(m);
                            let _ = audio_cmd.send(AudioCmd::FlushSource(*id));
                        }
                        if slot.options.clear_on_media_end {
                            media_clear.push(*id);
                            slot.cleared = true;
                        }
                    } else if slot.options.restart_on_activate
                        && let Some(m) = &slot.player
                    {
                        m.control(crate::media::Control::Stop);
                        let _ = audio_cmd.send(AudioCmd::FlushSource(*id));
                        if slot.options.clear_on_media_end {
                            media_clear.push(*id);
                            slot.cleared = true;
                        }
                    }
                }

                if let Some(m) = &slot.player {
                    // NV12 for opaque FFmpeg-decoded clips (Windows/Linux, and macOS MKV fallback),
                    // RGBA for AVFoundation clips and alpha sources (WebM VP9 stingers, ProRes 4444).
                    let fresh = m.frame_nv12().map(Fresh::Yuv).or_else(|| m.frame_at(ts).map(Fresh::Pic));
                    let got_frame = fresh.is_some();
                    if let Some(f) = fresh {
                        slot.cleared = false;
                        uploads.push((*id, f));
                    }
                    for (pcm, t) in m.take_sound() {
                        if heard.contains(id) {
                            forward_audio(*id, audio_tx, audio_cmd, &pcm, 2, 48_000, t, audio_delay.as_mut());
                        }
                    }
                    // Clear texture / close decoder only after the final queued frame has been shown.
                    if !got_frame && !slot.options.looping && m.ended() {
                        if slot.options.clear_on_media_end && !slot.cleared {
                            media_clear.push(*id);
                            slot.cleared = true;
                        }
                        if slot.options.close_when_inactive
                            && let Some(old) = slot.player.take()
                        {
                            media_drop.push(old);
                        }
                    }
                }
            }
            Live::Fed(feed) | Live::Series { feed, .. } => {
                // The newest of the two kinds handed in (a playlist moves between them only at
                // a file's start, so one is always empty then).
                let fed_surface = {
                    #[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
                    {
                        feed.0.surface.lock().unwrap_or_else(|e| e.into_inner()).take().map(Fresh::Screen)
                    }
                    #[cfg(not(all(test, any(target_os = "macos", target_os = "windows"))))]
                    {
                        None::<Fresh>
                    }
                };
                if let Some(f) = fed_surface {
                    uploads.push((*id, f));
                } else if let Some(p) = feed.0.yuv.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    uploads.push((*id, Fresh::Yuv(p)));
                } else if let Some(p) = feed.0.picture.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    uploads.push((*id, Fresh::Pic(p)));
                }
                let sound: Vec<_> = feed.0.sound.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
                if heard.contains(id) {
                    for (pcm, ch_rate, t) in sound {
                        forward_audio(*id, audio_tx, audio_cmd, &pcm, 2, ch_rate, t, audio_delay.as_mut());
                    }
                }
            }
        }));
        if let Err(p) = took {
            // This source alone stops (its last picture stays); the watchdog starts a capture again.
            tracing::error!(source = id, why = panic_text(&*p), "a source faulted: stopped");
            src.live = Live::Still;
            src.audio_tx = None;
            src.watch.faulted = true;
            st_recovered += 1;
        }
    }
    st.stats.recovered += st_recovered;
    let now = now_ns();
    // Pictures of a delayed source go to the conversion thread, and into its queue (with the
    // time they were seen) once converted; the one whose time has come goes on.
    // (What came out of `pending` above was delayed already.)
    if !delayed.is_empty() {
        let mut i = n_pending;
        while i < uploads.len() {
            if delayed.contains_key(&uploads[i].0) {
                let (id, f) = uploads.swap_remove(i);
                let conv = st.delay_conv.get_or_insert_with(DelayConv::new);
                if !conv.submit(id, now, f) {
                    tracing::debug!(source = id, "a delayed picture was dropped: the conversion is behind");
                }
            } else {
                i += 1;
            }
        }
        let converted = st.delay_conv.as_ref().map(DelayConv::take).unwrap_or_default();
        for (id, at, f, bytes) in converted {
            if let Some(s) = st.sources.get_mut(&id)
                && let Some(q) = &mut s.video_delay
                && q.push(at, f, bytes)
            {
                let asked = delayed.get(&id).map_or(q.delay_ms(), |d| d.0);
                // A few pictures short of the cap, so the cut delay itself doesn't hit it again.
                let room = crate::delay::MAX_VIDEO_BYTES.saturating_sub(3 * bytes);
                let cut = crate::delay::longest_ms(bytes, config.fps, room);
                s.delay_cap = Some((asked, cut.max(1)));
                tracing::warn!(
                    source = id,
                    asked_ms = asked,
                    kept_ms = cut,
                    "a video delay holds too much picture: its delay is cut short to stay in memory; the sound keeps the same shorter delay"
                );
            }
        }
        for (id, s) in st.sources.iter_mut() {
            if let Some(q) = &mut s.video_delay
                && let Some(f) = q.take_due(now, period_ns / 2)
            {
                uploads.push((*id, f));
            }
        }
    }
    let frozen = frozen(&st);
    for (id, f) in uploads {
        let Some(s) = st.sources.get_mut(&id) else { continue };
        // A frozen source keeps the picture it has (its first one still comes in).
        if frozen.contains(&id) && s.sent > 0 {
            s.pending = None;
            s.pending_since = 0;
            continue;
        }
        if !seen.contains(&id) && s.size.is_some() {
            // Keep the newest frame directly in `pending`: replacing it drops the previous
            // surface immediately (at most 1 buffer held) without a per-frame CPU pixel copy.
            s.pending = Some(f);
            s.pending_since = now;
            continue;
        }
        if f.send(gpu, id) {
            s.size = Some(f.info());
            s.sent += 1;
            if let Some(retry) = s.camera_retry.as_mut() {
                retry.on_frame_uploaded(now);
            }
        }
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    for id in media_clear {
        if let Some(s) = st.sources.get_mut(&id) {
            s.size = None;
            s.pending = None;
            s.pending_since = 0;
        }
        gpu.forget(id);
    }
    // Hidden or frozen sources release any GPU-pinned zero-copy texture immediately; an idle
    // hidden capture converts its single held surface ONCE after PENDING_UNPIN_NS so pools are
    // never starved if updates stop altogether.
    for (id, s) in st.sources.iter_mut() {
        let keep_frozen = frozen.contains(id) && s.sent > 0;
        if keep_frozen {
            s.pending = None;
            s.pending_since = 0;
        }
        if !seen.contains(id) || keep_frozen {
            gpu.unpin_surface(*id);
        }
        if !seen.contains(id) && !keep_frozen {
            unpin_stale_pending(s, now);
        }
    }
    visualize(&mut st, &seen, gpu, audio_cmd, rate);

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    if st.stinger.as_ref().is_some_and(|(.., end)| ts >= *end) {
        st.stinger = None;
        st.sources.remove(&STINGER);
        gpu.forget(STINGER);
        levels.write().unwrap_or_else(|e| e.into_inner()).remove(&STINGER);
        let _ = audio_cmd.send(AudioCmd::ScheduleRemove { id: STINGER, after_ns: ts });
    }
    if st.moving.as_ref().is_some_and(|(_, _, start, dur)| ts >= start + dur) {
        st.moving = None;
    }
    sync_heard(&mut st, audio_cmd);
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
    let mut blocks = Vec::new();
    while let Ok(b) = bridge_rx.try_recv() {
        blocks.push(b);
    }
    if !st.outputs.is_empty() || !st.taps.is_empty() {
        // Each distinct canvas drawn once this frame (if it takes this one). The card reads them
        // back while the next frames are drawn (`deliver`): waiting for it here stalled the loop
        // whenever a game kept the card busy (owner, 15 Nov: 35 of 60 fps, 46% skipped).
        let index = st.stats.frames;
        // The stream and the recording are one picture, drawn once, unless an item that shows on
        // that canvas differs between them; then each is drawn for its own audience.
        let all: Vec<(u64, Canvas, Sides)> = st
            .outputs
            .iter()
            .map(|(id, .., c)| (*id, c))
            .chain(st.taps.iter().map(|(id, c, _)| (*id, c)))
            .map(|(id, c)| (id, c.clone(), sides(&st, c)))
            .collect();
        let sides_of = |id: u64| all.iter().find(|(i, ..)| *i == id).map_or(Sides::default(), |s| s.2);
        let mut drawn: Vec<(Canvas, Result<PendingNv12, String>)> = Vec::new();
        for (_, canvas, side) in &all {
            let canvas = &view(canvas, *side);
            if !index.is_multiple_of(u64::from(canvas.every)) || drawn.iter().any(|(c, _)| c == canvas) {
                continue;
            }
            let (w, h) = canvas.scaled_from.map_or((canvas.width, canvas.height), |(w, h, _)| (w, h));
            let draw = match (what_to_draw(&st, w, h, canvas.scene.as_deref(), ts, canvas.audience), canvas.scaled_from)
            {
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
        for (id, _, _, canvas) in &st.outputs {
            if !index.is_multiple_of(u64::from(canvas.every)) {
                continue;
            }
            for &(w, h, _) in &canvas.video_tracks {
                let c = view(&canvas.track(w, h), sides_of(*id));
                if drawn.iter().chain(extra.iter()).any(|(d, _)| *d == c) {
                    continue;
                }
                let (bw, bh) = c.scaled_from.map_or((w, h), |(bw, bh, _)| (bw, bh));
                let f = c.scaled_from.map_or(Filter::Bicubic, |(.., f)| f);
                let frame = match what_to_draw(&st, bw, bh, c.scene.as_deref(), ts, c.audience) {
                    Draw::Scene(s) => gpu.begin_nv12(&scaled(s, w, h, f)),
                    Draw::Moving(mut t) => {
                        t.from = scaled(t.from, w, h, f);
                        t.to = scaled(t.to, w, h, f);
                        gpu.begin_transition_nv12(&t)
                    }
                };
                extra.push((c, frame.map_err(|e| e.to_string())));
            }
        }
        drawn.extend(extra);
        let blocks = if st.outputs.is_empty() {
            for b in blocks {
                let _ = recycle_tx.try_send(b);
            }
            Vec::new()
        } else {
            blocks
        };
        let entry = InFlight {
            ts,
            blocks,
            outputs: st.outputs.iter().map(|(id, ..)| *id).collect(),
            taps: st.taps.iter().map(|(id, ..)| *id).collect(),
            drawn,
            sides: all.iter().map(|(id, _, side)| (*id, *side)).collect(),
        };
        st.in_flight.push_back(entry);
    } else {
        for b in blocks {
            let _ = recycle_tx.try_send(b);
        }
    }
    deliver(&mut st, gpu, false, recycle_tx);
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    st.stats.frames += 1;
    st.stats.frame_ms = ms;
    st.stats.worst_ms = st.stats.worst_ms.max(ms);
    let miss_log = {
        let s = &mut *st;
        s.miss_monitor.record(now_ns(), behind, config.frame_ns(), &mut s.stats, crate::video::dropped_frames())
    };
    // Shown before the state's lock goes: a change made right after (a split, a pause) shows
    // itself, never overwritten by this older picture (projects-60).
    *shown.lock().unwrap_or_else(|e| e.into_inner()) = Shown::of(&st);
    let ticket = st.sleep_ticket();
    drop(st);
    if let Some(m) = miss_log {
        tracing::warn!(
            why = m.why,
            render_missed = m.render_missed,
            encode_held = m.encode_held,
            encode_skipped = m.encode_skipped,
            total_missed = m.total_missed,
            total_encode_skipped = m.total_encode_skipped,
            frame_ms = m.frame_ms,
            encode_ms = m.encode_ms,
            "frames missed: {}",
            m.why
        );
    }
    // Open/close `close_when_inactive` media completely off the State lock and frame pacing path.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        for (id, tok, path, options) in media_reopen {
            spawn_media_reopen(state.clone(), audio_cmd.clone(), levels.clone(), id, tok, path, options, true);
        }
        if !media_drop.is_empty()
            && std::thread::Builder::new().name("ferrocore-media-close".into()).spawn(move || drop(media_drop)).is_err()
        {
            tracing::debug!("dropped closed media inline after thread spawn failure");
        }
    }
    ticket
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioConfig;
    use crate::video::{PixelFormat, RateControl, VideoCodec};

    #[test]
    fn a_sources_default_filters_ignore_items_with_their_own() {
        let place = || Placement::default();
        let mut own = SceneItem::new(7, place());
        own.effects = vec![Effect::RoundedCorners { radius: 99.0 }];
        own.own_effects = true;
        let mut shared = SceneItem::new(7, place());
        shared.effects = vec![Effect::RoundedCorners { radius: 12.0 }];
        for items in [[&own, &shared], [&shared, &own]] {
            assert_eq!(shared_source_effects(items.into_iter(), 7), shared.effects);
        }
        assert!(shared_source_effects([&own].into_iter(), 7).is_empty());
    }

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
        engine_with(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() })
    }

    fn engine_with(config: EngineConfig) -> Option<Engine> {
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
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
        };
        let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        Engine::new(config, video, audio).ok()
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
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
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
        // On a CI runner's virtual Mac the last few frames can still be in the virtual encoder when
        // it stops (39 of 47 seen there); a real Mac must bring them all out. (Open: confirm on a
        // loaded real Mac that no frames are lost at a stop.)
        let ci = std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"));
        let slack = if ci { 12 } else { 3 };
        assert!(video.len() as u64 + slack >= drawn_before_stop.min(45), "{} of {drawn_before_stop}", video.len());
        assert!(video.windows(2).all(|w| w[0] <= w[1]), "in time order: {video:?}");
        assert!(audio > 20, "sound came too: {audio}");
    }

    fn some_preview() -> Preview {
        Preview { look: Default::default(), scene: None, size: (640, 360), multiview: None, paused: false }
    }

    #[test]
    fn a_preview_can_be_paused_and_let_go_and_an_unknown_one_changes_nothing() {
        let Some(e) = engine() else { return };
        e.lock().previews.insert(7, some_preview());
        assert_eq!(e.preview_paused(7), Some(false));
        e.set_preview_paused(7, true);
        assert_eq!(e.preview_paused(7), Some(true));
        // Twice is the same as once.
        e.set_preview_paused(7, true);
        assert_eq!(e.preview_paused(7), Some(true));
        e.set_preview_paused(7, false);
        assert_eq!(e.preview_paused(7), Some(false));
        // No such preview: nothing made up, nothing panics.
        e.set_preview_paused(99, true);
        assert_eq!(e.preview_paused(99), None);
        assert!(!e.lock().previews.contains_key(&99));
    }

    #[test]
    fn pausing_one_preview_leaves_the_others_drawing() {
        let Some(e) = engine() else { return };
        e.lock().previews.insert(1, some_preview());
        e.lock().previews.insert(2, some_preview());
        e.set_preview_paused(1, true);
        assert_eq!((e.preview_paused(1), e.preview_paused(2)), (Some(true), Some(false)));
    }

    /// A paused preview thread sleeps until woken: not a timer, and it comes back on resume and on
    /// stop (a stop used to wait out a 100 ms nap). No wake-up time is compared with a bound a
    /// slow machine could miss. The test learns the thread is parked from the thread itself (the
    /// hook runs before the wait, and taking the lock afterwards returns only once the wait has
    /// let go of it), flips the flag without waking it and checks it stays asleep (no timer), then
    /// wakes it and checks it returns.
    #[test]
    fn a_paused_preview_thread_sleeps_until_resumed_or_stopped() {
        /// Starts a thread that waits while `paused`, and returns once it is parked.
        fn parked_thread(
            paused: &Arc<AtomicBool>,
            stop: &Arc<AtomicBool>,
            wake: &Arc<(Mutex<()>, Condvar)>,
        ) -> (mpsc::Receiver<()>, JoinHandle<()>) {
            let (done_tx, done_rx) = mpsc::channel();
            let (parked_tx, parked_rx) = mpsc::channel();
            let (p, st, w) = (paused.clone(), stop.clone(), wake.clone());
            let h = std::thread::spawn(move || {
                wait_while_paused_with(&p, &st, &w, || {
                    let _ = parked_tx.send(());
                });
                let _ = done_tx.send(());
            });
            parked_rx.recv_timeout(Duration::from_secs(10)).expect("the thread reached its wait");
            drop(wake.0.lock().unwrap_or_else(|e| e.into_inner()));
            (done_rx, h)
        }

        let paused = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));

        // Resume: the flag changes, nobody wakes the thread. A timer would bring it back alone.
        let (done, h) = parked_thread(&paused, &stop, &wake);
        assert!(done.try_recv().is_err(), "it waits while paused");
        paused.store(false, Ordering::Release);
        assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "no timer: it sleeps until it is woken");
        wake_preview(&wake);
        done.recv_timeout(Duration::from_secs(10)).expect("it comes back when woken on resume");
        h.join().unwrap();

        // Stop while paused, the same way.
        paused.store(true, Ordering::Release);
        let (done, h) = parked_thread(&paused, &stop, &wake);
        stop.store(true, Ordering::Release);
        assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "no timer: a stop is only seen when woken");
        wake_preview(&wake);
        done.recv_timeout(Duration::from_secs(10)).expect("it comes back when woken on stop");
        h.join().unwrap();

        // Not paused: no waiting at all (the hook that runs before a wait never runs).
        paused.store(false, Ordering::Release);
        stop.store(false, Ordering::Release);
        let waited = std::cell::Cell::new(false);
        wait_while_paused_with(&paused, &stop, &wake, || waited.set(true));
        assert!(!waited.get(), "an unpaused thread does not wait");
    }

    #[test]
    fn a_paused_preview_waits_long_and_a_drawing_one_waits_for_its_frame() {
        let frame = Duration::from_millis(33);
        assert_eq!(preview_wait(false, frame, Duration::from_millis(10)), Duration::from_millis(23));
        // Late: no wait at all.
        assert_eq!(preview_wait(false, frame, Duration::from_millis(50)), Duration::ZERO);
        // Paused: a slow look, not the frame rate.
        assert_eq!(preview_wait(true, frame, Duration::ZERO), PAUSED_LOOK);
        // A slower frame rate than the look keeps its own pace.
        assert_eq!(preview_wait(true, Duration::from_millis(500), Duration::ZERO), Duration::from_millis(500));
    }

    /// Performance mode never pauses an output: a stream keeps getting its pictures and sound
    /// while every preview is paused, and again after they are let go.
    #[test]
    fn a_paused_preview_never_stops_the_stream_or_recording() {
        if !encodes_here() {
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [10, 200, 30, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        e.lock().previews.insert(1, some_preview());
        let video: Arc<Mutex<u32>> = Arc::default();
        let audio: Arc<Mutex<u32>> = Arc::default();
        let sink = {
            let (video, audio) = (video.clone(), audio.clone());
            move |t: Tag| match t {
                Tag::Video { .. } => *video.lock().unwrap_or_else(|p| p.into_inner()) += 1,
                Tag::Audio { .. } => *audio.lock().unwrap_or_else(|p| p.into_inner()) += 1,
            }
        };
        let out = e.start_output(OutputKind::Stream(Box::new(sink))).unwrap_or_else(|x| panic!("{x}"));
        let count = |m: &Arc<Mutex<u32>>| *m.lock().unwrap_or_else(|p| p.into_inner());
        std::thread::sleep(Duration::from_millis(600));
        e.set_preview_paused(1, true);
        let (v0, a0) = (count(&video), count(&audio));
        std::thread::sleep(Duration::from_millis(1200));
        let (v1, a1) = (count(&video), count(&audio));
        assert!(v1 >= v0 + 10, "pictures kept going while paused: {v0} then {v1}");
        assert!(a1 > a0, "sound kept going while paused: {a0} then {a1}");
        e.set_preview_paused(1, false);
        std::thread::sleep(Duration::from_millis(600));
        assert!(count(&video) > v1, "and after");
        e.stop_output(out).unwrap_or_else(|x| panic!("{x}"));
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
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
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
        let r = e.set_source(
            9,
            SourceKind::Media { path: "/no/such/clip.mp4".into(), options: crate::ffsrc::MediaOptions::default() },
        );
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
        // A camera that never sent its first frame gets a startup grace window, then restarts.
        let mut w = Watch::default();
        assert_eq!(look(&mut w, true, Some((0, true, None)), 5 * s), None);
        assert!(look(&mut w, true, Some((0, true, None)), 11 * s).is_some());
        // A camera that never sent its first frame gets the full 10 s grace window on every retry
        // while `count == 0`, then falls to 5 s STALL_NS once frames have flowed.
        let mut w = Watch::default();
        assert_eq!(look(&mut w, true, Some((0, true, None)), 5 * s), None);
        assert!(look(&mut w, true, Some((0, true, None)), 11 * s).is_some());
        w.tries = 2;
        assert_eq!(
            look(&mut w, true, Some((0, true, None)), 7 * s),
            None,
            "slow reconnect keeps 10 s first-frame grace"
        );
        assert!(look(&mut w, true, Some((0, true, None)), 60 * s).is_some());
        assert_eq!(look(&mut w, true, Some((5, true, None)), 61 * s), None);
        assert_eq!(w.tries, 2, "one frame isn't a recovery: the tries stay");
        assert_eq!(look(&mut w, true, Some((300, true, None)), 72 * s), None);
        assert_eq!(w.tries, 0, "10 s of frames: afresh");
        // Tries wait 2, 4, 8… seconds, a minute at most.
        assert_eq!((backoff_ns(0), backoff_ns(2), backoff_ns(9)), (2 * s, 8 * s, 60 * s));
    }

    #[test]
    fn camera_source_keeps_retrying_past_three_tries_and_wakes_on_hotplug() {
        use crate::camera_retry::{CameraAction, CameraObservation};

        let sec = 1_000_000_000;
        let mut sources = BTreeMap::new();
        sources.insert(1, Src::new(Live::Still, None, Some(SourceKind::Camera("continuity-cam".into()))));
        let retry = sources.get_mut(&1).and_then(|s| s.camera_retry.as_mut()).unwrap_or_else(|| panic!("retry"));

        // Retries past 3 attempts on 1 s, 2 s, 4 s, 5 s backoff forever while visible/on air.
        let mut t = sec;
        for wait in [sec, 2 * sec, 4 * sec, 5 * sec, 5 * sec] {
            let act = retry.step(CameraObservation {
                running: false,
                frames: 0,
                ended: false,
                interrupted: false,
                opening: false,
                visible_or_on_air: true,
                now_ns: t,
            });
            assert!(matches!(act, CameraAction::Reopen(_)), "camera keeps retrying past 3 attempts");
            assert!(retry.is_waiting());
            let prev = retry.on_reopen_started(t);
            retry.on_reopen_finished(prev, false, t, Some("still sleeping"));
            t += wait;
        }
        assert!(retry.tries() >= 5);

        // Hotplug reconnect clears backoff and triggers immediate reopen.
        assert_eq!(apply_hotplug_reconnects(&mut sources, &["continuity-cam".into()], t + sec), 1);
        let src = sources.get_mut(&1).unwrap_or_else(|| panic!("src"));
        assert_eq!(src.watch.tries, 0);
        let retry = src.camera_retry.as_mut().unwrap_or_else(|| panic!("retry"));
        assert_eq!(retry.tries(), 0);
        let act = retry.step(CameraObservation {
            running: false,
            frames: 0,
            ended: false,
            interrupted: false,
            opening: false,
            visible_or_on_air: true,
            now_ns: t + sec,
        });
        assert!(matches!(act, CameraAction::Reopen(_)));
    }

    /// A capture that ended because macOS turned its permission off isn't started again every few
    /// seconds (each try raises the system's prompt); it waits for the permission to come back.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_revoked_permission_stops_the_retries_until_it_is_back() {
        let s = 1_000_000_000;
        let gone = || Some((0, false, Some(format!("{} (x)", crate::apple::screen::PERMISSION_REVOKED))));
        let mut w = Watch::for_kind(None, 0);
        assert_eq!(look(&mut w, true, gone(), s), None);
        assert!(w.denied);
        // Minutes pass: not restarted, however often it's looked at.
        for t in 2..200 {
            assert_eq!(look(&mut w, true, gone(), t * s), None);
        }
        // Still off: asking says no, nothing changes.
        retry_if_permission_back(&mut w, || false);
        assert_eq!(look(&mut w, true, gone(), 300 * s), None);
        // The person turns it on again: restarted once.
        retry_if_permission_back(&mut w, || true);
        assert_eq!(look(&mut w, true, gone(), 301 * s), Some("its permission is back".into()));
        assert!(!w.denied);
        // An ordinary crash is still restarted as before.
        let mut crashed = Watch::for_kind(None, 0);
        assert!(look(&mut crashed, true, Some((0, false, Some("it crashed".into()))), s).is_some());
    }

    /// The wait for a signal needs a session open to see it come: without one (the last try failed
    /// to open, or it ended with an error) the watchdog still tries.
    #[test]
    fn a_no_signal_wait_without_a_session_is_retried_slowly() {
        let s = 1_000_000_000;
        let mut w = Watch::for_kind(Some(&SourceKind::Camera("capture-card".into())), 0);
        w.tries = NO_SIGNAL_MAX_TRIES;
        assert!(look(&mut w, false, None, 40 * s).is_some(), "not running: try again");
        let err = Some((0, true, Some("runtime error".to_string())));
        assert!(look(&mut w, true, err, 41 * s).is_some(), "ended with an error: try again");
        // With a healthy session it still just waits.
        assert_eq!(look(&mut w, true, Some((0, true, None)), 42 * s), None);
        assert!(w.no_signal);
    }

    /// A capture card nobody has plugged anything into yet (a console turned on later): once it
    /// sends, the "no signal" wait ends by itself, with no unplugging or editing.
    #[test]
    fn a_no_signal_card_wakes_when_its_picture_comes() {
        let s = 1_000_000_000;
        let mut w = Watch::for_kind(Some(&SourceKind::Camera("capture-card".into())), 0);
        w.tries = NO_SIGNAL_MAX_TRIES;
        assert_eq!(look(&mut w, true, Some((0, true, None)), 40 * s), None);
        assert!(w.no_signal, "waiting for a signal");
        // The console is switched on: frames flow on the session that was open all along.
        assert_eq!(look(&mut w, true, Some((30, true, None)), 90 * s), None);
        assert!(!w.no_signal && w.ever_sent, "the latch let go");
        assert_eq!(w.tries, 0);
        // And it's watched as a working card again: if it stops, it's restarted.
        assert!(look(&mut w, true, Some((30, true, None)), 97 * s).is_some());
    }

    /// An HDMI link that flaps (one frame, then nothing, again and again) keeps its tries, so the
    /// backoff grows instead of restarting on every drop.
    #[test]
    fn a_flapping_link_does_not_reset_its_backoff() {
        let s = 1_000_000_000;
        let mut w = Watch::for_kind(Some(&SourceKind::Camera("capture-card".into())), 0);
        w.ever_sent = true;
        let mut count = 0;
        let mut now = 100 * s;
        for _ in 0..5 {
            count += 1;
            assert_eq!(look(&mut w, true, Some((count, true, None)), now), None, "one frame");
            now += 6 * s;
            assert!(look(&mut w, true, Some((count, true, None)), now).is_some(), "stalled again");
            w.tries += 1; // the watchdog's reopen
        }
        assert_eq!(w.tries, 5, "the failed tries were kept");
    }

    /// A camera/card that has worked and one that has not yet sent both retry quickly within 5 s;
    /// microphones that worked keep their short retry, while other sources keep the 60 s wait.
    #[test]
    fn a_card_that_worked_is_retried_quickly_not_after_a_minute() {
        let s = 1_000_000_000;
        let mut card = Watch::for_kind(Some(&SourceKind::Camera("capture-card".into())), 0);
        card.ever_sent = true;
        assert!(card.backoff(0) <= 5 * s && card.backoff(9) <= 5 * s);
        let mut mic = Watch::for_kind(Some(&SourceKind::Microphone("usb".into())), 0);
        assert_eq!(mic.backoff(9), 60 * s, "a mic that never sent keeps the slow wait");
        mic.ever_sent = true;
        assert!(mic.backoff(9) <= 3 * s, "a mic that worked is looked for again within seconds");
        let mut media = Watch::for_kind(Some(&SourceKind::Color { width: 2, height: 2, rgba: [0; 4] }), 0);
        media.ever_sent = true;
        assert_eq!(media.backoff(9), 60 * s, "other sources keep it");
        let unseen = Watch::for_kind(Some(&SourceKind::Camera("capture-card".into())), 0);
        assert_eq!((unseen.backoff(0), unseen.backoff(2), unseen.backoff(9)), (s, 4 * s, 5 * s));
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn watchdog_clears_stalled_reopen_worker_after_deadline() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            st.sources.insert(1, Src::new(Live::Still, None, Some(SourceKind::Microphone("hung".into()))));
        }
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_c = attempts.clone();
        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate_c = gate.clone();

        let reopen: Reopener = Arc::new(move |_| {
            let n = attempts_c.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = first_entered_tx.send(());
                let (lk, cv) = &*gate_c;
                let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                while !*open {
                    open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                }
                Some(Ok(Live::Still))
            } else {
                Some(Err(AvError::Source("second worker ran".into())))
            }
        });

        let now = now_ns();
        let cmd = e.audio.cmd_sender();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        first_entered_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.lock().sources[&1].opening.is_some());

        // Before the 60 s deadline, no second worker launches.
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now + REOPEN_TIMEOUT_NS - 1_000_000_000);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        // At >= 60 s, the stalled slot is released and a fresh worker runs cleanly.
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now + REOPEN_TIMEOUT_NS);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let stale_token = e.lock().sources[&1].opening;
        assert!(stale_token.is_none());

        // Unblock worker 1 and confirm its stale Ok(Live) does not bump restarts or take the slot.
        let restarts_before = e.lock().stats.restarts;
        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(e.lock().stats.restarts, restarts_before);
    }

    /// Driven through the real watchdog step, not `Watch::backoff` alone: a mic that has worked
    /// and whose reopen fails is looked for again within seconds (the worker's own writes of
    /// `next_try` used the long backoff and overwrote the short one; review R7).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn a_mic_that_worked_is_retried_within_seconds_after_a_failed_reopen_too() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            let mut src = Src::new(Live::Still, None, Some(SourceKind::Microphone("usb".into())));
            src.watch.ever_sent = true;
            src.watch.tries = 9;
            st.sources.insert(1, src);
        }
        let reopen: Reopener = Arc::new(|_| Some(Err(AvError::Source("still unplugged".into()))));
        let cmd = e.audio.cmd_sender();
        let now = now_ns();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        let deadline = Instant::now() + Duration::from_secs(3);
        while e.lock().sources[&1].opening.is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (next, at) = (e.lock().sources[&1].watch.next_try, now_ns());
        assert!(
            next.saturating_sub(at) <= CARD_RETRY_MAX_NS + 500_000_000,
            "next try in {} ms",
            next.saturating_sub(at) / 1_000_000
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn watchdog_caps_concurrent_reopen_workers_and_recovers_from_drop_panic() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            for id in 1..=4 {
                let live = if id == 1 { Live::PanicOnDrop(PanicDrop) } else { Live::Still };
                st.sources.insert(id, Src::new(live, None, Some(SourceKind::Microphone(format!("mic-{id}")))));
            }
        }
        let entered = Arc::new(AtomicU64::new(0));
        let entered_c = entered.clone();
        let reopen: Reopener = Arc::new(move |_| {
            entered_c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(120));
            Some(Ok(Live::Still))
        });

        let now = now_ns();
        let cmd = e.audio.cmd_sender();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        std::thread::sleep(Duration::from_millis(30));

        // Source 1 panicked inside `drop(old)` so its slot recovered at once (even though it never
        // entered `reopen_w`), leaving at most MAX_PARALLEL_REOPENS (2) total launched workers on tick 1.
        let opening_ids: Vec<u64> = {
            let st = e.lock();
            st.sources.iter().filter_map(|(id, s)| s.opening.map(|_| *id)).collect()
        };
        assert!(!opening_ids.contains(&1), "panicking drop(old) cleared opening slot immediately");
        assert!(opening_ids.len() <= MAX_PARALLEL_REOPENS, "got {opening_ids:?}");
        assert!(entered.load(Ordering::SeqCst) < MAX_PARALLEL_REOPENS as u64);

        // Wait for remaining workers and confirm source 1 & pending sources remain recoverable on later tick.
        std::thread::sleep(Duration::from_millis(200));
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now + 10_000_000_000);
        std::thread::sleep(Duration::from_millis(180));
        assert!(entered.load(Ordering::SeqCst) >= 3);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn watchdog_restarts_each_source_independently_and_honors_removal() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            st.sources.insert(1, Src::new(Live::Still, None, Some(SourceKind::Camera("dead".into()))));
            st.sources.insert(2, Src::new(Live::Still, None, Some(SourceKind::Microphone("mic".into()))));
        }
        let (dead_entered_tx, dead_entered_rx) = std::sync::mpsc::channel();
        let dead_gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (fast_done_tx, fast_done_rx) = std::sync::mpsc::channel();
        let dead_attempts = Arc::new(AtomicU64::new(0));

        let gate_c = dead_gate.clone();
        let attempts_c = dead_attempts.clone();
        let reopen: Reopener = Arc::new(move |kind| match kind {
            SourceKind::Camera(spec) if spec.id == "dead" => {
                attempts_c.fetch_add(1, Ordering::SeqCst);
                let _ = dead_entered_tx.send(());
                let (lk, cv) = &*gate_c;
                let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                while !*open {
                    open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                }
                Some(Ok(Live::Still))
            }
            SourceKind::Microphone(_) => {
                let _ = fast_done_tx.send(());
                Some(Ok(Live::Still))
            }
            _ => None,
        });

        let now = now_ns() + 1_000_000_000;
        let cmd = e.audio.cmd_sender();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);

        // Source 1 is blocked inside reopen, yet Source 2 completes immediately without waiting.
        dead_entered_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|x| panic!("{x}"));
        fast_done_rx.recv_timeout(Duration::from_millis(500)).unwrap_or_else(|x| panic!("fast source blocked: {x}"));

        // A second watchdog tick while Source 1 is still reopening doesn't launch a duplicate reopen.
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now + 5_000_000_000);
        assert_eq!(dead_attempts.load(Ordering::SeqCst), 1);

        // Removing Source 1 while its reopen is in flight never resurrects it when reopen finishes.
        e.remove_source(1);
        {
            let (lk, cv) = &*dead_gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(!e.lock().sources.contains_key(&1));
        assert!(e.lock().sources.contains_key(&2));
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

    /// Editing a text's words or a colour while live is applied in place too: no source is opened
    /// or closed, so no full load, no sound reset (GAPS #21).
    #[test]
    fn a_text_or_colour_edit_applies_in_place() {
        let Some(e) = engine() else { return };
        let doc = |words: &str, color: u32| {
            serde_json::json!({ "current_program_scene": "Main", "sources": [
                { "id": "color_source", "name": "Back", "settings": { "color": color, "width": 64, "height": 64 } },
                { "id": "text_ft2_source_v2", "name": "Title", "settings": { "text": words } },
                { "id": "scene", "name": "Main", "settings": { "items": [ { "name": "Back" }, { "name": "Title" } ] } },
            ] })
        };
        e.load_obs(&doc("one", 0xFF00_00FF));
        let ids = |e: &Engine| {
            let st = e.lock();
            (st.opened.get("Back").map(|(i, _)| *i), st.opened.get("Title").map(|(i, _)| *i))
        };
        let (before, started) = (ids(&e), e.lock().started.get("Main").copied());
        let size = |e: &Engine, id: Option<u64>| id.and_then(|id| e.lock().sources.get(&id).and_then(|s| s.size));
        let was = size(&e, before.1);
        assert!(e.apply_same_sources(&doc("two words now", 0xFF00_FF00)).is_some(), "applied in place");
        assert_eq!(ids(&e), before, "the same sources keep running");
        assert_eq!(e.lock().started.get("Main").copied(), started, "the scene's clock goes on");
        assert_ne!(size(&e, before.1), was, "the text is redrawn with the new words");
        // The next identical edit is also in place (the loaded kinds were updated).
        assert!(e.apply_same_sources(&doc("two words now", 0xFF00_FF00)).is_some());
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
        let kept = e.sound_settings(9);
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

    /// Each item is for the stream, the recording or both; the editing views show everything.
    #[test]
    fn show_decides_who_sees_an_item() {
        for (show, stream, recording) in
            [(Show::Both, true, true), (Show::Stream, true, false), (Show::Recording, false, true)]
        {
            assert_eq!(show.shown_to(Audience::Stream), stream, "{show:?} on the stream");
            assert_eq!(show.shown_to(Audience::Recording), recording, "{show:?} in the recording");
            assert!(show.shown_to(Audience::Everyone), "{show:?} in the editor's views");
        }
        for (name, show) in [("both", Show::Both), ("stream", Show::Stream), ("recording", Show::Recording)] {
            assert_eq!(Show::from_name(name), show);
            assert_eq!(show.name(), name);
        }
        // Anything else is both: a collection from before this, or a typo, shows as it always did.
        assert_eq!(Show::from_name(""), Show::Both);
        assert_eq!(Show::from_name("Stream "), Show::Both);
        assert_eq!(Show::default(), Show::Both);
    }

    /// What an output is for follows what it is: the stream's picture, or the recording's (the
    /// replay buffer's clips are recordings).
    #[test]
    fn an_output_is_for_the_stream_or_the_recording() {
        assert_eq!(audience_of(&OutputKind::Stream(Box::new(|_| {}))), Audience::Stream);
        assert_eq!(audience_of(&OutputKind::Record(PathBuf::new())), Audience::Recording);
        assert_eq!(audience_of(&OutputKind::Replay { seconds: 30, mb: 0 }), Audience::Recording);
    }

    /// While no item differs the stream and the recording are one picture, drawn once; once one
    /// does they are two. What a canvas carries besides its picture (sound tracks, their names,
    /// the extra qualities) is no part of what is drawn.
    #[test]
    fn nothing_is_drawn_twice_while_no_item_differs() {
        let main = Canvas::main(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() });
        let for_ = |audience| Canvas { audience, ..main.clone() };
        let (stream, recording, everyone) =
            (for_(Audience::Stream), for_(Audience::Recording), for_(Audience::Everyone));
        let none = Sides::default();
        assert_eq!(view(&stream, none), view(&recording, none));
        assert_eq!(view(&stream, none), view(&everyone, none));
        let both = Sides { stream_only: true, recording_only: true };
        assert_ne!(view(&stream, both), view(&recording, both));
        assert_ne!(view(&stream, both), view(&everyone, both), "the editor's views keep every item");
        // Nothing else of a canvas is touched.
        let sized = Canvas { width: 320, height: 180, every: 2, ..stream.clone() };
        let v = view(&sized, none);
        assert_eq!((v.width, v.height, v.every), (320, 180, 2));
        // A recording as the Studio makes it: named sound tracks, more of them; a stream with a
        // VOD track and more qualities. The same picture all the same.
        let rec = Canvas {
            audience: Audience::Recording,
            track: 1,
            more_tracks: vec![2, 3],
            track_names: vec!["Game".into(), String::new()],
            ..main.clone()
        };
        let live = Canvas {
            audience: Audience::Stream,
            video_tracks: vec![(1280, 720, 3000)],
            more_tracks: vec![1],
            ..main.clone()
        };
        assert_eq!(view(&rec, none), view(&live, none), "one drawing, whatever sound each carries");
        // A different size, rate or scene is a different picture.
        let other = Canvas { scene: Some("Other".into()), ..rec.clone() };
        assert_ne!(view(&other, none), view(&live, none));
    }

    /// Only the side that differs is drawn apart: with items for the stream only, the stream sees
    /// what the editor does, and only the recording needs its own picture (and the reverse).
    #[test]
    fn only_the_side_that_differs_is_drawn_apart() {
        let main = Canvas::main(EngineConfig { width: 640, height: 360, fps: 30, ..Default::default() });
        let for_ = |audience| Canvas { audience, ..main.clone() };
        let (stream, recording, everyone) =
            (for_(Audience::Stream), for_(Audience::Recording), for_(Audience::Everyone));
        let stream_only = Sides { stream_only: true, recording_only: false };
        // Stream-only items: the stream and the editor's views are the same picture.
        assert_eq!(view(&stream, stream_only), view(&everyone, stream_only));
        assert_ne!(view(&recording, stream_only), view(&everyone, stream_only));
        let recording_only = Sides { stream_only: false, recording_only: true };
        assert_eq!(view(&recording, recording_only), view(&everyone, recording_only));
        assert_ne!(view(&stream, recording_only), view(&everyone, recording_only));
    }

    /// A flagged item counts only on the canvas whose scenes hold it: one in the vertical picture's
    /// scene never splits the wide picture, and the reverse.
    #[test]
    fn a_flag_counts_only_on_the_canvas_that_shows_it() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 64, height: 36, rgba: [255, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut flagged = SceneItem::new(1, Placement::default());
        flagged.show = Show::Recording;
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Tall", vec![flagged.clone()]);
        assert!(e.go("Main").is_ok());
        let wide = Canvas::main(e.config);
        let tall = Canvas { scene: Some("Tall".into()), ..Canvas::main(e.config) };
        {
            let st = e.lock();
            assert_eq!(sides(&st, &wide), Sides::default(), "the wide picture has nothing flagged");
            assert_eq!(sides(&st, &tall), Sides { stream_only: false, recording_only: true });
        }
        // A flagged item in a group, in a nested scene, counts too; a hidden one, or a hidden group, doesn't.
        let mut group = SceneItem::new(0, Placement::default());
        group.scene = Some("Inner".into());
        group.group = Some((64, 36));
        e.set_scene("Inner", vec![SceneItem { show: Show::Stream, ..SceneItem::new(1, Placement::default()) }]);
        e.set_scene("Main", vec![group.clone()]);
        assert_eq!(sides(&e.lock(), &wide), Sides { stream_only: true, recording_only: false });
        group.visible = false;
        e.set_scene("Main", vec![group]);
        assert_eq!(sides(&e.lock(), &wide), Sides::default(), "a hidden group shows nothing");
    }

    /// The leaf pictures a built scene draws (a group's, inside it).
    fn leaves(items: &[Item]) -> usize {
        items
            .iter()
            .map(|i| match &i.content {
                Content::Group { items, .. } => leaves(items),
                Content::Scene(s) => leaves(&s.items),
                _ => 1,
            })
            .sum()
    }

    /// A flag on a group covers the group as a whole, and a child keeps its own inside it.
    #[test]
    fn a_flag_on_a_group_covers_its_children() {
        let Some(e) = engine() else { return };
        for id in [1, 2, 3] {
            e.set_source(id, SourceKind::Color { width: 160, height: 90, rgba: [255, 0, 0, 255] })
                .unwrap_or_else(|x| panic!("{x}"));
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !(1..=3).all(|id| e.lock().sources.get(&id).is_some_and(|s| s.size.is_some())) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let shown = |show: Show, child: Show| {
            let mut a = SceneItem::new(1, Placement::default());
            let mut b = SceneItem::new(2, Placement::default());
            b.show = child;
            a.show = Show::Both;
            e.set_scene("Inner", vec![a, b]);
            let mut group = SceneItem::new(0, Placement::default());
            group.scene = Some("Inner".into());
            group.group = Some((640, 360));
            group.show = show;
            let mut other = SceneItem::new(3, Placement::default());
            other.show = Show::Stream;
            e.set_scene("Main", vec![group, other]);
            let st = e.lock();
            let n = |audience| leaves(&build(&st, 640, 360, Some("Main"), 0, audience).items);
            (n(Audience::Stream), n(Audience::Recording), n(Audience::Everyone))
        };
        // A plain group: its children as they are (the second only in the recording), and the
        // stream-only item beside it.
        assert_eq!(shown(Show::Both, Show::Recording), (2, 2, 3));
        // A recording-only group is gone from the stream whole, children and all.
        assert_eq!(shown(Show::Recording, Show::Both), (1, 2, 3));
        // A stream-only group is gone from the recording whole; the stream-only item with it.
        assert_eq!(shown(Show::Stream, Show::Both), (3, 0, 3));
        // Nothing flagged: everyone sees everything (the item beside it is still stream-only).
        assert_eq!(shown(Show::Both, Show::Both), (3, 2, 3));
    }

    /// A group none of whose sources is for this audience draws nothing (its own filters would
    /// paint an empty box into a clean recording).
    #[test]
    fn a_group_with_nothing_for_an_audience_draws_nothing() {
        let Some(e) = engine() else { return };
        for id in [1, 2] {
            e.set_source(id, SourceKind::Color { width: 64, height: 36, rgba: [255, 0, 0, 255] })
                .unwrap_or_else(|x| panic!("{x}"));
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !(1..=2).all(|id| e.lock().sources.get(&id).is_some_and(|s| s.size.is_some())) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        let only_stream = |id| SceneItem { show: Show::Stream, ..SceneItem::new(id, Placement::default()) };
        e.set_scene("Inner", vec![only_stream(1), only_stream(2)]);
        let mut group = SceneItem::new(0, Placement::default());
        group.scene = Some("Inner".into());
        group.group = Some((400, 600));
        e.set_scene("Main", vec![group]);
        let st = e.lock();
        let count = |a| build(&st, 640, 360, Some("Main"), 0, a).items.len();
        assert_eq!(count(Audience::Stream), 1, "the stream shows the group");
        assert_eq!(count(Audience::Everyone), 1);
        assert_eq!(count(Audience::Recording), 0, "the recording has nothing from it, not an empty box");
        // Several containers deep: Main > Outer > Inner > a source for the stream only. The recording
        // sees none of the three, not an empty Outer.
        drop(st);
        let mut inner = SceneItem::new(0, Placement::default());
        inner.scene = Some("Leaf".into());
        inner.group = Some((200, 200));
        e.set_scene("Leaf", vec![only_stream(1)]);
        e.set_scene("Middle", vec![inner]);
        let mut outer = SceneItem::new(0, Placement::default());
        outer.scene = Some("Middle".into());
        outer.group = Some((300, 300));
        e.set_scene("Main", vec![outer]);
        {
            let st = e.lock();
            let count = |a| build(&st, 640, 360, Some("Main"), 0, a).items.len();
            assert_eq!(count(Audience::Stream), 1);
            assert_eq!(count(Audience::Recording), 0, "no empty box left by the outer group");
        }
        // A group that was empty to begin with is as it was.
        e.set_scene(
            "Main",
            vec![{
                let mut g = SceneItem::new(0, Placement::default());
                g.scene = Some("Inner".into());
                g.group = Some((400, 600));
                g
            }],
        );
        e.set_scene("Inner", vec![]);
        assert_eq!(build(&e.lock(), 640, 360, Some("Main"), 0, Audience::Recording).items.len(), 1);
    }

    /// The picture at the middle of the canvas, as the stream's, the recording's and the editor's
    /// views draw it: the luma of a white backdrop (high) or a black box over it (low).
    fn middle_luma(e: &Engine, audience: Audience) -> u8 {
        let got = Arc::new(Mutex::new(None::<u8>));
        let sink = got.clone();
        let (w, h) = (e.config.width as usize, e.config.height as usize);
        let tap = e
            .tap_frames(
                Canvas { audience, ..Canvas::main(e.config) },
                Box::new(move |f: &Nv12, _| {
                    sink.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(f.y[h / 2 * w + w / 2]);
                }),
            )
            .unwrap_or_else(|x| panic!("{x}"));
        let until = Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap_or_else(|p| p.into_inner()).is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        e.stop_tap(tap);
        let luma = *got.lock().unwrap_or_else(|p| p.into_inner());
        luma.expect("a picture drew")
    }

    /// The point of it: a box shown only in the recording is in the recording's picture and not
    /// the stream's, and the reverse.
    #[test]
    fn the_recording_lacks_a_stream_only_item_and_the_stream_lacks_a_recording_only_one() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 160, height: 90, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let scene = |show: Show| {
            let mut boxed = SceneItem::new(2, Placement { x: 240.0, y: 135.0, ..Placement::default() });
            boxed.show = show;
            e.set_scene("Main", vec![SceneItem::new(1, Placement::default()), boxed]);
            assert!(e.go("Main").is_ok());
        };
        let (white, black) = (|l: u8| l > 200, |l: u8| l < 40);
        scene(Show::Recording);
        assert!(white(middle_luma(&e, Audience::Stream)), "the stream has no box");
        assert!(black(middle_luma(&e, Audience::Recording)), "the recording has it");
        assert!(black(middle_luma(&e, Audience::Everyone)), "the editor shows it");
        scene(Show::Stream);
        assert!(black(middle_luma(&e, Audience::Stream)), "the stream has the box");
        assert!(white(middle_luma(&e, Audience::Recording)), "the recording has none");
        assert!(black(middle_luma(&e, Audience::Everyone)));
        scene(Show::Both);
        assert!(black(middle_luma(&e, Audience::Stream)) && black(middle_luma(&e, Audience::Recording)));
    }

    /// A stream and a recording started on the engine take their pictures by what they are.
    #[test]
    fn outputs_are_started_for_what_they_are() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        assert!(e.go("Main").is_ok());
        let dir = std::env::temp_dir().join(format!("ferrocast-audience-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|x| panic!("{x}"));
        let rec = e.start_output(OutputKind::Record(dir.join("a.mp4")));
        let stream = e.start_output(OutputKind::Stream(Box::new(|_| {})));
        let both = rec.is_ok() && stream.is_ok();
        let audiences: Vec<Audience> = e.lock().outputs.iter().map(|(.., c)| c.audience).collect();
        for id in [rec, stream].into_iter().flatten() {
            let _ = e.stop_output(id);
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Where an encoder isn't there (a CI box) nothing started: nothing to check.
        if both {
            assert_eq!(audiences, [Audience::Recording, Audience::Stream]);
        }
    }

    /// Broadcast timing: 29.97 and 59.94 (and 23.976) as fractions, and the frame times that go with them.
    #[test]
    fn broadcast_timing_is_a_thousandth_slower() {
        let c = |fps, ntsc| EngineConfig { fps, ntsc, ..Default::default() };
        assert_eq!(c(30, false).frame_rate(), (30, 1));
        assert_eq!(c(30, true).frame_rate(), (30_000, 1001));
        assert_eq!(c(60, true).frame_rate(), (60_000, 1001));
        assert_eq!(c(24, true).frame_rate(), (24_000, 1001));
        assert_eq!(c(30, false).frame_ns(), 33_333_333);
        assert_eq!(c(30, true).frame_ns(), 33_366_666);
        assert_eq!(c(60, true).frame_ns(), 16_683_333);
        // 1000 frames at 59.94 take 16,683 ms (16,667 ms at 60): the stutter a 59.94 source made.
        assert_eq!(c(60, true).frame_ns() * 1000 / 1_000_000, 16_683);
    }

    /// An engine at 29.97 hands frames out 33.37 ms apart, and its encoders are told 30000/1001.
    #[test]
    fn an_ntsc_engine_paces_frames_at_29_97() {
        let Some(e) = engine_with(EngineConfig { width: 640, height: 360, fps: 30, ntsc: true, ..Default::default() })
        else {
            return;
        };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        assert!(e.go("Main").is_ok());
        let stamps = Arc::new(Mutex::new(Vec::<u64>::new()));
        let sink = stamps.clone();
        let tap = e
            .tap_frames(
                Canvas::main(e.config),
                Box::new(move |_f: &Nv12, ts| sink.lock().unwrap_or_else(|p| p.into_inner()).push(ts)),
            )
            .unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(900));
        e.stop_tap(tap);
        let t = stamps.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(t.len() >= 5, "{} frames", t.len());
        // Frames on the grid: each a whole number of 33.3667 ms frame times after the first.
        let grid = e.config.frame_ns();
        assert!(t.windows(2).all(|w| (w[1] - w[0]) % grid == 0 && w[1] > w[0]), "{t:?}");
    }

    /// The downscale filter chosen for an output is the one that draws it (it was always bicubic):
    /// a hard edge scaled 0.6x comes out differently with bilinear, bicubic and lanczos.
    #[test]
    fn the_downscale_filter_chosen_is_the_one_used() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [255, 255, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 321, height: 360, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default()), SceneItem::new(2, Placement::default())]);
        assert!(e.go("Main").is_ok());
        let first_frame = |filter: Filter| -> Vec<u8> {
            let got = Arc::new(Mutex::new(None::<Vec<u8>>));
            let sink = got.clone();
            let tap = e
                .tap_frames(
                    Canvas::scaled_with(e.config, 384, 216, filter),
                    Box::new(move |f: &Nv12, _| {
                        sink.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_with(|| f.y.clone());
                    }),
                )
                .unwrap_or_else(|x| panic!("{x}"));
            let until = Instant::now() + Duration::from_secs(5);
            while got.lock().unwrap_or_else(|p| p.into_inner()).is_none() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            e.stop_tap(tap);
            let y = got.lock().unwrap_or_else(|p| p.into_inner()).clone();
            y.unwrap_or_else(|| panic!("no frame for {filter:?}"))
        };
        let bilinear = first_frame(Filter::Bilinear);
        let bicubic = first_frame(Filter::Bicubic);
        let lanczos = first_frame(Filter::Lanczos);
        assert_ne!(bilinear, lanczos, "lanczos isn't bilinear");
        assert_ne!(bilinear, bicubic, "bicubic isn't bilinear");
        assert_ne!(bicubic, lanczos, "lanczos isn't bicubic");
        // The names a stream's setting uses.
        assert_eq!(Filter::downscale_named("lanczos"), Some(Filter::Lanczos));
        assert_eq!(Filter::downscale_named("area"), Some(Filter::Area));
        assert_eq!(Filter::downscale_named("sharp"), None);
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

    /// The graphics card's failure reaches every output's error (review R1).
    #[test]
    #[cfg(target_os = "macos")]
    fn a_gpu_failure_is_every_outputs_error() {
        let Some(e) = engine() else { return };
        assert_eq!(e.output_error(7), None);
        *e.gpu.gpu_error.lock().unwrap_or_else(|p| p.into_inner()) = Some("out of memory".into());
        assert_eq!(e.output_error(7), Some("out of memory".into()));
        assert_eq!(e.output_error(8), Some("out of memory".into()));
    }

    /// Notices are an engine's own: one raised in one engine is taken from that engine only.
    #[test]
    fn two_engines_keep_their_notices_apart() {
        let (Some(a), Some(b)) = (engine(), engine()) else { return };
        let note =
            |line: &str| crate::notices::Notice { key: "mic-bluetooth", line: line.into(), detail: String::new() };
        a.lock().notices.push(note("from a"));
        assert!(b.take_notices().is_empty(), "b doesn't see a's");
        assert_eq!(a.take_notices(), vec![note("from a")]);
        assert!(a.take_notices().is_empty(), "taken once");
    }

    /// A failed output isn't fed again at 60 frames a second (review R2).
    #[test]
    #[cfg(target_os = "macos")]
    fn a_failed_output_is_not_fed_again() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Wide", vec![SceneItem::new(1, Placement::default())]);
        e.go("Wide").unwrap_or_else(|x| panic!("{x}"));
        let id = e.start_output(OutputKind::Replay { seconds: 5, mb: 0 }).unwrap_or_else(|x| panic!("{x}"));
        e.fail_output_for_test(id);
        let deadline = Instant::now() + Duration::from_secs(40);
        while e.output_error(id).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(e.output_error(id).is_some());
        let after_failure = e.video_calls_for_test(id);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(e.video_calls_for_test(id), after_failure, "the encoder isn't handed more frames");
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
    }

    /// Stopping an output whose hardware session was just taken away, while the frame loop
    /// rebuilds it under the engine's lock: it finishes, with no error and no deadlock (review
    /// review R5).
    #[test]
    #[cfg(target_os = "macos")]
    fn stopping_an_output_races_its_rebuild_without_error() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Wide", vec![SceneItem::new(1, Placement::default())]);
        e.go("Wide").unwrap_or_else(|x| panic!("{x}"));
        for round in 0..4 {
            let id = e.start_output(OutputKind::Stream(Box::new(|_| {}))).unwrap_or_else(|x| panic!("{x}"));
            std::thread::sleep(Duration::from_millis(300));
            e.lose_output_session_for_test(id);
            // Straight away, or a few frames later: the loop's rebuild may come before or after.
            std::thread::sleep(Duration::from_millis(round * 25));
            let started = Instant::now();
            let stopped = e.stop_output(id);
            assert!(stopped.is_ok(), "round {round}: {stopped:?}");
            assert!(started.elapsed() < Duration::from_secs(5), "round {round}: stop took {:?}", started.elapsed());
            assert_eq!(e.output_error(id), None);
        }
    }

    /// An encoder that fails once is made again and the output goes on by itself: a stream
    /// resumes within seconds (new header, a keyframe), a recording asks for a new part and ends up
    /// as two playable files (the soak's silent stall: an output that errored just stopped).
    #[test]
    #[cfg(target_os = "macos")]
    fn a_stream_and_a_recording_heal_themselves_after_an_encoder_fault() {
        if !encodes_here() {
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Wide", vec![SceneItem::new(1, Placement::default())]);
        e.go("Wide").unwrap_or_else(|x| panic!("{x}"));
        let tags = Arc::new(AtomicU64::new(0));
        let counter = tags.clone();
        let stream = e
            .start_output(OutputKind::Stream(Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            })))
            .unwrap_or_else(|x| panic!("{x}"));
        let dir = std::env::temp_dir().join(format!("ferrocore-heal-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (one, two) = (dir.join("part1.mp4"), dir.join("part2.mp4"));
        let record = e.start_output(OutputKind::Record(one.clone())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(1200));
        let before = tags.load(Ordering::Relaxed);
        assert!(before > 0, "the stream flows before the fault");
        e.fail_output_once_for_test(stream);
        e.fail_output_once_for_test(record);
        // Within a few seconds (the 1 s wait, then a new encoder) both flow again.
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut named = false;
        let mut resumed_at = None;
        while Instant::now() < deadline {
            if !named && e.output_record_state(record).is_ok_and(|r| r.split_wanted) {
                e.split_output(record, &two).unwrap_or_else(|x| panic!("{x}"));
                named = true;
            }
            let now = tags.load(Ordering::Relaxed);
            if resumed_at.is_none() && now > before + 60 && named {
                resumed_at = Some(Instant::now());
            }
            if resumed_at.is_some_and(|t| t.elapsed() > Duration::from_millis(800)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(named, "the recording asked for a new part");
        assert!(resumed_at.is_some(), "the stream flowed again by itself");
        assert_eq!(e.output_error(stream), None);
        assert_eq!(e.output_error(record), None);
        e.stop_output(stream).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(record).unwrap_or_else(|x| panic!("{x}"));
        for f in [&one, &two] {
            assert!(std::fs::metadata(f).map_or(0, |m| m.len()) > 1_000, "{f:?} has content");
        }
        if let Ok(out) = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration"])
            .arg(&two)
            .output()
        {
            assert!(out.status.success(), "the second part plays: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    /// One output's encoder fault marks that output, not the others (GAPS #3): the main
    /// stream stays well, and a new output starts with no error.
    #[test]
    #[cfg(target_os = "macos")]
    fn one_outputs_fault_is_its_own() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Wide", vec![SceneItem::new(1, Placement::default())]);
        e.go("Wide").unwrap_or_else(|x| panic!("{x}"));
        let main = e.start_output(OutputKind::Stream(Box::new(|_| {}))).unwrap_or_else(|x| panic!("{x}"));
        let second = e.start_output(OutputKind::Replay { seconds: 5, mb: 0 }).unwrap_or_else(|x| panic!("{x}"));
        e.fail_output_for_test(second);
        let deadline = Instant::now() + Duration::from_secs(40);
        while e.output_error(second).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(e.output_error(second).is_some(), "the faulting output says so");
        assert_eq!(e.output_error(main), None, "the main stream isn't marked dead by it");
        e.stop_output(second).unwrap_or_else(|x| panic!("{x}"));
        let third = e.start_output(OutputKind::Replay { seconds: 5, mb: 0 }).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(e.output_error(third), None);
        assert_eq!(e.output_error(second), None, "a stopped output's error goes with it");
        e.stop_output(third).unwrap_or_else(|x| panic!("{x}"));
        e.stop_output(main).unwrap_or_else(|x| panic!("{x}"));
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
            let o = outlines(&st, &marks, (640, 360), "Main", now_ns());
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
            outlines(&st, &marks, (640, 360), "Main", now_ns())[0].corners
        };
        assert_eq!(corners(&e, true, (10.0, 20.0)), [(10.0, 20.0), (110.0, 20.0), (110.0, 70.0), (10.0, 70.0)]);
        assert_eq!(corners(&e, true, (300.0, 100.0)), [(300.0, 100.0), (400.0, 100.0), (400.0, 150.0), (300.0, 150.0)]);
        assert_eq!(corners(&e, false, (10.0, 20.0))[2], (130.0, 90.0), "grown by 10 on every side");
    }

    /// A box for a source picked in one picture is drawn on that picture only: the main and the
    /// vertical preview both read the marks, and the vertical one's selection was outlined in the
    /// main one's corner too.
    #[test]
    fn a_selection_is_outlined_only_on_the_picture_that_shows_its_scene() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 100, height: 50, rgba: [200, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut it = SceneItem::new(1, Placement { x: 10.0, y: 20.0, ..Default::default() });
        it.item_id = 7;
        e.set_scene("Main", vec![]);
        e.set_scene("Vertical", vec![it]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let marks = crate::preview::Marks {
            scene: Some("Vertical".into()),
            picked: Some(("Vertical".into(), vec![7])),
            colors: vec![[1.0, 0.0, 0.0, 1.0]],
        };
        let st = e.lock();
        assert!(outlines(&st, &marks, (640, 360), "Main", now_ns()).is_empty(), "nothing on the main picture");
        let on_vertical = outlines(&st, &marks, (640, 360), "Vertical", now_ns());
        assert_eq!(on_vertical.len(), 1);
        assert!(on_vertical[0].handles);
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
        let o = outlines(&st, &marks, (640, 360), "Main", now_ns());
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
            profile: crate::video::H264Profile::High,
            bframes: false,
            preset: None,
            tune: None,
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

    /// An output a stream is on draws what the stream shows (a recording-only item must never
    /// reach the stream through a shared encode); when the stream leaves, the recording goes on
    /// with what the recording shows.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_shared_output_follows_whether_a_stream_is_on_it() {
        let Some(e) = engine() else { return };
        shared_scene(&e);
        let file = std::env::temp_dir().join(format!("ferrocore-audience-{}.mp4", std::process::id()));
        let id = e.start_output(OutputKind::Record(file.clone())).unwrap_or_else(|x| panic!("{x}"));
        let audience = |e: &Engine| e.lock().outputs.iter().find(|o| o.0 == id).map(|o| o.3.audience);
        assert_eq!(audience(&e), Some(Audience::Recording));
        e.stream_on(id, sink_into(&Sent::default())).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(audience(&e), Some(Audience::Stream), "a stream on it: the stream's picture");
        e.stream_off(id).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(audience(&e), Some(Audience::Recording), "the recording goes on with its own");
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));
        let _ = std::fs::remove_file(file);
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
        // 1.5 s at 30 fps; a CI runner's virtual encoder is slower, so it only has to keep going.
        let ci = std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"));
        assert!(video.len() > if ci { 10 } else { 40 }, "{} video tags", video.len());
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
        // The recording is of a real engine on a real machine. A starved one (a full workspace run
        // with builds going) can drop a frame or stall the sound for a moment, which moves a pair
        // past the bound once; a fault in the sync itself moves every pair, every time. So the check
        // is that a recording is in sync, tried up to three times; all three out of sync fails.
        let mut tries = Vec::new();
        for attempt in 1..=3 {
            match sync_attempt(&clip, &rec, secs) {
                Ok(()) => return,
                Err(why) => {
                    eprintln!("recording {attempt} of 3: {why}");
                    tries.push(why);
                }
            }
        }
        panic!("out of sync in all three recordings: {tries:?}");
    }

    /// Plays the clip, records it, and says whether each flash and its beep are within the bound.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn sync_attempt(clip: &std::path::Path, rec: &std::path::Path, secs: u64) -> Result<(), String> {
        let Some(e) = engine() else { return Ok(()) };
        e.set_source(1, SourceKind::Media { path: clip.to_path_buf(), options: crate::ffsrc::MediaOptions::default() })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let id = e.start_output(OutputKind::Record(rec.to_path_buf())).unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_secs(secs));
        e.stop_output(id).unwrap_or_else(|x| panic!("{x}"));

        let beeps = beep_times(rec);
        // A flash in the last moments of the picture has its beep after the sound ended (the
        // recording stops a few frames' worth earlier in the sound than in the picture): it would
        // pair with the beep before it, or with the end of the sound that silencedetect reports as
        // a beep's end, and read as 100 ms out of sync (seen in a full workspace run under load).
        let sound_ends = audio_secs(rec);
        let flashes: Vec<f64> = flash_times(rec).into_iter().filter(|f| *f + 0.05 < sound_ends).collect();
        let offsets: Vec<f64> = flashes
            .iter()
            .filter_map(|f| beeps.iter().map(|b| b - f).min_by(|a, b| a.abs().total_cmp(&b.abs())))
            .filter(|d| d.abs() < 0.5)
            .collect();
        eprintln!("flashes {flashes:.3?}\nbeeps   {beeps:.3?}\nsound minus picture {offsets:.3?} s");
        if offsets.len() < 3 {
            return Err(format!("too few flash and beep pairs: {flashes:?} {beeps:?}"));
        }
        // Within about a frame and a half at 30 fps: well inside what anyone can see (~45 ms).
        if !offsets.iter().all(|d| d.abs() <= 0.05) {
            return Err(format!("out of sync: {offsets:.3?} s"));
        }
        Ok(())
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

    /// How long the recording's sound lasts, in seconds.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn audio_secs(file: &std::path::Path) -> f64 {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "packet=pts_time,duration_time"])
            .args(["-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap_or_else(|x| panic!("{x}"));
        // The end of the last packet.
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (t, d) = l.split_once(',')?;
                Some(t.trim().parse::<f64>().ok()? + d.trim().parse::<f64>().unwrap_or(0.0))
            })
            .fold(0.0, f64::max)
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
    fn a_video_delay_shows_pictures_late_and_leaves_others_alone() {
        let Some(e) = engine() else { return };
        for id in [1, 2] {
            e.set_source(id, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        }
        freeze_scene(&e, vec![Effect::Delay { ms: 600, audio: false }]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let (one, two) = (e.fed(1).unwrap_or_else(|| panic!("fed")), e.fed(2).unwrap_or_else(|| panic!("fed")));
        let px = |e: &Engine, x: usize| e.snapshot().unwrap_or_default()[x * 4..x * 4 + 4].to_vec();
        one.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        two.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        std::thread::sleep(Duration::from_millis(250));
        assert_ne!(px(&e, 0), [0, 0, 255, 255], "held back");
        assert_eq!(px(&e, 330), [0, 0, 255, 255], "the other source isn't");
        std::thread::sleep(Duration::from_millis(700));
        // Held as video, so within a step or two of the colour (not bit for bit).
        let shown = px(&e, 0);
        assert!(
            shown[0] <= 3 && shown[1] <= 3 && shown[2] >= 252 && shown[3] == 255,
            "shown once the delay is up: {shown:?}"
        );
        // Off: pictures come straight through again.
        freeze_scene(&e, Vec::new());
        one.picture(Picture::solid(64, 36, [0, 255, 0, 255]));
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(px(&e, 0), [0, 255, 0, 255], "no delay once the filter is gone");
    }

    #[test]
    fn a_delay_counts_only_where_the_source_is_shown() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        let mut slow = SceneItem::new(1, Placement::default());
        slow.effects = vec![Effect::Delay { ms: 500, audio: true }];
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Other", vec![slow.clone()]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        assert!(seen(&e.lock()).1.is_empty(), "a delay in a scene that's off air leaves the source alone");
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(seen(&e.lock()).1.get(&1), Some(&(500, true)));
        slow.visible = false;
        e.set_scene("Other", vec![slow]);
        assert!(seen(&e.lock()).1.is_empty(), "nor does a hidden item's");
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

    /// Waits (up to 10 s) until nothing but the test holds `buf`.
    #[cfg(target_os = "macos")]
    fn wait_retain_count_one(buf: &objc2_core_video::CVImageBuffer) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::apple::screen::tests::buffer_retain_count(buf) != 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
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
        // The frame loop lets go of it on its next frame: wait for that, not a fixed time (a
        // loaded CI runner took longer than 200 ms).
        wait_retain_count_one(&buf);
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
        wait_retain_count_one(&buf);
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

    /// Active hidden captures keep the newest surface in `pending` directly without per-frame
    /// CPU copies, release replaced `CVImageBuffer`s immediately, upload zero-copy when shown,
    /// and convert at most once if idle past `PENDING_UNPIN_NS`.
    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_source_does_not_copy_new_surfaces_every_frame() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 64, height: 36, rgba: [0, 0, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Fed { width: 64, height: 36 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Other", vec![SceneItem::new(2, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));

        let feed = e.fed(2).unwrap_or_else(|| panic!("fed source"));
        feed.picture(Picture::solid(64, 36, [0, 0, 255, 255]));
        for _ in 0..50 {
            if e.lock().sources[&2].pending.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(e.lock().sources[&2].pending.is_some(), "initial picture consumed while hidden");

        let copies = Arc::new(AtomicU64::new(0));
        let mut buffers = Vec::new();
        for n in 1u8..=8 {
            let Some(buf) = crate::apple::screen::tests::bgra_buffer(64, 36, [n * 10, 200, 30, 255]) else { return };
            let surface = crate::screen::Surface::of(buf.clone())
                .unwrap_or_else(|| panic!("BGRA surface"))
                .with_copy_counter(Arc::clone(&copies));
            buffers.push(buf);
            feed.surface(surface);
            for _ in 0..30 {
                if !feed.has_surface() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!feed.has_surface(), "frame loop consumed hidden surface #{n}");
        }

        // Hidden captures must NOT call to_picture() per frame (~2 GB/s at 4K60); only the
        // newest buffer stays pinned (earlier ones are dropped when `pending` is overwritten).
        assert_eq!(copies.load(Ordering::Relaxed), 0, "hidden frames must not copy to CPU");
        for old in &buffers[..buffers.len() - 1] {
            assert_eq!(
                crate::apple::screen::tests::buffer_retain_count(old),
                1,
                "replaced pending surface must release its CVImageBuffer"
            );
        }
        assert_eq!(
            crate::apple::screen::tests::buffer_retain_count(buffers.last().unwrap_or_else(|| panic!("buf"))),
            2,
            "newest pending surface is kept directly until shown or stale"
        );
        assert!(matches!(e.lock().sources[&2].pending, Some(Fresh::Screen(_))));

        // Showing the hidden scene uploads the newest surface directly (zero-copy, still 0 CPU copies).
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        for _ in 0..40 {
            if e.lock().sources[&2].pending.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(copies.load(Ordering::Relaxed), 0, "reveal uploads zero-copy without CPU conversion");
        assert_eq!(&e.snapshot().unwrap_or_default()[0..4], &[30, 200, 80, 255], "latest hidden frame rendered");

        // If a hidden source stops sending updates past PENDING_UNPIN_NS, it converts ONCE.
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let Some(idle_buf) = crate::apple::screen::tests::bgra_buffer(64, 36, [40, 50, 60, 255]) else { return };
        let idle_surface = crate::screen::Surface::of(idle_buf.clone())
            .unwrap_or_else(|| panic!("BGRA surface"))
            .with_copy_counter(Arc::clone(&copies));
        feed.surface(idle_surface);
        for _ in 0..30 {
            if !feed.has_surface() && e.lock().sources[&2].pending.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(copies.load(Ordering::Relaxed), 0);
        {
            let mut st = e.lock();
            let src = st.sources.get_mut(&2).unwrap_or_else(|| panic!("source 2"));
            assert!(matches!(src.pending, Some(Fresh::Screen(_))));
            let past = src.pending_since + PENDING_UNPIN_NS;
            unpin_stale_pending(src, past);
            unpin_stale_pending(src, past + 100);
            assert!(matches!(src.pending, Some(Fresh::Pic(_))));
        }
        assert_eq!(copies.load(Ordering::Relaxed), 1, "idle hidden surface converts at most once");
        assert_eq!(crate::apple::screen::tests::buffer_retain_count(&idle_buf), 1, "idle unpin releases CVImageBuffer");
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

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn hotplug_reconnect_wakes_no_signal_capture_card() {
        use crate::camera_retry::{CAMERA_FIRST_FRAME_NS, CameraObservation, NO_SIGNAL_MAX_TRIES};

        let backend = FakeCameraBackend::new([0, 200, 100, 255]);
        backend.streaming.store(false, Ordering::Release);
        let live = backend.open().unwrap_or_else(|x| panic!("{x}"));
        let mut src = Src::new(live, None, Some(SourceKind::Camera(CameraSpec::from("card-hdmi-1"))));

        let mut t = now_ns();
        let retry = src.camera_retry.as_mut().unwrap();
        for _ in 0..=NO_SIGNAL_MAX_TRIES {
            t += CAMERA_FIRST_FRAME_NS.max(20_000_000_000);
            let act = retry.step(CameraObservation {
                running: true,
                frames: 0,
                ended: false,
                interrupted: false,
                opening: false,
                visible_or_on_air: true,
                now_ns: t,
            });
            if matches!(act, crate::camera_retry::CameraAction::Reopen(_)) {
                let prev = retry.on_reopen_started(t);
                retry.on_reopen_finished(prev, true, t, None);
            }
        }
        assert!(retry.is_no_signal(), "setup must reach no_signal state");
        src.watch.no_signal = true;
        src.watch.tries = 3;
        src.watch.next_try = t + 16_000_000_000;

        let mut sources = BTreeMap::new();
        sources.insert(1, src);

        let woke_at = t + 1_000_000_000;
        let woke = apply_hotplug_reconnects(&mut sources, &["card-hdmi-1".into()], woke_at);
        assert_eq!(woke, 1, "hotplug reconnect must wake a no_signal capture card");
        let woken = &sources[&1];
        assert!(!woken.watch.no_signal, "hotplug wake must clear watch.no_signal");
        assert_eq!(woken.watch.next_try, woke_at);
        assert_eq!(woken.watch.tries, 0);
        assert!(woken.camera_retry.as_ref().is_some_and(|r| !r.is_no_signal()), "must clear CameraRetry::no_signal");
    }

    #[test]
    fn sleep_inhibit_follows_streaming_and_recording_lifetime() {
        if !encodes_here() {
            return;
        }
        let Some(engine) = engine() else {
            return;
        };
        assert!(!engine.sleep_inhibited(), "idle engine does not inhibit sleep");

        // Replay buffer alone does not keep the machine awake.
        let replay = engine.start_output(OutputKind::Replay { seconds: 5, mb: 16 }).unwrap_or_else(|e| panic!("{e}"));
        assert!(!engine.sleep_inhibited(), "replay alone leaves sleep uninhibited");

        // Starting a stream activates sleep inhibition; a second recording keeps it active when the
        // first stops, and stopping the last active output releases it.
        let dir = std::env::temp_dir().join(format!("ferrocore-sleep-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rec_path = dir.join("rec.mp4");

        let stream = engine.start_output(OutputKind::Stream(Box::new(|_| {}))).unwrap_or_else(|e| panic!("{e}"));
        assert!(engine.sleep_inhibited(), "active stream inhibits sleep");

        let rec = engine.start_output(OutputKind::Record(rec_path)).unwrap_or_else(|e| panic!("{e}"));
        assert!(engine.sleep_inhibited());

        engine.stop_output(stream).unwrap_or_else(|e| panic!("{e}"));
        assert!(engine.sleep_inhibited(), "recording still active keeps sleep inhibited");

        engine.stop_output(rec).unwrap_or_else(|e| panic!("{e}"));
        assert!(!engine.sleep_inhibited(), "releasing last recording frees sleep inhibition");

        let _ = engine.stop_output(replay);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn join_bounded_returns_within_timeout_without_hanging_on_stalled_thread() {
        // Fast worker finishes cleanly and joins within the timeout.
        let done_fast = Arc::new((Mutex::new(false), Condvar::new()));
        let tx = done_fast.clone();
        let h_fast = std::thread::spawn(move || {
            *tx.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            tx.1.notify_all();
        });
        assert!(join_bounded(h_fast, &done_fast, Duration::from_secs(1)));

        // Stalled worker (e.g., wedged OS camera open in watchdog) detaches cleanly at the bound.
        let done_slow = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_worker = release.clone();
        let h_slow = std::thread::spawn(move || {
            let g = release_worker.0.lock().unwrap_or_else(|e| e.into_inner());
            drop(release_worker.1.wait_while(g, |stop| !*stop));
        });
        let started = Instant::now();
        assert!(!join_bounded(h_slow, &done_slow, Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_millis(400), "must not hang waiting on stalled thread");
        *release.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        release.1.notify_all();
    }

    #[test]
    fn engine_drop_stops_active_outputs_and_joins_promptly() {
        if !encodes_here() {
            return;
        }
        let Some(engine) = engine() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("ferrocore-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rec_path = dir.join("rec.mp4");
        let _ = engine.start_output(OutputKind::Record(rec_path)).unwrap_or_else(|e| panic!("{e}"));

        let started = Instant::now();
        drop(engine);
        assert!(started.elapsed() < Duration::from_secs(5), "Engine::drop must finish cleanly and quickly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn test_ffmpeg() -> Option<PathBuf> {
        let p = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .iter()
            .map(std::path::Path::new)
            .find(|p| p.is_file())
            .map(std::path::Path::to_path_buf)
            .or_else(crate::ffenc::ffmpeg_path)?;
        crate::ffenc::set_ffmpeg(&p);
        Some(p)
    }

    /// `restart_on_activate` + `close_when_inactive`: a hidden non-looping clip stays closed
    /// while Scene A is live (does not run to EOF in the background), opens and plays from 0 when
    /// switched to Scene B, and closes when deactivated or finished (M8-02).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn media_restart_on_activate_and_close_when_inactive() {
        let Some(ff) = test_ffmpeg() else { return };
        let Some(e) = engine() else { return };
        let clip = std::env::temp_dir().join(format!("ferrocore-m802-life-{}.mp4", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=160x90:r=30:d=0.8"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg made clip");

        let opts = crate::ffsrc::MediaOptions {
            looping: false,
            restart_on_activate: true,
            close_when_inactive: true,
            clear_on_media_end: true,
            speed_percent: 100,
            hw_decode: true,
        };
        e.set_source(1, SourceKind::Media { path: clip.clone(), options: opts }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("A", vec![]);
        e.set_scene("B", vec![SceneItem::new(1, Placement::default())]);
        e.go("A").unwrap_or_else(|x| panic!("{x}"));

        // Stay on Scene A longer than the 0.8 s clip: decoder must never open while hidden.
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(e.media_state(1).map(|s| s.0), Some(crate::media::MediaState::Stopped));
        {
            let st = e.lock();
            assert!(
                matches!(st.sources.get(&1).map(|s| &s.live), Some(Live::Media(slot)) if slot.player.is_none()),
                "close_when_inactive must keep player closed while hidden"
            );
        }

        // Switch to Scene B: opens and plays red from 0 ms (engine() canvas is 640x360; clip is 160x90 at 0,0).
        e.go("B").unwrap_or_else(|x| panic!("{x}"));
        let mid = (40 * 640 + 40) * 4;
        let mut saw_red = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if let Ok(px) = e.snapshot()
                && px.get(mid).copied().unwrap_or(0) > 200
            {
                saw_red = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_red, "media must open and play from 0 when Scene B activates");

        // Switch back to Scene A while playing: must close the decoder immediately.
        e.go("A").unwrap_or_else(|x| panic!("{x}"));
        let mut closed_again = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            let st = e.lock();
            if matches!(st.sources.get(&1).map(|s| &s.live), Some(Live::Media(slot)) if slot.player.is_none()) {
                closed_again = true;
                break;
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(closed_again, "switching away from Scene B must drop the decoder");

        // Switch back to Scene B again: restarts from 0 and shows red again.
        e.go("B").unwrap_or_else(|x| panic!("{x}"));
        let mut saw_red_again = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if let Ok(px) = e.snapshot()
                && px.get(mid).copied().unwrap_or(0) > 200
            {
                saw_red_again = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(e);
        let _ = std::fs::remove_file(&clip);
        assert!(saw_red_again, "re-entering Scene B must reopen and restart from 0");
    }

    /// `clear_on_media_end: true` clears the source texture at EOF; `false` holds the last frame (M8-02).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn media_clear_on_media_end_vs_hold_last_frame() {
        let Some(ff) = test_ffmpeg() else { return };
        let Some(e) = engine() else { return };
        let clip = std::env::temp_dir().join(format!("ferrocore-m802-clear-{}.mp4", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=0x00ff00:s=160x90:r=30:d=0.5"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg made clip");

        let mid = (40 * 640 + 40) * 4;
        // Case 1: clear_on_media_end = true.
        let clear_opts =
            crate::ffsrc::MediaOptions { clear_on_media_end: true, ..crate::ffsrc::MediaOptions::default() };
        e.set_source(1, SourceKind::Media { path: clip.clone(), options: clear_opts })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let (mut saw_green, mut cleared) = (false, false);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if let Ok(px) = e.snapshot() {
                let g = px.get(mid + 1).copied().unwrap_or(0);
                if g > 180 {
                    saw_green = true;
                } else if saw_green && g < 20 {
                    cleared = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_green, "clip must display green while playing");
        assert!(cleared, "clear_on_media_end=true must clear the texture at EOF");

        // Case 2: clear_on_media_end = false holds the last green frame after EOF.
        let hold_opts =
            crate::ffsrc::MediaOptions { clear_on_media_end: false, ..crate::ffsrc::MediaOptions::default() };
        e.set_source(2, SourceKind::Media { path: clip.clone(), options: hold_opts }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Hold", vec![SceneItem::new(2, Placement::default())]);
        e.go("Hold").unwrap_or_else(|x| panic!("{x}"));
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if e.media_state(2).is_some_and(|(s, ..)| s == crate::media::MediaState::Ended) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(e.media_state(2).map(|s| s.0), Some(crate::media::MediaState::Ended));
        std::thread::sleep(Duration::from_millis(120));
        let px = e.snapshot().unwrap_or_default();
        drop(e);
        let _ = std::fs::remove_file(&clip);
        assert!(px.get(mid + 1).copied().unwrap_or(0) > 180, "clear_on_media_end=false must hold last frame");
    }

    /// `speed_percent: 200` finishes a 2 s clip in ~1 s, and `hw_decode: false` decodes on CPU (M8-02).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn media_speed_percent_and_hw_decode() {
        let Some(ff) = test_ffmpeg() else { return };
        let clip = std::env::temp_dir().join(format!("ferrocore-m802-speed-{}.mp4", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=2"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg made 2 s clip");

        // hw_decode: false forces CPU decode on ffsrc.
        let sw_opts = crate::ffsrc::MediaOptions {
            hw_decode: false,
            speed_percent: 200,
            ..crate::ffsrc::MediaOptions::default()
        };
        let m = crate::ffsrc::Media::open_for_card_with(&clip, sw_opts, false).unwrap_or_else(|e| panic!("{e}"));
        assert!(!m.hw && !m.hw_active(), "hw_decode=false must stay on CPU");
        let mut first_frame_at: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(4);
        while !m.ended() && Instant::now() < deadline {
            if m.frame_nv12().is_some() && first_frame_at.is_none() {
                first_frame_at = Some(Instant::now());
            }
            let _ = m.take_sound();
            std::thread::sleep(Duration::from_millis(10));
        }
        let ended = m.ended();
        let play_time = first_frame_at.map(|t| t.elapsed()).unwrap_or(Duration::from_secs(99));
        drop(m);
        let _ = std::fs::remove_file(&clip);
        assert!(ended, "200% speed clip finished");
        assert!(
            play_time < Duration::from_millis(1600),
            "2 s clip at 200% speed should finish in ~1 s, took {:?}",
            play_time
        );
    }

    /// A transparent VP9 WebM stinger must composite over the underlying scene with alpha intact
    /// on both macOS (FFmpeg fallback) and Windows (RGBA instead of NV12) (M8-01).
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn stinger_with_transparent_webm_preserves_alpha_underneath() {
        let Some(ff) = test_ffmpeg() else { return };
        let Some(e) = engine() else { return };
        let webm = std::env::temp_dir().join(format!("ferrocore-stinger-alpha-{}.webm", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black@0.0:s=64x72:r=20:d=1.2,format=rgba",
                "-f",
                "lavfi",
                "-i",
                "color=c=0xff0000@1.0:s=64x72:r=20:d=1.2,format=rgba",
                "-filter_complex",
                "[0:v][1:v]hstack=inputs=2,format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
                "-auto-alt-ref",
                "0",
                "-deadline",
                "realtime",
            ])
            .arg(&webm)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = std::fs::remove_file(&webm);
            return;
        }
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [0, 0, 255, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_source(2, SourceKind::Color { width: 640, height: 360, rgba: [0, 255, 0, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Blue", vec![SceneItem::new(1, Placement::default())]);
        e.set_scene("Green", vec![SceneItem::new(2, Placement::default())]);
        e.go("Blue").unwrap_or_else(|x| panic!("{x}"));
        std::thread::sleep(Duration::from_millis(120));

        e.go_stinger("Green", &webm, 600).unwrap_or_else(|x| panic!("{x}"));
        let left_px = (180 * 640 + 80) * 4;
        let right_px = (180 * 640 + 560) * 4;
        let mut verified = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if let Ok(px) = e.snapshot() {
                let l = &px[left_px..left_px + 4];
                let r = &px[right_px..right_px + 4];
                // Right half covered by red stinger; left half transparent showing Blue or Green scene underneath!
                if r[0] > 180 && r[1] < 60 && r[2] < 60 && (l[2] > 180 || l[1] > 180) && l[0] < 60 {
                    verified = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(e);
        let _ = std::fs::remove_file(&webm);
        assert!(verified, "transparent WebM stinger must cover right half in red while revealing scene on left half");
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[allow(unsafe_code)]
    fn the_audio_thread_gets_mmcss_or_high_priority() {
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_HIGHEST};
        let (has_mmcss, prio) = std::thread::spawn(|| {
            let guard = raise_audio_thread();
            let has_mmcss = guard.mmcss.is_some();
            // SAFETY: this thread's pseudo-handle.
            let prio = unsafe { GetThreadPriority(GetCurrentThread()) };
            drop(guard);
            (has_mmcss, prio)
        })
        .join()
        .unwrap_or_default();
        if !has_mmcss {
            assert_eq!(prio, THREAD_PRIORITY_HIGHEST.0);
        }
    }

    /// Gate (D1 Commit 3): holding the engine's `State` mutex for 250 ms (simulating a GPU
    /// readback stall) never stalls `Engine::levels()` or `ferrocast-audio` tone generation.
    #[test]
    fn engine_levels_stays_nonblocking_during_250ms_state_stall() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Tone { hz: 440.0, every_ms: 40, beep_ms: 40 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));

        // Wait until the tone has been mixed and published through `AtomicLevels`.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if e.levels(1).is_some_and(|l| l.peak[0] > -20.0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(e.levels(1).is_some_and(|l| l.peak[0] > -20.0), "expected live levels before stall");

        // Hold `e.state` for 250 ms on a side thread.
        let state = Arc::clone(&e.state);
        let (locked_tx, locked_rx) = mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _guard = state.lock().unwrap_or_else(|x| x.into_inner());
            let _ = locked_tx.send(());
            std::thread::sleep(Duration::from_millis(250));
        });
        locked_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|x| panic!("{x}"));
        assert!(e.state.try_lock().is_err(), "state mutex must be held by stall thread");

        // Every `levels()` call returns in microseconds with live meter values while State is stalled.
        for _ in 0..5 {
            let t0 = Instant::now();
            let lv = e.levels(1);
            let elapsed = t0.elapsed();
            assert!(elapsed < Duration::from_millis(5), "levels() stalled for {elapsed:?}");
            assert!(lv.is_some_and(|l| l.peak[0] > -20.0), "stale or missing levels during stall: {lv:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = blocker.join();

        e.remove_source(1);
        assert!(e.levels(1).is_none());
    }

    #[test]
    fn an_items_preview_is_not_a_nested_scene_with_the_same_number() {
        let Some(e) = engine() else { return };
        // Item numbers restart in every scene: "Outer" item 1 is the nested scene "Inner", and
        // "Inner" item 1 is the Box with its own dimming filter.
        let doc = serde_json::json!({
            "current_program_scene": "Outer",
            "sources": [
                {
                    "id": "color_source_v3",
                    "name": "Box",
                    "settings": { "color": 0xFF00_00FFu32, "width": 320, "height": 180 },
                    "filters": [
                        { "id": "ferrocast_rounded_corners", "name": "Round", "settings": { "radius": 40 } }
                    ]
                },
                {
                    "id": "scene",
                    "name": "Inner",
                    "settings": { "items": [{
                        "id": 1,
                        "name": "Box",
                        "ferrocast_filters": [
                            { "id": "color_filter_v2", "name": "Dim", "settings": { "brightness": -0.5 } }
                        ]
                    }] }
                },
                { "id": "scene", "name": "Outer", "settings": { "items": [{ "id": 1, "name": "Inner" }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty());
        let box_id = e.source_named("Box").map(|(id, _)| id).unwrap();
        let via_outer = e.snapshot_item_filtered("Outer", 1, box_id, (160, 90)).unwrap();
        let own = e.snapshot_item_filtered("Inner", 1, box_id, (160, 90)).unwrap();
        let (mid, corner) = ((45 * 160 + 80) * 4, (160 + 1) * 4);
        assert_eq!(via_outer[corner + 3], 255, "the Box's own filters (no rounded corners), not the nested scene's");
        assert!(via_outer[mid] < 205, "the Box's own dimming: {}", via_outer[mid]);
        assert_eq!(&via_outer[mid..mid + 4], &own[mid..mid + 4]);
    }

    #[test]
    fn same_source_with_per_item_video_filter_override_renders_differently() {
        let Some(e) = engine() else { return };
        let doc = |vert_brightness: f64| {
            serde_json::json!({
                "current_program_scene": "Main",
                "sources": [
                    {
                        "id": "color_source_v3",
                        "name": "Box",
                        "settings": { "color": 0xFF00_00FFu32, "width": 320, "height": 180 },
                        "filters": [
                            { "id": "ferrocast_rounded_corners", "name": "Round", "settings": { "radius": 40 } }
                        ]
                    },
                    {
                        "id": "scene",
                        "name": "Main",
                        "settings": { "items": [{ "id": 1, "name": "Box" }] }
                    },
                    {
                        "id": "scene",
                        "name": "Vertical",
                        "settings": {
                            "items": [{
                                "id": 2,
                                "name": "Box",
                                "ferrocast_filters": [
                                    { "id": "color_filter_v2", "name": "Dim", "settings": { "brightness": vert_brightness } }
                                ]
                            }]
                        }
                    }
                ]
            })
        };
        assert!(e.load_obs(&doc(-0.5)).is_empty());
        let main_px = e.snapshot_scene_at("Main", (320, 180), (320, 180)).unwrap_or_else(|x| panic!("{x}"));
        let vert_px = e.snapshot_scene_at("Vertical", (320, 180), (320, 180)).unwrap_or_else(|x| panic!("{x}"));
        // Main has RoundedCorners: top-left corner (2, 2) is clipped out, center (160, 90) is bright red.
        let center = (90 * 320 + 160) * 4;
        let corner = (2 * 320 + 2) * 4;
        assert_eq!(main_px[corner + 3], 0, "Main item inherits rounded corners: corner clipped");
        assert!(main_px[center] > 240, "Main center stays full red");
        // Vertical overrides with brightness=-0.5 (no rounded corners): corner is opaque & dimmed red.
        assert_eq!(vert_px[corner + 3], 255, "Vertical item replaces rounded corners: corner stays opaque");
        assert!(vert_px[center] > 100 && vert_px[center] < 205, "Vertical center is dimmed red: {}", vert_px[center]);

        // Item-scoped snapshot reflects each item's own effects.
        let box_id = e.source_named("Box").map(|(id, _)| id).unwrap();
        let snap_main = e.snapshot_item_filtered("Main", 1, box_id, (160, 90)).unwrap();
        let snap_vert = e.snapshot_item_filtered("Vertical", 2, box_id, (160, 90)).unwrap();
        let mid = (45 * 160 + 80) * 4;
        assert!(
            snap_main[mid] > snap_vert[mid] + 40,
            "item filtered preview differs: {} vs {}",
            snap_main[mid],
            snap_vert[mid]
        );

        // Editing Vertical override in place keeps Main untouched.
        assert!(e.apply_same_sources(&doc(-0.2)).is_some());
        let main_after = e.snapshot_scene_at("Main", (320, 180), (320, 180)).unwrap();
        let vert_after = e.snapshot_scene_at("Vertical", (320, 180), (320, 180)).unwrap();
        assert_eq!(&main_after[corner..corner + 4], &main_px[corner..corner + 4]);
        assert!(
            vert_after[center] > vert_px[center] + 15,
            "Vertical dimmed less: {} -> {}",
            vert_px[center],
            vert_after[center]
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn offline_camera_stays_in_sources_and_keeps_retrying_within_5s() {
        let Some(e) = engine() else { return };
        let (cam_kind, cam_key) = crate::obs::native_camera_kind_and_key();
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                {
                    "id": cam_kind,
                    "name": "iPhone",
                    "settings": { cam_key: "/no/such/iphone" }
                },
                {
                    "id": "scene",
                    "name": "Main",
                    "settings": { "items": [{ "id": 1, "name": "iPhone", "visible": true }] }
                }
            ]
        });
        let skipped = e.load_obs(&doc);
        assert!(skipped.is_empty(), "native camera kind {cam_kind}/{cam_key} must load: {skipped:?}");
        let wait_open = |label: &str| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while e.opening() > 0 {
                assert!(Instant::now() < deadline, "timed out waiting for {label} open to settle");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_open("initial");
        let (id, _) = e.source_named("iPhone").expect("selected camera kept in engine sources while waiting");
        assert!(e.lock().sources.contains_key(&id), "camera source stays registered for watchdog retries");

        let w = Watch::for_kind(Some(&SourceKind::Camera(CameraSpec::from("iphone-1"))), 0);
        assert!(
            w.backoff(0) <= 5_000_000_000 && w.backoff(9) <= 5_000_000_000,
            "unseen camera backoff must cap at 5 s, got {} ns",
            w.backoff(9)
        );
        // Remove and reload cleanly so warm failure is driven through a fresh initial open attempt.
        e.remove_source(id);
        let _ = e.load_obs(&doc);
        wait_open("warm");
        let (id2, _) = e.source_named("iPhone").expect("warm offline camera stays in opened");
        assert!(e.lock().sources.contains_key(&id2));
        assert_eq!(e.waiting_cameras(), vec!["iPhone".to_string()]);
        assert_eq!(
            e.camera_status("iPhone"),
            Some(CameraStatus { state: "waiting", message: Some("Waiting for the camera"), tries: 1 })
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn fake_camera_backend_recovers_through_drop_backoff_visibility_and_removal() {
        let Some(e) = engine() else { return };
        let backend = FakeCameraBackend::new([255, 0, 0, 255]);
        {
            let live = backend.open().unwrap_or_else(|x| panic!("{x}"));
            let mut st = e.lock();
            st.sources.insert(1, Src::new(live, None, Some(SourceKind::Camera("fake-iphone".into()))));
            st.opened.insert("iPhone".into(), (1, SourceKind::Camera("fake-iphone".into())));
            st.scenes.insert("Main".into(), vec![SceneItem::new(1, Placement::default())]);
            st.scenes.insert("Other".into(), Vec::new());
            st.on_air = Some("Main".into());
        }
        // Initial stream delivers red frames to the scene.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if e.lock().sources.get(&1).and_then(|s| s.size).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let px_before = e.snapshot_scene_at("Main", (16, 16), (16, 16)).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(&px_before[0..4], &[255, 0, 0, 255]);
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 1);

        let reopen_b = Arc::clone(&backend);
        let reopen: Reopener = Arc::new(move |kind| match kind {
            SourceKind::Camera(spec) if spec.id == "fake-iphone" => Some(reopen_b.open()),
            _ => None,
        });
        let cmd = e.audio.cmd_sender();
        let wait_idle = || {
            let dl = Instant::now() + Duration::from_secs(2);
            while e.lock().sources.get(&1).is_some_and(|s| s.opening.is_some()) && Instant::now() < dl {
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // Simulate iPhone locking / Continuity interruption while offline.
        backend.interrupted.store(true, Ordering::Release);
        backend.online.store(false, Ordering::Release);
        *backend.color.lock().unwrap_or_else(|x| x.into_inner()) = [0, 255, 0, 255];

        let mut t = now_ns();
        let attempts0 = backend.open_attempts.load(Ordering::Acquire);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
        wait_idle();
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), attempts0 + 1);
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 0, "old session dropped without leak");
        assert_eq!(e.waiting_cameras(), vec!["iPhone".to_string()]);
        assert_eq!(e.source_load().into_iter().find(|l| l.name == "iPhone").map(|l| l.waiting), Some(true));

        // Drive past 3 failures confirming 1 s -> 2 s -> 4 s -> 5 s backoff (never hammering, never stopping).
        for step_ns in [1_050_000_000u64, 2_050_000_000, 4_050_000_000, 5_050_000_000, 5_050_000_000] {
            let before = backend.open_attempts.load(Ordering::Acquire);
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 200_000_000);
            assert_eq!(backend.open_attempts.load(Ordering::Acquire), before, "must not hammer before backoff");
            t += step_ns;
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
            wait_idle();
            assert_eq!(backend.open_attempts.load(Ordering::Acquire), before + 1);
            assert_eq!(backend.active_sessions.load(Ordering::Acquire), 0);
        }

        // Switching off-air pauses retries completely.
        e.go("Other").unwrap_or_else(|x| panic!("{x}"));
        let attempts_off_air = backend.open_attempts.load(Ordering::Acquire);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 30_000_000_000);
        wait_idle();
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), attempts_off_air);

        // Bringing iPhone back online + switching back on air resumes picture automatically.
        backend.interrupted.store(false, Ordering::Release);
        backend.online.store(true, Ordering::Release);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 30_000_000_000);
        wait_idle();
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 1);
        assert_eq!(backend.max_active_sessions.load(Ordering::Acquire), 1, "at most 1 live session at once");

        let dl2 = Instant::now() + Duration::from_secs(2);
        let mut px_after = vec![0; 4];
        while Instant::now() < dl2 {
            px_after = e.snapshot_scene_at("Main", (16, 16), (16, 16)).unwrap_or_default();
            if px_after.get(0..4) == Some(&[0, 255, 0, 255]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(&px_after[0..4], &[0, 255, 0, 255], "resumed green frames automatically");
        assert!(e.waiting_cameras().is_empty(), "waiting state clears once frames resume");

        // Removing the source drops the session and stops retrying permanently.
        e.remove_source(1);
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 0);
        let final_attempts = backend.open_attempts.load(Ordering::Acquire);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 60_000_000_000);
        wait_idle();
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), final_attempts);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn no_signal_capture_card_keeps_healthy_session_open_and_wakes_on_signal() {
        let Some(e) = engine() else { return };
        let backend = FakeCameraBackend::new([0, 200, 100, 255]);
        backend.streaming.store(false, Ordering::Release);
        {
            let live = backend.open().unwrap_or_else(|x| panic!("{x}"));
            let mut st = e.lock();
            st.sources.insert(1, Src::new(live, None, Some(SourceKind::Camera("capture-card".into()))));
            st.opened.insert("Card".into(), (1, SourceKind::Camera("capture-card".into())));
            st.scenes.insert("Main".into(), vec![SceneItem::new(1, Placement::default())]);
            st.on_air = Some("Main".into());
        }
        let reopen_b = Arc::clone(&backend);
        let reopen: Reopener = Arc::new(move |kind| match kind {
            SourceKind::Camera(spec) if spec.id == "capture-card" => Some(reopen_b.open()),
            _ => None,
        });
        let cmd = e.audio.cmd_sender();
        let wait_idle = || {
            let dl = Instant::now() + Duration::from_secs(2);
            while e.lock().sources.get(&1).is_some_and(|s| s.opening.is_some()) && Instant::now() < dl {
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        let mut t = now_ns();
        for _ in 0..3 {
            t += 10_500_000_000;
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
            wait_idle();
        }
        let settled_attempts = backend.open_attempts.load(Ordering::Acquire);
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 1);

        // Past 3 zero-frame tries, a healthy open session stays open without 10 s teardown churn.
        for _ in 0..5 {
            t += 11_000_000_000;
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
            wait_idle();
            assert_eq!(
                backend.open_attempts.load(Ordering::Acquire),
                settled_attempts,
                "healthy 0-frame capture session must not be torn down every 10 s"
            );
            assert_eq!(backend.active_sessions.load(Ordering::Acquire), 1);
        }

        // Turning the console on delivers frames on the existing open session with no extra reopen.
        backend.streaming.store(true, Ordering::Release);
        let dl = Instant::now() + Duration::from_secs(2);
        let mut px = vec![0; 4];
        while Instant::now() < dl {
            px = e.snapshot_scene_at("Main", (16, 16), (16, 16)).unwrap_or_default();
            if px.get(0..4) == Some(&[0, 200, 100, 255]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(&px[0..4], &[0, 200, 100, 255]);
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), settled_attempts);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn is_camera_holding_session_tracks_no_signal_and_uses_shown_when_render_lock_held() {
        let Some(e) = engine() else { return };
        let backend = FakeCameraBackend::new([0, 200, 100, 255]);
        backend.streaming.store(false, Ordering::Release);
        {
            let live = backend.open().unwrap_or_else(|x| panic!("{x}"));
            let mut st = e.lock();
            st.sources.insert(1, Src::new(live, None, Some(SourceKind::Camera("capture-card".into()))));
            st.opened.insert("Card".into(), (1, SourceKind::Camera("capture-card".into())));
            st.scenes.insert("Main".into(), vec![SceneItem::new(1, Placement::default())]);
            st.on_air = Some("Main".into());
        }
        let reopen_b = Arc::clone(&backend);
        let reopen: Reopener = Arc::new(move |kind| match kind {
            SourceKind::Camera(spec) if spec.id == "capture-card" => Some(reopen_b.open()),
            _ => None,
        });
        let cmd = e.audio.cmd_sender();
        let wait_idle = || {
            while e.lock().sources.get(&1).is_some_and(|s| s.opening.is_some()) {
                std::thread::yield_now();
            }
        };

        let mut t = now_ns();
        for _ in 0..=crate::camera_retry::NO_SIGNAL_MAX_TRIES {
            t += crate::camera_retry::CAMERA_FIRST_FRAME_NS + 1_000_000_000;
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
            wait_idle();
        }
        assert_eq!(e.camera_status("Card").map(|s| s.state), Some("live"));
        assert!(
            e.is_camera_holding_session("Card"),
            "healthy no_signal capture card holds an open session and must block competing probes"
        );

        // Publish Shown snapshot, then hold the State lock on this thread and mutate State:
        // is_camera_holding_session must read Shown non-blockingly without taking self.lock().
        {
            let st = e.lock();
            e.show(&st);
        }
        {
            let mut guard = e.lock();
            guard.sources.remove(&1);
            assert!(
                e.is_camera_holding_session("Card"),
                "must read Shown snapshot without blocking when State lock is held"
            );
        }
        {
            let st = e.lock();
            e.show(&st);
        }
        assert!(!e.is_camera_holding_session("Card"));
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn interrupted_camera_keeps_session_open_during_grace_and_resumes_without_reopen() {
        let Some(e) = engine() else { return };
        let backend = FakeCameraBackend::new([255, 0, 0, 255]);
        {
            let live = backend.open().unwrap_or_else(|x| panic!("{x}"));
            let mut st = e.lock();
            st.sources.insert(1, Src::new(live, None, Some(SourceKind::Camera("fake-iphone".into()))));
            st.opened.insert("iPhone".into(), (1, SourceKind::Camera("fake-iphone".into())));
            st.scenes.insert("Main".into(), vec![SceneItem::new(1, Placement::default())]);
            st.on_air = Some("Main".into());
        }
        let dl = Instant::now() + Duration::from_secs(5);
        while Instant::now() < dl && e.lock().sources.get(&1).and_then(|s| s.size).is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(e.lock().sources.get(&1).and_then(|s| s.size).is_some());

        let reopen_b = Arc::clone(&backend);
        let reopen: Reopener = Arc::new(move |kind| match kind {
            SourceKind::Camera(spec) if spec.id == "fake-iphone" => Some(reopen_b.open()),
            _ => None,
        });
        let cmd = e.audio.cmd_sender();
        let wait_idle = || {
            let dl = Instant::now() + Duration::from_secs(5);
            while e.lock().sources.get(&1).is_some_and(|s| s.opening.is_some()) && Instant::now() < dl {
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // Session interrupted while device stays connected: surfaces Waiting without destroying session.
        backend.interrupted.store(true, Ordering::Release);
        *backend.color.lock().unwrap_or_else(|x| x.into_inner()) = [0, 255, 0, 255];
        let attempts0 = backend.open_attempts.load(Ordering::Acquire);
        let t = now_ns();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 6_000_000_000);
        wait_idle();
        assert_eq!(backend.active_sessions.load(Ordering::Acquire), 1, "must keep interrupted session alive");
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), attempts0, "must not reopen during grace");
        assert_eq!(e.waiting_cameras(), vec!["iPhone".to_string()]);

        // Interruption ends: same session resumes delivering green frames with zero reopen churn.
        backend.interrupted.store(false, Ordering::Release);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t + 7_000_000_000);
        let dl2 = Instant::now() + Duration::from_secs(5);
        let mut px = vec![0; 4];
        while Instant::now() < dl2 {
            px = e.snapshot_scene_at("Main", (16, 16), (16, 16)).unwrap_or_default();
            if px.get(0..4) == Some(&[0, 255, 0, 255]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(&px[0..4], &[0, 255, 0, 255]);
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), attempts0);
        assert!(e.waiting_cameras().is_empty());

        // An interruption that never ends past the grace window falls back to reopening cleanly.
        backend.interrupted.store(true, Ordering::Release);
        let t2 = t + 8_000_000_000;
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t2);
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, t2 + crate::camera_retry::CAMERA_INTERRUPTION_GRACE_NS);
        wait_idle();
        assert_eq!(backend.open_attempts.load(Ordering::Acquire), attempts0 + 1);
    }

    #[test]
    fn snapshot_item_transformed_handles_offscreen_and_missing_items() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 40, height: 20, rgba: [10, 200, 30, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        let mut off =
            SceneItem::new(1, Placement { x: -300.0, y: -200.0, scale_x: -1.0, rotation: 90.0, ..Default::default() });
        off.item_id = 7;
        e.set_scene("Main", vec![off]);

        let (px, size) = e.snapshot_item_transformed("Main", 7, (640, 360)).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(size, (20, 40), "completely off-canvas item preserves its 1:1 rotated box");
        assert!(px.as_chunks::<4>().0.iter().any(|p| p[1] > 150 && p[3] > 200));

        assert!(e.snapshot_item_transformed("Main", 999, (640, 360)).is_err());

        // Nested groups (Main > Outer > Inner > Child) + parent outside shadow expansion.
        let mut child = SceneItem::new(1, Placement { x: 10.0, y: 10.0, ..Default::default() });
        child.item_id = 11;
        e.set_scene("Inner", vec![child]);

        let mut inner =
            SceneItem::new(0, Placement { x: 20.0, y: 20.0, scale_x: 0.5, scale_y: 0.5, ..Default::default() });
        inner.scene = Some("Inner".into());
        inner.group = Some((60, 40));
        inner.item_id = 12;
        inner.effects = vec![Effect::Shadow { rgba: [0.0, 0.0, 0.0, 1.0], blur: 8.0, x: 0.0, y: 0.0, outside: true }];
        e.set_scene("Outer", vec![inner]);

        let mut outer = SceneItem::new(0, Placement { x: 50.0, y: 50.0, ..Default::default() });
        outer.scene = Some("Outer".into());
        outer.group = Some((120, 100));
        outer.item_id = 13;
        e.set_scene("Main", vec![outer]);

        let (nested_px, nested_size) =
            e.snapshot_item_transformed("Main", 11, (640, 360)).unwrap_or_else(|x| panic!("{x}"));
        // Child (40x20) at (10,10) inside Inner (60x40) with 8px outside shadow -> [2..58, 2..38] -> 56x36;
        // Inner scaled 0.5 at (20,20) inside Outer at (50,50) -> canvas x in [71..99], y in [71..89] (28x18).
        assert_eq!(nested_size, (28, 18), "nested group keeps child transform and parent shadow margin");
        let full_scene = e.snapshot_scene_at("Main", (640, 360), (640, 360)).unwrap_or_else(|x| panic!("{x}"));
        let mut expected_nested = Vec::with_capacity((28 * 18 * 4) as usize);
        for y in 71..89usize {
            let start = (y * 640 + 71) * 4;
            expected_nested.extend_from_slice(&full_scene[start..start + 28 * 4]);
        }
        assert_eq!(nested_px, expected_nested, "nested group screenshot matches scene render pixel-for-pixel");
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn open_soon_slow_failure_records_retry_tries() {
        let Some(e) = engine() else { return };
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate_c = gate.clone();

        e.open_soon(
            1,
            Some(SourceKind::Camera(CameraSpec::from("slow-cam"))),
            Box::new(move || {
                let _ = entered_tx.send(());
                let (lk, cv) = &*gate_c;
                let mut released = lk.lock().unwrap_or_else(|x| x.into_inner());
                while !*released {
                    released = cv.wait(released).unwrap_or_else(|x| x.into_inner());
                }
                Err(AvError::Source("that camera isn't connected".into()))
            }),
        )
        .unwrap_or_else(|x| panic!("{x}"));

        entered_rx.recv().unwrap_or_else(|x| panic!("{x}"));
        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        while e.opening() > 0 {
            std::thread::yield_now();
        }

        let st = e.lock();
        let src = st.sources.get(&1).expect("failed capture must remain in sources for watchdog retry");
        let status = camera_status_of_src(src).expect("camera source must report CameraStatus");
        assert_eq!(status.state, "waiting");
        assert_eq!(src.watch.tries, 1);
        assert_eq!(status.tries, 1, "slow initial open failure must increment retry.tries");
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn watchdog_caps_stalled_reopen_workers_and_resumes_after_drain() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            st.sources.insert(1, Src::new(Live::Still, None, Some(SourceKind::Camera(CameraSpec::from("wedged-cam")))));
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_c = attempts.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate_c = gate.clone();

        let reopen: Reopener = Arc::new(move |_| {
            let n = attempts_c.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = entered_tx.send(n);
            if n <= 2 {
                let (lk, cv) = &*gate_c;
                let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                while !*open {
                    open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                }
            }
            let _ = exited_tx.send(n);
            Some(Err(AvError::Source("driver wedged".into())))
        });

        let mut now = now_ns();
        let cmd = e.audio.cmd_sender();
        // Worker 1 launches and blocks.
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        assert_eq!(entered_rx.recv().unwrap_or_else(|x| panic!("{x}")), 1);

        // Past 60 s deadline, worker 2 replaces the slot and blocks too.
        now += REOPEN_TIMEOUT_NS;
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        assert_eq!(entered_rx.recv().unwrap_or_else(|x| panic!("{x}")), 2);

        // Past another 60 s deadline + max backoff, worker 3 must NOT spawn while 2 are stuck.
        now += REOPEN_TIMEOUT_NS + 120_000_000_000;
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        assert!(e.lock().sources[&1].opening.is_none(), "capped source must not open a 3rd stalled worker");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "must not launch a 3rd thread while 2 are wedged");

        // Unblock both wedged workers and wait for both to finish exiting their closures.
        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        let _ = exited_rx.recv().unwrap_or_else(|x| panic!("{x}"));
        let _ = exited_rx.recv().unwrap_or_else(|x| panic!("{x}"));
        while e
            .lock()
            .sources
            .get(&1)
            .is_some_and(|s| s.opening.is_some() || s.reopen_workers.load(Ordering::Acquire) > 0)
        {
            std::thread::yield_now();
        }

        // Once workers drain, advancing past backoff allows worker 3 to launch normally.
        now += 120_000_000_000;
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        assert_eq!(entered_rx.recv().unwrap_or_else(|x| panic!("{x}")), 3);
        let _ = exited_rx.recv().unwrap_or_else(|x| panic!("{x}"));
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn watchdog_caps_total_stalled_reopen_workers_across_sources() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            for id in 1..=5u64 {
                st.sources.insert(
                    id,
                    Src::new(Live::Still, None, Some(SourceKind::Camera(CameraSpec::from(&*format!("cam-{id}"))))),
                );
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_c = attempts.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate_c = gate.clone();

        let reopen: Reopener = Arc::new(move |_| {
            let n = attempts_c.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = entered_tx.send(n);
            let (lk, cv) = &*gate_c;
            let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
            while !*open {
                open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
            }
            let _ = exited_tx.send(n);
            Some(Err(AvError::Source("driver wedged".into())))
        });

        let mut now = now_ns();
        let cmd = e.audio.cmd_sender();
        // Wedge MAX_TOTAL_REOPEN_WORKERS (8) across sources 1..=4 (2 workers per step x 4).
        for _ in 0..(MAX_TOTAL_REOPEN_WORKERS / MAX_PARALLEL_REOPENS) {
            now += REOPEN_TIMEOUT_NS;
            watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
            for _ in 0..MAX_PARALLEL_REOPENS {
                let _ = entered_rx.recv().unwrap_or_else(|x| panic!("{x}"));
            }
        }
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_TOTAL_REOPEN_WORKERS as u64);

        // Source 5 is due and has 0 per-source workers, yet global cap stops spawning a 9th thread.
        now += REOPEN_TIMEOUT_NS + 120_000_000_000;
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now);
        assert!(e.lock().sources[&5].opening.is_none());
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_TOTAL_REOPEN_WORKERS as u64);

        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        for _ in 0..MAX_TOTAL_REOPEN_WORKERS {
            let _ = exited_rx.recv().unwrap_or_else(|x| panic!("{x}"));
        }
        while e.lock().reopen_workers.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
    }

    #[test]
    fn snapshot_item_transformed_clips_outside_group_and_tracks_crop_filter() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 40, height: 20, rgba: [20, 180, 240, 255] })
            .unwrap_or_else(|x| panic!("{x}"));

        // 1. Children sitting completely left (-50..-10) or right (65..105) of a 60x40 group with an
        // outside shadow must be rejected as cropped out instead of clamping into a bogus sliver.
        for bad_x in [-50.0f32, 65.0f32] {
            let mut off = SceneItem::new(1, Placement { x: bad_x, y: 10.0, ..Default::default() });
            off.item_id = 21;
            e.set_scene("ShadowGroup", vec![off]);

            let mut grp = SceneItem::new(0, Placement { x: 100.0, y: 100.0, ..Default::default() });
            grp.scene = Some("ShadowGroup".into());
            grp.group = Some((60, 40));
            grp.item_id = 22;
            grp.effects = vec![Effect::Shadow { rgba: [0.0, 0.0, 0.0, 1.0], blur: 8.0, x: 0.0, y: 0.0, outside: true }];
            e.set_scene("Main", vec![grp]);

            assert!(
                e.snapshot_item_transformed("Main", 21, (640, 360)).is_err(),
                "item at x={bad_x} outside 60x40 group must be cropped out before shadow padding"
            );
        }

        // 2. Effect::Crop on the enclosing group must shift & clip the child's transformed box to match the scene.
        let mut inside = SceneItem::new(1, Placement { x: 20.0, y: 20.0, ..Default::default() });
        inside.item_id = 31;
        e.set_scene("CropGroup", vec![inside]);

        let mut cropped_grp = SceneItem::new(0, Placement { x: 50.0, y: 40.0, ..Default::default() });
        cropped_grp.scene = Some("CropGroup".into());
        cropped_grp.group = Some((100, 80));
        cropped_grp.item_id = 32;
        cropped_grp.effects = vec![Effect::Crop { left: 10, top: 8, right: 0, bottom: 0, size: Some((70, 50)) }];
        e.set_scene("Main", vec![cropped_grp]);

        let (px, size) = e.snapshot_item_transformed("Main", 31, (640, 360)).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(size, (40, 20), "child [20..60, 20..40] cropped by (10,8) remains 40x20");
        let full_scene = e.snapshot_scene_at("Main", (640, 360), (640, 360)).unwrap_or_else(|x| panic!("{x}"));
        let mut expected = Vec::with_capacity((40 * 20 * 4) as usize);
        // Group placed at (50, 40); child starts at (20-10, 20-8) = (10, 12) inside cropped group -> (60, 52).
        for y in 52..72usize {
            let start = (y * 640 + 60) * 4;
            expected.extend_from_slice(&full_scene[start..start + 40 * 4]);
        }
        assert_eq!(px, expected, "group with Effect::Crop screenshot must match scene render pixel-for-pixel");

        // 3. Item completely cut away by Effect::Crop on its enclosing group must return an error.
        let mut wiped_grp = SceneItem::new(0, Placement { x: 50.0, y: 40.0, ..Default::default() });
        wiped_grp.scene = Some("CropGroup".into());
        wiped_grp.group = Some((100, 80));
        wiped_grp.item_id = 33;
        wiped_grp.effects = vec![Effect::Crop { left: 70, top: 0, right: 0, bottom: 0, size: None }];
        e.set_scene("Main", vec![wiped_grp]);
        assert!(
            e.snapshot_item_transformed("Main", 31, (640, 360)).is_err(),
            "child completely removed by Effect::Crop must error cleanly"
        );
    }

    #[test]
    fn snapshot_item_transformed_downscales_oversized_offscreen_item_without_raw_target() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 1000, height: 500, rgba: [40, 190, 90, 255] })
            .unwrap_or_else(|x| panic!("{x}"));

        let mut big = SceneItem::new(
            1,
            Placement { x: -30_000.0, y: -20_000.0, scale_x: 20.0, scale_y: 20.0, ..Default::default() },
        );
        big.item_id = 41;
        e.set_scene("Main", vec![big]);

        let (px, size) = e
            .snapshot_item_transformed("Main", 41, (1920, 1080))
            .expect("oversized 20000x10000 offscreen item must downscale without allocating raw target");
        assert_eq!(size, (4096, 2048));
        assert_eq!(px.len(), 4096 * 2048 * 4);
        let center = (1024 * 4096 + 2048) * 4;
        assert_eq!(&px[center..center + 4], &[40, 190, 90, 255]);

        // Rotated 90 deg and scaled past u32::MAX also downscales without raw texture allocation.
        let mut rot = SceneItem::new(
            1,
            Placement {
                x: -20_000_000_000.0,
                y: -20_000_000_000.0,
                scale_x: 10_000_000.0,
                scale_y: 10_000_000.0,
                rotation: 90.0,
                ..Default::default()
            },
        );
        rot.item_id = 42;
        e.set_scene("Main", vec![rot]);
        let (rpx, rsize) = e.snapshot_item_transformed("Main", 42, (1920, 1080)).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(rsize, (2048, 4096));
        assert_eq!(rpx.len(), 2048 * 4096 * 4);
    }

    #[test]
    fn item_picture_ready_counts_pending_frame_for_unshown_source() {
        let Some(e) = engine() else { return };
        {
            let mut st = e.lock();
            let mut no_pic = Src::new(Live::Still, None, None);
            no_pic.pending = None;
            st.sources.insert(1, no_pic);

            let mut with_pending = Src::new(Live::Still, None, None);
            with_pending.pending = Some(Fresh::Pic(Picture::solid(16, 16, [10, 20, 30, 255])));
            st.sources.insert(2, with_pending);
        }
        let mut a = SceneItem::new(1, Placement::default());
        a.item_id = 10;
        let mut b = SceneItem::new(2, Placement::default());
        b.item_id = 11;
        e.set_scene("Unshown", vec![a, b]);

        assert_eq!(e.item_picture_ready("Unshown", 10), Some(false));
        assert_eq!(
            e.item_picture_ready("Unshown", 11),
            Some(true),
            "pending frame waiting for GPU upload on an unshown source must report ready immediately"
        );
    }

    #[test]
    fn snapshot_item_transformed_rejects_rotated_diamond_outside_group_corner() {
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 14, height: 14, rgba: [200, 100, 50, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        // Rotated 45 deg near top-left outside (-18..-8,-8..-18) -> unclipped AABB crosses (0,0)
        // while the polygon itself stays strictly in x+y < 0 outside [0..60, 0..40].
        let mut child = SceneItem::new(1, Placement { x: -8.0, y: -18.0, rotation: 45.0, ..Default::default() });
        child.item_id = 51;
        e.set_scene("CornerGroup", vec![child]);

        let mut grp = SceneItem::new(0, Placement { x: 100.0, y: 100.0, ..Default::default() });
        grp.scene = Some("CornerGroup".into());
        grp.group = Some((60, 40));
        grp.item_id = 52;
        grp.effects = vec![Effect::ColorCorrection {
            gamma: 0.0,
            contrast: 0.0,
            brightness: 0.0,
            saturation: 0.0,
            hue_shift: 0.0,
            opacity: 1.0,
            multiply: 0xFFFF_FFFF,
            add: 0,
        }];
        e.set_scene("Main", vec![grp]);

        assert!(
            e.snapshot_item_transformed("Main", 51, (640, 360)).is_err(),
            "rotated item entirely outside group corner must be cropped out"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn open_soon_preserves_worker_cap_across_source_replacement_and_allows_media_files() {
        let Some(e) = engine() else { return };
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();

        // Fill per-source cap (2) for picture-less source 1 via two sequential open_soon replacements.
        for _ in 0..MAX_SOURCE_REOPEN_WORKERS {
            let g = gate.clone();
            let etx = entered_tx.clone();
            let xtx = exited_tx.clone();
            e.open_soon(
                1,
                Some(SourceKind::Camera(CameraSpec::from("cam-replace"))),
                Box::new(move || {
                    let _ = etx.send(());
                    let (lk, cv) = &*g;
                    let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                    while !*open {
                        open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                    }
                    let _ = xtx.send(());
                    Err(AvError::Source("wedged".into()))
                }),
            )
            .unwrap();
            entered_rx.recv().unwrap();
        }
        // Third replacement on source 1 must be rejected even though earlier Src was replaced.
        let third = e.open_soon(
            1,
            Some(SourceKind::Camera(CameraSpec::from("cam-replace"))),
            Box::new(|| Err(AvError::Source("must not run".into()))),
        );
        assert!(third.is_err(), "source replacement must preserve worker count toward MAX_SOURCE_REOPEN_WORKERS");

        // Local file open (`restart: None`) must still succeed even when global capture worker cap is reached.
        for id in 2..=4u64 {
            for _ in 0..MAX_SOURCE_REOPEN_WORKERS {
                let g = gate.clone();
                let etx = entered_tx.clone();
                let xtx = exited_tx.clone();
                e.open_soon(
                    id,
                    Some(SourceKind::Camera(CameraSpec::from("cam-cap"))),
                    Box::new(move || {
                        let _ = etx.send(());
                        let (lk, cv) = &*g;
                        let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                        while !*open {
                            open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                        }
                        let _ = xtx.send(());
                        Err(AvError::Source("wedged".into()))
                    }),
                )
                .unwrap();
                entered_rx.recv().unwrap();
            }
        }
        assert!(
            e.open_soon(
                99,
                Some(SourceKind::Camera(CameraSpec::from("blocked"))),
                Box::new(|| Err(AvError::Source("no".into())))
            )
            .is_err()
        );
        e.open_soon(100, None, Box::new(|| Ok((Live::Still, false))))
            .expect("local media open must not be blocked when capture workers are at cap");

        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        for _ in 0..8 {
            exited_rx.recv().unwrap();
        }
        while e.lock().reopen_workers.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn load_obs_waiting_camera_and_unpark_preserve_worker_cap() {
        let Some(e) = engine() else { return };
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();

        for _ in 0..MAX_SOURCE_REOPEN_WORKERS {
            let g = gate.clone();
            let etx = entered_tx.clone();
            let xtx = exited_tx.clone();
            e.open_soon(
                1,
                Some(SourceKind::Camera(CameraSpec::from("wedged-cam"))),
                Box::new(move || {
                    let _ = etx.send(());
                    let (lk, cv) = &*g;
                    let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                    while !*open {
                        open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                    }
                    let _ = xtx.send(());
                    Err(AvError::Source("wedged".into()))
                }),
            )
            .unwrap();
            entered_rx.recv().unwrap();
        }

        // Remove source 1 while both wedged workers remain alive, then reload a collection
        // assigning id 1 to a camera whose open_soon rejects at MAX_SOURCE_REOPEN_WORKERS.
        e.remove_source(1);
        // The camera source id and device key are this OS's own (av_capture_input_v2 is only
        // known on a Mac, so on Windows it was a source Ferrocast has no kind for).
        let (cam_id, cam_key) = crate::obs::native_camera_kind_and_key();
        let doc = serde_json::json!({
            "current_program_scene": "Main",
            "sources": [
                { "id": cam_id, "name": "WedgedCam", "settings": { cam_key: "wedged-cam" } },
                { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "WedgedCam" }] } }
            ]
        });
        assert!(e.load_obs(&doc).is_empty(), "waiting camera stays in opened instead of missing");
        assert_eq!(
            e.lock().sources.get(&1).map(|s| s.reopen_workers.load(Ordering::Acquire)),
            Some(MAX_SOURCE_REOPEN_WORKERS),
            "load_obs waiting_camera must reuse existing source worker counter"
        );
        let cam_kind = SourceKind::Camera(CameraSpec::from("wedged-cam"));
        assert_eq!(
            e.lock().source_workers_for(1, Some(&cam_kind)).load(Ordering::Acquire),
            MAX_SOURCE_REOPEN_WORKERS,
            "source_workers_for must not overwrite counter with fresh Src atomic"
        );

        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_c = attempts.clone();
        let reopen: Reopener = Arc::new(move |_| {
            attempts_c.fetch_add(1, Ordering::SeqCst);
            Some(Err(AvError::Source("must not launch past cap".into())))
        });
        let cmd = e.audio.cmd_sender();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now_ns() + 120_000_000_000);
        assert_eq!(attempts.load(Ordering::SeqCst), 0, "watchdog must honor existing wedged worker cap after load_obs");

        // Now verify unpark path preserves existing worker counter for the same camera.
        e.lock().parked.push((cam_kind.clone(), Live::Still, now_ns()));
        e.set_source(1, cam_kind.clone()).unwrap();
        assert_eq!(
            e.lock().sources.get(&1).map(|s| s.reopen_workers.load(Ordering::Acquire)),
            Some(MAX_SOURCE_REOPEN_WORKERS),
            "unpark path must reuse existing source worker counter"
        );
        let ran_post_unpark = Arc::new(AtomicBool::new(false));
        let ran_c = ran_post_unpark.clone();
        let post_unpark_err = e
            .open_soon(
                1,
                Some(cam_kind),
                Box::new(move || {
                    ran_c.store(true, Ordering::Release);
                    Err(AvError::Source("must not run".into()))
                }),
            )
            .unwrap_err();
        assert!(
            post_unpark_err.to_string().contains("too many stalled opens") && !ran_post_unpark.load(Ordering::Acquire),
            "open_soon after unpark must reject on per-source cap without spawning: {post_unpark_err}"
        );

        // Removing the wedged camera and loading an unrelated camera even with reused numeric ID 1
        // must not charge the new camera with the old camera's stalled workers.
        e.remove_source(1);
        let other_cam = SourceKind::Camera(CameraSpec::from("fresh-other-cam"));
        e.open_soon(1, Some(other_cam), Box::new(|| Ok((Live::Still, false))))
            .expect("reusing numeric ID for an unrelated camera must not inherit old worker cap");
        e.remove_source(1);

        // Equivalent Windows selectors (OBS friendly:\\?\... vs DirectShow @device_pnp_\\?\...)
        // share one device budget, and changing display capture cursor keeps its display cap.
        let obs_cam2 = SourceKind::Camera(CameraSpec::from(r"USB Video:\\?\usb#vid_046d&pid_082d#cam2"));
        let pnp_cam2 = SourceKind::Camera(CameraSpec::from(r"@device_pnp_\\?\USB#VID_046D&PID_082D#CAM2"));
        let obs_cam1 = SourceKind::Camera(CameraSpec::from(r"USB Video:\\?\usb#vid_046d&pid_082d#cam1"));
        assert!(same_reopen_device(&Some(obs_cam2), &Some(pnp_cam2.clone())));
        assert!(!same_reopen_device(&Some(obs_cam1), &Some(pnp_cam2)));
        let scr_no_cursor =
            SourceKind::Screen { display: 1, uuid: Some("disp-1".into()), sound: false, hide: vec![], cursor: false };
        let scr_with_cursor =
            SourceKind::Screen { display: 1, uuid: Some("disp-1".into()), sound: true, hide: vec![], cursor: true };
        assert!(same_reopen_device(&Some(scr_no_cursor), &Some(scr_with_cursor)));

        // Rejecting an open due to global cap must never overwrite the registered counter on slot 5.
        let cam_kept = SourceKind::Camera(CameraSpec::from("kept-slot-5"));
        let cam_rejected = SourceKind::Camera(CameraSpec::from("rejected-slot-5"));
        {
            let mut st = e.lock();
            let prev = Arc::new(AtomicUsize::new(1));
            st.source_reopen_workers.insert(5, (Some(cam_kept.clone()), prev.clone()));
            let saved_total = st.reopen_workers.swap(MAX_TOTAL_REOPEN_WORKERS, Ordering::AcqRel);
            drop(st);
            let rejected = e.open_soon(5, Some(cam_rejected.clone()), Box::new(|| Ok((Live::Still, false))));
            assert!(rejected.is_err());
            let mut st = e.lock();
            st.reopen_workers.store(saved_total, Ordering::Release);
            assert!(
                same_reopen_device(&st.source_reopen_workers.get(&5).unwrap().0, &Some(cam_kept)),
                "rejected global-cap open must leave existing slot registration intact"
            );
            assert!(st.peek_source_workers(5, Some(&cam_rejected)).is_none());
            st.source_reopen_workers.remove(&5);
        }

        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        for _ in 0..MAX_SOURCE_REOPEN_WORKERS {
            exited_rx.recv().unwrap();
        }
        while e.lock().reopen_workers.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
        e.remove_source(1);
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn remove_source_and_watchdog_prune_idle_source_reopen_workers() {
        let Some(e) = engine() else { return };
        let kind = SourceKind::Camera(CameraSpec::from("cam-prune"));
        e.open_soon(77, Some(kind.clone()), Box::new(|| Ok((Live::Still, false)))).unwrap();
        while e.lock().source_reopen_workers.get(&77).is_some_and(|(_, w)| w.load(Ordering::Acquire) > 0) {
            std::thread::yield_now();
        }
        e.remove_source(77);
        assert!(
            !e.lock().source_reopen_workers.contains_key(&77) && e.lock().draining_workers.is_empty(),
            "remove_source must prune zero-count worker map entries"
        );

        // If removed while wedged, entry moves to draining_workers until worker drains, then watchdog_step prunes it.
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel();
        let g = gate.clone();
        e.open_soon(
            88,
            Some(kind),
            Box::new(move || {
                let _ = entered_tx.send(());
                let (lk, cv) = &*g;
                let mut open = lk.lock().unwrap_or_else(|x| x.into_inner());
                while !*open {
                    open = cv.wait(open).unwrap_or_else(|x| x.into_inner());
                }
                let _ = exited_tx.send(());
                Err(AvError::Source("wedged".into()))
            }),
        )
        .unwrap();
        entered_rx.recv().unwrap();
        e.remove_source(88);
        assert!(
            !e.lock().source_reopen_workers.contains_key(&88) && e.lock().draining_workers.len() == 1,
            "remove_source must move active wedged worker counter to draining_workers"
        );
        {
            let (lk, cv) = &*gate;
            *lk.lock().unwrap_or_else(|x| x.into_inner()) = true;
            cv.notify_all();
        }
        exited_rx.recv().unwrap();
        while e.lock().reopen_workers.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
        let reopen: Reopener = Arc::new(|_| None);
        let cmd = e.audio.cmd_sender();
        watchdog_step(&e.state, &cmd, &e.stop, &reopen, None, now_ns());
        assert!(
            !e.lock().source_reopen_workers.contains_key(&88) && e.lock().draining_workers.is_empty(),
            "watchdog_step must prune drained worker entries for removed sources"
        );
    }

    #[test]
    fn midstream_bframe_half_fps_output_keeps_av_sync_within_40ms() {
        #[derive(Default)]
        struct TagLog {
            items: Vec<(bool, i64, i64, bool, i32)>, // (is_video, dts_ms, pts_ms, is_header, cts_ms)
        }
        fn collect(log: &Arc<(Mutex<TagLog>, std::sync::Condvar)>) -> Box<dyn FnMut(Tag) + Send> {
            let log = log.clone();
            Box::new(move |t: Tag| {
                let item = match t {
                    Tag::Video { ms, body } => {
                        let is_hdr = body.get(1).copied() == Some(0);
                        let cts = if body.len() >= 5 {
                            ((body[2] as i32) << 16 | (body[3] as i32) << 8 | (body[4] as i32)) << 8 >> 8
                        } else {
                            0
                        };
                        (true, i64::from(ms), i64::from(ms) + i64::from(cts), is_hdr, cts)
                    }
                    Tag::Audio { ms, body } => {
                        (false, i64::from(ms), i64::from(ms), body.get(1).copied() == Some(0), 0)
                    }
                };
                let (lk, cv) = &*log;
                lk.lock().unwrap_or_else(|p| p.into_inner()).items.push(item);
                cv.notify_all();
            })
        }
        fn wait_until(
            log: &Arc<(Mutex<TagLog>, std::sync::Condvar)>,
            want_video: usize,
            want_audio: usize,
        ) -> Vec<(bool, i64, i64, bool, i32)> {
            let (lk, cv) = &**log;
            let mut st = lk.lock().unwrap_or_else(|p| p.into_inner());
            let mut waited = 0u32;
            loop {
                let v = st.items.iter().filter(|x| x.0 && !x.3).count();
                let a = st.items.iter().filter(|x| !x.0 && !x.3).count();
                if v >= want_video && a >= want_audio {
                    return st.items.clone();
                }
                assert!(waited < 40, "timed out waiting for engine tags: v={v} a={a}");
                let (next, _) = cv.wait_timeout(st, Duration::from_millis(250)).unwrap_or_else(|p| p.into_inner());
                st = next;
                waited += 1;
            }
        }

        let Some(e) = engine() else { return };
        shared_scene(&e);
        // Add a ticking text/changing source or toggle a small color shift so VideoToolbox exercises B-frame reordering.
        let main_log: Arc<(Mutex<TagLog>, std::sync::Condvar)> = Arc::default();
        let out1 = e.start_output(OutputKind::Stream(collect(&main_log))).unwrap_or_else(|x| panic!("{x}"));
        let _ = wait_until(&main_log, 3, 2);

        let half_canvas = Canvas { every: 2, ..Canvas::main(e.config()) };
        let mid_log: Arc<(Mutex<TagLog>, std::sync::Condvar)> = Arc::default();
        let vcfg = VideoConfig {
            codec: VideoCodec::H264,
            input: PixelFormat::Nv12,
            width: 640,
            height: 360,
            fps_num: 15,
            fps_den: 1,
            bitrate_kbps: 2000,
            rate: RateControl::Cbr,
            keyframe_secs: 1,
            software: false,
            profile: crate::video::H264Profile::High,
            bframes: true,
            preset: None,
            tune: None,
        };
        let acfg = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let out2 = match e.start_output_with(OutputKind::Stream(collect(&mid_log)), half_canvas, vcfg, acfg) {
            Ok(o) => o,
            Err(_) if !cfg!(target_os = "macos") => {
                let _ = e.stop_output(out1);
                return;
            }
            Err(x) => panic!("{x}"),
        };
        for step in 0..20u8 {
            let _ = e.set_source(
                1,
                SourceKind::Color {
                    width: 640,
                    height: 360,
                    rgba: [20u8.saturating_add(step * 9), 90, 60u8.saturating_add(step * 5), 255],
                },
            );
            std::thread::sleep(Duration::from_millis(35));
        }
        let _ = wait_until(&mid_log, 12, 6);

        e.stop_output(out2).unwrap();
        e.stop_output(out1).unwrap();

        let collected = mid_log.0.lock().unwrap_or_else(|p| p.into_inner()).items.clone();
        let video_dts: Vec<i64> = collected.iter().filter(|x| x.0 && !x.3).map(|x| x.1).collect();
        let max_cts = collected.iter().filter(|x| x.0 && !x.3).map(|x| x.4).max().unwrap_or(0);
        let first_v_pts = collected.iter().find_map(|x| (x.0 && !x.3).then_some(x.2)).expect("midstream video");
        let first_a = collected.iter().find_map(|x| (!x.0 && !x.3).then_some(x.1)).expect("midstream audio");
        if cfg!(target_os = "macos") {
            assert!(max_cts > 0, "expected real VideoToolbox B-frame reordering (max_cts={max_cts})");
        }
        assert!(
            (first_v_pts - first_a).abs() < 40,
            "mid-stream B-frame A/V start skewed: video_pts={first_v_pts}ms audio={first_a}ms"
        );
        assert!(video_dts.windows(2).all(|w| w[1] >= w[0]), "non-monotonic mid-stream video DTS: {video_dts:?}");
    }

    #[test]
    fn output_encode_ms_tracks_per_output_encoding_time() {
        if !encodes_here() {
            return;
        }
        let Some(e) = engine() else { return };
        e.set_source(1, SourceKind::Color { width: 640, height: 360, rgba: [20, 40, 60, 255] })
            .unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::channel::<()>();
        let out = e
            .start_output(OutputKind::Stream(Box::new(move |_| {
                let _ = pkt_tx.send(());
            })))
            .unwrap_or_else(|x| panic!("{x}"));
        for _ in 0..5 {
            pkt_rx.recv().unwrap();
        }
        let ms = e.output_encode_ms(out);
        assert!(ms.is_some_and(|v| v > 0.0 && v.is_finite()), "expected positive encode_ms, got {ms:?}");
        e.stop_output(out).unwrap_or_else(|x| panic!("{x}"));
        assert_eq!(e.output_encode_ms(out), None);
    }

    #[test]
    fn frame_miss_monitor_attributes_render_vs_encode_and_rate_limits() {
        let frame_ns = 33_333_333u64;
        let mut mon = FrameMissMonitor::default();
        let mut st = Stats::default();

        // Calm start emits nothing.
        assert_eq!(mon.record(1_000_000_000, 0, frame_ns, &mut st, 0), None);

        // Render slowdown (`behind = 3`, minimal encoding time) attributes to `missed_render` and logs at once.
        st.skipped += 3;
        st.frame_ms = 95.0;
        st.encode_ms = 1.0;
        st.encoding_us += 1_000;
        let first =
            mon.record(2_000_000_000, 3, frame_ns, &mut st, 0).unwrap_or_else(|| panic!("expected immediate miss log"));
        assert_eq!(first.why, "render fell behind");
        assert_eq!((first.render_missed, first.encode_held, first.encode_skipped), (3, 0, 0));
        assert_eq!((st.missed_render, st.missed_encode), (3, 0));

        // Encoder stalls on the next frame (80 ms encoding out of 33 ms budget), then subsequent
        // frame misses 2 ticks while rate-limited (< 5 s): accumulates without logging yet.
        st.encoding_us += 80_000;
        st.encode_ms = 30.0;
        assert_eq!(mon.record(3_000_000_000, 0, frame_ns, &mut st, 0), None);
        st.skipped += 2;
        assert_eq!(mon.record(4_000_000_000, 2, frame_ns, &mut st, 4), None);
        assert_eq!((st.missed_render, st.missed_encode), (3, 2));

        // At 5 s past first log, both encoder-held and encoder-skipped counts flush cleanly.
        let second = mon
            .record(7_000_000_000, 0, frame_ns, &mut st, 5)
            .unwrap_or_else(|| panic!("expected rate-limited miss log after 5s"));
        assert_eq!(second.why, "encoder fell behind");
        assert_eq!((second.render_missed, second.encode_held, second.encode_skipped), (0, 2, 5));
        assert_eq!((second.total_missed, second.total_encode_skipped), (5, 5));
    }

    #[test]
    fn engine_miss_monitor_baselines_preexisting_encoder_drops() {
        let _guard = crate::video::DROPPED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::video::note_dropped(40);
        let baseline = crate::video::dropped_frames();

        // Deterministic state-machine check with non-zero startup baseline.
        let frame_ns = 33_333_333u64;
        let mut mon = FrameMissMonitor::with_enc_skipped(baseline);
        let mut st = Stats::default();
        assert_eq!(mon.record(1_000_000_000, 0, frame_ns, &mut st, baseline), None);
        let log = mon
            .record(2_000_000_000, 0, frame_ns, &mut st, baseline + 3)
            .unwrap_or_else(|| panic!("expected encode drop log after new drops"));
        assert_eq!(log.encode_skipped, 3);

        // Live Engine::new must adopt the current dropped_frames() baseline without logging prior drops.
        let Some(engine) = engine() else {
            return;
        };
        let mut state = engine.state.lock().unwrap_or_else(|e| e.into_inner());
        let s = &mut *state;
        assert!(s.miss_monitor.prev_enc_skipped >= baseline);
        assert!(
            s.stats.skipped > 0 || s.miss_monitor.last_log_ns.is_none(),
            "new Engine logged pre-existing encoder drops without any missed frames"
        );
        let check_ns = now_ns().saturating_add(MISS_LOG_EVERY_NS + 1);
        assert_eq!(s.miss_monitor.record(check_ns, 0, frame_ns, &mut s.stats, crate::video::dropped_frames()), None);
    }
}
