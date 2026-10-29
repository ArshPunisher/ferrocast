//! A scene collection (2.1 Studio): scenes in order, the sources in them, the global sound
//! devices, filters and the transition between scenes. libobs holds the sources; this holds a
//! reference to each scene and finds everything else by name, as OBS's own window does.
//!
//! Saved and loaded in OBS's own scene-collection format (`basic/scenes/*.json`): the same
//! `sources` array libobs writes, `scene_order`, `current_scene` and the six audio device keys,
//! so a collection can go to and from OBS.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ffi::{self, Api, Level, cstr, text};
use crate::{OptionInfo, Studio, StudioError, check_name};

mod vertical;
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

/// OBS's names for its transitions, which a scene's override is saved by, with their kinds.
const TRANSITION_NAMES: [(&str, &str); 7] = [
    ("fade_transition", "Fade"),
    ("cut_transition", "Cut"),
    ("swipe_transition", "Swipe"),
    ("slide_transition", "Slide"),
    ("fade_to_color_transition", "Fade to Color"),
    ("wipe_transition", "Luma Wipe"),
    ("obs_stinger_transition", "Stinger"),
];

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

/// An item as it was, to put back (undo): its source (held, so a removed one can come back),
/// place in the scene's order, transform and switches.
pub struct ItemSnap {
    studio: Studio,
    src: *mut ffi::ObsSource,
    pub scene: String,
    pub id: i64,
    pub name: String,
    /// From the bottom of the scene, or of its group (0 is the lowest).
    order: usize,
    /// The group it was in, if any (a source removed from a group goes back into it), and its
    /// box on the canvas then: a group's own pixels shift as its sources move, so that's where
    /// it goes back to.
    group: Option<i64>,
    canvas: Option<[f32; 6]>,
    transform: Transform,
    visible: bool,
    locked: bool,
    blend: c_int,
    blend_method: c_int,
    scale_filter: c_int,
    show: Option<TransitionInfo>,
    hide: Option<TransitionInfo>,
    deinterlace: (c_int, c_int),
}

impl std::fmt::Debug for ItemSnap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemSnap").field("scene", &self.scene).field("id", &self.id).field("name", &self.name).finish()
    }
}

impl Drop for ItemSnap {
    fn drop(&mut self) {
        self.studio.inner.api.obs_source_release(self.src);
    }
}

/// A scene as it was, to bring back once it's removed (undo): OBS's saved form of it, its
/// place in the list, and its sources held so they outlive it.
pub struct SceneSnap {
    studio: Studio,
    data: Value,
    pub name: String,
    index: usize,
    /// Each held source with its name when the scene was removed.
    held: Vec<(*mut ffi::ObsSource, String)>,
}

impl std::fmt::Debug for SceneSnap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneSnap").field("name", &self.name).field("index", &self.index).finish()
    }
}

impl Drop for SceneSnap {
    fn drop(&mut self) {
        for (s, _) in &self.held {
            self.studio.inner.api.obs_source_release(*s);
        }
    }
}

/// A source's filters as they were (kind, name, settings, on), in order, to put back (undo).
#[derive(Debug, Clone, PartialEq)]
pub struct FilterSnap {
    pub source: String,
    filters: Vec<(String, String, Value, bool)>,
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

const MEDIA_STATES: [&str; 8] = ["none", "playing", "opening", "buffering", "paused", "stopped", "ended", "error"];

/// A sound's mixer state as it was, to put back (undo).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSnap {
    pub source: String,
    volume: f32,
    muted: bool,
    monitor: c_int,
    balance: f32,
    sync: i64,
    flags: u32,
    mixers: u32,
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

/// A source reference of our own, released when dropped.
struct Ref<'a> {
    api: &'a Api,
    ptr: *mut ffi::ObsSource,
}

impl Drop for Ref<'_> {
    fn drop(&mut self) {
        self.api.obs_source_release(self.ptr);
    }
}

/// A volume meter on one source.
struct Meter {
    studio: Studio,
    vm: *mut ffi::ObsVolmeter,
    cell: Box<Mutex<Level>>,
    /// Last asked for: a meter nobody reads for a while goes.
    read: std::time::Instant,
}

/// How long a meter nobody reads is kept (several screens may each read their own sounds).
const METER_KEPT: std::time::Duration = std::time::Duration::from_secs(5);

impl Meter {
    fn param(&self) -> *mut c_void {
        &*self.cell as *const Mutex<Level> as *mut c_void
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        let a = &self.studio.inner.api;
        // Removed first, so libobs makes no more calls into `cell` before it's freed.
        a.obs_volmeter_remove_callback(self.vm, ffi::store_level, self.param());
        a.obs_volmeter_destroy(self.vm);
    }
}

const SILENT: f32 = -100.0;

fn db(v: f32) -> f32 {
    if v.is_finite() { v.max(SILENT) } else { SILENT }
}

fn monitor_of(t: i32) -> Monitor {
    match t {
        1 => Monitor::OnlyMe,
        2 => Monitor::MeAndStream,
        _ => Monitor::Off,
    }
}

/// Track numbers (1 to 6) from libobs's mixer bits.
fn tracks_of(mask: u32) -> Vec<u8> {
    (1..=6u8).filter(|t| mask & 1 << (t - 1) != 0).collect()
}

fn no_scene(name: &str) -> StudioError {
    StudioError::Failed(format!("there's no scene called \"{name}\""))
}

fn no_source(name: &str) -> StudioError {
    StudioError::Failed(format!("there's no source called \"{name}\""))
}

/// The scenes, sources and sound of one collection. Not `Send`: main thread, like [`Studio`].
pub struct Collection {
    studio: Studio,
    /// Scene sources in the order shown, each with our reference.
    scenes: RefCell<Vec<*mut ffi::ObsSource>>,
    current: RefCell<*mut ffi::ObsSource>,
    /// The transition on air (private; channel 0), our reference.
    transition: RefCell<*mut ffi::ObsSource>,
    transition_info: RefCell<TransitionInfo>,
    /// The kind of the transition on air now (a scene's override can differ from the Studio's).
    active_kind: RefCell<String>,
    meters: RefCell<HashMap<String, Meter>>,
    /// Each transition kind's own settings (a stinger's video, a swipe's direction…), used
    /// whenever that kind is made.
    transition_settings: RefCell<HashMap<String, Value>>,
    /// Play or pause asked for lately, by source: libobs applies it on the next frame, so a
    /// second press before then must not read the old state.
    media_intent: RefCell<HashMap<String, (bool, std::time::Instant)>>,
    /// Meters read true peak (between samples) instead of sample peak.
    true_peak: RefCell<bool>,
    /// Studio Mode's preview scene (one of `scenes`), or null when Studio Mode is off.
    preview: RefCell<*mut ffi::ObsSource>,
    /// OBS's quick transitions: one-click ways to send the preview on air.
    quick: RefCell<Vec<TransitionInfo>>,
    studio_options: RefCell<StudioOptions>,
    /// A T-bar drag under way: the scene on air before it (to go back to if it's let go near
    /// the start).
    tbar: RefCell<Option<*mut ffi::ObsSource>>,
    /// Faded to black: the scene that was on air, to fade back to.
    black: RefCell<Option<*mut ffi::ObsSource>>,
    /// The last transition was driven by the T-bar (libobs then reports the bar's place, not
    /// a time).
    by_hand: RefCell<bool>,
    /// Scenes whose pictures are asked for (the scene grid), kept showing between pictures as
    /// OBS's multiview does: a scene that only shows for one draw hasn't loaded its media or
    /// captures. Our reference each, with the last time asked; let go by `let_thumbs_go`.
    thumb_showing: RefCell<HashMap<String, (*mut ffi::ObsSource, std::time::Instant)>>,
    /// The vertical picture, when it's on.
    vertical: RefCell<Option<vertical::Vertical>>,
    /// The vertical picture while it's off: kept whole (its scenes and sources), drawing
    /// nothing, so turning it on again brings it back as it was (owner, 28 Oct).
    parked: RefCell<Option<vertical::Vertical>>,
    /// Items fitted before their source had a size (scene, item id, when): fitted again once
    /// it has one, so their frame hugs the picture (`settle_fits`).
    waiting_fit: RefCell<Vec<(String, i64, std::time::Instant)>>,
    /// Each capture's last size, by its item: a window resized keeps its shape on the canvas
    /// (`follow_sizes`).
    sizes_seen: RefCell<std::collections::HashMap<(usize, i64, i64), Seen>>,
    /// The last capture check and when it ran (`capture_problems`).
    captures_seen: RefCell<(Option<std::time::Instant>, Vec<CaptureProblem>)>,
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

/// The quick transitions a new collection starts with, as OBS's.
fn default_quick() -> Vec<TransitionInfo> {
    vec![
        TransitionInfo { kind: "cut_transition".into(), ms: 0 },
        TransitionInfo { kind: "fade_transition".into(), ms: 300 },
    ]
}

impl std::fmt::Debug for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collection").field("scenes", &self.scenes.borrow().len()).finish()
    }
}

impl Collection {
    /// A collection with one empty scene, on air.
    pub fn new(studio: &Studio, first_scene: &str) -> Result<Collection, StudioError> {
        let c = Collection::empty(studio)?;
        c.add_scene(first_scene)?;
        c.go(c.scenes.borrow()[0], 0);
        Ok(c)
    }

    fn empty(studio: &Studio) -> Result<Collection, StudioError> {
        let c = Collection {
            studio: studio.clone(),
            scenes: RefCell::new(Vec::new()),
            current: RefCell::new(ptr::null_mut()),
            transition: RefCell::new(ptr::null_mut()),
            transition_info: RefCell::new(TransitionInfo { kind: "fade_transition".into(), ms: 300 }),
            active_kind: RefCell::new(String::new()),
            meters: RefCell::new(HashMap::new()),
            transition_settings: RefCell::new(HashMap::new()),
            preview: RefCell::new(ptr::null_mut()),
            quick: RefCell::new(default_quick()),
            studio_options: RefCell::new(StudioOptions::default()),
            tbar: RefCell::new(None),
            black: RefCell::new(None),
            by_hand: RefCell::new(false),
            media_intent: RefCell::new(HashMap::new()),
            thumb_showing: RefCell::new(HashMap::new()),
            true_peak: RefCell::new(false),
            vertical: RefCell::new(None),
            parked: RefCell::new(None),
            waiting_fit: RefCell::new(Vec::new()),
            sizes_seen: RefCell::new(std::collections::HashMap::new()),
            captures_seen: RefCell::new((None, Vec::new())),
        };
        c.make_transition("fade_transition")?;
        Ok(c)
    }

    fn api(&self) -> &Api {
        &self.studio.inner.api
    }

    fn make_transition(&self, kind: &str) -> Result<(), StudioError> {
        let a = self.api();
        let (id, n) = (cstr(kind), cstr("Transition"));
        let settings = self.transition_settings.borrow().get(kind).cloned();
        let data = settings.map_or(ptr::null_mut(), |v| a.obs_data_create_from_json(cstr(&v.to_string()).as_ptr()));
        let t = a.obs_source_create_private(id.as_ptr(), n.as_ptr(), data);
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if t.is_null() {
            return Err(StudioError::Failed(format!("the transition \"{kind}\" isn't available")));
        }
        // The new one starts on what the old one shows (a Studio Mode copy, or black).
        let old = *self.transition.borrow();
        let on = if old.is_null() { ptr::null_mut() } else { a.obs_transition_get_active_source(old) };
        if !on.is_null() {
            a.obs_transition_set(t, on);
            a.obs_source_release(on);
        } else if old.is_null() && !self.current.borrow().is_null() {
            a.obs_transition_set(t, *self.current.borrow());
        }
        a.obs_set_output_source(0, t);
        self.active_kind.replace(kind.to_string());
        let old = self.transition.replace(t);
        if !old.is_null() {
            a.obs_source_release(old);
        }
        Ok(())
    }

