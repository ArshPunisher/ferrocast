//! The Studio's scenes kept by Ferrocast itself, with no libobs: the scene collection is a
//! document in OBS's own format (what `Collection::save` writes and OBS reads), edited here and
//! handed to Ferrocast's own engine after each change, which draws it (unchanged sources keep
//! running). The same calls as `Collection`, returning the same answers, so the app can move
//! over call by call (ENGINE.md, step 3).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::collection::{
    AudioInfo, BLENDS, BOUNDS, FilterInfo, ItemEdit, ItemInfo, Monitor, SCALE_FILTERS, SceneInfo, SourceUse, Transform,
    TransitionInfo,
};
use crate::naming::{NameVideo, file_name};
use crate::{Canvas, RecordPlan, RecordStats, ReplayPlan, StreamSettings, StreamStats, StudioError, check_name, own};

/// The size a picture is glanced at (`look`).
const LOOK_W: u32 = 32;
const LOOK_H: u32 = 18;

/// OBS's global sound channels, in its saved collection: Desktop Audio 1-2, Mic/Aux 1-4.
const CHANNEL_KEYS: [&str; 6] = [
    "DesktopAudioDevice1",
    "DesktopAudioDevice2",
    "AuxAudioDevice1",
    "AuxAudioDevice2",
    "AuxAudioDevice3",
    "AuxAudioDevice4",
];

/// The hidden scenes Studio Mode puts on air: copies of a scene's layout (two, so the old one is
/// still there while the transition to the new one plays). Not a name anyone can type.
pub(crate) const PROGRAM_COPY: [&str; 2] = ["\u{1}On air 1", "\u{1}On air 2"];

/// The scene store.
#[derive(Debug)]
pub struct Stage {
    canvas: Canvas,
    /// The collection, as OBS saves one.
    doc: RefCell<Value>,
    /// Set while an edit's engine update is done by the caller itself (a moved source goes
    /// straight to the engine's item, not through opening the whole collection again).
    quiet: std::cell::Cell<bool>,
    /// Where monitored sound plays (a device's name; None: the system's), kept for each engine.
    // Read only where monitoring plays (macOS, Windows); Linux keeps it for the next build.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    monitor_device: RefCell<Option<String>>,
    /// Ferrocast's own engine with it open, once something asked for it.
    engine: RefCell<Option<Arc<own::Shared>>>,
    /// Web page sources, each a page rendered for our engine, with the settings it opened with.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pages: RefCell<std::collections::HashMap<String, (Value, crate::ffi::web::FedPage)>>,
    /// Media from network addresses (guests' cameras, streams), each read by FFmpeg.
    nets: RefCell<std::collections::HashMap<String, (Value, crate::netmedia::NetMedia)>>,
    /// NDI sources, each read from the network.
    ndis: RefCell<std::collections::HashMap<String, (String, crate::ndi_io::NdiIn)>>,
    /// NDI sources that didn't start, with why (said once, tried again at each edit).
    ndi_failed: RefCell<std::collections::HashMap<String, String>>,
    /// Remove background filters at work, by their mask picture's name.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    cutouts: RefCell<std::collections::HashMap<String, crate::cutout::Cutout>>,
    /// The frame rate as last worked out: when, the engine's frames and skipped count then,
    /// and the rate over the second before (`stats`).
    fps_seen: std::cell::Cell<Option<(std::time::Instant, u64, u64, f64)>>,
}

fn fail(msg: impl Into<String>) -> StudioError {
    StudioError::Failed(msg.into())
}

fn name_of(s: &Value) -> &str {
    s.get("name").and_then(Value::as_str).unwrap_or_default()
}

/// A source's or filter's settings with its kind's defaults under them, as OBS reads them: OBS
/// saves only what differs from the defaults, so an imported crop with no "relative" is a
/// relative one, and its form should say so (9 Nov).
fn with_defaults(it: &Value) -> Value {
    let mut out = crate::kinds::defaults(kind_of(it)).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let (Some(all), Some(own)) = (out.as_object_mut(), it.get("settings").and_then(Value::as_object)) {
        for (k, v) in own {
            all.insert(k.clone(), v.clone());
        }
    }
    out
}

fn kind_of(s: &Value) -> &str {
    s.get("versioned_id").or_else(|| s.get("id")).and_then(Value::as_str).unwrap_or_default()
}

fn is_scene(s: &Value) -> bool {
    matches!(s.get("id").and_then(Value::as_str), Some("scene" | "group"))
}

/// Takes out the sources no scene shows any more, as OBS does when a source's last item goes:
/// a group too (and then its own sources). Scenes stay, and so do what a Copy of a source
/// copies and the sound Sound bars show. Before, on 10 Nov on Windows, every removed source kept running in the engine (six
/// FFmpegs for one clip on air, 26% CPU, frames late) and its name stayed taken.
fn drop_unused(d: &mut Value) {
    // What Studio Mode's copy on air shows stays until the next transition, as in OBS.
    let on_air: Vec<String> = ["program_copy", "program_prev"]
        .iter()
        .filter_map(|k| d.pointer(&format!("/ferrocast/{k}/items")).and_then(Value::as_array))
        .flatten()
        .map(|i| name_of(i).to_string())
        .collect();
    let Some(list) = d.get_mut("sources").and_then(Value::as_array_mut) else { return };
    let scene = |s: &Value| s.get("id").and_then(Value::as_str) == Some("scene");
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut todo: Vec<&Value> = list.iter().filter(|s| scene(s)).collect();
    for n in &on_air {
        if used.insert(n.clone())
            && let Some(next) = list.iter().find(|x| name_of(x) == n)
        {
            todo.push(next);
        }
    }
    while let Some(s) = todo.pop() {
        let items = s.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten().map(name_of);
        // What a Copy of a source copies, and the sound Sound bars show.
        let copied = match kind_of(s) {
            "ferrocast_clone" | "source-clone" => s.pointer("/settings/clone").and_then(Value::as_str),
            "ferrocast_visualizer" => s.pointer("/settings/source").and_then(Value::as_str),
            _ => None,
        };
        for n in items.chain(copied) {
            if used.insert(n.to_string())
                && let Some(next) = list.iter().find(|x| name_of(x) == n)
            {
                todo.push(next);
            }
        }
    }
    list.retain(|s| scene(s) || used.contains(name_of(s)));
}

/// A new id for something saved (OBS writes UUIDs; any unique string does).
fn uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{:032x}", t ^ (u128::from(N.fetch_add(1, Ordering::Relaxed)) << 64))
}

/// dB to OBS's saved volume (a multiplier) and back.
fn mul_of(db: f32) -> f64 {
    if db <= -100.0 { 0.0 } else { 10f64.powf(f64::from(db) / 20.0) }
}

fn db_of(mul: f64) -> f32 {
    if mul <= 0.0 { -100.0 } else { (20.0 * mul.log10()).max(-100.0) as f32 }
}

fn ok_or<T>(v: Option<T>) -> Result<T, StudioError> {
    v.ok_or_else(|| fail("Pick the sources to group first."))
}

/// A new item as OBS saves one: its own size at the top left, shown.
fn new_item(source: &Value, id: i64) -> Value {
    json!({
        "name": name_of(source),
        "source_uuid": source.get("uuid").cloned().unwrap_or(Value::Null),
        "id": id,
        "visible": true,
        "locked": false,
        "rot": 0.0,
        "pos": { "x": 0.0, "y": 0.0 },
        "scale": { "x": 1.0, "y": 1.0 },
        "align": 5,
        "bounds_type": 0,
        "bounds_align": 0,
        "bounds": { "x": 0.0, "y": 0.0 },
        "crop_left": 0, "crop_top": 0, "crop_right": 0, "crop_bottom": 0,
        "scale_filter": "disable",
        "blend_method": "default",
        "blend_type": "normal",
        "group_item_backup": false,
    })
}

impl Stage {
    /// A collection with one empty scene, on air.
    pub fn new(canvas: Canvas, first_scene: &str) -> Result<Self, StudioError> {
        check_name(first_scene)?;
        let doc = json!({
            "name": "Ferrocast",
            "sources": [],
            "scene_order": [],
            "current_scene": first_scene,
            "current_program_scene": first_scene,
            "current_transition": "Fade",
            "transition_duration": 300,
        });
        let s = Self {
            canvas,
            doc: RefCell::new(doc),
            quiet: std::cell::Cell::new(false),
            monitor_device: RefCell::new(None),
            engine: RefCell::new(None),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            pages: RefCell::default(),
            nets: RefCell::default(),
            ndis: RefCell::default(),
            ndi_failed: RefCell::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            cutouts: RefCell::default(),
            fps_seen: std::cell::Cell::new(None),
        };
        s.add_scene(first_scene)?;
        Ok(s)
    }

    /// A collection OBS (or `save`) wrote. One with no scenes gets `first_scene`.
    pub fn load(canvas: Canvas, doc: &Value, first_scene: &str) -> Result<Self, StudioError> {
        let mut doc = doc.clone();
        if !doc.get("sources").is_some_and(Value::is_array) {
            doc["sources"] = json!([]);
        }
        // Sources an older Ferrocast kept after their last scene item went (they ran unseen).
        drop_unused(&mut doc);
        let s = Self {
            canvas,
            doc: RefCell::new(doc),
            quiet: std::cell::Cell::new(false),
            monitor_device: RefCell::new(None),
            engine: RefCell::new(None),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            pages: RefCell::default(),
            nets: RefCell::default(),
            ndis: RefCell::default(),
            ndi_failed: RefCell::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            cutouts: RefCell::default(),
            fps_seen: std::cell::Cell::new(None),
        };
        if s.scenes().is_empty() {
            s.add_scene(first_scene)?;
        }
        if !s.scenes().iter().any(|c| c.current) {
            let first = s.scenes()[0].name.clone();
            s.edit(|d| {
                d["current_scene"] = json!(first);
                d["current_program_scene"] = json!(first);
            });
        }
        Ok(s)
    }

    /// The collection as OBS writes a scene collection.
    pub fn save(&self, name: &str) -> Value {
        let mut d = self.doc.borrow().clone();
        d["name"] = json!(name);
        d
    }

