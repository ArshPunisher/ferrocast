//! The Studio's scenes kept by Ferrocast itself, with no libobs: the scene collection is a
//! document in OBS's own format (what `Collection::save` writes and OBS reads), edited here and
//! handed to Ferrocast's own engine after each change, which draws it (unchanged sources keep
//! running). The same calls as `Collection`, returning the same answers, so the app can move
//! over call by call (ENGINE.md, step 3).

use std::cell::RefCell;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::collection::{
    AudioInfo, BLENDS, BOUNDS, FilterInfo, ItemEdit, ItemInfo, Monitor, SCALE_FILTERS, SceneInfo, SourceUse, Transform,
    TransitionInfo,
};
use crate::{Canvas, StudioError, check_name, own};

/// OBS's global sound channels, in its saved collection: Desktop Audio 1-2, Mic/Aux 1-4.
const CHANNEL_KEYS: [&str; 6] = [
    "DesktopAudioDevice1",
    "DesktopAudioDevice2",
    "AuxAudioDevice1",
    "AuxAudioDevice2",
    "AuxAudioDevice3",
    "AuxAudioDevice4",
];

/// The scene store.
#[derive(Debug)]
pub struct Stage {
    canvas: Canvas,
    /// The collection, as OBS saves one.
    doc: RefCell<Value>,
    /// Ferrocast's own engine with it open, once something asked for it.
    engine: RefCell<Option<Arc<own::Shared>>>,
}

fn fail(msg: impl Into<String>) -> StudioError {
    StudioError::Failed(msg.into())
}

fn name_of(s: &Value) -> &str {
    s.get("name").and_then(Value::as_str).unwrap_or_default()
}

fn kind_of(s: &Value) -> &str {
    s.get("versioned_id").or_else(|| s.get("id")).and_then(Value::as_str).unwrap_or_default()
}