    /// A public source by name, with our reference.
    fn find(&self, name: &str) -> Option<Ref<'_>> {
        let n = cstr(name);
        let p = self.api().obs_get_source_by_name(n.as_ptr());
        (!p.is_null()).then_some(Ref { api: self.api(), ptr: p })
    }

    fn source(&self, name: &str) -> Result<Ref<'_>, StudioError> {
        self.find(name).ok_or_else(|| no_source(name))
    }

    fn name_free(&self, name: &str) -> Result<(), StudioError> {
        check_name(name)?;
        // Vertical scenes have their own names in libobs (their canvas's), but names are one
        // list for people.
        if self.vertical_scene(name).is_some() || self.parked_scene(name).is_some() {
            return Err(StudioError::Failed(format!("name: there's already a source or scene called \"{name}\".")));
        }
        if let Some(src) = self.find(name) {
            if self.in_use().contains(&src.ptr) {
                return Err(StudioError::Failed(format!("name: there's already a source or scene called \"{name}\".")));
            }
            // Only an undo step holds it (it was removed): it steps aside under another name,
            // and gets its own back if the step is undone.
            let mut n = 1;
            let aside = loop {
                let candidate = format!("{name} (removed {n})");
                if self.find(&candidate).is_none() {
                    break candidate;
                }
                n += 1;
            };
            self.api().obs_source_set_name(src.ptr, cstr(&aside).as_ptr());
        }
        Ok(())
    }

    /// Every source in use: the scenes, what's in them (groups included) and the global devices.
    fn in_use(&self) -> Vec<*mut ffi::ObsSource> {
        let a = self.api();
        let mut out: Vec<*mut ffi::ObsSource> = self.held_scenes();
        for s in self.held_scenes().iter() {
            for i in self.raw_items(a.obs_scene_from_source(*s)) {
                out.push(a.obs_sceneitem_get_source(i));
                if a.obs_sceneitem_is_group(i) {
                    out.extend(self.group_items(i).into_iter().map(|g| a.obs_sceneitem_get_source(g)));
                }
            }
        }
        for (ch, _) in CHANNEL_KEYS {
            let src = a.obs_get_output_source(ch);
            if !src.is_null() {
                out.push(src);
                a.obs_source_release(src);
            }
        }
        out
    }

    /// A source an undo step held gets its own name back (if no source in use took it).
    fn reclaim_name(&self, src: *mut ffi::ObsSource, name: &str) -> Result<(), StudioError> {
        let a = self.api();
        if text(a.obs_source_get_name(src)) == name {
            return Ok(());
        }
        self.name_free(name)?;
        a.obs_source_set_name(src, cstr(name).as_ptr());
        Ok(())
    }

    /// The main picture's scenes, then the vertical one's.
    fn every_scene(&self) -> Vec<*mut ffi::ObsSource> {
        let mut all = self.scenes.borrow().clone();
        if let Some(v) = self.vertical.borrow().as_ref() {
            all.extend(v.scenes.iter().copied());
        }
        all
    }

    /// Every scene held, the vertical picture's included while it's off: what's saved and
    /// what a collection switch clears.
    fn held_scenes(&self) -> Vec<*mut ffi::ObsSource> {
        let mut all = self.every_scene();
        if let Some(v) = self.parked.borrow().as_ref() {
            all.extend(v.scenes.iter().copied());
        }
        all
    }

    /// A main-picture scene by name (what's on air, Studio Mode and transitions are its own).
    fn scene_src(&self, name: &str) -> Result<*mut ffi::ObsSource, StudioError> {
        let a = self.api();
        self.scenes
            .borrow()
            .iter()
            .copied()
            .find(|s| text(a.obs_source_get_name(*s)) == name)
            .ok_or_else(|| no_scene(name))
    }

    /// A scene by name in either picture (its sources, pictures and undo work the same).
    fn any_scene(&self, name: &str) -> Result<*mut ffi::ObsSource, StudioError> {
        self.scene_src(name).or_else(|e| self.vertical_scene(name).ok_or(e))
    }

    fn scene_ptr(&self, name: &str) -> Result<*mut ffi::ObsScene, StudioError> {
        Ok(self.api().obs_scene_from_source(self.any_scene(name)?))
    }

    /// Puts a scene on air, through the transition (`ms` 0 cuts).
    fn go(&self, scene: *mut ffi::ObsSource, ms: u32) {
        let a = self.api();
        let t = *self.transition.borrow();
        let send = self.on_air_copy(scene);
        let to = send.unwrap_or(scene);
        if ms == 0 || !a.obs_transition_start(t, ffi::OBS_TRANSITION_MODE_AUTO, ms, to) {
            a.obs_transition_set(t, to);
        }
        self.by_hand.replace(false);
        // The transition holds the copy now.
        if let Some(copy) = send {
            a.obs_source_release(copy);
        }
        self.current.replace(scene);
        self.black.replace(None);
        // A switch made elsewhere (a shortcut, a multiview click) ends any T-bar drag.
        self.tbar.replace(None);
    }

    /// In Studio Mode with "Duplicate scene" on, a private copy of `scene` to send on air (our
    /// reference), so editing the preview doesn't touch the stream.
    fn on_air_copy(&self, scene: *mut ffi::ObsSource) -> Option<*mut ffi::ObsSource> {
        let o = *self.studio_options.borrow();
        if self.preview.borrow().is_null() || !o.duplicate_scene {
            return None;
        }
        let a = self.api();
        let how = if o.duplicate_sources { ffi::OBS_SCENE_DUP_PRIVATE_COPY } else { ffi::OBS_SCENE_DUP_PRIVATE_REFS };
        let copy = a.obs_scene_duplicate(a.obs_scene_from_source(scene), a.obs_source_get_name(scene), how);
        if copy.is_null() {
            return None;
        }
        let src = a.obs_source_get_ref(a.obs_scene_get_source(copy));
        a.obs_scene_release(copy);
        (!src.is_null()).then_some(src)
    }

    /// A timed transition is still under way (a held T-bar is `tbar`).
    fn transitioning(&self) -> bool {
        if *self.by_hand.borrow() {
            return false;
        }
        let t = self.api().obs_transition_get_time(*self.transition.borrow());
        t > 0.0 && t < 1.0
    }

    // ---- Scenes ----

    pub fn scenes(&self) -> Vec<SceneInfo> {
        let a = self.api();
        let cur = *self.current.borrow();
        let pre = *self.preview.borrow();
        self.scenes
            .borrow()
            .iter()
            .map(|s| SceneInfo {
                name: text(a.obs_source_get_name(*s)),
                items: self.raw_items(a.obs_scene_from_source(*s)).len(),
                current: *s == cur,
                preview: *s == pre,
                transition: self.scene_transition(*s),
            })
            .collect()
    }

    /// The scene on air.
    pub fn current(&self) -> String {
        text(self.api().obs_source_get_name(*self.current.borrow()))
    }

    pub fn add_scene(&self, name: &str) -> Result<(), StudioError> {
        self.name_free(name)?;
        let a = self.api();
        let n = cstr(name);
        let scene = a.obs_scene_create(n.as_ptr());
        if scene.is_null() {
            return Err(StudioError::Failed(format!("the scene \"{name}\" couldn't be made")));
        }
        let src = a.obs_source_get_ref(a.obs_scene_get_source(scene));
        a.obs_scene_release(scene);
        self.scenes.borrow_mut().push(src);
        Ok(())
    }

    /// A copy of a scene showing the same sources, right after it.
    pub fn duplicate_scene(&self, name: &str, new_name: &str) -> Result<(), StudioError> {
        let scene = self.scene_ptr(name)?;
        self.name_free(new_name)?;
        let a = self.api();
        let n = cstr(new_name);
        let copy = a.obs_scene_duplicate(scene, n.as_ptr(), ffi::OBS_SCENE_DUP_REFS);
        if copy.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be copied")));
        }
        let src = a.obs_source_get_ref(a.obs_scene_get_source(copy));
        a.obs_scene_release(copy);
        // The copy is on the same picture as the scene.
        if let Some(orig) = self.vertical_scene(name) {
            if let Some(v) = self.vertical.borrow_mut().as_mut() {
                let at = v.scenes.iter().position(|s| *s == orig).map_or(v.scenes.len(), |i| i + 1);
                v.scenes.insert(at, src);
            }
            return Ok(());
        }
        let at = self.index_of(name)? + 1;
        self.scenes.borrow_mut().insert(at, src);
        Ok(())
    }

    /// Where a scene is in its picture's list.
    fn index_of(&self, name: &str) -> Result<usize, StudioError> {
        if let Some(src) = self.vertical_scene(name) {
            return Ok(self
                .vertical
                .borrow()
                .as_ref()
                .and_then(|v| v.scenes.iter().position(|s| *s == src))
                .unwrap_or(0));
        }
        let src = self.scene_src(name)?;
        Ok(self.scenes.borrow().iter().position(|s| *s == src).unwrap_or(0))
    }

    /// Removes a scene (never the last one). If it was on air, the one before it goes on.
    pub fn remove_scene(&self, name: &str) -> Result<(), StudioError> {
        if let Some(v) = self.vertical_scene(name) {
            return self.remove_vertical_scene(v);
        }
        let src = self.scene_src(name)?;
        if self.scenes.borrow().len() == 1 {
            return Err(StudioError::Failed("A collection keeps at least one scene. Add another first.".into()));
        }
        if self.tbar.borrow().is_some() {
            return Err(StudioError::Failed("Let go of the T-bar first.".into()));
        }
        let at = self.index_of(name)?;
        self.scenes.borrow_mut().remove(at);
        if *self.current.borrow() == src {
            let next = self.scenes.borrow()[at.saturating_sub(1)];
            if self.black.borrow().is_some() {
                // Stays black: fading back brings the next one in.
                self.current.replace(next);
                self.black.replace(Some(next));
            } else {
                self.go(next, 0);
            }
        }
        if *self.preview.borrow() == src {
            let next = *self.current.borrow();
            self.preview.replace(next);
        }
        let a = self.api();
        a.obs_source_remove(src);
        a.obs_source_release(src);
        Ok(())
    }

    /// Moves a scene to `to` in the list (0 is the top).
    pub fn move_scene(&self, name: &str, to: usize) -> Result<(), StudioError> {
        if let Some(v) = self.vertical_scene(name) {
            self.move_vertical_scene(v, to);
            return Ok(());
        }
        let at = self.index_of(name)?;
        let mut v = self.scenes.borrow_mut();
        let s = v.remove(at);
        let to = to.min(v.len());
        v.insert(to, s);
        Ok(())
    }

    /// Puts a scene on air through the transition.
    pub fn set_current(&self, name: &str) -> Result<(), StudioError> {
        if self.vertical_scene(name).is_some() {
            return self.set_vertical_current(name);
        }
        let src = self.scene_src(name)?;
        // The scene's own transition if it has one, else the Studio's.
        let studio = self.transition_info.borrow().clone();
        let mut want = self.scene_transition(src).unwrap_or_else(|| studio.clone());
        if want.kind != *self.active_kind.borrow() && self.make_transition(&want.kind).is_err() {
            // The scene's own transition isn't on this computer: the Studio's goes instead.
            tracing::warn!(scene = name, kind = want.kind, "a scene's transition isn't available; using the Studio's");
            want = studio;
            if want.kind != *self.active_kind.borrow() {
                self.make_transition(&want.kind)?;
            }
        }
        self.go(src, want.ms);
        Ok(())
    }

    // ---- Studio Mode ----

    /// Studio Mode's preview scene, or None when it's off.
    pub fn preview_scene(&self) -> Option<String> {
        let p = *self.preview.borrow();
        (!p.is_null()).then(|| text(self.api().obs_source_get_name(p)))
    }

    /// Turns Studio Mode on (the preview starts as what's on air) or off.
    pub fn set_studio_mode(&self, on: bool) {
        if on == !self.preview.borrow().is_null() {
            return;
        }
        let next = if on { *self.current.borrow() } else { ptr::null_mut() };
        self.preview.replace(next);
        self.tbar.replace(None);
        if self.black.borrow().is_none() {
            // On: what's on air becomes a copy (with "Duplicate scene"); off: the scene itself.
            // The picture doesn't change.
            let cur = *self.current.borrow();
            self.go(cur, 0);
        }
    }

    pub fn studio_options(&self) -> StudioOptions {
        *self.studio_options.borrow()
    }

    /// Changes how Studio Mode sends scenes on air. A change to copying puts what's on air
    /// again (as OBS does), never mid-transition.
    pub fn set_studio_options(&self, o: StudioOptions) -> Result<(), StudioError> {
        let was = self.studio_options.replace(o);
        let copying = |o: StudioOptions| (o.duplicate_scene, o.duplicate_scene && o.duplicate_sources);
        if copying(was) != copying(o) && !self.preview.borrow().is_null() && self.black.borrow().is_none() {
            if self.tbar.borrow().is_some() || self.transitioning() {
                self.studio_options.replace(was);
                return Err(StudioError::Failed("Wait for the transition to end, then change this.".into()));
            }
            let cur = *self.current.borrow();
            self.go(cur, 0);
        }
        Ok(())
    }

    /// Swaps the preview and what's on air, with a cut (Studio Mode).
    pub fn swap_scenes(&self) -> Result<(), StudioError> {
        let pre = *self.preview.borrow();
        if pre.is_null() {
            return Err(StudioError::Failed("Studio Mode is off.".into()));
        }
        if self.tbar.borrow().is_some() || self.transitioning() {
            return Err(StudioError::Failed("Wait for the transition to end.".into()));
        }
        let cur = *self.current.borrow();
        self.go(pre, 0);
        self.preview.replace(cur);
        Ok(())
    }

    /// Fades what's on air to black, or back from black to it.
    pub fn fade_to_black(&self) -> Result<bool, StudioError> {
        if self.tbar.borrow().is_some() {
            return Err(StudioError::Failed("Let go of the T-bar first.".into()));
        }
        let a = self.api();
        let ms = self.transition_info.borrow().ms.max(300);
        if self.black.borrow().is_some() {
            // Back to the scene on air.
            let cur = *self.current.borrow();
            self.go_kind_fade(ms, cur);
            self.black.replace(None);
            return Ok(false);
        }
        // A cut can't fade: a fade does it.
        if *self.active_kind.borrow() == "cut_transition" {
            self.make_transition("fade_transition")?;
        }
        let t = *self.transition.borrow();
        if !a.obs_transition_start(t, ffi::OBS_TRANSITION_MODE_AUTO, ms, ptr::null_mut()) {
            a.obs_transition_set(t, ptr::null_mut());
        }
        self.by_hand.replace(false);
        self.black.replace(Some(*self.current.borrow()));
        Ok(true)
    }

    /// Faded to black now.
    pub fn is_black(&self) -> bool {
        self.black.borrow().is_some()
    }

    fn go_kind_fade(&self, ms: u32, scene: *mut ffi::ObsSource) {
        if *self.active_kind.borrow() == "cut_transition" {
            let _ = self.make_transition("fade_transition");
        }
        self.go(scene, ms);
    }

    /// Moves Studio Mode's T-bar to `at` (0 is what's on air, 1 the preview). The first move
    /// starts a hand-driven transition; letting go (`release`) near either end finishes it or
    /// goes back, anywhere else holds both scenes mixed, as OBS does.
    pub fn tbar(&self, at: f32, release: bool) -> Result<TBarState, StudioError> {
        let pre = *self.preview.borrow();
        if pre.is_null() {
            return Err(StudioError::Failed("Studio Mode is off.".into()));
        }
        if !at.is_finite() || !(0.0..=1.0).contains(&at) {
            return Err(StudioError::Failed("at: 0 to 1.".into()));
        }
        let a = self.api();
        if self.tbar.borrow().is_none() {
            if release {
                return Ok(TBarState { active: false, at: 0.0 });
            }
            let cur = *self.current.borrow();
            if (pre == cur && !self.studio_options.borrow().duplicate_scene) || self.transitioning() {
                return Ok(TBarState { active: false, at: 0.0 });
            }
            // The scene's own transition, else the Studio's; a cut or stinger can't be held
            // part way, so a fade drives the T-bar instead.
            let by_hand = |k: &str| k != "cut_transition" && k != "obs_stinger_transition";
            let studio = self.transition_info.borrow().clone();
            let kind = self
                .scene_transition(pre)
                .map(|t| t.kind)
                .filter(|k| by_hand(k))
                .or_else(|| by_hand(&studio.kind).then(|| studio.kind.clone()))
                .unwrap_or_else(|| "fade_transition".into());
            if kind != *self.active_kind.borrow() && self.make_transition(&kind).is_err() {
                self.make_transition("fade_transition")?;
            }
            let t = *self.transition.borrow();
            a.obs_transition_set_manual_torque(t, 8.0, 0.05);
            let send = self.on_air_copy(pre);
            let started = a.obs_transition_start(t, ffi::OBS_TRANSITION_MODE_MANUAL, 300, send.unwrap_or(pre));
            if let Some(copy) = send {
                a.obs_source_release(copy);
            }
            if !started {
                return Ok(TBarState { active: false, at: 0.0 });
            }
            self.tbar.replace(Some(cur));
            self.by_hand.replace(true);
            self.current.replace(pre);
            self.black.replace(None);
        }
        let t = *self.transition.borrow();
        if !release {
            a.obs_transition_set_manual_time(t, at.clamp(0.01, 0.99));
            return Ok(TBarState { active: true, at });
        }
        if at >= 0.9 {
            a.obs_transition_set_manual_time(t, 1.0);
            let was = self.tbar.replace(None);
            if let Some(old) = was {
                self.swap_after(old);
            }
            Ok(TBarState { active: false, at: 0.0 })
        } else if at <= 0.1 {
            // Back to what was on air (OBS once kept the preview as "on air" here).
            let src = a.obs_transition_get_source(t, ffi::OBS_TRANSITION_SOURCE_A);
            a.obs_transition_set(t, src);
            if !src.is_null() {
                a.obs_source_release(src);
            }
            if let Some(old) = self.tbar.replace(None) {
                self.current.replace(old);
            }
            Ok(TBarState { active: false, at: 0.0 })
        } else {
            a.obs_transition_set_manual_time(t, at);
            Ok(TBarState { active: true, at })
        }
    }

    /// Where the T-bar stands now.
    pub fn tbar_state(&self) -> TBarState {
        if self.tbar.borrow().is_none() {
            return TBarState { active: false, at: 0.0 };
        }
        TBarState { active: true, at: self.api().obs_transition_get_time(*self.transition.borrow()) }
    }

    /// With "Swap" on, the preview shows what was on air before (if it's still a scene).
    fn swap_after(&self, old: *mut ffi::ObsSource) {
        if !self.studio_options.borrow().swap || self.preview.borrow().is_null() {
            return;
        }
        if old != *self.current.borrow() && self.scenes.borrow().contains(&old) {
            self.preview.replace(old);
        }
    }

    /// Picks the scene Studio Mode's preview shows (to edit, then send on air).
    pub fn set_preview(&self, name: &str) -> Result<(), StudioError> {
        if self.preview.borrow().is_null() {
            return Err(StudioError::Failed("Studio Mode is off.".into()));
        }
        let src = self.scene_src(name)?;
        self.preview.replace(src);
        Ok(())
    }

    /// Sends the preview on air: with `quick` (a quick transition), else as a click on the scene
    /// would (its own transition, or the Studio's).
    pub fn transition_now(&self, quick: Option<&TransitionInfo>) -> Result<(), StudioError> {
        let pre = *self.preview.borrow();
        if pre.is_null() {
            return Err(StudioError::Failed("Studio Mode is off.".into()));
        }
        if self.tbar.borrow().is_some() {
            return Err(StudioError::Failed("Let go of the T-bar first.".into()));
        }
        // One at a time, as in OBS: a click mid-transition does nothing.
        if self.transitioning() {
            return Ok(());
        }
        let old = *self.current.borrow();
        // Nothing would change on air.
        if old == pre && !self.studio_options.borrow().duplicate_scene && self.black.borrow().is_none() {
            return Ok(());
        }
        let name = text(self.api().obs_source_get_name(pre));
        match quick {
            None => self.set_current(&name)?,
            Some(t) => {
                if t.ms > 20_000 {
                    return Err(StudioError::Failed("ms: a transition lasts 0 to 20000 ms.".into()));
                }
                if t.kind != *self.active_kind.borrow() {
                    self.make_transition(&t.kind)?;
                }
                self.go(pre, if t.kind == "cut_transition" { 0 } else { t.ms });
            }
        }
        self.swap_after(old);
        Ok(())
    }

    pub fn quick_transitions(&self) -> Vec<TransitionInfo> {
        self.quick.borrow().clone()
    }

    /// Replaces the quick transitions (up to 10, each one of the Studio's transitions).
    pub fn set_quick_transitions(&self, list: &[TransitionInfo]) -> Result<(), StudioError> {
        if list.len() > 10 {
            return Err(StudioError::Failed("Up to 10 quick transitions.".into()));
        }
        for t in list {
            if !TRANSITION_NAMES.iter().any(|(k, _)| *k == t.kind) || t.ms > 20_000 {
                return Err(StudioError::Failed(format!("\"{}\" isn't a transition here", t.kind)));
            }
        }
        self.quick.replace(list.to_vec());
        Ok(())
    }

    /// A scene's own transition, saved as OBS saves it (its private "transition" and
    /// "transition_duration"), so collections from OBS keep theirs.
    fn scene_transition(&self, scene: *mut ffi::ObsSource) -> Option<TransitionInfo> {
        let a = self.api();
        let d = a.obs_source_get_private_settings(scene);
        if d.is_null() {
            return None;
        }
        let name = text(a.obs_data_get_string(d, c"transition".as_ptr()));
        let ms = a.obs_data_get_int(d, c"transition_duration".as_ptr());
        a.obs_data_release(d);
        let kind = TRANSITION_NAMES.iter().find(|(_, n)| *n == name)?.0;
        // OBS leaves the duration out when it's the default (300 ms); Cut takes none.
        let ms = if kind == "cut_transition" {
            0
        } else if ms > 0 {
            ms.min(20_000) as u32
        } else {
            300
        };
        Some(TransitionInfo { kind: kind.into(), ms })
    }

    /// Gives a scene its own transition, or (None) takes it away.
    pub fn set_scene_transition(&self, scene: &str, t: Option<&TransitionInfo>) -> Result<(), StudioError> {
        let src = self.scene_src(scene)?;
        let (name, ms) = match t {
            Some(t) => {
                if t.ms > 20_000 {
                    return Err(StudioError::Failed("ms: a transition lasts 0 to 20000 ms.".into()));
                }
                let n = TRANSITION_NAMES.iter().find(|(k, _)| *k == t.kind).map(|(_, n)| *n).ok_or_else(|| {
                    StudioError::Failed("kind: fade, cut, swipe, slide, fade to colour or wipe.".into())
                })?;
                (n, i64::from(t.ms))
            }
            None => ("", 0),
        };
        let a = self.api();
        let d = a.obs_source_get_private_settings(src);
        if d.is_null() {
            return Err(StudioError::Failed(format!("\"{scene}\" couldn't keep its transition")));
        }
        let n = cstr(name);
        a.obs_data_set_string(d, c"transition".as_ptr(), n.as_ptr());
        a.obs_data_set_int(d, c"transition_duration".as_ptr(), ms);
        a.obs_data_release(d);
        Ok(())
    }

    pub fn transition(&self) -> TransitionInfo {
        self.transition_info.borrow().clone()
    }

    pub fn set_transition(&self, t: &TransitionInfo) -> Result<(), StudioError> {
        if t.ms > 20_000 {
            return Err(StudioError::Failed("ms: a transition lasts 0 to 20000 ms.".into()));
        }
        if t.kind != *self.active_kind.borrow() {
            self.make_transition(&t.kind)?;
        }
        self.transition_info.replace(t.clone());
        Ok(())
    }

    /// A transition kind's own settings (only what was set; OBS fills in the rest).
    pub fn transition_settings(&self, kind: &str) -> Value {
        self.transition_settings.borrow().get(kind).cloned().unwrap_or_else(|| json!({}))
    }

    /// Changes the keys given of a transition kind's settings; the one on air takes them at once.
    pub fn set_transition_settings(&self, kind: &str, settings: &Value) -> Result<(), StudioError> {
        let Some(patch) = settings.as_object() else {
            return Err(StudioError::Failed("settings: an object of OBS's keys.".into()));
        };
        if !TRANSITION_NAMES.iter().any(|(k, _)| *k == kind) {
            return Err(StudioError::Failed(format!("\"{kind}\" isn't a transition here")));
        }
        let merged = {
            let mut all = self.transition_settings.borrow_mut();
            let entry = all.entry(kind.to_string()).or_insert_with(|| json!({}));
            if let Some(obj) = entry.as_object_mut() {
                for (k, v) in patch {
                    obj.insert(k.clone(), v.clone());
                }
            }
            entry.clone()
        };
        if *self.active_kind.borrow() == kind {
            let a = self.api();
            let data = a.obs_data_create_from_json(cstr(&merged.to_string()).as_ptr());
            if !data.is_null() {
                a.obs_source_update(*self.transition.borrow(), data);
                a.obs_data_release(data);
            }
        }
        Ok(())
    }

    /// Renames a source or a scene (names are unique across both, as in OBS).
    pub fn rename(&self, name: &str, new_name: &str) -> Result<(), StudioError> {
        if name == new_name {
            return Ok(());
        }
        // A vertical scene isn't among libobs's named sources.
        let src = match self.vertical_scene(name) {
            Some(v) => Ref { api: self.api(), ptr: self.api().obs_source_get_ref(v) },
            None => self.source(name)?,
        };
        self.name_free(new_name)?;
        let n = cstr(new_name);
        self.api().obs_source_set_name(src.ptr, n.as_ptr());
        // Taken out first: the map's borrow ends before the meter goes back under the new name.
        let meter = self.meters.borrow_mut().remove(name);
        if let Some(m) = meter {
            self.meters.borrow_mut().insert(new_name.to_string(), m);
        }
        Ok(())
    }

    // ---- Items ----

    fn raw_items(&self, scene: *mut ffi::ObsScene) -> Vec<*mut ffi::ObsSceneItem> {
        let mut out: Vec<*mut ffi::ObsSceneItem> = Vec::new();
        if !scene.is_null() {
            self.api().obs_scene_enum_items(scene, ffi::collect_items, &mut out as *mut _ as *mut c_void);
        }
        out
    }

    fn group_items(&self, item: *mut ffi::ObsSceneItem) -> Vec<*mut ffi::ObsSceneItem> {
        let mut out: Vec<*mut ffi::ObsSceneItem> = Vec::new();
        self.api().obs_sceneitem_group_enum_items(item, ffi::collect_items, &mut out as *mut _ as *mut c_void);
        out
    }

    fn info(&self, item: *mut ffi::ObsSceneItem) -> ItemInfo {
        let a = self.api();
        let src = a.obs_sceneitem_get_source(item);
        let group = a.obs_sceneitem_is_group(item);
        ItemInfo {
            id: a.obs_sceneitem_get_id(item),
            name: text(a.obs_source_get_name(src)),
            kind: text(a.obs_source_get_id(src)),
            visible: a.obs_sceneitem_visible(item),
            locked: a.obs_sceneitem_locked(item),
            audio: a.obs_source_get_output_flags(src) & ffi::OBS_SOURCE_AUDIO != 0,
            group,
            blend: BLENDS
                .get(usize::try_from(a.obs_sceneitem_get_blending_mode(item)).unwrap_or(0))
                .unwrap_or(&BLENDS[0])
                .to_string(),
            scale_filter: SCALE_FILTERS
                .get(usize::try_from(a.obs_sceneitem_get_scale_filter(item)).unwrap_or(0))
                .unwrap_or(&SCALE_FILTERS[0])
                .to_string(),
            srgb_off: a.obs_sceneitem_get_blending_method(item) == 1,
            frame: self.place(item).frame(),
            children: if group {
                // A group's sources keep their place inside the group; the page works on the
                // canvas, so their boxes go out in canvas pixels.
                let m = self.group_matrix(item);
                self.group_items(item)
                    .into_iter()
                    .rev()
                    .map(|i| {
                        let mut c = self.info(i);
                        c.frame = on_canvas(c.frame, &m);
                        c
                    })
                    .collect()
            } else {
                Vec::new()
            },
            show_transition: self.item_transition(item, true),
            hide_transition: self.item_transition(item, false),
            deinterlace: (a.obs_source_get_output_flags(src) & ffi::OBS_SOURCE_ASYNC != 0).then(|| {
                DEINTERLACE
                    .get(usize::try_from(a.obs_source_get_deinterlace_mode(src)).unwrap_or(0))
                    .unwrap_or(&DEINTERLACE[0])
                    .to_string()
            }),
            top_field_first: a.obs_source_get_deinterlace_field_order(src) == 0,
            media: a.obs_source_get_output_flags(src) & ffi::OBS_SOURCE_CONTROLLABLE_MEDIA != 0,
        }
    }

    /// An item's show (or hide) transition.
    fn item_transition(&self, item: *mut ffi::ObsSceneItem, show: bool) -> Option<TransitionInfo> {
        let a = self.api();
        let t = a.obs_sceneitem_get_transition(item, show);
        if t.is_null() {
            return None;
        }
        Some(TransitionInfo {
            kind: text(a.obs_source_get_id(t)),
            ms: a.obs_sceneitem_get_transition_duration(item, show),
        })
    }

    /// Gives an item a transition for when it's shown (or hidden), or none (`None`). It uses
    /// that kind's settings as the Studio has them.
    pub fn set_item_transition(
        &self,
        scene: &str,
        id: i64,
        show: bool,
        t: Option<&TransitionInfo>,
    ) -> Result<(), StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
        let Some(t) = t else {
            a.obs_sceneitem_set_transition(item, show, ptr::null_mut());
            return Ok(());
        };
        if t.ms > 20_000 {
            return Err(StudioError::Failed("ms: a transition lasts 0 to 20000 ms.".into()));
        }
        if !TRANSITION_NAMES.iter().any(|(k, _)| *k == t.kind) {
            return Err(StudioError::Failed("kind: fade, cut, swipe, slide, fade to colour, wipe or stinger.".into()));
        }
        let settings = self.transition_settings.borrow().get(&t.kind).cloned();
        let data = settings.map_or(ptr::null_mut(), |v| a.obs_data_create_from_json(cstr(&v.to_string()).as_ptr()));
        let (k, n) = (cstr(&t.kind), cstr(if show { "Show transition" } else { "Hide transition" }));
        let src = a.obs_source_create_private(k.as_ptr(), n.as_ptr(), data);
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if src.is_null() {
            return Err(StudioError::Failed(format!("the transition \"{}\" isn't available", t.kind)));
        }
        a.obs_sceneitem_set_transition(item, show, src);
        a.obs_source_release(src);
        a.obs_sceneitem_set_transition_duration(item, show, t.ms);
        Ok(())
    }

    /// A source's deinterlacing now: its mode and whether the top field goes first.
    pub fn deinterlace_of(&self, name: &str) -> Result<(String, bool), StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        let m = usize::try_from(a.obs_source_get_deinterlace_mode(src.ptr)).unwrap_or(0);
        Ok((
            DEINTERLACE.get(m).unwrap_or(&DEINTERLACE[0]).to_string(),
            a.obs_source_get_deinterlace_field_order(src.ptr) == 0,
        ))
    }

    /// How video from a device or file is deinterlaced (one of [`DEINTERLACE`]).
    pub fn set_deinterlace(&self, name: &str, mode: &str, top_field_first: bool) -> Result<(), StudioError> {
        let m = DEINTERLACE
            .iter()
            .position(|d| *d == mode)
            .ok_or_else(|| StudioError::Failed(format!("deinterlace: one of {}.", DEINTERLACE.join(", "))))?;
        let src = self.source(name)?;
        let a = self.api();
        if a.obs_source_get_output_flags(src.ptr) & ffi::OBS_SOURCE_ASYNC == 0 {
            return Err(StudioError::Failed(format!("\"{name}\" isn't video from a device or file.")));
        }
        a.obs_source_set_deinterlace_mode(src.ptr, c_int::try_from(m).unwrap_or(0));
        a.obs_source_set_deinterlace_field_order(src.ptr, if top_field_first { 0 } else { 1 });
        Ok(())
    }

    /// A scene's sources, top first (as OBS lists them).
    pub fn items(&self, scene: &str) -> Result<Vec<ItemInfo>, StudioError> {
        let s = self.scene_ptr(scene)?;
        Ok(self.raw_items(s).into_iter().rev().map(|i| self.info(i)).collect())
    }

    fn item(&self, scene: &str, id: i64) -> Result<*mut ffi::ObsSceneItem, StudioError> {
        let s = self.scene_ptr(scene)?;
        for i in self.raw_items(s) {
            if self.api().obs_sceneitem_get_id(i) == id {
                return Ok(i);
            }
            if self.api().obs_sceneitem_is_group(i)
                && let Some(c) = self.group_items(i).into_iter().find(|c| self.api().obs_sceneitem_get_id(*c) == id)
            {
                return Ok(c);
            }
        }
        Err(StudioError::Failed(format!("there's no source with id {id} in \"{scene}\"")))
    }

    /// Adds a new source of `kind` to a scene, on top; with `fit`, fitted to the canvas.
    /// Returns its item.
    pub fn add_source(
        &self,
        scene: &str,
        kind: &str,
        name: &str,
        settings: &Value,
        fit: bool,
    ) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.name_free(name)?;
        let a = self.api();
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        let (id, n) = (cstr(kind), cstr(name));
        let src = a.obs_source_create(id.as_ptr(), n.as_ptr(), data, ptr::null_mut());
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if src.is_null() {
            return Err(StudioError::Failed(format!("the source \"{name}\" ({kind}) couldn't be made")));
        }
        let item = a.obs_scene_add(s, src);
        // The scene holds it now (or nothing does and it goes).
        a.obs_source_release(src);
        if item.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        if fit {
            self.fit(item);
        }
        Ok(self.info(item))
    }

    /// Adds a source that already exists (in another scene) to a scene, on top.
    pub fn add_existing(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        let src = self.source(name)?;
        if src.ptr == self.any_scene(scene).unwrap_or(ptr::null_mut()) {
            return Err(StudioError::Failed("A scene can't be put inside itself.".into()));
        }
        let item = self.api().obs_scene_add(s, src.ptr);
        if item.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        Ok(self.info(item))
    }

    /// The size of the picture an item is in.
    fn size_of_item(&self, item: *mut ffi::ObsSceneItem) -> (u32, u32) {
        self.size_of(self.api().obs_sceneitem_get_scene(item))
    }

    /// Fits an item inside its picture, centred, keeping its shape. The source itself is sized
    /// (no bounding box), so its frame is exactly the picture it shows: in a tall picture a wide
    /// source would otherwise get a frame the whole picture's size round a strip of picture.
    /// A source with no size yet (a capture that hasn't sent a frame), or one turned or
    /// flipped, gets OBS's bounding box instead, which fits whatever it later shows.
    fn fit(&self, item: *mut ffi::ObsSceneItem) {
        let a = self.api();
        let c = Size::of(self.size_of_item(item));
        let p = self.place(item);
        let upright = p.rot.rem_euclid(360.0) < 0.01 && p.scale.0 > 0.0 && p.scale.1 > 0.0;
        if upright && p.size.0 > 0 && p.size.1 > 0 && c.width > 0 && c.height > 0 {
            let (w, h) = (p.size.0 as f32, p.size.1 as f32);
            let k = (c.width as f32 / w).min(c.height as f32 / h);
            a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_NONE);
            a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
            a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: k, y: k });
            a.obs_sceneitem_set_pos(
                item,
                &ffi::Vec2 {
                    x: ((c.width as f32 - w * k) / 2.0).round(),
                    y: ((c.height as f32 - h * k) / 2.0).round(),
                },
            );
            return;
        }
        if upright && p.size == (0, 0) {
            // No frame yet: fitted again when it has one.
            let scene = text(a.obs_source_get_name(a.obs_scene_get_source(a.obs_sceneitem_get_scene(item))));
            let id = a.obs_sceneitem_get_id(item);
            let mut w = self.waiting_fit.borrow_mut();
            w.retain(|(s, i, _)| !(s == &scene && *i == id));
            w.push((scene, id, std::time::Instant::now()));
        }
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
        a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_SCALE_INNER);
        a.obs_sceneitem_set_bounds_alignment(item, ffi::OBS_ALIGN_CENTER);
        a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: c.width as f32, y: c.height as f32 });
    }

    /// Takes a fitted item out of its bounding box, its picture staying where it shows in it
    /// (scaled, no box): its frame is then the picture itself. Only for an upright item whose
    /// box centres it (as `fit` makes it); others are left as they are.
    fn unbox(&self, item: *mut ffi::ObsSceneItem) {
        let a = self.api();
        let p = self.place(item);
        let Some((bw, bh)) = p.bounds else { return };
        let (w, h) = (p.size.0 as f32, p.size.1 as f32);
        if w <= 0.0 || h <= 0.0 || p.rot.rem_euclid(360.0) > 0.01 || p.scale.0 <= 0.0 || p.scale.1 <= 0.0 {
            return;
        }
        if a.obs_sceneitem_get_bounds_alignment(item) != ffi::OBS_ALIGN_CENTER {
            return;
        }
        let k = (bw / w).min(bh / h);
        let f = p.frame();
        let x = f[4] + (bw - w * k) / 2.0;
        let y = f[5] + (bh - h * k) / 2.0;
        a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_NONE);
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
        a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: k, y: k });
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: x.round(), y: y.round() });
    }

    /// The middle of the item's box on the canvas, as it is now (libobs works the box out
    /// only on its next frame otherwise: a new item's is still empty).
    fn box_middle(&self, item: *mut ffi::ObsSceneItem) -> ffi::Vec2 {
        let mut m = ffi::Matrix4::default();
        self.api().obs_sceneitem_force_update_transform(item);
        self.api().obs_sceneitem_get_box_transform(item, &mut m);
        ffi::Vec2 { x: (m.x.x + m.y.x) * 0.5 + m.t.x, y: (m.x.y + m.y.y) * 0.5 + m.t.y }
    }

    /// How a group's own pixels map onto the canvas (its sources are placed in them), as
    /// libobs draws it now.
    fn group_matrix(&self, group: *mut ffi::ObsSceneItem) -> ffi::Matrix4 {
        let a = self.api();
        let mut m = ffi::Matrix4::default();
        a.obs_sceneitem_force_update_transform(group);
        a.obs_sceneitem_get_draw_transform(group, &mut m);
        m
    }

    /// Runs `f` with the groups of a scene held still: libobs re-fits a group around its
    /// sources on its own thread, which shifts the group's pixels under a change made in
    /// several steps (a source landed off where it was put, 2.1.3 test). They re-fit after.
    fn holding_groups<T>(&self, scene: *mut ffi::ObsScene, f: impl FnOnce() -> T) -> T {
        let a = self.api();
        let groups: Vec<_> = self.raw_items(scene).into_iter().filter(|i| a.obs_sceneitem_is_group(*i)).collect();
        for g in &groups {
            a.obs_sceneitem_defer_group_resize_begin(*g);
        }
        let out = f();
        for g in &groups {
            a.obs_sceneitem_defer_group_resize_end(*g);
        }
        out
    }

    /// The group `item` is in, if it's in one.
    fn group_of(&self, scene: *mut ffi::ObsScene, item: *mut ffi::ObsSceneItem) -> Option<*mut ffi::ObsSceneItem> {
        let g = self.api().obs_sceneitem_get_group(scene, item);
        (!g.is_null()).then_some(g)
    }

    /// Makes a change (a flip or a turn) in place: the box keeps its middle where it was.
    /// An item without a bounding box is anchored at its top left first, as OBS's own flips
    /// leave it: libobs draws a flipped item anchored anywhere else off its box (by half its
    /// width when anchored in the middle, 2.1.1).
    fn in_place(&self, item: *mut ffi::ObsSceneItem, change: impl FnOnce()) {
        let a = self.api();
        let before = self.box_middle(item);
        if a.obs_sceneitem_get_bounds_type(item) == ffi::OBS_BOUNDS_NONE {
            let mut m = ffi::Matrix4::default();
            a.obs_sceneitem_get_box_transform(item, &mut m);
            // Anchored at its top left, the box's first corner is where the item sits.
            a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
            a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: m.t.x, y: m.t.y });
        }
        change();
        let after = self.box_middle(item);
        let mut pos = ffi::Vec2 { x: 0.0, y: 0.0 };
        a.obs_sceneitem_get_pos(item, &mut pos);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: pos.x + before.x - after.x, y: pos.y + before.y - after.y });
    }

    fn scale_by(&self, item: *mut ffi::ObsSceneItem, x: f32, y: f32) {
        let a = self.api();
        self.in_place(item, || {
            let mut s = ffi::Vec2 { x: 1.0, y: 1.0 };
            a.obs_sceneitem_get_scale(item, &mut s);
            a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: s.x * x, y: s.y * y });
        });
    }

    fn turn(&self, item: *mut ffi::ObsSceneItem, deg: f32) {
        let a = self.api();
        self.in_place(item, || {
            a.obs_sceneitem_set_rot(item, (a.obs_sceneitem_get_rot(item) + deg).rem_euclid(360.0));
        });
    }

    /// A new, empty group on top of a scene (OBS's Add Group).
    pub fn add_group(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.name_free(name)?;
        let n = cstr(name);
        let item = self.api().obs_scene_add_group2(s, n.as_ptr(), true);
        if item.is_null() {
            return Err(StudioError::Failed(format!("the group \"{name}\" couldn't be made")));
        }
        Ok(self.info(item))
    }

    /// Puts sources of a scene into a new group, each staying where it is on screen and the
    /// group where the topmost of them was (OBS's Group Selected Items). Groups and sources
    /// already in a group can't go in. → the group.
    pub fn group_together(&self, scene: &str, ids: &[i64], name: &str) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.name_free(name)?;
        let a = self.api();
        let top = self.raw_items(s);
        // libobs wants them bottom first, as the scene holds them.
        let mut items: Vec<*mut ffi::ObsSceneItem> = Vec::new();
        for i in &top {
            if ids.contains(&a.obs_sceneitem_get_id(*i)) {
                if a.obs_sceneitem_is_group(*i) {
                    return Err(StudioError::Failed("A group can't go inside another group.".into()));
                }
                items.push(*i);
            }
        }
        if items.len() != ids.len() {
            return Err(StudioError::Failed("Pick sources of this scene that aren't already in a group.".into()));
        }
        if items.is_empty() {
            return Err(StudioError::Failed("Pick the sources to group.".into()));
        }
        for i in &items {
            a.obs_sceneitem_force_update_transform(*i);
        }
        let n = cstr(name);
        let group = a.obs_scene_insert_group2(s, n.as_ptr(), items.as_mut_ptr(), items.len(), true);
        if group.is_null() {
            return Err(StudioError::Failed(format!("the group \"{name}\" couldn't be made")));
        }
        Ok(self.info(group))
    }

    /// The sources in group `id` of a scene (names, top first) and the group's own name.
    pub fn group_parts(&self, scene: &str, id: i64) -> Result<(String, Vec<String>), StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
        if !a.obs_sceneitem_is_group(item) {
            return Err(StudioError::Failed("That isn't a group.".into()));
        }
        let name = |i: *mut ffi::ObsSceneItem| text(a.obs_source_get_name(a.obs_sceneitem_get_source(i)));
        Ok((name(item), self.group_items(item).into_iter().rev().map(name).collect()))
    }

    /// Ids of a scene's own sources (not in a group) with these names.
    pub fn ids_named(&self, scene: &str, names: &[String]) -> Result<Vec<i64>, StudioError> {
        let s = self.scene_ptr(scene)?;
        let a = self.api();
        Ok(self
            .raw_items(s)
            .into_iter()
            .filter(|i| names.contains(&text(a.obs_source_get_name(a.obs_sceneitem_get_source(*i)))))
            .map(|i| a.obs_sceneitem_get_id(i))
            .collect())
    }

    /// An item's place among its neighbours, counted from the top as the list shows them, and
    /// its source's name (for undoing a reorder, in a group too).
    pub fn item_place(&self, scene: &str, id: i64) -> Result<(usize, String), StudioError> {
        let a = self.api();
        let s = self.scene_ptr(scene)?;
        let item = self.item(scene, id)?;
        let top = self.raw_items(s);
        let sib = if top.contains(&item) {
            top
        } else {
            let g = a.obs_sceneitem_get_group(s, item);
            if g.is_null() { top } else { self.group_items(g) }
        };
        let from_bottom = sib.iter().position(|i| *i == item).unwrap_or(0);
        let name = text(a.obs_source_get_name(a.obs_sceneitem_get_source(item)));
        Ok((sib.len().saturating_sub(1).saturating_sub(from_bottom), name))
    }

    /// Moves an item to place `to` among its neighbours (the scene's sources, or its group's),
    /// counted from the top as the list shows them (0 is on top of everything). OBS's drag in
    /// its Sources list.
    pub fn item_order(&self, scene: &str, id: i64, to: usize) -> Result<(), StudioError> {
        let a = self.api();
        let s = self.scene_ptr(scene)?;
        let item = self.item(scene, id)?;
        let top = self.raw_items(s);
        let n = if top.contains(&item) {
            top.len()
        } else {
            let g = a.obs_sceneitem_get_group(s, item);
            if g.is_null() { top.len() } else { self.group_items(g).len() }
        };
        // libobs counts from the bottom.
        let pos = n.saturating_sub(1).saturating_sub(to.min(n.saturating_sub(1)));
        a.obs_sceneitem_set_order_position(item, c_int::try_from(pos).unwrap_or(0));
        Ok(())
    }

    /// Takes a group apart: its sources stay where they are, in the scene (OBS's Ungroup).
    pub fn ungroup(&self, scene: &str, id: i64) -> Result<(), StudioError> {
        let item = self.item(scene, id)?;
        if !self.api().obs_sceneitem_is_group(item) {
            return Err(StudioError::Failed("That isn't a group.".into()));
        }
        self.api().obs_sceneitem_group_ungroup2(item, true);
        Ok(())
    }

    /// Moves an item into a group of the same scene (`group`), or out of its group (None),
    /// keeping where it is on screen.
    pub fn set_group(&self, scene: &str, id: i64, group: Option<i64>) -> Result<(), StudioError> {
        let s = self.scene_ptr(scene)?;
        self.holding_groups(s, || self.set_group_held(scene, id, group))
    }

    fn set_group_held(&self, scene: &str, id: i64, group: Option<i64>) -> Result<(), StudioError> {
        let s = self.scene_ptr(scene)?;
        let item = self.item(scene, id)?;
        let a = self.api();
        let now = a.obs_sceneitem_get_group(s, item);
        // libobs keeps an item where it is on screen using both transforms, which it otherwise
        // works out only on the next frame (a group made a moment ago has none yet): the item
        // jumped when it went in (2.1.3 test).
        a.obs_sceneitem_force_update_transform(item);
        if !now.is_null() {
            a.obs_sceneitem_force_update_transform(now);
        }
        match group {
            Some(g) => {
                let target = self.item(scene, g)?;
                if !a.obs_sceneitem_is_group(target) {
                    return Err(StudioError::Failed("group: pick a group of this scene.".into()));
                }
                if a.obs_sceneitem_is_group(item) {
                    return Err(StudioError::Failed("A group can't go inside another group.".into()));
                }
                if now != target {
                    // Where it is on the canvas, to put it back there inside the group: libobs's
                    // own sum is off for a group that's been resized (2.1.3 test).
                    let f = self.place(item).frame();
                    let f = if now.is_null() { f } else { on_canvas(f, &self.group_matrix(now)) };
                    a.obs_sceneitem_force_update_transform(target);
                    if !now.is_null() {
                        a.obs_sceneitem_group_remove_item(now, item);
                    }
                    // Its id may change on the way out: find it again by source.
                    let src = a.obs_sceneitem_get_source(item);
                    let moved = self.raw_items(s).into_iter().find(|i| a.obs_sceneitem_get_source(*i) == src);
                    let moved = moved.unwrap_or(item);
                    a.obs_sceneitem_group_add_item(target, moved);
                    let (w, h) = (f[0].hypot(f[1]), f[2].hypot(f[3]));
                    if w >= 1.0 && h >= 1.0 {
                        self.set_item_box(scene, a.obs_sceneitem_get_id(moved), f[4], f[5], w, h)?;
                    }
                }
            }
            None => {
                if !now.is_null() {
                    a.obs_sceneitem_group_remove_item(now, item);
                }
            }
        }
        Ok(())
    }

    /// Pastes a copy of a source (OBS's Paste (Duplicate)): a new, independent source made
    /// from it ("Camera 2"), on top of `scene`.
    pub fn paste_copy(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        let src = self.source(name)?;
        let a = self.api();
        let mut n = 2;
        let new_name = loop {
            let c = format!("{name} {n}");
            if self.name_free(&c).is_ok() {
                break c;
            }
            n += 1;
        };
        let cn = cstr(&new_name);
        let mut copy = a.obs_source_duplicate(src.ptr, cn.as_ptr(), false);
        if copy == src.ptr {
            // Kinds that can't be copied hand back the same source: one from its settings.
            a.obs_source_release(copy);
            let data = a.obs_source_get_settings(src.ptr);
            copy = a.obs_source_create(a.obs_source_get_id(src.ptr), cn.as_ptr(), data, ptr::null_mut());
            if !data.is_null() {
                a.obs_data_release(data);
            }
            if !copy.is_null() {
                a.obs_source_copy_filters(copy, src.ptr);
            }
        }
        if copy.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be copied")));
        }
        let item = a.obs_scene_add(s, copy);
        a.obs_source_release(copy);
        if item.is_null() {
            return Err(StudioError::Failed(format!("\"{new_name}\" couldn't be put in the scene")));
        }
        Ok(self.info(item))
    }

    /// A copy of the item's source, named like it ("Camera 2"), just above it in the scene
    /// with the same place, size, turn and blending.
    fn duplicate(&self, scene: &str, item: *mut ffi::ObsSceneItem) -> Result<(), StudioError> {
        let a = self.api();
        let s = self.scene_ptr(scene)?;
        let src = a.obs_sceneitem_get_source(item);
        let base = text(a.obs_source_get_name(src));
        let mut n = 2;
        let name = loop {
            let c = format!("{base} {n}");
            if self.name_free(&c).is_ok() {
                break c;
            }
            n += 1;
        };
        let cn = cstr(&name);
        let mut copy = a.obs_source_duplicate(src, cn.as_ptr(), false);
        // Kinds that can't be duplicated (web pages) hand back the same source: make a new
        // one of the kind from its settings instead, and give it the same filters.
        if copy == src {
            a.obs_source_release(copy);
            let data = a.obs_source_get_settings(src);
            copy = a.obs_source_create(a.obs_source_get_id(src), cn.as_ptr(), data, ptr::null_mut());
            if !data.is_null() {
                a.obs_data_release(data);
            }
            if !copy.is_null() {
                a.obs_source_copy_filters(copy, src);
            }
        }
        if copy.is_null() {
            return Err(StudioError::Failed(format!("\"{base}\" couldn't be copied")));
        }
        let new = a.obs_scene_add(s, copy);
        a.obs_source_release(copy);
        if new.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        let mut v = ffi::Vec2 { x: 0.0, y: 0.0 };
        a.obs_sceneitem_get_pos(item, &mut v);
        a.obs_sceneitem_set_pos(new, &v);
        a.obs_sceneitem_get_scale(item, &mut v);
        a.obs_sceneitem_set_scale(new, &v);
        a.obs_sceneitem_get_bounds(item, &mut v);
        a.obs_sceneitem_set_bounds(new, &v);
        a.obs_sceneitem_set_alignment(new, a.obs_sceneitem_get_alignment(item));
        a.obs_sceneitem_set_bounds_type(new, a.obs_sceneitem_get_bounds_type(item));
        a.obs_sceneitem_set_bounds_alignment(new, a.obs_sceneitem_get_bounds_alignment(item));
        a.obs_sceneitem_set_rot(new, a.obs_sceneitem_get_rot(item));
        a.obs_sceneitem_set_blending_mode(new, a.obs_sceneitem_get_blending_mode(item));
        a.obs_sceneitem_set_visible(new, a.obs_sceneitem_visible(item));
        // New items go on top: walk it down to just above the original.
        let ids: Vec<i64> = self.raw_items(s).into_iter().map(|i| a.obs_sceneitem_get_id(i)).collect();
        let (orig, made) = (a.obs_sceneitem_get_id(item), a.obs_sceneitem_get_id(new));
        if let (Some(o), Some(m)) = (ids.iter().position(|x| *x == orig), ids.iter().position(|x| *x == made)) {
            for _ in (o + 1)..m {
                a.obs_sceneitem_set_order(new, ffi::OBS_ORDER_MOVE_DOWN);
            }
        }
        Ok(())
    }

    pub fn edit_item(&self, scene: &str, id: i64, edit: ItemEdit) -> Result<(), StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
        let c = Size::of(self.size_of(self.scene_ptr(scene)?));
        let blend = |m: c_int| a.obs_sceneitem_set_blending_mode(item, m);
        match edit {
            ItemEdit::Show | ItemEdit::Hide => {
                a.obs_sceneitem_set_visible(item, edit == ItemEdit::Show);
            }
            ItemEdit::Lock | ItemEdit::Unlock => {
                a.obs_sceneitem_set_locked(item, edit == ItemEdit::Lock);
            }
            ItemEdit::Fit => self.fit(item),
            ItemEdit::Remove => a.obs_sceneitem_remove(item),
            ItemEdit::Up => a.obs_sceneitem_set_order(item, ffi::OBS_ORDER_MOVE_UP),
            ItemEdit::Down => a.obs_sceneitem_set_order(item, ffi::OBS_ORDER_MOVE_DOWN),
            ItemEdit::Top => a.obs_sceneitem_set_order(item, ffi::OBS_ORDER_MOVE_TOP),
            ItemEdit::Bottom => a.obs_sceneitem_set_order(item, ffi::OBS_ORDER_MOVE_BOTTOM),
            ItemEdit::Stretch => {
                a.obs_sceneitem_set_rot(item, 0.0);
                a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
                a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
                a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_STRETCH);
                a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: c.width as f32, y: c.height as f32 });
            }
            ItemEdit::Center => {
                // Moved, not re-anchored: libobs draws a flipped item anchored in its middle off
                // its box (see `in_place`). Inside a group, the canvas's middle in its pixels.
                let mid = self.box_middle(item);
                let target = (c.width as f32 / 2.0, c.height as f32 / 2.0);
                let target = match self.group_of(self.scene_ptr(scene)?, item) {
                    Some(g) => off_canvas(&self.group_matrix(g), target),
                    None => target,
                };
                let mut pos = ffi::Vec2 { x: 0.0, y: 0.0 };
                a.obs_sceneitem_get_pos(item, &mut pos);
                a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: pos.x + target.0 - mid.x, y: pos.y + target.1 - mid.y });
            }
            ItemEdit::Reset => {
                a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_NONE);
                a.obs_sceneitem_set_rot(item, 0.0);
                a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: 1.0, y: 1.0 });
                a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
                a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
            }
            ItemEdit::FlipH => self.scale_by(item, -1.0, 1.0),
            ItemEdit::FlipV => self.scale_by(item, 1.0, -1.0),
            ItemEdit::RotateCw => self.turn(item, 90.0),
            ItemEdit::RotateCcw => self.turn(item, -90.0),
            ItemEdit::Rotate180 => self.turn(item, 180.0),
            ItemEdit::Duplicate => self.duplicate(scene, item)?,
            ItemEdit::BlendNormal => blend(0),
            ItemEdit::BlendAdditive => blend(1),
            ItemEdit::BlendSubtract => blend(2),
            ItemEdit::BlendScreen => blend(3),
            ItemEdit::BlendMultiply => blend(4),
            ItemEdit::BlendLighten => blend(5),
            ItemEdit::BlendDarken => blend(6),
            ItemEdit::BlendMethodDefault => a.obs_sceneitem_set_blending_method(item, 0),
            ItemEdit::BlendMethodSrgbOff => a.obs_sceneitem_set_blending_method(item, 1),
            ItemEdit::ScaleOff => a.obs_sceneitem_set_scale_filter(item, 0),
            ItemEdit::ScalePoint => a.obs_sceneitem_set_scale_filter(item, 1),
            ItemEdit::ScaleBicubic => a.obs_sceneitem_set_scale_filter(item, 2),
            ItemEdit::ScaleBilinear => a.obs_sceneitem_set_scale_filter(item, 3),
            ItemEdit::ScaleLanczos => a.obs_sceneitem_set_scale_filter(item, 4),
            ItemEdit::ScaleArea => a.obs_sceneitem_set_scale_filter(item, 5),
        }
        Ok(())
    }

    /// What decides where an item is (libobs works its box out only on the next frame).
    fn place(&self, item: *mut ffi::ObsSceneItem) -> Place {
        let a = self.api();
        let src = a.obs_sceneitem_get_source(item);
        let (mut pos, mut scale, mut bounds) =
            (ffi::Vec2 { x: 0.0, y: 0.0 }, ffi::Vec2 { x: 1.0, y: 1.0 }, ffi::Vec2 { x: 0.0, y: 0.0 });
        let mut crop = ffi::Crop::default();
        a.obs_sceneitem_get_pos(item, &mut pos);
        a.obs_sceneitem_get_scale(item, &mut scale);
        a.obs_sceneitem_get_bounds(item, &mut bounds);
        a.obs_sceneitem_get_crop(item, &mut crop);
        let cut = |size: u32, x: c_int, y: c_int| {
            let c = u32::try_from(x.max(0)).unwrap_or(0).saturating_add(u32::try_from(y.max(0)).unwrap_or(0));
            if c > size { 2 } else { size - c }
        };
        Place {
            pos: (pos.x, pos.y),
            scale: (scale.x, scale.y),
            rot: a.obs_sceneitem_get_rot(item),
            align: a.obs_sceneitem_get_alignment(item),
            bounds: (a.obs_sceneitem_get_bounds_type(item) != ffi::OBS_BOUNDS_NONE).then_some((bounds.x, bounds.y)),
            size: (
                cut(a.obs_source_get_width(src), crop.left, crop.right),
                cut(a.obs_source_get_height(src), crop.top, crop.bottom),
            ),
        }
    }

    /// An item's place, size, turn, bounding box and crop, as Edit Transform shows them.
    pub fn transform(&self, scene: &str, id: i64) -> Result<Transform, StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
        let (mut pos, mut scale, mut bounds) =
            (ffi::Vec2 { x: 0.0, y: 0.0 }, ffi::Vec2 { x: 1.0, y: 1.0 }, ffi::Vec2 { x: 0.0, y: 0.0 });
        let mut crop = ffi::Crop::default();
        a.obs_sceneitem_get_pos(item, &mut pos);
        a.obs_sceneitem_get_scale(item, &mut scale);
        a.obs_sceneitem_get_bounds(item, &mut bounds);
        a.obs_sceneitem_get_crop(item, &mut crop);
        let src = a.obs_sceneitem_get_source(item);
        Ok(Transform {
            x: pos.x,
            y: pos.y,
            rotation: a.obs_sceneitem_get_rot(item),
            scale_x: scale.x,
            scale_y: scale.y,
            alignment: a.obs_sceneitem_get_alignment(item),
            bounds: BOUNDS
                .get(usize::try_from(a.obs_sceneitem_get_bounds_type(item)).unwrap_or(0))
                .unwrap_or(&BOUNDS[0])
                .to_string(),
            bounds_alignment: a.obs_sceneitem_get_bounds_alignment(item),
            bounds_width: bounds.x,
            bounds_height: bounds.y,
            crop_left: crop.left,
            crop_top: crop.top,
            crop_right: crop.right,
            crop_bottom: crop.bottom,
            width: a.obs_source_get_width(src),
            height: a.obs_source_get_height(src),
        })
    }

    /// Sets all of an item's transform at once (its width and height are ignored) → the item.
    pub fn set_transform(&self, scene: &str, id: i64, t: &Transform) -> Result<ItemInfo, StudioError> {
        let bounds = BOUNDS
            .iter()
            .position(|b| *b == t.bounds)
            .ok_or_else(|| StudioError::Failed(format!("\"{}\" isn't a kind of bounding box", t.bounds)))?;
        let item = self.item(scene, id)?;
        let a = self.api();
        a.obs_sceneitem_set_bounds_type(item, c_int::try_from(bounds).unwrap_or(0));
        a.obs_sceneitem_set_bounds_alignment(item, t.bounds_alignment);
        a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: t.bounds_width, y: t.bounds_height });
        a.obs_sceneitem_set_crop(
            item,
            &ffi::Crop { left: t.crop_left, top: t.crop_top, right: t.crop_right, bottom: t.crop_bottom },
        );
        a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: t.scale_x, y: t.scale_y });
        a.obs_sceneitem_set_rot(item, t.rotation.rem_euclid(360.0));
        a.obs_sceneitem_set_alignment(item, t.alignment);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: t.x, y: t.y });
        Ok(self.info(item))
    }

    /// Remembers an item as it is now (a top-level item of `scene`), to put back later.
    pub fn snap(&self, scene: &str, id: i64) -> Result<ItemSnap, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.holding_groups(s, || self.snap_held(scene, id))
    }

    fn snap_held(&self, scene: &str, id: i64) -> Result<ItemSnap, StudioError> {
        let s = self.scene_ptr(scene)?;
        let a = self.api();
        let is = |i: &*mut ffi::ObsSceneItem| a.obs_sceneitem_get_id(*i) == id;
        let top = self.raw_items(s);
        // In the scene itself, or inside one of its groups.
        let (item, order, group) = match top.iter().position(is) {
            Some(o) => (top[o], o, None),
            None => top
                .iter()
                .filter(|g| a.obs_sceneitem_is_group(**g))
                .find_map(|g| {
                    let kids = self.group_items(*g);
                    kids.iter().position(is).map(|o| (kids[o], o, Some(a.obs_sceneitem_get_id(*g))))
                })
                .ok_or_else(|| StudioError::Failed(format!("there's no source with id {id} in \"{scene}\"")))?,
        };
        let src = a.obs_source_get_ref(a.obs_sceneitem_get_source(item));
        if src.is_null() {
            return Err(StudioError::Failed("that source is being removed".into()));
        }
        Ok(ItemSnap {
            studio: self.studio.clone(),
            src,
            scene: scene.to_string(),
            id,
            name: text(a.obs_source_get_name(src)),
            order,
            group,
            canvas: group
                .and_then(|g| self.item(scene, g).ok())
                .map(|g| on_canvas(self.place(item).frame(), &self.group_matrix(g))),
            transform: self.transform(scene, id)?,
            visible: a.obs_sceneitem_visible(item),
            locked: a.obs_sceneitem_locked(item),
            blend: a.obs_sceneitem_get_blending_mode(item),
            blend_method: a.obs_sceneitem_get_blending_method(item),
            scale_filter: a.obs_sceneitem_get_scale_filter(item),
            show: self.item_transition(item, true),
            hide: self.item_transition(item, false),
            deinterlace: (a.obs_source_get_deinterlace_mode(src), a.obs_source_get_deinterlace_field_order(src)),
        })
    }

    /// Puts an item back as `snap` remembers it, adding its source to the scene again if the
    /// item was removed. Returns its id now (new if it was added again).
    pub fn restore(&self, snap: &ItemSnap) -> Result<i64, StudioError> {
        let s = self.scene_ptr(&snap.scene)?;
        self.holding_groups(s, || self.restore_held(snap))
    }

    fn restore_held(&self, snap: &ItemSnap) -> Result<i64, StudioError> {
        let s = self.scene_ptr(&snap.scene)?;
        let a = self.api();
        let item = match self.item(&snap.scene, snap.id) {
            Ok(i) => i,
            Err(_) => {
                self.reclaim_name(snap.src, &snap.name)?;
                let i = a.obs_scene_add(s, snap.src);
                if i.is_null() {
                    return Err(StudioError::Failed(format!("\"{}\" couldn't be put back", snap.name)));
                }
                // Back into its group, if that's still there (its place inside it is set below).
                if let Some(g) = snap.group.and_then(|g| self.item(&snap.scene, g).ok()) {
                    a.obs_sceneitem_group_add_item(g, i);
                }
                i
            }
        };
        let id = a.obs_sceneitem_get_id(item);
        self.set_transform(&snap.scene, id, &snap.transform)?;
        a.obs_sceneitem_set_visible(item, snap.visible);
        a.obs_sceneitem_set_locked(item, snap.locked);
        a.obs_sceneitem_set_blending_mode(item, snap.blend);
        a.obs_sceneitem_set_blending_method(item, snap.blend_method);
        a.obs_sceneitem_set_scale_filter(item, snap.scale_filter);
        a.obs_sceneitem_set_order_position(item, c_int::try_from(snap.order).unwrap_or(0));
        // Inside a group: back where it was on the canvas.
        if let Some(f) = snap.canvas.filter(|_| self.group_of(s, item).is_some()) {
            let (w, h) = (f[0].hypot(f[1]), f[2].hypot(f[3]));
            if w >= 1.0 && h >= 1.0 {
                self.set_item_box(&snap.scene, id, f[4], f[5], w, h)?;
            }
        }
        for (show, t) in [(true, &snap.show), (false, &snap.hide)] {
            if self.item_transition(item, show) != *t {
                self.set_item_transition(&snap.scene, id, show, t.as_ref())?;
            }
        }
        if a.obs_source_get_output_flags(snap.src) & ffi::OBS_SOURCE_ASYNC != 0 {
            a.obs_source_set_deinterlace_mode(snap.src, snap.deinterlace.0);
            a.obs_source_set_deinterlace_field_order(snap.src, snap.deinterlace.1);
        }
        Ok(id)
    }

    /// Remembers a scene as it is now, to bring it back after it's removed.
    pub fn snap_scene(&self, name: &str) -> Result<SceneSnap, StudioError> {
        let src = self.any_scene(name)?;
        let a = self.api();
        let d = a.obs_save_source(src);
        let mut data = self.data_json(d);
        if !d.is_null() {
            a.obs_data_release(d);
        }
        // It comes back as a new scene, with its own id.
        if let Some(o) = data.as_object_mut() {
            o.remove("uuid");
        }
        let mut held = Vec::new();
        for i in self.raw_items(a.obs_scene_from_source(src)) {
            let mut all = vec![i];
            if a.obs_sceneitem_is_group(i) {
                all.extend(self.group_items(i));
            }
            for i in all {
                let r = a.obs_source_get_ref(a.obs_sceneitem_get_source(i));
                if !r.is_null() {
                    held.push((r, text(a.obs_source_get_name(r))));
                }
            }
        }
        Ok(SceneSnap { studio: self.studio.clone(), data, name: name.to_string(), index: self.index_of(name)?, held })
    }

    /// Brings back a removed scene at its old place in the list.
    pub fn restore_scene(&self, snap: &SceneSnap) -> Result<(), StudioError> {
        self.name_free(&snap.name)?;
        // Its sources first get their names back (items find them by id either way).
        for (src, name) in &snap.held {
            self.reclaim_name(*src, name)?;
        }
        let a = self.api();
        let s = cstr(&json!({ "sources": [snap.data] }).to_string());
        let holder = a.obs_data_create_from_json(s.as_ptr());
        if holder.is_null() {
            return Err(StudioError::Failed(format!("\"{}\" couldn't be read back", snap.name)));
        }
        let arr = a.obs_data_get_array(holder, c"sources".as_ptr());
        let mut ctx = ffi::LoadCtx { api: a as *const Api, scenes: Vec::new() };
        if !arr.is_null() {
            a.obs_load_sources(arr, Some(ffi::keep_scenes), &mut ctx as *mut _ as *mut c_void);
            a.obs_data_array_release(arr);
        }
        a.obs_data_release(holder);
        let mut loaded = ctx.scenes.into_iter();
        let Some(src) = loaded.next() else {
            return Err(StudioError::Failed(format!("\"{}\" couldn't be brought back", snap.name)));
        };
        for extra in loaded {
            a.obs_source_release(extra);
        }
        // Back on the picture it was on (it keeps its canvas when saved).
        if self.is_vertical(a.obs_scene_from_source(src))
            && let Some(v) = self.vertical.borrow_mut().as_mut()
        {
            let at = snap.index.min(v.scenes.len());
            v.scenes.insert(at, src);
            return Ok(());
        }
        // The vertical picture is off: back among its kept scenes, not the main picture's
        // (CodeRabbit).
        if let Some(v) = self.parked.borrow_mut().as_mut()
            && a.canvas_of(src) == v.canvas
        {
            let at = snap.index.min(v.scenes.len());
            v.scenes.insert(at, src);
            return Ok(());
        }
        let mut v = self.scenes.borrow_mut();
        let at = snap.index.min(v.len());
        v.insert(at, src);
        Ok(())
    }

    /// Remembers a source's filters as they are now.
    pub fn snap_filters(&self, name: &str) -> Result<FilterSnap, StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        let filters = self
            .raw_filters(src.ptr)
            .into_iter()
            .map(|f| {
                (
                    text(a.obs_source_get_id(f)),
                    text(a.obs_source_get_name(f)),
                    self.own_settings(f),
                    a.obs_source_enabled(f),
                )
            })
            .collect();
        Ok(FilterSnap { source: name.to_string(), filters })
    }

    /// Puts a source's filters back as `snap` remembers them. Filters still there keep running
    /// (their settings are put back); missing ones are made again, extra ones go.
    pub fn restore_filters(&self, snap: &FilterSnap) -> Result<(), StudioError> {
        let src = self.source(&snap.source)?;
        let a = self.api();
        for f in self.raw_filters(src.ptr) {
            let n = text(a.obs_source_get_name(f));
            let k = text(a.obs_source_get_id(f));
            if !snap.filters.iter().any(|(kind, name, ..)| *name == n && *kind == k) {
                a.obs_source_filter_remove(src.ptr, f);
            }
        }
        // In order, each to the end: libobs keeps its list the other way round.
        for (kind, name, settings, on) in &snap.filters {
            let have = self.filter(&src, name).ok().filter(|f| text(a.obs_source_get_id(f.ptr)) == *kind);
            let f = match have {
                Some(f) => {
                    self.reset_of(f.ptr, settings)?;
                    f
                }
                None => {
                    if let Ok(other) = self.filter(&src, name) {
                        a.obs_source_filter_remove(src.ptr, other.ptr);
                    }
                    self.add_filter(&snap.source, kind, name, settings)?;
                    self.filter(&src, name)?
                }
            };
            a.obs_source_set_enabled(f.ptr, *on);
            a.obs_source_filter_set_order(src.ptr, f.ptr, ffi::OBS_ORDER_MOVE_BOTTOM);
        }
        Ok(())
    }

    /// A source's own settings (what was set, not OBS's defaults), to put back later.
    pub fn snap_settings(&self, name: &str) -> Result<Value, StudioError> {
        let src = self.source(name)?;
        Ok(self.own_settings(src.ptr))
    }

    /// Puts a source's settings back exactly (keys not in `settings` return to OBS's defaults).
    pub fn restore_settings(&self, name: &str, settings: &Value) -> Result<(), StudioError> {
        let src = self.source(name)?;
        self.reset_of(src.ptr, settings)
    }

    fn own_settings(&self, src: *mut ffi::ObsSource) -> Value {
        let a = self.api();
        let d = a.obs_source_get_settings(src);
        let v = self.data_json(d);
        if !d.is_null() {
            a.obs_data_release(d);
        }
        if v.is_object() { v } else { json!({}) }
    }

    fn reset_of(&self, src: *mut ffi::ObsSource, settings: &Value) -> Result<(), StudioError> {
        let a = self.api();
        let data = a.obs_data_create_from_json(cstr(&settings.to_string()).as_ptr());
        if data.is_null() {
            return Err(StudioError::Failed("settings: OBS couldn't read them.".into()));
        }
        a.obs_source_reset_settings(src, data);
        a.obs_data_release(data);
        Ok(())
    }

    /// Remembers a sound's mixer state as it is now.
    pub fn snap_audio(&self, name: &str) -> Result<AudioSnap, StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        Ok(AudioSnap {
            source: name.to_string(),
            volume: a.obs_source_get_volume(src.ptr),
            muted: a.obs_source_muted(src.ptr),
            monitor: a.obs_source_get_monitoring_type(src.ptr),
            balance: a.obs_source_get_balance_value(src.ptr),
            sync: a.obs_source_get_sync_offset(src.ptr),
            flags: a.obs_source_get_flags(src.ptr),
            mixers: a.obs_source_get_audio_mixers(src.ptr),
        })
    }

    pub fn restore_audio(&self, snap: &AudioSnap) -> Result<(), StudioError> {
        let src = self.source(&snap.source)?;
        let a = self.api();
        a.obs_source_set_volume(src.ptr, snap.volume);
        a.obs_source_set_muted(src.ptr, snap.muted);
        a.obs_source_set_monitoring_type(src.ptr, snap.monitor);
        a.obs_source_set_balance_value(src.ptr, snap.balance);
        a.obs_source_set_sync_offset(src.ptr, snap.sync);
        a.obs_source_set_flags(src.ptr, snap.flags);
        a.obs_source_set_audio_mixers(src.ptr, snap.mixers);
        Ok(())
    }

    /// A video, playlist or slideshow's play controls (OBS's media controls).
    pub fn media(&self, name: &str, what: MediaAct) -> Result<MediaInfo, StudioError> {
        let src = self.media_source(name)?;
        let a = self.api();
        let pause = |p: bool| {
            a.obs_source_media_play_pause(src.ptr, p);
            self.media_intent.borrow_mut().insert(name.to_string(), (p, std::time::Instant::now()));
        };
        match what {
            MediaAct::Play => pause(false),
            MediaAct::Pause => pause(true),
            MediaAct::PlayPause => {
                // A press within a moment of the last goes the other way from it, not from
                // the state libobs hasn't updated yet.
                let recent = self
                    .media_intent
                    .borrow()
                    .get(name)
                    .filter(|(_, at)| at.elapsed() < std::time::Duration::from_millis(500))
                    .map(|(p, _)| *p);
                let paused_now = recent.unwrap_or_else(|| a.obs_source_media_get_state(src.ptr) != 1);
                pause(!paused_now);
            }
            MediaAct::Restart => a.obs_source_media_restart(src.ptr),
            MediaAct::Stop => a.obs_source_media_stop(src.ptr),
            MediaAct::Next => a.obs_source_media_next(src.ptr),
            MediaAct::Previous => a.obs_source_media_previous(src.ptr),
        }
        Ok(self.media_info(src.ptr))
    }

    /// Where a video, playlist or slideshow is.
    pub fn media_state(&self, name: &str) -> Result<MediaInfo, StudioError> {
        let src = self.media_source(name)?;
        Ok(self.media_info(src.ptr))
    }

    /// Jumps a video to `ms` in.
    pub fn media_seek(&self, name: &str, ms: i64) -> Result<MediaInfo, StudioError> {
        let src = self.media_source(name)?;
        self.api().obs_source_media_set_time(src.ptr, ms.max(0));
        Ok(self.media_info(src.ptr))
    }

    fn media_source(&self, name: &str) -> Result<Ref<'_>, StudioError> {
        let src = self.source(name)?;
        if self.api().obs_source_get_output_flags(src.ptr) & ffi::OBS_SOURCE_CONTROLLABLE_MEDIA == 0 {
            return Err(StudioError::Failed(format!("\"{name}\" isn't a video, playlist or slideshow.")));
        }
        Ok(src)
    }

    fn media_info(&self, src: *mut ffi::ObsSource) -> MediaInfo {
        let a = self.api();
        let state = usize::try_from(a.obs_source_media_get_state(src)).unwrap_or(0);
        MediaInfo {
            state: MEDIA_STATES.get(state).copied().unwrap_or("none"),
            time_ms: a.obs_source_media_get_time(src).max(0),
            duration_ms: a.obs_source_media_get_duration(src).max(0),
        }
    }

    /// Every file a source, filter or transition kind uses that isn't on this computer (OBS's
    /// Missing Files): file and folder options, and each file of a list (slideshows).
    pub fn missing_files(&self) -> Vec<MissingFile> {
        let a = self.api();
        let mut out = Vec::new();
        let mut seen: Vec<*mut ffi::ObsSource> = Vec::new();
        for src in self.in_use() {
            if seen.contains(&src) || a.obs_source_is_scene(src) {
                continue;
            }
            seen.push(src);
            let name = text(a.obs_source_get_name(src));
            self.missing_in(src, &name, None, &mut out);
            for f in self.raw_filters(src) {
                self.missing_in(f, &text(a.obs_source_get_name(f)), Some(&name), &mut out);
            }
        }
        // Scenes have filters too.
        for scene in self.every_scene().iter() {
            let name = text(a.obs_source_get_name(*scene));
            for f in self.raw_filters(*scene) {
                self.missing_in(f, &text(a.obs_source_get_name(f)), Some(&name), &mut out);
            }
        }
        for (kind, settings) in self.transition_settings.borrow().iter() {
            for key in ["path", "luma_image", "track_matte_path"] {
                // Only whole paths: a Luma Wipe names one of OBS's own images instead.
                if let Some(p) = settings.get(key).and_then(Value::as_str)
                    && std::path::Path::new(p).is_absolute()
                    && !std::path::Path::new(p).exists()
                {
                    out.push(MissingFile {
                        source: String::new(),
                        parent: None,
                        kind: kind.clone(),
                        key: key.into(),
                        label: key.into(),
                        path: p.into(),
                    });
                }
            }
        }
        out
    }

    fn missing_in(&self, src: *mut ffi::ObsSource, name: &str, parent: Option<&str>, out: &mut Vec<MissingFile>) {
        let a = self.api();
        let settings = self.settings_of(src);
        let kind = text(a.obs_source_get_id(src));
        let mut opts = self.studio.source_options(src, true);
        while let Some(o) = opts.pop() {
            opts.extend(o.group.iter().cloned());
            if !o.visible {
                continue;
            }
            // Whole paths only (not addresses, not names a plugin resolves itself).
            let missing = |p: &str| std::path::Path::new(p).is_absolute() && !std::path::Path::new(p).exists();
            let mut push = |p: &str| {
                out.push(MissingFile {
                    source: name.into(),
                    parent: parent.map(String::from),
                    kind: kind.clone(),
                    key: o.key.clone(),
                    label: o.label.clone(),
                    path: p.into(),
                })
            };
            match (o.kind, o.path) {
                ("path", "file" | "folder") => {
                    if let Some(p) = settings.get(&o.key).and_then(Value::as_str).filter(|p| missing(p)) {
                        push(p);
                    }
                }
                ("editable_list", "files") => {
                    for item in settings.get(&o.key).and_then(Value::as_array).into_iter().flatten() {
                        if let Some(p) = item.get("value").and_then(Value::as_str).filter(|p| missing(p)) {
                            push(p);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Points a missing file's setting at `to` (or, `to` empty, clears it; a list loses the
    /// item): OBS's Missing Files "Select file" and "Clear".
    pub fn replace_file(&self, m: &MissingFile, to: &str) -> Result<(), StudioError> {
        if m.source.is_empty() {
            let mut patch = serde_json::Map::new();
            patch.insert(m.key.clone(), json!(to));
            return self.set_transition_settings(&m.kind, &Value::Object(patch));
        }
        let owner = match &m.parent {
            Some(p) => {
                let parent = self.source(p)?;
                self.filter(&parent, &m.source)?.ptr
            }
            None => self.source(&m.source)?.ptr,
        };
        let settings = self.settings_of(owner);
        let next = match settings.get(&m.key) {
            Some(Value::Array(list)) => {
                let mut list = list.clone();
                if to.is_empty() {
                    list.retain(|i| i.get("value").and_then(Value::as_str) != Some(m.path.as_str()));
                } else {
                    for i in &mut list {
                        if i.get("value").and_then(Value::as_str) == Some(m.path.as_str())
                            && let Some(o) = i.as_object_mut()
                        {
                            o.insert("value".into(), json!(to));
                        }
                    }
                }
                Value::Array(list)
            }
            Some(Value::String(p)) if *p == m.path => json!(to),
            _ => return Err(StudioError::Failed(format!("\"{}\" doesn't use {} any more", m.source, m.path))),
        };
        let mut patch = serde_json::Map::new();
        patch.insert(m.key.clone(), next);
        self.update_of(owner, &Value::Object(patch))
    }

    /// A scene's own transition now (to put back).
    pub fn scene_transition_of(&self, scene: &str) -> Result<Option<TransitionInfo>, StudioError> {
        Ok(self.scene_transition(self.scene_src(scene)?))
    }

    /// Where a scene is in the list (0 is the top).
    pub fn scene_index(&self, name: &str) -> Result<usize, StudioError> {
        self.index_of(name)
    }

    /// The ids of a scene's top-level items (to find what an act added).
    pub fn item_ids(&self, scene: &str) -> Result<Vec<i64>, StudioError> {
        let s = self.scene_ptr(scene)?;
        Ok(self.raw_items(s).into_iter().map(|i| self.api().obs_sceneitem_get_id(i)).collect())
    }

    /// What's on air is a private copy of the scene (Studio Mode's "Duplicate scene"), to check.
    #[doc(hidden)]
    pub fn on_air_is_copy(&self) -> bool {
        let a = self.api();
        let on = a.obs_transition_get_active_source(*self.transition.borrow());
        if on.is_null() {
            return false;
        }
        let copy = !self.scenes.borrow().contains(&on);
        a.obs_source_release(on);
        copy
    }

    /// The item's box as libobs last worked it out (on a frame), to check [`ItemInfo::frame`].
    #[doc(hidden)]
    pub fn drawn_frame(&self, scene: &str, id: i64) -> Result<[f32; 6], StudioError> {
        let item = self.item(scene, id)?;
        let mut m = ffi::Matrix4::default();
        self.api().obs_sceneitem_get_box_transform(item, &mut m);
        Ok([m.x.x, m.x.y, m.y.x, m.y.y, m.t.x, m.t.y])
    }

    /// Moves and sizes an item from the preview: its box's first corner goes to (`x`, `y`)
    /// and its sides become `w` and `h` long (canvas pixels), keeping its turn and flips. A
    /// box set by OBS's "bounding box" changes that box, anything else its scale (as OBS's
    /// own preview does). Returns it as it is now.
    pub fn set_item_box(&self, scene: &str, id: i64, x: f32, y: f32, w: f32, h: f32) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.holding_groups(s, || self.set_item_box_held(scene, id, x, y, w, h))
    }

    fn set_item_box_held(&self, scene: &str, id: i64, x: f32, y: f32, w: f32, h: f32) -> Result<ItemInfo, StudioError> {
        const MAX: f32 = 100_000.0;
        if ![x, y, w, h].iter().all(|v| v.is_finite() && v.abs() <= MAX) || w < 1.0 || h < 1.0 {
            return Err(StudioError::Failed("That size or place is off the canvas.".into()));
        }
        let item = self.item(scene, id)?;
        let a = self.api();
        // Inside a group the box is in the group's own pixels: bring the canvas box there.
        let group = self.group_of(self.scene_ptr(scene)?, item).map(|g| self.group_matrix(g));
        let (x, y, w, h) = match &group {
            Some(m) => {
                let (lx, ly) = off_canvas(m, (x, y));
                let (sx, sy) = (m.x.x.hypot(m.x.y), m.y.x.hypot(m.y.y));
                if !(sx > 0.0 && sy > 0.0) {
                    return Err(StudioError::Failed("That group has no size to place a source in.".into()));
                }
                (lx, ly, w / sx, h / sy)
            }
            None => (x, y, w, h),
        };
        let next = self.place(item).with_box(x, y, w, h);
        if next.bounds.is_some() {
            a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: next.box_size().0, y: next.box_size().1 });
        } else {
            a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: next.scale.0, y: next.scale.1 });
        }
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: next.pos.0, y: next.pos.1 });
        let mut info = self.info(item);
        if let Some(m) = &group {
            info.frame = on_canvas(info.frame, m);
        }
        Ok(info)
    }

    /// Crops an item from the preview (OBS's Alt-drag on a handle): `crop` is left, top, right,
    /// bottom in the source's own pixels. What's left of the picture stays where it was on
    /// screen. Returns it as it is now (its box on the canvas).
    pub fn set_item_crop(&self, scene: &str, id: i64, crop: [i32; 4]) -> Result<ItemInfo, StudioError> {
        let s = self.scene_ptr(scene)?;
        self.holding_groups(s, || self.set_item_crop_held(scene, id, crop))
    }

    fn set_item_crop_held(&self, scene: &str, id: i64, crop: [i32; 4]) -> Result<ItemInfo, StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
        let src = a.obs_sceneitem_get_source(item);
        let (w, h) = (a.obs_source_get_width(src) as i64, a.obs_source_get_height(src) as i64);
        let [l, t, r, b] = crop.map(i64::from);
        if [l, t, r, b].iter().any(|v| *v < 0) || l + r > w - 2 || t + b > h - 2 {
            return Err(StudioError::Failed("That crops away the whole picture.".into()));
        }
        let mut was = ffi::Crop::default();
        a.obs_sceneitem_get_crop(item, &mut was);
        // The new top-left source pixel, where it is on screen now (in the item's own space:
        // the canvas, or its group's pixels).
        let mut before = ffi::Matrix4::default();
        a.obs_sceneitem_force_update_transform(item);
        a.obs_sceneitem_get_draw_transform(item, &mut before);
        let (dx, dy) = ((crop[0] - was.left) as f32, (crop[1] - was.top) as f32);
        let keep = (dx * before.x.x + dy * before.y.x + before.t.x, dx * before.x.y + dy * before.y.y + before.t.y);
        a.obs_sceneitem_set_crop(item, &ffi::Crop { left: crop[0], top: crop[1], right: crop[2], bottom: crop[3] });
        let mut after = ffi::Matrix4::default();
        a.obs_sceneitem_force_update_transform(item);
        a.obs_sceneitem_get_draw_transform(item, &mut after);
        let mut pos = ffi::Vec2 { x: 0.0, y: 0.0 };
        a.obs_sceneitem_get_pos(item, &mut pos);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: pos.x + keep.0 - after.t.x, y: pos.y + keep.1 - after.t.y });
        let mut info = self.info(item);
        if let Some(g) = self.group_of(self.scene_ptr(scene)?, item) {
            info.frame = on_canvas(info.frame, &self.group_matrix(g));
        }
        Ok(info)
    }

    /// Every source in a scene (groups included), with the scenes it's in, in scene order.
    pub fn sources(&self) -> Vec<SourceUse> {
        let a = self.api();
        let mut out: Vec<SourceUse> = Vec::new();
        for s in self.every_scene().iter() {
            let scene = text(a.obs_source_get_name(*s));
            let mut items = Vec::new();
            for i in self.raw_items(a.obs_scene_from_source(*s)) {
                items.push(i);
                if a.obs_sceneitem_is_group(i) {
                    items.extend(self.group_items(i));
                }
            }
            for i in items.into_iter().rev() {
                let src = a.obs_sceneitem_get_source(i);
                if a.obs_source_is_scene(src) {
                    continue;
                }
                let name = text(a.obs_source_get_name(src));
                match out.iter_mut().find(|u| u.name == name) {
                    Some(u) if !u.scenes.contains(&scene) => u.scenes.push(scene.clone()),
                    Some(_) => {}
                    None => {
                        out.push(SourceUse { name, kind: text(a.obs_source_get_id(src)), scenes: vec![scene.clone()] })
                    }
                }
            }
        }
        out
    }

    // ---- Source settings ----

    pub fn settings(&self, name: &str) -> Result<Value, StudioError> {
        let src = self.source(name)?;
        Ok(self.settings_of(src.ptr))
    }

    fn settings_of(&self, src: *mut ffi::ObsSource) -> Value {
        let a = self.api();
        let data = a.obs_source_get_settings(src);
        if data.is_null() {
            return json!({});
        }
        let s = text(a.obs_data_get_json_with_defaults(data));
        a.obs_data_release(data);
        serde_json::from_str(&s).unwrap_or_else(|_| json!({}))
    }

    pub fn options(&self, name: &str) -> Result<Vec<OptionInfo>, StudioError> {
        let src = self.source(name)?;
        Ok(self.studio.source_options(src.ptr, true))
    }

    /// Changes only the keys given; the rest stay.
    pub fn update(&self, name: &str, settings: &Value) -> Result<(), StudioError> {
        let src = self.source(name)?;
        self.update_of(src.ptr, settings)
    }

    fn update_of(&self, src: *mut ffi::ObsSource, settings: &Value) -> Result<(), StudioError> {
        if !settings.is_object() {
            return Err(StudioError::Failed("settings: an object of OBS setting keys and values.".into()));
        }
        let a = self.api();
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        if data.is_null() {
            return Err(StudioError::Failed("settings: OBS couldn't read them.".into()));
        }
        a.obs_source_update(src, data);
        a.obs_data_release(data);
        Ok(())
    }

    // ---- Sound ----

    fn audio_info(&self, src: *mut ffi::ObsSource, channel: Option<u32>) -> AudioInfo {
        let a = self.api();
        AudioInfo {
            name: text(a.obs_source_get_name(src)),
            kind: text(a.obs_source_get_id(src)),
            channel,
            volume_db: db(a.obs_mul_to_db(a.obs_source_get_volume(src))),
            muted: a.obs_source_muted(src),
            monitor: monitor_of(a.obs_source_get_monitoring_type(src)),
            balance: a.obs_source_get_balance_value(src),
            sync_ms: a.obs_source_get_sync_offset(src) / 1_000_000,
            mono: a.obs_source_get_flags(src) & ffi::OBS_SOURCE_FLAG_FORCE_MONO != 0,
            tracks: tracks_of(a.obs_source_get_audio_mixers(src)),
            active: a.obs_source_audio_active(src),
        }
    }

    /// One sound's mixer state.
    pub fn audio_of(&self, name: &str) -> Result<AudioInfo, StudioError> {
        let src = self.source(name)?;
        let ch = CHANNEL_KEYS.iter().map(|(c, _)| *c).find(|c| {
            let s = self.api().obs_get_output_source(*c);
            let same = s == src.ptr;
            if !s.is_null() {
                self.api().obs_source_release(s);
            }
            same
        });
        Ok(self.audio_info(src.ptr, ch))
    }

    /// Left to right, 0 to 1.
    pub fn set_balance(&self, name: &str, balance: f32) -> Result<(), StudioError> {
        if !balance.is_finite() || !(0.0..=1.0).contains(&balance) {
            return Err(StudioError::Failed("balance: 0 (left) to 1 (right).".into()));
        }
        let src = self.source(name)?;
        self.api().obs_source_set_balance_value(src.ptr, balance);
        Ok(())
    }

    /// Delays (or, negative, brings forward) a sound to line it up with the picture.
    pub fn set_sync(&self, name: &str, ms: i64) -> Result<(), StudioError> {
        if !(-950..=20_000).contains(&ms) {
            return Err(StudioError::Failed("sync_ms: -950 to 20000 ms.".into()));
        }
        let src = self.source(name)?;
        self.api().obs_source_set_sync_offset(src.ptr, ms * 1_000_000);
        Ok(())
    }

    pub fn set_mono(&self, name: &str, mono: bool) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        let f = a.obs_source_get_flags(src.ptr);
        let f = if mono { f | ffi::OBS_SOURCE_FLAG_FORCE_MONO } else { f & !ffi::OBS_SOURCE_FLAG_FORCE_MONO };
        a.obs_source_set_flags(src.ptr, f);
        Ok(())
    }

    /// The audio tracks (1 to 6) a sound goes into.
    pub fn set_tracks(&self, name: &str, tracks: &[u8]) -> Result<(), StudioError> {
        if tracks.iter().any(|t| !(1..=6).contains(t)) {
            return Err(StudioError::Failed("tracks: numbers 1 to 6.".into()));
        }
        let src = self.source(name)?;
        let mask = tracks.iter().fold(0u32, |m, t| m | 1 << (t - 1));
        self.api().obs_source_set_audio_mixers(src.ptr, mask);
        Ok(())
    }

    /// The global devices (every scene).
    pub fn channels(&self) -> Vec<AudioInfo> {
        let a = self.api();
        CHANNEL_KEYS
            .iter()
            .filter_map(|(ch, _)| {
                let src = a.obs_get_output_source(*ch);
                if src.is_null() {
                    return None;
                }
                let info = self.audio_info(src, Some(*ch));
                a.obs_source_release(src);
                Some(info)
            })
            .collect()
    }

    /// What a scene's mixer shows: the global devices, then the scene's own sounds (groups
    /// included), each once.
    pub fn audio(&self, scene: &str) -> Result<Vec<AudioInfo>, StudioError> {
        let s = self.scene_ptr(scene)?;
        let a = self.api();
        let mut out = self.channels();
        let mut items = Vec::new();
        for i in self.raw_items(s) {
            items.push(i);
            if a.obs_sceneitem_is_group(i) {
                items.extend(self.group_items(i));
            }
        }
        for i in items.into_iter().rev() {
            let src = a.obs_sceneitem_get_source(i);
            let name = text(a.obs_source_get_name(src));
            if a.obs_source_get_output_flags(src) & ffi::OBS_SOURCE_AUDIO != 0 && !out.iter().any(|x| x.name == name) {
                out.push(self.audio_info(src, None));
            }
        }
        Ok(out)
    }

    /// Puts a device on a global channel (1 to 6) as a new source, or takes it off (`None`).
    pub fn set_channel(&self, ch: u32, device: Option<(&str, &str, &Value)>) -> Result<(), StudioError> {
        if !(1..=6).contains(&ch) {
            return Err(StudioError::Failed("channel: 1 to 6.".into()));
        }
        let a = self.api();
        let Some((kind, name, settings)) = device else {
            let old = a.obs_get_output_source(ch);
            if !old.is_null() {
                self.meters.borrow_mut().remove(&text(a.obs_source_get_name(old)));
                a.obs_source_remove(old);
                a.obs_source_release(old);
            }
            a.obs_set_output_source(ch, ptr::null_mut());
            return Ok(());
        };
        // The device on this channel may keep its name: it goes before the new one is made.
        let old = a.obs_get_output_source(ch);
        let same = !old.is_null() && text(a.obs_source_get_name(old)) == name;
        if !old.is_null() {
            a.obs_source_release(old);
        }
        if same {
            self.set_channel(ch, None)?;
        } else {
            self.name_free(name)?;
        }
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        let (id, n) = (cstr(kind), cstr(name));
        let src = a.obs_source_create(id.as_ptr(), n.as_ptr(), data, ptr::null_mut());
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if src.is_null() {
            return Err(StudioError::Failed(format!("the device \"{name}\" ({kind}) couldn't be made")));
        }
        self.set_channel(ch, None)?;
        a.obs_set_output_source(ch, src);
        a.obs_source_release(src);
        Ok(())
    }

    pub fn set_volume(&self, name: &str, volume_db: f32) -> Result<(), StudioError> {
        if !volume_db.is_finite() || !(SILENT..=26.0).contains(&volume_db) {
            return Err(StudioError::Failed("volume_db: -100 to 26.".into()));
        }
        let src = self.source(name)?;
        let a = self.api();
        let mul = if volume_db <= SILENT { 0.0 } else { a.obs_db_to_mul(volume_db) };
        a.obs_source_set_volume(src.ptr, mul);
        Ok(())
    }

    pub fn set_muted(&self, name: &str, muted: bool) -> Result<(), StudioError> {
        let src = self.source(name)?;
        self.api().obs_source_set_muted(src.ptr, muted);
        Ok(())
    }

    pub fn set_monitor(&self, name: &str, m: Monitor) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let t = match m {
            Monitor::Off => 0,
            Monitor::OnlyMe => 1,
            Monitor::MeAndStream => 2,
        };
        self.api().obs_source_set_monitoring_type(src.ptr, t);
        Ok(())
    }

    /// Meters read true peak (OBS's "True Peak", catching peaks between samples) or sample
    /// peak (lighter).
    pub fn set_true_peak(&self, on: bool) {
        self.true_peak.replace(on);
        let a = self.api();
        for m in self.meters.borrow().values() {
            a.obs_volmeter_set_peak_meter_type(m.vm, c_int::from(on));
        }
    }

    /// The levels of these sounds now. Meters are made for new names and dropped for names
    /// no longer asked about.
    pub fn levels(&self, names: &[String]) -> HashMap<String, LevelInfo> {
        let a = self.api();
        let mut meters = self.meters.borrow_mut();
        let now = std::time::Instant::now();
        meters.retain(|k, m| names.contains(k) || now.duration_since(m.read) < METER_KEPT);
        let mut out = HashMap::new();
        for name in names {
            if !meters.contains_key(name) {
                let Some(src) = self.find(name) else { continue };
                let vm = a.obs_volmeter_create(ffi::OBS_FADER_LOG);
                if vm.is_null() {
                    continue;
                }
                // enum obs_peak_meter_type: 0 sample peak, 1 true peak.
                a.obs_volmeter_set_peak_meter_type(vm, c_int::from(*self.true_peak.borrow()));
                let m = Meter {
                    studio: self.studio.clone(),
                    vm,
                    cell: Box::new(Mutex::new(Level { peak: [SILENT; 2], magnitude: [SILENT; 2] })),
                    read: now,
                };
                a.obs_volmeter_add_callback(vm, ffi::store_level, m.param());
                a.obs_volmeter_attach_source(vm, src.ptr);
                meters.insert(name.clone(), m);
            }
            if let Some(m) = meters.get_mut(name) {
                m.read = now;
            }
            if let Some(l) = meters.get(name).and_then(|m| m.cell.lock().ok().map(|l| *l)) {
                out.insert(name.clone(), LevelInfo { peak: l.peak.map(db), magnitude: l.magnitude.map(db) });
            }
        }
        out
    }

    // ---- Filters ----

    fn raw_filters(&self, src: *mut ffi::ObsSource) -> Vec<*mut ffi::ObsSource> {
        let mut out: Vec<*mut ffi::ObsSource> = Vec::new();
        self.api().obs_source_enum_filters(src, ffi::collect_filters, &mut out as *mut _ as *mut c_void);
        out
    }

    pub fn filters(&self, name: &str) -> Result<Vec<FilterInfo>, StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        Ok(self
            .raw_filters(src.ptr)
            .into_iter()
            .map(|f| FilterInfo {
                name: text(a.obs_source_get_name(f)),
                kind: text(a.obs_source_get_id(f)),
                enabled: a.obs_source_enabled(f),
            })
            .collect())
    }

    fn filter(&self, src: &Ref<'_>, filter: &str) -> Result<Ref<'_>, StudioError> {
        let n = cstr(filter);
        let f = self.api().obs_source_get_filter_by_name(src.ptr, n.as_ptr());
        if f.is_null() {
            return Err(StudioError::Failed(format!("there's no filter called \"{filter}\" on this source")));
        }
        Ok(Ref { api: self.api(), ptr: f })
    }

    /// Adds a filter at the end (it runs last).
    pub fn add_filter(&self, name: &str, kind: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        let src = self.source(name)?;
        check_name(filter)?;
        if self.filter(&src, filter).is_ok() {
            return Err(StudioError::Failed(format!("name: there's already a filter called \"{filter}\" here.")));
        }
        let a = self.api();
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        let (id, n) = (cstr(kind), cstr(filter));
        let f = a.obs_source_create_private(id.as_ptr(), n.as_ptr(), data);
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if f.is_null() {
            return Err(StudioError::Failed(format!("the filter \"{filter}\" ({kind}) couldn't be made")));
        }
        a.obs_source_filter_add(src.ptr, f);
        a.obs_source_release(f);
        Ok(())
    }

    pub fn remove_filter(&self, name: &str, filter: &str) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        self.api().obs_source_filter_remove(src.ptr, f.ptr);
        Ok(())
    }

    /// A picture of a scene (at the canvas size) or a source (at its own size), saved as a
    /// PNG at `path`.
    pub fn screenshot(&self, name: &str, path: &std::path::Path) -> Result<(), StudioError> {
        // A vertical scene isn't among libobs's named sources.
        if let Some(v) = self.vertical_scene(name) {
            return self.screenshot_of(v, name, path);
        }
        let src = self.source(name)?;
        self.screenshot_of(src.ptr, name, path)
    }

    /// A picture of what's on air exactly (black when faded, both scenes mid-transition), at
    /// the canvas size, saved as a PNG at `path`.
    pub fn screenshot_program(&self, path: &std::path::Path) -> Result<(), StudioError> {
        let t = *self.transition.borrow();
        self.screenshot_of(t, "what's on air", path)
    }

    fn screenshot_of(&self, src: *mut ffi::ObsSource, name: &str, path: &std::path::Path) -> Result<(), StudioError> {
        let src = Ref { api: self.api(), ptr: self.api().obs_source_get_ref(src) };
        if src.ptr.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" has nothing to show yet.")));
        }
        let a = self.api();
        let (w, h) = if a.obs_source_is_scene(src.ptr) {
            self.size_of(a.obs_scene_from_source(src.ptr))
        } else if src.ptr == *self.transition.borrow() {
            let c = self.studio.canvas();
            (c.width, c.height)
        } else {
            (a.obs_source_get_width(src.ptr), a.obs_source_get_height(src.ptr))
        };
        if w == 0 || h == 0 {
            return Err(StudioError::Failed(format!("\"{name}\" has nothing to show yet.")));
        }
        let px = ffi::snapshot(a, src.ptr, w, h)
            .ok_or_else(|| StudioError::Failed(format!("\"{name}\" couldn't be drawn for a screenshot")))?;
        let file = std::fs::File::create(path)
            .map_err(|e| StudioError::Failed(format!("the screenshot couldn't be saved ({e})")))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(&px))
            .map_err(|e| StudioError::Failed(format!("the screenshot couldn't be saved ({e})")))?;
        Ok(())
    }

    /// A small picture of a scene as it looks now (`width` pixels wide, the canvas's shape), as a
    /// PNG data URL: the Scenes grid's tiles, as OBS's multiview.
    pub fn thumbnail(&self, scene: &str, width: u32) -> Result<String, StudioError> {
        use base64::Engine as _;
        let s = self.any_scene(scene)?;
        let a = self.api();
        let c = Size::of(self.size_of(a.obs_scene_from_source(s)));
        if c.width == 0 || c.height == 0 {
            return Err(StudioError::Failed("The canvas has no size yet.".into()));
        }
        let w = width.clamp(64, 640);
        let h = ((w as u64 * c.height as u64) / c.width as u64).max(1) as u32;
        {
            let mut held = self.thumb_showing.borrow_mut();
            match held.get_mut(scene) {
                Some((src, at)) if *src == s => *at = std::time::Instant::now(),
                _ => {
                    // New, or the name is another scene now (undone removal, renames).
                    if let Some((old, _)) = held.remove(scene) {
                        a.obs_source_dec_showing(old);
                        a.obs_source_release(old);
                    }
                    let r = a.obs_source_get_ref(s);
                    if !r.is_null() {
                        a.obs_source_inc_showing(r);
                        held.insert(scene.to_string(), (r, std::time::Instant::now()));
                    }
                }
            }
        }
        let src = Ref { api: a, ptr: a.obs_source_get_ref(s) };
        let px = ffi::snapshot_scaled(a, src.ptr, (c.width, c.height), w, h)
            .ok_or_else(|| StudioError::Failed(format!("\"{scene}\" couldn't be drawn")))?;
        let mut out = Vec::with_capacity(px.len() / 4);
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(&px))
            .map_err(|e| StudioError::Failed(format!("\"{scene}\"'s picture couldn't be made ({e})")))?;
        Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&out)))
    }

    /// Lets go of scenes kept showing for pictures that haven't been asked for in a while (the
    /// grid closed, or the scene went), or of all of them with `all`.
    /// Fits again the items fitted before their source had a size, once it has one. One still
    /// with the bounding box `fit` gave it is fitted afresh; one moved or sized meanwhile keeps
    /// where its picture shows, without the box: a window capture whose window was picked later
    /// otherwise kept a canvas-sized frame round a smaller picture, and lining the frame up
    /// left black edges (owner, 29 Oct). Called often (the stats read); waits up to 30 min.
    pub fn settle_fits(&self) {
        if self.waiting_fit.borrow().is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut *self.waiting_fit.borrow_mut());
        let mut keep = Vec::new();
        for (scene, id, since) in waiting {
            let Ok(item) = self.item(&scene, id) else { continue };
            let a = self.api();
            // Given another kind of box (Stretch, Edit Transform) or none: it's the user's now.
            if a.obs_sceneitem_get_bounds_type(item) != ffi::OBS_BOUNDS_SCALE_INNER {
                continue;
            }
            let c = self.size_of_item(item);
            let p = self.place(item);
            let untouched = p.pos == (0.0, 0.0)
                && p.bounds.is_some_and(|(w, h)| (w - c.0 as f32).abs() < 0.5 && (h - c.1 as f32).abs() < 0.5);
            if p.size.0 > 0 && p.size.1 > 0 {
                if untouched {
                    self.fit(item);
                } else {
                    self.unbox(item);
                }
            } else if since.elapsed() < std::time::Duration::from_secs(30 * 60) {
                keep.push((scene, id, since));
            }
        }
        self.waiting_fit.borrow_mut().extend(keep);
    }

    /// A captured window or display that changes size is shown at its new shape, never
    /// stretched: its picture is fitted in the box it was given (one scale for both sides, the
    /// same top left). OBS keeps each side's scale, so a window made narrower or taller was
    /// squashed (owner, 29 Oct). The box is the one last set by hand, so a window that goes
    /// square and back comes back to its size; a crop isn't taken for a resize. Upright items
    /// without a bounding box, in groups too; called often (the stats read). True when one was
    /// fitted again (the scenes then want saving).
    pub fn follow_sizes(&self) -> bool {
        const CAPTURES: [&str; 5] =
            ["screen_capture", "window_capture", "display_capture", "game_capture", "xcomposite_input"];
        let a = self.api();
        let mut seen = self.sizes_seen.borrow_mut();
        let mut now = std::collections::HashMap::new();
        let mut moved = false;
        for scene in self.every_scene() {
            let top = self.raw_items(a.obs_scene_from_source(scene));
            // A group's sources count their ids in the group: keyed by the group's id too.
            let mut all = Vec::new();
            for i in top {
                if a.obs_sceneitem_is_group(i) {
                    let g = a.obs_sceneitem_get_id(i);
                    all.extend(self.group_items(i).into_iter().map(|c| (g, c)));
                }
                all.push((0, i));
            }
            for (group, item) in all {
                let src = a.obs_sceneitem_get_source(item);
                if !CAPTURES.contains(&text(a.obs_source_get_id(src)).as_str()) {
                    continue;
                }
                let raw = (a.obs_source_get_width(src), a.obs_source_get_height(src));
                let p = self.place(item);
                if raw.0 == 0 || raw.1 == 0 || p.size.0 == 0 || p.size.1 == 0 {
                    continue;
                }
                let key = (scene as usize, group, a.obs_sceneitem_get_id(item));
                let shown = (p.size.0 as f32 * p.scale.0.abs(), p.size.1 as f32 * p.scale.1.abs());
                let fresh = Seen { raw, target: shown, scale: p.scale };
                let Some(was) = seen.get(&key).copied() else {
                    now.insert(key, fresh);
                    continue;
                };
                let same_scale = (was.scale.0 - p.scale.0).abs() < 1e-4 && (was.scale.1 - p.scale.1).abs() < 1e-4;
                if was.raw == raw {
                    // Resized by hand (or not at all): that's the box from now on.
                    now.insert(key, if same_scale { was } else { fresh });
                    continue;
                }
                if p.bounds.is_some() || p.rot.rem_euclid(360.0) > 0.01 {
                    now.insert(key, fresh);
                    continue;
                }
                // The crop is the same: the picture before was the old size less it.
                let cut = (raw.0.saturating_sub(p.size.0), raw.1.saturating_sub(p.size.1));
                let before = Place {
                    size: (was.raw.0.saturating_sub(cut.0).max(1), was.raw.1.saturating_sub(cut.1).max(1)),
                    ..p
                };
                let k = (was.target.0 / p.size.0 as f32).min(was.target.1 / p.size.1 as f32);
                if !k.is_finite() || k <= 0.0 {
                    now.insert(key, fresh);
                    continue;
                }
                let f = before.frame();
                let fitted = Place { scale: (k * p.scale.0.signum(), k * p.scale.1.signum()), ..p }.with_box(
                    f[4],
                    f[5],
                    p.size.0 as f32 * k,
                    p.size.1 as f32 * k,
                );
                a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: fitted.scale.0, y: fitted.scale.1 });
                a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: fitted.pos.0, y: fitted.pos.1 });
                now.insert(key, Seen { raw, target: was.target, scale: fitted.scale });
                moved = true;
            }
        }
        *seen = now;
        moved
    }

    /// Captures on air that show nothing, for the black screen fixer (29 Oct): a screen
    /// capture without macOS's Screen Recording permission, a capture with no picture at all
    /// (a closed window, a camera that didn't start), or one whose picture is all black. Looked
    /// at every 3 s at most (a 32×18 still of each); the page says so after it lasts.
    pub fn capture_problems(&self) -> Vec<CaptureProblem> {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(3);
        if let (Some(at), last) = &*self.captures_seen.borrow()
            && at.elapsed() < EVERY
        {
            return last.clone();
        }
        let a = self.api();
        // What the main picture really shows: its transition's scene, which in Studio Mode can
        // be a private copy of the scene (CodeRabbit). The vertical picture shows its own.
        let shown = a.obs_transition_get_active_source(*self.transition.borrow());
        let mut on_air = vec![if shown.is_null() { *self.current.borrow() } else { shown }];
        if let Some(v) = self.vertical.borrow().as_ref() {
            on_air.push(v.current);
        }
        let mut out: Vec<CaptureProblem> = Vec::new();
        // Visible sources, through groups and scenes shown inside scenes (each scene once).
        let mut items = Vec::new();
        let mut visited: Vec<*mut ffi::ObsSource> = Vec::new();
        let mut todo: Vec<*mut ffi::ObsSource> = on_air.iter().copied().filter(|s| !s.is_null()).collect();
        while let Some(scene) = todo.pop() {
            if visited.contains(&scene) {
                continue;
            }
            visited.push(scene);
            let mut here = Vec::new();
            for i in self.raw_items(a.obs_scene_from_source(scene)) {
                if !a.obs_sceneitem_visible(i) {
                    continue;
                }
                if a.obs_sceneitem_is_group(i) {
                    here.extend(self.group_items(i).into_iter().filter(|c| a.obs_sceneitem_visible(*c)));
                } else {
                    here.push(i);
                }
            }
            for i in here {
                let src = a.obs_sceneitem_get_source(i);
                if !a.obs_scene_from_source(src).is_null() {
                    todo.push(src);
                } else {
                    items.push(i);
                }
            }
        }
        if !shown.is_null() {
            a.obs_source_release(shown);
        }
        {
            for item in items {
                let src = a.obs_sceneitem_get_source(item);
                let kind = text(a.obs_source_get_id(src));
                let screen = SCREEN_KINDS.contains(&kind.as_str());
                if !screen && !CAMERA_KINDS.contains(&kind.as_str()) {
                    continue;
                }
                let name = text(a.obs_source_get_name(src));
                if out.iter().any(|p| p.source == name) {
                    continue;
                }
                let problem = if screen && !crate::screen_access() {
                    "permission"
                } else {
                    let (w, h) = (a.obs_source_get_width(src), a.obs_source_get_height(src));
                    if w == 0 || h == 0 {
                        "no_picture"
                    } else if ffi::snapshot_scaled(a, src, (w, h), 32, 18).is_some_and(|px| all_black(&px)) {
                        "black"
                    } else {
                        continue;
                    }
                };
                out.push(CaptureProblem { source: name, kind, problem });
            }
        }
        *self.captures_seen.borrow_mut() = (Some(std::time::Instant::now()), out.clone());
        out
    }

    /// Live captions from a sound (plan item 7): its audio, after its filters, to the Mac's
    /// own speech recognizer; `on_text` gets the sentence heard so far, and true once it's
    /// finished, on the main thread. macOS only; elsewhere it says so.
    pub fn start_captions(
        &self,
        source: &str,
        on_text: impl Fn(String, bool) + 'static,
    ) -> Result<Captions, StudioError> {
        #[cfg(target_os = "macos")]
        {
            use ffi::speech::{Access, Listener, Unavailable};
            let src = self.find(source).ok_or_else(|| no_source(source))?.ptr;
            let mut info =
                ffi::AudioInfo2 { samples_per_sec: 0, speakers: 0, max_buffering_ms: 0, fixed_buffering: false };
            let rate = if self.api().obs_get_audio_info2(&mut info) && info.samples_per_sec > 0 {
                info.samples_per_sec
            } else {
                48_000
            };
            match Listener::start(self.api(), src, rate, on_text) {
                Ok(listener) => Ok(Captions { listener, _studio: self.studio.clone() }),
                Err(Unavailable::NoAccess(Access::NotAsked)) => {
                    Err(StudioError::Failed("Allow speech recognition first.".into()))
                }
                Err(Unavailable::NoAccess(_)) => Err(StudioError::Failed(
                    "Ferrocast isn't allowed to recognise speech: turn it on in System Settings, Privacy & Security, Speech Recognition."
                        .into(),
                )),
                Err(Unavailable::NotOnDevice) => Err(StudioError::Failed(
                    "This Mac can't recognise your language on the device yet: in System Settings, Keyboard, turn on Dictation to download it."
                        .into(),
                )),
                Err(Unavailable::NoRecognizer) => {
                    Err(StudioError::Failed("Speech recognition isn't available on this Mac.".into()))
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (source, on_text);
            Err(StudioError::Failed("Live captions work on a Mac for now.".into()))
        }
    }

    pub fn let_thumbs_go(&self, all: bool) {
        const KEEP: std::time::Duration = std::time::Duration::from_secs(10);
        let a = self.api();
        self.thumb_showing.borrow_mut().retain(|_, (src, at)| {
            if !all && at.elapsed() < KEEP {
                return true;
            }
            a.obs_source_dec_showing(*src);
            a.obs_source_release(*src);
            false
        });
    }

    /// Clicks, stops clicking or reloads a Web page source.
    pub fn web(&self, name: &str, what: WebAct) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let a = self.api();
        #[cfg(target_os = "macos")]
        if text(a.obs_source_get_id(src.ptr)) == crate::WEB_PAGE_KIND {
            let data = a.obs_obj_get_data(src.ptr);
            ffi::web::act(
                data,
                match what {
                    WebAct::Interact => ffi::web::Do::Interact(name.to_string()),
                    WebAct::Stop => ffi::web::Do::Stop,
                    WebAct::Reload => ffi::web::Do::Reload,
                },
            );
            return Ok(());
        }
        // OBS's own Browser source (Windows): reload through its "Refresh cache" button.
        if text(a.obs_source_get_id(src.ptr)) == "browser_source" && what == WebAct::Reload {
            let props = a.obs_source_properties(src.ptr);
            if !props.is_null() {
                let key = cstr("refreshnocache");
                let p = a.obs_properties_get(props, key.as_ptr());
                if !p.is_null() {
                    a.obs_property_button_clicked(p, src.ptr.cast());
                }
                a.obs_properties_destroy(props);
            }
            return Ok(());
        }
        if text(a.obs_source_get_id(src.ptr)) == "browser_source" {
            return Err(StudioError::Failed(
                "Clicking into OBS's own Browser source isn't here yet. It can be reloaded.".into(),
            ));
        }
        let _ = what;
        Err(StudioError::Failed(format!("\"{name}\" isn't a web page.")))
    }

    /// Copies every filter of `from` onto `name`, after its own.
    pub fn paste_filters(&self, name: &str, from: &str) -> Result<(), StudioError> {
        let dst = self.source(name)?;
        let src = self.source(from)?;
        if dst.ptr == src.ptr {
            return Err(StudioError::Failed("Pick another source to paste its filters onto.".into()));
        }
        self.api().obs_source_copy_filters(dst.ptr, src.ptr);
        Ok(())
    }

    pub fn set_filter_enabled(&self, name: &str, filter: &str, on: bool) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        self.api().obs_source_set_enabled(f.ptr, on);
        Ok(())
    }

    /// Moves a filter up (earlier) or down (later) in the order.
    pub fn move_filter(&self, name: &str, filter: &str, edit: ItemEdit) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        let m = match edit {
            ItemEdit::Up => ffi::OBS_ORDER_MOVE_UP,
            ItemEdit::Down => ffi::OBS_ORDER_MOVE_DOWN,
            ItemEdit::Top => ffi::OBS_ORDER_MOVE_TOP,
            ItemEdit::Bottom => ffi::OBS_ORDER_MOVE_BOTTOM,
            _ => return Err(StudioError::Failed("move: up, down, top or bottom.".into())),
        };
        self.api().obs_source_filter_set_order(src.ptr, f.ptr, m);
        Ok(())
    }

    pub fn filter_settings(&self, name: &str, filter: &str) -> Result<Value, StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        Ok(self.settings_of(f.ptr))
    }

    pub fn filter_options(&self, name: &str, filter: &str) -> Result<Vec<OptionInfo>, StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        Ok(self.studio.source_options(f.ptr, true))
    }

    pub fn update_filter(&self, name: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let f = self.filter(&src, filter)?;
        self.update_of(f.ptr, settings)
    }

    // ---- Saving and loading (OBS's format) ----

    fn data_json(&self, data: *mut ffi::ObsData) -> Value {
        if data.is_null() {
            return Value::Null;
        }
        serde_json::from_str(&text(self.api().obs_data_get_json(data))).unwrap_or(Value::Null)
    }

    /// The collection as OBS writes a scene collection.
    pub fn save(&self, name: &str) -> Value {
        let a = self.api();
        let mut doc = serde_json::Map::new();
        let mut skip: Vec<*mut ffi::ObsSource> = Vec::new();
        for (ch, key) in CHANNEL_KEYS {
            let src = a.obs_get_output_source(ch);
            if src.is_null() {
                continue;
            }
            let data = a.obs_save_source(src);
            doc.insert(key.into(), self.data_json(data));
            if !data.is_null() {
                a.obs_data_release(data);
            }
            skip.push(src);
            a.obs_source_release(src);
        }
        let keep: Vec<_> = self.in_use().into_iter().filter(|s| !skip.contains(s)).collect();
        let arr = a.obs_save_sources_filtered(ffi::save_only_listed, &keep as *const _ as *mut c_void);
        let holder = a.obs_data_create();
        let key = cstr("sources");
        a.obs_data_set_array(holder, key.as_ptr(), arr);
        let sources = self.data_json(holder).get("sources").cloned().unwrap_or_else(|| json!([]));
        a.obs_data_release(holder);
        a.obs_data_array_release(arr);
        let cur = self.current();
        let t = self.transition();
        doc.insert("name".into(), json!(name));
        doc.insert("current_scene".into(), json!(cur));
        doc.insert("current_program_scene".into(), json!(cur));
        doc.insert(
            "scene_order".into(),
            Value::Array(self.scenes().into_iter().map(|s| json!({ "name": s.name })).collect()),
        );
        doc.insert("sources".into(), sources);
        let obs_name = |k: &str| TRANSITION_NAMES.iter().find(|(id, _)| *id == k).map_or("Fade", |(_, n)| *n);
        doc.insert("current_transition".into(), json!(obs_name(&t.kind)));
        doc.insert("transition_duration".into(), json!(t.ms));
        // As OBS keeps them, so OBS opens these with their settings too.
        let settings = self.transition_settings.borrow().clone();
        let mut kinds: Vec<&String> = settings.keys().collect();
        kinds.sort();
        doc.insert(
            "transitions".into(),
            Value::Array(
                kinds.iter().map(|k| json!({ "name": obs_name(k), "id": k, "settings": settings[*k] })).collect(),
            ),
        );
        doc.insert(
            "ferrocast".into(),
            json!({
                "transition": t,
                "transition_settings": settings,
                "quick_transitions": self.quick_transitions(),
                "studio_mode": self.preview_scene(),
                "studio_options": self.studio_options(),
                "vertical": self.save_vertical(),
            }),
        );
        // OBS keeps Studio Mode's preview as the "current scene".
        if let Some(p) = self.preview_scene() {
            doc.insert("current_scene".into(), json!(p));
        }
        Value::Object(doc)
    }

    /// Loads a saved collection (Ferrocast's or OBS's). With no scenes in it, one empty scene
    /// called `first_scene` is made.
    pub fn load(studio: &Studio, doc: &Value, first_scene: &str) -> Result<Collection, StudioError> {
        let c = Collection::empty(studio)?;
        let a = c.api();
        for (ch, key) in CHANNEL_KEYS {
            let Some(v) = doc.get(key).filter(|v| v.is_object()) else { continue };
            let s = cstr(&v.to_string());
            let data = a.obs_data_create_from_json(s.as_ptr());
            if data.is_null() {
                continue;
            }
            let src = a.obs_load_source(data);
            a.obs_data_release(data);
            if !src.is_null() {
                a.obs_set_output_source(ch, src);
                a.obs_source_release(src);
            }
        }
        // The vertical canvas first, so its scenes find it as they load.
        let saved_vertical = doc.pointer("/ferrocast/vertical").filter(|v| v.is_object());
        let mut vertical_canvas = saved_vertical.and_then(|v| c.load_vertical_canvas(v));
        let sources = doc.get("sources").filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([]));
        let s = cstr(&json!({ "sources": sources }).to_string());
        let holder = a.obs_data_create_from_json(s.as_ptr());
        if !holder.is_null() {
            let key = cstr("sources");
            let arr = a.obs_data_get_array(holder, key.as_ptr());
            let mut ctx = ffi::LoadCtx { api: a as *const Api, scenes: Vec::new() };
            if !arr.is_null() {
                a.obs_load_sources(arr, Some(ffi::keep_scenes), &mut ctx as *mut _ as *mut c_void);
                a.obs_data_array_release(arr);
            }
            a.obs_data_release(holder);
            // In the saved order; any scene the order doesn't name goes last.
            let order: Vec<String> = doc
                .get("scene_order")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(|s| s.get("name")?.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut scenes = ctx.scenes;
            scenes.sort_by_key(|s| {
                let n = text(a.obs_source_get_name(*s));
                order.iter().position(|o| *o == n).unwrap_or(usize::MAX)
            });
            if let (Some(canvas), Some(saved)) = (vertical_canvas.take(), saved_vertical) {
                c.adopt_vertical(canvas, saved, &mut scenes);
            }
            c.scenes.replace(scenes);
        }
        if let Some(canvas) = vertical_canvas {
            // Loaded, but there were no sources to put on it.
            a.canvas_remove_release(canvas);
        }
        if c.scenes.borrow().is_empty() {
            c.add_scene(first_scene)?;
        }
        // Each kind's settings: Ferrocast's own, or OBS's list of transitions (first of a kind).
        let mut kept: HashMap<String, Value> = doc
            .pointer("/ferrocast/transition_settings")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter(|(_, v)| v.is_object()).map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let obs_list = doc.get("transitions").and_then(Value::as_array).cloned().unwrap_or_default();
        for t in &obs_list {
            if let (Some(id), Some(set)) = (t.get("id").and_then(Value::as_str), t.get("settings"))
                && set.is_object()
                && TRANSITION_NAMES.iter().any(|(k, _)| *k == id)
            {
                kept.entry(id.to_string()).or_insert_with(|| set.clone());
            }
        }
        c.transition_settings.replace(kept);
        // OBS names the one in use; its list (or OBS's own names) gives the kind.
        let obs_current = doc.get("current_transition").and_then(Value::as_str).and_then(|name| {
            obs_list
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|t| t.get("id").and_then(Value::as_str))
                .or_else(|| TRANSITION_NAMES.iter().find(|(_, n)| *n == name).map(|(k, _)| *k))
                .filter(|k| TRANSITION_NAMES.iter().any(|(id, _)| id == k))
                .map(str::to_string)
        });
        if let Some(t) =
            doc.pointer("/ferrocast/transition").and_then(|t| serde_json::from_value::<TransitionInfo>(t.clone()).ok())
        {
            // An unknown kind keeps the fade.
            let _ = c.set_transition(&t);
        } else {
            let ms = doc.get("transition_duration").and_then(Value::as_u64).unwrap_or(300).min(20_000) as u32;
            let kind = obs_current.unwrap_or_else(|| "fade_transition".into());
            if c.set_transition(&TransitionInfo { kind, ms }).is_err() {
                let _ = c.set_transition(&TransitionInfo { kind: "fade_transition".into(), ms });
            }
        }
        let want = doc
            .get("current_program_scene")
            .or_else(|| doc.get("current_scene"))
            .and_then(Value::as_str)
            .and_then(|n| c.scene_src(n).ok());
        let first = c.scenes.borrow()[0];
        c.go(want.unwrap_or(first), 0);
        if let Some(q) = doc
            .pointer("/ferrocast/quick_transitions")
            .and_then(|q| serde_json::from_value::<Vec<TransitionInfo>>(q.clone()).ok())
        {
            let _ = c.set_quick_transitions(&q);
        }
        // Studio Mode comes back with its preview.
        if let Some(o) = doc
            .pointer("/ferrocast/studio_options")
            .and_then(|o| serde_json::from_value::<StudioOptions>(o.clone()).ok())
        {
            c.studio_options.replace(o);
        }
        if let Some(p) = doc.pointer("/ferrocast/studio_mode").and_then(Value::as_str) {
            c.set_studio_mode(true);
            let _ = c.set_preview(p);
        }
        Ok(c)
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        self.meters.borrow_mut().clear();
        self.let_thumbs_go(true);
        let a = &self.studio.inner.api;
        a.obs_set_output_source(0, ptr::null_mut());
        // As OBS clears a collection: every source in a scene (the vertical picture's while
        // it's off too), each scene, the devices.
        let mut sources: Vec<*mut ffi::ObsSource> = Vec::new();
        for s in self.held_scenes().iter() {
            for i in self.raw_items(a.obs_scene_from_source(*s)) {
                let mut all = vec![i];
                if a.obs_sceneitem_is_group(i) {
                    all.extend(self.group_items(i));
                }
                for it in all {
                    let src = a.obs_sceneitem_get_source(it);
                    if !sources.contains(&src) && !a.obs_source_is_scene(src) {
                        sources.push(src);
                    }
                }
            }
        }
        for src in sources {
            a.obs_source_remove(src);
        }
        for s in self.scenes.borrow_mut().drain(..) {
            a.obs_source_remove(s);
            a.obs_source_release(s);
        }
        for (ch, _) in CHANNEL_KEYS {
            let src = a.obs_get_output_source(ch);
            if !src.is_null() {
                a.obs_source_remove(src);
                a.obs_source_release(src);
            }
            a.obs_set_output_source(ch, ptr::null_mut());
        }
        let t = self.transition.replace(ptr::null_mut());
        if !t.is_null() {
            a.obs_source_release(t);
        }
        self.drop_vertical();
    }
}