    /// Ferrocast's own engine with this collection open: the one running, or a new one.
    pub(crate) fn engine(&self) -> Result<Arc<own::Shared>, StudioError> {
        if let Some(e) = self.engine.borrow().as_ref() {
            return Ok(e.clone());
        }
        crate::webfetch::install();
        let shared = own::Shared::open_at(self.canvas, &self.doc.borrow())?;
        // Monitoring plays from the start, as libobs keeps its device open: opened only when
        // Settings were saved, every new engine (each start, a new picture size) had none, and
        // a source set to be heard here ("Monitor only", "Monitor and output") played nowhere.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Err(e) = shared.engine.set_monitoring(true, self.monitor_device.borrow().as_deref()) {
            tracing::warn!("monitoring couldn't start: {e}");
        }
        *self.engine.borrow_mut() = Some(shared.clone());
        self.sync_pages();
        Ok(shared)
    }

    /// Changes the document, then the engine shows it.
    fn edit<T>(&self, f: impl FnOnce(&mut Value) -> T) -> T {
        let out = f(&mut self.doc.borrow_mut());
        if self.quiet.get() {
            return out;
        }
        if let Some(e) = self.engine.borrow().as_ref() {
            e.reopen(&self.doc.borrow());
        }
        self.sync_pages();
        out
    }

    /// Starts or stops finding the person for each Remove background filter that's on.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn sync_cutouts(&self, e: &Arc<own::Shared>) {
        let want: Vec<(String, String)> = self
            .sources_list()
            .iter()
            .flat_map(|s| {
                let source = name_of(s).to_string();
                s.get("filters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|f| {
                        f.get("versioned_id").or_else(|| f.get("id")).and_then(Value::as_str)
                            == Some("ferrocast_remove_background")
                            && f.get("enabled").and_then(Value::as_bool) != Some(false)
                    })
                    .map(move |f| {
                        let fname = f.get("name").and_then(Value::as_str).unwrap_or("ferrocast_remove_background");
                        // The engine names the mask picture so (ferrocore::obs).
                        (format!("{source}: {fname}"), source.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut cutouts = self.cutouts.borrow_mut();
        // Gone, or working for an engine that's gone (a new canvas): stopped.
        cutouts.retain(|mask, c| c.serves(e) && want.iter().any(|(m, _)| m == mask));
        for (mask, source) in want {
            cutouts
                .entry(mask.clone())
                .or_insert_with(|| crate::cutout::Cutout::start(Arc::downgrade(e), source, mask));
        }
    }

    /// Starts, restarts or stops reading the network media sources (a guest's camera…), and
    /// hands each its source's feed.
    fn sync_ndis(&self, e: &own::Shared) {
        let want: Vec<(String, String)> = self
            .sources_list()
            .iter()
            .filter(|s| kind_of(s) == "ndi_source")
            .map(|s| {
                let n = s.pointer("/settings/ndi_source_name").and_then(Value::as_str).unwrap_or_default();
                (name_of(s).to_string(), n.to_string())
            })
            .filter(|(_, n)| !n.is_empty())
            .collect();
        let mut ndis = self.ndis.borrow_mut();
        ndis.retain(|name, (was, _)| want.iter().any(|(n, src)| n == name && src == was));
        for (name, src) in want {
            let Some(feed) = e.engine.source_named(&name).and_then(|(id, _)| e.engine.fed(id)) else { continue };
            match ndis.get(&name) {
                Some((_, r)) => r.set_feed(feed),
                None => match crate::ndi_io::NdiIn::start(&src, feed) {
                    Ok(r) => {
                        self.ndi_failed.borrow_mut().remove(&name);
                        ndis.insert(name, (src, r));
                    }
                    Err(e) => {
                        if self.ndi_failed.borrow_mut().insert(name.clone(), e.clone()).as_ref() != Some(&e) {
                            tracing::warn!(source = %name, "an NDI source didn't start: {e}");
                        }
                    }
                },
            }
        }
    }

    /// Sends what's on air out on the network as the NDI source `name`.
    pub fn ndi_output(&self, name: &str) -> Result<crate::NdiOut, StudioError> {
        let e = self.engine()?;
        let size = (self.canvas.width, self.canvas.height);
        crate::ndi_io::NdiOut::start(Arc::downgrade(&e), name, size, self.canvas.fps).map_err(fail)
    }

    fn sync_nets(&self, e: &own::Shared) {
        let net: Vec<(String, Value)> = self
            .sources_list()
            .iter()
            .filter(|s| kind_of(s) == "ffmpeg_source")
            .filter(|s| s.pointer("/settings/is_local_file").and_then(Value::as_bool) == Some(false))
            .map(|s| (name_of(s).to_string(), s.get("settings").cloned().unwrap_or_default()))
            .collect();
        let mut nets = self.nets.borrow_mut();
        nets.retain(|name, (was, _)| net.iter().any(|(n, set)| n == name && set == was));
        for (name, set) in net {
            let Some(feed) = e.engine.source_named(&name).and_then(|(id, _)| e.engine.fed(id)) else { continue };
            match nets.get(&name) {
                Some((_, m)) => m.set_feed(feed),
                None => match crate::netmedia::NetMedia::start(&set, feed) {
                    Ok(m) => {
                        nets.insert(name, (set, m));
                    }
                    Err(e) => tracing::warn!(source = %name, "a network media source didn't start: {e}"),
                },
            }
        }
    }

    /// Opens, updates or closes the web pages our engine shows, as the collection has them, and
    /// hands each its source's feed (a new one after the collection is opened again). Main
    /// thread (WebKit on a Mac).
    fn sync_pages(&self) {
        if let Some(e) = self.engine.borrow().clone() {
            self.sync_ndis(&e);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if let Some(e) = self.engine.borrow().clone() {
            self.sync_nets(&e);
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let Some(e) = self.engine.borrow().clone() else { return };
            let web: Vec<(String, Value)> = self
                .sources_list()
                .iter()
                .filter(|s| matches!(kind_of(s), "browser_source" | "ferrocast_web"))
                .map(|s| (name_of(s).to_string(), s.get("settings").cloned().unwrap_or_default()))
                .collect();
            self.sync_nets(&e);
            self.sync_cutouts(&e);
            let mut pages = self.pages.borrow_mut();
            pages.retain(|name, _| web.iter().any(|(n, _)| n == name));
            for (name, set) in web {
                let Some(feed) = e.engine.source_named(&name).and_then(|(id, _)| e.engine.fed(id)) else { continue };
                let n = |k: &str, d: u32| set.get(k).and_then(Value::as_u64).filter(|v| *v > 0).map_or(d, |v| v as u32);
                let (w, h, fps) = (n("width", 800), n("height", 600), n("fps", 30));
                let css = set.get("css").and_then(Value::as_str).unwrap_or_default().to_string();
                let local = set.get("is_local_file").and_then(Value::as_bool) == Some(true);
                let url = if local {
                    let file = set.get("local_file").and_then(Value::as_str).unwrap_or_default();
                    format!("file://{file}")
                } else {
                    set.get("url").and_then(Value::as_str).unwrap_or_default().to_string()
                };
                match pages.get_mut(&name) {
                    Some((was, page)) => {
                        if *was != set {
                            page.update(&url, w, h, &css, fps);
                            *was = set;
                        }
                        page.set_feed(feed);
                    }
                    None => {
                        if let Some(page) = crate::ffi::web::FedPage::open(&url, w, h, &css, fps, feed) {
                            pages.insert(name, (set, page));
                        }
                    }
                }
            }
        }
    }

    fn sources_list(&self) -> Vec<Value> {
        self.doc.borrow().get("sources").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    fn source(&self, name: &str) -> Result<Value, StudioError> {
        self.sources_list()
            .into_iter()
            .chain(self.channels_list())
            .find(|s| name_of(s) == name)
            .ok_or_else(|| fail(format!("There's no source called \"{name}\".")))
    }

    fn channels_list(&self) -> Vec<Value> {
        let d = self.doc.borrow();
        CHANNEL_KEYS.iter().filter_map(|k| d.get(*k).cloned()).collect()
    }

    /// Runs `f` on the saved source called `name` (a channel's too).
    fn with_source<T>(&self, name: &str, f: impl FnOnce(&mut Value) -> T) -> Result<T, StudioError> {
        let found = self.edit(|d| {
            if let Some(s) = d["sources"].as_array_mut().and_then(|l| l.iter_mut().find(|s| name_of(s) == name)) {
                return Some(f(s));
            }
            for k in CHANNEL_KEYS {
                if let Some(s) = d.get_mut(k).filter(|s| name_of(s) == name) {
                    return Some(f(s));
                }
            }
            None
        });
        found.ok_or_else(|| fail(format!("There's no source called \"{name}\".")))
    }

    fn name_free(&self, name: &str) -> Result<(), StudioError> {
        check_name(name)?;
        if self.sources_list().iter().chain(self.channels_list().iter()).any(|s| name_of(s) == name) {
            return Err(fail(format!("\"{name}\" is taken: pick another name.")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ scenes

    /// The vertical picture's scenes, in order (kept beside the main ones in the collection).
    fn vertical_order(&self) -> Vec<String> {
        let d = self.doc.borrow();
        d.pointer("/ferrocast/vertical/scene_order")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| o.get("name").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// The main picture's scenes, in order.
    fn order(&self) -> Vec<String> {
        let vertical = self.vertical_order();
        let d = self.doc.borrow();
        let scenes: Vec<String> = self
            .sources_list()
            .iter()
            .filter(|s| s.get("id").and_then(Value::as_str) == Some("scene"))
            .map(|s| name_of(s).to_string())
            .filter(|n| !vertical.contains(n))
            .collect();
        let mut out: Vec<String> = d
            .get("scene_order")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| o.get("name").and_then(Value::as_str))
            .filter(|n| scenes.iter().any(|s| s == n))
            .map(String::from)
            .collect();
        for s in scenes {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    fn scene_items(&self, scene: &str) -> Result<Vec<Value>, StudioError> {
        let s = self.source(scene)?;
        if !is_scene(&s) {
            return Err(fail(format!("\"{scene}\" isn't a scene.")));
        }
        Ok(s.get("settings").and_then(|v| v.get("items")).and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn set_order(&self, order: &[String]) {
        let list: Vec<Value> = order.iter().map(|n| json!({ "name": n })).collect();
        self.edit(|d| d["scene_order"] = Value::Array(list));
    }

    pub fn scenes(&self) -> Vec<SceneInfo> {
        let cur = self.current();
        self.order()
            .into_iter()
            .map(|name| {
                let items = self.scene_items(&name).map_or(0, |i| i.len());
                SceneInfo { current: name == cur, items, name, preview: false, transition: None }
            })
            .collect()
    }

    pub fn current(&self) -> String {
        self.doc.borrow().get("current_program_scene").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    /// Opens the scenes again in the engine as they are (a plugin's effect came or went).
    pub fn refresh(&self) {
        if let Some(e) = self.engine.borrow().as_ref() {
            e.reopen(&self.doc.borrow());
        }
    }

    /// Instant replay: every Instant Replay source plays `file` (a replay just saved) from the
    /// start. Returns the first such source that asked to be shown while it plays, and the scene
    /// it's in (the app goes there and back), when one did.
    pub fn show_replay(&self, file: &Path) -> Result<Option<(String, String)>, StudioError> {
        let file = file.to_str().ok_or_else(|| fail("The replay's file name isn't readable."))?;
        let replays: Vec<(String, bool)> = self
            .sources_list()
            .iter()
            .filter(|s| kind_of(s) == "ferrocast_replay")
            .map(|s| (name_of(s).to_string(), s.pointer("/settings/switch").and_then(Value::as_bool).unwrap_or(true)))
            .collect();
        if replays.is_empty() {
            return Ok(None);
        }
        self.edit(|d| {
            for s in d["sources"].as_array_mut().into_iter().flatten() {
                if kind_of(s) == "ferrocast_replay" {
                    s["settings"]["local_file"] = json!(file);
                }
            }
        });
        let scenes = self.order();
        Ok(replays.into_iter().filter(|(_, switch)| *switch).find_map(|(name, _)| {
            let scene = scenes
                .iter()
                .find(|sc| self.scene_items(sc).is_ok_and(|items| items.iter().any(|it| name_of(it) == name)))?;
            Some((name, scene.clone()))
        }))
    }

    /// The scene shown over whatever is on air (the top layer: Exeldro's Downstream Keyer, built
    /// in), when one is set and still there.
    pub fn top_layer(&self) -> Option<String> {
        let top = self.doc.borrow().get("ferrocast_top_layer").and_then(Value::as_str).map(String::from)?;
        self.order().contains(&top).then_some(top)
    }

    /// Shows `scene` over every scene on air, or none.
    pub fn set_top_layer(&self, scene: Option<&str>) -> Result<(), StudioError> {
        if let Some(s) = scene {
            self.scene_index(s)?;
        }
        self.edit(|d| match scene {
            Some(s) => d["ferrocast_top_layer"] = json!(s),
            None => {
                if let Some(o) = d.as_object_mut() {
                    o.remove("ferrocast_top_layer");
                }
            }
        });
        Ok(())
    }

    pub fn scene_index(&self, name: &str) -> Result<usize, StudioError> {
        self.order().iter().position(|n| n == name).ok_or_else(|| fail(format!("There's no scene called \"{name}\".")))
    }

    pub fn add_scene(&self, name: &str) -> Result<(), StudioError> {
        self.name_free(name)?;
        let scene = json!({
            "id": "scene", "versioned_id": "scene", "name": name, "uuid": uuid(),
            "settings": { "items": [], "id_counter": 0, "custom_size": false },
            "mixers": 0, "sync": 0, "flags": 0, "volume": 1.0, "balance": 0.5, "enabled": true, "muted": false,
            "filters": [],
        });
        let mut order = self.order();
        order.push(name.to_string());
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(scene)));
        self.set_order(&order);
        Ok(())
    }

    pub fn remove_scene(&self, name: &str) -> Result<(), StudioError> {
        let order = self.order();
        if !order.iter().any(|n| n == name) {
            return Err(fail(format!("There's no scene called \"{name}\".")));
        }
        if order.len() == 1 {
            return Err(fail("A collection keeps at least one scene."));
        }
        let rest: Vec<String> = order.into_iter().filter(|n| n != name).collect();
        let on_air = self.current() == name;
        self.edit(|d| {
            if let Some(l) = d["sources"].as_array_mut() {
                l.retain(|s| name_of(s) != name);
                // Out of every other scene too (a scene shown inside one).
                for s in l.iter_mut() {
                    if let Some(items) = s.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
                        items.retain(|i| name_of(i) != name);
                    }
                }
            }
            // Its sources go too, unless another scene shows them.
            drop_unused(d);
            if on_air {
                d["current_scene"] = json!(rest[0]);
                d["current_program_scene"] = json!(rest[0]);
            }
            // A removed top layer is no top layer.
            if d.get("ferrocast_top_layer").and_then(Value::as_str) == Some(name)
                && let Some(o) = d.as_object_mut()
            {
                o.remove("ferrocast_top_layer");
            }
        });
        self.set_order(&rest);
        Ok(())
    }

    pub fn move_scene(&self, name: &str, to: usize) -> Result<(), StudioError> {
        let mut order = self.order();
        let at =
            order.iter().position(|n| n == name).ok_or_else(|| fail(format!("There's no scene called \"{name}\".")))?;
        let s = order.remove(at);
        order.insert(to.min(order.len()), s);
        self.set_order(&order);
        Ok(())
    }

    /// Puts a scene on air, through the Studio's transition (the engine plays it).
    pub fn set_current(&self, name: &str) -> Result<(), StudioError> {
        self.scene_index(name)?;
        // The scene's own way in (its Transition Override), else the Studio's.
        let t = self.scene_transition_of(name)?.unwrap_or_else(|| self.transition());
        let was = self.current();
        let studio_mode = self.preview_scene().is_some();
        {
            let mut d = self.doc.borrow_mut();
            // Studio Mode's preview stays where it is (OBS saves it as current_scene).
            if !studio_mode {
                d["current_scene"] = json!(name);
            }
            d["current_program_scene"] = json!(name);
        }
        if studio_mode {
            // On air as a copy of its own (a scene picked straight for the program): edits in
            // the preview wait for the next transition.
            return self.send_copy_on_air(name, &t.kind, u64::from(t.ms));
        }
        if was != name
            && let Some(e) = self.engine.borrow().as_ref()
        {
            e.go_with(name, &t.kind, u64::from(t.ms), &self.transition_settings(&t.kind))?;
        }
        Ok(())
    }

    pub fn duplicate_scene(&self, name: &str, new_name: &str) -> Result<(), StudioError> {
        let items = self.scene_items(name)?;
        self.add_scene(new_name)?;
        self.with_source(new_name, |s| {
            s["settings"]["items"] = Value::Array(items.clone());
            s["settings"]["id_counter"] = json!(items.iter().filter_map(|i| i.get("id")?.as_i64()).max().unwrap_or(0));
        })?;
        let order = self.order();
        // Just after the one it copies.
        let at = order.iter().position(|n| n == name).map_or(order.len(), |i| i + 1);
        self.move_scene(new_name, at)
    }

    /// Renames a scene or a source, everywhere it's used.
    pub fn rename(&self, name: &str, new_name: &str) -> Result<(), StudioError> {
        if name == new_name {
            return Ok(());
        }
        self.source(name)?;
        self.name_free(new_name)?;
        let order: Vec<String> =
            self.order().into_iter().map(|n| if n == name { new_name.to_string() } else { n }).collect();
        self.edit(|d| {
            for k in ["current_scene", "current_program_scene", "ferrocast_top_layer"] {
                if d.get(k).and_then(Value::as_str) == Some(name) {
                    d[k] = json!(new_name);
                }
            }
            for k in CHANNEL_KEYS {
                if let Some(s) = d.get_mut(k).filter(|s| name_of(s) == name) {
                    s["name"] = json!(new_name);
                }
            }
            if let Some(l) = d["sources"].as_array_mut() {
                for s in l.iter_mut() {
                    if name_of(s) == name {
                        s["name"] = json!(new_name);
                    }
                    if let Some(items) = s.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
                        for i in items.iter_mut().filter(|i| name_of(i) == name) {
                            i["name"] = json!(new_name);
                        }
                    }
                }
            }
        });
        self.set_order(&order);
        // The vertical picture's list and scene on air, and transition overrides, by name too.
        self.edit(|d| {
            if let Some(v) = d.pointer_mut("/ferrocast/vertical") {
                if v.get("current_scene").and_then(Value::as_str) == Some(name) {
                    v["current_scene"] = json!(new_name);
                }
                for o in v.get_mut("scene_order").and_then(Value::as_array_mut).into_iter().flatten() {
                    if o.get("name").and_then(Value::as_str) == Some(name) {
                        o["name"] = json!(new_name);
                    }
                }
            }
            if let Some(t) = d.pointer_mut("/ferrocast/scene_transitions").and_then(Value::as_object_mut)
                && let Some(v) = t.remove(name)
            {
                t.insert(new_name.to_string(), v);
            }
            if d.pointer("/ferrocast/studio_mode").and_then(Value::as_str) == Some(name) {
                d["ferrocast"]["studio_mode"] = json!(new_name);
            }
            // Studio Mode's copy on air: its scene, and the sources it shows, by name.
            for k in ["program_copy", "program_prev"] {
                let Some(c) = d.pointer_mut(&format!("/ferrocast/{k}")) else { continue };
                if c.get("scene").and_then(Value::as_str) == Some(name) {
                    c["scene"] = json!(new_name);
                }
                for i in c.get_mut("items").and_then(Value::as_array_mut).into_iter().flatten() {
                    if name_of(i) == name {
                        i["name"] = json!(new_name);
                    }
                }
            }
        });
        Ok(())
    }

    // ------------------------------------------------------------------ items

    /// The source's size as drawn in a scene: its own size through its filters (a crop makes
    /// it smaller, a shadow or border bigger), as OBS sizes a source. The outline round it on
    /// the canvas and every move or resize use this (9 Nov: a cropped source kept the outline
    /// of its whole picture).
    fn size_of(&self, source: &str) -> (u32, u32) {
        let (w, h) = self.own_size_of(source);
        if w == 0 || h == 0 {
            return (w, h);
        }
        let s = self.source(source).unwrap_or(Value::Null);
        let filters = s.get("filters").and_then(Value::as_array).cloned().unwrap_or_default();
        filters
            .iter()
            .filter(|f| f.get("enabled").and_then(Value::as_bool) != Some(false))
            .filter_map(|f| {
                let set = with_defaults(f);
                ferrocore::obs::effect_from(kind_of(f), &set)
            })
            .fold((w, h), |(w, h), e| e.out_size(w, h))
    }

    /// The source's own picture size, before its filters, once our engine has one (else what
    /// its settings say).
    fn own_size_of(&self, source: &str) -> (u32, u32) {
        if let Some(e) = self.engine.borrow().as_ref()
            && let Some((_, Some(size))) = e.engine.source_named(source)
        {
            return size;
        }
        let s = self.source(source).unwrap_or(Value::Null);
        let set = s.get("settings").cloned().unwrap_or(Value::Null);
        let n = |k: &str| set.get(k).and_then(Value::as_u64).filter(|v| *v > 0).map(|v| v as u32);
        match kind_of(&s) {
            k if k.starts_with("color_source") => {
                (n("width").unwrap_or(self.canvas.width), n("height").unwrap_or(self.canvas.height))
            }
            "scene" => (self.canvas.width, self.canvas.height),
            "group" => (n("cx").unwrap_or(0), n("cy").unwrap_or(0)),
            // A copy is its source's size, through that one's filters when it brings them; it
            // isn't a source of its own in the engine. It was 0×0, so it couldn't be sized or
            // dragged by its handles (10 Nov). A copy of a copy is followed a few steps.
            "ferrocast_clone" | "source-clone" => {
                let mut seen = vec![source.to_string()];
                let mut cur = s.clone();
                loop {
                    let target = cur.pointer("/settings/clone").and_then(Value::as_str).unwrap_or_default().to_string();
                    if target.is_empty() || seen.contains(&target) || seen.len() > 8 {
                        return (0, 0);
                    }
                    let filters = cur.pointer("/settings/filters").and_then(Value::as_bool).unwrap_or(true);
                    let t = self.source(&target).unwrap_or(Value::Null);
                    if matches!(kind_of(&t), "ferrocast_clone" | "source-clone") {
                        seen.push(target);
                        cur = t;
                        continue;
                    }
                    return if filters { self.size_of(&target) } else { self.own_size_of(&target) };
                }
            }
            _ => (0, 0),
        }
    }

    fn info_of(&self, it: &Value) -> ItemInfo {
        let name = name_of(it).to_string();
        let src = self.source(&name).unwrap_or(Value::Null);
        let (w, h) = self.size_of(&name);
        let p = ferrocore::obs::placement_of(it);
        let a = ferrocore::draw_transform(&p, w, h);
        let (cw, ch) = ferrocore::cropped(w, h, p.crop);
        let (cw, ch) = (cw as f32, ch as f32);
        let kind = kind_of(&src).to_string();
        let media =
            matches!(kind.as_str(), "ffmpeg_source" | "vlc_source" | "slideshow" | "slideshow_v2" | "ferrocast_replay");
        let audio = src.get("mixers").and_then(Value::as_u64).is_some_and(|m| m > 0)
            && !matches!(
                kind.as_str(),
                "scene"
                    | "group"
                    | "image_source"
                    | "color_source_v3"
                    | "text_ft2_source_v2"
                    // A copy has no sound of its own: its source's is heard once.
                    | "ferrocast_clone"
                    | "source-clone"
                    | "ferrocast_countdown"
                    | "ferrocast_visualizer"
                    | "ferrocast_web_text"
                    | "ferrocast_inputs"
            );
        ItemInfo {
            id: it.get("id").and_then(Value::as_i64).unwrap_or(0),
            name,
            visible: it.get("visible").and_then(Value::as_bool).unwrap_or(true),
            locked: it.get("locked").and_then(Value::as_bool).unwrap_or(false),
            audio,
            group: kind == "group",
            blend: it.get("blend_type").and_then(Value::as_str).unwrap_or("normal").to_string(),
            scale_filter: match it.get("scale_filter").and_then(Value::as_str).unwrap_or("disable") {
                "disable" => "off".to_string(),
                f => f.to_string(),
            },
            srgb_off: it.get("blend_method").and_then(Value::as_str) == Some("srgb_off"),
            frame: [a.a * cw, a.b * cw, a.c * ch, a.d * ch, a.e, a.f],
            children: Vec::new(),
            show_transition: None,
            hide_transition: None,
            deinterlace: None,
            top_field_first: false,
            media,
            kind,
        }
    }

    /// A scene's items, top first (as OBS's Sources list shows them).
    pub fn items(&self, scene: &str) -> Result<Vec<ItemInfo>, StudioError> {
        Ok(self.scene_items(scene)?.iter().rev().map(|i| self.info_of(i)).collect())
    }

    pub fn item_ids(&self, scene: &str) -> Result<Vec<i64>, StudioError> {
        Ok(self.scene_items(scene)?.iter().rev().filter_map(|i| i.get("id")?.as_i64()).collect())
    }

    fn item(&self, scene: &str, id: i64) -> Result<Value, StudioError> {
        self.scene_items(scene)?
            .into_iter()
            .find(|i| i.get("id").and_then(Value::as_i64) == Some(id))
            .ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))
    }

    /// Changes a scene's item list.
    fn with_items<T>(
        &self,
        scene: &str,
        f: impl FnOnce(&mut Vec<Value>, &mut Map<String, Value>) -> T,
    ) -> Result<T, StudioError> {
        self.scene_items(scene)?;
        self.with_source(scene, |s| {
            let set = s["settings"].as_object_mut().map(std::mem::take).unwrap_or_default();
            let mut set = set;
            let mut items: Vec<Value> = set.remove("items").and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let out = f(&mut items, &mut set);
            set.insert("items".into(), Value::Array(items));
            s["settings"] = Value::Object(set);
            out
        })
    }

    fn with_item<T>(&self, scene: &str, id: i64, f: impl FnOnce(&mut Value) -> T) -> Result<T, StudioError> {
        self.with_items(scene, |items, _| {
            items.iter_mut().find(|i| i.get("id").and_then(Value::as_i64) == Some(id)).map(f)
        })?
        .ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))
    }

    /// Puts `source` (an existing one) at the top of `scene` → its item.
    fn place(&self, scene: &str, source: &Value) -> Result<ItemInfo, StudioError> {
        let item = self.with_items(scene, |items, set| {
            let id = set
                .get("id_counter")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(items.iter().filter_map(|i| i.get("id")?.as_i64()).max().unwrap_or(0))
                + 1;
            set.insert("id_counter".into(), json!(id));
            let it = new_item(source, id);
            items.push(it.clone());
            it
        })?;
        Ok(self.info_of(&item))
    }

    /// Makes a source and puts it at the top of `scene`; with `fit`, fitted to the screen.
    pub fn add_source(
        &self,
        scene: &str,
        kind: &str,
        name: &str,
        settings: &Value,
        fit: bool,
    ) -> Result<ItemInfo, StudioError> {
        self.scene_items(scene)?;
        self.name_free(name)?;
        // The kind's defaults, under what was given.
        let mut set = crate::kinds::defaults(kind).unwrap_or_else(|| json!({}));
        if let (Some(base), Some(given)) = (set.as_object_mut(), settings.as_object()) {
            for (k, v) in given {
                base.insert(k.clone(), v.clone());
            }
        }
        let source = json!({
            "id": kind, "versioned_id": kind, "name": name, "uuid": uuid(),
            "settings": set,
            "mixers": 255, "sync": 0, "flags": 0, "volume": 1.0, "balance": 0.5,
            "enabled": true, "muted": false, "monitoring_type": 0, "filters": [],
        });
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(source.clone())));
        let mut info = self.place(scene, &source)?;
        if fit {
            self.edit_item(scene, info.id, ItemEdit::Fit)?;
            info = self.info_of(&self.item(scene, info.id)?);
        }
        Ok(info)
    }

    /// Shows a source already made (in another scene) in `scene` too.
    pub fn add_existing(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let source = self.source(name)?;
        if name == scene {
            return Err(fail("A scene can't show itself."));
        }
        self.place(scene, &source)
    }

    pub fn transform(&self, scene: &str, id: i64) -> Result<Transform, StudioError> {
        let it = self.item(scene, id)?;
        let p = ferrocore::obs::placement_of(&it);
        let (w, h) = self.size_of(name_of(&it));
        Ok(Transform {
            x: p.x,
            y: p.y,
            rotation: p.rotation,
            scale_x: p.scale_x,
            scale_y: p.scale_y,
            alignment: p.alignment,
            bounds: BOUNDS[it.get("bounds_type").and_then(Value::as_u64).unwrap_or(0).min(6) as usize].to_string(),
            bounds_alignment: p.bounds_alignment,
            bounds_width: p.bounds_width,
            bounds_height: p.bounds_height,
            crop_left: p.crop[0] as i32,
            crop_top: p.crop[1] as i32,
            crop_right: p.crop[2] as i32,
            crop_bottom: p.crop[3] as i32,
            width: w,
            height: h,
        })
    }

    pub fn set_transform(&self, scene: &str, id: i64, t: &Transform) -> Result<ItemInfo, StudioError> {
        let bounds =
            BOUNDS.iter().position(|b| *b == t.bounds).ok_or_else(|| fail(format!("bounds: one of {BOUNDS:?}.")))?;
        // A drag sends many of these a second: the moved item goes straight to the engine (the
        // picture and its outline move in the same frame), instead of the whole collection
        // being opened again for each step. Anything it can't find takes the long way.
        self.quiet.set(true);
        let it = self.with_item(scene, id, |i| {
            i["pos"] = json!({ "x": t.x, "y": t.y });
            i["rot"] = json!(t.rotation);
            i["scale"] = json!({ "x": t.scale_x, "y": t.scale_y });
            i["align"] = json!(t.alignment);
            i["bounds_type"] = json!(bounds);
            i["bounds_align"] = json!(t.bounds_alignment);
            i["bounds"] = json!({ "x": t.bounds_width, "y": t.bounds_height });
            for (k, v) in [
                ("crop_left", t.crop_left),
                ("crop_top", t.crop_top),
                ("crop_right", t.crop_right),
                ("crop_bottom", t.crop_bottom),
            ] {
                i[k] = json!(v.max(0));
            }
            i.clone()
        });
        self.quiet.set(false);
        let it = it?;
        let moved = self
            .engine
            .borrow()
            .as_ref()
            .is_some_and(|e| e.engine.set_item_placement(scene, id, ferrocore::obs::placement_of(&it)));
        if !moved {
            if let Some(e) = self.engine.borrow().as_ref() {
                e.reopen(&self.doc.borrow());
            }
            self.sync_pages();
        }
        Ok(self.info_of(&it))
    }

    /// Moves and sizes an item so its picture fills `x, y, w, h` on the canvas (a drag in the
    /// preview), keeping its turn and crop.
    pub fn set_item_box(&self, scene: &str, id: i64, x: f32, y: f32, w: f32, h: f32) -> Result<ItemInfo, StudioError> {
        let mut t = self.transform(scene, id)?;
        let (cw, ch) = ferrocore::cropped(
            t.width,
            t.height,
            [t.crop_left, t.crop_top, t.crop_right, t.crop_bottom].map(|c| c.max(0) as u32),
        );
        if t.bounds != "none" {
            (t.bounds_width, t.bounds_height) = (w, h);
        } else if cw > 0 && ch > 0 {
            (t.scale_x, t.scale_y) = (w / cw as f32 * t.scale_x.signum(), h / ch as f32 * t.scale_y.signum());
        }
        // The box's top left, whatever the item is aligned by.
        t.alignment = 5;
        (t.x, t.y) = (x, y);
        self.set_transform(scene, id, &t)
    }

    pub fn set_item_crop(&self, scene: &str, id: i64, crop: [i32; 4]) -> Result<ItemInfo, StudioError> {
        let mut t = self.transform(scene, id)?;
        [t.crop_left, t.crop_top, t.crop_right, t.crop_bottom] = crop.map(|c| c.max(0));
        self.set_transform(scene, id, &t)
    }

    /// Where an item is in its scene (0 is the top) and the scene's name.
    pub fn item_place(&self, scene: &str, id: i64) -> Result<(usize, String), StudioError> {
        let ids = self.item_ids(scene)?;
        let at = ids.iter().position(|i| *i == id).ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))?;
        Ok((at, scene.to_string()))
    }

    /// Moves an item to place `to` (0 is the top).
    pub fn item_order(&self, scene: &str, id: i64, to: usize) -> Result<(), StudioError> {
        self.with_items(scene, |items, _| {
            let Some(at) = items.iter().position(|i| i.get("id").and_then(Value::as_i64) == Some(id)) else { return };
            let it = items.remove(at);
            // Saved bottom first; `to` counts from the top.
            let to = items.len().saturating_sub(to.min(items.len()));
            items.insert(to, it);
        })
    }

    pub fn edit_item(&self, scene: &str, id: i64, edit: ItemEdit) -> Result<(), StudioError> {
        use ItemEdit as E;
        let (cw, ch) = (self.canvas.width as f32, self.canvas.height as f32);
        let set = |k: &'static str, v: Value| self.with_item(scene, id, move |i| i[k] = v).map(drop);
        match edit {
            E::Show => set("visible", json!(true)),
            E::Hide => set("visible", json!(false)),
            E::Lock => set("locked", json!(true)),
            E::Unlock => set("locked", json!(false)),
            E::Remove => {
                self.item(scene, id)?;
                // Out of the scene, and the source with it when nothing shows it any more: one
                // change, so the engine closes it at once.
                self.edit(|d| {
                    let found = d["sources"].as_array_mut().and_then(|l| l.iter_mut().find(|s| name_of(s) == scene));
                    if let Some(items) =
                        found.and_then(|s| s.pointer_mut("/settings/items")).and_then(Value::as_array_mut)
                    {
                        items.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id));
                    }
                    drop_unused(d);
                });
                Ok(())
            }
            E::Up | E::Down | E::Top | E::Bottom => {
                let (at, _) = self.item_place(scene, id)?;
                let last = self.item_ids(scene)?.len().saturating_sub(1);
                let to = match edit {
                    E::Up => at.saturating_sub(1),
                    E::Down => (at + 1).min(last),
                    E::Top => 0,
                    _ => last,
                };
                self.item_order(scene, id, to)
            }
            E::Fit | E::Stretch => {
                let bounds = if edit == E::Fit { "fit" } else { "stretch" };
                let mut t = self.transform(scene, id)?;
                (t.x, t.y, t.alignment, t.rotation, t.scale_x, t.scale_y) = (0.0, 0.0, 5, 0.0, 1.0, 1.0);
                (t.bounds, t.bounds_alignment, t.bounds_width, t.bounds_height) = (bounds.into(), 0, cw, ch);
                self.set_transform(scene, id, &t).map(drop)
            }
            E::Center => {
                let info = self.info_of(&self.item(scene, id)?);
                let f = info.frame;
                // The box's middle onto the canvas's middle.
                let (mx, my) = (f[4] + (f[0] + f[2]) / 2.0, f[5] + (f[1] + f[3]) / 2.0);
                let mut t = self.transform(scene, id)?;
                (t.x, t.y) = (t.x + cw / 2.0 - mx, t.y + ch / 2.0 - my);
                self.set_transform(scene, id, &t).map(drop)
            }
            E::Reset => {
                let mut t = self.transform(scene, id)?;
                (t.x, t.y, t.alignment, t.rotation, t.scale_x, t.scale_y) = (0.0, 0.0, 5, 0.0, 1.0, 1.0);
                (t.bounds, t.bounds_width, t.bounds_height) = ("none".into(), 0.0, 0.0);
                (t.crop_left, t.crop_top, t.crop_right, t.crop_bottom) = (0, 0, 0, 0);
                self.set_transform(scene, id, &t).map(drop)
            }
            E::FlipH | E::FlipV => {
                let mut t = self.transform(scene, id)?;
                if edit == E::FlipH {
                    t.scale_x = -t.scale_x;
                } else {
                    t.scale_y = -t.scale_y;
                }
                self.set_transform(scene, id, &t).map(drop)
            }
            E::RotateCw | E::RotateCcw | E::Rotate180 => {
                let by = match edit {
                    E::RotateCw => 90.0,
                    E::RotateCcw => -90.0,
                    _ => 180.0,
                };
                let mut t = self.transform(scene, id)?;
                t.rotation = (t.rotation + by).rem_euclid(360.0);
                self.set_transform(scene, id, &t).map(drop)
            }
            E::Duplicate => {
                let it = self.item(scene, id)?;
                let src = self.source(name_of(&it))?;
                let mut n = 2;
                let copy = loop {
                    let c = format!("{} {n}", name_of(&src));
                    if self.name_free(&c).is_ok() {
                        break c;
                    }
                    n += 1;
                };
                let mut s = src.clone();
                s["name"] = json!(copy);
                s["uuid"] = json!(uuid());
                self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(s.clone())));
                let new = self.place(scene, &s)?;
                // Where the original is, just above it.
                self.with_item(scene, new.id, |i| {
                    for k in [
                        "pos",
                        "rot",
                        "scale",
                        "align",
                        "bounds_type",
                        "bounds_align",
                        "bounds",
                        "crop_left",
                        "crop_top",
                        "crop_right",
                        "crop_bottom",
                        "scale_filter",
                        "blend_type",
                        "blend_method",
                    ] {
                        if let Some(v) = it.get(k) {
                            i[k] = v.clone();
                        }
                    }
                })?;
                let (at, _) = self.item_place(scene, id)?;
                self.item_order(scene, new.id, at)
            }
            E::BlendNormal
            | E::BlendAdditive
            | E::BlendSubtract
            | E::BlendScreen
            | E::BlendMultiply
            | E::BlendLighten
            | E::BlendDarken => {
                let i = [
                    E::BlendNormal,
                    E::BlendAdditive,
                    E::BlendSubtract,
                    E::BlendScreen,
                    E::BlendMultiply,
                    E::BlendLighten,
                    E::BlendDarken,
                ]
                .iter()
                .position(|b| *b == edit)
                .unwrap_or(0);
                set("blend_type", json!(BLENDS[i]))
            }
            E::BlendMethodDefault => set("blend_method", json!("default")),
            E::BlendMethodSrgbOff => set("blend_method", json!("srgb_off")),
            E::ScaleOff | E::ScalePoint | E::ScaleBicubic | E::ScaleBilinear | E::ScaleLanczos | E::ScaleArea => {
                let i = [E::ScaleOff, E::ScalePoint, E::ScaleBicubic, E::ScaleBilinear, E::ScaleLanczos, E::ScaleArea]
                    .iter()
                    .position(|b| *b == edit)
                    .unwrap_or(0);
                set("scale_filter", json!(if i == 0 { "disable" } else { SCALE_FILTERS[i] }))
            }
        }
    }

    // ------------------------------------------------------------------ sources

    /// Every source and the scenes it's in.
    pub fn sources(&self) -> Vec<SourceUse> {
        let all = self.sources_list();
        all.iter()
            .filter(|s| !is_scene(s))
            .map(|s| {
                let name = name_of(s).to_string();
                let scenes = all
                    .iter()
                    .filter(|sc| is_scene(sc))
                    .filter(|sc| {
                        sc.pointer("/settings/items")
                            .and_then(Value::as_array)
                            .is_some_and(|i| i.iter().any(|i| name_of(i) == name))
                    })
                    .map(|sc| name_of(sc).to_string())
                    .collect();
                SourceUse { kind: kind_of(s).to_string(), name, scenes }
            })
            .collect()
    }

    pub fn settings(&self, name: &str) -> Result<Value, StudioError> {
        Ok(with_defaults(&self.source(name)?))
    }

    /// A source's kind ("dshow_input").
    pub fn kind(&self, name: &str) -> Result<String, StudioError> {
        Ok(kind_of(&self.source(name)?).to_string())
    }

    /// A source's settings form (its kind's, from our engine's kinds).
    pub fn options(&self, name: &str) -> Result<Vec<crate::OptionInfo>, StudioError> {
        let s = self.source(name)?;
        let mut opts = crate::kinds::options(kind_of(&s))
            .ok_or_else(|| fail(format!("Ferrocast's own engine can't show \"{name}\" yet.")))?;
        self.fill_choices(name, &mut opts)?;
        Ok(opts)
    }

    /// The choices a form takes from this collection, put into `opts` (a kind's form read on
    /// its own has them empty): a clone's sources and scenes, the sounds sound bars can listen
    /// to. The app reads a kind's form off the main thread (devices) and fills these after.
    pub fn fill_choices(&self, name: &str, opts: &mut [crate::OptionInfo]) -> Result<(), StudioError> {
        let s = self.source(name)?;
        // Sound bars listen to the sound on air or a source with sound in this collection.
        if kind_of(&s) == "ferrocast_visualizer"
            && let Some(o) = opts.iter_mut().find(|o| o.key == "source")
        {
            let all = self.sources_list();
            let heard: Vec<String> = all
                .iter()
                .filter(|x| name_of(x) != name && !is_scene(x))
                .filter(|x| x.get("mixers").and_then(Value::as_u64).is_some_and(|m| m > 0))
                .filter(|x| {
                    // Pictures and words never make sound, whatever their mixer flags say.
                    !matches!(
                        kind_of(x),
                        "image_source"
                            | "color_source_v3"
                            | "text_ft2_source_v2"
                            | "ferrocast_clone"
                            | "ferrocast_countdown"
                            | "ferrocast_visualizer"
                            | "ferrocast_web_text"
                            | "ferrocast_inputs"
                            | "slideshow"
                            | "slideshow_v2"
                            | "image_slide_show"
                    )
                })
                .map(|x| name_of(x).to_string())
                .collect();
            o.choices = std::iter::once("Sound on air".to_string()).chain(heard.iter().cloned()).collect();
            o.values = std::iter::once(json!("")).chain(heard.into_iter().map(Value::from)).collect();
        }
        // A clone picks from this collection: every other source, then the scenes.
        if kind_of(&s) == "ferrocast_clone"
            && let Some(o) = opts.iter_mut().find(|o| o.key == "clone")
        {
            let all = self.sources_list();
            let (scenes, others): (Vec<&Value>, Vec<&Value>) =
                all.iter().filter(|x| name_of(x) != name).partition(|x| is_scene(x));
            let names: Vec<String> = others.iter().chain(&scenes).map(|x| name_of(x).to_string()).collect();
            o.choices = names.clone();
            o.values = names.into_iter().map(Value::from).collect();
        }
        Ok(())
    }

    /// Changes some of a source's settings (the rest kept).
    pub fn update(&self, name: &str, settings: &Value) -> Result<(), StudioError> {
        let Some(new) = settings.as_object() else { return Err(fail("settings: an object.")) };
        self.with_source(name, |s| {
            if !s["settings"].is_object() {
                s["settings"] = json!({});
            }
            for (k, v) in new {
                s["settings"][k] = v.clone();
            }
        })
    }

    // ------------------------------------------------------------------ sound

    fn audio_info(&self, s: &Value, channel: Option<u32>) -> AudioInfo {
        let mixers = s.get("mixers").and_then(Value::as_u64).unwrap_or(1);
        AudioInfo {
            name: name_of(s).to_string(),
            kind: kind_of(s).to_string(),
            channel,
            volume_db: db_of(s.get("volume").and_then(Value::as_f64).unwrap_or(1.0)),
            muted: s.get("muted").and_then(Value::as_bool).unwrap_or(false),
            monitor: match s.get("monitoring_type").and_then(Value::as_u64) {
                Some(1) => Monitor::OnlyMe,
                Some(2) => Monitor::MeAndStream,
                _ => Monitor::Off,
            },
            balance: s.get("balance").and_then(Value::as_f64).unwrap_or(0.5) as f32,
            sync_ms: s.get("sync").and_then(Value::as_i64).unwrap_or(0) / 1_000_000,
            mono: s.get("flags").and_then(Value::as_u64).unwrap_or(0) & 1 != 0,
            tracks: (1..=6u8).filter(|t| mixers & (1 << (t - 1)) != 0).collect(),
            // Making sound: our engine has it running (a device opened, a file playing).
            active: self.engine.borrow().as_ref().is_some_and(|e| e.engine.source_named(name_of(s)).is_some()),
        }
    }

    pub fn audio_of(&self, name: &str) -> Result<AudioInfo, StudioError> {
        let ch = {
            let d = self.doc.borrow();
            CHANNEL_KEYS.iter().position(|k| d.get(*k).is_some_and(|s| name_of(s) == name)).map(|i| i as u32 + 1)
        };
        Ok(self.audio_info(&self.source(name)?, ch))
    }

    /// The global channels in use (Desktop Audio, Mic/Aux).
    pub fn channels(&self) -> Vec<AudioInfo> {
        let d = self.doc.borrow().clone();
        CHANNEL_KEYS
            .iter()
            .enumerate()
            .filter_map(|(i, k)| d.get(*k).map(|s| self.audio_info(s, Some(i as u32 + 1))))
            .collect()
    }

    pub fn set_volume(&self, name: &str, volume_db: f32) -> Result<(), StudioError> {
        self.with_source(name, |s| s["volume"] = json!(mul_of(volume_db.clamp(-100.0, 26.0))))
    }

    pub fn set_muted(&self, name: &str, muted: bool) -> Result<(), StudioError> {
        self.with_source(name, |s| s["muted"] = json!(muted))
    }

    pub fn set_balance(&self, name: &str, balance: f32) -> Result<(), StudioError> {
        self.with_source(name, |s| s["balance"] = json!(balance.clamp(0.0, 1.0)))
    }

    pub fn set_sync(&self, name: &str, ms: i64) -> Result<(), StudioError> {
        self.with_source(name, |s| s["sync"] = json!(ms.clamp(-950, 20_000) * 1_000_000))
    }

    pub fn set_mono(&self, name: &str, mono: bool) -> Result<(), StudioError> {
        self.with_source(name, |s| {
            let f = s.get("flags").and_then(Value::as_u64).unwrap_or(0);
            s["flags"] = json!(if mono { f | 1 } else { f & !1 });
        })
    }

    pub fn set_tracks(&self, name: &str, tracks: &[u8]) -> Result<(), StudioError> {
        if tracks.iter().any(|t| !(1..=6).contains(t)) {
            return Err(fail("tracks: numbers 1 to 6."));
        }
        let mixers: u64 = tracks.iter().map(|t| 1u64 << (t - 1)).sum();
        self.with_source(name, |s| s["mixers"] = json!(mixers))
    }

    pub fn set_monitor(&self, name: &str, m: Monitor) -> Result<(), StudioError> {
        let v = match m {
            Monitor::Off => 0,
            Monitor::OnlyMe => 1,
            Monitor::MeAndStream => 2,
        };
        self.with_source(name, |s| s["monitoring_type"] = json!(v))
    }

    // ------------------------------------------------------------------ filters

    fn filter_list(&self, name: &str) -> Result<Vec<Value>, StudioError> {
        Ok(self.source(name)?.get("filters").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub fn filters(&self, name: &str) -> Result<Vec<FilterInfo>, StudioError> {
        Ok(self
            .filter_list(name)?
            .iter()
            .map(|f| FilterInfo {
                name: name_of(f).to_string(),
                kind: kind_of(f).to_string(),
                enabled: f.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            })
            .collect())
    }

    fn with_filters<T>(&self, name: &str, f: impl FnOnce(&mut Vec<Value>) -> T) -> Result<T, StudioError> {
        self.with_source(name, |s| {
            let mut list = s.get("filters").and_then(Value::as_array).cloned().unwrap_or_default();
            let out = f(&mut list);
            s["filters"] = Value::Array(list);
            out
        })
    }

    pub fn add_filter(&self, name: &str, kind: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        check_name(filter)?;
        if self.filter_list(name)?.iter().any(|f| name_of(f) == filter) {
            return Err(fail(format!("\"{name}\" already has a filter called \"{filter}\".")));
        }
        let mut set = crate::kinds::defaults(kind).unwrap_or_else(|| json!({}));
        if let (Some(base), Some(given)) = (set.as_object_mut(), settings.as_object()) {
            for (k, v) in given {
                base.insert(k.clone(), v.clone());
            }
        }
        let f = json!({
            "id": kind, "versioned_id": kind, "name": filter, "uuid": uuid(), "enabled": true,
            "settings": set,
        });
        self.with_filters(name, |l| l.push(f))
    }

    pub fn remove_filter(&self, name: &str, filter: &str) -> Result<(), StudioError> {
        let gone = self.with_filters(name, |l| {
            let n = l.len();
            l.retain(|f| name_of(f) != filter);
            n != l.len()
        })?;
        if gone { Ok(()) } else { Err(fail(format!("\"{name}\" has no filter called \"{filter}\"."))) }
    }

    pub fn set_filter_enabled(&self, name: &str, filter: &str, on: bool) -> Result<(), StudioError> {
        self.with_filter(name, filter, |f| f["enabled"] = json!(on))
    }

    pub fn filter_settings(&self, name: &str, filter: &str) -> Result<Value, StudioError> {
        self.filter_list(name)?
            .into_iter()
            .find(|f| name_of(f) == filter)
            .map(|f| with_defaults(&f))
            .ok_or_else(|| fail(format!("\"{name}\" has no filter called \"{filter}\".")))
    }

    /// A filter's settings form.
    pub fn filter_options(&self, name: &str, filter: &str) -> Result<Vec<crate::OptionInfo>, StudioError> {
        let f = self
            .filter_list(name)?
            .into_iter()
            .find(|f| name_of(f) == filter)
            .ok_or_else(|| fail(format!("\"{name}\" has no filter called \"{filter}\".")))?;
        crate::kinds::options(kind_of(&f))
            .ok_or_else(|| fail(format!("Ferrocast's own engine doesn't run \"{filter}\" yet.")))
    }

    pub fn update_filter(&self, name: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        let Some(new) = settings.as_object() else { return Err(fail("settings: an object.")) };
        self.with_filter(name, filter, |f| {
            if !f["settings"].is_object() {
                f["settings"] = json!({});
            }
            for (k, v) in new {
                f["settings"][k] = v.clone();
            }
        })
    }

    /// Moves a filter up or down its list (`Up`, `Down`, `Top`, `Bottom`).
    pub fn move_filter(&self, name: &str, filter: &str, edit: ItemEdit) -> Result<(), StudioError> {
        self.with_filters(name, |l| {
            let Some(at) = l.iter().position(|f| name_of(f) == filter) else { return };
            let f = l.remove(at);
            let to = match edit {
                ItemEdit::Up => at.saturating_sub(1),
                ItemEdit::Down => (at + 1).min(l.len()),
                ItemEdit::Top => 0,
                ItemEdit::Bottom => l.len(),
                _ => at,
            };
            l.insert(to, f);
        })
    }

    /// Copies `from`'s filters onto `name` (OBS's Paste Filters), each its own copy.
    pub fn paste_filters(&self, name: &str, from: &str) -> Result<(), StudioError> {
        let copies = self.filter_list(from)?;
        let have: Vec<String> = self.filter_list(name)?.iter().map(|f| name_of(f).to_string()).collect();
        self.with_filters(name, |l| {
            for mut f in copies {
                let mut n = name_of(&f).to_string();
                while have.contains(&n) || l.iter().any(|g| name_of(g) == n) {
                    n.push_str(" 2");
                }
                f["name"] = json!(n);
                f["uuid"] = json!(uuid());
                l.push(f);
            }
        })
    }

    fn with_filter(&self, name: &str, filter: &str, f: impl FnOnce(&mut Value)) -> Result<(), StudioError> {
        let found = self.with_filters(name, |l| l.iter_mut().find(|x| name_of(x) == filter).map(f).is_some())?;
        if found { Ok(()) } else { Err(fail(format!("\"{name}\" has no filter called \"{filter}\"."))) }
    }

    // ------------------------------------------------------------------ pictures

    /// A small picture of a scene as it looks now (`width` pixels wide, the canvas's shape), as
    /// a PNG data URL: the Scenes grid's tiles.
    pub fn thumbnail(&self, scene: &str, width: u32) -> Result<String, StudioError> {
        let (px, w, h) = self.thumbnail_pixels(scene, width)?;
        crate::collection::png_data_url(&px, w, h, scene)
    }

    /// One source's own picture now, before its filters, `width` wide as RGBA pixels, with its
    /// height and the source's real size (the crop editor draws on it); None until it has one.
    #[allow(clippy::type_complexity)]
    pub fn source_picture_pixels(
        &self,
        name: &str,
        width: u32,
    ) -> Result<Option<(Vec<u8>, u32, u32, (u32, u32))>, StudioError> {
        self.source(name)?;
        let e = self.engine()?;
        let Some((id, Some((sw, sh)))) = e.engine.source_named(name) else { return Ok(None) };
        if sw == 0 || sh == 0 {
            return Ok(None);
        }
        // `width` wide at the source's own shape; a tall one (a phone camera) is kept to 640
        // high, narrower to match, so it's never squashed.
        let mut w = u64::from(width.clamp(64, 640).min(sw.max(64)));
        let mut h = (w * u64::from(sh) / u64::from(sw)).max(1);
        if h > 640 {
            w = (640 * u64::from(sw) / u64::from(sh)).max(1);
            h = 640;
        }
        let (w, h) = (w as u32, h as u32);
        Ok(e.engine.snapshot_source_at(id, (w, h)).map(|px| (px, w, h, (sw, sh))))
    }

    /// A scene's picture `width` wide as RGBA pixels (with its height): the drawing, which needs
    /// this thread; turning it into a PNG ([`png_data_url`]) can happen elsewhere.
    pub fn thumbnail_pixels(&self, scene: &str, width: u32) -> Result<(Vec<u8>, u32, u32), StudioError> {
        self.scene_index(scene)?;
        let (cw, ch) = (self.canvas.width.max(1), self.canvas.height.max(1));
        let w = width.clamp(64, 640);
        let h = ((u64::from(w) * u64::from(ch)) / u64::from(cw)).max(1) as u32;
        let px = self
            .engine()?
            .engine
            .snapshot_scene_at(scene, (cw, ch), (w, h))
            .map_err(|e| fail(format!("\"{scene}\" couldn't be drawn: {e}")))?;
        Ok((px, w, h))
    }

    // ------------------------------------------------------------------ outputs

    /// Where a recording of `plan` goes now: its folder and OBS's name pattern, with " (2)"…
    /// added when the name's taken (unless it may be overwritten).
    pub fn record_path(&self, plan: &RecordPlan) -> PathBuf {
        path_for(self.canvas, plan)
    }

    fn output(&self, how: own::Drawing<'_>) -> Result<own::OwnStream, StudioError> {
        let doc = self.doc.borrow().clone();
        own::OwnStream::drawn(self.engine()?, &doc, how)
    }

    /// Streams what's on air over RTMP to `server` with `key`.
    pub fn stream(&self, server: &str, key: &str, s: &StreamSettings) -> Result<StageOutput, StudioError> {
        if let Some(why) = own::unsupported(s, true) {
            return Err(fail(format!("Ferrocast's own engine doesn't do {why} yet.")));
        }
        // Track 1 to viewers; Twitch's VOD track after it (sent as Enhanced RTMP multitrack audio).
        let tracks: Vec<u8> = std::iter::once(1).chain(s.vod_track).collect();
        let how = own::Drawing {
            send_to: Some(own::Send { server, key, reconnect: s.reconnect }),
            record: None,
            encode: &s.encode,
            tracks: &tracks,
            delay_s: s.delay_s,
            vertical: false,
            replay: None,
            solo: None,
        };
        Ok(StageOutput::new(self.output(how)?, None, self.canvas))
    }

    /// Records what's on air (or `solo`, one scene or source on its own) to a file named as
    /// `plan` says → the recording; `StageOutput::path` says where.
    pub fn record(&self, plan: &RecordPlan, solo: Option<&str>) -> Result<StageOutput, StudioError> {
        std::fs::create_dir_all(&plan.folder)
            .map_err(|e| fail(format!("The recordings folder {} can't be made: {e}", plan.folder.display())))?;
        self.record_at(&self.record_path(plan), plan, solo)
    }

    /// The same, to `path` (named already).
    pub fn record_at(&self, path: &Path, plan: &RecordPlan, solo: Option<&str>) -> Result<StageOutput, StudioError> {
        if let Some(why) = own::unsupported_recording(plan) {
            return Err(fail(format!("Ferrocast's own engine doesn't do {why} yet.")));
        }
        let path = path.to_path_buf();
        let how = own::Drawing {
            send_to: None,
            record: Some(&path),
            encode: &plan.encode,
            tracks: &plan.tracks,
            delay_s: 0,
            vertical: false,
            replay: None,
            solo,
        };
        let mut own = self.output(how)?;
        own.set_plan(plan);
        Ok(StageOutput::new(own, Some(path), self.canvas))
    }

    /// `Collection`'s names for the same, so the app calls either.
    pub fn stream_drawn(&self, server: &str, key: &str, s: &StreamSettings) -> Result<StageOutput, StudioError> {
        self.stream(server, key, s)
    }

    pub fn vertical_stream_drawn(
        &self,
        server: &str,
        key: &str,
        s: &StreamSettings,
    ) -> Result<StageOutput, StudioError> {
        self.stream_vertical(server, key, s)
    }

    pub fn record_drawn(&self, path: &Path, plan: &RecordPlan) -> Result<StageOutput, StudioError> {
        self.record_at(path, plan, None)
    }

    pub fn record_source_drawn(&self, name: &str, path: &Path, plan: &RecordPlan) -> Result<StageOutput, StudioError> {
        self.record_at(path, plan, Some(name))
    }

    pub fn replay_drawn(&self, plan: &ReplayPlan) -> Result<StageOutput, StudioError> {
        self.replay(plan)
    }

    /// OBS's replay buffer: the last `plan.seconds` kept, saved on request (`save_replay`).
    pub fn replay(&self, plan: &ReplayPlan) -> Result<StageOutput, StudioError> {
        if !(5..=21_600).contains(&plan.seconds) {
            return Err(fail("seconds: 5 to 21600."));
        }
        if !matches!(plan.format.as_str(), "mkv" | "mp4") {
            return Err(fail(
                "Ferrocast's own engine saves replays as MKV or MP4: pick one of those in Settings › Recording.",
            ));
        }
        let how = own::Drawing {
            send_to: None,
            record: None,
            encode: &plan.encode,
            tracks: &plan.tracks,
            delay_s: 0,
            vertical: false,
            replay: Some((plan.seconds, plan.mb)),
            solo: None,
        };
        let mut out = StageOutput::new(self.output(how)?, None, self.canvas);
        out.replay = Some((plan.clone(), Arc::default()));
        Ok(out)
    }

    // ------------------------------------------------------------------ undo

    /// The whole collection as it is now, to put back with `restore_all` (undo and redo).
    pub fn snapshot(&self) -> Value {
        self.doc.borrow().clone()
    }

    /// Puts the collection back as `snapshot` had it; the engine follows (unchanged sources keep
    /// running).
    pub fn restore_all(&self, snapshot: &Value) {
        let on_air = self.current();
        self.edit(|d| {
            *d = snapshot.clone();
            // Undo changes the scenes, not what's on air.
            if d.get("sources").and_then(Value::as_array).is_some_and(|l| l.iter().any(|s| name_of(s) == on_air)) {
                d["current_program_scene"] = json!(on_air);
            }
        });
    }

    // ------------------------------------------------------------------ Studio Mode

    /// Studio Mode's preview scene (what goes on air next), or None when it's off.
    pub fn preview_scene(&self) -> Option<String> {
        self.doc.borrow().pointer("/ferrocast/studio_mode").and_then(Value::as_str).map(String::from)
    }

    /// Turns Studio Mode on (the preview starts on what's on air) or off.
    ///
    /// On, what's on air is a copy of its scene's layout (OBS's "Duplicate Scene", on by
    /// default): the items' places, sizes, order and visibility, showing the same sources. The
    /// preview edits the scene itself; the copy changes only at a transition (owner, 11 Nov:
    /// resizing in the preview moved it on air at once).
    pub fn set_studio_mode(&self, on: bool) {
        let now = self.current();
        {
            let mut d = self.doc.borrow_mut();
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"]["studio_mode"] = if on { json!(now) } else { Value::Null };
            d["current_scene"] = json!(now);
            if let Some(f) = d["ferrocast"].as_object_mut() {
                for k in ["program_copy", "program_prev", "program_on_air"] {
                    f.remove(k);
                }
            }
        }
        if on {
            let items = self.scene_items(&now).unwrap_or_default();
            let mut d = self.doc.borrow_mut();
            d["ferrocast"]["program_copy"] = json!({ "scene": now, "items": items, "private": PROGRAM_COPY[0] });
            d["ferrocast"]["program_on_air"] = json!(PROGRAM_COPY[0]);
        }
        self.refresh();
    }

    /// Studio Mode: `scene` on air as a new copy of its layout, through a transition from the
    /// copy on air now (both are in the engine while it plays; the old one goes after).
    fn send_copy_on_air(&self, scene: &str, kind: &str, ms: u64) -> Result<(), StudioError> {
        let items = self.scene_items(scene)?;
        let prev = self.doc.borrow().pointer("/ferrocast/program_copy").cloned();
        let prev_private = prev.as_ref().and_then(|p| p.get("private")).and_then(Value::as_str).map(String::from);
        let private = if prev_private.as_deref() == Some(PROGRAM_COPY[0]) { PROGRAM_COPY[1] } else { PROGRAM_COPY[0] };
        {
            let mut d = self.doc.borrow_mut();
            d["current_program_scene"] = json!(scene);
            d["ferrocast"]["program_copy"] = json!({ "scene": scene, "items": items, "private": private });
            match (prev, &prev_private) {
                (Some(p), Some(on)) => {
                    d["ferrocast"]["program_prev"] = p;
                    d["ferrocast"]["program_on_air"] = json!(on);
                }
                _ => {
                    if let Some(f) = d["ferrocast"].as_object_mut() {
                        f.remove("program_prev");
                        f.remove("program_on_air");
                    }
                }
            }
            drop_unused(&mut d);
        }
        // The engine with both copies, the old one still on air; then the transition.
        self.refresh();
        let settings = self.transition_settings(kind);
        if let Some(e) = self.engine.borrow().as_ref() {
            e.go_with(private, kind, ms, &settings)?;
        }
        let mut d = self.doc.borrow_mut();
        d["ferrocast"]["program_on_air"] = json!(private);
        if let Some(f) = d["ferrocast"].as_object_mut() {
            f.remove("program_prev");
        }
        Ok(())
    }

    /// Picks the scene Studio Mode's preview shows.
    pub fn set_preview(&self, name: &str) -> Result<(), StudioError> {
        self.scene_index(name)?;
        if self.preview_scene().is_none() {
            return Err(fail("Studio Mode is off."));
        }
        let mut d = self.doc.borrow_mut();
        d["ferrocast"]["studio_mode"] = json!(name);
        // OBS saves Studio Mode's preview as `current_scene`.
        d["current_scene"] = json!(name);
        Ok(())
    }

    /// Sends the preview on air (OBS's Transition button), through `quick` or the Studio's
    /// transition; the preview then shows what was on air (OBS's "swap").
    pub fn transition_now(&self, quick: Option<&TransitionInfo>) -> Result<(), StudioError> {
        let pre = self.preview_scene().ok_or_else(|| fail("Studio Mode is off."))?;
        let old = self.current();
        // The same scene too: what was edited in the preview goes on air (OBS, with Duplicate
        // Scene on).
        match quick {
            Some(t) => {
                if t.ms > 20_000 {
                    return Err(fail("ms: a transition lasts 0 to 20000 ms."));
                }
                self.send_copy_on_air(&pre, &t.kind, u64::from(t.ms))?;
            }
            None => self.set_current(&pre)?,
        }
        let mut d = self.doc.borrow_mut();
        d["ferrocast"]["studio_mode"] = json!(old);
        d["current_scene"] = json!(old);
        Ok(())
    }

    // ------------------------------------------------------------------ groups

    /// Puts items `ids` of `scene` into a new group `name`, each staying where it shows → the
    /// group's item.
    pub fn group_together(&self, scene: &str, ids: &[i64], name: &str) -> Result<ItemInfo, StudioError> {
        self.name_free(name)?;
        let items = self.scene_items(scene)?;
        let picked: Vec<Value> = items
            .iter()
            .filter(|i| i.get("id").and_then(Value::as_i64).is_some_and(|id| ids.contains(&id)))
            .cloned()
            .collect();
        if picked.is_empty() {
            return Err(fail("Pick the sources to group first."));
        }
        // The box round them all on the canvas: the group's place and size.
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for it in &picked {
            let f = self.info_of(it).frame;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                let (x, y) = (f[4] + u * f[0] + v * f[2], f[5] + u * f[1] + v * f[3]);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
        let children: Vec<Value> = picked
            .into_iter()
            .map(|mut it| {
                let (x, y) = (
                    it.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    it.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                );
                it["pos"] = json!({ "x": x - x0, "y": y - y0 });
                it
            })
            .collect();
        let (cx, cy) = ((x1 - x0).max(1.0).ceil() as u32, (y1 - y0).max(1.0).ceil() as u32);
        let group = json!({
            "id": "group", "versioned_id": "group", "name": name, "uuid": uuid(),
            "settings": { "items": children, "cx": cx, "cy": cy, "id_counter": 0, "custom_size": true },
            "mixers": 0, "flags": 0, "volume": 1.0, "balance": 0.5, "enabled": true, "muted": false, "filters": [],
        });
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(group.clone())));
        // Where the topmost picked item was, the group goes.
        let top = ok_or(self.item_ids(scene)?.into_iter().find(|i| ids.contains(i)))?;
        let (at, _) = self.item_place(scene, top)?;
        self.with_items(scene, |l, _| {
            l.retain(|i| !i.get("id").and_then(Value::as_i64).is_some_and(|id| ids.contains(&id)))
        })?;
        let info = self.place(scene, &group)?;
        self.with_item(scene, info.id, |i| i["pos"] = json!({ "x": x0, "y": y0 }))?;
        self.item_order(scene, info.id, at)?;
        Ok(self.info_of(&self.item(scene, info.id)?))
    }

    /// An empty group at the top of `scene`.
    pub fn add_group(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        self.scene_items(scene)?;
        self.name_free(name)?;
        let group = json!({
            "id": "group", "versioned_id": "group", "name": name, "uuid": uuid(),
            "settings": { "items": [], "cx": 0, "cy": 0, "id_counter": 0, "custom_size": false },
            "mixers": 0, "flags": 0, "volume": 1.0, "balance": 0.5, "enabled": true, "muted": false, "filters": [],
        });
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(group.clone())));
        self.place(scene, &group)
    }

    /// Takes a group's items out into its scene, each where it shows; the group goes.
    pub fn ungroup(&self, scene: &str, id: i64) -> Result<(), StudioError> {
        let g = self.item(scene, id)?;
        let name = name_of(&g).to_string();
        let src = self.source(&name)?;
        if kind_of(&src) != "group" {
            return Err(fail(format!("\"{name}\" isn't a group.")));
        }
        let p = ferrocore::obs::placement_of(&g);
        let (gw, gh) = self.size_of(&name);
        let to_canvas = ferrocore::draw_transform(&p, gw, gh);
        let (scale, turn) =
            ((p.scale_x * p.scale_x + p.scale_y * p.scale_y).sqrt() / std::f32::consts::SQRT_2, p.rotation);
        let children: Vec<Value> =
            src.pointer("/settings/items").and_then(Value::as_array).cloned().unwrap_or_default();
        let (at, _) = self.item_place(scene, id)?;
        self.with_items(scene, |l, set| {
            l.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id));
            let mut next = set
                .get("id_counter")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(l.iter().filter_map(|i| i.get("id")?.as_i64()).max().unwrap_or(0));
            // Bottom first, at the group's place in the list (counted from the top).
            let at_bottom = l.len().saturating_sub(at);
            for (k, mut c) in children.into_iter().enumerate() {
                let (x, y) = (
                    c.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    c.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                );
                let (nx, ny) = to_canvas.apply(x, y);
                c["pos"] = json!({ "x": nx, "y": ny });
                let (sx, sy) = (
                    c.pointer("/scale/x").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                    c.pointer("/scale/y").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                );
                c["scale"] = json!({ "x": sx * scale, "y": sy * scale });
                c["rot"] = json!((c.get("rot").and_then(Value::as_f64).unwrap_or(0.0) as f32 + turn).rem_euclid(360.0));
                next += 1;
                c["id"] = json!(next);
                l.insert((at_bottom + k).min(l.len()), c);
            }
            set.insert("id_counter".into(), json!(next));
        })?;
        // The group goes when no scene shows it any more.
        let shown =
            self.order().iter().any(|sc| self.scene_items(sc).is_ok_and(|i| i.iter().any(|i| name_of(i) == name)));
        if !shown {
            self.edit(|d| d["sources"].as_array_mut().map(|l| l.retain(|s| name_of(s) != name)));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ the engine itself

    /// The canvas everything is drawn at.
    pub fn canvas(&self) -> Canvas {
        self.canvas
    }

    /// A new canvas size or frame rate: our engine starts again at it (outputs and previews
    /// must be made again; the app does). Refused while anything uses the engine.
    pub fn set_canvas(&mut self, canvas: Canvas) -> Result<(), StudioError> {
        if canvas.width == 0 || canvas.height == 0 || canvas.fps == 0 {
            return Err(fail("The picture needs a size and a frame rate."));
        }
        // Remove background's workers hold the engine for a moment each round: stopped first, so
        // only outputs count (and started again if the size can't change).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.cutouts.borrow_mut().clear();
        if let Some(n) = self.engine.borrow().as_ref().map(Arc::strong_count).filter(|n| *n > 1) {
            tracing::warn!(holders = n - 1, "the picture size can't change: something still uses the engine");
            self.sync_pages();
            return Err(fail("The picture size can't change while streaming or recording. Stop first."));
        }
        *self.engine.borrow_mut() = None;
        self.canvas = canvas;
        Ok(())
    }

    /// How the engine is doing, as the Studio's stats show it.
    pub fn stats(&self) -> crate::EngineStats {
        let (cpu, memory_mb) = crate::system::usage();
        let s = self.engine.borrow().as_ref().map(|e| e.engine.stats()).unwrap_or_default();
        crate::EngineStats {
            cpu,
            fps: self.fps_now(s.frames, s.skipped, std::time::Instant::now()),
            frame_ms: f64::from(s.frame_ms),
            lagged: s.skipped.min(u64::from(u32::MAX)) as u32,
            frames: s.frames.min(u64::from(u32::MAX)) as u32,
            memory_mb,
            skipped: 0,
            encoded: s.frames.min(u64::from(u32::MAX)) as u32,
            recovered: s.recovered.min(u64::from(u32::MAX)) as u32,
            restarts: s.restarts.min(u64::from(u32::MAX)) as u32,
        }
    }

    /// The frame rate now, as OBS shows it: over the last second or so (at least a second
    /// between counts), not since the Studio started. That average stayed low long after a
    /// busy moment: "20 / 30" on 10 Nov while every frame came on time. The first count, with
    /// nothing before it, is the share since the start.
    fn fps_now(&self, frames: u64, skipped: u64, now: std::time::Instant) -> f64 {
        let canvas = f64::from(self.canvas.fps);
        let since_start = canvas * frames as f64 / (frames + skipped).max(1) as f64;
        let fps = match self.fps_seen.get() {
            Some((at, f, _, fps)) if now.duration_since(at) < std::time::Duration::from_secs(1) && frames >= f => {
                return fps;
            }
            Some((at, f, _, _)) if frames >= f => (frames - f) as f64 / now.duration_since(at).as_secs_f64().max(0.001),
            // A new engine (its count started again), or the first count.
            _ => since_start,
        };
        let fps = fps.min(canvas);
        self.fps_seen.set(Some((now, frames, skipped, fps)));
        fps
    }

    /// Where "Hear it" plays: the system's default, then every output device by name.
    pub fn monitoring_devices(&self) -> Vec<(String, String)> {
        #[allow(unused_mut)]
        let mut out = vec![("Default".to_string(), "default".to_string())];
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        out.extend(ferrocore::monitor::devices().into_iter().map(|n| (n.clone(), n)));
        out
    }

    /// Plays monitored sound on device `id` (a name from `monitoring_devices`, or "default").
    pub fn set_monitoring_device(&self, _name: &str, id: &str) -> Result<(), StudioError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let device = (id != "default" && !id.is_empty()).then_some(id);
            *self.monitor_device.borrow_mut() = device.map(String::from);
            self.engine()?.engine.set_monitoring(true, device).map_err(|e| fail(e.to_string()))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = id;
            Ok(())
        }
    }

    // ------------------------------------------------------------------ meters and channels

    /// Each named source's sound now (dBFS; -100 is silence): the mixer's meters.
    pub fn levels(&self, names: &[String]) -> std::collections::HashMap<String, crate::collection::LevelInfo> {
        let Some(e) = self.engine.borrow().clone() else { return Default::default() };
        names
            .iter()
            .filter_map(|n| {
                let (id, _) = e.engine.source_named(n)?;
                let l = e.engine.levels(id)?;
                // Silence is -100, not -inf: JSON has no infinity, so it went out as null, which
                // the page read as 0 dB, a full red meter the moment the sound stopped.
                let floor = |v: [f32; 2]| v.map(|x| if x.is_finite() { x.max(-100.0) } else { -100.0 });
                Some((n.clone(), crate::collection::LevelInfo { peak: floor(l.peak), magnitude: floor(l.rms) }))
            })
            .collect()
    }

    /// Ferrocast's sound care: `{ ducking: { voices, music, depth_db } | null, loudness_lufs:
    /// number | null }` (music quieter while you talk; every track steered to a loudness).
    pub fn audio_care(&self) -> Value {
        let a = &self.doc.borrow()["ferrocast"]["audio"];
        json!({
            "ducking": if a["ducking"].is_object() { a["ducking"].clone() } else { Value::Null },
            "loudness_lufs": a["loudness_lufs"].clone(),
        })
    }

    /// Sets the sound care (as `audio_care` gives it); an undo step like any edit.
    pub fn set_audio_care(&self, care: &Value) -> Result<(), StudioError> {
        let names = |k: &str| -> Result<Vec<String>, StudioError> {
            let list = care["ducking"][k].as_array().cloned().unwrap_or_default();
            if list.len() > 16 {
                return Err(fail(format!("ducking.{k}: 16 sources at most.")));
            }
            list.iter()
                .map(|n| n.as_str().map(String::from).ok_or_else(|| fail(format!("ducking.{k}: source names."))))
                .collect()
        };
        let ducking = if care["ducking"].is_object() {
            let (voices, music) = (names("voices")?, names("music")?);
            if voices.iter().any(|v| music.contains(v)) {
                return Err(fail("ducking: a source can't duck itself."));
            }
            let depth = care["ducking"]["depth_db"].as_f64().unwrap_or(-12.0);
            if !(-40.0..=-3.0).contains(&depth) {
                return Err(fail("ducking.depth_db: -40 to -3 dB."));
            }
            json!({ "voices": voices, "music": music, "depth_db": depth })
        } else {
            Value::Null
        };
        let loudness = match care["loudness_lufs"].as_f64() {
            Some(l) if (-30.0..=-8.0).contains(&l) => json!(l),
            Some(_) => return Err(fail("loudness_lufs: -30 to -8 LUFS (-14 for most platforms).")),
            None => Value::Null,
        };
        self.edit(|d| {
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"]["audio"] = json!({ "ducking": ducking, "loudness_lufs": loudness });
        });
        Ok(())
    }

    /// The privacy guard: `{ hide_notifications: bool, hide_apps: string[] }`, what a display
    /// capture leaves out (notifications unless turned off; apps by bundle id or name).
    pub fn privacy(&self) -> Value {
        let p = &self.doc.borrow()["ferrocast"]["privacy"];
        json!({
            "hide_notifications": p["hide_notifications"].as_bool() != Some(false),
            "hide_apps": p["hide_apps"].as_array().cloned().unwrap_or_default(),
            // Leaving apps out of a capture is something only macOS does: elsewhere it isn't
            // offered (a setting that did nothing would show what it promised to hide).
            "supported": cfg!(target_os = "macos"),
        })
    }

    /// Sets it (as `privacy` gives it); an undo step like any edit. Display captures start again
    /// with the new apps left out.
    pub fn set_privacy(&self, privacy: &Value) -> Result<(), StudioError> {
        let notifications = match &privacy["hide_notifications"] {
            Value::Null => true,
            Value::Bool(b) => *b,
            _ => return Err(fail("hide_notifications: true or false.")),
        };
        let list = privacy["hide_apps"].as_array().cloned().unwrap_or_default();
        if list.len() > 32 {
            return Err(fail("hide_apps: 32 apps at most."));
        }
        let mut apps: Vec<String> = Vec::new();
        for a in &list {
            let a = a.as_str().map(str::trim).unwrap_or_default();
            if a.is_empty() || a.len() > 200 || a.chars().any(char::is_control) {
                return Err(fail("hide_apps: app names or bundle ids."));
            }
            if !apps.iter().any(|b| b.eq_ignore_ascii_case(a)) {
                apps.push(a.to_string());
            }
        }
        self.edit(|d| {
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"]["privacy"] = json!({ "hide_notifications": notifications, "hide_apps": apps });
        });
        Ok(())
    }

    /// How these sources' pictures look now (a 32x18 glance): brightness, how much it varies, and a
    /// fingerprint that changes when the picture does. Sources with no picture yet are left out.
    pub fn look(&self, names: &[String]) -> Result<std::collections::HashMap<String, crate::Look>, StudioError> {
        // Started if it isn't yet (a camera only loaded has no picture to look at otherwise).
        let e = self.engine()?;
        Ok(names
            .iter()
            .filter_map(|n| {
                let (id, _) = e.engine.source_named(n)?;
                let rgba = e.engine.snapshot_source_at(id, (LOOK_W, LOOK_H))?;
                Some((n.clone(), crate::Look::of(&rgba)))
            })
            .collect())
    }

    /// Puts a device on global channel `ch` (1-6: Desktop Audio 1-2, Mic/Aux 1-4), heard in every
    /// scene, or takes it off (None). The device is (kind, name, settings).
    pub fn set_channel(&self, ch: u32, device: Option<(&str, &str, &Value)>) -> Result<(), StudioError> {
        let key = *CHANNEL_KEYS.get(ch.wrapping_sub(1) as usize).ok_or_else(|| fail("channel: 1 to 6."))?;
        match device {
            None => {
                self.edit(|d| d.as_object_mut().map(|m| m.remove(key)));
            }
            Some((kind, name, settings)) => {
                let taken = self.doc.borrow().get(key).is_some_and(|s| name_of(s) == name);
                if !taken {
                    self.name_free(name)?;
                }
                let mut set = crate::kinds::defaults(kind).unwrap_or_else(|| json!({}));
                if let (Some(base), Some(given)) = (set.as_object_mut(), settings.as_object()) {
                    for (k, v) in given {
                        base.insert(k.clone(), v.clone());
                    }
                }
                let src = json!({
                    "id": kind, "versioned_id": kind, "name": name, "uuid": uuid(), "settings": set,
                    "mixers": 255, "sync": 0, "flags": 0, "volume": 1.0, "balance": 0.5,
                    "enabled": true, "muted": false, "monitoring_type": 0, "filters": [],
                });
                self.edit(|d| d[key] = src);
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ files

    /// Files sources and filters use that aren't there any more (OBS's Missing Files).
    pub fn missing_files(&self) -> Vec<crate::collection::MissingFile> {
        const KEYS: [(&str, &str); 4] =
            [("file", "Image File"), ("local_file", "Local File"), ("image_path", "Path"), ("text_file", "Text File")];
        let mut out = Vec::new();
        let mut check = |s: &Value, parent: Option<&str>| {
            for (k, label) in KEYS {
                let Some(p) = s.pointer(&format!("/settings/{k}")).and_then(Value::as_str) else { continue };
                // A text file is only read when "from file" is on; a network media address isn't a file.
                if (k == "text_file" && s.pointer("/settings/from_file").and_then(Value::as_bool) != Some(true))
                    || (k == "local_file"
                        && s.pointer("/settings/is_local_file").and_then(Value::as_bool) == Some(false))
                {
                    continue;
                }
                if !p.is_empty() && !Path::new(p).exists() {
                    out.push(crate::collection::MissingFile {
                        source: name_of(s).to_string(),
                        parent: parent.map(String::from),
                        kind: kind_of(s).to_string(),
                        key: k.to_string(),
                        label: label.to_string(),
                        path: p.to_string(),
                    });
                }
            }
        };
        for s in self.sources_list().iter().chain(self.channels_list().iter()) {
            check(s, None);
            for f in s.get("filters").and_then(Value::as_array).into_iter().flatten() {
                check(f, Some(name_of(s)));
            }
        }
        out
    }

    /// Points a missing file's setting at `to` instead.
    pub fn replace_file(&self, m: &crate::collection::MissingFile, to: &str) -> Result<(), StudioError> {
        let set = json!({ m.key.clone(): to });
        match &m.parent {
            Some(parent) => self.update_filter(parent, &m.source, &set),
            None => self.update(&m.source, &set),
        }
    }

    /// A picture of a scene (at its canvas's size) as a PNG at `path`.
    pub fn screenshot(&self, name: &str, path: &Path) -> Result<(), StudioError> {
        let (px, size) = self.screenshot_pixels(name)?;
        write_png(path, &px, size)
    }

    /// A scene (or vertical scene) at its picture's full size, as RGBA pixels: saving them as a
    /// PNG ([`write_png`]) can happen off this thread.
    pub fn screenshot_pixels(&self, name: &str) -> Result<(Vec<u8>, (u32, u32)), StudioError> {
        let size = if self.vertical_order().iter().any(|n| n == name) {
            crate::collection::VERTICAL_SIZE
        } else {
            self.scene_index(name)?;
            (self.canvas.width, self.canvas.height)
        };
        let px = self.engine()?.engine.snapshot_scene_at(name, size, size).map_err(|e| fail(e.to_string()))?;
        Ok((px, size))
    }

    /// A picture of what's on air exactly, as a PNG at `path`.
    pub fn screenshot_program(&self, path: &Path) -> Result<(), StudioError> {
        let (px, size) = self.screenshot_program_pixels()?;
        write_png(path, &px, size)
    }

    /// What's on air as RGBA pixels and their size.
    pub fn screenshot_program_pixels(&self) -> Result<(Vec<u8>, (u32, u32)), StudioError> {
        let px = self.engine()?.engine.snapshot().map_err(|e| fail(e.to_string()))?;
        Ok((px, (self.canvas.width, self.canvas.height)))
    }

    // ------------------------------------------------------------------ the vertical picture

    fn vertical_doc(&self) -> Option<Value> {
        self.doc.borrow().pointer("/ferrocast/vertical").cloned()
    }

    /// The vertical picture (9:16, its own scenes) when it's on.
    pub fn vertical(&self) -> Option<crate::collection::VerticalInfo> {
        let v = self.vertical_doc()?;
        if v.get("on").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let current = v.get("current_scene").and_then(Value::as_str).unwrap_or_default().to_string();
        let (w, h) = crate::collection::VERTICAL_SIZE;
        Some(crate::collection::VerticalInfo {
            width: w,
            height: h,
            scenes: self
                .vertical_order()
                .into_iter()
                .map(|name| SceneInfo {
                    items: self.scene_items(&name).map_or(0, |i| i.len()),
                    current: name == current,
                    name,
                    preview: false,
                    transition: None,
                })
                .collect(),
            current,
        })
    }

    /// Turns the vertical picture on: as it was, or with a first scene.
    pub fn start_vertical(&self, first_scene: &str) -> Result<(), StudioError> {
        if self.vertical_order().is_empty() {
            self.add_scene(first_scene)?;
            let order: Vec<String> = self.order().into_iter().filter(|n| n != first_scene).collect();
            self.set_order(&order);
            self.edit(|d| {
                if !d.get("ferrocast").is_some_and(Value::is_object) {
                    d["ferrocast"] = json!({});
                }
                d["ferrocast"]["vertical"] =
                    json!({ "on": true, "current_scene": first_scene, "scene_order": [{ "name": first_scene }] });
            });
        } else {
            self.edit(|d| d["ferrocast"]["vertical"]["on"] = json!(true));
        }
        Ok(())
    }

    /// Turns it off, keeping its scenes for next time.
    pub fn stop_vertical(&self) {
        if self.vertical_doc().is_some() {
            self.edit(|d| d["ferrocast"]["vertical"]["on"] = json!(false));
        }
    }

    pub fn add_vertical_scene(&self, name: &str) -> Result<(), StudioError> {
        if self.vertical().is_none() {
            return Err(fail("Turn the vertical picture on first."));
        }
        let main = self.order();
        self.add_scene(name)?;
        self.set_order(&main);
        self.edit(|d| {
            if let Some(l) = d.pointer_mut("/ferrocast/vertical/scene_order").and_then(Value::as_array_mut) {
                l.push(json!({ "name": name }));
            }
        });
        Ok(())
    }

    /// Puts one of the vertical picture's scenes on air there.
    pub fn set_vertical_current(&self, name: &str) -> Result<(), StudioError> {
        if !self.vertical_order().iter().any(|n| n == name) {
            return Err(fail(format!("The vertical picture has no scene called \"{name}\".")));
        }
        self.edit(|d| d["ferrocast"]["vertical"]["current_scene"] = json!(name));
        Ok(())
    }

    /// Streams the vertical picture (its own scenes, 9:16), as `stream` streams the main one.
    pub fn stream_vertical(&self, server: &str, key: &str, s: &StreamSettings) -> Result<StageOutput, StudioError> {
        if self.vertical().is_none() {
            return Err(fail("The vertical picture is off."));
        }
        let mut s = s.clone();
        s.encode.scale = None;
        // Twitch's VOD track is the main picture's only.
        s.vod_track = None;
        if let Some(why) = own::unsupported(&s, true) {
            return Err(fail(format!("Ferrocast's own engine doesn't do {why} yet.")));
        }
        let how = own::Drawing {
            send_to: Some(own::Send { server, key, reconnect: s.reconnect }),
            record: None,
            encode: &s.encode,
            tracks: &[],
            delay_s: s.delay_s,
            vertical: true,
            replay: None,
            solo: None,
        };
        Ok(StageOutput::new(self.output(how)?, None, self.canvas))
    }

    // ------------------------------------------------------------------ face follow

    /// Starts an item following faces (or what stands out, `action`): it fills the picture's
    /// height, centred; how it was comes back with `stop_follow`.
    pub fn start_follow(&self, scene: &str, id: i64, action: bool) -> Result<crate::FaceFollow, StudioError> {
        use crate::collection::follow::aim;
        let before = self.transform(scene, id)?;
        if before.width == 0 || before.height == 0 {
            return Err(fail("That source shows no picture yet."));
        }
        let (cw, ch) = self.canvas_of(scene);
        let shown_w = (before.width as i32 - before.crop_left - before.crop_right).max(1) as f32;
        let shown_h = (before.height as i32 - before.crop_top - before.crop_bottom).max(1) as f32;
        let scale = ch as f32 / shown_h;
        let t = Transform {
            x: aim(0.5, shown_w * scale, cw as f32),
            y: 0.0,
            rotation: 0.0,
            scale_x: scale,
            scale_y: scale,
            alignment: 5,
            bounds: "none".into(),
            bounds_alignment: 0,
            bounds_width: 0.0,
            bounds_height: 0.0,
            ..before.clone()
        };
        self.set_transform(scene, id, &t)?;
        Ok(crate::FaceFollow { scene: scene.into(), id, action, before, misses: 0 })
    }

    /// A small picture of the following item's source, to find the face in.
    pub fn follow_look(&self, f: &crate::FaceFollow) -> Result<Option<crate::FollowLook>, StudioError> {
        use crate::collection::follow::LOOK_WIDTH;
        let name = name_of(&self.item(&f.scene, f.id)?).to_string();
        let (sw, sh) = self.own_size_of(&name);
        let e = self.engine()?;
        let Some((id, _)) = e.engine.source_named(&name) else { return Ok(None) };
        if sw == 0 || sh == 0 {
            return Ok(None);
        }
        let h = ((LOOK_WIDTH as f32 * sh as f32 / sw as f32).round() as u32).clamp(1, LOOK_WIDTH * 4);
        Ok(e.engine.snapshot_source_at(id, (LOOK_WIDTH, h)).map(|rgba| crate::FollowLook {
            rgba,
            width: LOOK_WIDTH,
            height: h,
        }))
    }

    /// Moves the item a step toward `face` (the biggest found), or keeps it where it is.
    pub fn follow_step(&self, f: &mut crate::FaceFollow, face: Option<crate::Face>) -> Result<(), StudioError> {
        use crate::collection::follow::{aim, ease, shown_mid};
        let Some(face) = face else {
            f.misses += 1;
            return Ok(());
        };
        f.misses = 0;
        let t = self.transform(&f.scene, f.id)?;
        let (cw, _) = self.canvas_of(&f.scene);
        let shown_w = (t.width as i32 - t.crop_left - t.crop_right).max(1) as f32 * t.scale_x.abs();
        let mid = shown_mid(&face, t.width as f32, t.crop_left as f32, t.crop_right as f32);
        let left = match t.alignment & 3 {
            1 => t.x,
            2 => t.x - shown_w,
            _ => t.x - shown_w / 2.0,
        };
        let to = ease(left, aim(mid, shown_w, cw as f32), cw as f32);
        if to != left {
            self.set_transform(&f.scene, f.id, &Transform { x: t.x + (to - left), ..t })?;
        }
        Ok(())
    }

    /// Puts the item back as it was before following.
    pub fn stop_follow(&self, f: &crate::FaceFollow) {
        if self.item(&f.scene, f.id).is_ok() {
            let _ = self.set_transform(&f.scene, f.id, &f.before);
        }
    }

    /// The canvas a scene is drawn on: the vertical picture's, or the main one.
    fn canvas_of(&self, scene: &str) -> (u32, u32) {
        if self.vertical_order().iter().any(|n| n == scene) {
            crate::collection::VERTICAL_SIZE
        } else {
            (self.canvas.width, self.canvas.height)
        }
    }

    // ------------------------------------------------------------------ listening

    /// Starts the virtual camera (Linux: a v4l2loopback camera) showing what's on air, or
    /// `source` alone, at the canvas's size and frame rate.
    #[cfg(target_os = "linux")]
    pub fn virtual_camera(&self, source: Option<&str>) -> Result<crate::VirtualCamera, StudioError> {
        let e = self.engine()?;
        if let Some(name) = source
            && e.engine.source_named(name).is_none()
        {
            return Err(fail(format!("There's no source called \"{name}\".")));
        }
        let size = (self.canvas.width, self.canvas.height);
        let cam =
            crate::virtualcam::Loopback::start(Arc::downgrade(&e), source.map(String::from), size, self.canvas.fps)
                .map_err(fail)?;
        Ok(crate::VirtualCamera { cam })
    }

    /// Listens to a source's sound (mono, 48 kHz) for anything that wants it: Whisper captions.
    pub fn tap_sound(&self, source: &str) -> Result<crate::SoundTap, StudioError> {
        let e = self.engine()?;
        let (id, _) =
            e.engine.source_named(source).ok_or_else(|| fail(format!("There's no source called \"{source}\".")))?;
        Ok(crate::SoundTap { tap: crate::collection::Tapped::Own(e.engine.tap_sound(id)) })
    }

    /// Live captions on a source's sound, recognised on this Mac (Apple's Speech, on-device):
    /// `on_text` gets the words so far, and true when a sentence is done. Main thread.
    pub fn start_captions(
        &self,
        source: &str,
        on_text: impl Fn(String, bool) + 'static,
    ) -> Result<crate::Captions, StudioError> {
        #[cfg(target_os = "macos")]
        {
            use crate::ffi::speech::{Access, Listener, Unavailable};
            let e = self.engine()?;
            let (id, _) =
                e.engine.source_named(source).ok_or_else(|| fail(format!("There's no source called \"{source}\".")))?;
            let tap = e.engine.tap_sound(id);
            let (listener, feeder) = Listener::start_fed(48_000, on_text).map_err(|e| match e {
                Unavailable::NoAccess(Access::NotAsked) => fail("Allow speech recognition first."),
                Unavailable::NoAccess(_) => fail(
                    "Ferrocast isn't allowed to recognise speech: turn it on in System Settings, Privacy & Security, Speech Recognition.",
                ),
                Unavailable::NotOnDevice => fail(
                    "This Mac can't recognise your language on the device yet: in System Settings, Keyboard, turn on Dictation to download it.",
                ),
                Unavailable::NoRecognizer => fail("Speech recognition isn't available on this Mac."),
            })?;
            // Our engine's sound to the recogniser, a few times a second.
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread = {
                let stop = stop.clone();
                std::thread::Builder::new()
                    .name("ferrocast-captions".into())
                    .spawn(move || {
                        while !stop.load(std::sync::atomic::Ordering::Acquire) {
                            feeder.feed(&tap.take());
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    })
                    .map_err(|e| fail(format!("Captions couldn't start: {e}")))?
            };
            Ok(crate::Captions { listener, pump: Some((stop, thread)) })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (source, on_text);
            Err(fail("Live captions work on a Mac for now."))
        }
    }

    // ------------------------------------------------------------------ Studio Mode's extras

    /// Ferrocast's own settings kept in the collection (Studio Mode's options, quick
    /// transitions, transition overrides), under `key`.
    fn extra(&self, key: &str) -> Value {
        self.doc.borrow().pointer(&format!("/ferrocast/{key}")).cloned().unwrap_or(Value::Null)
    }

    fn set_extra(&self, key: &str, v: Value) {
        self.edit(|d| {
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"][key] = v;
        });
    }

    pub fn studio_options(&self) -> crate::collection::StudioOptions {
        serde_json::from_value(self.extra("studio_options")).unwrap_or_default()
    }

    pub fn set_studio_options(&self, o: crate::collection::StudioOptions) -> Result<(), StudioError> {
        self.set_extra("studio_options", json!(o));
        Ok(())
    }

    /// Swaps what's on air and Studio Mode's preview, at once.
    pub fn swap_scenes(&self) -> Result<(), StudioError> {
        self.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }))
    }

    /// OBS's quick transitions (one click sends the preview on air through each).
    pub fn quick_transitions(&self) -> Vec<TransitionInfo> {
        serde_json::from_value(self.extra("quick")).unwrap_or_else(|_| {
            vec![
                TransitionInfo { kind: "cut_transition".into(), ms: 0 },
                TransitionInfo { kind: "fade_transition".into(), ms: 300 },
            ]
        })
    }

    pub fn set_quick_transitions(&self, list: &[TransitionInfo]) -> Result<(), StudioError> {
        if list.len() > 16 || list.iter().any(|t| t.ms > 20_000) {
            return Err(fail("At most 16 quick transitions, each 0 to 20000 ms."));
        }
        self.set_extra("quick", json!(list));
        Ok(())
    }

    /// A scene's own way in (OBS's Transition Override), or the Studio's (None).
    pub fn scene_transition_of(&self, scene: &str) -> Result<Option<TransitionInfo>, StudioError> {
        self.scene_index(scene)?;
        Ok(serde_json::from_value(self.extra("scene_transitions").get(scene).cloned().unwrap_or(Value::Null)).ok())
    }

    pub fn set_scene_transition(&self, scene: &str, t: Option<&TransitionInfo>) -> Result<(), StudioError> {
        self.scene_index(scene)?;
        let mut all = self.extra("scene_transitions");
        if !all.is_object() {
            all = json!({});
        }
        match t {
            Some(t) => all[scene] = json!(t),
            None => {
                all.as_object_mut().map(|m| m.remove(scene));
            }
        }
        self.set_extra("scene_transitions", all);
        Ok(())
    }

    /// How an item appears when shown (or goes when hidden): kept, drawn as a cut for now.
    pub fn set_item_transition(
        &self,
        scene: &str,
        id: i64,
        show: bool,
        t: Option<&TransitionInfo>,
    ) -> Result<(), StudioError> {
        let key = if show { "show_transition" } else { "hide_transition" };
        let v = t.map_or(json!({}), |t| json!({ "id": t.kind, "duration": t.ms }));
        self.with_item(scene, id, |i| i[key] = v)
    }

    /// Each transition kind's own settings.
    pub fn transition_settings(&self, kind: &str) -> Value {
        self.extra("transition_settings").get(kind).cloned().unwrap_or_else(|| json!({}))
    }

    pub fn set_transition_settings(&self, kind: &str, settings: &Value) -> Result<(), StudioError> {
        let mut all = self.extra("transition_settings");
        if !all.is_object() {
            all = json!({});
        }
        all[kind] = settings.clone();
        self.set_extra("transition_settings", all);
        Ok(())
    }

    /// Fades what's on air to black over the Studio's transition time, or back → whether it's
    /// black now.
    pub fn fade_to_black(&self) -> Result<bool, StudioError> {
        let e = self.engine()?;
        let to = !e.is_black();
        e.fade_black(to, u64::from(self.transition().ms.max(300)))?;
        Ok(to)
    }

    pub fn is_black(&self) -> bool {
        self.engine.borrow().as_ref().is_some_and(|e| e.is_black())
    }

    /// The T-bar: let go past halfway, the preview goes on air (dragged positions aren't drawn
    /// part way yet).
    pub fn tbar(&self, at: f32, release: bool) -> Result<crate::collection::TBarState, StudioError> {
        if self.preview_scene().is_none() {
            return Err(fail("Studio Mode is off."));
        }
        if release && at >= 0.5 {
            self.transition_now(None)?;
        }
        Ok(crate::collection::TBarState { active: !release, at: if release { 0.0 } else { at.clamp(0.0, 1.0) } })
    }

    pub fn tbar_state(&self) -> crate::collection::TBarState {
        crate::collection::TBarState { active: false, at: 0.0 }
    }

    // ------------------------------------------------------------------ more items

    /// A copy of a source (its own settings and filters) placed in `scene` (OBS's Paste
    /// (Duplicate)) → its item.
    pub fn paste_copy(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let mut src = self.source(name)?;
        let mut n = 2;
        let copy = loop {
            let c = format!("{name} {n}");
            if self.name_free(&c).is_ok() {
                break c;
            }
            n += 1;
        };
        src["name"] = json!(copy);
        src["uuid"] = json!(uuid());
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(src.clone())));
        self.place(scene, &src)
    }

    /// Moves an item into a group of the same scene (by the group's item id), or out of any.
    pub fn set_group(&self, scene: &str, id: i64, group: Option<i64>) -> Result<(), StudioError> {
        match group {
            Some(g) => {
                let gname = name_of(&self.item(scene, g)?).to_string();
                let it = self.item(scene, id)?;
                let gpos = (
                    self.item(scene, g)?.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0),
                    self.item(scene, g)?.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0),
                );
                self.with_items(scene, |l, _| l.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id)))?;
                self.with_items(&gname, |l, set| {
                    let mut c = it.clone();
                    let x = c.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) - gpos.0;
                    let y = c.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) - gpos.1;
                    c["pos"] = json!({ "x": x, "y": y });
                    let next = set.get("id_counter").and_then(Value::as_i64).unwrap_or(0) + 1;
                    set.insert("id_counter".into(), json!(next));
                    c["id"] = json!(next);
                    l.push(c);
                })
            }
            None => Err(fail("Ungroup the group to take its sources out.")),
        }
    }

    /// A group item's name and its sources' names.
    pub fn group_parts(&self, scene: &str, id: i64) -> Result<(String, Vec<String>), StudioError> {
        let g = name_of(&self.item(scene, id)?).to_string();
        let parts = self.scene_items(&g)?.iter().map(|i| name_of(i).to_string()).collect();
        Ok((g, parts))
    }

    /// The item ids showing each of `names` in `scene`.
    pub fn ids_named(&self, scene: &str, names: &[String]) -> Result<Vec<i64>, StudioError> {
        Ok(self
            .scene_items(scene)?
            .iter()
            .filter(|i| names.iter().any(|n| n == name_of(i)))
            .filter_map(|i| i.get("id")?.as_i64())
            .collect())
    }

    /// The sounds a scene makes (its sources with sound) and the global channels.
    pub fn audio(&self, scene: &str) -> Result<Vec<AudioInfo>, StudioError> {
        let mut out: Vec<AudioInfo> = Vec::new();
        for it in self.scene_items(scene)?.iter().rev() {
            let name = name_of(it);
            if let Ok(src) = self.source(name)
                && !is_scene(&src)
                && self.info_of(it).audio
                && !out.iter().any(|a| a.name == name)
            {
                out.push(self.audio_info(&src, None));
            }
        }
        out.extend(self.channels());
        Ok(out)
    }

    /// Deinterlacing: our engine's sources are never interlaced.
    pub fn deinterlace_of(&self, _name: &str) -> Result<(String, bool), StudioError> {
        Ok(("disable".into(), true))
    }

    pub fn set_deinterlace(&self, _name: &str, mode: &str, _top_field_first: bool) -> Result<(), StudioError> {
        if mode == "disable" { Ok(()) } else { Err(fail("Ferrocast's own engine doesn't deinterlace yet.")) }
    }

    /// OBS's media controls on a video or sound file → where it stands after.
    pub fn media(
        &self,
        name: &str,
        what: crate::collection::MediaAct,
    ) -> Result<crate::collection::MediaInfo, StudioError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            use crate::collection::MediaAct as A;
            use ferrocore::media::Control as C;
            let e = self.engine()?;
            let id = self.media_id(&e, name)?;
            let c = match what {
                A::Play => C::Play,
                A::Pause => C::Pause,
                A::PlayPause => match e.engine.media_state(id).map(|s| s.0) {
                    Some(ferrocore::media::MediaState::Playing) => C::Pause,
                    _ => C::Play,
                },
                A::Restart => C::Restart,
                A::Stop => C::Stop,
                // One file: next and previous start it again (OBS's for a playlist).
                A::Next | A::Previous => C::Restart,
            };
            e.engine.media_control(id, c).map_err(|e| fail(e.to_string()))?;
            self.media_state(name)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = what;
            Err(fail(format!("\"{name}\": media files play on a Mac for now.")))
        }
    }

    /// Whether a source's first picture is in (our engine knows its size by then): an instant
    /// replay's scene waits for it, or it went on air empty for up to a second.
    pub fn has_picture(&self, name: &str) -> bool {
        self.engine.borrow().as_ref().and_then(|e| e.engine.source_named(name)).is_some_and(|(_, size)| size.is_some())
    }

    pub fn media_state(&self, name: &str) -> Result<crate::collection::MediaInfo, StudioError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            use ferrocore::media::MediaState as S;
            let e = self.engine()?;
            let id = self.media_id(&e, name)?;
            let (state, at, dur) =
                e.engine.media_state(id).ok_or_else(|| fail(format!("\"{name}\" isn't playing.")))?;
            Ok(crate::collection::MediaInfo {
                state: match state {
                    S::Playing => "playing",
                    S::Paused => "paused",
                    S::Stopped => "stopped",
                    S::Ended => "ended",
                },
                time_ms: (at / 1_000_000) as i64,
                duration_ms: (dur / 1_000_000) as i64,
                file_bytes: self.file_bytes(name),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(fail(format!("\"{name}\": media files play on a Mac for now.")))
        }
    }

    /// A media source's file size, when its file is on this computer.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn file_bytes(&self, name: &str) -> Option<u64> {
        let v = self.settings(name).ok()?;
        let path = v.get("local_file").and_then(Value::as_str).filter(|p| !p.is_empty())?;
        std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
    }

    /// Jumps to `ms` into a video or sound file.
    pub fn media_seek(&self, name: &str, ms: i64) -> Result<crate::collection::MediaInfo, StudioError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let e = self.engine()?;
            let id = self.media_id(&e, name)?;
            let ns = u64::try_from(ms.max(0)).unwrap_or(0) * 1_000_000;
            e.engine.media_control(id, ferrocore::media::Control::Seek(ns)).map_err(|e| fail(e.to_string()))?;
            self.media_state(name)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = ms;
            self.media_state(name)
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn media_id(&self, e: &own::Shared, name: &str) -> Result<u64, StudioError> {
        self.source(name)?;
        e.engine.source_named(name).map(|(id, _)| id).ok_or_else(|| fail(format!("\"{name}\" isn't playing yet.")))
    }

    /// Clicks into a web page source (in a window), stops, or reloads it.
    pub fn web(&self, name: &str, what: crate::collection::WebAct) -> Result<(), StudioError> {
        #[cfg(target_os = "macos")]
        {
            use crate::collection::WebAct;
            use crate::ffi::web::Do;
            let pages = self.pages.borrow();
            let (_, page) = pages.get(name).ok_or_else(|| fail(format!("\"{name}\" isn't a web page here.")))?;
            page.act(match what {
                WebAct::Interact => Do::Interact(format!("{name} (Interact)")),
                WebAct::Stop => Do::Stop,
                WebAct::Reload => Do::Reload,
            });
            Ok(())
        }
        #[cfg(target_os = "windows")]
        {
            use crate::collection::WebAct;
            use crate::ffi::web::Do;
            let pages = self.pages.borrow();
            let (_, page) = pages.get(name).ok_or_else(|| fail(format!("\"{name}\" isn't a web page here.")))?;
            page.act(match what {
                WebAct::Interact => Do::Interact(String::new()),
                WebAct::Stop => Do::Stop,
                WebAct::Reload => Do::Reload,
            })
            .map_err(fail)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = what;
            Err(fail(format!("\"{name}\": web pages work on a Mac and Windows for now.")))
        }
    }

    // libobs's housekeeping, which our engine doesn't need: kept so the app calls the same.

    pub fn set_true_peak(&self, _on: bool) {}

    /// A Move transition under way: our engine plays it itself.
    pub fn moving(&self) -> bool {
        false
    }

    pub fn tick_moves(&self) -> bool {
        false
    }

    pub fn finish_moves(&self) {}

    pub fn settle_fits(&self) {}

    pub fn follow_sizes(&self) -> bool {
        false
    }

    pub fn let_thumbs_go(&self, _all: bool) {}

    /// Each source's load on the engine (pictures sent to the card so far, their size): Stats
    /// shows which one keeps it busiest. Empty before the engine opens.
    pub fn source_load(&self) -> Vec<ferrocore::engine::SourceLoad> {
        self.engine.borrow().as_ref().map(|e| e.engine.source_load()).unwrap_or_default()
    }

    /// Captures on air that have shown nothing for a few seconds (the black screen fixer):
    /// a screen, window or app capture without Screen Recording allowed (macOS), or one with no
    /// picture (nothing picked, the window gone, a camera that never sent). The status bar and
    /// the Ready check say why and what to do.
    pub fn capture_problems(&self) -> Vec<crate::collection::CaptureProblem> {
        let Some(e) = self.engine.borrow().clone() else { return Vec::new() };
        let quiet = e.engine.quiet_captures(3_000_000_000);
        if quiet.is_empty() {
            return Vec::new();
        }
        let allowed = crate::screen_access();
        quiet
            .into_iter()
            .map(|(source, screen)| {
                let kind = self.source(&source).map(|s| kind_of(&s).to_string()).unwrap_or_default();
                let problem = if screen && !allowed { "permission" } else { "no_picture" };
                crate::collection::CaptureProblem { source, kind, problem }
            })
            .collect()
    }

    // ------------------------------------------------------------------ transitions

    /// The Studio's transition: its kind and length.
    pub fn transition(&self) -> TransitionInfo {
        let d = self.doc.borrow();
        let name = d.get("current_transition").and_then(Value::as_str).unwrap_or("Fade");
        let kind = d
            .get("transitions")
            .and_then(Value::as_array)
            .and_then(|t| t.iter().find(|t| name_of(t) == name))
            .map(|t| kind_of(t).to_string())
            .unwrap_or_else(|| if name == "Cut" { "cut_transition".into() } else { "fade_transition".into() });
        let ms = d.get("transition_duration").and_then(Value::as_u64).unwrap_or(300) as u32;
        TransitionInfo { kind, ms }
    }

    pub fn set_transition(&self, t: &TransitionInfo) -> Result<(), StudioError> {
        let name = match t.kind.as_str() {
            "cut_transition" => "Cut".to_string(),
            "fade_transition" => "Fade".to_string(),
            k => k.to_string(),
        };
        self.edit(|d| {
            let list = d.get("transitions").and_then(Value::as_array).cloned().unwrap_or_default();
            if !matches!(t.kind.as_str(), "cut_transition" | "fade_transition")
                && !list.iter().any(|x| name_of(x) == name)
            {
                let mut l = list;
                l.push(json!({ "id": t.kind, "name": name, "settings": {} }));
                d["transitions"] = Value::Array(l);
            }
            d["current_transition"] = json!(name);
            d["transition_duration"] = json!(t.ms.min(20_000));
        });
        Ok(())
    }
}

