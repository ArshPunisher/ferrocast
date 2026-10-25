//! A scene collection (2.1 Studio): scenes in order, the sources in them, the global sound
//! devices, filters and the transition between scenes. libobs holds the sources; this holds a
//! reference to each scene and finds everything else by name, as OBS's own window does.
//!
//! Saved and loaded in OBS's own scene-collection format (`basic/scenes/*.json`): the same
//! `sources` array libobs writes, `scene_order`, `current_scene` and the six audio device keys,
//! so a collection can go to and from OBS.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
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
}

/// A source in a scene, top first. A group lists its own items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub children: Vec<ItemInfo>,
}

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
}

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
    meters: RefCell<HashMap<String, Meter>>,
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
            meters: RefCell::new(HashMap::new()),
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
        let t = a.obs_source_create_private(id.as_ptr(), n.as_ptr(), ptr::null_mut());
        if t.is_null() {
            return Err(StudioError::Failed(format!("the transition \"{kind}\" isn't available")));
        }
        let cur = *self.current.borrow();
        if !cur.is_null() {
            a.obs_transition_set(t, cur);
        }
        a.obs_set_output_source(0, t);
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
        let ms = self.transition_info.borrow().ms;
        self.go(src, ms);
        Ok(())
    }

    pub fn transition(&self) -> TransitionInfo {
        self.transition_info.borrow().clone()
    }

    pub fn set_transition(&self, t: &TransitionInfo) -> Result<(), StudioError> {
        if t.ms > 20_000 {
            return Err(StudioError::Failed("ms: a transition lasts 0 to 20000 ms.".into()));
        }
        if t.kind != self.transition_info.borrow().kind {
            self.make_transition(&t.kind)?;
        }
        self.transition_info.replace(t.clone());
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
        let c = self.studio.inner.canvas;
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
        a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_SCALE_INNER);
        a.obs_sceneitem_set_bounds_alignment(item, ffi::OBS_ALIGN_CENTER);
        a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: c.width as f32, y: c.height as f32 });
    }

    pub fn edit_item(&self, scene: &str, id: i64, edit: ItemEdit) -> Result<(), StudioError> {
        let item = self.item(scene, id)?;
        let a = self.api();
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
        }
        Ok(())
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
        meters.retain(|k, _| names.contains(k));
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
                };
                a.obs_volmeter_add_callback(vm, ffi::store_level, m.param());
                a.obs_volmeter_attach_source(vm, src.ptr);
                meters.insert(name.clone(), m);
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
        doc.insert("current_transition".into(), json!(if t.kind == "cut_transition" { "Cut" } else { "Fade" }));
        doc.insert("transition_duration".into(), json!(t.ms));
        doc.insert("transitions".into(), json!([]));
        doc.insert("ferrocast".into(), json!({ "transition": t }));
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
        if let Some(t) =
            doc.pointer("/ferrocast/transition").and_then(|t| serde_json::from_value::<TransitionInfo>(t.clone()).ok())
        {
            // An unknown kind keeps the fade.
            let _ = c.set_transition(&t);
        } else if let Some(ms) = doc.get("transition_duration").and_then(Value::as_u64) {
            let _ = c.set_transition(&TransitionInfo { kind: "fade_transition".into(), ms: ms.min(20_000) as u32 });
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
