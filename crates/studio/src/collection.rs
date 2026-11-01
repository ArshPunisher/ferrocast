//! What the Studio's scene collection hands out (2.1 Studio): scenes, items, sources, filters,
//! sound and transitions as the app sees them, and the shared pieces (captions, sound taps,
//! face following, the vertical picture's size). The collection itself is `stage::Stage`.

use serde::{Deserialize, Serialize};

use crate::StudioError;
use crate::ffi;

pub(crate) mod follow;
mod vertical;

pub use ffi::tap::TapReader;
pub use follow::{Face, FaceFollow, FollowLook, find_busy, find_faces, focus_x};
pub use vertical::{VERTICAL_SIZE, VerticalInfo};

/// OBS's keys for the six global sound channels (1 and 2: desktop audio, 3 to 6: mics).
pub const CHANNEL_KEYS: [(u32, &str); 6] = [
    (1, "DesktopAudioDevice1"),
    (2, "DesktopAudioDevice2"),
    (3, "AuxAudioDevice1"),
    (4, "AuxAudioDevice2"),
    (5, "AuxAudioDevice3"),
    (6, "AuxAudioDevice4"),
];

/// A scene, as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneInfo {
    pub name: String,
    /// Sources directly in it.
    pub items: usize,
    /// On air.
    pub current: bool,
    /// In Studio Mode's preview (what goes on air next).
    pub preview: bool,
    /// Its own way in, instead of the Studio's (OBS's "Transition Override").
    pub transition: Option<TransitionInfo>,
}

/// A source in a scene, top first. A group lists its own items.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemInfo {
    /// The item's id in its scene (stable while it's there).
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub locked: bool,
    /// It makes sound (shows in the mixer).
    pub audio: bool,
    pub group: bool,
    /// How it mixes with what's under it: one of [`BLENDS`].
    pub blend: String,
    /// How it's scaled: one of [`SCALE_FILTERS`].
    pub scale_filter: String,
    /// Blending with sRGB off (OBS's "SRGB Off" blending method).
    pub srgb_off: bool,
    /// Where it is on the canvas, in pixels: its box's two sides and its first corner,
    /// `[side_x.x, side_x.y, side_y.x, side_y.y, corner.x, corner.y]` (a point (u, v) of the
    /// box, each 0 to 1, is at `corner + u * side_x + v * side_y`). In a group, inside it.
    pub frame: [f32; 6],
    pub children: Vec<ItemInfo>,
    /// How it appears when shown, and goes when hidden (None: at once).
    pub show_transition: Option<TransitionInfo>,
    pub hide_transition: Option<TransitionInfo>,
    /// For video from a device or file: one of [`DEINTERLACE`]; None for other sources.
    pub deinterlace: Option<String>,
    /// Deinterlacing takes the top field first (else the bottom).
    pub top_field_first: bool,
    /// A video, playlist or slideshow with play controls.
    pub media: bool,
}

/// `enum obs_deinterlace_mode`, in order.
pub const DEINTERLACE: [&str; 9] =
    ["off", "discard", "retro", "blend", "blend_2x", "linear", "linear_2x", "yadif", "yadif_2x"];

/// `enum obs_bounds_type`, in order: none, stretch, fit inside (scale inner), fill (scale
/// outer), to width, to height, shrink only (max only).
pub const BOUNDS: [&str; 7] = ["none", "stretch", "fit", "fill", "width", "height", "max"];