/// A stream, recording or replay buffer our engine runs for the stage. Dropping it ends it
/// (encoders flushed, the server told, the file closed).
#[derive(Debug)]
pub struct StageOutput {
    own: own::OwnStream,
    /// A recording's file now (a split moves it on).
    path: Option<PathBuf>,
    canvas: Canvas,
    /// A replay buffer's plan, and how its saves went.
    replay: Option<(ReplayPlan, Arc<Mutex<own::ReplaySaved>>)>,
}

impl StageOutput {
    fn new(own: own::OwnStream, path: Option<PathBuf>, canvas: Canvas) -> Self {
        Self { own, path, canvas, replay: None }
    }

    pub fn stats(&self) -> StreamStats {
        self.own.stats()
    }

    pub fn record_stats(&self) -> RecordStats {
        self.own.record_stats()
    }

    /// Which encoder the output uses ("h264_nvenc"…), while it runs.
    pub fn encoder(&self) -> Option<String> {
        self.own.encoder()
    }

    pub fn last_error(&self) -> Option<String> {
        self.own.last_error().or_else(|| {
            self.replay.as_ref().and_then(|(_, d)| d.lock().unwrap_or_else(|e| e.into_inner()).failed.clone())
        })
    }

    /// What our engine couldn't bring over from the collection (left out of the picture).
    pub fn skipped(&self) -> Vec<String> {
        self.own.skipped()
    }