fn is_scene(s: &Value) -> bool {
    matches!(s.get("id").and_then(Value::as_str), Some("scene" | "group"))
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
        let s = Self { canvas, doc: RefCell::new(doc), engine: RefCell::new(None) };
        s.add_scene(first_scene)?;
        Ok(s)
    }

    /// A collection OBS (or `save`) wrote. One with no scenes gets `first_scene`.
    pub fn load(canvas: Canvas, doc: &Value, first_scene: &str) -> Result<Self, StudioError> {
        let mut doc = doc.clone();
        if !doc.get("sources").is_some_and(Value::is_array) {
            doc["sources"] = json!([]);
        }
        let s = Self { canvas, doc: RefCell::new(doc), engine: RefCell::new(None) };
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
        let shared = own::Shared::open_at(self.canvas, &self.doc.borrow())?;
        *self.engine.borrow_mut() = Some(shared.clone());
        Ok(shared)
    }

    /// Changes the document, then the engine shows it.
    fn edit<T>(&self, f: impl FnOnce(&mut Value) -> T) -> T {
        let out = f(&mut self.doc.borrow_mut());
        if let Some(e) = self.engine.borrow().as_ref() {
            e.reopen(&self.doc.borrow());
        }
        out
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

    fn order(&self) -> Vec<String> {
        let d = self.doc.borrow();
        let scenes: Vec<String> = self
            .sources_list()
            .iter()
            .filter(|s| s.get("id").and_then(Value::as_str) == Some("scene"))
            .map(|s| name_of(s).to_string())
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
            if on_air {
                d["current_scene"] = json!(rest[0]);
                d["current_program_scene"] = json!(rest[0]);
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
        let t = self.transition();
        self.doc.borrow_mut()["current_scene"] = json!(name);
        self.doc.borrow_mut()["current_program_scene"] = json!(name);
        if let Some(e) = self.engine.borrow().as_ref() {
            e.follow(&self.doc.borrow());
            let _ = t;
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
            for k in ["current_scene", "current_program_scene"] {
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
        Ok(())
    }

    // ------------------------------------------------------------------ items

    /// The source's picture size, once our engine has one (else what its settings say).
    fn size_of(&self, source: &str) -> (u32, u32) {
        if let Some(e) = self.engine.borrow().as_ref()
            && let Some((_, Some(size))) = e.engine.source_named(source)
        {
            return size;
        }
        let s = self.source(source).unwrap_or(Value::Null);
        let set = s.get("settings").cloned().unwrap_or(Value::Null);
        let n = |k: &str| set.get(k).and_then(Value::as_u64).map(|v| v as u32);
        match kind_of(&s) {
            k if k.starts_with("color_source") => {
                (n("width").unwrap_or(self.canvas.width), n("height").unwrap_or(self.canvas.height))
            }
            "scene" => (self.canvas.width, self.canvas.height),
            "group" => (n("cx").unwrap_or(0), n("cy").unwrap_or(0)),
            _ => (0, 0),
        }
    }

    fn info_of(&self, it: &Value) -> ItemInfo {
        let name = name_of(it).to_string();
        let src = self.source(&name).unwrap_or(Value::Null);
        let (w, h) = self.size_of(&name);
        let p = ferrocast_av::obs::placement_of(it);
        let a = ferrocast_av::draw_transform(&p, w, h);
        let (cw, ch) = ferrocast_av::cropped(w, h, p.crop);
        let (cw, ch) = (cw as f32, ch as f32);
        let kind = kind_of(&src).to_string();
        let media = matches!(kind.as_str(), "ffmpeg_source" | "vlc_source" | "slideshow" | "slideshow_v2");
        let audio = src.get("mixers").and_then(Value::as_u64).is_some_and(|m| m > 0)
            && !matches!(kind.as_str(), "scene" | "group" | "image_source" | "color_source_v3" | "text_ft2_source_v2");
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
        let source = json!({
            "id": kind, "versioned_id": kind, "name": name, "uuid": uuid(),
            "settings": if settings.is_object() { settings.clone() } else { json!({}) },
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
        let p = ferrocast_av::obs::placement_of(&it);
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
        })?;
        Ok(self.info_of(&it))
    }

    /// Moves and sizes an item so its picture fills `x, y, w, h` on the canvas (a drag in the
    /// preview), keeping its turn and crop.
    pub fn set_item_box(&self, scene: &str, id: i64, x: f32, y: f32, w: f32, h: f32) -> Result<ItemInfo, StudioError> {
        let mut t = self.transform(scene, id)?;
        let (cw, ch) = ferrocast_av::cropped(
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
                self.with_items(scene, |items, _| items.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id)))
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
        Ok(self.source(name)?.get("settings").cloned().unwrap_or_else(|| json!({})))
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
        let f = json!({
            "id": kind, "versioned_id": kind, "name": filter, "uuid": uuid(), "enabled": true,
            "settings": if settings.is_object() { settings.clone() } else { json!({}) },
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
            .map(|f| f.get("settings").cloned().unwrap_or_else(|| json!({})))
            .ok_or_else(|| fail(format!("\"{name}\" has no filter called \"{filter}\".")))
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
        self.scene_index(scene)?;
        let (cw, ch) = (self.canvas.width.max(1), self.canvas.height.max(1));
        let w = width.clamp(64, 640);
        let h = ((u64::from(w) * u64::from(ch)) / u64::from(cw)).max(1) as u32;
        let px = self
            .engine()?
            .engine
            .snapshot_scene_at(scene, (cw, ch), (w, h))
            .map_err(|e| fail(format!("\"{scene}\" couldn't be drawn: {e}")))?;
        crate::collection::png_data_url(&px, w, h, scene)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stage() -> Stage {
        Stage::new(Canvas { width: 1920, height: 1080, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"))
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
        assert_eq!(ok(s.add_existing("Main", "Blue")).name, "Blue", "a source can come back from the list");
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
        let got = ferrocast_av::obs::import(&doc, (1920, 1080));
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
}