/// A picture's width and height, read as the canvas's are.
#[derive(Clone, Copy)]
struct Size {
    width: u32,
    height: u32,
}

impl Size {
    fn of((width, height): (u32, u32)) -> Size {
        Size { width, height }
    }
}
/// Where an item is, as libobs works out its box (`update_item_transform` in obs-scene.c):
/// the box is `bounds` (or the cropped source size times `scale`), its `align` point is at
/// `pos`, turned `rot` degrees about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Place {
    pub pos: (f32, f32),
    pub scale: (f32, f32),
    pub rot: f32,
    pub align: u32,
    pub bounds: Option<(f32, f32)>,
    /// The source's size after cropping.
    pub size: (u32, u32),
}

/// Live captions running on one sound; dropping it stops them.
#[derive(Debug)]
pub struct Captions {
    // Dropped first: the listener uses the Studio's libobs, kept alive by the field after it.
    #[cfg(target_os = "macos")]
    listener: ffi::speech::Listener,
    _studio: Studio,
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

/// Screen and window captures (each system's), and cameras: the kinds the black screen fixer
/// looks at.
const SCREEN_KINDS: [&str; 6] =
    ["screen_capture", "display_capture", "window_capture", "game_capture", "monitor_capture", "xcomposite_input"];
const CAMERA_KINDS: [&str; 5] =
    ["macos-avcapture", "macos-avcapture-fast", "av_capture_input", "dshow_input", "v4l2_input"];

/// A still (RGBA rows) with nothing to see: every pixel near black, or see-through.
fn all_black(px: &[u8]) -> bool {
    px.as_chunks::<4>().0.iter().all(|p| p[3] < 16 || p[0].max(p[1]).max(p[2]) < 12)
}

/// A capture as `follow_sizes` last saw it: its uncropped size, the box it's fitted in, and
/// its scale then (another one means it was resized by hand).
#[derive(Clone, Copy)]
struct Seen {
    raw: (u32, u32),
    target: (f32, f32),
    scale: (f32, f32),
}

impl Place {
    fn box_size(&self) -> (f32, f32) {
        self.bounds.unwrap_or((self.size.0 as f32 * self.scale.0, self.size.1 as f32 * self.scale.1))
    }