    /// A new video bitrate while live.
    pub fn set_video_kbps(&self, kbps: u32) {
        self.own.set_video_kbps(kbps);
    }

    /// The file a recording writes now (empty for a stream).
    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(""))
    }

    /// Stats of a direct send (none: those aren't in our engine yet).
    /// An SRT, RIST or WHIP destination's stats (None when there's no such one).
    pub fn direct_stats(&self, id: &str) -> Option<StreamStats> {
        self.own.direct_stats(id)
    }

    pub fn pause(&self, on: bool) -> Result<(), StudioError> {
        self.own.pause(on)
    }

    /// Goes on in a new file, named as the recording's plan says.
    pub fn split(&mut self) -> Result<(), StudioError> {
        let plan = self.own.plan().cloned().ok_or_else(|| fail("Only a recording can be split."))?;
        let next = path_for(self.canvas, &plan);
        self.own.split(&next)?;
        self.path = Some(next);
        Ok(())
    }

    /// Splits when the plan's minutes or megabytes are reached; call it often (a tick).
    pub fn tick(&mut self) {
        if self.own.split_due()
            && let Err(e) = self.split()
        {
            tracing::warn!("the recording couldn't go on in a new file: {e}");
        }
    }

    /// Saves what a replay buffer holds, written in the background (`last_replay` names it).
    pub fn save_replay(&self) -> Result<(), StudioError> {
        let (plan, done) = self.replay.as_ref().ok_or_else(|| fail("This isn't a replay buffer."))?;
        let as_record = RecordPlan {
            folder: plan.folder.clone(),
            name: plan.name.clone(),
            format: plan.format.clone(),
            spaces: plan.spaces,
            ..RecordPlan::default()
        };
        self.own.save_replay(&path_for(self.canvas, &as_record), done.clone())
    }

    pub fn last_replay(&self) -> Option<PathBuf> {
        self.replay.as_ref().and_then(|(_, d)| d.lock().unwrap_or_else(|e| e.into_inner()).last.clone())
    }

    // `Streaming`'s, `Recording`'s and `ReplayBuffer`'s names, so the app calls either.

    /// Keeps it with the stage (the app's tick): a recording splits when its plan says.
    /// Returns false: the stage's edits reach our engine as they're made.
    pub fn follow(&mut self, _stage: &Stage) -> bool {
        self.tick();
        false
    }

    /// Drawn by our engine: always.
    pub fn drawn(&self) -> bool {
        true
    }

    /// Running (not stopped by a problem).
    pub fn active(&self) -> bool {
        self.last_error().is_none()
    }

    /// Saves the replay buffer (as `save_replay`).
    pub fn save(&self) -> Result<(), StudioError> {
        self.save_replay()
    }

    /// A chapter mark in the recording here (OBS's "Add Chapter Marker"): named, or
    /// "Chapter n" when the name is empty. In MP4 and MKV files alike.
    pub fn chapter(&self, name: &str) -> Result<(), StudioError> {
        self.own.chapter(name)
    }

    /// SRT, RIST and WebRTC sends aren't in our engine yet.
    /// Sends this stream to an SRT, RIST or WebRTC (WHIP) destination too: `srt://` (the key
    /// is the stream id), `rist://` (simple profile) or an `https://` WHIP endpoint (the key is
    /// its bearer token).
    pub fn add_direct(&mut self, id: &str, url: &str, key: &str) -> Result<(), StudioError> {
        self.own.add_direct(id, url, key, None)
    }

    /// `add_direct` for a WebRTC server with simulcast: `lower`'s stream (the lighter one) goes
    /// beside this one as a second layer, where the server takes it.
    pub fn add_direct_layered(
        &mut self,
        id: &str,
        url: &str,
        key: &str,
        lower: &StageOutput,
    ) -> Result<(), StudioError> {
        self.own.add_direct(id, url, key, lower.own.hub())
    }

    pub fn remove_direct(&mut self, id: &str) {
        self.own.remove_direct(id);
    }
}

