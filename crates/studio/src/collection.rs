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
    /// Where it is on the canvas, in pixels: its box's two sides and its first corner,
    /// `[side_x.x, side_x.y, side_y.x, side_y.y, corner.x, corner.y]` (a point (u, v) of the
    /// box, each 0 to 1, is at `corner + u * side_x + v * side_y`). In a group, inside it.
    pub frame: [f32; 6],
    pub children: Vec<ItemInfo>,
}

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
    /// From the bottom of the scene (0 is the lowest).
    order: usize,
    transform: Transform,
    visible: bool,
    locked: bool,
    blend: c_int,
    scale_filter: c_int,
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
        let cur = *self.current.borrow();
        if !cur.is_null() {
            a.obs_transition_set(t, cur);
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
        if self.find(name).is_some() {
            return Err(StudioError::Failed(format!("name: there's already a source or scene called \"{name}\".")));
        }
        Ok(())
    }

    fn scene_src(&self, name: &str) -> Result<*mut ffi::ObsSource, StudioError> {
        let a = self.api();
        self.scenes
            .borrow()
            .iter()
            .copied()
            .find(|s| text(a.obs_source_get_name(*s)) == name)
            .ok_or_else(|| no_scene(name))
    }

    fn scene_ptr(&self, name: &str) -> Result<*mut ffi::ObsScene, StudioError> {
        Ok(self.api().obs_scene_from_source(self.scene_src(name)?))
    }

    /// Puts a scene on air, through the transition (`ms` 0 cuts).
    fn go(&self, scene: *mut ffi::ObsSource, ms: u32) {
        let a = self.api();
        let t = *self.transition.borrow();
        if ms == 0 || !a.obs_transition_start(t, ffi::OBS_TRANSITION_MODE_AUTO, ms, scene) {
            a.obs_transition_set(t, scene);
        }
        self.current.replace(scene);
    }

    // ---- Scenes ----

    pub fn scenes(&self) -> Vec<SceneInfo> {
        let a = self.api();
        let cur = *self.current.borrow();
        self.scenes
            .borrow()
            .iter()
            .map(|s| SceneInfo {
                name: text(a.obs_source_get_name(*s)),
                items: self.raw_items(a.obs_scene_from_source(*s)).len(),
                current: *s == cur,
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
        let at = self.index_of(name)? + 1;
        self.scenes.borrow_mut().insert(at, src);
        Ok(())
    }

    fn index_of(&self, name: &str) -> Result<usize, StudioError> {
        let src = self.scene_src(name)?;
        Ok(self.scenes.borrow().iter().position(|s| *s == src).unwrap_or(0))
    }

    /// Removes a scene (never the last one). If it was on air, the one before it goes on.
    pub fn remove_scene(&self, name: &str) -> Result<(), StudioError> {
        let src = self.scene_src(name)?;
        if self.scenes.borrow().len() == 1 {
            return Err(StudioError::Failed("A collection keeps at least one scene. Add another first.".into()));
        }
        let at = self.index_of(name)?;
        self.scenes.borrow_mut().remove(at);
        if *self.current.borrow() == src {
            let next = self.scenes.borrow()[at.saturating_sub(1)];
            self.go(next, 0);
        }
        let a = self.api();
        a.obs_source_remove(src);
        a.obs_source_release(src);
        Ok(())
    }

    /// Moves a scene to `to` in the list (0 is the top).
    pub fn move_scene(&self, name: &str, to: usize) -> Result<(), StudioError> {
        let at = self.index_of(name)?;
        let mut v = self.scenes.borrow_mut();
        let s = v.remove(at);
        let to = to.min(v.len());
        v.insert(to, s);
        Ok(())
    }

    /// Puts a scene on air through the transition.
    pub fn set_current(&self, name: &str) -> Result<(), StudioError> {
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
        let src = self.source(name)?;
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
            frame: self.place(item).frame(),
            children: if group {
                self.group_items(item).into_iter().rev().map(|i| self.info(i)).collect()
            } else {
                Vec::new()
            },
        }
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
        if src.ptr == self.scene_src(scene).unwrap_or(ptr::null_mut()) {
            return Err(StudioError::Failed("A scene can't be put inside itself.".into()));
        }
        let item = self.api().obs_scene_add(s, src.ptr);
        if item.is_null() {
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        Ok(self.info(item))
    }

    fn fit(&self, item: *mut ffi::ObsSceneItem) {
        let a = self.api();
        let c = self.studio.canvas();
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
        a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_SCALE_INNER);
        a.obs_sceneitem_set_bounds_alignment(item, ffi::OBS_ALIGN_CENTER);
        a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: c.width as f32, y: c.height as f32 });
    }

    /// Puts the item's anchor at its middle without moving it on screen, so flips and turns
    /// happen in place (as OBS does).
    fn anchor_middle(&self, item: *mut ffi::ObsSceneItem) {
        let a = self.api();
        let mut m = ffi::Matrix4::default();
        a.obs_sceneitem_get_box_transform(item, &mut m);
        let mid = ffi::Vec2 { x: (m.x.x + m.y.x) * 0.5 + m.t.x, y: (m.x.y + m.y.y) * 0.5 + m.t.y };
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_CENTER);
        a.obs_sceneitem_set_pos(item, &mid);
    }

    fn scale_by(&self, item: *mut ffi::ObsSceneItem, x: f32, y: f32) {
        let a = self.api();
        self.anchor_middle(item);
        let mut s = ffi::Vec2 { x: 1.0, y: 1.0 };
        a.obs_sceneitem_get_scale(item, &mut s);
        a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: s.x * x, y: s.y * y });
    }

    fn turn(&self, item: *mut ffi::ObsSceneItem, deg: f32) {
        let a = self.api();
        self.anchor_middle(item);
        a.obs_sceneitem_set_rot(item, (a.obs_sceneitem_get_rot(item) + deg).rem_euclid(360.0));
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
        let c = self.studio.canvas();
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
                a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_CENTER);
                a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: c.width as f32 / 2.0, y: c.height as f32 / 2.0 });
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
        let items = self.raw_items(s);
        let order = items
            .iter()
            .position(|i| self.api().obs_sceneitem_get_id(*i) == id)
            .ok_or_else(|| StudioError::Failed(format!("there's no source with id {id} in \"{scene}\"")))?;
        let item = items[order];
        let a = self.api();
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
            transform: self.transform(scene, id)?,
            visible: a.obs_sceneitem_visible(item),
            locked: a.obs_sceneitem_locked(item),
            blend: a.obs_sceneitem_get_blending_mode(item),
            scale_filter: a.obs_sceneitem_get_scale_filter(item),
        })
    }

    /// Puts an item back as `snap` remembers it, adding its source to the scene again if the
    /// item was removed. Returns its id now (new if it was added again).
    pub fn restore(&self, snap: &ItemSnap) -> Result<i64, StudioError> {
        let s = self.scene_ptr(&snap.scene)?;
        let a = self.api();
        let item = match self.raw_items(s).into_iter().find(|i| a.obs_sceneitem_get_id(*i) == snap.id) {
            Some(i) => i,
            None => {
                let i = a.obs_scene_add(s, snap.src);
                if i.is_null() {
                    return Err(StudioError::Failed(format!("\"{}\" couldn't be put back", snap.name)));
                }
                i
            }
        };
        let id = a.obs_sceneitem_get_id(item);
        self.set_transform(&snap.scene, id, &snap.transform)?;
        a.obs_sceneitem_set_visible(item, snap.visible);
        a.obs_sceneitem_set_locked(item, snap.locked);
        a.obs_sceneitem_set_blending_mode(item, snap.blend);
        a.obs_sceneitem_set_scale_filter(item, snap.scale_filter);
        a.obs_sceneitem_set_order_position(item, c_int::try_from(snap.order).unwrap_or(0));
        Ok(id)
    }

    /// The ids of a scene's top-level items (to find what an act added).
    pub fn item_ids(&self, scene: &str) -> Result<Vec<i64>, StudioError> {
        let s = self.scene_ptr(scene)?;
        Ok(self.raw_items(s).into_iter().map(|i| self.api().obs_sceneitem_get_id(i)).collect())
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
        const MAX: f32 = 100_000.0;
        if ![x, y, w, h].iter().all(|v| v.is_finite() && v.abs() <= MAX) || w < 1.0 || h < 1.0 {
            return Err(StudioError::Failed("That size or place is off the canvas.".into()));
        }
        let item = self.item(scene, id)?;
        let a = self.api();
        let next = self.place(item).with_box(x, y, w, h);
        if next.bounds.is_some() {
            a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: next.box_size().0, y: next.box_size().1 });
        } else {
            a.obs_sceneitem_set_scale(item, &ffi::Vec2 { x: next.scale.0, y: next.scale.1 });
        }
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: next.pos.0, y: next.pos.1 });
        Ok(self.info(item))
    }

    /// Every source in a scene (groups included), with the scenes it's in, in scene order.
    pub fn sources(&self) -> Vec<SourceUse> {
        let a = self.api();
        let mut out: Vec<SourceUse> = Vec::new();
        for s in self.scenes.borrow().iter() {
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
        }
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
        let src = self.source(name)?;
        let a = self.api();
        let (w, h) = if a.obs_source_is_scene(src.ptr) {
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
        let arr = a.obs_save_sources_filtered(ffi::save_unless_listed, &skip as *const _ as *mut c_void);
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
        doc.insert("ferrocast".into(), json!({ "transition": t, "transition_settings": settings }));
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
            c.scenes.replace(scenes);
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
        Ok(c)
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        self.meters.borrow_mut().clear();
        let a = &self.studio.inner.api;
        a.obs_set_output_source(0, ptr::null_mut());
        // As OBS clears a collection: every source in a scene, each scene, the devices.
        let mut sources: Vec<*mut ffi::ObsSource> = Vec::new();
        for s in self.scenes.borrow().iter() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Place {
        Place { pos: (0.0, 0.0), scale: (1.0, 1.0), rot: 0.0, align: 5, bounds: None, size: (400, 300) }
    }

    fn close(a: [f32; 6], b: [f32; 6]) {
        assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01), "{a:?} != {b:?}");
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