/// Everything OBS's Edit Transform dialog shows for an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    /// Where its alignment point is on the canvas.
    pub x: f32,
    pub y: f32,
    /// Degrees, clockwise.
    pub rotation: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    /// Which point of it sits at (x, y): OBS's bits (0 the middle; 1 left, 2 right, 4 top,
    /// 8 bottom).
    pub alignment: u32,
    /// One of [`BOUNDS`].
    pub bounds: String,
    pub bounds_alignment: u32,
    pub bounds_width: f32,
    pub bounds_height: f32,
    pub crop_left: i32,
    pub crop_top: i32,
    pub crop_right: i32,
    pub crop_bottom: i32,
    /// The source's own size before cropping (only read).
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// A file a source (or filter, or transition) uses that isn't there any more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingFile {
    /// The source or filter's name; for a transition, "" (its kind is `kind`).
    pub source: String,
    /// For a filter: the source it's on.
    pub parent: Option<String>,
    /// The OBS kind (`image_source`, `obs_stinger_transition`…).
    pub kind: String,
    /// The setting holding it, and the option's label.
    pub key: String,
    pub label: String,
    pub path: String,
}

/// What to do to a video, playlist or slideshow (OBS's media controls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAct {
    Play,
    Pause,
    /// Pause when playing, else play.
    PlayPause,
    Restart,
    Stop,
    Next,
    Previous,
}

/// Where a video, playlist or slideshow is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaInfo {
    /// none, playing, opening, buffering, paused, stopped, ended or error.
    pub state: &'static str,
    /// Played and total, ms (0 when unknown, as for a slideshow).
    pub time_ms: i64,
    pub duration_ms: i64,
}

/// `enum obs_blending_type`, in order.
pub const BLENDS: [&str; 7] = ["normal", "additive", "subtract", "screen", "multiply", "lighten", "darken"];

/// `enum obs_scale_type`, in order.
pub const SCALE_FILTERS: [&str; 6] = ["off", "point", "bicubic", "bilinear", "lanczos", "area"];

/// A change to one item in a scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Fills the screen, ignoring its shape.
    Stretch,
    /// Moves it to the middle of the screen, its size kept.
    Center,
    /// Back to its own size at the top left, not turned or flipped.
    Reset,
    FlipH,
    FlipV,
    RotateCw,
    RotateCcw,
    Rotate180,
    /// A copy of the source (its own settings and filters) just above it.
    Duplicate,
    BlendNormal,
    BlendAdditive,
    BlendSubtract,
    BlendScreen,
    BlendMultiply,
    BlendLighten,
    BlendDarken,
    /// OBS's Blending Method: the usual one, or with sRGB off (old blending, for some
    /// effects).
    BlendMethodDefault,
    BlendMethodSrgbOff,
    ScaleOff,
    ScalePoint,
    ScaleBicubic,
    ScaleBilinear,
    ScaleLanczos,
    ScaleArea,
}

/// Something to do with a Web page source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebAct {
    /// Puts the page in a window to click and type in.
    Interact,
    /// Takes it back off-screen.
    Stop,
    /// Loads it again.
    Reload,
}

/// Who hears a sound besides the stream ("Hear it" in the mixer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Monitor {
    /// Only the stream and recordings.
    Off,
    /// Only you (headphones), not the stream.
    OnlyMe,
    /// You and the stream.
    MeAndStream,
}

/// A sound in the mixer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioInfo {
    pub name: String,
    pub kind: String,
    /// A global device (1 to 6), heard in every scene; None for one in this scene only.
    pub channel: Option<u32>,
    /// dB, from -100 (silent) to 26.
    pub volume_db: f32,
    pub muted: bool,
    pub monitor: Monitor,
    /// Left to right, 0 to 1 (0.5 is the middle).
    pub balance: f32,
    /// Heard this much later (negative: earlier), ms.
    pub sync_ms: i64,
    /// Both speakers get the same sound.
    pub mono: bool,
    /// The audio tracks (1 to 6) it's mixed into.
    pub tracks: Vec<u8>,
    /// Making sound now (OBS's "active": a playing file, an open device).
    pub active: bool,
}

/// A source and the scenes it's in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceUse {
    pub name: String,
    pub kind: String,
    pub scenes: Vec<String>,
}

/// A filter on a source, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilterInfo {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
}