/// The Studio's preview of the stage, drawn by our engine in a view over the page (macOS): the
/// same calls as `Preview`. Its outlines and handles come from the stage's own items, taken on
/// each `follow` (the app's tick) and kept between. Removed when dropped.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug)]
pub struct StageView {
    shared: Arc<own::Shared>,
    id: u64,
    view: ChildView,
    look: RefCell<ViewLook>,
    /// Shows the vertical picture (it follows that picture's scene on air).
    vertical: bool,
}

/// The window the preview draws in: an NSView on a Mac, a child window on Windows.
#[cfg(target_os = "macos")]
type ChildView = crate::ffi::mac::ChildView;
#[cfg(target_os = "windows")]
type ChildView = crate::ffi::win::ChildView;

/// Puts `view` at `r` (the page's box; a Mac also wants the page's height, its y runs up).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn place(view: &ChildView, r: &crate::Rect) {
    #[cfg(target_os = "macos")]
    view.set_frame(r.x, r.y, r.w, r.h, r.vh);
    #[cfg(target_os = "windows")]
    view.set_frame(r.x, r.y, r.w, r.h);
}

/// What a view keeps to draw round and over the picture.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Default)]
struct ViewLook {
    background: [f32; 4],
    /// Where the canvas is in the view (device pixels), when the page says.
    area: Option<(f32, f32, f32, f32)>,
    /// The view's size (device pixels) the page's `area` was for.
    area_for: Option<(u32, u32)>,
    margin: f32,
    /// The scene and items picked (outlined with handles).
    picked: Option<(String, Vec<i64>)>,
    /// A scene whose sources all get an outline.
    frames: Option<String>,
    shade: f32,
    /// The scene shown instead of what's on air (Studio Mode).
    scene: Option<String>,
    /// The view's pixels per point (the guides' labels are sized by it).
    ppp: f32,
    /// Distance guides for the picked source (while it's dragged).
    guides: bool,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Stage {
    /// A preview of the stage over `window`'s page at `rect` (points), `scale` its pixels per
    /// point: what's on air.
    pub fn preview_drawn(
        &self,
        window: raw_window_handle::RawWindowHandle,
        rect: crate::Rect,
        scale: f64,
    ) -> Result<StageView, StudioError> {
        self.view_of(window, rect, scale, false)
    }

    /// The vertical picture's preview (its own scenes, 9:16).
    pub fn vertical_preview_drawn(
        &self,
        window: raw_window_handle::RawWindowHandle,
        rect: crate::Rect,
        scale: f64,
    ) -> Result<StageView, StudioError> {
        if self.vertical().is_none() {
            return Err(fail("The vertical picture is off."));
        }
        self.view_of(window, rect, scale, true)
    }

    fn view_of(
        &self,
        window: raw_window_handle::RawWindowHandle,
        rect: crate::Rect,
        scale: f64,
        vertical: bool,
    ) -> Result<StageView, StudioError> {
        let shared = self.engine()?;
        let r = crate::with_edge(rect);
        let made = match window {
            #[cfg(target_os = "macos")]
            raw_window_handle::RawWindowHandle::AppKit(h) => {
                crate::ffi::mac::child_view(h.ns_view.as_ptr(), r.x, r.y, r.w, r.h, r.vh)
            }
            #[cfg(target_os = "windows")]
            raw_window_handle::RawWindowHandle::Win32(h) => {
                crate::ffi::win::child_view(h.hwnd.get() as *mut std::ffi::c_void, r.x, r.y, r.w, r.h)
            }
            _ => return Err(fail("that isn't this system's window")),
        };
        let view = made.ok_or_else(|| fail("the preview can only be made on the main thread"))?;
        let (cx, cy) = view.pixels().unwrap_or_else(|| crate::px(r, scale));
        let (size, scene) = if vertical {
            (crate::collection::VERTICAL_SIZE, self.vertical().map(|v| v.current))
        } else {
            ((self.canvas.width, self.canvas.height), None)
        };
        // SAFETY: the view lives as long as the StageView, which stops the engine drawing in it
        // before it goes (`Drop`).
        #[allow(unsafe_code)]
        let started = unsafe { shared.engine.start_preview_of(view.ptr(), cx, cy, size, scene) };
        let id = match started {
            Ok(id) => id,
            Err(e) => {
                view.remove();
                return Err(fail(format!("the preview couldn't be made: {e}")));
            }
        };
        let look = ViewLook {
            background: [10.0 / 255.0, 11.0 / 255.0, 13.0 / 255.0, 1.0],
            margin: crate::edge_px(scale) as f32,
            ppp: ppp(scale),
            ..Default::default()
        };
        // The engine draws at the view's size the moment it changes (AppKit following the
        // window included), not when the page next says.
        #[cfg(target_os = "macos")]
        {
            // Weak: AppKit can keep the view (and this) a moment after the preview is dropped,
            // and a strong one made the engine look busy, so a new picture size was refused as
            // "streaming or recording" when nothing was (owner, 11 Nov).
            let (engine, pid) = (Arc::downgrade(&shared), id);
            view.on_resize(Box::new(move |w, h| {
                if let Some(e) = engine.upgrade() {
                    e.engine.resize_preview(pid, w, h);
                }
            }));
        }
        let v = StageView { shared, id, view, look: RefCell::new(look), vertical };
        v.follow(self);
        Ok(v)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl StageView {
    /// Moves or resizes it (the page's box changed). Main thread.
    pub fn set_rect(&mut self, rect: crate::Rect, scale: f64) {
        // A box of the page's own (Studio Mode's halves): placed by the page only.
        #[cfg(target_os = "macos")]
        self.view.follow_window(false);
        let r = crate::with_edge(rect);
        place(&self.view, &r);
        {
            let mut l = self.look.borrow_mut();
            l.area = None;
            l.margin = crate::edge_px(scale) as f32;
            l.ppp = ppp(scale);
        }
        let (cx, cy) = self.view.pixels().unwrap_or_else(|| crate::px(r, scale));
        self.shared.engine.resize_preview(self.id, cx, cy);
        self.push();
    }

    /// Covers `area` (the page's whole stage) with the canvas at `canvas` inside it (points
    /// from the area's top left). Main thread.
    pub fn set_area(&mut self, area: crate::Rect, canvas: (f64, f64, f64, f64), scale: f64) {
        // The whole stage, between panels of fixed size: it follows the window itself. Not
        // the vertical picture's, which shares the stage's width with it.
        #[cfg(target_os = "macos")]
        self.view.follow_window(!self.vertical);
        place(&self.view, &area);
        let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        self.look.borrow_mut().area =
            Some(((canvas.0 * s) as f32, (canvas.1 * s) as f32, (canvas.2 * s) as f32, (canvas.3 * s) as f32));
        let (cx, cy) = self.view.pixels().unwrap_or_else(|| crate::px(area, scale));
        {
            let mut l = self.look.borrow_mut();
            l.area_for = Some(crate::px(area, scale));
            l.ppp = ppp(scale);
        }
        self.shared.engine.resize_preview(self.id, cx, cy);
        self.push();
    }

    /// Shows `scene` instead of what's on air (Studio Mode), or what's on air again.
    pub fn show_scene(&self, scene: Option<&str>) {
        self.look.borrow_mut().scene = scene.map(String::from);
        self.shared.engine.set_preview_scene(self.id, scene.map(String::from));
    }

    pub fn shown_scene(&self) -> Option<String> {
        self.look.borrow().scene.clone()
    }

    /// Outlines `ids` of `scene` with handles, or nothing (from the next tick).
    pub fn select(&self, pick: Option<(&str, &[i64])>) {
        self.look.borrow_mut().picked = pick.map(|(s, i)| (s.to_string(), i.to_vec()));
        self.push();
    }

    /// Outlines every source of `scene` in its own colour, or none (from the next tick).
    pub fn frames(&self, scene: Option<&str>) {
        self.look.borrow_mut().frames = scene.map(String::from);
        self.push();
    }

    /// Draws the picked source's distances to the canvas edges (while it's dragged), or not.
    pub fn guides(&self, on: bool) {
        if std::mem::replace(&mut self.look.borrow_mut().guides, on) != on {
            self.push();
        }
    }

    /// The colour round the canvas (0xAABBGGRR).
    pub fn background(&self, abgr: u32) {
        let c = |shift: u32| ((abgr >> shift) & 0xff) as f32 / 255.0;
        self.look.borrow_mut().background = [c(0), c(8), c(16), 1.0];
        self.view.set_backdrop(abgr);
        self.push();
    }

    /// A dark layer over the view, 0 to 1 (a dialog's backdrop).
    pub fn set_shade(&self, shade: f64) {
        self.look.borrow_mut().shade = shade.clamp(0.0, 1.0) as f32;
        self.push();
    }

    /// Leaves the picture out where the page's menus lie over it (points from the box's top
    /// left: x, y, w, h, corner radius). Main thread.
    pub fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
        let edge = if self.look.borrow().area.is_none() { crate::PREVIEW_EDGE } else { 0.0 };
        let moved: Vec<_> = holes.iter().map(|&(x, y, w, h, r)| (x + edge, y + edge, w, h, r)).collect();
        self.view.set_holes(&moved);
    }

    /// How much of the picture shows, 0 to 1. Main thread.
    pub fn set_opacity(&self, opacity: f64) {
        self.view.set_opacity(opacity);
    }

    /// Mouse actions over it go to `cb` (Windows: the picture's own window takes them; on a
    /// Mac the page gets them itself).
    pub fn set_input(&self, cb: Box<dyn Fn(crate::PreviewInput)>) {
        #[cfg(target_os = "windows")]
        self.view.set_input(cb);
        #[cfg(not(target_os = "windows"))]
        let _ = cb;
    }

    /// The cursor over it (Windows only), by its CSS name.
    pub fn set_cursor(&self, css: &str) {
        #[cfg(target_os = "windows")]
        self.view.set_cursor(css);
        #[cfg(not(target_os = "windows"))]
        let _ = css;
    }

    /// Keeps it with the stage (the app's tick): the vertical picture's scene on air, and the
    /// outlines taken from the items as they are now.
    pub fn follow(&self, stage: &Stage) {
        if self.vertical {
            self.shared.engine.set_preview_scene(self.id, stage.vertical().map(|v| v.current));
        }
        self.push();
    }

    /// Sends the look to the engine.
    fn push(&self) {
        let l = self.look.borrow();
        self.shared.engine.set_preview_look(
            self.id,
            ferrocore::preview::PreviewLook {
                background: l.background,
                area: l.area,
                area_for: l.area_for,
                margin: l.margin,
                outlines: Vec::new(),
                // The engine outlines them from its own items, with the picture's frame.
                marks: Some(ferrocore::preview::Marks {
                    scene: l.frames.clone(),
                    picked: l.picked.clone(),
                    colors: crate::ffi::FRAME_COLORS.iter().map(|&(r, g, b)| [r, g, b, 1.0]).collect(),
                }),
                shade: l.shade,
                guides: l.guides.then_some(l.ppp),
            },
        );
    }
}

/// Pixels per point from a page's scale (1 when it's nonsense).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn ppp(scale: f64) -> f32 {
    if scale.is_finite() && scale > 0.0 { scale as f32 } else { 1.0 }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Drop for StageView {
    fn drop(&mut self) {
        // The engine lets go of the view before it goes.
        self.shared.engine.stop_preview(self.id);
        self.view.remove();
    }
}

/// A projector: a window of its own (or a display, full screen) showing what's on air, a scene,
/// or a source alone, drawn by our engine. Closed when dropped.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug)]
pub struct StageProjector {
    shared: Arc<own::Shared>,
    id: u64,
    win: crate::ffi::projector_window::Win,
    /// A multiview's layout (shared with its window's clicks and resizes).
    multiview: Option<std::rc::Rc<RefCell<MultiviewState>>>,
}

/// What a multiview shows and where: the scenes, Studio Mode's preview, the look, the canvas
/// and the window's size in pixels.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone, PartialEq)]
struct MultiviewState {
    names: Vec<String>,
    preview: Option<String>,
    look: crate::MultiviewLook,
    canvas: (u32, u32),
    view: (u32, u32),
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl MultiviewState {
    /// Drawn at the window's shape, at most 1920 wide (each box is far smaller than a scene).
    fn size(&self) -> (u32, u32) {
        let (vw, vh) = (self.view.0.max(1) as f32, self.view.1.max(1) as f32);
        let k = (1920.0 / vw).min(1.0);
        (((vw * k).round() as u32).max(1), ((vh * k).round() as u32).max(1))
    }

    fn aspect(&self) -> f32 {
        self.canvas.0.max(1) as f32 / self.canvas.1.max(1) as f32
    }

    fn laid_out(&self) -> (Vec<crate::projector::Area>, Vec<crate::projector::Area>) {
        let (w, h) = self.size();
        let preview = self.preview.is_some() && self.look.layout != crate::MultiviewLayout::ScenesOnly;
        crate::projector::multiview_in(self.look.layout, preview, self.names.len(), w as f32, h as f32, self.aspect())
    }

    /// Studio Mode's preview first (green), then what's on air (red), then each scene (red
    /// while on air, green while previewed); names under them, safe areas in the big ones.
    fn boxes(&self) -> Vec<ferrocore::engine::MultiviewBox> {
        use ferrocore::engine::MultiviewBox;
        const RED: [u8; 4] = [214, 40, 40, 255];
        const GREEN: [u8; 4] = [40, 180, 80, 255];
        const GREY: [u8; 4] = [70, 70, 70, 255];
        let a = |r: &crate::projector::Area| (r.x, r.y, r.w, r.h);
        let label = |s: &str| self.look.labels.then(|| s.to_string());
        let marks = |r: &crate::projector::Area| {
            if self.look.safe_areas { crate::projector::safe_areas(*r).iter().map(a).collect() } else { Vec::new() }
        };
        let (bigs, cells) = self.laid_out();
        let mut out = Vec::new();
        let mut bigs = bigs.iter();
        if self.preview.is_some()
            && self.look.layout != crate::MultiviewLayout::ScenesOnly
            && let Some(r) = bigs.next()
        {
            out.push(MultiviewBox {
                scene: self.preview.clone(),
                area: a(r),
                frame: GREEN,
                live: None,
                marks: marks(r),
                label: label("Preview"),
            });
        }
        if let Some(r) = bigs.next() {
            out.push(MultiviewBox {
                scene: None,
                area: a(r),
                frame: RED,
                live: None,
                marks: marks(r),
                label: label("Program"),
            });
        }
        for (name, r) in self.names.iter().zip(&cells) {
            let frame = if self.preview.as_deref() == Some(name) { GREEN } else { GREY };
            out.push(MultiviewBox {
                scene: Some(name.clone()),
                area: a(r),
                frame,
                live: Some(RED),
                marks: Vec::new(),
                label: label(name),
            });
        }
        out
    }

    /// The scene a click at (`x`, `y`) in a `w`×`h` view lands on.
    fn hit(&self, x: f64, y: f64, w: f64, h: f64) -> Option<String> {
        let (mw, mh) = self.size();
        let (x, y) = ((x / w.max(1.0)) as f32 * mw as f32, (y / h.max(1.0)) as f32 * mh as f32);
        self.laid_out().1.iter().position(|r| r.contains(x, y)).and_then(|i| self.names.get(i).cloned())
    }

    fn apply(&self, shared: &own::Shared, id: u64) {
        shared.engine.set_preview_multiview(id, self.size(), self.canvas, self.boxes());
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Stage {
    /// Opens a projector of `of` on `screen` (full screen) or in a window; `on_close` runs when
    /// it's closed there (Esc, the window's button), `on_pick` with a scene clicked in a
    /// multiview, laid out as `look`. Main thread.
    pub fn projector(
        &self,
        of: &crate::ProjectorOf,
        screen: Option<usize>,
        look: crate::MultiviewLook,
        on_close: Box<dyn Fn()>,
        on_pick: Box<dyn Fn(String)>,
    ) -> Result<StageProjector, StudioError> {
        use crate::ProjectorOf as P;
        let shared = self.engine()?;
        let canvas = (self.canvas.width, self.canvas.height);
        let (title, scene, size) = match of {
            P::Program | P::Multiview => {
                let title = if *of == P::Multiview { "Multiview" } else { "Projector: what's on air" };
                (title.to_string(), None, canvas)
            }
            P::Source(name) => {
                let canvas = self.canvas_of(name);
                let (scene, w, h) = own::show_alone(&shared.engine, name, canvas)?;
                shared.keep_alone(name);
                (format!("Projector: {name}"), Some(scene), (w, h))
            }
        };
        let aspect = f64::from(size.0.max(1)) / f64::from(size.1.max(1));
        let multiview = (*of == P::Multiview).then(|| {
            std::rc::Rc::new(RefCell::new(MultiviewState {
                names: self.scenes().into_iter().map(|s| s.name).collect(),
                preview: self.preview_scene(),
                look,
                canvas,
                view: canvas,
            }))
        });
        // Resizes reach the engine once the preview is there (a multiview is laid out again).
        let id_cell = std::rc::Rc::new(std::cell::Cell::new(None::<u64>));
        let resize = {
            // Weak, as the stage view's: the window can outlive the projector a moment.
            let (weak, id_cell, mv) = (Arc::downgrade(&shared), id_cell.clone(), multiview.clone());
            Box::new(move |w: u32, h: u32| {
                let Some(shared) = weak.upgrade() else { return };
                if let Some(id) = id_cell.get() {
                    shared.engine.resize_preview(id, w, h);
                    if let Some(mv) = &mv {
                        mv.borrow_mut().view = (w, h);
                        mv.borrow().apply(&shared, id);
                    }
                }
            })
        };
        let click: Box<dyn Fn(f64, f64, f64, f64)> = match &multiview {
            Some(mv) => {
                let mv = mv.clone();
                Box::new(move |x, y, w, h| {
                    let hit = mv.borrow().hit(x, y, w, h);
                    if let Some(scene) = hit {
                        on_pick(scene);
                    }
                })
            }
            None => Box::new(|_, _, _, _| {}),
        };
        let hooks = crate::ffi::projector_window::Hooks { click, close: on_close, resize };
        let win = crate::ffi::projector_window::open(&title, aspect, screen, hooks)
            .ok_or_else(|| fail("The projector's window couldn't be made."))?;
        let (pw, ph) = win.pixels();
        // SAFETY: the window's view lives as long as the StageProjector, which stops the engine
        // drawing in it before the window goes (`Drop`).
        #[allow(unsafe_code)]
        let id = unsafe { shared.engine.start_preview_of(win.view_ptr(), pw, ph, size, scene) }
            .map_err(|e| fail(format!("the projector couldn't be drawn: {e}")))?;
        id_cell.set(Some(id));
        shared.engine.set_preview_look(
            id,
            ferrocore::preview::PreviewLook { background: [0.0, 0.0, 0.0, 1.0], ..Default::default() },
        );
        if let Some(mv) = &multiview {
            mv.borrow_mut().view = (pw, ph);
            mv.borrow().apply(&shared, id);
        }
        Ok(StageProjector { shared, id, win, multiview })
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl StageProjector {
    /// Its window's number (to screenshot it in checks).
    pub fn window_number(&self) -> isize {
        self.win.number()
    }

    /// Keeps the window above the others, or not.
    pub fn set_on_top(&self, on: bool) {
        self.win.set_on_top(on);
    }

    /// A multiview follows the scenes, Studio Mode's preview and its look (nothing to do when
    /// they're as they were, or for other projectors).
    pub fn update(&self, stage: &Stage, look: crate::MultiviewLook) {
        let Some(mv) = &self.multiview else { return };
        let names: Vec<String> = stage.scenes().into_iter().map(|s| s.name).collect();
        let next = MultiviewState { names, preview: stage.preview_scene(), look, ..mv.borrow().clone() };
        if *mv.borrow() != next {
            next.apply(&self.shared, self.id);
            *mv.borrow_mut() = next;
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Drop for StageProjector {
    fn drop(&mut self) {
        self.shared.engine.stop_preview(self.id);
        self.win.close();
    }
}

/// Where our engine draws no preview yet (Linux): the view can't be made (its calls are the
/// same, so the app builds everywhere).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
pub struct StageView(());

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[allow(clippy::unused_self)]
impl StageView {
    pub fn set_rect(&mut self, _rect: crate::Rect, _scale: f64) {}
    pub fn set_area(&mut self, _area: crate::Rect, _canvas: (f64, f64, f64, f64), _scale: f64) {}
    pub fn show_scene(&self, _scene: Option<&str>) {}
    pub fn shown_scene(&self) -> Option<String> {
        None
    }
    pub fn select(&self, _pick: Option<(&str, &[i64])>) {}
    pub fn frames(&self, _scene: Option<&str>) {}
    pub fn guides(&self, _on: bool) {}
    pub fn background(&self, _abgr: u32) {}
    pub fn set_shade(&self, _shade: f64) {}
    pub fn set_holes(&self, _holes: &[(f64, f64, f64, f64, f64)]) {}
    pub fn set_opacity(&self, _opacity: f64) {}
    pub fn set_input(&self, _cb: Box<dyn Fn(crate::PreviewInput)>) {}
    pub fn set_cursor(&self, _css: &str) {}
    pub fn follow(&self, _stage: &Stage) {}
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Stage {
    pub fn preview_drawn(
        &self,
        _window: raw_window_handle::RawWindowHandle,
        _rect: crate::Rect,
        _scale: f64,
    ) -> Result<StageView, StudioError> {
        Err(fail("Ferrocast's own engine draws the preview on a Mac and on Windows so far."))
    }

    pub fn vertical_preview_drawn(
        &self,
        window: raw_window_handle::RawWindowHandle,
        rect: crate::Rect,
        scale: f64,
    ) -> Result<StageView, StudioError> {
        self.preview_drawn(window, rect, scale)
    }
}

/// RGBA rows saved as a PNG.
/// Saves RGBA pixels as a PNG (any thread).
pub fn write_png(path: &Path, px: &[u8], (w, h): (u32, u32)) -> Result<(), StudioError> {
    let file = std::fs::File::create(path).map_err(|e| fail(format!("the screenshot couldn't be saved ({e})")))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut wr| wr.write_image_data(px))
        .map_err(|e| fail(format!("the screenshot couldn't be saved ({e})")))
}

/// A recording's next file name (as `Stage::record_path`).
fn path_for(canvas: Canvas, plan: &RecordPlan) -> PathBuf {
    let out = plan.encode.scale.unwrap_or((canvas.width, canvas.height));
    let video = NameVideo { fps: canvas.fps, canvas: (canvas.width, canvas.height), output: out };
    let first = plan.folder.join(file_name(&plan.name, plan.extension(), plan.spaces, video, chrono::Local::now()));
    if plan.overwrite || !first.exists() {
        return first;
    }
    let stem = first.file_stem().and_then(|s| s.to_str()).unwrap_or("Recording").to_string();
    let gap = if plan.spaces { " " } else { "_" };
    (2..)
        .map(|n| plan.folder.join(format!("{stem}{gap}({n}).{}", plan.extension())))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage() -> Stage {
        Stage::new(Canvas { width: 1920, height: 1080, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"))
    }

    /// Monitored sound plays from the moment the engine opens (not only after Settings are
    /// saved), on the device last picked, through each new engine.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn monitoring_plays_from_the_engine_start() {
        let mut s = stage();
        // No graphics card or no sound device here (some CI machines): nothing to check.
        let Ok(e) = s.engine() else { return };
        if ferrocore::monitor::devices().is_empty() {
            return;
        }
        assert!(e.engine.monitoring_stats().is_some(), "monitoring is on from the start");
        drop(e);
        // A new picture size makes a new engine: it monitors too.
        if s.set_canvas(Canvas { width: 1280, height: 720, fps: 30 }).is_ok()
            && let Ok(e) = s.engine()
        {
            assert!(e.engine.monitoring_stats().is_some(), "and after a new engine");
        }
    }

    #[test]
    fn sound_care_is_kept_in_the_document_and_checked() {
        let s = stage();
        assert_eq!(s.audio_care(), json!({ "ducking": null, "loudness_lufs": null }));
        let care =
            json!({ "ducking": { "voices": ["Mic"], "music": ["Music"], "depth_db": -15.0 }, "loudness_lufs": -14.0 });
        s.set_audio_care(&care).unwrap();
        assert_eq!(s.audio_care(), care);
        // Bad ones refused, the good one kept.
        let bad = [
            json!({ "ducking": { "voices": ["Mic"], "music": ["Mic"] } }),
            json!({ "ducking": { "voices": [], "music": [], "depth_db": 3.0 } }),
            json!({ "loudness_lufs": -2.0 }),
        ];
        for b in bad {
            assert!(s.set_audio_care(&b).is_err(), "{b}");
        }
        assert_eq!(s.audio_care(), care);
        // Off again.
        s.set_audio_care(&json!({})).unwrap();
        assert_eq!(s.audio_care(), json!({ "ducking": null, "loudness_lufs": null }));
    }

    #[test]
    fn the_privacy_guard_is_kept_in_the_document_and_checked() {
        let s = stage();
        let mac = cfg!(target_os = "macos");
        assert_eq!(s.privacy(), json!({ "hide_notifications": true, "hide_apps": [], "supported": mac }));
        s.set_privacy(&json!({ "hide_notifications": false, "hide_apps": ["Mail", " com.hnc.Discord ", "mail"] }))
            .unwrap();
        let kept = json!({ "hide_notifications": false, "hide_apps": ["Mail", "com.hnc.Discord"], "supported": mac });
        assert_eq!(s.privacy(), kept);
        assert_eq!(ferrocore::obs::privacy_hide(&s.doc.borrow()), vec!["Mail", "com.hnc.Discord"]);
        let many: Vec<String> = (0..33).map(|i| format!("App {i}")).collect();
        for b in [json!({ "hide_apps": [""] }), json!({ "hide_apps": [1] }), json!({ "hide_apps": many })] {
            assert!(s.set_privacy(&b).is_err(), "{b}");
        }
        assert_eq!(s.privacy(), kept);
        s.set_privacy(&json!({})).unwrap();
        assert_eq!(ferrocore::obs::privacy_hide(&s.doc.borrow()), vec![ferrocore::obs::NOTIFICATIONS.to_string()]);
    }

    /// Sound bars listen to the sound on air or a source with sound, never to themselves or
    /// to a picture; they aren't in the mixer.
    #[test]
    fn sound_bars_listen_to_sources_with_sound() {
        let s = stage();
        ok(s.add_source("Main", "ffmpeg_source", "Music", &json!({ "local_file": "/no/such.mp3" }), false));
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({}), false));
        // Silent kinds the mixer may still list (owner, 10 Nov): not offered.
        ok(s.add_source("Main", "ferrocast_web_text", "Viewers", &json!({}), false));
        ok(s.add_source("Main", "slideshow", "Photos", &json!({}), false));
        ok(s.add_source("Main", "ferrocast_visualizer", "Bars", &json!({}), false));
        let opts = ok(s.options("Bars"));
        let o = opts.iter().find(|o| o.key == "source").unwrap_or_else(|| panic!("a Listen to list"));
        assert_eq!(o.choices, ["Sound on air", "Music"]);
        assert_eq!(o.values, [json!(""), json!("Music")]);
        assert!(ok(s.audio("Main")).iter().all(|a| a.name != "Bars"), "not in the mixer");
    }

    /// Installed plugins' effects that are on are offered in the Effect list, after the ready-made.
    #[test]
    fn plugin_effects_are_in_the_effect_list() {
        let fx = "fn effect(uv: vec2<f32>) -> vec4<f32> { return pic(uv); }";
        ferrocore::shaderfx::set_plugins(vec![ferrocore::shaderfx::PluginEffect {
            id: "com.ana.same".into(),
            name: "Same".into(),
            code: fx.into(),
        }]);
        let opts = crate::kinds::options("ferrocast_shader").unwrap_or_default();
        let list = opts.iter().find(|o| o.key == "preset").unwrap_or_else(|| panic!("an Effect list"));
        assert_eq!(list.choices.first().map(String::as_str), Some("Your own"));
        assert_eq!(list.choices.last().map(String::as_str), Some("Same (plugin)"));
        assert_eq!(list.values.last(), Some(&json!("plugin:com.ana.same")));
        ferrocore::shaderfx::set_plugins(Vec::new());
        let opts = crate::kinds::options("ferrocast_shader").unwrap_or_default();
        let list = opts.iter().find(|o| o.key == "preset").unwrap_or_else(|| panic!("an Effect list"));
        assert!(!list.choices.iter().any(|c| c.ends_with("(plugin)")), "gone once it's off");
    }

    /// The top layer follows its scene's name and goes with it; only a scene can be one.
    #[test]
    fn the_top_layer_is_a_scene_and_follows_it() {
        let s = stage();
        ok(s.add_scene("Logo"));
        assert_eq!(s.top_layer(), None);
        assert!(s.set_top_layer(Some("Nope")).is_err(), "no such scene");
        ok(s.set_top_layer(Some("Logo")));
        assert_eq!(s.top_layer().as_deref(), Some("Logo"));
        ok(s.rename("Logo", "Brand"));
        assert_eq!(s.top_layer().as_deref(), Some("Brand"), "renamed with its scene");
        ok(s.remove_scene("Brand"));
        assert_eq!(s.top_layer(), None, "gone with its scene");
        ok(s.add_scene("Logo"));
        assert_eq!(s.top_layer(), None, "a new scene of the same name isn't the top layer again");
        ok(s.set_top_layer(Some("Logo")));
        ok(s.set_top_layer(None));
        assert_eq!(s.top_layer(), None);
    }

    /// Instant replay: a saved replay goes to every Instant Replay source; the first that asked
    /// names its scene to show; none set, a source not in a scene, or switch off: no scene.
    #[test]
    fn an_instant_replay_gets_each_new_replay() {
        let s = stage();
        let file = Path::new("/rec/Replay 1.mp4");
        assert_eq!(ok(s.show_replay(file)), None, "no replay source: nothing to do");
        ok(s.add_scene("Replay"));
        ok(s.add_source("Replay", "ferrocast_replay", "Instant replay", &json!({}), false));
        ok(s.add_source("Main", "ferrocast_replay", "Corner replay", &json!({ "switch": false }), false));
        assert_eq!(ok(s.settings("Instant replay"))["local_file"], json!(""), "clear until one is saved");
        assert_eq!(ok(s.show_replay(file)), Some(("Instant replay".into(), "Replay".into())));
        for name in ["Instant replay", "Corner replay"] {
            assert_eq!(ok(s.settings(name))["local_file"], json!("/rec/Replay 1.mp4"), "{name} plays it");
        }
        // The next one replaces it.
        ok(s.show_replay(Path::new("/rec/Replay 2.mp4")));
        assert_eq!(ok(s.settings("Corner replay"))["local_file"], json!("/rec/Replay 2.mp4"));
        // Switch off everywhere: it plays where it is, no scene goes on air.
        ok(s.update("Instant replay", &json!({ "switch": false })));
        assert_eq!(ok(s.show_replay(file)), None);
        // It has sound: it's in the mixer, and it's media (its play controls show).
        let item = ok(s.items("Main")).into_iter().find(|i| i.name == "Corner replay");
        assert!(item.is_some_and(|i| i.media), "an instant replay has play controls");
        let opts = crate::kinds::options("ferrocast_replay").unwrap_or_default();
        assert_eq!(opts.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["switch"], "its file isn't in its form");
    }

    /// A clone's Source list holds every other source, then the scenes, never itself.
    #[test]
    fn a_clone_picks_from_this_collection() {
        let s = stage();
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "width": 20, "height": 20 }), false));
        ok(s.add_source("Main", "ferrocast_clone", "Copy", &json!({}), false));
        let opts = ok(s.options("Copy"));
        let pick = opts.iter().find(|o| o.key == "clone").unwrap_or_else(|| panic!("a Source list"));
        assert_eq!(pick.choices, ["Red", "Main"], "the other source, then the scene; not itself");
        assert_eq!(pick.values, [json!("Red"), json!("Main")]);
        assert!(opts.iter().any(|o| o.key == "filters" && o.kind == "bool"));
        // The app's way: the kind's form read on its own (empty list), then filled from the
        // collection (owner, 10 Nov: the drawer said "Nothing to choose").
        let mut read = crate::kinds::options("ferrocast_clone").unwrap_or_default();
        assert!(read.iter().find(|o| o.key == "clone").is_some_and(|o| o.choices.is_empty()));
        ok(s.fill_choices("Copy", &mut read));
        let pick = read.iter().find(|o| o.key == "clone").unwrap_or_else(|| panic!("a Source list"));
        assert_eq!(pick.choices, ["Red", "Main"]);
        // Its defaults: nothing picked yet, the source's filters included.
        assert_eq!(ok(s.settings("Copy"))["filters"], json!(true));
        // A countdown is silent too.
        ok(s.add_source("Main", "ferrocast_countdown", "Timer", &json!({}), false));
        assert!(ok(s.audio("Main")).iter().all(|a| a.name != "Timer"), "a countdown isn't in the mixer");
        // No sound of its own: not in the mixer (found in the real app, 10 Nov).
        let item = ok(s.items("Main")).into_iter().find(|i| i.name == "Copy").unwrap_or_else(|| panic!("the copy"));
        assert!(!item.audio, "a copy has no sound of its own");
        assert!(ok(s.audio("Main")).iter().all(|a| a.name != "Copy"), "not in the mixer");
    }

    fn ok<T>(r: Result<T, StudioError>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn scenes_are_made_ordered_renamed_and_removed() {
        let s = stage();
        ok(s.add_scene("Two"));
        ok(s.add_scene("Three"));
        assert!(s.add_scene("Two").is_err(), "names are unique");
        assert_eq!(s.scenes().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Main", "Two", "Three"]);
        ok(s.move_scene("Three", 0));
        ok(s.set_current("Two"));
        assert_eq!(s.current(), "Two");
        ok(s.rename("Two", "Second"));
        assert_eq!(s.current(), "Second", "on air keeps its new name");
        assert_eq!(s.scenes().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Three", "Main", "Second"]);
        ok(s.remove_scene("Second"));
        assert_eq!(s.current(), "Three", "the first scene goes on air instead");
        ok(s.remove_scene("Main"));
        assert!(s.remove_scene("Three").is_err(), "one scene is always kept");
    }

    #[test]
    fn items_are_placed_moved_and_ordered() {
        let s = stage();
        let red = ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "width": 200, "height": 100 }), false));
        let blue = ok(s.add_source("Main", "color_source_v3", "Blue", &json!({ "width": 50, "height": 50 }), false));
        assert_eq!(red.frame, [200.0, 0.0, 0.0, 100.0, 0.0, 0.0], "its own size at the top left");
        assert_eq!(ok(s.item_ids("Main")), [blue.id, red.id], "the newest on top");
        let box_ = ok(s.set_item_box("Main", red.id, 100.0, 50.0, 400.0, 200.0));
        assert_eq!(box_.frame, [400.0, 0.0, 0.0, 200.0, 100.0, 50.0], "dragged and sized");
        ok(s.edit_item("Main", red.id, ItemEdit::Top));
        assert_eq!(ok(s.item_ids("Main")), [red.id, blue.id]);
        ok(s.edit_item("Main", blue.id, ItemEdit::Fit));
        let fitted = ok(s.items("Main")).into_iter().find(|i| i.id == blue.id).map(|i| i.frame);
        assert_eq!(fitted, Some([1080.0, 0.0, 0.0, 1080.0, 420.0, 0.0]), "a square fits the screen's height, centred");
        ok(s.edit_item("Main", red.id, ItemEdit::Hide));
        assert!(!ok(s.items("Main"))[0].visible);
        ok(s.edit_item("Main", red.id, ItemEdit::Duplicate));
        assert!(s.sources().iter().any(|u| u.name == "Red 2"), "a copy of the source");
        ok(s.edit_item("Main", blue.id, ItemEdit::Remove));
        assert_eq!(ok(s.items("Main")).len(), 2);
        assert!(!s.sources().iter().any(|u| u.name == "Blue"), "its last item gone, the source goes too");
        assert!(s.add_existing("Main", "Blue").is_err());
        ok(s.add_source("Main", "color_source_v3", "Blue", &json!({}), false));
    }

    #[test]
    fn the_frame_rate_is_the_last_seconds() {
        let s = stage();
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        // A busy start (half the frames skipped) shows at first, with nothing before it…
        assert_eq!(s.fps_now(1000, 1000, at(0)), 15.0);
        // …then each second's own rate, read again no sooner than a second later.
        assert_eq!(s.fps_now(1015, 1000, at(500)), 15.0);
        assert_eq!(s.fps_now(1030, 1000, at(1000)), 30.0);
        assert_eq!(s.fps_now(1050, 1010, at(2000)), 20.0);
        // Never above the canvas's rate; a new engine starts its count again.
        assert_eq!(s.fps_now(1200, 1010, at(3000)), 30.0);
        assert_eq!(s.fps_now(10, 0, at(4000)), 30.0);
    }

    #[test]
    fn a_copy_of_a_source_has_its_size() {
        let s = stage();
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "width": 200, "height": 100 }), false));
        let copy = ok(s.add_source("Main", "ferrocast_clone", "Copy", &json!({ "clone": "Red" }), false));
        assert_eq!(copy.frame, [200.0, 0.0, 0.0, 100.0, 0.0, 0.0], "the copied one's size, not 0×0");
        let sized = ok(s.set_item_box("Main", copy.id, 10.0, 20.0, 400.0, 200.0));
        assert_eq!(sized.frame, [400.0, 0.0, 0.0, 200.0, 10.0, 20.0], "it can be sized");
        // A copy of a copy, and one of itself (nothing to size by).
        let again = ok(s.add_source("Main", "ferrocast_clone", "Copy 2", &json!({ "clone": "Copy" }), false));
        assert_eq!(again.frame[0], 200.0);
        let looped = ok(s.add_source("Main", "ferrocast_clone", "Loop", &json!({ "clone": "Loop" }), false));
        assert_eq!(looped.frame[0], 0.0);
    }