    /// The box's align point from its first corner (libobs rounds toward zero, as here).
    fn origin(&self) -> (f32, f32) {
        let (w, h) = self.box_size();
        let (w, h) = (w as i32, h as i32);
        let x = if self.align & ffi::OBS_ALIGN_RIGHT != 0 {
            w as f32
        } else if self.align & ffi::OBS_ALIGN_LEFT == 0 {
            (w / 2) as f32
        } else {
            0.0
        };
        let y = if self.align & ffi::OBS_ALIGN_BOTTOM != 0 {
            h as f32
        } else if self.align & ffi::OBS_ALIGN_TOP == 0 {
            (h / 2) as f32
        } else {
            0.0
        };
        (x, y)
    }

    fn turn(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (s, c) = self.rot.to_radians().sin_cos();
        (x * c - y * s, x * s + y * c)
    }

    /// `[side_x, side_y, corner]` as in [`ItemInfo::frame`].
    pub fn frame(&self) -> [f32; 6] {
        let (w, h) = self.box_size();
        let u = self.turn((w, 0.0));
        let v = self.turn((0.0, h));
        let o = self.turn(self.origin());
        [u.0, u.1, v.0, v.1, self.pos.0 - o.0, self.pos.1 - o.1]
    }

    /// The same item with its box's first corner at (`x`, `y`) and sides `w` and `h` long.
    /// The scale keeps its sign (a flip stays); a source with no size yet only moves.
    pub fn with_box(&self, x: f32, y: f32, w: f32, h: f32) -> Place {
        let mut p = *self;
        if p.bounds.is_some() {
            p.bounds = Some((w, h));
        } else {
            if p.size.0 > 0 {
                p.scale.0 = w / p.size.0 as f32 * if p.scale.0 < 0.0 { -1.0 } else { 1.0 };
            }
            if p.size.1 > 0 {
                p.scale.1 = h / p.size.1 as f32 * if p.scale.1 < 0.0 { -1.0 } else { 1.0 };
            }
        }
        let o = p.turn(p.origin());
        p.pos = (x + o.0, y + o.1);
        p
    }
}

/// A box ([`ItemInfo::frame`]) in a group's pixels, put on the canvas by the group's
/// transform (libobs's row vectors: a point p goes to p.x·m.x + p.y·m.y + m.t).
fn on_canvas(f: [f32; 6], m: &ffi::Matrix4) -> [f32; 6] {
    let side = |x: f32, y: f32| (x * m.x.x + y * m.y.x, x * m.x.y + y * m.y.y);
    let (ux, uy) = side(f[0], f[1]);
    let (vx, vy) = side(f[2], f[3]);
    let (cx, cy) = side(f[4], f[5]);
    [ux, uy, vx, vy, cx + m.t.x, cy + m.t.y]
}

/// A canvas point in a group's pixels (the inverse of [`on_canvas`] for a point).
fn off_canvas(m: &ffi::Matrix4, p: (f32, f32)) -> (f32, f32) {
    let (a, b, c, d) = (m.x.x, m.x.y, m.y.x, m.y.y);
    let det = a * d - b * c;
    if det.abs() < f32::EPSILON {
        return p;
    }
    let (px, py) = (p.0 - m.t.x, p.1 - m.t.y);
    ((px * d - py * c) / det, (py * a - px * b) / det)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_is_black_only_when_nothing_shows() {
        // Pure black, see-through and very dark count as nothing; one lit pixel doesn't.
        assert!(all_black(&[0, 0, 0, 255, 5, 8, 11, 255]));
        assert!(all_black(&[200, 200, 200, 0, 0, 0, 0, 255]));
        assert!(!all_black(&[0, 0, 0, 255, 12, 0, 0, 255]));
        assert!(!all_black(&[0, 0, 0, 255, 255, 255, 255, 255]));
        // An empty still (nothing drawn) has nothing to see.
        assert!(all_black(&[]));
    }

    fn plain() -> Place {
        Place { pos: (0.0, 0.0), scale: (1.0, 1.0), rot: 0.0, align: 5, bounds: None, size: (400, 300) }
    }

    fn close(a: [f32; 6], b: [f32; 6]) {
        assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01), "{a:?} != {b:?}");
    }

    #[test]
    fn group_pixels_and_canvas_go_both_ways() {
        let m4 = |ax: f32, ay: f32, bx: f32, by: f32, tx: f32, ty: f32| ffi::Matrix4 {
            x: ffi::Vec4 { x: ax, y: ay, z: 0.0, w: 0.0 },
            y: ffi::Vec4 { x: bx, y: by, z: 0.0, w: 0.0 },
            z: ffi::Vec4 { x: 0.0, y: 0.0, z: 1.0, w: 0.0 },
            t: ffi::Vec4 { x: tx, y: ty, z: 0.0, w: 1.0 },
        };
        // A group at (300, 200), half size.
        let half = m4(0.5, 0.0, 0.0, 0.5, 300.0, 200.0);
        assert_eq!(on_canvas([100.0, 0.0, 0.0, 50.0, 40.0, 20.0], &half), [50.0, 0.0, 0.0, 25.0, 320.0, 210.0]);
        assert_eq!(off_canvas(&half, (320.0, 210.0)), (40.0, 20.0));
        // Turned a quarter, doubled: a point there and back lands where it started.
        let turned = m4(0.0, 2.0, -2.0, 0.0, 500.0, 100.0);
        let p = (37.0, -12.5);
        let f = on_canvas([0.0, 0.0, 0.0, 0.0, p.0, p.1], &turned);
        let back = off_canvas(&turned, (f[4], f[5]));
        assert!((back.0 - p.0).abs() < 1e-3 && (back.1 - p.1).abs() < 1e-3, "{back:?}");
        // A group with no size leaves the point alone rather than dividing by zero.
        assert_eq!(off_canvas(&m4(0.0, 0.0, 0.0, 0.0, 9.0, 9.0), (1.0, 2.0)), (1.0, 2.0));
    }

    #[test]
    fn boxes_come_out_as_libobs_works_them_out() {
        close(plain().frame(), [400.0, 0.0, 0.0, 300.0, 0.0, 0.0]);
        // Centred anchor (a flip or turn put it there), flipped across.
        let flipped = Place { pos: (500.0, 200.0), scale: (-2.0, 1.0), align: 0, ..plain() };
        close(flipped.frame(), [-800.0, 0.0, 0.0, 300.0, 900.0, 50.0]);
        // A quarter turn about the centre.
        let turned = Place { pos: (500.0, 200.0), rot: 90.0, align: 0, ..plain() };
        close(turned.frame(), [0.0, 400.0, -300.0, 0.0, 650.0, 0.0]);
        // A bounding box is the box, whatever the scale.
        let bounded = Place { bounds: Some((1920.0, 1080.0)), scale: (3.0, 3.0), ..plain() };
        close(bounded.frame(), [1920.0, 0.0, 0.0, 1080.0, 0.0, 0.0]);
    }

    #[test]
    fn a_new_box_lands_exactly_where_asked() {
        for p in [
            plain(),
            Place { pos: (500.0, 200.0), scale: (-2.0, 1.0), align: 0, ..plain() },
            Place { pos: (500.0, 200.0), rot: 30.0, align: 10, ..plain() },
            Place { bounds: Some((1920.0, 1080.0)), ..plain() },
        ] {
            let f = p.with_box(100.0, 50.0, 640.0, 360.0).frame();
            let (lu, lv) = (f[0].hypot(f[1]), f[2].hypot(f[3]));
            assert!((lu - 640.0).abs() < 0.01 && (lv - 360.0).abs() < 0.01, "{p:?}: {f:?}");
            close([f[4], f[5], 0.0, 0.0, 0.0, 0.0], [100.0, 50.0, 0.0, 0.0, 0.0, 0.0]);
        }
        // A flip stays a flip.
        let f = Place { scale: (-1.0, 1.0), ..plain() }.with_box(0.0, 0.0, 200.0, 150.0);
        assert_eq!(f.scale, (-0.5, 0.5));
        // A source with no size yet only moves.
        let empty = Place { size: (0, 0), ..plain() }.with_box(10.0, 20.0, 50.0, 50.0);
        assert_eq!((empty.scale, empty.pos), ((1.0, 1.0), (10.0, 20.0)));
    }
}