/// A sound's level now, dBFS (-100 is silence).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LevelInfo {
    pub peak: [f32; 2],
    pub magnitude: [f32; 2],
}

/// How the picture goes from one scene to the next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionInfo {
    /// `fade_transition`, `cut_transition`, `swipe_transition`…
    pub kind: String,
    pub ms: u32,
}

/// How Studio Mode sends the preview on air (OBS's options under the Transition button).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioOptions {
    /// After a transition the preview shows what was on air (so the two swap).
    pub swap: bool,
    /// What goes on air is a copy of the scene: moving, hiding or showing its sources in the
    /// preview doesn't touch the stream.
    pub duplicate_scene: bool,
    /// The copy has copies of the sources too, so their settings can change without the
    /// stream seeing it (uses more of the computer; captures and media can't be copied).
    pub duplicate_sources: bool,
    /// A double-click on a scene sends it on air.
    pub double_click: bool,
}

impl Default for StudioOptions {
    fn default() -> Self {
        // OBS's defaults.
        StudioOptions { swap: true, duplicate_scene: true, duplicate_sources: false, double_click: false }
    }
}

/// Where a T-bar drag stands.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TBarState {
    /// Still held between both scenes.
    pub active: bool,
    /// 0 (the scene on air) to 1 (the preview).
    pub at: f32,
}

/// RGBA rows as a PNG data URL (a scene's tile).
pub(crate) fn png_data_url(px: &[u8], w: u32, h: u32, scene: &str) -> Result<String, StudioError> {
    use base64::Engine as _;
    {
        let mut out = Vec::with_capacity(px.len() / 4);
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(px))
            .map_err(|e| StudioError::Failed(format!("\"{scene}\"'s picture couldn't be made ({e})")))?;
        Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&out)))
    }
}

/// Live captions running on one sound; dropping it stops them.
#[derive(Debug)]
pub struct Captions {
    #[cfg(target_os = "macos")]
    pub(crate) listener: ffi::speech::Listener,
    /// Our engine's tap handed to the listener: its thread and its stop.
    pub(crate) pump: Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl Drop for Captions {
    fn drop(&mut self) {
        if let Some((stop, t)) = self.pump.take() {
            stop.store(true, std::sync::atomic::Ordering::Release);
            let _ = t.join();
        }
    }
}

impl Captions {
    /// False once recognition kept failing and stopped (turn them on again to retry).
    pub fn working(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.listener.working()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

/// Listening to one sound; dropping it stops. Can move to another thread.
#[derive(Debug)]
pub struct SoundTap {
    pub(crate) tap: Tapped,
}

/// Who listens: our engine's mixer.
#[derive(Debug)]
pub(crate) enum Tapped {
    Own(ferrocast_av::mix::Tap),
}

impl SoundTap {
    pub fn rate(&self) -> u32 {
        48_000
    }

    /// What it hears, for another thread: `take()` drains it.
    pub fn reader(&self) -> TapReader {
        match &self.tap {
            Tapped::Own(t) => TapReader::own(t),
        }
    }
}

/// Speech recognition for this app: "allowed", "not_asked", "denied", "restricted", or
/// "unsupported" where there's none.
pub fn captions_access() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        match ffi::speech::access() {
            ffi::speech::Access::Allowed => "allowed",
            ffi::speech::Access::NotAsked => "not_asked",
            ffi::speech::Access::Denied => "denied",
            ffi::speech::Access::Restricted => "restricted",
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        "unsupported"
    }
}

/// Shows macOS's prompt for speech recognition (once).
pub fn ask_captions_access() {
    #[cfg(target_os = "macos")]
    ffi::speech::ask();
}

/// A capture on air that shows nothing (`capture_problems`): `problem` is `permission`
/// (macOS's Screen Recording is off for this app), `no_picture` or `black`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CaptureProblem {
    pub source: String,
    pub kind: String,
    pub problem: &'static str,
}