    #[test]
    fn a_source_goes_when_no_scene_shows_it() {
        let s = stage();
        ok(s.add_scene("Two"));
        let red = ok(s.add_source("Main", "color_source_v3", "Red", &json!({}), false));
        ok(s.add_existing("Two", "Red"));
        ok(s.add_source("Two", "ferrocast_clone", "Copy", &json!({ "clone": "Red" }), false));
        let before = s.snapshot();
        ok(s.edit_item("Main", red.id, ItemEdit::Remove));
        assert!(s.sources().iter().any(|u| u.name == "Red"), "still shown in Two");
        let two = ok(s.items("Two"));
        let red_in_two = two.iter().find(|i| i.name == "Red").map(|i| i.id).unwrap_or_default();
        ok(s.edit_item("Two", red_in_two, ItemEdit::Remove));
        assert!(s.sources().iter().any(|u| u.name == "Red"), "the Copy in Two still shows it");
        ok(s.remove_scene("Two"));
        let names: Vec<String> = s.sources().into_iter().map(|u| u.name).collect();
        assert!(!names.iter().any(|n| n == "Red" || n == "Copy"), "nothing shows them now: {names:?}");
        s.restore_all(&before);
        assert!(s.sources().iter().any(|u| u.name == "Red"), "undo brings it back");
        // A collection that kept one anyway (older Ferrocast) loses it when opened.
        let mut doc = s.snapshot();
        if let Some(l) = doc["sources"].as_array_mut() {
            l.push(json!({ "id": "color_source_v3", "name": "Left over" }));
        }
        let again = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &doc, "Main"));
        assert!(!again.sources().iter().any(|u| u.name == "Left over"));
    }

    #[test]
    fn sound_and_filters_are_kept_as_obs_saves_them() {
        let s = stage();
        ok(s.add_source("Main", "coreaudio_input_capture", "Mic", &json!({}), false));
        ok(s.set_volume("Mic", -6.0));
        ok(s.set_tracks("Mic", &[1, 3]));
        ok(s.set_sync("Mic", 120));
        ok(s.set_mono("Mic", true));
        ok(s.set_monitor("Mic", Monitor::MeAndStream));
        let a = ok(s.audio_of("Mic"));
        assert!((a.volume_db + 6.0).abs() < 0.01 && a.tracks == [1, 3] && a.sync_ms == 120 && a.mono);
        assert_eq!(a.monitor, Monitor::MeAndStream);
        let saved = ok(s.source("Mic"));
        assert_eq!(
            (saved["mixers"].as_u64(), saved["sync"].as_i64(), saved["monitoring_type"].as_u64()),
            (Some(5), Some(120_000_000), Some(2))
        );
        ok(s.add_filter("Mic", "noise_gate_filter", "Gate", &json!({ "open_threshold": -26.0 })));
        ok(s.add_filter("Mic", "compressor_filter", "Comp", &json!({})));
        ok(s.move_filter("Mic", "Comp", ItemEdit::Top));
        ok(s.set_filter_enabled("Mic", "Gate", false));
        ok(s.update_filter("Mic", "Gate", &json!({ "close_threshold": -32.0 })));
        let f = ok(s.filters("Mic"));
        assert_eq!((f[0].name.as_str(), f[1].name.as_str(), f[1].enabled), ("Comp", "Gate", false));
        assert_eq!(ok(s.filter_settings("Mic", "Gate"))["open_threshold"], -26.0, "the rest kept");
        ok(s.remove_filter("Mic", "Comp"));
        assert_eq!(ok(s.filters("Mic")).len(), 1);
    }

    #[test]
    fn what_it_saves_our_engine_reads() {
        let s = stage();
        let item = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Card",
            &json!({ "color": 4278190335u32, "width": 100, "height": 50 }),
            false,
        ));
        ok(s.set_item_box("Main", item.id, 10.0, 20.0, 200.0, 100.0));
        ok(s.add_filter("Card", "ferrocast_rounded_corners", "Round", &json!({ "radius": 8 })));
        ok(s.set_transition(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }));
        let doc = s.save("Mine");
        assert_eq!(s.transition().kind, "cut_transition");
        let got = ferrocore::obs::import(&doc, (1920, 1080));
        assert!(got.skipped.is_empty(), "nothing left out: {:?}", got.skipped);
        assert_eq!(got.current.as_deref(), Some("Main"));
        let main = got.scenes.iter().find(|(n, _)| n == "Main").map(|(_, i)| i.clone()).unwrap_or_default();
        assert_eq!(main.len(), 1);
        assert_eq!((main[0].placement.x, main[0].placement.y, main[0].placement.scale_x), (10.0, 20.0, 2.0));
        assert_eq!(main[0].effects.len(), 1, "the filter came along");
        // And it opens again as it was.
        let again = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &doc, "Scene"));
        assert_eq!(ok(again.items("Main"))[0].frame, [200.0, 0.0, 0.0, 100.0, 10.0, 20.0]);
    }

    #[test]
    fn our_engine_draws_each_edit_at_once() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let Ok(e) = s.engine() else { return };
        let red = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Red",
            &json!({ "color": 4278190335u32, "width": 320, "height": 180 }),
            false,
        ));
        let px = |x: usize, y: usize| {
            e.engine.snapshot().map(|p| p[(y * 320 + x) * 4..(y * 320 + x) * 4 + 3].to_vec()).unwrap_or_default()
        };
        assert_eq!(px(10, 10), [255, 0, 0], "drawn as soon as it's added");
        ok(s.set_item_box("Main", red.id, 160.0, 0.0, 160.0, 180.0));
        assert_eq!(px(10, 10), [0, 0, 0], "moved away from the left half");
        assert_eq!(px(300, 10), [255, 0, 0]);
        ok(s.edit_item("Main", red.id, ItemEdit::Hide));
        assert_eq!(px(300, 10), [0, 0, 0], "hidden");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn remove_background_takes_out_what_isnt_a_person() {
        let s = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        if s.engine().is_err() {
            return;
        }
        // Green, with no one in it: once Vision has looked, it's all background.
        ok(s.add_source("Main", "color_source_v3", "Card", &json!({ "color": 4278255360u32 }), false));
        ok(s.add_filter("Card", "ferrocast_remove_background", "Cut", &json!({})));
        assert_eq!(s.cutouts.borrow().len(), 1, "a worker for the filter");
        let path = std::env::temp_dir().join(format!("ferrocast-cutout-{}.png", std::process::id()));
        let mut gone = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            ok(s.screenshot_program(&path));
            let img = image_px(&path);
            if img.get(1).is_some_and(|g| *g < 10) {
                gone = true;
                break;
            }
        }
        let _ = std::fs::remove_file(&path);
        assert!(gone, "the background went");
        ok(s.remove_filter("Card", "Cut"));
        assert!(s.cutouts.borrow().is_empty(), "its worker stops with it");
    }

    #[cfg(target_os = "macos")]
    fn image_px(path: &Path) -> Vec<u8> {
        let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
        let Ok(mut r) = png::Decoder::new(std::io::BufReader::new(f)).read_info() else { return Vec::new() };
        let mut px = vec![0; r.output_buffer_size()];
        r.next_frame(&mut px).map(|_| px).unwrap_or_default()
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn records_splits_and_replays_with_no_libobs() {
        let s = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        if s.engine().is_err() {
            return;
        }
        ok(s.add_source("Main", "color_source_v3", "Card", &json!({ "color": 4278255360u32 }), false));
        let dir = std::env::temp_dir().join(format!("ferrocast-stage-{}", std::process::id()));
        let plan = RecordPlan {
            folder: dir.clone(),
            name: "Take %hh%mm%ss".into(),
            format: "mkv".into(),
            encode: crate::EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc"),
            ..RecordPlan::default()
        };
        let mut rec = ok(s.record(&plan, None));
        let first = rec.path().to_path_buf();
        assert!(first.starts_with(&dir) && first.extension().is_some_and(|e| e == "mkv"), "{}", first.display());
        let replay = ok(s.replay(&ReplayPlan {
            folder: dir.clone(),
            name: "Replay %hh%mm%ss".into(),
            format: "mp4".into(),
            spaces: true,
            seconds: 5,
            mb: 0,
            tracks: vec![1],
            encode: plan.encode.clone(),
        }));
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // What's written so far shows in its stats (the Stats panel's size and rate).
        let stats = rec.record_stats();
        assert!(stats.active && stats.bytes > 0, "bytes written are counted: {stats:?}");
        ok(rec.split());
        let second = rec.path().to_path_buf();
        assert_ne!(first, second, "a new file, named again (\" (2)\" if in the same second)");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        ok(replay.save_replay());
        drop(rec);
        for _ in 0..50 {
            if replay.last_replay().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let saved = replay.last_replay().unwrap_or_else(|| panic!("the replay was saved: {:?}", replay.last_error()));
        for f in [&first, &second, &saved] {
            assert!(std::fs::metadata(f).is_ok_and(|m| m.len() > 1000), "{} was written", f.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_multiview_boxes_each_scene_and_answers_clicks() {
        let mut mv = MultiviewState {
            names: vec!["A".into(), "B".into()],
            preview: None,
            look: crate::MultiviewLook::default(),
            canvas: (1920, 1080),
            view: (3840, 2160),
        };
        assert_eq!(mv.size(), (1920, 1080), "drawn at most 1920 wide");
        let b = mv.boxes();
        assert_eq!(b.len(), 3, "what's on air, then each scene");
        assert_eq!((b[0].scene.as_deref(), b[0].label.as_deref()), (None, Some("Program")));
        assert_eq!(b[2].scene.as_deref(), Some("B"));
        assert!(b[1].live.is_some(), "a scene box turns red while on air");
        // A click in the middle of B's box (in a view half the size) picks B.
        let (x, y, w, h) = b[2].area;
        let hit = mv.hit(f64::from((x + w / 2.0) / 2.0), f64::from((y + h / 2.0) / 2.0), 960.0, 540.0);
        assert_eq!(hit.as_deref(), Some("B"));
        assert_eq!(mv.hit(1.0, 1.0, 960.0, 540.0), None, "the program box isn't a scene to pick");
        // Studio Mode: its preview first, framed green, safe areas when asked, no names.
        mv.preview = Some("B".into());
        mv.look = crate::MultiviewLook { labels: false, safe_areas: true, ..mv.look };
        let b = mv.boxes();
        assert_eq!(b.len(), 4);
        assert_eq!((b[0].scene.as_deref(), b[0].marks.len()), (Some("B"), 3));
        assert!(b.iter().all(|x| x.label.is_none()));
        assert_eq!(b[3].frame, b[0].frame, "the previewed scene's box is green too");
        // Scenes only: no big boxes.
        mv.look.layout = crate::MultiviewLayout::ScenesOnly;
        assert_eq!(mv.boxes().len(), 2);
    }

    #[test]
    fn studio_mode_previews_then_sends_on_air() {
        let s = stage();
        ok(s.add_scene("Next"));
        assert_eq!(s.preview_scene(), None);
        assert!(s.set_preview("Next").is_err(), "Studio Mode is off");
        s.set_studio_mode(true);
        assert_eq!(s.preview_scene().as_deref(), Some("Main"));
        ok(s.set_preview("Next"));
        ok(s.transition_now(None));
        assert_eq!((s.current().as_str(), s.preview_scene().as_deref()), ("Next", Some("Main")), "swapped");
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        assert_eq!(s.current(), "Main");
        let saved = s.save("x");
        assert_eq!(saved["current_scene"], "Next", "OBS keeps Studio Mode's preview as current_scene");
        s.set_studio_mode(false);
        assert_eq!(s.preview_scene(), None);
    }

    /// Studio Mode as OBS has it (Duplicate Scene): the same scene in the preview and on air,
    /// an edit in the preview doesn't move it on air until the transition (owner, 11 Nov).
    #[test]
    fn studio_mode_edits_wait_for_the_transition() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let red = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Red",
            &json!({ "color": 0xFF00_00FFu32, "width": 40, "height": 40 }),
            false,
        ));
        ok(s.set_item_box("Main", red.id, 0.0, 0.0, 40.0, 40.0));
        s.set_studio_mode(true);
        let copy = |s: &Stage| s.snapshot().pointer("/ferrocast/program_copy/items/0/pos/x").and_then(Value::as_f64);
        assert_eq!(copy(&s), Some(0.0), "on air: a copy of the scene");
        ok(s.set_item_box("Main", red.id, 200.0, 0.0, 40.0, 40.0));
        assert_eq!(copy(&s), Some(0.0), "the preview's edit isn't on air yet");
        let engine = s.engine().is_ok();
        let red_at = |s: &Stage, x: u32| {
            let (px, (w, _)) = s.screenshot_program_pixels().unwrap_or_else(|e| panic!("{e}"));
            let i = ((10 * w + x) * 4) as usize;
            px[i] > 200 && px[i + 1] < 60
        };
        if engine {
            assert!(red_at(&s, 10) && !red_at(&s, 210), "on air: still where it was");
        }
        // The same scene in the preview and on air: the transition sends the edit.
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        assert_eq!(copy(&s), Some(200.0));
        assert_eq!(s.current(), "Main");
        if engine {
            assert!(red_at(&s, 210) && !red_at(&s, 10), "on air: where the preview put it");
        }
        // A source taken out of the preview stays on air until the next transition.
        ok(s.edit_item("Main", red.id, ItemEdit::Remove));
        assert!(s.sources().iter().any(|u| u.name == "Red"), "kept for the copy on air");
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        ok(s.rename("Main", "Show"));
        assert_eq!(s.snapshot().pointer("/ferrocast/program_copy/scene").and_then(Value::as_str), Some("Show"));
        // Off: what's on air is the scene itself again.
        s.set_studio_mode(false);
        assert!(s.snapshot().pointer("/ferrocast/program_copy").is_none());
    }

    #[test]
    fn groups_keep_their_items_where_they_show() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 50, "height": 50 }), false));
        ok(s.set_item_box("Main", a.id, 100.0, 100.0, 100.0, 100.0));
        ok(s.set_item_box("Main", b.id, 300.0, 150.0, 50.0, 50.0));
        let g = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        assert_eq!(g.frame, [250.0, 0.0, 0.0, 100.0, 100.0, 100.0], "the box round both");
        assert_eq!(ok(s.items("Main")).len(), 1);
        // Our engine reads the group with both inside, each where it was.
        let got = ferrocore::obs::import(&s.save("x"), (1920, 1080));
        assert!(got.groups.iter().any(|n| n == "Pair") && got.skipped.is_empty());
        ok(s.ungroup("Main", g.id));
        let back: Vec<[f32; 6]> = ok(s.items("Main")).iter().map(|i| i.frame).collect();
        assert!(
            back.contains(&[100.0, 0.0, 0.0, 100.0, 100.0, 100.0])
                && back.contains(&[50.0, 0.0, 0.0, 50.0, 300.0, 150.0]),
            "{back:?}"
        );
        assert!(!s.sources().iter().any(|u| u.name == "Pair"), "the empty group went");
    }

    #[test]
    fn undo_puts_the_whole_collection_back() {
        let s = stage();
        let before = s.snapshot();
        ok(s.add_source("Main", "color_source_v3", "A", &json!({}), false));
        ok(s.add_scene("Two"));
        s.restore_all(&before);
        assert_eq!(s.scenes().len(), 1);
        assert!(ok(s.items("Main")).is_empty());
    }

    #[test]
    fn a_new_source_starts_from_its_kinds_defaults() {
        let s = stage();
        let c = ok(s.add_source("Main", "color_source_v3", "Back", &json!({ "color": 4278190080u32 }), false));
        assert_eq!(c.frame[0..4], [1920.0, 0.0, 0.0, 1080.0], "no size given: the canvas's");
        let set = ok(s.settings("Back"));
        assert_eq!((set["color"].as_u64(), set["width"].as_u64()), (Some(4278190080), Some(0)), "given, then defaults");
        assert!(ok(s.options("Back")).iter().any(|o| o.key == "color"));
        ok(s.add_filter("Back", "chroma_key_filter_v2", "Key", &json!({})));
        assert!(ok(s.filter_options("Back", "Key")).iter().any(|o| o.key == "similarity"));
        // What OBS left unsaved reads as its default (a crop saved by OBS has no "relative").
        ok(s.add_filter("Back", "crop_filter", "Crop", &json!({})));
        ok(s.with_filters("Back", |l| l.iter_mut().for_each(|f| f["settings"] = json!({ "left": 28 }))));
        let crop = ok(s.filter_settings("Back", "Crop"));
        assert_eq!((crop["left"].as_i64(), crop["relative"].as_bool()), (Some(28), Some(true)));
    }

    #[test]
    fn a_cropped_source_is_outlined_at_its_cropped_size() {
        let s = stage();
        ok(s.add_source("Main", "color_source_v3", "Card", &json!({ "width": 1920, "height": 1080 }), false));
        let frame = || ok(s.items("Main")).into_iter().find(|i| i.name == "Card").map(|i| i.frame).unwrap_or_default();
        assert_eq!(frame()[0..4], [1920.0, 0.0, 0.0, 1080.0]);
        ok(s.add_filter("Card", "crop_filter", "Crop", &json!({ "left": 100, "right": 20, "top": 30 })));
        assert_eq!(frame()[0..4], [1800.0, 0.0, 0.0, 1050.0], "the crop's size, not the whole picture's");
        // Switched off, the whole picture again.
        ok(s.with_filters("Card", |l| l.iter_mut().for_each(|f| f["enabled"] = json!(false))));
        assert_eq!(frame()[0..4], [1920.0, 0.0, 0.0, 1080.0]);
    }

    #[test]
    fn the_vertical_picture_keeps_its_own_scenes() {
        let s = stage();
        assert!(s.vertical().is_none());
        ok(s.start_vertical("Tall"));
        ok(s.add_vertical_scene("Tall 2"));
        ok(s.set_vertical_current("Tall 2"));
        let v = s.vertical().unwrap_or_else(|| panic!("on"));
        assert_eq!((v.width, v.height, v.current.as_str(), v.scenes.len()), (1080, 1920, "Tall 2", 2));
        assert_eq!(s.scenes().len(), 1, "the main picture's list stays its own");
        assert!(s.set_vertical_current("Main").is_err());
        s.stop_vertical();
        assert!(s.vertical().is_none());
        ok(s.start_vertical("Other"));
        assert_eq!(s.vertical().map(|v| v.scenes.len()), Some(2), "back as it was");
    }

    #[test]
    fn missing_files_are_found_and_replaced() {
        let s = stage();
        ok(s.add_source("Main", "image_source", "Logo", &json!({ "file": "/nowhere/logo.png" }), false));
        ok(s.add_source(
            "Main",
            "ffmpeg_source",
            "Web",
            &json!({ "is_local_file": false, "local_file": "/nowhere/x.mp4" }),
            false,
        ));
        let m = s.missing_files();
        assert_eq!(m.len(), 1, "a network address isn't a file: {m:?}");
        let here = std::env::temp_dir();
        ok(s.replace_file(&m[0], &here.display().to_string()));
        assert!(s.missing_files().is_empty());
    }

    #[test]
    fn a_global_channel_comes_and_goes() {
        let s = stage();
        ok(s.set_channel(4, Some(("coreaudio_input_capture", "Mic/Aux", &json!({})))));
        assert_eq!(s.channels().first().map(|c| (c.channel, c.name.clone())), Some((Some(4), "Mic/Aux".to_string())));
        ok(s.set_volume("Mic/Aux", -3.0));
        assert!((ok(s.audio_of("Mic/Aux")).volume_db + 3.0).abs() < 0.01);
        ok(s.set_channel(4, None));
        assert!(s.channels().is_empty());
    }

    #[test]
    fn studio_extras_are_kept_in_the_collection() {
        let s = stage();
        ok(s.add_scene("Two"));
        let quick = vec![TransitionInfo { kind: "fade_transition".into(), ms: 900 }];
        ok(s.set_quick_transitions(&quick));
        ok(s.set_scene_transition("Two", Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        ok(s.set_transition_settings("fade_to_color_transition", &json!({ "color": 4278190080u32 })));
        let again = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &s.save("x"), "Scene"));
        assert_eq!(again.quick_transitions(), quick);
        assert_eq!(ok(again.scene_transition_of("Two")).map(|t| t.kind), Some("cut_transition".into()));
        assert_eq!(again.transition_settings("fade_to_color_transition")["color"], 4278190080u32);
        assert_eq!(ok(again.scene_transition_of("Main")), None);
    }

    #[test]
    fn fade_to_black_and_back() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let Ok(e) = s.engine() else { return };
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "color": 4278190335u32 }), false));
        ok(s.set_transition(&TransitionInfo { kind: "fade_transition".into(), ms: 300 }));
        let px = || {
            e.engine.snapshot().map(|p| p[(90 * 320 + 160) * 4..(90 * 320 + 160) * 4 + 3].to_vec()).unwrap_or_default()
        };
        assert_eq!(px(), [255, 0, 0]);
        assert!(ok(s.fade_to_black()) && s.is_black());
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(px(), [0, 0, 0], "black once the fade is over");
        // An edit meanwhile doesn't bring the picture back.
        ok(s.add_scene("Other"));
        assert_eq!(px(), [0, 0, 0]);
        assert!(!ok(s.fade_to_black()));
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(px(), [255, 0, 0], "back");
    }

    #[test]
    fn face_follow_fills_the_height_looks_and_goes_back() {
        let s = Stage::new(Canvas { width: 1080, height: 1920, fps: 30 }, "Tall").unwrap_or_else(|e| panic!("{e}"));
        if s.engine().is_err() {
            return;
        }
        let cam = ok(s.add_source(
            "Tall",
            "color_source_v3",
            "Cam",
            &json!({ "color": 4278255360u32, "width": 1920, "height": 1080 }),
            false,
        ));
        let mut f = ok(s.start_follow("Tall", cam.id, false));
        let t = ok(s.transform("Tall", cam.id));
        assert!((t.scale_y - 1920.0 / 1080.0).abs() < 1e-3, "fills the height");
        assert!((t.x - (540.0 - 1920.0 * t.scale_x / 2.0)).abs() < 0.5, "centred");
        let look = ok(s.follow_look(&f)).unwrap_or_else(|| panic!("a picture to look at"));
        assert_eq!((look.width, look.height, look.rgba.len()), (320, 180, 320 * 180 * 4));
        assert_eq!(&look.rgba[0..3], &[0, 255, 0], "the source itself");
        // A face at the left edge: the item steps right (never past its edge).
        let face = crate::Face { x: 0.0, y: 0.4, w: 0.1, h: 0.2 };
        ok(s.follow_step(&mut f, Some(face)));
        assert!(ok(s.transform("Tall", cam.id)).x > t.x);
        s.stop_follow(&f);
        assert_eq!(ok(s.transform("Tall", cam.id)).scale_x, 1.0, "back as it was");
    }

    #[test]
    fn renaming_a_vertical_scene_keeps_it_vertical() {
        let s = stage();
        ok(s.start_vertical("Tall"));
        ok(s.set_scene_transition("Main", Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        ok(s.rename("Tall", "Tall 2"));
        assert_eq!(s.vertical().map(|v| v.current), Some("Tall 2".into()));
        assert_eq!(s.scenes().len(), 1, "still not in the main list");
        ok(s.rename("Main", "First"));
        assert!(ok(s.scene_transition_of("First")).is_some(), "its override came along");
    }
}
