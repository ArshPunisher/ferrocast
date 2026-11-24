//! The Studio's scenes kept by Ferrocast itself, with no libobs: the scene collection is a
//! document in OBS's own format (what `Collection::save` writes and OBS reads), edited here and
//! handed to Ferrocast's own engine after each change, which draws it (unchanged sources keep
//! running). The same calls as `Collection`, returning the same answers, so the app can move
//! over call by call (ENGINE.md, step 3).

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::cell::Cell;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::collection::{
    AudioInfo, BLENDS, BOUNDS, FilterInfo, ItemEdit, ItemInfo, Monitor, NamedTransition, ResolvedTransition,
    SCALE_FILTERS, SceneInfo, SourceUse, Transform, TransitionInfo, TransitionMatrixRule,
};
use crate::naming::{NameVideo, file_name};
use crate::{Canvas, RecordPlan, RecordStats, ReplayPlan, StreamSettings, StreamStats, StudioError, check_name, own};

/// The size a picture is glanced at (`look`).
const LOOK_W: u32 = 32;
const LOOK_H: u32 = 18;

/// OBS's global sound channels, in its saved collection: Desktop Audio 1-2, Mic/Aux 1-4.
#[derive(Clone, Copy)]
enum FilterTarget<'a> {
    Source(&'a str),
    Item { scene: &'a str, id: i64 },
}

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

/// Reads each named source's levels (dBFS, floored at -100), from the engine's wait-free
/// meters: over the long (300 ms) window, or `fast` over a few blocks.
fn read_levels(
    e: &own::Shared,
    names: &[String],
    fast: bool,
) -> std::collections::HashMap<String, crate::collection::LevelInfo> {
    names
        .iter()
        .filter_map(|n| {
            let (id, _) = e.engine.source_named(n)?;
            let l = if fast { e.engine.levels_fast(id)? } else { e.engine.levels(id)? };
            // Silence is -100, not -inf: JSON has no infinity, so it went out as null, which
            // the page read as 0 dB, a full red meter the moment the sound stopped.
            let floor = |v: [f32; 2]| v.map(|x| if x.is_finite() { x.max(-100.0) } else { -100.0 });
            Some((
                n.clone(),
                crate::collection::LevelInfo {
                    peak: floor(l.peak),
                    magnitude: floor(l.rms),
                    input_peak: floor(l.input_peak),
                    gated: l.gated,
                },
            ))
        })
        .collect()
}

/// The running engine, for readers that must not wait for the Studio's main thread (the meters
/// poll thirty times a second). Weak: the picture-size check counts strong holders.
static LIVE_ENGINE: std::sync::Mutex<std::sync::Weak<own::Shared>> = std::sync::Mutex::new(std::sync::Weak::new());

fn publish_engine(shared: Option<&Arc<own::Shared>>) {
    *LIVE_ENGINE.lock().unwrap_or_else(|e| e.into_inner()) = shared.map_or_else(std::sync::Weak::new, Arc::downgrade);
}

/// The meters (the long window, or `fast` over a few blocks), read without the Studio's main thread. None when no engine
/// is running (the caller then asks the main thread, which says why).
pub fn levels_now(
    names: &[String],
    fast: bool,
) -> Option<std::collections::HashMap<String, crate::collection::LevelInfo>> {
    let e = LIVE_ENGINE.lock().unwrap_or_else(|e| e.into_inner()).upgrade()?;
    Some(read_levels(&e, names, fast))
}

/// The scene store.
#[derive(Debug)]
pub struct Stage {
    canvas: Canvas,
    /// The collection, as OBS saves one.
    doc: RefCell<Value>,
    /// Set while an edit's engine update is done by the caller itself (a moved source goes
    /// straight to the engine's item, not through opening the whole collection again).
    quiet: std::cell::Cell<bool>,
    reloads: std::cell::Cell<usize>,
    /// Keys held down (push to show, hide, filter on or off): how the engine sees an item or a
    /// filter while they are down. Never in `doc`, so never saved, never in an undo snapshot or
    /// an export, and gone with this stage (a crash or quit shows the collection as it was).
    holds: RefCell<Holds>,
    /// Where monitored sound plays (a device's name; None: the system's), kept for each engine.
    // Read only where monitoring plays (macOS, Windows); Linux keeps it for the next build.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    monitor_device: RefCell<Option<String>>,
    /// Meters read true peak (Settings).
    true_peak: std::cell::Cell<bool>,
    /// Low latency sound (Settings).
    low_latency: std::cell::Cell<bool>,
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
    /// Remove background at work: one worker for each source that has it, by the source's name.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    cutouts: RefCell<std::collections::HashMap<String, crate::cutout::Cutout>>,
    /// The frame rate as last worked out: when, the engine's frames and skipped count then,
    /// and the rate over the second before (`stats`).
    fps_seen: std::cell::Cell<Option<(std::time::Instant, u64, u64, f64)>>,
    /// The encoders' dropped-frame count when this stage began (the count is the program's): what
    /// the stats call "skipped" is what came after.
    dropped_base: u64,
}

/// What held keys change in the engine's view (see `Stage::holds`).
#[derive(Debug, Default)]
struct Holds {
    items: std::collections::HashMap<(String, i64), bool>,
    filters: std::collections::HashMap<(String, String), bool>,
}

impl Holds {
    fn is_empty(&self) -> bool {
        self.items.is_empty() && self.filters.is_empty()
    }

    /// Puts the held changes on `doc` (a copy made for the engine).
    fn apply(&self, doc: &mut Value) {
        let Some(sources) = doc.get_mut("sources").and_then(Value::as_array_mut) else { return };
        for ((source, filter), on) in &self.filters {
            for s in sources.iter_mut().filter(|s| name_of(s) == source) {
                for f in s.get_mut("filters").and_then(Value::as_array_mut).into_iter().flatten() {
                    if name_of(f) == filter {
                        f["enabled"] = json!(on);
                    }
                }
            }
        }
        let group_names: Vec<String> =
            sources.iter().filter(|s| kind_of(s) == "group").map(|s| name_of(s).to_string()).collect();
        for ((scene, id), visible) in &self.items {
            // The item is in the scene itself or in a group shown in it (at any depth).
            let mut lists = vec![scene.clone()];
            let mut seen = 0;
            while seen < lists.len() {
                let name = lists[seen].clone();
                seen += 1;
                let Some(src) = sources.iter_mut().find(|s| name_of(s) == name) else { continue };
                let mut groups = Vec::new();
                for it in src.pointer_mut("/settings/items").and_then(Value::as_array_mut).into_iter().flatten() {
                    if it.get("id").and_then(Value::as_i64) == Some(*id) {
                        it["visible"] = json!(visible);
                    }
                    groups.push(name_of(it).to_string());
                }
                for g in groups {
                    if group_names.contains(&g) && !lists.contains(&g) {
                        lists.push(g);
                    }
                }
            }
        }
    }
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

/// Where an item is saved as shown: its `ferrocast_show` ("stream" or "recording"; missing,
/// or anything else, is both, as every collection from before this had it).
const SHOW_KEY: &str = "ferrocast_show";

fn shown_of(item: &Value) -> ferrocore::engine::Show {
    ferrocore::engine::Show::from_name(item.get(SHOW_KEY).and_then(Value::as_str).unwrap_or(""))
}

fn kind_of(s: &Value) -> &str {
    s.get("versioned_id").or_else(|| s.get("id")).and_then(Value::as_str).unwrap_or_default()
}

fn is_scene(s: &Value) -> bool {
    matches!(s.get("id").and_then(Value::as_str), Some("scene" | "group"))
}

/// A web page whose sound isn't sent to the mixer ("Control audio via Ferrocast" off, OBS's
/// `reroute_audio`): a picture, with no sound row.
fn silent_page(kind: &str, src: &Value) -> bool {
    matches!(kind, "browser_source" | "ferrocast_web")
        && src.pointer("/settings/reroute_audio").and_then(Value::as_bool) != Some(true)
}

/// The device a source reads (its `device_id` setting), for telling a capture card from a mic.
fn device_of(s: &Value) -> &str {
    s.pointer("/settings/device_id").and_then(Value::as_str).unwrap_or("")
}

/// Words in a device's or a source's name that say it is a capture card or a video interface:
/// its sound is the game's or the camera's, not a voice (the owner's "USB2 Digital Audio" is a
/// MacroSilicon card whose id reads "USB2 Video").
const CAPTURE_CARD_WORDS: [&str; 12] = [
    "capture",
    "cam link",
    "elgato",
    "hdmi",
    "av.io",
    "avermedia",
    "magewell",
    "blackmagic",
    "decklink",
    "usb video",
    "usb2 video",
    "digital audio",
];

/// Whether a sound is judged as a microphone when the owner hasn't said: a microphone input or one
/// of OBS's Mic/Aux channels, unless its name says capture card. The desktop's and an app's sound
/// (and everything that isn't an input device) never is.
pub fn mic_by_default(kind: &str, name: &str, device: &str, channel: Option<u32>) -> bool {
    if matches!(channel, Some(1 | 2)) {
        return false;
    }
    let input = matches!(
        kind,
        "coreaudio_input_capture" | "wasapi_input_capture" | "pulse_input_capture" | "alsa_input_capture"
    );
    if !input && !matches!(channel, Some(3..=6)) {
        return false;
    }
    let (name, device) = (name.to_lowercase(), device.to_lowercase());
    !CAPTURE_CARD_WORDS.iter().any(|w| name.contains(w) || device.contains(w))
}

/// Whether `kind` can produce audio (excludes picture-only sources and video-only cameras).
fn is_audio_kind(kind: &str) -> bool {
    !matches!(
        kind,
        "scene"
            | "group"
            | "image_source"
            | "color_source"
            | "color_source_v2"
            | "color_source_v3"
            | "text_ft2_source"
            | "text_ft2_source_v2"
            | "text_gdiplus"
            | "text_gdiplus_v2"
            | "text_gdiplus_v3"
            | "ferrocast_clone"
            | "source-clone"
            | "ferrocast_countdown"
            | "ferrocast_visualizer"
            | "ferrocast_web_text"
            | "ferrocast_inputs"
            | "slideshow"
            | "slideshow_v2"
            | "image_slide_show"
    ) && !PICTURE_ONLY_DEVICES.contains(&kind)
}

fn source_has_audio(src: &Value) -> bool {
    src.get("mixers").and_then(Value::as_u64).is_some_and(|m| m > 0) && is_audio_kind(kind_of(src))
}

/// Updates any compressor filter on `source` whose `sidechain_source` names `from` to `to`.
/// OBS's "none" means no ducking (the engine never keys by a source of that name), so a source
/// called that, renamed, leaves it alone.
fn rename_sidechain_refs(source: &mut Value, from: &str, to: &str) {
    if from.trim().eq_ignore_ascii_case("none") {
        return;
    }
    for flt in source.get_mut("filters").and_then(Value::as_array_mut).into_iter().flatten() {
        if kind_of(flt) == "compressor_filter"
            && flt.pointer("/settings/sidechain_source").and_then(Value::as_str) == Some(from)
        {
            flt["settings"]["sidechain_source"] = json!(to);
        }
    }
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

/// `old` with " k" on the end, the name cut so the whole fits the 100 letters a name may have.
fn numbered(old: &str, k: u32) -> String {
    let suffix = format!(" {k}");
    let room = 100usize.saturating_sub(suffix.chars().count());
    let stem: String = old.chars().take(room).collect();
    format!("{}{suffix}", stem.trim_end())
}

/// An item with its source (a group: every source in it), copied out of a collection.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceClip {
    pub item: Value,
    /// The item's source first, then what a group holds.
    pub sources: Vec<Value>,
}

impl SourceClip {
    pub fn to_json(&self) -> Value {
        json!({ "item": self.item, "sources": self.sources })
    }

    pub fn from_json(v: &Value) -> Option<SourceClip> {
        let sources = v.get("sources")?.as_array()?.clone();
        Some(SourceClip { item: v.get("item")?.clone(), sources })
    }
}

/// Filters copied from a source or an item.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterClip {
    pub filters: Vec<Value>,
    pub from: String,
}

impl FilterClip {
    pub fn to_json(&self) -> Value {
        json!({ "filters": self.filters, "from": self.from })
    }

    pub fn from_json(v: &Value) -> Option<FilterClip> {
        Some(FilterClip { filters: v.get("filters")?.as_array()?.clone(), from: v.get("from")?.as_str()?.to_string() })
    }
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

/// A new item as OBS saves one: its own size at the top left, shown. Screens, windows and web
/// pages get bicubic scaling (OBS's default is none, bilinear): their text stays sharp when the
/// item is made bigger or smaller (Fit to screen is), and it costs nothing at their own size.
fn new_item(source: &Value, id: i64) -> Value {
    let sharp = matches!(
        kind_of(source),
        "screen_capture"
            | "display_capture"
            | "window_capture"
            | "monitor_capture"
            | "xshm_input"
            | "xcomposite_input"
            | "pipewire-screen-capture-source"
            | "pipewire-desktop-capture-source"
            | "pipewire-window-capture-source"
            | "browser_source"
            | "ferrocast_web_text"
    );
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
        "scale_filter": if sharp { "bicubic" } else { "disable" },
        "blend_method": "default",
        "blend_type": "normal",
        "group_item_backup": false,
    })
}

fn has_item_filter_override(it: &Value) -> bool {
    it.get("ferrocast_filters").and_then(Value::as_array).is_some()
}

fn clone_filters_with_fresh_uuids(filters: &[Value], keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    filters
        .iter()
        .filter(|f| keep(f))
        .map(|f| {
            let mut copy = f.clone();
            copy["uuid"] = json!(uuid());
            copy
        })
        .collect()
}

fn video_filters_from(source: &Value) -> Vec<Value> {
    let list = source.get("filters").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    clone_filters_with_fresh_uuids(list, |f| !ferrocore::obs::is_audio_filter_kind(kind_of(f)))
}

/// What Remove background has to work on: each source and the mask pictures its filters feed
/// (the engine names them: `source: filter` for the source's own filters, `scene#item: filter`
/// for an item's own). The mask comes from the picture alone, so one worker serves all of them.
/// Hidden items are left out. Studio Mode's copies of the scene on air count as scenes.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn cutout_plan(sources: &[Value], doc: &Value) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut want: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    let mut add = |source: &str, mask: String| {
        let l = want.entry(source.to_string()).or_default();
        if !l.contains(&mask) {
            l.push(mask);
        }
    };
    let mut shared_users: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut clone_users: Vec<(String, String)> = Vec::new();
    let mut containers: Vec<(String, Vec<Value>)> = Vec::new();
    for s in sources {
        let kind = kind_of(s);
        if matches!(kind, "scene" | "group") {
            let items = s.pointer("/settings/items").and_then(Value::as_array).cloned().unwrap_or_default();
            containers.push((name_of(s).to_string(), items));
        } else if matches!(kind, "ferrocast_clone" | "source-clone")
            && s.pointer("/settings/filters").and_then(Value::as_bool) != Some(false)
            && let Some(target) = s.pointer("/settings/clone").and_then(Value::as_str)
        {
            clone_users.push((name_of(s).to_string(), target.to_string()));
        }
    }
    for k in ["program_copy", "program_prev"] {
        let c = &doc["ferrocast"][k];
        if let (Some(private), Some(items)) = (c["private"].as_str(), c["items"].as_array())
            && PROGRAM_COPY.contains(&private)
        {
            containers.push((private.to_string(), items.clone()));
        }
    }
    for (container, items) in &containers {
        for it in items {
            if it.get("visible").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let src_name = name_of(it).to_string();
            if let Some(ov) = it.get("ferrocast_filters").and_then(Value::as_array) {
                let item_id = it.get("id").and_then(Value::as_i64).unwrap_or(0);
                for f in ov.iter().filter(|f| is_active_cutout(f)) {
                    add(&src_name, format!("{container}#{item_id}: {}", cutout_filter_name(f)));
                }
            } else {
                shared_users.insert(src_name);
            }
        }
    }
    // Any clone that brings its target's filters keeps source-level cutouts on that target live
    // when at least one placement uses the clone without an item override.
    loop {
        let mut changed = false;
        for (clone_name, target) in &clone_users {
            if shared_users.contains(clone_name) && shared_users.insert(target.clone()) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for s in sources {
        if matches!(kind_of(s), "scene" | "group") {
            continue;
        }
        let source = name_of(s).to_string();
        if !shared_users.contains(&source) {
            continue;
        }
        for f in s.get("filters").and_then(Value::as_array).into_iter().flatten().filter(|f| is_active_cutout(f)) {
            add(&source, format!("{source}: {}", cutout_filter_name(f)));
        }
    }
    want
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn is_active_cutout(f: &Value) -> bool {
    kind_of(f) == "ferrocast_remove_background" && f.get("enabled").and_then(Value::as_bool) != Some(false)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn cutout_filter_name(f: &Value) -> &str {
    f.get("name").and_then(Value::as_str).unwrap_or("ferrocast_remove_background")
}

/// Cameras and capture cards: a picture only. Their sound is a device of its own (a capture
/// card's "USB2 Digital Audio"), with a row of its own in the mixer: theirs showed a second,
/// silent row (owner, 17 Nov; OBS shows one), and Sound bars mustn't listen to them.
const PICTURE_ONLY_DEVICES: [&str; 6] =
    ["av_capture_input", "av_capture_input_v2", "macos-avcapture", "macos-avcapture-fast", "dshow_input", "v4l2_input"];

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
            "transitions": [
                { "id": "fade_transition", "name": "Fade", "duration": 300, "settings": {} },
                { "id": "cut_transition", "name": "Cut", "duration": 0, "settings": {} },
            ],
        });
        let s = Self {
            canvas,
            doc: RefCell::new(doc),
            quiet: std::cell::Cell::new(false),
            reloads: std::cell::Cell::new(0),
            holds: RefCell::default(),
            monitor_device: RefCell::new(None),
            true_peak: std::cell::Cell::new(false),
            low_latency: std::cell::Cell::new(false),
            engine: RefCell::new(None),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            pages: RefCell::default(),
            nets: RefCell::default(),
            ndis: RefCell::default(),
            ndi_failed: RefCell::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            cutouts: RefCell::default(),
            fps_seen: std::cell::Cell::new(None),
            dropped_base: ferrocore::video::dropped_frames(),
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
        import_and_normalize_transitions(&mut doc);
        let s = Self {
            canvas,
            doc: RefCell::new(doc),
            quiet: std::cell::Cell::new(false),
            reloads: std::cell::Cell::new(0),
            holds: RefCell::default(),
            monitor_device: RefCell::new(None),
            true_peak: std::cell::Cell::new(false),
            low_latency: std::cell::Cell::new(false),
            engine: RefCell::new(None),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            pages: RefCell::default(),
            nets: RefCell::default(),
            ndis: RefCell::default(),
            ndi_failed: RefCell::default(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            cutouts: RefCell::default(),
            fps_seen: std::cell::Cell::new(None),
            dropped_base: ferrocore::video::dropped_frames(),
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
        let shared = self.live(|d| own::Shared::open_at(self.canvas, d))?;
        // Monitoring plays from the start, as libobs keeps its device open: opened only when
        // Settings were saved, every new engine (each start, a new picture size) had none, and
        // a source set to be heard here ("Monitor only", "Monitor and output") played nowhere.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Err(e) = shared.engine.set_monitoring(true, self.monitor_device.borrow().as_deref()) {
            tracing::warn!("monitoring couldn't start: {e}");
        }
        shared.engine.set_true_peak(self.true_peak.get());
        shared.engine.set_low_latency(self.low_latency.get());
        *self.engine.borrow_mut() = Some(shared.clone());
        publish_engine(Some(&shared));
        self.sync_pages();
        Ok(shared)
    }

    /// Runs `f` on the collection as the engine should show it: the document with the held keys'
    /// changes on top (a copy only while a key is held).
    fn live<R>(&self, f: impl FnOnce(&Value) -> R) -> R {
        let holds = self.holds.borrow();
        if holds.is_empty() {
            return f(&self.doc.borrow());
        }
        let mut shown = self.doc.borrow().clone();
        holds.apply(&mut shown);
        f(&shown)
    }

    /// A push-to-show or push-to-hide key: item `id` of `scene` looks shown or hidden to the
    /// engine (`Some`) until it is let go (`None`). The document itself does not change.
    pub fn hold_item(&self, scene: &str, id: i64, visible: Option<bool>) -> Result<(), StudioError> {
        self.item(scene, id)?;
        {
            let mut h = self.holds.borrow_mut();
            let key = (scene.to_string(), id);
            match visible {
                Some(v) => drop(h.items.insert(key, v)),
                None => drop(h.items.remove(&key)),
            }
        }
        self.edit(|_| ());
        Ok(())
    }

    /// As [`hold_item`](Self::hold_item) for a source's own filter switched on or off.
    pub fn hold_filter(&self, source: &str, filter: &str, enabled: Option<bool>) -> Result<(), StudioError> {
        if !self.filter_list(source)?.iter().any(|f| name_of(f) == filter) {
            return Err(fail(format!("\"{source}\" has no filter called \"{filter}\".")));
        }
        {
            let mut h = self.holds.borrow_mut();
            let key = (source.to_string(), filter.to_string());
            match enabled {
                Some(v) => drop(h.filters.insert(key, v)),
                None => drop(h.filters.remove(&key)),
            }
        }
        self.edit(|_| ());
        Ok(())
    }

    /// Changes the document, then the engine shows it.
    /// A change to the collection's document that the scene graph doesn't read (transitions and
    /// the matrix): nothing is reloaded, so an edit made while a transition plays can't pop the
    /// outgoing scene's sources off the screen.
    fn edit_meta<T>(&self, f: impl FnOnce(&mut Value) -> T) -> T {
        let out = f(&mut self.doc.borrow_mut());
        if !self.quiet.get() {
            self.sync_pages();
        }
        out
    }

    /// How many times the whole scene graph was reloaded for an edit (for tests).
    pub fn graph_reloads(&self) -> usize {
        self.reloads.get()
    }

    fn edit<T>(&self, f: impl FnOnce(&mut Value) -> T) -> T {
        let out = f(&mut self.doc.borrow_mut());
        if self.quiet.get() {
            return out;
        }
        self.reloads.set(self.reloads.get() + 1);
        if let Some(e) = self.engine.borrow().as_ref() {
            self.live(|d| e.reopen(d));
        }
        self.sync_pages();
        out
    }

    /// Starts or stops finding the person for each Remove background filter that's on: one
    /// worker for each source, feeding every filter's mask picture.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn sync_cutouts(&self, e: &Arc<own::Shared>) {
        let want = cutout_plan(&self.sources_list(), &self.doc.borrow());
        let mut cutouts = self.cutouts.borrow_mut();
        // Gone, or working for an engine that's gone (a new canvas): stopped.
        cutouts.retain(|source, c| c.serves(e) && want.contains_key(source));
        for (source, masks) in want {
            match cutouts.get(&source) {
                Some(c) => c.set_masks(masks),
                None => {
                    cutouts.insert(source.clone(), crate::cutout::Cutout::start(Arc::downgrade(e), source, masks));
                }
            }
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
        self.ndi_failed.borrow_mut().retain(|name, _| want.iter().any(|(n, _)| n == name));
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

    /// The source already showing the camera or capture card that `kind` and `settings` would
    /// open, if one does (any of the camera kinds, the same device).
    fn using_device(&self, kind: &str, settings: &Value) -> Option<String> {
        let want = ferrocore::obs::camera_device(kind, settings)?;
        self.sources_list().into_iter().find_map(|s| {
            let dev = ferrocore::obs::camera_device(kind_of(&s), s.get("settings")?)?;
            (dev == want).then(|| name_of(&s).to_string())
        })
    }

    /// Whether `scene` shows the source `name` (in a group too).
    fn shows(&self, scene: &str, name: &str) -> bool {
        fn has(items: &[ItemInfo], name: &str) -> bool {
            items.iter().any(|i| i.name == name || has(&i.children, name))
        }
        self.items(scene).is_ok_and(|l| has(&l, name))
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
        main_scenes(&self.doc.borrow())
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
        let pre = self.preview_scene();
        self.order()
            .into_iter()
            .map(|name| {
                let items = self.scene_items(&name).map_or(0, |i| i.len());
                let preview = pre.as_deref() == Some(name.as_str());
                let transition = self.scene_transition_of(&name).ok().flatten();
                let transition_name = self.scene_transition_name_of(&name);
                SceneInfo { current: name == cur, items, name, preview, transition, transition_name }
            })
            .collect()
    }

    pub fn current(&self) -> String {
        self.doc.borrow().get("current_program_scene").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    /// Opens the scenes again in the engine as they are (a plugin's effect came or went).
    pub fn refresh(&self) {
        if let Some(e) = self.engine.borrow().as_ref() {
            self.live(|d| e.reopen_with(d, true));
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
            if let Some(st) = d.pointer_mut("/ferrocast/scene_transitions").and_then(Value::as_object_mut) {
                st.remove(name);
            }
            if let Some(m) = d.pointer_mut("/ferrocast/transition_matrix").and_then(Value::as_array_mut) {
                m.retain(|r| {
                    r.get("from").and_then(Value::as_str) != Some(name)
                        && r.get("to").and_then(Value::as_str) != Some(name)
                });
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

    /// Puts a scene on air, through the resolved transition (the engine plays it).
    /// When the vertical canvas is enabled, its own scenes switch there instead.
    pub fn set_current(&self, name: &str) -> Result<(), StudioError> {
        if self.vertical().is_some() && self.vertical_order().iter().any(|n| n == name) {
            return self.set_vertical_current(name);
        }
        self.scene_index(name)?;
        let was = self.current();
        let t = self.resolve_transition(&was, name);
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
            return self.send_copy_on_air_with(name, &t.kind, u64::from(t.ms), &t.settings);
        }
        if let Some(e) = self.engine.borrow().as_ref()
            && (was != name || e.is_black())
        {
            e.go_with(name, &t.kind, u64::from(t.ms), &t.settings)?;
        }
        Ok(())
    }

    pub fn duplicate_scene(&self, name: &str, new_name: &str) -> Result<(), StudioError> {
        let mut items = self.scene_items(name)?;
        self.add_scene(new_name)?;
        // A group belongs to one scene (as in OBS): the copy gets groups of its own, holding the
        // same sources, so changing one scene's group leaves the other's alone.
        for it in items.iter_mut() {
            if let Some(ov) = it.get_mut("ferrocast_filters").and_then(Value::as_array_mut) {
                for f in ov {
                    f["uuid"] = json!(uuid());
                }
            }
            let gname = name_of(it).to_string();
            let Ok(src) = self.source(&gname) else { continue };
            if kind_of(&src) != "group" {
                continue;
            }
            let mut n = 2;
            let copy = loop {
                let c = format!("{gname} {n}");
                if self.name_free(&c).is_ok() {
                    break c;
                }
                n += 1;
            };
            let mut g = src.clone();
            g["name"] = json!(copy);
            g["uuid"] = json!(uuid());
            self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(g.clone())));
            it["name"] = json!(copy);
            it["source_uuid"] = g["uuid"].clone();
        }
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
                if let Some(s) = d.get_mut(k) {
                    if name_of(s) == name {
                        s["name"] = json!(new_name);
                    }
                    rename_sidechain_refs(s, name, new_name);
                }
            }
            if let Some(l) = d["sources"].as_array_mut() {
                for s in l.iter_mut() {
                    if name_of(s) == name {
                        s["name"] = json!(new_name);
                    }
                    rename_sidechain_refs(s, name, new_name);
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
            if let Some(m) = d.pointer_mut("/ferrocast/transition_matrix").and_then(Value::as_array_mut) {
                for r in m.iter_mut() {
                    if r.get("from").and_then(Value::as_str) == Some(name) {
                        r["from"] = json!(new_name);
                    }
                    if r.get("to").and_then(Value::as_str) == Some(name) {
                        r["to"] = json!(new_name);
                    }
                }
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
        self.item_size_of(source, None)
    }

    fn item_size_of(&self, source: &str, item: Option<&Value>) -> (u32, u32) {
        let (w, h) = self.own_size_of(source);
        if w == 0 || h == 0 {
            return (w, h);
        }
        let s = self.source(source).unwrap_or(Value::Null);
        let filters = item
            .and_then(|it| it.get("ferrocast_filters").and_then(Value::as_array))
            .or_else(|| s.get("filters").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        let effects: Vec<ferrocore::Effect> = filters
            .iter()
            .filter(|f| f.get("enabled").and_then(Value::as_bool) != Some(false))
            .filter_map(|f| {
                let set = with_defaults(f);
                ferrocore::obs::effect_from(kind_of(f), &set)
            })
            .collect();
        // As the engine places it: a shadow drawn outside the box doesn't count.
        ferrocore::Effect::placed_size(&effects, w, h)
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
        self.info_with(it, true)
    }

    /// An item's info; `expand` lists a group's sources (one level: a group's own sources
    /// are never expanded again, so a group shown inside another can't loop).
    fn info_with(&self, it: &Value, expand: bool) -> ItemInfo {
        let name = name_of(it).to_string();
        let src = self.source(&name).unwrap_or(Value::Null);
        let (w, h) = self.item_size_of(&name, Some(it));
        let p = ferrocore::obs::placement_of(it);
        let a = ferrocore::draw_transform(&p, w, h);
        let (cw, ch) = ferrocore::cropped(w, h, p.crop);
        let (cw, ch) = (cw as f32, ch as f32);
        let kind = kind_of(&src).to_string();
        let media =
            matches!(kind.as_str(), "ffmpeg_source" | "vlc_source" | "slideshow" | "slideshow_v2" | "ferrocast_replay");
        // A web page with "Control audio in Ferrocast" off has no sound row (S60).
        let audio = source_has_audio(&src) && !silent_page(&kind, &src);
        // A group's sources, top first, each framed on the canvas through the group.
        let children = if kind == "group" && expand {
            self.scene_items(&name)
                .unwrap_or_default()
                .iter()
                .rev()
                .map(|c| {
                    let mut ci = self.info_with(c, false);
                    let f = ci.frame;
                    let unit = ferrocore::Affine { a: f[0], b: f[1], c: f[2], d: f[3], e: f[4], f: f[5] };
                    let on = unit.then(&a);
                    ci.frame = [on.a, on.b, on.c, on.d, on.e, on.f];
                    ci
                })
                .collect()
        } else {
            Vec::new()
        };
        ItemInfo {
            id: it.get("id").and_then(Value::as_i64).unwrap_or(0),
            name,
            visible: it.get("visible").and_then(Value::as_bool).unwrap_or(true),
            show: shown_of(it).name().to_string(),
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
            children,
            show_transition: None,
            hide_transition: None,
            deinterlace: None,
            top_field_first: false,
            media,
            kind,
            own_filters: has_item_filter_override(it),
        }
    }

    /// A scene's items, top first (as OBS's Sources list shows them).
    pub fn items(&self, scene: &str) -> Result<Vec<ItemInfo>, StudioError> {
        Ok(self.scene_items(scene)?.iter().rev().map(|i| self.info_of(i)).collect())
    }

    /// Whether any item the engine could draw on the wide picture is shown only on the stream or
    /// only in the recording: then the two pictures differ, so a recording can't share the stream's
    /// encode ("Same as stream"), which carries the stream's picture. It reads the collection as
    /// the engine does (Studio Mode's copy on air included) and counts what shows: not a hidden
    /// source or group, not the vertical picture's scenes (it has no recording), and not the copies
    /// OBS keeps of a group's items (`group_item_backup`).
    pub fn has_selective_items(&self) -> bool {
        let doc = self.doc.borrow();
        let doc = own::engine_doc(&doc);
        let vertical = self.vertical_order();
        let sources: &[Value] = doc["sources"].as_array().map_or(&[], Vec::as_slice);
        fn walk(sources: &[Value], scene: &Value, depth: u32) -> bool {
            let items = scene.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten();
            for it in items {
                if it.get("group_item_backup").and_then(Value::as_bool) == Some(true)
                    || it.get("visible").and_then(Value::as_bool) == Some(false)
                {
                    continue;
                }
                if shown_of(it) != ferrocore::engine::Show::Both {
                    return true;
                }
                // A nested scene or a group shown here: what is in it is shown with it.
                if depth < 8
                    && let Some(inner) = sources.iter().find(|s| name_of(s) == name_of(it) && is_scene(s))
                    && walk(sources, inner, depth + 1)
                {
                    return true;
                }
            }
            false
        }
        sources
            .iter()
            .filter(|s| kind_of(s) == "scene" && !vertical.iter().any(|v| v == name_of(s)))
            .any(|s| walk(sources, s, 0))
    }

    pub fn item_ids(&self, scene: &str) -> Result<Vec<i64>, StudioError> {
        Ok(self.scene_items(scene)?.iter().rev().filter_map(|i| i.get("id")?.as_i64()).collect())
    }

    fn item(&self, scene: &str, id: i64) -> Result<Value, StudioError> {
        self.scene_items(&self.home(scene, id))?
            .into_iter()
            .find(|i| i.get("id").and_then(Value::as_i64) == Some(id))
            .ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))
    }

    /// Where an item of `scene` lives: the scene itself, or the group shown in it that holds
    /// it (as OBS's, a group's sources are edited through the scene). An unknown id answers
    /// `scene`, so the usual "has no item" follows.
    fn home(&self, scene: &str, id: i64) -> String {
        let Ok(items) = self.scene_items(scene) else { return scene.to_string() };
        let has = |l: &[Value]| l.iter().any(|i| i.get("id").and_then(Value::as_i64) == Some(id));
        if has(&items) {
            return scene.to_string();
        }
        for g in items.iter().map(name_of) {
            if self.source(g).is_ok_and(|s| kind_of(&s) == "group") && self.scene_items(g).is_ok_and(|l| has(&l)) {
                return g.to_string();
            }
        }
        scene.to_string()
    }

    /// The highest item id in `scene` and in the groups it shows: a new item takes the next,
    /// so a source in a group never shares an id with one in its scene.
    fn top_id(&self, scene: &str) -> i64 {
        let items = self.scene_items(scene).unwrap_or_default();
        let mut top = self
            .source(scene)
            .ok()
            .and_then(|s| s.pointer("/settings/id_counter").and_then(Value::as_i64))
            .unwrap_or(0);
        for it in &items {
            top = top.max(it.get("id").and_then(Value::as_i64).unwrap_or(0));
            let name = name_of(it);
            if self.source(name).is_ok_and(|s| kind_of(&s) == "group") {
                for c in self.scene_items(name).unwrap_or_default() {
                    top = top.max(c.get("id").and_then(Value::as_i64).unwrap_or(0));
                }
            }
        }
        top
    }

    /// The group item (in `scene`) holding item `id`, when it's inside one.
    fn group_of(&self, scene: &str, id: i64) -> Option<Value> {
        let home = self.home(scene, id);
        (home != scene).then(|| self.scene_items(scene).ok()?.into_iter().find(|i| name_of(i) == home)).flatten()
    }

    /// A group's own scale on each side as it shows (signed: a flip is negative), Fit and
    /// Stretch boxes included.
    fn group_scale(&self, group_item: &Value) -> (f32, f32) {
        let g = self.group_map(group_item);
        let p = ferrocore::obs::placement_of(group_item);
        let sign = |v: f32| if v < 0.0 { -1.0 } else { 1.0 };
        (
            (g.a * g.a + g.b * g.b).sqrt().max(1e-6) * sign(p.scale_x),
            (g.c * g.c + g.d * g.d).sqrt().max(1e-6) * sign(p.scale_y),
        )
    }

    /// How a group's own pixels map onto the canvas.
    fn group_map(&self, group_item: &Value) -> ferrocore::Affine {
        let (w, h) = self.item_size_of(name_of(group_item), Some(group_item));
        ferrocore::draw_transform(&ferrocore::obs::placement_of(group_item), w, h)
    }

    /// Fits a group to what it holds (OBS does the same): its size becomes the box round its
    /// sources, which move so they stay where they show on the canvas. An empty group stays.
    fn refit_group(&self, scene: &str, gid: i64) -> Result<(), StudioError> {
        let gi = self.item(scene, gid)?;
        let gname = name_of(&gi).to_string();
        let kids = self.scene_items(&gname)?;
        // Nothing to fit yet, or a source that doesn't know its size yet (a camera or a video
        // still opening reads 0×0): fitting now would shrink the group round nothing.
        if kids.is_empty() || kids.iter().any(|c| matches!(self.item_size_of(name_of(c), Some(c)), (0, _) | (_, 0))) {
            return Ok(());
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for c in &kids {
            let f = self.info_of(c).frame;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                let (x, y) = (f[4] + u * f[0] + v * f[2], f[5] + u * f[1] + v * f[3]);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return Ok(());
        }
        // A group sized by a Fit or Stretch box: that box becomes its plain scale first, so a new
        // size doesn't stretch what's already in it.
        if gi.get("bounds_type").and_then(Value::as_u64).unwrap_or(0) != 0 {
            let (gx, gy) = self.group_scale(&gi);
            let at = self.group_map(&gi).apply(0.0, 0.0);
            self.with_item(scene, gid, |i| {
                i["bounds_type"] = json!(0);
                i["scale"] = json!({ "x": gx, "y": gy });
            })?;
            let now = self.group_map(&self.item(scene, gid)?).apply(0.0, 0.0);
            self.with_item(scene, gid, |i| {
                let x = i.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32 + at.0 - now.0;
                let y = i.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32 + at.1 - now.1;
                i["pos"] = json!({ "x": x, "y": y });
            })?;
        }
        let gi = self.item(scene, gid)?;
        // Where the box's corner shows now, before anything changes.
        let target = self.group_map(&gi).apply(x0, y0);
        let (cx, cy) = ((x1 - x0).max(1.0).ceil() as u32, (y1 - y0).max(1.0).ceil() as u32);
        self.with_items(&gname, |l, set| {
            for c in l.iter_mut() {
                let x = c.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32 - x0;
                let y = c.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32 - y0;
                c["pos"] = json!({ "x": x, "y": y });
            }
            set.insert("cx".into(), json!(cx));
            set.insert("cy".into(), json!(cy));
            set.insert("custom_size".into(), json!(true));
        })?;
        // The group moves so its new corner lands where the box's corner was.
        let gi = self.item(scene, gid)?;
        let now = self.group_map(&gi).apply(0.0, 0.0);
        let (dx, dy) = (target.0 - now.0, target.1 - now.1);
        self.with_item(scene, gid, |i| {
            let x = i.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32 + dx;
            let y = i.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32 + dy;
            i["pos"] = json!({ "x": x, "y": y });
        })
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
        self.with_items(&self.home(scene, id), |items, _| {
            items.iter_mut().find(|i| i.get("id").and_then(Value::as_i64) == Some(id)).map(f)
        })?
        .ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))
    }

    /// Puts `source` (an existing one) at the top of `scene` → its item.
    fn place(&self, scene: &str, source: &Value) -> Result<ItemInfo, StudioError> {
        self.place_with(scene, source, None)
    }

    fn place_with(
        &self,
        scene: &str,
        source: &Value,
        override_filters: Option<Vec<Value>>,
    ) -> Result<ItemInfo, StudioError> {
        let floor = self.top_id(scene);
        let item = self.with_items(scene, |items, set| {
            let id = set
                .get("id_counter")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(floor)
                .max(items.iter().filter_map(|i| i.get("id")?.as_i64()).max().unwrap_or(0))
                + 1;
            set.insert("id_counter".into(), json!(id));
            let mut it = new_item(source, id);
            if let Some(ov) = override_filters {
                it["ferrocast_filters"] = Value::Array(ov);
            }
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
        self.add_source_or_reuse(scene, kind, name, settings, fit).map(|(i, _)| i)
    }

    /// [`Self::add_source`], and whether it showed a source already there (a camera or capture
    /// card already used: its settings are that source's, not the ones given).
    pub fn add_source_or_reuse(
        &self,
        scene: &str,
        kind: &str,
        name: &str,
        settings: &Value,
        fit: bool,
    ) -> Result<(ItemInfo, bool), StudioError> {
        self.scene_items(scene)?;
        // A camera or capture card already used here: that source again, never the device
        // opened twice. A capture card can't be: both stopped sending and it wouldn't open
        // again ("isn't connected") until one was removed (owner, 17 Nov: added as a Capture
        // card, then again as a Camera).
        if let Some(have) = self.using_device(kind, settings) {
            if self.shows(scene, &have) {
                return Err(fail(format!("This device is already in this scene, as \"{have}\".")));
            }
            // One switched off would never show here: it's asked for, so it's on again.
            self.with_source(&have, |s| s["enabled"] = json!(true))?;
            let mut info = self.add_existing(scene, &have)?;
            if fit {
                self.edit_item(scene, info.id, ItemEdit::Fit)?;
                info = self.info_of(&self.item(scene, info.id)?);
            }
            return Ok((info, true));
        }
        self.name_free(name)?;
        // The kind's defaults, under what was given.
        let mut set = crate::kinds::defaults(kind).unwrap_or_else(|| json!({}));
        if let (Some(base), Some(given)) = (set.as_object_mut(), settings.as_object()) {
            for (k, v) in given {
                base.insert(k.clone(), v.clone());
            }
        }
        remember_capture(kind, &mut set, true);
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
        Ok((info, false))
    }

    /// Shows a source already made (in another scene) in `scene` too. When the source already
    /// carries video filters, the new item gets its own copy (`own_filters: true`) so edits on one
    /// canvas do not change the other.
    pub fn add_existing(&self, scene: &str, name: &str) -> Result<ItemInfo, StudioError> {
        let source = self.source(name)?;
        if name == scene {
            return Err(fail("A scene can't show itself."));
        }
        let video = video_filters_from(&source);
        self.place_with(scene, &source, (!video.is_empty()).then_some(video))
    }

    pub fn transform(&self, scene: &str, id: i64) -> Result<Transform, StudioError> {
        let it = self.item(scene, id)?;
        let p = ferrocore::obs::placement_of(&it);
        let (w, h) = self.item_size_of(name_of(&it), Some(&it));
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
        // Inside a group: the group fits its sources again, as OBS does after every move
        // (that also hands the engine the whole change).
        let home = self.home(scene, id);
        if home != scene {
            let gid = self
                .scene_items(scene)?
                .iter()
                .find(|i| name_of(i) == home)
                .and_then(|i| i.get("id").and_then(Value::as_i64));
            if let Some(gid) = gid {
                self.refit_group(scene, gid)?;
            }
            // The move was saved quietly; a refit that had nothing to do (a source still
            // opening) changed nothing else, so the engine is told here either way.
            if let Some(e) = self.engine.borrow().as_ref() {
                self.live(|d| e.reopen(d));
            }
            self.sync_pages();
            let it = self.item(scene, id)?;
            let mut info = self.info_of(&it);
            if let Some(gi) = gid.and_then(|g| self.item(scene, g).ok()) {
                let f = info.frame;
                let unit = ferrocore::Affine { a: f[0], b: f[1], c: f[2], d: f[3], e: f[4], f: f[5] };
                let on = unit.then(&self.group_map(&gi));
                info.frame = [on.a, on.b, on.c, on.d, on.e, on.f];
            }
            return Ok(info);
        }
        let moved = self
            .engine
            .borrow()
            .as_ref()
            .is_some_and(|e| e.engine.set_item_placement(scene, id, ferrocore::obs::placement_of(&it)));
        if !moved {
            if let Some(e) = self.engine.borrow().as_ref() {
                self.live(|d| e.reopen(d));
            }
            self.sync_pages();
        }
        Ok(self.info_of(&it))
    }

    /// Moves and sizes several items at once, each as `set_item_box` does → the items, in the
    /// order given. All of them or none: an id that isn't there is refused before any moves, and
    /// a failure part way puts the collection back as it was.
    pub fn set_item_boxes(
        &self,
        scene: &str,
        boxes: &[(i64, f32, f32, f32, f32)],
    ) -> Result<Vec<ItemInfo>, StudioError> {
        for (id, ..) in boxes {
            self.item(scene, *id)?;
        }
        // A group with one of its own sources: moving the source fits the group again, and the
        // group's box then lands on the changed group (the source moves twice). Asked for one at
        // a time, not both.
        let asked: std::collections::HashSet<i64> = boxes.iter().map(|b| b.0).collect();
        if self.items(scene)?.iter().any(|g| asked.contains(&g.id) && g.children.iter().any(|c| asked.contains(&c.id)))
        {
            return Err(fail("a group and a source inside it can't be moved together.".to_string()));
        }
        let before = self.snapshot();
        for &(id, x, y, w, h) in boxes {
            if let Err(e) = self.set_item_box(scene, id, x, y, w, h) {
                self.restore_all(&before);
                return Err(e);
            }
        }
        // As things ended up (a move inside a group fits the group again), in the order asked;
        // a source inside a group is read through the scene, in canvas pixels.
        let list = self.items(scene)?;
        let found = |id: i64| {
            list.iter().flat_map(|i| std::iter::once(i).chain(i.children.iter())).find(|i| i.id == id).cloned()
        };
        boxes.iter().map(|b| found(b.0).ok_or_else(|| fail(format!("\"{scene}\" has no item {}.", b.0)))).collect()
    }

    /// Moves and sizes an item so its picture fills `x, y, w, h` on the canvas (a drag in the
    /// preview), keeping its turn and crop.
    pub fn set_item_box(&self, scene: &str, id: i64, x: f32, y: f32, w: f32, h: f32) -> Result<ItemInfo, StudioError> {
        // In a group: the canvas box into the group's own pixels.
        let home = self.home(scene, id);
        let (x, y, w, h) = match (home != scene)
            .then(|| self.scene_items(scene).ok()?.into_iter().find(|i| name_of(i) == home))
            .flatten()
        {
            Some(gi) => {
                let g = self.group_map(&gi);
                match g.inverse() {
                    Some(back) => {
                        let (lx, ly) = back.apply(x, y);
                        let (sx, sy) = ((g.a * g.a + g.b * g.b).sqrt(), (g.c * g.c + g.d * g.d).sqrt());
                        (lx, ly, w / sx.max(1e-6), h / sy.max(1e-6))
                    }
                    None => (x, y, w, h),
                }
            }
            None => (x, y, w, h),
        };
        let mut t = self.transform(scene, id)?;
        let (cw, ch) = ferrocore::cropped(
            t.width,
            t.height,
            [t.crop_left, t.crop_top, t.crop_right, t.crop_bottom].map(|c| c.max(0) as u32),
        );
        if t.bounds != "none" {
            // A box of another shape than the picture's (a side handle with the preview's
            // frame lock open) stretches it: Fit, Fill and the rest would keep the picture's
            // shape inside the new box, so the drag did nothing (owner's recording, 12 Nov).
            // The picture's shape as Fit keeps it: its own scale counts (libobs's bounds do).
            let (sx, sy) = (t.scale_x.abs(), t.scale_y.abs());
            let other_shape = cw > 0 && ch > 0 && h > 0.0 && sx > 0.0 && sy > 0.0 && {
                let (box_aspect, picture) = (w / h, cw as f32 * sx / (ch as f32 * sy));
                (box_aspect / picture - 1.0).abs() > 0.005
            };
            if other_shape && t.bounds != "stretch" {
                t.bounds = "stretch".into();
                t.bounds_alignment = 0;
            }
            (t.bounds_width, t.bounds_height) = (w, h);
        } else if cw > 0 && ch > 0 {
            (t.scale_x, t.scale_y) = (w / cw as f32 * t.scale_x.signum(), h / ch as f32 * t.scale_y.signum());
        }
        // The box's top left, whatever the item is aligned by.
        t.alignment = 5;
        (t.x, t.y) = (x, y);
        let info = self.set_transform(scene, id, &t)?;
        // A flipped item in bounds is drawn from another corner than `x, y` (the bounds' alignment
        // and the flip move its origin): the frame's corner is what the caller sees and sends back,
        // so `x, y` are moved by what they are off, and a nudge of 1 pixel is 1 pixel.
        if home == scene {
            let (dx, dy) = (x - info.frame[4], y - info.frame[5]);
            if dx.abs() > 0.01 || dy.abs() > 0.01 {
                (t.x, t.y) = (t.x + dx, t.y + dy);
                return self.set_transform(scene, id, &t);
            }
        }
        Ok(info)
    }

    /// Sets how many of the source's pixels are cut from each side (left, top, right, bottom).
    /// The picture that stays visible stays where it is on the canvas, as OBS's crop drag does:
    /// cutting the left or the top moves the item's box in by as much, never the picture with it
    /// (owner, 25 Nov: dragging an edge in moved the whole source).
    pub fn set_item_crop(&self, scene: &str, id: i64, crop: [i32; 4]) -> Result<ItemInfo, StudioError> {
        let crop = crop.map(|c| c.max(0));
        let before = self.transform(scene, id)?;
        // Where the box's top-left corner has to be, in the box's own 0..1 terms of the old
        // box: the old visible picture's top-left plus what is newly cut from the left and top.
        let (w, h) = (before.width.max(1) as f32, before.height.max(1) as f32);
        let seen = (
            (w - before.crop_left as f32 - before.crop_right as f32).max(1.0),
            (h - before.crop_top as f32 - before.crop_bottom as f32).max(1.0),
        );
        let corner = ((crop[0] - before.crop_left) as f32 / seen.0, (crop[1] - before.crop_top) as f32 / seen.1);
        let target = self.item_point(scene, id, corner)?;
        let mut t = before;
        [t.crop_left, t.crop_top, t.crop_right, t.crop_bottom] = crop;
        self.set_transform(scene, id, &t)?;
        // Inside bounds (fit, stretch, ...) the picture is fitted to the box, so it can't stay.
        if t.bounds == "none" {
            self.settle_point(scene, id, (0.0, 0.0), target)?;
        }
        let it = self.item(scene, id)?;
        Ok(self.info_of(&it))
    }

    /// Where an item is in its scene (0 is the top) and the scene's name.
    pub fn item_place(&self, scene: &str, id: i64) -> Result<(usize, String), StudioError> {
        let home = self.home(scene, id);
        let ids = self.item_ids(&home)?;
        let at = ids.iter().position(|i| *i == id).ok_or_else(|| fail(format!("\"{scene}\" has no item {id}.")))?;
        Ok((at, home))
    }

    /// Moves an item to place `to` (0 is the top).
    pub fn item_order(&self, scene: &str, id: i64, to: usize) -> Result<(), StudioError> {
        self.with_items(&self.home(scene, id), |items, _| {
            let Some(at) = items.iter().position(|i| i.get("id").and_then(Value::as_i64) == Some(id)) else { return };
            let it = items.remove(at);
            // Saved bottom first; `to` counts from the top.
            let to = items.len().saturating_sub(to.min(items.len()));
            items.insert(to, it);
        })
    }

    /// Where the point `at` of an item's box (0..1 across and down it, its own orientation
    /// before any flip: the picture's top-left is (0, 0)) is on the canvas, through its group
    /// inside one.
    fn item_point(&self, scene: &str, id: i64, at: (f32, f32)) -> Result<(f32, f32), StudioError> {
        let it = self.item(scene, id)?;
        let mut f = self.info_of(&it).frame;
        if let Some(gi) = self.group_of(scene, id) {
            let unit = ferrocore::Affine { a: f[0], b: f[1], c: f[2], d: f[3], e: f[4], f: f[5] };
            let on = unit.then(&self.group_map(&gi));
            f = [on.a, on.b, on.c, on.d, on.e, on.f];
        }
        Ok((f[4] + f[0] * at.0 + f[2] * at.1, f[5] + f[1] * at.0 + f[3] * at.1))
    }

    /// Where the middle of an item's box is on the canvas.
    fn item_centre(&self, scene: &str, id: i64) -> Result<(f32, f32), StudioError> {
        self.item_point(scene, id, (0.5, 0.5))
    }

    /// Moves an item (changing its position only) until the point `at` of its box is on the
    /// canvas at `target`. Inside a group the group fits itself round its sources again after
    /// every write, so its map changes under the item: the move is measured again after each
    /// write until the point is there (one round outside a group, a few inside).
    fn settle_point(&self, scene: &str, id: i64, at: (f32, f32), target: (f32, f32)) -> Result<(), StudioError> {
        for _ in 0..6 {
            let now = self.item_point(scene, id, at)?;
            if (target.0 - now.0).abs() < 0.01 && (target.1 - now.1).abs() < 0.01 {
                break;
            }
            let (dx, dy) = match self.group_of(scene, id).and_then(|gi| self.group_map(&gi).inverse()) {
                Some(back) => {
                    let (a, b) = (back.apply(target.0, target.1), back.apply(now.0, now.1));
                    (a.0 - b.0, a.1 - b.1)
                }
                None => (target.0 - now.0, target.1 - now.1),
            };
            let mut t = self.transform(scene, id)?;
            (t.x, t.y) = (t.x + dx, t.y + dy);
            self.set_transform(scene, id, &t)?;
        }
        Ok(())
    }

    /// Changes an item's transform with `change`, then moves it so the middle of its box is
    /// where it was on the canvas: a flip or a turn happens in place, not about its alignment
    /// point (which, for the default top-left one, swung the picture away, partly out of the
    /// canvas).
    fn change_in_place(&self, scene: &str, id: i64, change: impl FnOnce(&mut Transform)) -> Result<(), StudioError> {
        let from = self.item_centre(scene, id)?;
        let mut t = self.transform(scene, id)?;
        change(&mut t);
        self.set_transform(scene, id, &t)?;
        self.settle_point(scene, id, (0.5, 0.5), from)
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
            // Both is the default and isn't kept: the file stays what it was before this.
            E::StreamAndRecording => self.with_item(scene, id, |i| drop(i.as_object_mut().map(|o| o.remove(SHOW_KEY)))),
            E::StreamOnly => set(SHOW_KEY, json!("stream")),
            E::RecordingOnly => set(SHOW_KEY, json!("recording")),
            E::Remove => {
                self.item(scene, id)?;
                let home = self.home(scene, id);
                // Out of the scene (or its group), and the source with it when nothing shows it
                // any more: one change, so the engine closes it at once.
                self.edit(|d| {
                    let found = d["sources"].as_array_mut().and_then(|l| l.iter_mut().find(|s| name_of(s) == home));
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
                let (at, home) = self.item_place(scene, id)?;
                let last = self.item_ids(&home)?.len().saturating_sub(1);
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
                // Inside a group: the canvas's box, in the group's own pixels.
                if let Some(gi) = self.group_of(scene, id) {
                    let g = self.group_map(&gi);
                    let back = g.inverse().ok_or_else(|| fail("That group is squashed flat: reset its size first."))?;
                    let p = ferrocore::obs::placement_of(&gi);
                    let (gx, gy) = self.group_scale(&gi);
                    (t.x, t.y) = back.apply(0.0, 0.0);
                    t.rotation = (-p.rotation).rem_euclid(360.0);
                    (t.bounds_width, t.bounds_height) = (cw / gx.abs(), ch / gy.abs());
                }
                self.set_transform(scene, id, &t).map(drop)
            }
            E::Center => {
                let it = self.item(scene, id)?;
                let group = self.group_of(scene, id);
                let mut f = self.info_of(&it).frame;
                if let Some(gi) = &group {
                    let unit = ferrocore::Affine { a: f[0], b: f[1], c: f[2], d: f[3], e: f[4], f: f[5] };
                    let on = unit.then(&self.group_map(gi));
                    f = [on.a, on.b, on.c, on.d, on.e, on.f];
                }
                // The box's middle onto the canvas's middle (through the group, inside one).
                let (mx, my) = (f[4] + (f[0] + f[2]) / 2.0, f[5] + (f[1] + f[3]) / 2.0);
                let mut t = self.transform(scene, id)?;
                match group.and_then(|gi| self.group_map(&gi).inverse()) {
                    Some(back) => {
                        let (to, from) = (back.apply(cw / 2.0, ch / 2.0), back.apply(mx, my));
                        (t.x, t.y) = (t.x + to.0 - from.0, t.y + to.1 - from.1);
                    }
                    None => (t.x, t.y) = (t.x + cw / 2.0 - mx, t.y + ch / 2.0 - my),
                }
                self.set_transform(scene, id, &t).map(drop)
            }
            E::Reset => {
                let mut t = self.transform(scene, id)?;
                (t.x, t.y, t.alignment, t.rotation, t.scale_x, t.scale_y) = (0.0, 0.0, 5, 0.0, 1.0, 1.0);
                (t.bounds, t.bounds_width, t.bounds_height) = ("none".into(), 0.0, 0.0);
                (t.crop_left, t.crop_top, t.crop_right, t.crop_bottom) = (0, 0, 0, 0);
                self.set_transform(scene, id, &t).map(drop)
            }
            E::FlipH | E::FlipV => self.change_in_place(scene, id, |t| {
                if edit == E::FlipH {
                    t.scale_x = -t.scale_x;
                } else {
                    t.scale_y = -t.scale_y;
                }
            }),
            E::RotateCw | E::RotateCcw | E::Rotate180 => {
                let by = match edit {
                    E::RotateCw => 90.0,
                    E::RotateCcw => -90.0,
                    _ => 180.0,
                };
                self.change_in_place(scene, id, |t| t.rotation = (t.rotation + by).rem_euclid(360.0))
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
                let src_flts = src.get("filters").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                if let Some(ov) = it.get("ferrocast_filters").and_then(Value::as_array) {
                    let mut combined = clone_filters_with_fresh_uuids(ov, |_| true);
                    combined.extend(clone_filters_with_fresh_uuids(src_flts, |f| {
                        ferrocore::obs::is_audio_filter_kind(kind_of(f))
                    }));
                    s["filters"] = Value::Array(combined);
                } else {
                    s["filters"] = Value::Array(clone_filters_with_fresh_uuids(src_flts, |_| true));
                }
                self.fresh_ids(scene, &mut s);
                self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(s.clone())));
                // Inside a group: the copy goes in the same group, just above the original (OBS).
                let home = self.home(scene, id);
                if home != scene {
                    let next = self.top_id(scene) + 1;
                    let mut c = it.clone();
                    if let Some(obj) = c.as_object_mut() {
                        obj.remove("ferrocast_filters");
                    }
                    c["id"] = json!(next);
                    c["name"] = json!(copy);
                    c["source_uuid"] = s["uuid"].clone();
                    self.with_items(&home, |l, _| {
                        // Saved bottom first: just after it in the list is just above it.
                        let at = l.iter().position(|i| i.get("id").and_then(Value::as_i64) == Some(id));
                        l.insert(at.map_or(l.len(), |a| a + 1), c);
                    })?;
                    return self.with_items(scene, |_, set| {
                        set.insert("id_counter".into(), json!(next));
                    });
                }
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
                        SHOW_KEY,
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
                let capture = (kind_of(s) == "screen_capture")
                    .then(|| s.pointer("/settings/type").and_then(Value::as_u64).unwrap_or(0));
                SourceUse { kind: kind_of(s).to_string(), name, scenes, capture }
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
                    ) && !PICTURE_ONLY_DEVICES.contains(&kind_of(x))
                })
                .map(|x| name_of(x).to_string())
                .collect();
            o.choices = std::iter::once("Sound on air".to_string()).chain(heard.iter().cloned()).collect();
            o.values = std::iter::once(json!("")).chain(heard.into_iter().map(Value::from)).collect();
        }
        // A camera or capture card: only the sizes and rates it really has (a USB card offered
        // 4K sent a squeezed 1920×2160).
        #[cfg(target_os = "macos")]
        if let Some(id) = ferrocore::obs::camera_device(kind_of(&s), &s["settings"]) {
            let modes = ferrocore::camera::modes(&id);
            if !modes.is_empty() {
                let size = s
                    .pointer("/settings/resolution")
                    .and_then(Value::as_str)
                    .and_then(|r| r.split_once('x'))
                    .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
                let fps = s.pointer("/settings/fps").and_then(Value::as_u64).unwrap_or(0);
                let own = ferrocore::camera::own_size(&id);
                let (sizes, rates) = crate::kinds::real_size_rate(&modes, size, fps, own);
                for (key, list) in [("resolution", sizes), ("fps", rates)] {
                    if let Some(o) = opts.iter_mut().find(|o| o.key == key) {
                        o.choices = list.iter().map(|(l, _)| l.clone()).collect();
                        o.values = list.into_iter().map(|(_, v)| v).collect();
                    }
                }
            }
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
            if ["window", "display", "monitor", "type"].iter().any(|k| new.contains_key(*k)) {
                let kind = kind_of(s).to_string();
                remember_capture(&kind, &mut s["settings"], new.contains_key("window"));
            }
            // A camera's new size that can't reach its saved frame rate gets the best rate it
            // can, so the form never says 50 while the card sends 30.
            #[cfg(target_os = "macos")]
            if new.contains_key("resolution")
                && !new.contains_key("fps")
                && let Some(id) = ferrocore::obs::camera_device(kind_of(s), &s["settings"])
            {
                let size = s["settings"]["resolution"]
                    .as_str()
                    .and_then(|r| r.split_once('x'))
                    .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
                let fps = s["settings"]["fps"].as_u64().unwrap_or(0);
                if let Some(to) = crate::kinds::reachable_rate(&ferrocore::camera::modes(&id), size, fps) {
                    s["settings"]["fps"] = json!(to);
                }
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
            push_to_talk: s.get("push-to-talk").and_then(Value::as_bool).unwrap_or(false),
            push_to_talk_ms: s.get("push-to-talk-delay").and_then(Value::as_u64).unwrap_or(0).min(5_000) as u32,
            push_to_mute: s.get("push-to-mute").and_then(Value::as_bool).unwrap_or(false),
            push_to_mute_ms: s.get("push-to-mute-delay").and_then(Value::as_u64).unwrap_or(0).min(5_000) as u32,
            mic: s
                .get("ferrocast_mic")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| mic_by_default(kind_of(s), name_of(s), device_of(s), channel)),
            shown: true,
        }
    }

    /// "This is a mic" on or off for a sound (the live watch judges only mics): kept in the
    /// source, `None` back to what its kind says.
    pub fn set_mic(&self, name: &str, on: Option<bool>) -> Result<(), StudioError> {
        self.with_source(name, |s| match on {
            Some(on) => s["ferrocast_mic"] = json!(on),
            None => {
                if let Some(o) = s.as_object_mut() {
                    o.remove("ferrocast_mic");
                }
            }
        })
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

    /// Global channels plus every audio source in the collection (including off-scene sources),
    /// without rendering or saving anything.
    pub fn all_audio(&self) -> Vec<AudioInfo> {
        let mut out = self.channels();
        for s in self.sources_list() {
            if source_has_audio(&s) {
                let name = name_of(&s);
                if !out.iter().any(|a| a.name == name) {
                    out.push(self.audio_info(&s, None));
                }
            }
        }
        out
    }

    pub fn set_volume(&self, name: &str, volume_db: f32) -> Result<(), StudioError> {
        self.with_source(name, |s| s["volume"] = json!(mul_of(volume_db.clamp(-100.0, 26.0))))
    }

    pub fn set_muted(&self, name: &str, muted: bool) -> Result<(), StudioError> {
        self.with_source(name, |s| s["muted"] = json!(muted))
    }

    /// Push-to-talk (`talk`) or push-to-mute on or off for a sound, and its release delay (OBS's
    /// keys: `push-to-talk`, `push-to-talk-delay`, in ms, at most 5 s).
    pub fn set_push(&self, name: &str, talk: bool, on: Option<bool>, delay_ms: Option<u32>) -> Result<(), StudioError> {
        let key = if talk { "push-to-talk" } else { "push-to-mute" };
        self.with_source(name, |s| {
            if let Some(on) = on {
                s[key] = json!(on);
            }
            if let Some(ms) = delay_ms {
                s[format!("{key}-delay")] = json!(ms.min(5_000));
            }
        })
    }

    /// A push-to-talk (`talk`) or push-to-mute key went down or up for a sound. False when the
    /// sound hasn't that switched on (the caller then does what a key did before: toggles the mute).
    pub fn push_key(&self, name: &str, talk: bool, down: bool) -> Result<bool, StudioError> {
        let on = {
            let src = self.source(name)?;
            src.get(if talk { "push-to-talk" } else { "push-to-mute" }).and_then(Value::as_bool).unwrap_or(false)
        };
        if on
            && let Some(e) = self.engine.borrow().clone()
            && let Some((id, _)) = e.engine.source_named(name)
        {
            e.engine.push_key(id, talk, down);
        }
        Ok(on)
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
        self.target_filters(FilterTarget::Source(name))
    }

    pub fn item_filters(&self, scene: &str, id: i64) -> Result<Vec<FilterInfo>, StudioError> {
        self.target_filters(FilterTarget::Item { scene, id })
    }

    fn target_filters(&self, target: FilterTarget<'_>) -> Result<Vec<FilterInfo>, StudioError> {
        let to_info = |f: &Value, scope: Option<&str>| FilterInfo {
            name: name_of(f).to_string(),
            kind: kind_of(f).to_string(),
            enabled: f.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            scope: scope.map(str::to_string),
        };
        match target {
            FilterTarget::Source(name) => Ok(self.filter_list(name)?.iter().map(|f| to_info(f, None)).collect()),
            FilterTarget::Item { scene, id } => {
                let it = self.item(scene, id)?;
                let src = self.filter_list(name_of(&it))?;
                if let Some(ov) = it.get("ferrocast_filters").and_then(Value::as_array) {
                    let mut out: Vec<FilterInfo> = ov.iter().map(|f| to_info(f, Some("item"))).collect();
                    out.extend(
                        src.iter()
                            .filter(|f| ferrocore::obs::is_audio_filter_kind(kind_of(f)))
                            .map(|f| to_info(f, Some("source"))),
                    );
                    Ok(out)
                } else {
                    Ok(src.iter().map(|f| to_info(f, Some("source"))).collect())
                }
            }
        }
    }

    fn with_filters<T>(&self, name: &str, f: impl FnOnce(&mut Vec<Value>) -> T) -> Result<T, StudioError> {
        self.with_source(name, |s| {
            let mut list = s.get("filters").and_then(Value::as_array).cloned().unwrap_or_default();
            let out = f(&mut list);
            s["filters"] = Value::Array(list);
            out
        })
    }

    /// Switches a scene item between `"This picture only"` (`own = true`: copies the source's
    /// video filters into an item override) and `"Everywhere this source is used"` (`own = false`:
    /// clears the item override so it shares the source's filters again). Audio filters always stay
    /// per source.
    pub fn set_item_filter_scope(&self, scene: &str, id: i64, own: bool) -> Result<ItemInfo, StudioError> {
        let it = self.item(scene, id)?;
        let src = self.source(name_of(&it))?;
        if own {
            if !has_item_filter_override(&it) {
                let video = video_filters_from(&src);
                self.with_item(scene, id, |i| {
                    i["ferrocast_filters"] = Value::Array(video);
                })?;
            }
        } else if has_item_filter_override(&it) {
            self.with_item(scene, id, |i| {
                if let Some(obj) = i.as_object_mut() {
                    obj.remove("ferrocast_filters");
                }
            })?;
        }
        self.item(scene, id).map(|i| self.info_of(&i))
    }

    fn target_owner_label(&self, target: FilterTarget<'_>) -> Result<String, StudioError> {
        match target {
            FilterTarget::Source(name) => Ok(name.to_string()),
            FilterTarget::Item { scene, id } => {
                let it = self.item(scene, id)?;
                Ok(name_of(&it).to_string())
            }
        }
    }

    /// Resolves `(source_name, write_to_item_override)` for mutating a filter on `target`.
    fn resolve_filter_store(
        &self,
        target: FilterTarget<'_>,
        kind: Option<&str>,
        filter_name: Option<&str>,
    ) -> Result<(String, bool), StudioError> {
        match target {
            FilterTarget::Source(name) => Ok((name.to_string(), false)),
            FilterTarget::Item { scene, id } => {
                let it = self.item(scene, id)?;
                let src_name = name_of(&it).to_string();
                let src = self.source(&src_name)?;
                let Some(ov) = it.get("ferrocast_filters").and_then(Value::as_array) else {
                    return Ok((src_name, false));
                };
                if let Some(k) = kind {
                    return Ok((src_name, !ferrocore::obs::is_audio_filter_kind(k)));
                }
                if let Some(fname) = filter_name {
                    if ov.iter().any(|f| name_of(f) == fname) {
                        return Ok((src_name, true));
                    }
                    let is_src_audio = src.get("filters").and_then(Value::as_array).is_some_and(|l| {
                        l.iter().any(|f| name_of(f) == fname && ferrocore::obs::is_audio_filter_kind(kind_of(f)))
                    });
                    return Ok((src_name, !is_src_audio));
                }
                Ok((src_name, true))
            }
        }
    }

    fn with_target_filters<T>(
        &self,
        target: FilterTarget<'_>,
        kind: Option<&str>,
        filter_name: Option<&str>,
        f: impl FnOnce(&mut Vec<Value>) -> T,
    ) -> Result<T, StudioError> {
        let (src_name, on_item) = self.resolve_filter_store(target, kind, filter_name)?;
        match (target, on_item) {
            (FilterTarget::Item { scene, id }, true) => self.with_item(scene, id, |it| {
                let mut list = it.get("ferrocast_filters").and_then(Value::as_array).cloned().unwrap_or_default();
                let out = f(&mut list);
                it["ferrocast_filters"] = Value::Array(list);
                out
            }),
            _ => self.with_filters(&src_name, f),
        }
    }

    fn target_filter_value(&self, target: FilterTarget<'_>, filter: &str) -> Result<Value, StudioError> {
        let owner = self.target_owner_label(target)?;
        match target {
            FilterTarget::Source(name) => self
                .filter_list(name)?
                .into_iter()
                .find(|f| name_of(f) == filter)
                .ok_or_else(|| fail(format!("\"{owner}\" has no filter called \"{filter}\"."))),
            FilterTarget::Item { scene, id } => {
                let it = self.item(scene, id)?;
                let src = self.filter_list(name_of(&it))?;
                let found = match it.get("ferrocast_filters").and_then(Value::as_array) {
                    Some(ov) => ov.iter().find(|f| name_of(f) == filter).cloned().or_else(|| {
                        src.into_iter()
                            .find(|f| name_of(f) == filter && ferrocore::obs::is_audio_filter_kind(kind_of(f)))
                    }),
                    None => src.into_iter().find(|f| name_of(f) == filter),
                };
                found.ok_or_else(|| fail(format!("\"{owner}\" has no filter called \"{filter}\".")))
            }
        }
    }

    pub fn add_filter(&self, name: &str, kind: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        self.add_target_filter(FilterTarget::Source(name), kind, filter, settings)
    }

    pub fn add_item_filter(
        &self,
        scene: &str,
        id: i64,
        kind: &str,
        filter: &str,
        settings: &Value,
    ) -> Result<(), StudioError> {
        self.add_target_filter(FilterTarget::Item { scene, id }, kind, filter, settings)
    }

    fn add_target_filter(
        &self,
        target: FilterTarget<'_>,
        kind: &str,
        filter: &str,
        settings: &Value,
    ) -> Result<(), StudioError> {
        check_name(filter)?;
        let owner = self.target_owner_label(target)?;
        // Free in the list it's shown in, and in the one it's written to (a sound filter added
        // through an item goes to the source's own).
        let (store, on_item) = self.resolve_filter_store(target, Some(kind), None)?;
        let taken_in_store = !on_item
            && self.names_on_source(&store, ferrocore::obs::is_audio_filter_kind(kind))?.iter().any(|n| n == filter);
        if taken_in_store || self.target_filters(target)?.iter().any(|f| f.name == filter) {
            return Err(fail(format!("\"{owner}\" already has a filter called \"{filter}\".")));
        }
        let mut set = crate::kinds::defaults(kind).unwrap_or_else(|| json!({}));
        // A new shadow spills past the box and the picture keeps its size. Not a default: an
        // older one (without the key) keeps fitting into its box, as it always has.
        if kind == "ferrocast_shadow" {
            set["outside"] = json!(true);
        }
        if let (Some(base), Some(given)) = (set.as_object_mut(), settings.as_object()) {
            for (k, v) in given {
                base.insert(k.clone(), v.clone());
            }
        }
        let f = json!({
            "id": kind, "versioned_id": kind, "name": filter, "uuid": uuid(), "enabled": true,
            "settings": set,
        });
        self.with_target_filters(target, Some(kind), None, |l| l.push(f))
    }

    pub fn remove_filter(&self, name: &str, filter: &str) -> Result<(), StudioError> {
        self.remove_target_filter(FilterTarget::Source(name), filter)
    }

    pub fn remove_item_filter(&self, scene: &str, id: i64, filter: &str) -> Result<(), StudioError> {
        self.remove_target_filter(FilterTarget::Item { scene, id }, filter)
    }

    fn remove_target_filter(&self, target: FilterTarget<'_>, filter: &str) -> Result<(), StudioError> {
        let owner = self.target_owner_label(target)?;
        let gone = self.with_target_filters(target, None, Some(filter), |l| {
            let n = l.len();
            l.retain(|f| name_of(f) != filter);
            n != l.len()
        })?;
        if gone { Ok(()) } else { Err(fail(format!("\"{owner}\" has no filter called \"{filter}\"."))) }
    }

    /// Switches a source's filter. The item overrides that came from it (a copy made when the
    /// source was added to another scene) have a filter of the same name and kind: they follow,
    /// so a hotkey, a Stream Deck button or obs-websocket reaches every scene the source is in.
    /// An item's own filter of that name that is a different kind is left alone.
    /// Switches a source's own filter on or off and nothing else: a picture's own copy of it (an
    /// item with its own filters) keeps its state. A held key uses this, so letting it go can't
    /// change what another picture had.
    pub fn set_source_filter_enabled(&self, name: &str, filter: &str, on: bool) -> Result<(), StudioError> {
        self.set_target_filter_enabled(FilterTarget::Source(name), filter, on)
    }

    pub fn set_filter_enabled(&self, name: &str, filter: &str, on: bool) -> Result<(), StudioError> {
        let kind = self.filter_list(name)?.iter().find(|f| name_of(f) == filter).map(|f| kind_of(f).to_string());
        self.set_target_filter_enabled(FilterTarget::Source(name), filter, on)?;
        let Some(kind) = kind else { return Ok(()) };
        let follows = |it: &Value| {
            name_of(it) == name
                && it.get("ferrocast_filters").and_then(Value::as_array).is_some_and(|l| {
                    l.iter().any(|f| {
                        name_of(f) == filter && kind_of(f) == kind && f["enabled"].as_bool().unwrap_or(true) != on
                    })
                })
        };
        let any = self
            .sources_list()
            .iter()
            .any(|s| s.pointer("/settings/items").and_then(Value::as_array).is_some_and(|l| l.iter().any(follows)));
        if any {
            self.edit(|d| {
                for s in d["sources"].as_array_mut().into_iter().flatten() {
                    for it in s.pointer_mut("/settings/items").and_then(Value::as_array_mut).into_iter().flatten() {
                        if !follows(it) {
                            continue;
                        }
                        for f in it["ferrocast_filters"].as_array_mut().into_iter().flatten() {
                            if name_of(f) == filter && kind_of(f) == kind {
                                f["enabled"] = json!(on);
                            }
                        }
                    }
                }
            });
        }
        Ok(())
    }

    pub fn set_item_filter_enabled(&self, scene: &str, id: i64, filter: &str, on: bool) -> Result<(), StudioError> {
        self.set_target_filter_enabled(FilterTarget::Item { scene, id }, filter, on)
    }

    fn set_target_filter_enabled(&self, target: FilterTarget<'_>, filter: &str, on: bool) -> Result<(), StudioError> {
        self.with_target_filter(target, filter, |f| f["enabled"] = json!(on))
    }

    pub fn filter_settings(&self, name: &str, filter: &str) -> Result<Value, StudioError> {
        self.target_filter_settings(FilterTarget::Source(name), filter)
    }

    pub fn item_filter_settings(&self, scene: &str, id: i64, filter: &str) -> Result<Value, StudioError> {
        self.target_filter_settings(FilterTarget::Item { scene, id }, filter)
    }

    fn target_filter_settings(&self, target: FilterTarget<'_>, filter: &str) -> Result<Value, StudioError> {
        self.target_filter_value(target, filter).map(|f| with_defaults(&f))
    }

    /// A filter's settings form.
    pub fn filter_options(&self, name: &str, filter: &str) -> Result<Vec<crate::OptionInfo>, StudioError> {
        self.target_filter_options(FilterTarget::Source(name), filter)
    }

    pub fn item_filter_options(
        &self,
        scene: &str,
        id: i64,
        filter: &str,
    ) -> Result<Vec<crate::OptionInfo>, StudioError> {
        self.target_filter_options(FilterTarget::Item { scene, id }, filter)
    }

    fn target_filter_options(
        &self,
        target: FilterTarget<'_>,
        filter: &str,
    ) -> Result<Vec<crate::OptionInfo>, StudioError> {
        let owner = self.target_owner_label(target)?;
        let f = self.target_filter_value(target, filter)?;
        let kind = kind_of(&f);
        let mut opts = crate::kinds::options(kind)
            .ok_or_else(|| fail(format!("Ferrocast's own engine doesn't run \"{filter}\" yet.")))?;
        if kind == "compressor_filter"
            && let Some(o) = opts.iter_mut().find(|o| o.key == "sidechain_source")
        {
            let mut choices = vec!["None".to_string()];
            let mut values = vec![json!("none")];
            let entries = self.audio_source_entries(&owner);
            let saved = f
                .pointer("/settings/sidechain_source")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"));
            let mut matched_saved = false;
            for (src_name, uuid) in &entries {
                choices.push(src_name.clone());
                if let Some(sv) = saved
                    && sv != src_name
                    && uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(sv))
                {
                    values.push(json!(sv));
                    matched_saved = true;
                } else {
                    if saved == Some(src_name.as_str()) {
                        matched_saved = true;
                    }
                    values.push(json!(src_name));
                }
            }
            if let Some(sv) = saved
                && !matched_saved
            {
                choices.push(format!("{sv} (missing)"));
                values.push(json!(sv));
            }
            o.choices = choices;
            o.values = values;
        }
        Ok(opts)
    }

    /// Whether `src` makes sound a compressor could duck under: it has a sound track, and the import
    /// gives its kind sound on this platform (a Windows display capture has none: the import drops a
    /// compressor keyed to it).
    fn has_sound_to_key(&self, src: &Value) -> bool {
        src.get("mixers").and_then(Value::as_u64).is_some_and(|m| m > 0)
            && ferrocore::obs::source_has_sound(src, (self.canvas.width, self.canvas.height))
    }

    /// Other audio sources in this collection (global channels first, then scene sources),
    /// excluding `exclude` and non-audio sources, for sidechain pickers.
    fn audio_source_entries(&self, exclude: &str) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = Vec::new();
        for s in self.channels_list() {
            let n = name_of(&s);
            if n.is_empty() || n == exclude || out.iter().any(|(x, _)| x == n) {
                continue;
            }
            let uuid = s.get("uuid").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string);
            out.push((n.to_string(), uuid));
        }
        for s in self.sources_list() {
            let n = name_of(&s);
            if n.is_empty() || n == exclude || is_scene(&s) || out.iter().any(|(x, _)| x == n) {
                continue;
            }
            if !is_audio_kind(kind_of(&s)) || !self.has_sound_to_key(&s) {
                continue;
            }
            let uuid = s.get("uuid").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string);
            out.push((n.to_string(), uuid));
        }
        out
    }

    /// Broken sidechain compressor loops in the current scene collection (`(source, key)`).
    pub fn sidechain_cycles(&self) -> Vec<(String, String)> {
        ferrocore::obs::sidechain_cycles(&self.doc.borrow())
    }

    pub fn update_filter(&self, name: &str, filter: &str, settings: &Value) -> Result<(), StudioError> {
        self.update_target_filter(FilterTarget::Source(name), filter, settings)
    }

    pub fn update_item_filter(&self, scene: &str, id: i64, filter: &str, settings: &Value) -> Result<(), StudioError> {
        self.update_target_filter(FilterTarget::Item { scene, id }, filter, settings)
    }

    fn update_target_filter(
        &self,
        target: FilterTarget<'_>,
        filter: &str,
        settings: &Value,
    ) -> Result<(), StudioError> {
        let Some(new) = settings.as_object() else { return Err(fail("settings: an object.")) };
        self.with_target_filter(target, filter, |f| {
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
        self.move_target_filter(FilterTarget::Source(name), filter, edit)
    }

    pub fn move_item_filter(&self, scene: &str, id: i64, filter: &str, edit: ItemEdit) -> Result<(), StudioError> {
        self.move_target_filter(FilterTarget::Item { scene, id }, filter, edit)
    }

    fn move_target_filter(&self, target: FilterTarget<'_>, filter: &str, edit: ItemEdit) -> Result<(), StudioError> {
        self.with_target_filters(target, None, Some(filter), |l| {
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
        self.paste_target_filters(FilterTarget::Source(name), FilterTarget::Source(from))
    }

    /// Copies filters from a source (`from_item: None`) or an item override (`from_item: Some`)
    /// onto `name`.
    pub fn paste_filters_from(
        &self,
        name: &str,
        from: &str,
        from_item: Option<(&str, i64)>,
    ) -> Result<(), StudioError> {
        let src = match from_item {
            Some((scene, id)) => FilterTarget::Item { scene, id },
            None => FilterTarget::Source(from),
        };
        self.paste_target_filters(FilterTarget::Source(name), src)
    }

    /// Copies `from`'s video (and when shared or audio, audio) filters onto a scene item.
    pub fn paste_item_filters(&self, scene: &str, id: i64, from: &str) -> Result<(), StudioError> {
        self.paste_target_filters(FilterTarget::Item { scene, id }, FilterTarget::Source(from))
    }

    /// Copies filters from a source (`from_item: None`) or another item override onto `(scene, id)`.
    pub fn paste_item_filters_from(
        &self,
        scene: &str,
        id: i64,
        from: &str,
        from_item: Option<(&str, i64)>,
    ) -> Result<(), StudioError> {
        let src = match from_item {
            Some((fscene, fid)) => FilterTarget::Item { scene: fscene, id: fid },
            None => FilterTarget::Source(from),
        };
        self.paste_target_filters(FilterTarget::Item { scene, id }, src)
    }

    /// The filter names a new filter written to `source`'s own list must not take: that list's,
    /// and for a sound filter (also shown in every item's own list of that source) theirs too.
    fn names_on_source(&self, source: &str, audio: bool) -> Result<Vec<String>, StudioError> {
        let mut names: Vec<String> = self.filter_list(source)?.iter().map(|f| name_of(f).to_string()).collect();
        if audio {
            for s in self.sources_list() {
                for it in s.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten() {
                    if name_of(it) == source {
                        let own = it.get("ferrocast_filters").and_then(Value::as_array).into_iter().flatten();
                        names.extend(own.map(|f| name_of(f).to_string()));
                    }
                }
            }
        }
        Ok(names)
    }

    fn target_filter_values(&self, target: FilterTarget<'_>) -> Result<Vec<Value>, StudioError> {
        match target {
            FilterTarget::Source(name) => self.filter_list(name),
            FilterTarget::Item { scene, id } => {
                let it = self.item(scene, id)?;
                let src = self.filter_list(name_of(&it))?;
                match it.get("ferrocast_filters").and_then(Value::as_array) {
                    Some(ov) => {
                        let mut out = ov.clone();
                        out.extend(src.into_iter().filter(|f| ferrocore::obs::is_audio_filter_kind(kind_of(f))));
                        Ok(out)
                    }
                    None => Ok(src),
                }
            }
        }
    }

    fn paste_target_filters(&self, target: FilterTarget<'_>, from: FilterTarget<'_>) -> Result<(), StudioError> {
        let copies = self.target_filter_values(from)?;
        self.paste_filter_list(target, copies)
    }

    /// Adds `copies` (filters as copied) to `target`, each its own copy with a name that's free.
    fn paste_filter_list(&self, target: FilterTarget<'_>, copies: Vec<Value>) -> Result<(), StudioError> {
        let mut have: Vec<String> = self.target_filters(target)?.into_iter().map(|f| f.name).collect();
        // And the names in the source's own list, where what isn't an item's own lands.
        let (store, _) = self.resolve_filter_store(target, None, None)?;
        have.extend(self.names_on_source(&store, true)?);
        let rename_unique = |f: &mut Value, have: &mut Vec<String>| {
            let base = name_of(f).to_string();
            let mut n = base.clone();
            let mut k = 2;
            while have.contains(&n) && k < 10_000 {
                // Numbered like a source's name, and never past the length a name may have.
                n = numbered(&base, k);
                k += 1;
            }
            have.push(n.clone());
            f["name"] = json!(n);
            f["uuid"] = json!(uuid());
        };
        if let FilterTarget::Item { scene, id } = target {
            let it = self.item(scene, id)?;
            if has_item_filter_override(&it) {
                let (mut video, mut audio) = (Vec::new(), Vec::new());
                for mut f in copies {
                    rename_unique(&mut f, &mut have);
                    if ferrocore::obs::is_audio_filter_kind(kind_of(&f)) {
                        audio.push(f);
                    } else {
                        video.push(f);
                    }
                }
                if !video.is_empty() {
                    self.with_item(scene, id, |i| {
                        let mut list =
                            i.get("ferrocast_filters").and_then(Value::as_array).cloned().unwrap_or_default();
                        list.extend(video);
                        i["ferrocast_filters"] = Value::Array(list);
                    })?;
                }
                if !audio.is_empty() {
                    self.with_filters(name_of(&it), |l| l.extend(audio))?;
                }
                return Ok(());
            }
        }
        let mut renamed = Vec::with_capacity(copies.len());
        for mut f in copies {
            rename_unique(&mut f, &mut have);
            renamed.push(f);
        }
        self.with_target_filters(target, None, None, |l| l.extend(renamed))
    }

    // ------------------------------------------------------------------ clipboard

    /// Copies an item with its source (a group with every source inside it) out of this
    /// collection, as plain data the Studio keeps while another collection is open.
    pub fn copy_source(&self, scene: &str, id: i64) -> Result<SourceClip, StudioError> {
        let mut item = self.item(scene, id)?;
        // A source inside a group keeps its place on the canvas: its stored position is the
        // group's local one (scale and turn of the group are not carried).
        if let Some(g) = self.group_of(scene, id) {
            let x = item.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let y = item.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let (cx, cy) = self.group_map(&g).apply(x, y);
            item["pos"] = json!({ "x": cx, "y": cy });
        }
        let root = self.source(name_of(&item))?;
        let mut sources = Vec::new();
        self.collect_clip(&root, &mut sources, 0)?;
        Ok(SourceClip { item, sources })
    }

    fn collect_clip(&self, src: &Value, out: &mut Vec<Value>, depth: usize) -> Result<(), StudioError> {
        if kind_of(src) == "scene" {
            return Err(fail(format!("\"{}\" is a scene: duplicate the scene instead of copying it.", name_of(src))));
        }
        if depth > 16 {
            return Err(fail("Groups inside groups go too deep to copy."));
        }
        if out.iter().any(|o| name_of(o) == name_of(src)) {
            return Ok(());
        }
        out.push(src.clone());
        if kind_of(src) == "group" {
            for child in src.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten() {
                let c = self.source(name_of(child))?;
                self.collect_clip(&c, out, depth + 1)?;
            }
        }
        Ok(())
    }

    /// Puts a copied item and its sources into `scene` (of any collection) → the new item and
    /// notes on what changed. A name taken becomes "Name 2"; a clone whose target isn't here is
    /// an error; a kind this computer can't run is refused; a sidechain to a source that isn't
    /// here is set to none.
    pub fn paste_source(&self, scene: &str, clip: &SourceClip) -> Result<(ItemInfo, Vec<String>), StudioError> {
        self.scene_items(scene)?;
        let root_name = clip.sources.first().map(|s| name_of(s).to_string()).unwrap_or_default();
        if clip.sources.is_empty() || name_of(&clip.item) != root_name {
            return Err(fail("The copied source is empty."));
        }
        let can = crate::kinds::kinds().inputs;
        for src in &clip.sources {
            let kind = kind_of(src);
            let here = self.sources_list().iter().any(|o| kind_of(o) == kind);
            if kind != "group" && !can.iter().any(|k| k.id == kind) && !here {
                return Err(fail(format!(
                    "\"{}\" is a {kind} source, which this computer can't run (it was copied on another system).",
                    name_of(src)
                )));
            }
        }
        // Every filter on it, too, must be one this computer has: nothing is added otherwise.
        let can_filter = crate::kinds::kinds().filters;
        let attached = clip
            .sources
            .iter()
            .flat_map(|s| s.get("filters").and_then(Value::as_array).into_iter().flatten())
            .chain(clip.item.get("ferrocast_filters").and_then(Value::as_array).into_iter().flatten())
            .chain(
                clip.sources
                    .iter()
                    .filter(|s| kind_of(s) == "group")
                    .flat_map(|g| g.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten())
                    .flat_map(|it| it.get("ferrocast_filters").and_then(Value::as_array).into_iter().flatten()),
            );
        let have_filter = |k: &str| {
            self.sources_list().iter().any(|o| {
                let own = |l: &Value| {
                    l.get("filters").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| kind_of(x) == k))
                };
                own(o)
                    || o.pointer("/settings/items").and_then(Value::as_array).is_some_and(|items| {
                        items.iter().any(|it| {
                            it.get("ferrocast_filters")
                                .and_then(Value::as_array)
                                .is_some_and(|f| f.iter().any(|x| kind_of(x) == k))
                        })
                    })
            })
        };
        if let Some(f) =
            attached.into_iter().find(|f| !can_filter.iter().any(|k| k.id == kind_of(f)) && !have_filter(kind_of(f)))
        {
            return Err(fail(format!(
                "The filter \"{}\" is a {} filter, which this computer can't run (it was copied on another system).",
                name_of(f),
                kind_of(f)
            )));
        }
        // New names, in order, each free here and unlike the others pasted with it.
        let mut map: Vec<(String, String)> = Vec::new();
        for src in &clip.sources {
            let old = name_of(src).to_string();
            let mut n = old.clone();
            let mut k = 2;
            while self.name_free(&n).is_err() || map.iter().any(|(_, new)| *new == n) {
                n = numbered(&old, k);
                k += 1;
                if k > 10_000 {
                    return Err(fail(format!("Couldn't find a free name for \"{old}\": rename it first.")));
                }
            }
            map.push((old, n));
        }
        // One fresh uuid for each source by its place in the copy: two with no uuid of their own
        // must not share one.
        let fresh_ids: Vec<String> = clip.sources.iter().map(|_| uuid()).collect();
        let new_uuids: Vec<(String, String)> = clip
            .sources
            .iter()
            .zip(&fresh_ids)
            .map(|(c, n)| (c["uuid"].as_str().unwrap_or("").to_string(), n.clone()))
            .collect();
        // What a name (or a uuid) pointed at before points at after: a source pasted with this
        // one by its new name or its new uuid, else nothing (it may still be here).
        let renamed = |old: &str| {
            map.iter()
                .find(|(o, _)| o == old)
                .map(|(_, n)| n.clone())
                .or_else(|| new_uuids.iter().find(|(u, _)| !u.is_empty() && u == old).map(|(_, n)| n.clone()))
        };
        let mut notes = Vec::new();
        let mut fresh: Vec<Value> = Vec::new();
        for (si, src) in clip.sources.iter().enumerate() {
            let mut c = src.clone();
            let old = name_of(src).to_string();
            let new = renamed(&old).unwrap_or_else(|| old.clone());
            c["name"] = json!(new);
            c["uuid"] = json!(fresh_ids[si]);
            if matches!(kind_of(&c), "ferrocast_clone" | "source-clone") {
                let target = c.pointer("/settings/clone").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(n) = renamed(&target) {
                    c["settings"]["clone"] = json!(n);
                } else if !target.is_empty() && self.source(&target).is_err() {
                    return Err(fail(format!(
                        "\"{old}\" shows \"{target}\", which isn't in this collection: paste that source first."
                    )));
                }
            }
            // A Sound bars source listens to another source: the copy listens to the pasted one.
            if kind_of(&c) == "ferrocast_visualizer"
                && let Some(n) = c.pointer("/settings/source").and_then(Value::as_str).and_then(renamed)
            {
                c["settings"]["source"] = json!(n);
            }
            if let Some(fs) = c.get_mut("filters").and_then(Value::as_array_mut) {
                self.fix_pasted_filters(fs, &new, &renamed, &mut notes);
            }
            fresh.push(c);
        }
        // A group's items point at the pasted children by their new names.
        let uuids: Vec<(String, String)> =
            fresh.iter().map(|c| (name_of(c).to_string(), c["uuid"].as_str().unwrap_or("").to_string())).collect();
        for c in fresh.iter_mut().filter(|c| kind_of(c) == "group") {
            if let Some(items) = c.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
                for it in items {
                    let owner = name_of(it).to_string();
                    if let Some(own) = it.get_mut("ferrocast_filters").and_then(Value::as_array_mut) {
                        self.fix_pasted_filters(own, &owner, &renamed, &mut notes);
                    }
                    if let Some(n) = renamed(name_of(it)) {
                        it["source_uuid"] = json!(uuids.iter().find(|(x, _)| *x == n).map(|(_, u)| u.clone()));
                        it["name"] = json!(n);
                    }
                }
            }
        }
        let root = fresh[0].clone();
        let mut item = clip.item.clone();
        if let Some(ov) = item.get_mut("ferrocast_filters").and_then(Value::as_array_mut) {
            self.fix_pasted_filters(ov, name_of(&root), &renamed, &mut notes);
        }
        self.edit(|d| {
            if let Some(l) = d["sources"].as_array_mut() {
                // Children first, so a group never lists a source that isn't there yet.
                l.extend(fresh.iter().skip(1).cloned());
                l.push(fresh[0].clone());
            }
        });
        let placed = self.place(scene, &root)?;
        // A group's children get ids above everything in this scene, so none repeats one here.
        let mut next = self.top_id(scene).max(placed.id) + 1;
        for g in fresh.iter().filter(|c| kind_of(c) == "group") {
            self.with_source(name_of(g), |s| {
                if let Some(items) = s.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
                    for it in items {
                        it["id"] = json!(next);
                        next += 1;
                    }
                }
                s["settings"]["id_counter"] = json!(next - 1);
            })?;
        }
        // The scene's counter goes past the children too, so the next item added gets a new id.
        let high = next - 1;
        self.with_items(scene, |_, set| {
            let have = set.get("id_counter").and_then(Value::as_i64).unwrap_or(0);
            set.insert("id_counter".into(), json!(have.max(high)));
        })?;
        item["id"] = json!(placed.id);
        item["name"] = root["name"].clone();
        item["source_uuid"] = root["uuid"].clone();
        self.with_item(scene, placed.id, |i| *i = item)?;
        let info = self.info_of(&self.item(scene, placed.id)?);
        Ok((info, notes))
    }

    /// Fresh ids for pasted filters, and each sidechain to a source that isn't here set to none.
    fn fix_pasted_filters(
        &self,
        filters: &mut [Value],
        owner: &str,
        renamed: &dyn Fn(&str) -> Option<String>,
        notes: &mut Vec<String>,
    ) {
        for f in filters.iter_mut() {
            f["uuid"] = json!(uuid());
            if kind_of(f) != "compressor_filter" {
                continue;
            }
            let Some(side) = f.pointer("/settings/sidechain_source").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            if side.trim().is_empty() || side.trim().eq_ignore_ascii_case("none") {
                continue;
            }
            if let Some(n) = renamed(&side) {
                f["settings"]["sidechain_source"] = json!(n);
            } else if self.source(&side).is_err()
                && !self
                    .sources_list()
                    .iter()
                    .chain(self.channels_list().iter())
                    .any(|s| s["uuid"].as_str().is_some_and(|u| u.eq_ignore_ascii_case(&side)) || name_of(s) == side)
            {
                f["settings"]["sidechain_source"] = json!("none");
                notes.push(format!(
                    "The compressor \"{}\" on \"{owner}\" listened to \"{side}\", which isn't in this collection: its sidechain is set to None.",
                    name_of(f)
                ));
            }
        }
    }

    /// Copies a source's (or an item's own) filters out as plain data.
    pub fn copy_filters(&self, name: &str, item: Option<(&str, i64)>) -> Result<FilterClip, StudioError> {
        let target = match item {
            Some((scene, id)) => FilterTarget::Item { scene, id },
            None => FilterTarget::Source(name),
        };
        let filters = self.target_filter_values(target)?;
        let from = match item {
            Some((scene, id)) => name_of(&self.item(scene, id)?).to_string(),
            None => name.to_string(),
        };
        Ok(FilterClip { filters, from })
    }

    /// Pastes copied filters onto a source or an item → notes on what changed (a filter this
    /// computer can't run is refused; a sidechain to a source that isn't here is set to none).
    pub fn paste_filter_values(
        &self,
        name: &str,
        item: Option<(&str, i64)>,
        clip: &FilterClip,
    ) -> Result<Vec<String>, StudioError> {
        let can = crate::kinds::kinds().filters;
        if let Some(f) = clip.filters.iter().find(|f| !can.iter().any(|k| k.id == kind_of(f))) {
            return Err(fail(format!(
                "The filter \"{}\" is a {} filter, which this computer can't run.",
                name_of(f),
                kind_of(f)
            )));
        }
        let (target, owner) = match item {
            Some((scene, id)) => (FilterTarget::Item { scene, id }, name_of(&self.item(scene, id)?).to_string()),
            None => (FilterTarget::Source(name), name.to_string()),
        };
        // The target must exist before anything is changed.
        self.target_filters(target)?;
        let mut copies = clip.filters.clone();
        let mut notes = Vec::new();
        self.fix_pasted_filters(&mut copies, &owner, &|_| None, &mut notes);
        self.paste_filter_list(target, copies)?;
        Ok(notes)
    }

    fn with_target_filter(
        &self,
        target: FilterTarget<'_>,
        filter: &str,
        f: impl FnOnce(&mut Value),
    ) -> Result<(), StudioError> {
        let owner = self.target_owner_label(target)?;
        let found = self.with_target_filters(target, None, Some(filter), |l| {
            l.iter_mut().find(|x| name_of(x) == filter).map(f).is_some()
        })?;
        if found { Ok(()) } else { Err(fail(format!("\"{owner}\" has no filter called \"{filter}\"."))) }
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
        filters: bool,
    ) -> Result<Option<(Vec<u8>, u32, u32, (u32, u32))>, StudioError> {
        Ok(self.source_picture_job(name, width, filters)?.and_then(|job| job()))
    }

    /// [`Self::item_picture_job`] run on this thread.
    #[allow(clippy::type_complexity)]
    pub fn item_picture_pixels(
        &self,
        scene: &str,
        id: i64,
        width: u32,
        filters: bool,
    ) -> Result<Option<(Vec<u8>, u32, u32, (u32, u32))>, StudioError> {
        Ok(self.item_picture_job(scene, id, width, filters)?.and_then(|job| job()))
    }

    /// Like [`Self::item_picture_pixels`], failing if `expected_source` is given and does not
    /// match the item's source name.
    #[allow(clippy::type_complexity)]
    pub fn item_picture_pixels_for(
        &self,
        expected_source: Option<&str>,
        scene: &str,
        id: i64,
        width: u32,
        filters: bool,
    ) -> Result<Option<(Vec<u8>, u32, u32, (u32, u32))>, StudioError> {
        Ok(self.item_picture_job_for(expected_source, scene, id, width, filters)?.and_then(|job| job()))
    }

    /// [`Self::source_picture_pixels`] in two: what's needed is read here (on the window's
    /// thread), and the drawing and reading back off the graphics card is the job, for any
    /// thread: done on the window's thread it held the window up each time (the drawer and the
    /// Scenes grid ask every few seconds).
    #[allow(clippy::type_complexity)]
    pub fn source_picture_job(
        &self,
        name: &str,
        width: u32,
        filters: bool,
    ) -> Result<Option<impl FnOnce() -> Option<(Vec<u8>, u32, u32, (u32, u32))> + Send + 'static + use<>>, StudioError>
    {
        self.picture_job_for(name, None, width, filters)
    }

    /// Like [`Self::source_picture_job`], using one scene item's video filter list when
    /// `filters` is true.
    #[allow(clippy::type_complexity)]
    pub fn item_picture_job(
        &self,
        scene: &str,
        id: i64,
        width: u32,
        filters: bool,
    ) -> Result<Option<impl FnOnce() -> Option<(Vec<u8>, u32, u32, (u32, u32))> + Send + 'static + use<>>, StudioError>
    {
        self.item_picture_job_for(None, scene, id, width, filters)
    }

    /// Like [`Self::item_picture_job`], verifying `expected_source` matches the item when given.
    #[allow(clippy::type_complexity)]
    pub fn item_picture_job_for(
        &self,
        expected_source: Option<&str>,
        scene: &str,
        id: i64,
        width: u32,
        filters: bool,
    ) -> Result<Option<impl FnOnce() -> Option<(Vec<u8>, u32, u32, (u32, u32))> + Send + 'static + use<>>, StudioError>
    {
        let it = self.item(scene, id)?;
        let src = name_of(&it);
        if let Some(want) = expected_source
            && want != src
        {
            return Err(fail(format!("There's no source called \"{want}\" for item {id} in \"{scene}\".")));
        }
        let home = self.home(scene, id);
        self.picture_job_for(src, Some((home, id)), width, filters)
    }

    #[allow(clippy::type_complexity)]
    fn picture_job_for(
        &self,
        name: &str,
        item: Option<(String, i64)>,
        width: u32,
        filters: bool,
    ) -> Result<Option<impl FnOnce() -> Option<(Vec<u8>, u32, u32, (u32, u32))> + Send + 'static + use<>>, StudioError>
    {
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
        Ok(Some(move || {
            let px = if filters {
                match &item {
                    Some((home, item_id)) => e.engine.snapshot_item_filtered(home, *item_id, id, (w, h)),
                    None => e.engine.snapshot_source_filtered(id, (w, h)),
                }
            } else {
                e.engine.snapshot_source_at(id, (w, h))
            };
            px.map(|px| (px, w, h, (sw, sh)))
        }))
    }

    /// Reuses an already-open capture source matching `(kind, key, value)` for a picker
    /// thumbnail so the picker never opens a second camera session or grabs hardware unnecessarily.
    #[allow(clippy::type_complexity)]
    pub fn matching_device_picture_job(
        &self,
        kind: &str,
        key: &str,
        value: &Value,
        width: u32,
    ) -> Option<impl FnOnce() -> Option<(Vec<u8>, u32, u32)> + Send + 'static + use<>> {
        let want_camera = PICTURE_ONLY_DEVICES.contains(&kind);
        let want_screen = matches!(
            kind,
            "screen_capture" | "display_capture" | "window_capture" | "monitor_capture" | "xshm_input" | "game_capture"
        );
        if !want_camera && !want_screen {
            return None;
        }
        let matched = self.sources_list().into_iter().find(|s| {
            let sk = kind_of(s);
            let kind_matches = if want_camera {
                PICTURE_ONLY_DEVICES.contains(&sk)
            } else {
                sk == kind
                    || (matches!(sk, "screen_capture" | "display_capture" | "monitor_capture")
                        && matches!(kind, "screen_capture" | "display_capture" | "monitor_capture"))
            };
            if !kind_matches {
                return false;
            }
            let set = s.get("settings").and_then(Value::as_object);
            let Some(set) = set else { return false };
            if let Some(v) = set.get(key) {
                return v == value;
            }
            if want_camera {
                return ["device", "uid", "video_device_id"].iter().any(|k| set.get(*k).is_some_and(|v| v == value));
            }
            false
        })?;
        let name = name_of(&matched).to_string();
        let job = self.source_picture_job(&name, width.clamp(64, 320), false).ok().flatten()?;
        Some(move || {
            let (px, pw, ph, _) = job()?;
            Some((px, pw, ph))
        })
    }

    /// A scene's picture `width` wide as RGBA pixels (with its height).
    pub fn thumbnail_pixels(&self, scene: &str, width: u32) -> Result<(Vec<u8>, u32, u32), StudioError> {
        self.thumbnail_job(scene, width)?()
    }

    /// [`Self::thumbnail_pixels`] in two, as [`Self::source_picture_job`].
    #[allow(clippy::type_complexity)]
    pub fn thumbnail_job(
        &self,
        scene: &str,
        width: u32,
    ) -> Result<impl FnOnce() -> Result<(Vec<u8>, u32, u32), StudioError> + Send + 'static + use<>, StudioError> {
        self.scene_index(scene)?;
        let (cw, ch) = (self.canvas.width.max(1), self.canvas.height.max(1));
        let w = width.clamp(64, 640);
        let h = ((u64::from(w) * u64::from(ch)) / u64::from(cw)).max(1) as u32;
        let e = self.engine()?;
        let scene = scene.to_string();
        Ok(move || {
            let px = e
                .engine
                .snapshot_scene_at(&scene, (cw, ch), (w, h))
                .map_err(|x| fail(format!("\"{scene}\" couldn't be drawn: {x}")))?;
            Ok((px, w, h))
        })
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
        for what in own::not_applied(&s.encode, "") {
            say_once(format!("the stream starts without {what}: Ferrocore's encoders don't take them"));
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
            video_tracks: &s.video_tracks,
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
        for what in own::not_applied(&plan.encode, &plan.muxer) {
            say_once(format!("the recording starts without {what}: Ferrocore doesn't take them"));
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
            video_tracks: &[],
        };
        let mut own = self.output(how)?;
        own.set_plan(plan);
        Ok(StageOutput::new(own, Some(path), self.canvas))
    }

    /// A recording "Same as stream" that shares the running `stream`'s encoder (see
    /// [`can_share`]), into `path`.
    /// Whether `out` was started from this collection (only then can it share its encode: a
    /// recording kept through a switch to another collection draws that one; CodeRabbit).
    pub fn started(&self, out: &StageOutput) -> bool {
        self.engine.borrow().as_ref().is_some_and(|e| out.own.runs_on(e))
    }

    pub fn record_sharing(
        &self,
        stream: &StageOutput,
        path: &Path,
        plan: &RecordPlan,
    ) -> Result<StageOutput, StudioError> {
        for what in own::not_applied(&plan.encode, &plan.muxer) {
            say_once(format!("the recording starts without {what}: Ferrocore doesn't take them"));
        }
        if !self.started(stream) {
            return Err(StudioError::Failed("The stream runs in another collection.".into()));
        }
        let mut own = stream.own.record_joining(path, plan)?;
        own.set_plan(plan);
        Ok(StageOutput::new(own, Some(path.to_path_buf()), self.canvas))
    }

    /// A stream that shares the running `recording`'s encoder (a recording "Same as stream"
    /// started first; see [`can_share`]).
    pub fn stream_sharing(
        &self,
        recording: &StageOutput,
        server: &str,
        key: &str,
        s: &StreamSettings,
    ) -> Result<StageOutput, StudioError> {
        if !self.started(recording) {
            return Err(StudioError::Failed("The recording runs in another collection.".into()));
        }
        let send = own::Send { server, key, reconnect: s.reconnect };
        let own = recording.own.stream_joining(send, &s.encode, s.delay_s)?;
        Ok(StageOutput::new(own, None, self.canvas))
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
        let how = own::Drawing {
            send_to: None,
            record: None,
            encode: &plan.encode,
            tracks: &plan.tracks,
            delay_s: 0,
            vertical: false,
            replay: Some((plan.seconds, plan.mb)),
            solo: None,
            video_tracks: &[],
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
        // Studio Mode's live state: what the preview holds and the copy on air. Undo changes the
        // scenes, not what's on air, so these stay as they are now.
        let live: Vec<(&str, Value)> = ["studio_mode", "program_copy", "program_on_air", "program_prev"]
            .iter()
            .filter_map(|k| self.doc.borrow().pointer(&format!("/ferrocast/{k}")).map(|v| (*k, v.clone())))
            .collect();
        let studio_on = self.preview_scene().is_some();
        let current_scene = self.doc.borrow().get("current_scene").cloned();
        // The sources the copy on air shows, as they are now: undo may be to a time before one
        // existed, and the engine must not get an on-air item with no source.
        let old_sources = self.doc.borrow().get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
        self.edit(|d| {
            *d = snapshot.clone();
            let has = |d: &Value, n: &str| {
                d.get("sources").and_then(Value::as_array).is_some_and(|l| l.iter().any(|s| name_of(s) == n))
            };
            if has(d, &on_air) {
                d["current_program_scene"] = json!(on_air);
            }
            if studio_on {
                if !d.get("ferrocast").is_some_and(Value::is_object) {
                    d["ferrocast"] = json!({});
                }
                if let Some(f) = d["ferrocast"].as_object_mut() {
                    for k in ["studio_mode", "program_copy", "program_on_air", "program_prev"] {
                        f.remove(k);
                    }
                    for (k, v) in &live {
                        f.insert((*k).to_string(), v.clone());
                    }
                }
                // The preview is a scene that is still there: the live one, else the one the
                // snapshot had, else what's on air (a scene whose adding was undone is gone).
                let live_preview = live.iter().find(|(k, _)| *k == "studio_mode").and_then(|(_, v)| v.as_str());
                let snap_preview = snapshot.pointer("/ferrocast/studio_mode").and_then(Value::as_str);
                let snap_current = snapshot.get("current_scene").and_then(Value::as_str);
                let first = main_scenes(d).into_iter().next();
                let preview = [live_preview, snap_preview, Some(on_air.as_str()), snap_current]
                    .into_iter()
                    .flatten()
                    .find(|n| has(d, n))
                    .map(String::from)
                    // Last: any scene that is there (the live ones were all added after the snapshot).
                    .or(first);
                if let Some(p) = preview {
                    d["ferrocast"]["studio_mode"] = json!(p);
                    d["current_scene"] = json!(p);
                } else if let Some(c) = current_scene.as_ref().filter(|c| c.as_str().is_some_and(|n| has(d, n))) {
                    d["current_scene"] = c.clone();
                }
                // What the copy on air (and the one going off) shows keeps its sources, and what
                // those need in turn (a group's sources, the source a copy or a sound bar shows).
                let mut todo: Vec<String> = ["program_copy", "program_prev"]
                    .iter()
                    .filter_map(|k| d.pointer(&format!("/ferrocast/{k}/items")).and_then(Value::as_array))
                    .flatten()
                    .map(|i| name_of(i).to_string())
                    .collect();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                while let Some(n) = todo.pop() {
                    if !seen.insert(n.clone()) {
                        continue;
                    }
                    let have = d
                        .get("sources")
                        .and_then(Value::as_array)
                        .and_then(|l| l.iter().find(|x| name_of(x) == n))
                        .cloned();
                    let def = match have {
                        Some(def) => def,
                        None => {
                            let Some(def) = old_sources.iter().find(|x| name_of(x) == n).cloned() else { continue };
                            if let Some(list) = d.get_mut("sources").and_then(Value::as_array_mut) {
                                list.push(def.clone());
                            }
                            def
                        }
                    };
                    let items =
                        def.pointer("/settings/items").and_then(Value::as_array).into_iter().flatten().map(name_of);
                    let copied = match kind_of(&def) {
                        "ferrocast_clone" | "source-clone" => def.pointer("/settings/clone").and_then(Value::as_str),
                        "ferrocast_visualizer" => def.pointer("/settings/source").and_then(Value::as_str),
                        _ => None,
                    };
                    todo.extend(items.chain(copied).map(String::from));
                }
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
    fn send_copy_on_air_with(&self, scene: &str, kind: &str, ms: u64, settings: &Value) -> Result<(), StudioError> {
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
        if let Some(e) = self.engine.borrow().as_ref() {
            e.go_with(private, kind, ms, settings)?;
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
                let settings = self.transition_settings(&t.kind);
                self.send_copy_on_air_with(&pre, &t.kind, u64::from(t.ms), &settings)?;
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
        let (gw, gh) = self.item_size_of(&name, Some(&g));
        let to_canvas = ferrocore::draw_transform(&p, gw, gh);
        // Each side on its own, so a group scaled more one way keeps its sources' shapes.
        let (scale, turn) = (self.group_scale(&g), p.rotation);
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
                c["scale"] = json!({ "x": sx * scale.0, "y": sy * scale.1 });
                // A bounding box is in the group's pixels too.
                let (bw, bh) = (
                    c.pointer("/bounds/x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    c.pointer("/bounds/y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                );
                c["bounds"] = json!({ "x": bw * scale.0.abs(), "y": bh * scale.1.abs() });
                c["rot"] = json!((c.get("rot").and_then(Value::as_f64).unwrap_or(0.0) as f32 + turn).rem_euclid(360.0));
                // Where it is shown: the group's choice covered it, and goes on covering it. One
                // for the stream only in a group for the recording only was shown nowhere, so it
                // stays out of sight rather than suddenly appearing.
                use ferrocore::engine::Show;
                let (group_show, own) = (shown_of(&g), shown_of(&c));
                let (now, nowhere) = match (group_show, own) {
                    (Show::Both, own) => (own, false),
                    (g, Show::Both) => (g, false),
                    (g, own) if g == own => (own, false),
                    (_, own) => (own, true),
                };
                if let Some(o) = c.as_object_mut() {
                    if now == Show::Both {
                        o.remove(SHOW_KEY);
                    } else {
                        o.insert(SHOW_KEY.into(), json!(now.name()));
                    }
                    if nowhere {
                        o.insert("visible".into(), json!(false));
                    }
                }
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

    /// What the engine running now mixes and paces at: the sound rate, and whether it runs at the
    /// broadcast rate (29.97, 59.94, 23.976). None before it has started. These change only when
    /// the engine opens again.
    pub fn engine_format(&self) -> Option<(u32, bool)> {
        self.engine.borrow().as_ref().map(|s| {
            let c = s.engine.config();
            (c.sample_rate, c.ntsc)
        })
    }

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
        // A picture being drawn for the Scenes grid or the drawer holds the engine a moment
        // (off the window's thread): waited for, briefly.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while self.engine.borrow().as_ref().is_some_and(|e| Arc::strong_count(e) > 1)
            && std::time::Instant::now() < until
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if let Some(n) = self.engine.borrow().as_ref().map(Arc::strong_count).filter(|n| *n > 1) {
            tracing::warn!(holders = n - 1, "the picture size can't change: something still uses the engine");
            self.sync_pages();
            return Err(fail("The picture size can't change while streaming or recording. Stop first."));
        }
        *self.engine.borrow_mut() = None;
        publish_engine(None);
        self.canvas = canvas;
        Ok(())
    }

    /// How the engine is doing, as the Studio's stats show it.
    pub fn stats(&self) -> crate::EngineStats {
        let (cpu, memory_mb) = crate::system::usage();
        let s = self.engine.borrow().as_ref().map(|e| e.engine.stats()).unwrap_or_default();
        // Frames the encoders left out (FFmpeg's queue full, VideoToolbox's drops), apart from the
        // frames the engine didn't draw in time (`lagged`): the health meter names the encoder
        // from these. Never more than the frames given to them (a new engine counts from 0 again).
        let encoded = s.frames.min(u64::from(u32::MAX)) as u32;
        let skipped =
            ferrocore::video::dropped_frames().saturating_sub(self.dropped_base).min(u64::from(encoded)) as u32;
        crate::EngineStats {
            cpu,
            fps: self.fps_now(s.frames, s.skipped, std::time::Instant::now()),
            frame_ms: f64::from(s.frame_ms),
            lagged: s.skipped.min(u64::from(u32::MAX)) as u32,
            frames: s.frames.min(u64::from(u32::MAX)) as u32,
            memory_mb,
            skipped,
            encoded,
            recovered: s.recovered.min(u64::from(u32::MAX)) as u32,
            restarts: s.restarts.min(u64::from(u32::MAX)) as u32,
            encode_ms: f64::from(s.encode_ms),
            encoding_ms: s.encoding_us / 1000,
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
    ///
    /// Asking for a source's level starts the engine when it isn't running: the sources' sound
    /// is only opened by the engine, and the first-run setup's "Camera and mic check" asks for
    /// the mic's level with no preview on screen to have started it, so the meter never moved
    /// (owner, 24 Nov: "mic doesnt show here on setup when i talk").
    pub fn levels(&self, names: &[String]) -> std::collections::HashMap<String, crate::collection::LevelInfo> {
        let running = self.engine.borrow().clone();
        let Some(e) = running.or_else(|| if names.is_empty() { None } else { self.engine().ok() }) else {
            return Default::default();
        };
        read_levels(&e, names, false)
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
            let a = ferrocore::obs::app_name(a.as_str().unwrap_or_default());
            let a = a.as_str();
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
            [("file", "Image"), ("local_file", "File"), ("image_path", "Image"), ("text_file", "Text file")];
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
        for t in self.named_transitions() {
            for (k, label) in [("path", "Video"), ("luma_image", "Image")] {
                if let Some(p) = t.settings.get(k).and_then(Value::as_str)
                    && !p.is_empty()
                    && !Path::new(p).exists()
                {
                    out.push(crate::collection::MissingFile {
                        source: t.name.clone(),
                        parent: None,
                        kind: t.kind.clone(),
                        key: k.to_string(),
                        label: label.to_string(),
                        path: p.to_string(),
                    });
                }
            }
        }
        out
    }

    /// Points a missing file's setting at `to` instead.
    pub fn replace_file(&self, m: &crate::collection::MissingFile, to: &str) -> Result<(), StudioError> {
        let set = json!({ m.key.clone(): to });
        if is_valid_transition_kind(&m.kind) {
            if !m.source.is_empty() {
                // By name: a transition renamed or deleted meanwhile is an error, never a
                // silent overwrite of another transition's settings.
                return self.update_named_transition(&m.source, None, None, Some(&set), None).map(|_| ());
            }
            return self.set_transition_settings(&m.kind, &set);
        }
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
        } else if self.scene_index(name).is_ok() {
            (self.canvas.width, self.canvas.height)
        } else {
            // A source (OBS's Screenshot (Source)): drawn on its own, at its own size.
            self.source(name)?;
            let e = self.engine()?;
            let (scene, w, h) = crate::own::show_alone(&e.engine, name, (self.canvas.width, self.canvas.height))?;
            e.keep_alone(name);
            let px = e.engine.snapshot_scene_at(&scene, (w, h), (w, h)).map_err(|x| fail(x.to_string()));
            e.release_alone(name);
            return px.map(|px| (px, (w, h)));
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
                    transition_name: None,
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
            video_tracks: &[],
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

    /// Starts the virtual camera (Windows: Ferrocast Camera) showing what's on air, or `source`
    /// alone (a scene, or a source at its own size), fitted into the camera's size.
    #[cfg(target_os = "windows")]
    pub fn virtual_camera(&self, source: Option<&str>) -> Result<crate::VirtualCamera, StudioError> {
        let e = self.engine()?;
        let canvas = (self.canvas.width, self.canvas.height);
        let (scene, size, alone) = match source {
            Some(name) => {
                if e.engine.source_named(name).is_none()
                    && self.scene_index(name).is_err()
                    && e.engine.group_size(name).is_none()
                {
                    return Err(fail(format!("There's no source called \"{name}\".")));
                }
                let (scene, w, h) = own::show_alone(&e.engine, name, canvas)?;
                e.keep_alone(name);
                (Some(scene), (w, h), Some(crate::AloneGuard { shared: e.clone(), name: name.to_string() }))
            }
            None => (None, canvas, None),
        };
        let cam = crate::wincam::Camera::start(Arc::downgrade(&e), scene, size, self.canvas.fps).map_err(fail)?;
        Ok(crate::VirtualCamera { cam, _alone: alone })
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

    /// The named transition override applied when switching into `scene` (`Any -> scene`), if any.
    pub fn scene_transition_name_of(&self, scene: &str) -> Option<String> {
        let valid: std::collections::HashSet<String> = self.named_transitions().into_iter().map(|t| t.name).collect();
        self.transition_matrix_raw()
            .into_iter()
            .rev()
            .find(|r| r.from.is_none() && r.to.as_deref() == Some(scene) && valid.contains(&r.transition))
            .map(|r| r.transition)
    }

    /// A scene's own way in (OBS's Transition Override), or the Studio's (None).
    pub fn scene_transition_of(&self, scene: &str) -> Result<Option<TransitionInfo>, StudioError> {
        self.scene_index(scene)?;
        if let Some(name) = self.scene_transition_name_of(scene)
            && let Some(t) = self.named_transitions().into_iter().find(|t| t.name == name)
        {
            return Ok(Some(TransitionInfo { kind: t.kind, ms: t.ms }));
        }
        Ok(serde_json::from_value(self.extra("scene_transitions").get(scene).cloned().unwrap_or(Value::Null)).ok())
    }

    pub fn set_scene_transition(&self, scene: &str, t: Option<&TransitionInfo>) -> Result<(), StudioError> {
        self.scene_index(scene)?;
        self.edit_meta(|d| {
            if let Some(m) = d.pointer_mut("/ferrocast/transition_matrix").and_then(Value::as_array_mut) {
                m.retain(|r| {
                    !(r.get("from").is_none_or(Value::is_null) && r.get("to").and_then(Value::as_str) == Some(scene))
                });
            }
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            if !d["ferrocast"].get("scene_transitions").is_some_and(Value::is_object) {
                d["ferrocast"]["scene_transitions"] = json!({});
            }
            match t {
                Some(t) => d["ferrocast"]["scene_transitions"][scene] = json!(t),
                None => {
                    if let Some(m) = d["ferrocast"]["scene_transitions"].as_object_mut() {
                        m.remove(scene);
                    }
                }
            }
        });
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
        // Named transitions are the truth once the collection has them; the old per-kind map
        // is only the fallback for a collection that never had any.
        let all = self.named_transitions();
        if let Some(t) =
            all.iter().find(|t| t.default && t.kind == kind).or_else(|| all.iter().find(|t| t.kind == kind))
        {
            return t.settings.clone();
        }
        if let Some(v) = self.extra("transition_settings").get(kind)
            && v.is_object()
        {
            return v.clone();
        }
        json!({})
    }

    /// Changes only the keys sent (the rest of the kind's settings stay).
    pub fn set_transition_settings(&self, kind: &str, settings: &Value) -> Result<(), StudioError> {
        let merge = |old: &Value| -> Value {
            let mut out = if old.is_object() { old.clone() } else { json!({}) };
            if let (Some(dst), Some(src)) = (out.as_object_mut(), settings.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            out
        };
        let mut all = self.extra("transition_settings");
        if !all.is_object() {
            all = json!({});
        }
        all[kind] = merge(&all[kind]);
        self.edit_meta(|d| {
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"]["transition_settings"] = all;
            // The preset the reads pick (the default one of this kind, else the first), so what is
            // written is what `transition_settings` gives back; other presets stay as they are.
            let all = named_transitions_from_doc(d);
            let target = all
                .iter()
                .find(|t| t.default && t.kind == kind)
                .or_else(|| all.iter().find(|t| t.kind == kind))
                .map(|t| t.name.clone());
            if let Some(list) = d.get_mut("transitions").and_then(Value::as_array_mut)
                && let Some(entry) = target
                    .as_deref()
                    .and_then(|c| list.iter_mut().find(|t| name_of(t).trim() == c && kind_of(t) == kind))
            {
                entry["settings"] = merge(&entry["settings"]);
            }
        });
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
        self.fresh_ids(scene, &mut src);
        self.edit(|d| d["sources"].as_array_mut().map(|l| l.push(src.clone())));
        self.place(scene, &src)
    }

    /// A copied group's sources get ids no other item of `scene` has (they're edited through
    /// the scene, by id).
    fn fresh_ids(&self, scene: &str, src: &mut Value) {
        if kind_of(src) != "group" {
            return;
        }
        let mut next = self.top_id(scene);
        if let Some(items) = src.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
            for c in items {
                next += 1;
                c["id"] = json!(next);
            }
        }
        if let Some(set) = src.get_mut("settings").and_then(Value::as_object_mut) {
            set.insert("id_counter".into(), json!(next));
        }
        let _ = self.with_items(scene, |_, set| {
            set.insert("id_counter".into(), json!(next));
        });
    }

    /// Moves an item into a group of the same scene (by the group's item id), or out of any.
    pub fn set_group(&self, scene: &str, id: i64, group: Option<i64>) -> Result<(), StudioError> {
        let it = self.item(scene, id)?;
        let home = self.home(scene, id);
        if kind_of(&self.source(name_of(&it))?) == "group" {
            return Err(fail("A group can't go in a group."));
        }
        // Where it shows on the canvas now: its place, size and turn.
        let (scale, turn, at_canvas) = match (home != scene)
            .then(|| self.scene_items(scene).ok()?.into_iter().find(|i| name_of(i) == home))
            .flatten()
        {
            Some(gi) => {
                let p = ferrocore::obs::placement_of(&gi);
                let g = self.group_map(&gi);
                let (x, y) = (
                    it.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    it.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                );
                (self.group_scale(&gi), p.rotation, g.apply(x, y))
            }
            None => (
                (1.0, 1.0),
                0.0,
                (
                    it.pointer("/pos/x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    it.pointer("/pos/y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                ),
            ),
        };
        let mut c = it.clone();
        let (sx, sy) = (
            c.pointer("/scale/x").and_then(Value::as_f64).unwrap_or(1.0) as f32 * scale.0,
            c.pointer("/scale/y").and_then(Value::as_f64).unwrap_or(1.0) as f32 * scale.1,
        );
        let rot = c.get("rot").and_then(Value::as_f64).unwrap_or(0.0) as f32 + turn;
        // A bounding box (Fit, Stretch…) is in its parent's pixels too.
        let (bw, bh) = (
            c.pointer("/bounds/x").and_then(Value::as_f64).unwrap_or(0.0) as f32 * scale.0.abs(),
            c.pointer("/bounds/y").and_then(Value::as_f64).unwrap_or(0.0) as f32 * scale.1.abs(),
        );
        let old_group = (home != scene).then(|| {
            self.scene_items(scene)
                .ok()
                .and_then(|l| l.into_iter().find(|i| name_of(i) == home))
                .and_then(|g| g.get("id").and_then(Value::as_i64))
        });
        match group {
            Some(g) => {
                let gi = self.item(scene, g)?;
                let gname = name_of(&gi).to_string();
                if kind_of(&self.source(&gname)?) != "group" {
                    return Err(fail(format!("\"{gname}\" isn't a group.")));
                }
                if gname == home {
                    return Ok(());
                }
                // An empty group takes the canvas's own pixels, so what goes in stays put.
                if self.scene_items(&gname)?.is_empty() {
                    self.with_item(scene, g, |i| {
                        i["pos"] = json!({ "x": 0.0, "y": 0.0 });
                        i["scale"] = json!({ "x": 1.0, "y": 1.0 });
                        i["rot"] = json!(0.0);
                        i["align"] = json!(5);
                        i["bounds_type"] = json!(0);
                    })?;
                    let (cw, ch) = (self.canvas.width, self.canvas.height);
                    self.with_items(&gname, |_, set| {
                        set.insert("cx".into(), json!(cw));
                        set.insert("cy".into(), json!(ch));
                    })?;
                }
                let gi = self.item(scene, g)?;
                let p = ferrocore::obs::placement_of(&gi);
                let gmap = self.group_map(&gi);
                let back = gmap.inverse().ok_or_else(|| fail("That group is squashed flat: reset its size first."))?;
                let (lx, ly) = back.apply(at_canvas.0, at_canvas.1);
                // Each side on its own, so a group scaled more one way doesn't change its shape.
                let (gx, gy) = self.group_scale(&gi);
                c["pos"] = json!({ "x": lx, "y": ly });
                c["scale"] = json!({ "x": sx / gx, "y": sy / gy });
                c["bounds"] = json!({ "x": bw / gx.abs(), "y": bh / gy.abs() });
                c["rot"] = json!((rot - p.rotation).rem_euclid(360.0));
                let next = self.top_id(scene) + 1;
                c["id"] = json!(next);
                self.with_items(&home, |l, _| l.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id)))?;
                self.with_items(&gname, |l, _| l.push(c))?;
                self.with_items(scene, |_, set| {
                    set.insert("id_counter".into(), json!(next));
                })?;
                self.refit_group(scene, g)?;
            }
            None => {
                let Some(Some(gid)) = old_group else {
                    return Err(fail("It isn't in a group."));
                };
                c["pos"] = json!({ "x": at_canvas.0, "y": at_canvas.1 });
                c["scale"] = json!({ "x": sx, "y": sy });
                c["bounds"] = json!({ "x": bw, "y": bh });
                c["rot"] = json!(rot.rem_euclid(360.0));
                let next = self.top_id(scene) + 1;
                c["id"] = json!(next);
                let (at, _) = self.item_place(scene, gid)?;
                self.with_items(&home, |l, _| l.retain(|i| i.get("id").and_then(Value::as_i64) != Some(id)))?;
                self.with_items(scene, |l, set| {
                    l.push(c);
                    set.insert("id_counter".into(), json!(next));
                })?;
                // Just above its group.
                self.item_order(scene, next, at)?;
                self.refit_group(scene, gid)?;
            }
        }
        // The group it left fits what's left.
        if let Some(Some(gid)) = old_group
            && group.is_some()
        {
            self.refit_group(scene, gid)?;
        }
        Ok(())
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

    /// The sounds a scene makes: the global channels first, then its sources with sound, top first,
    /// with those inside its groups and the scenes shown inside it (OBS hears them all; so does
    /// the engine's mixer). `shown` says whether it is on air through every item above it.
    pub fn audio(&self, scene: &str) -> Result<Vec<AudioInfo>, StudioError> {
        // The devices heard in every scene first (Desktop Audio, Mic/Aux), then the scene's own
        // sources: the same order in "This scene" and "All", so a mic never jumps.
        let mut out: Vec<AudioInfo> = self.channels();
        let mut path = vec![scene.to_string()];
        self.collect_audio(scene, true, &mut out, &mut path)?;
        Ok(out)
    }

    /// `scene`'s sounds into `out`; `path` is the scenes entered so far (no scene is entered twice,
    /// and nesting stops a few deep, as the engine's own walk does).
    fn collect_audio(
        &self,
        scene: &str,
        shown: bool,
        out: &mut Vec<AudioInfo>,
        path: &mut Vec<String>,
    ) -> Result<(), StudioError> {
        for it in self.scene_items(scene)?.iter().rev() {
            let name = name_of(it);
            let on = shown && it.get("visible").and_then(Value::as_bool).unwrap_or(true);
            let Ok(src) = self.source(name) else { continue };
            if is_scene(&src) {
                // A group's sources, or a scene inside this one.
                if path.len() < 5 && !path.iter().any(|p| p == name) {
                    path.push(name.to_string());
                    let _ = self.collect_audio(name, on, out, path);
                    path.pop();
                }
            } else if self.info_of(it).audio {
                // Listed where it's first seen, on air when any placement of it is.
                if let Some(a) = out.iter_mut().find(|a| a.name == name) {
                    a.shown |= on;
                } else {
                    let mut info = self.audio_info(&src, None);
                    info.shown = on;
                    out.push(info);
                }
            }
        }
        Ok(())
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

    /// Which web page sources stopped drawing (their browser's watchdog gave up): (name, stalled).
    pub fn stalled_pages(&self) -> Vec<(String, bool)> {
        // Web pages are drawn only on a Mac and Windows: elsewhere there are none to stall.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.pages.borrow().iter().map(|(name, (_, page))| (name.clone(), page.stalled())).collect()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Vec::new()
        }
    }

    /// What the captures this collection's engine opened want said on the dashboard, since the
    /// last call (each engine keeps its own).
    pub fn take_notices(&self) -> Vec<ferrocore::notices::Notice> {
        self.engine.borrow().as_ref().map(|e| e.engine.take_notices()).unwrap_or_default()
    }

    /// Low latency sound (Settings); kept for every engine made.
    pub fn set_low_latency(&self, on: bool) {
        self.low_latency.set(on);
        if let Some(e) = self.engine.borrow().as_ref() {
            e.engine.set_low_latency(on);
        }
    }

    /// Meters read true peak (between samples) or sample peak; kept for every engine made.
    pub fn set_true_peak(&self, on: bool) {
        self.true_peak.set(on);
        if let Some(e) = self.engine.borrow().as_ref() {
            e.engine.set_true_peak(on);
        }
    }

    // libobs's housekeeping, which our engine doesn't need: kept so the app calls the same.

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

    /// The Studio's default transition: its kind and length.
    pub fn transition(&self) -> TransitionInfo {
        let all = self.named_transitions();
        if let Some(d) = all.iter().find(|t| t.default).or_else(|| all.first()) {
            return TransitionInfo { kind: d.kind.clone(), ms: d.ms };
        }
        TransitionInfo { kind: "fade_transition".into(), ms: 300 }
    }

    pub fn set_transition(&self, t: &TransitionInfo) -> Result<(), StudioError> {
        if !is_valid_transition_kind(&t.kind) {
            return Err(fail(format!("Unknown transition kind \"{}\".", t.kind)));
        }
        let ms = if t.kind == "cut_transition" { 0 } else { t.ms.min(20_000) };
        let cur_name =
            self.doc.borrow().get("current_transition").and_then(Value::as_str).unwrap_or("Fade").to_string();
        let all = self.named_transitions();
        let target_name = if all.iter().any(|x| x.name == cur_name && x.kind == t.kind) {
            cur_name
        } else {
            let canonical = match t.kind.as_str() {
                "cut_transition" => "Cut",
                "fade_transition" => "Fade",
                k => k,
            };
            all.iter()
                .find(|x| x.name == canonical && x.kind == t.kind)
                .or_else(|| all.iter().find(|x| x.kind == t.kind))
                .map_or_else(|| canonical.to_string(), |x| x.name.clone())
        };
        let target_name = if all.iter().any(|x| x.name == target_name && x.kind != t.kind) {
            let mut n = 2;
            loop {
                let c = format!("{target_name} {n}");
                if !all.iter().any(|x| x.name == c) {
                    break c;
                }
                n += 1;
            }
        } else {
            target_name
        };
        self.edit_meta(|d| {
            let mut list = named_transitions_from_doc(d);
            if let Some(existing) = list.iter_mut().find(|x| x.name == target_name) {
                existing.kind = t.kind.clone();
                existing.ms = ms;
            } else {
                list.push(NamedTransition {
                    name: target_name.clone(),
                    kind: t.kind.clone(),
                    ms,
                    settings: d
                        .pointer(&format!("/ferrocast/transition_settings/{}", t.kind))
                        .filter(|v| v.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                    default: true,
                });
            }
            for x in &mut list {
                x.default = x.name == target_name;
            }
            write_transitions_to_doc(d, &list, &target_name, ms);
        });
        Ok(())
    }

    /// All saved named transitions in the collection (always at least one, with one default).
    pub fn named_transitions(&self) -> Vec<NamedTransition> {
        named_transitions_from_doc(&self.doc.borrow())
    }

    /// The raw stored matrix rules without filtering (for internal cleanup).
    fn transition_matrix_raw(&self) -> Vec<TransitionMatrixRule> {
        transition_matrix_from_doc(&self.doc.borrow())
    }

    /// The valid from-scene / to-scene matrix rules for this collection.
    pub fn transition_matrix(&self) -> Vec<TransitionMatrixRule> {
        let valid_tr: std::collections::HashSet<String> =
            self.named_transitions().into_iter().map(|t| t.name).collect();
        let valid_scenes: std::collections::HashSet<String> = self.order().into_iter().collect();
        self.transition_matrix_raw()
            .into_iter()
            .filter(|r| {
                !(r.from.is_none() && r.to.is_none())
                    && valid_tr.contains(&r.transition)
                    && r.from.as_ref().is_none_or(|f| valid_scenes.contains(f))
                    && r.to.as_ref().is_none_or(|t| valid_scenes.contains(t))
            })
            .collect()
    }

    /// Resolves which transition plays when switching from scene `from` to scene `to`.
    /// Precedence: exact pair (`from` -> `to`) -> `Any` -> `to` (or legacy scene override) -> `from` -> `Any` -> default.
    pub fn resolve_transition(&self, from: &str, to: &str) -> ResolvedTransition {
        let all = self.named_transitions();
        let rules = self.transition_matrix();
        let by_name = |name: &str| {
            all.iter().find(|t| t.name == name).map(|t| ResolvedTransition {
                name: t.name.clone(),
                kind: t.kind.clone(),
                ms: if t.kind == "cut_transition" { 0 } else { t.ms },
                settings: t.settings.clone(),
            })
        };
        // 1. Exact pair: from -> to.
        if let Some(r) = rules.iter().rev().find(|r| r.from.as_deref() == Some(from) && r.to.as_deref() == Some(to))
            && let Some(t) = by_name(&r.transition)
        {
            return t;
        }
        // 2. Destination wildcard: Any -> to (then legacy ferrocast.scene_transitions override if present).
        if let Some(r) = rules.iter().rev().find(|r| r.from.is_none() && r.to.as_deref() == Some(to))
            && let Some(t) = by_name(&r.transition)
        {
            return t;
        }
        if let Ok(Some(legacy)) = serde_json::from_value::<Option<TransitionInfo>>(
            self.extra("scene_transitions").get(to).cloned().unwrap_or(Value::Null),
        ) {
            return ResolvedTransition {
                name: String::new(),
                ms: if legacy.kind == "cut_transition" { 0 } else { legacy.ms.min(20_000) },
                settings: self.transition_settings(&legacy.kind),
                kind: legacy.kind,
            };
        }
        // 3. Source wildcard: from -> Any.
        if let Some(r) = rules.iter().rev().find(|r| r.from.as_deref() == Some(from) && r.to.is_none())
            && let Some(t) = by_name(&r.transition)
        {
            return t;
        }
        // 4. Default transition.
        if let Some(d) = all.iter().find(|t| t.default).or_else(|| all.first()) {
            return ResolvedTransition {
                name: d.name.clone(),
                kind: d.kind.clone(),
                ms: if d.kind == "cut_transition" { 0 } else { d.ms },
                settings: d.settings.clone(),
            };
        }
        let d = self.transition();
        ResolvedTransition { name: String::new(), ms: d.ms, settings: self.transition_settings(&d.kind), kind: d.kind }
    }

    pub fn add_named_transition(&self, t: &NamedTransition) -> Result<NamedTransition, StudioError> {
        let name = t.name.trim();
        check_name(name)?;
        if !is_valid_transition_kind(&t.kind) {
            return Err(fail(format!("Unknown transition kind \"{}\".", t.kind)));
        }
        if t.ms > 20_000 {
            return Err(fail("ms: a transition lasts 0 to 20000 ms."));
        }
        if !t.settings.is_object() {
            return Err(fail("settings: an object of OBS transition settings."));
        }
        let mut list = self.named_transitions();
        if list.iter().any(|x| x.name == name) {
            return Err(fail(format!("There's already a transition called \"{name}\".")));
        }
        let ms = if t.kind == "cut_transition" { 0 } else { t.ms };
        let created = NamedTransition {
            name: name.to_string(),
            kind: t.kind.clone(),
            ms,
            settings: t.settings.clone(),
            default: t.default,
        };
        list.push(created.clone());
        let default_name = if t.default {
            name.to_string()
        } else {
            list.iter().find(|x| x.default).map_or_else(|| name.to_string(), |x| x.name.clone())
        };
        let default_ms = list.iter().find(|x| x.name == default_name).map_or(ms, |x| x.ms);
        for x in &mut list {
            x.default = x.name == default_name;
        }
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, &default_name, default_ms);
            mark_named_transitions_v2(d);
        });
        Ok(list.into_iter().find(|x| x.name == name).unwrap_or(created))
    }

    pub fn update_named_transition(
        &self,
        name: &str,
        kind: Option<&str>,
        ms: Option<u32>,
        settings: Option<&Value>,
        default: Option<bool>,
    ) -> Result<NamedTransition, StudioError> {
        if let Some(k) = kind
            && !is_valid_transition_kind(k)
        {
            return Err(fail(format!("Unknown transition kind \"{k}\".")));
        }
        if let Some(m) = ms
            && m > 20_000
        {
            return Err(fail("ms: a transition lasts 0 to 20000 ms."));
        }
        if let Some(s) = settings
            && !s.is_object()
        {
            return Err(fail("settings: an object of OBS transition settings."));
        }
        let mut list = self.named_transitions();
        let Some(pos) = list.iter().position(|x| x.name == name) else {
            return Err(fail(format!("There's no transition called \"{name}\".")));
        };
        let kind_changed = kind.is_some_and(|k| k != list[pos].kind);
        if let Some(k) = kind {
            list[pos].kind = k.to_string();
            if k == "cut_transition" {
                list[pos].ms = 0;
            }
        }
        if let Some(m) = ms {
            list[pos].ms = if list[pos].kind == "cut_transition" { 0 } else { m };
        }
        if let Some(s) = settings {
            if kind_changed || !list[pos].settings.is_object() {
                list[pos].settings = s.clone();
            } else if let (Some(dst), Some(src)) = (list[pos].settings.as_object_mut(), s.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
        } else if kind_changed {
            list[pos].settings = json!({});
        }
        let default_name = if default == Some(true) {
            name.to_string()
        } else {
            list.iter().find(|x| x.default).map_or_else(|| name.to_string(), |x| x.name.clone())
        };
        for x in &mut list {
            x.default = x.name == default_name;
        }
        let default_ms = list.iter().find(|x| x.name == default_name).map_or(300, |x| x.ms);
        let updated = list[pos].clone();
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, &default_name, default_ms);
            mark_named_transitions_v2(d);
        });
        Ok(updated)
    }

    pub fn rename_named_transition(&self, name: &str, new_name: &str) -> Result<NamedTransition, StudioError> {
        let new_name = new_name.trim();
        check_name(new_name)?;
        let mut list = self.named_transitions();
        let Some(pos) = list.iter().position(|x| x.name == name) else {
            return Err(fail(format!("There's no transition called \"{name}\".")));
        };
        if name == new_name {
            return Ok(list[pos].clone());
        }
        if list.iter().any(|x| x.name == new_name) {
            return Err(fail(format!("There's already a transition called \"{new_name}\".")));
        }
        list[pos].name = new_name.to_string();
        let updated = list[pos].clone();
        let default_name = list.iter().find(|x| x.default).map_or_else(|| new_name.to_string(), |x| x.name.clone());
        let default_ms = list.iter().find(|x| x.name == default_name).map_or(300, |x| x.ms);
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, &default_name, default_ms);
            if let Some(m) = d.pointer_mut("/ferrocast/transition_matrix").and_then(Value::as_array_mut) {
                for r in m.iter_mut() {
                    if r.get("transition").and_then(Value::as_str) == Some(name) {
                        r["transition"] = json!(new_name);
                    }
                }
            }
            mark_named_transitions_v2(d);
        });
        Ok(updated)
    }

    pub fn duplicate_named_transition(
        &self,
        name: &str,
        new_name: Option<&str>,
    ) -> Result<NamedTransition, StudioError> {
        let mut list = self.named_transitions();
        let Some(src) = list.iter().find(|x| x.name == name).cloned() else {
            return Err(fail(format!("There's no transition called \"{name}\".")));
        };
        let picked = match new_name {
            Some(n) => {
                let n = n.trim();
                check_name(n)?;
                if list.iter().any(|x| x.name == n) {
                    return Err(fail(format!("There's already a transition called \"{n}\".")));
                }
                n.to_string()
            }
            None => free_transition_name(&list, name)?,
        };
        let dup = NamedTransition { name: picked, default: false, ..src };
        list.push(dup.clone());
        let default_name = list.iter().find(|x| x.default).map_or_else(|| list[0].name.clone(), |x| x.name.clone());
        let default_ms = list.iter().find(|x| x.name == default_name).map_or(300, |x| x.ms);
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, &default_name, default_ms);
            mark_named_transitions_v2(d);
        });
        Ok(dup)
    }

    pub fn set_default_named_transition(&self, name: &str) -> Result<(), StudioError> {
        let mut list = self.named_transitions();
        if !list.iter().any(|x| x.name == name) {
            return Err(fail(format!("There's no transition called \"{name}\".")));
        }
        for x in &mut list {
            x.default = x.name == name;
        }
        let ms = list.iter().find(|x| x.name == name).map_or(300, |x| x.ms);
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, name, ms);
            mark_named_transitions_v2(d);
        });
        Ok(())
    }

    pub fn delete_named_transition(&self, name: &str) -> Result<(), StudioError> {
        let mut list = self.named_transitions();
        if !list.iter().any(|x| x.name == name) {
            return Err(fail(format!("There's no transition called \"{name}\".")));
        }
        if list.len() <= 1 {
            return Err(fail("At least one transition must remain."));
        }
        let was_default = list.iter().any(|x| x.name == name && x.default);
        list.retain(|x| x.name != name);
        let default_name = if was_default {
            list.iter().find(|x| x.name == "Fade").or_else(|| list.first()).map(|x| x.name.clone()).unwrap_or_default()
        } else {
            list.iter().find(|x| x.default).map_or_else(|| list[0].name.clone(), |x| x.name.clone())
        };
        for x in &mut list {
            x.default = x.name == default_name;
        }
        let default_ms = list.iter().find(|x| x.name == default_name).map_or(300, |x| x.ms);
        self.edit_meta(|d| {
            write_transitions_to_doc(d, &list, &default_name, default_ms);
            if let Some(m) = d.pointer_mut("/ferrocast/transition_matrix").and_then(Value::as_array_mut) {
                m.retain(|r| r.get("transition").and_then(Value::as_str) != Some(name));
            }
            mark_named_transitions_v2(d);
        });
        Ok(())
    }

    pub fn set_matrix_rule(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        transition: Option<&str>,
    ) -> Result<(), StudioError> {
        if from.is_none() && to.is_none() {
            return Err(fail(
                "Pick a From scene, a To scene, or both; the default transition already covers Any to Any.",
            ));
        }
        if let Some(f) = from {
            self.scene_index(f)?;
        }
        if let Some(t) = to {
            self.scene_index(t)?;
        }
        if transition.is_some() && from.is_some() && from == to {
            // Going from a scene to itself isn't a switch; the grid has no such cell either.
            return Err(fail("From and To are the same scene: pick two different scenes."));
        }
        if let Some(tr) = transition
            && !self.named_transitions().iter().any(|x| x.name == tr)
        {
            return Err(fail(format!("There's no transition called \"{tr}\".")));
        }
        let mut rules = self.transition_matrix();
        rules.retain(|r| !(r.from.as_deref() == from && r.to.as_deref() == to));
        if let Some(tr) = transition {
            rules.push(TransitionMatrixRule {
                from: from.map(String::from),
                to: to.map(String::from),
                transition: tr.to_string(),
            });
        }
        self.edit_meta(|d| {
            if !d.get("ferrocast").is_some_and(Value::is_object) {
                d["ferrocast"] = json!({});
            }
            d["ferrocast"]["transition_matrix"] = json!(rules);
            // Any -> to in the matrix supersedes any legacy scene_transitions entry.
            if from.is_none()
                && let Some(scene) = to
                && let Some(st) = d["ferrocast"].get_mut("scene_transitions").and_then(Value::as_object_mut)
            {
                st.remove(scene);
            }
            mark_named_transitions_v2(d);
        });
        Ok(())
    }
}

pub fn is_valid_transition_kind(kind: &str) -> bool {
    matches!(
        kind,
        "fade_transition"
            | "cut_transition"
            | "swipe_transition"
            | "slide_transition"
            | "fade_to_color_transition"
            | "wipe_transition"
            | "obs_stinger_transition"
            | "ferrocast_move"
    )
}

/// OBS's own transitions, by the names it shows them under (it doesn't save these in the
/// collection's `transitions`, only custom ones).
fn builtin_kind(name: &str) -> Option<&'static str> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "cut" => "cut_transition",
        "fade" => "fade_transition",
        "swipe" => "swipe_transition",
        "slide" => "slide_transition",
        "stinger" => "obs_stinger_transition",
        "fade to color" => "fade_to_color_transition",
        "luma wipe" => "wipe_transition",
        _ => return None,
    })
}

fn guess_transition_kind(name: &str) -> &'static str {
    if let Some(k) = builtin_kind(name) {
        k
    } else if name.eq_ignore_ascii_case("cut") || name == "cut_transition" {
        "cut_transition"
    } else if is_valid_transition_kind(name) {
        match name {
            "cut_transition" => "cut_transition",
            "swipe_transition" => "swipe_transition",
            "slide_transition" => "slide_transition",
            "fade_to_color_transition" => "fade_to_color_transition",
            "wipe_transition" => "wipe_transition",
            "obs_stinger_transition" => "obs_stinger_transition",
            "ferrocast_move" => "ferrocast_move",
            _ => "fade_transition",
        }
    } else {
        "fade_transition"
    }
}

fn free_transition_name(existing: &[NamedTransition], base: &str) -> Result<String, StudioError> {
    let mut n = 2usize;
    loop {
        let suffix = format!(" {n}");
        let max_stem = 100usize.saturating_sub(suffix.chars().count());
        let stem: String = base.chars().take(max_stem).collect();
        let candidate = format!("{stem}{suffix}");
        if !existing.iter().any(|t| t.name == candidate) {
            return Ok(candidate);
        }
        n += 1;
        if n > 10_000 {
            return Err(fail("Too many transitions with similar names."));
        }
    }
}

fn mark_named_transitions_v2(d: &mut Value) {
    if !d.get("ferrocast").is_some_and(Value::is_object) {
        d["ferrocast"] = json!({});
    }
    d["ferrocast"]["named_transitions_v2"] = json!(true);
}

fn write_transitions_to_doc(d: &mut Value, list: &[NamedTransition], default_name: &str, default_ms: u32) {
    let arr: Vec<Value> = list
        .iter()
        .map(|t| {
            json!({
                "id": t.kind,
                "name": t.name,
                "duration": if t.kind == "cut_transition" { 0 } else { t.ms.min(20_000) },
                "settings": if t.settings.is_object() { t.settings.clone() } else { json!({}) },
            })
        })
        .collect();
    d["transitions"] = Value::Array(arr);
    d["current_transition"] = json!(default_name);
    d["transition_duration"] = json!(default_ms.min(20_000));
}

fn named_transitions_from_doc(d: &Value) -> Vec<NamedTransition> {
    let current = d.get("current_transition").and_then(Value::as_str).unwrap_or("Fade").trim();
    let current = if current.is_empty() { "Fade" } else { current };
    let current_ms = d.get("transition_duration").and_then(Value::as_u64).unwrap_or(300).min(20_000) as u32;
    let is_v2 = d.pointer("/ferrocast/named_transitions_v2").and_then(Value::as_bool).unwrap_or(false);
    let legacy_settings =
        if !is_v2 { d.pointer("/ferrocast/transition_settings").and_then(Value::as_object) } else { None };
    let mut out: Vec<NamedTransition> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let raw = d.get("transitions").and_then(Value::as_array);
    let had_entries = raw.is_some_and(|a| !a.is_empty());
    if let Some(arr) = raw {
        for entry in arr {
            let Some(raw_name) = entry.get("name").and_then(Value::as_str) else { continue };
            let name = raw_name.trim();
            if name.is_empty() || !seen.insert(name.to_string()) {
                continue;
            }
            let raw_kind = match kind_of(entry) {
                // OBS's Move Transition (Exeldro): the one Ferrocast draws itself.
                "move_transition" => "ferrocast_move",
                k => k,
            };
            let kind = if is_valid_transition_kind(raw_kind) {
                raw_kind.to_string()
            } else {
                guess_transition_kind(name).to_string()
            };
            let ms = if kind == "cut_transition" {
                0
            } else if let Some(dur) = entry.get("duration").and_then(Value::as_u64) {
                dur.min(20_000) as u32
            } else if name == current {
                current_ms
            } else {
                300
            };
            let mut settings = entry.get("settings").filter(|s| s.is_object()).cloned().unwrap_or_else(|| json!({}));
            if settings.as_object().is_some_and(|m| m.is_empty())
                && let Some(leg) = legacy_settings.and_then(|m| m.get(&kind)).filter(|v| v.is_object())
            {
                settings = leg.clone();
            }
            out.push(NamedTransition { name: name.to_string(), kind, ms, settings, default: false });
        }
    }
    if !seen.contains(current) {
        let kind = guess_transition_kind(current).to_string();
        let ms = if kind == "cut_transition" { 0 } else { current_ms };
        let settings =
            legacy_settings.and_then(|m| m.get(&kind)).filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        out.insert(0, NamedTransition { name: current.to_string(), kind, ms, settings, default: false });
        seen.insert(current.to_string());
    }
    if !had_entries {
        if !seen.contains("Fade") {
            out.push(NamedTransition {
                name: "Fade".into(),
                kind: "fade_transition".into(),
                ms: 300,
                settings: legacy_settings
                    .and_then(|m| m.get("fade_transition"))
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                default: false,
            });
            seen.insert("Fade".into());
        }
        if !seen.contains("Cut") {
            out.push(NamedTransition {
                name: "Cut".into(),
                kind: "cut_transition".into(),
                ms: 0,
                settings: legacy_settings
                    .and_then(|m| m.get("cut_transition"))
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                default: false,
            });
        }
    }
    let def_idx = out.iter().position(|t| t.name == current).unwrap_or(0);
    if let Some(d) = out.get_mut(def_idx) {
        d.default = true;
    }
    out
}

fn transition_matrix_from_doc(d: &Value) -> Vec<TransitionMatrixRule> {
    let Some(raw) = d.pointer("/ferrocast/transition_matrix").and_then(Value::as_array) else {
        return Vec::new();
    };
    raw.iter()
        .filter_map(|v| serde_json::from_value::<TransitionMatrixRule>(v.clone()).ok())
        .map(|mut r| {
            r.from = r.from.filter(|s| !s.trim().is_empty());
            r.to = r.to.filter(|s| !s.trim().is_empty());
            r
        })
        .filter(|r| !(r.from.is_none() && r.to.is_none()) && !r.transition.trim().is_empty())
        .collect()
}

/// Normalizes transition data when loading a saved, OBS, or Streamlabs collection:
/// preserves old collections unchanged while upgrading OBS `transitions` & scene `private_settings.transition`.
fn import_and_normalize_transitions(d: &mut Value) {
    let is_v2 = d.pointer("/ferrocast/named_transitions_v2").and_then(Value::as_bool).unwrap_or(false);
    let mut transitions = named_transitions_from_doc(d);
    let mut rules = transition_matrix_from_doc(d);
    let mut imported_scene_overrides = false;

    if !is_v2 {
        // OBS stores per-scene Transition Override in `sources[i].private_settings.transition`.
        let vert_scenes: std::collections::HashSet<String> = d
            .pointer("/ferrocast/vertical/scene_order")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
            .collect();
        let obs_overrides: Vec<(String, String, Option<u32>)> = d
            .get("sources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| s.get("id").and_then(Value::as_str) == Some("scene"))
            .filter_map(|s| {
                let sname = s.get("name").and_then(Value::as_str)?.to_string();
                if vert_scenes.contains(&sname) {
                    return None;
                }
                let ps = s.get("private_settings")?;
                let tr = ps.get("transition").and_then(Value::as_str)?.trim();
                if tr.is_empty() {
                    return None;
                }
                let dur = ps.get("transition_duration").and_then(Value::as_u64).map(|x| x.min(20_000) as u32);
                Some((sname, tr.to_string(), dur))
            })
            .collect();

        for (scene_name, tr_name, dur_opt) in obs_overrides {
            let has_matrix = rules.iter().any(|r| r.from.is_none() && r.to.as_deref() == Some(&scene_name));
            let has_legacy = d
                .pointer("/ferrocast/scene_transitions")
                .and_then(|v| v.get(scene_name.as_str()))
                .is_some_and(|v| !v.is_null());
            if has_matrix || has_legacy {
                continue;
            }
            // OBS plays the default when it can't find a custom transition by that name (it was
            // removed): no preset is made up for it. Its own built-ins aren't saved, so those are
            // known by name.
            if !transitions.iter().any(|t| t.name == tr_name) && builtin_kind(&tr_name).is_none() {
                continue;
            }
            if !transitions.iter().any(|t| t.name == tr_name) {
                let kind = guess_transition_kind(&tr_name).to_string();
                let ms = if kind == "cut_transition" { 0 } else { dur_opt.unwrap_or(300) };
                transitions.push(NamedTransition {
                    name: tr_name.clone(),
                    kind,
                    ms,
                    settings: json!({}),
                    default: false,
                });
            }
            // OBS gives an override with no saved duration 300 ms, not the base preset's.
            let dur = dur_opt.unwrap_or(300);
            let target_tr = if let Some(base) = transitions.iter().find(|t| t.name == tr_name).cloned()
                && base.kind != "cut_transition"
                && base.ms != dur
            {
                // Kept inside the 100-character limit every later edit checks.
                let suffix = format!(" ({dur}ms)");
                let stem: String = base.name.chars().take(100usize.saturating_sub(suffix.chars().count())).collect();
                let custom = format!("{stem}{suffix}");
                // A preset of that name is reused only if it is the same thing; otherwise this
                // one gets a name of its own.
                let same = |t: &NamedTransition| t.kind == base.kind && t.ms == dur && t.settings == base.settings;
                match transitions.iter().find(|t| t.name == custom) {
                    Some(t) if same(t) => custom,
                    Some(_) => {
                        let own = free_transition_name(&transitions, &custom).unwrap_or_else(|_| custom.clone());
                        transitions.push(NamedTransition {
                            name: own.clone(),
                            kind: base.kind,
                            ms: dur,
                            settings: base.settings,
                            default: false,
                        });
                        own
                    }
                    None => {
                        transitions.push(NamedTransition {
                            name: custom.clone(),
                            kind: base.kind,
                            ms: dur,
                            settings: base.settings,
                            default: false,
                        });
                        custom
                    }
                }
            } else {
                tr_name
            };
            rules.push(TransitionMatrixRule { from: None, to: Some(scene_name), transition: target_tr });
            imported_scene_overrides = true;
        }
    }

    // Deduplicate rules (last wins) and keep only those referencing existing main scenes & transitions.
    let valid_tr: std::collections::HashSet<String> = transitions.iter().map(|t| t.name.clone()).collect();
    let vert_scenes: std::collections::HashSet<String> = d
        .pointer("/ferrocast/vertical/scene_order")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let valid_scenes: std::collections::HashSet<String> = d
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("id").and_then(Value::as_str) == Some("scene"))
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
        .filter(|n| !vert_scenes.contains(n))
        .collect();
    let mut deduped: Vec<TransitionMatrixRule> = Vec::new();
    for r in rules {
        if valid_tr.contains(&r.transition)
            && !(r.from.is_some() && r.from == r.to)
            && r.from.as_ref().is_none_or(|f| valid_scenes.contains(f))
            && r.to.as_ref().is_none_or(|t| valid_scenes.contains(t))
        {
            deduped.retain(|x| !(x.from == r.from && x.to == r.to));
            deduped.push(r);
        }
    }

    let default_name = transitions
        .iter()
        .find(|t| t.default)
        .or_else(|| transitions.first())
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "Fade".into());
    let default_ms = transitions.iter().find(|t| t.name == default_name).map_or(300, |t| t.ms);

    if d.get("transitions").is_some() || imported_scene_overrides || !deduped.is_empty() || is_v2 {
        write_transitions_to_doc(d, &transitions, &default_name, default_ms);
    }
    if !deduped.is_empty() || d.pointer("/ferrocast/transition_matrix").is_some() {
        if !d.get("ferrocast").is_some_and(Value::is_object) {
            d["ferrocast"] = json!({});
        }
        d["ferrocast"]["transition_matrix"] = json!(deduped);
    }
    if imported_scene_overrides {
        mark_named_transitions_v2(d);
    }
}

/// Reads named transitions and matrix rules directly from a saved collection document (when the Studio is stopped).
pub fn saved_named_transitions(doc: Option<&Value>) -> Vec<NamedTransition> {
    doc.map(named_transitions_from_doc).unwrap_or_else(|| {
        vec![
            NamedTransition {
                name: "Fade".into(),
                kind: "fade_transition".into(),
                ms: 300,
                settings: json!({}),
                default: true,
            },
            NamedTransition {
                name: "Cut".into(),
                kind: "cut_transition".into(),
                ms: 0,
                settings: json!({}),
                default: false,
            },
        ]
    })
}

/// A collection's main-picture scenes in order: the ones in `scene_order`, then any scene that
/// has a source entry but isn't listed there; never the vertical canvas's. A running Studio and a
/// saved collection read scenes the same way through this.
fn main_scenes(d: &Value) -> Vec<String> {
    let vertical: Vec<String> = d
        .pointer("/ferrocast/vertical/scene_order")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| o.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let scenes: Vec<String> = d
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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

/// The matrix rules of a saved collection, without the ones naming a scene or transition that
/// is no longer there (the same filter a running Studio applies).
pub fn saved_transition_matrix(doc: Option<&Value>) -> Vec<TransitionMatrixRule> {
    let Some(d) = doc else { return Vec::new() };
    let valid_tr: std::collections::HashSet<String> =
        named_transitions_from_doc(d).into_iter().map(|t| t.name).collect();
    let valid_scenes: std::collections::HashSet<String> = main_scenes(d).into_iter().collect();
    transition_matrix_from_doc(d)
        .into_iter()
        .filter(|r| {
            !(r.from.is_none() && r.to.is_none())
                && valid_tr.contains(&r.transition)
                && r.from.as_ref().is_none_or(|f| valid_scenes.contains(f))
                && r.to.as_ref().is_none_or(|t| valid_scenes.contains(t))
        })
        .collect()
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

    /// A stream and a recording share this output's encode (Same as stream): they carry one
    /// picture, so something shown on only one of them can't be.
    pub fn shares_encoder(&self) -> bool {
        self.own.shares_encoder()
    }

    pub fn record_stats(&self) -> RecordStats {
        self.own.record_stats()
    }

    /// Why this recording stopped by itself (a full disk, a drive taken away): a short phrase,
    /// and what to do with the system's own words. None while it runs, or for a stream.
    pub fn record_problem(&self) -> Option<(String, String)> {
        self.own.record_problem().map(|p| (p.reason, p.detail))
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

    /// A new encoder, output size and frame rate while live, without stopping (Optimize).
    pub fn reshape(&self, e: &crate::EncodeSettings, fps: u32) -> Result<(), StudioError> {
        self.own.reshape(e, fps)
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

/// How much of a preview's view shows: the page's opacity, but a paused (performance mode) view
/// is clear, whatever the page asks, so its "Preview paused" card shows there.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn view_opacity(paused: bool, asked: f64) -> f64 {
    if paused { 0.0 } else { asked }
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
    /// Performance mode: nothing is drawn and the view is clear (the page shows its
    /// "Preview paused" card there). Outputs are never touched.
    paused: Cell<bool>,
    /// The opacity the page asked for (a paused view is clear whatever it is).
    opacity: Cell<f64>,
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
    /// The second colour (0xAABBGGRR) of the stripes over what runs past the canvas; the first is
    /// `background`. None draws no stripes.
    stripe: Option<u32>,
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
        let v = StageView {
            shared,
            id,
            view,
            look: RefCell::new(look),
            vertical,
            paused: Cell::new(false),
            opacity: Cell::new(1.0),
        };
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

    /// The second colour of the stripes drawn over the part of a source that runs past the
    /// canvas (0xAABBGGRR; the first is `background`), or none for a plain surround.
    pub fn overflow_stripes(&self, abgr: Option<u32>) {
        if std::mem::replace(&mut self.look.borrow_mut().stripe, abgr) != abgr {
            self.push();
        }
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
        self.opacity.set(opacity);
        self.view.set_opacity(self.shown_opacity());
    }

    fn shown_opacity(&self) -> f64 {
        view_opacity(self.paused.get(), self.opacity.get())
    }

    /// Performance mode: stops (or resumes) drawing this view; while paused it is clear so the
    /// page's own placeholder shows. Only this view: no output is touched. Main thread.
    pub fn set_paused(&self, paused: bool) {
        if self.paused.replace(paused) == paused {
            return;
        }
        self.shared.engine.set_preview_paused(self.id, paused);
        if !paused {
            self.push();
        }
        self.view.set_opacity(self.shown_opacity());
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Mouse actions over it go to `cb` (Windows: the picture's own window takes them; on a
    /// Mac the page gets them itself).
    pub fn set_input(&self, cb: Box<dyn Fn(crate::PreviewInput)>) {
        #[cfg(target_os = "windows")]
        self.view.set_input(cb);
        #[cfg(not(target_os = "windows"))]
        let _ = cb;
    }

    /// The cursor over it (Windows and Mac), by its CSS name.
    pub fn set_cursor(&self, css: &str) {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        self.view.set_cursor(css);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let _ = css;
    }

    /// Keeps it with the stage (the app's tick): the vertical picture's scene on air, and the
    /// outlines taken from the items as they are now.
    pub fn follow(&self, stage: &Stage) {
        // Paused: nothing is drawn, so nothing is sent to the engine either (it is sent once on resume).
        if self.paused.get() {
            return;
        }
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
                overflow: l.stripe.map(|abgr| {
                    let c = |shift: u32| ((abgr >> shift) & 0xff) as f32 / 255.0;
                    ferrocore::preview::Overflow {
                        a: l.background,
                        b: [c(0), c(8), c(16), 1.0],
                        // 18 points across, as the page's own stripes would be.
                        period: 18.0 * l.ppp,
                    }
                }),
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
    /// Source name pinned via `keep_alone`, released on `Drop`.
    alone: Option<String>,
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
        let (title, scene, size, alone) = match of {
            P::Program | P::Multiview => {
                let title = if *of == P::Multiview { "Multiview" } else { "Projector: what's on air" };
                (title.to_string(), None, canvas, None)
            }
            P::Source(name) => {
                let canvas = self.canvas_of(name);
                let (scene, w, h) = own::show_alone(&shared.engine, name, canvas)?;
                shared.keep_alone(name);
                (format!("Projector: {name}"), Some(scene), (w, h), Some(name.clone()))
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
        let hooks =
            crate::ffi::projector_window::Hooks { click, close: on_close, resize, hide_cursor: Default::default() };
        let Some(win) = crate::ffi::projector_window::open(&title, aspect, screen, hooks) else {
            if let Some(name) = &alone {
                shared.release_alone(name);
            }
            return Err(fail("The projector's window couldn't be made."));
        };
        let (pw, ph) = win.pixels();
        // SAFETY: the window's view lives as long as the StageProjector, which stops the engine
        // drawing in it before the window goes (`Drop`).
        #[allow(unsafe_code)]
        let id = match unsafe { shared.engine.start_preview_of(win.view_ptr(), pw, ph, size, scene) } {
            Ok(id) => id,
            Err(e) => {
                if let Some(name) = &alone {
                    shared.release_alone(name);
                }
                return Err(fail(format!("the projector couldn't be drawn: {e}")));
            }
        };
        id_cell.set(Some(id));
        shared.engine.set_preview_look(
            id,
            ferrocore::preview::PreviewLook { background: [0.0, 0.0, 0.0, 1.0], ..Default::default() },
        );
        if let Some(mv) = &multiview {
            mv.borrow_mut().view = (pw, ph);
            mv.borrow().apply(&shared, id);
        }
        Ok(StageProjector { shared, id, win, multiview, alone })
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

    /// Hides the pointer while it is over the window (OBS's "Hide cursor over projectors").
    pub fn set_hide_cursor(&self, on: bool) {
        self.win.set_hide_cursor(on);
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
        if let Some(name) = &self.alone {
            self.shared.release_alone(name);
        }
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
    pub fn overflow_stripes(&self, _abgr: Option<u32>) {}
    pub fn set_shade(&self, _shade: f64) {}
    pub fn set_holes(&self, _holes: &[(f64, f64, f64, f64, f64)]) {}
    pub fn set_opacity(&self, _opacity: f64) {}
    pub fn set_paused(&self, _paused: bool) {}
    pub fn is_paused(&self) -> bool {
        false
    }
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

/// Whether a recording can share the stream's encoder (one encode for both, where the Mac's
/// one video encoder can't do two at 4K60): Ok, or why not, in words for the person (the
/// encoder-budget warning names the same reason; projects-60's review). `same_as_stream`: the
/// recording's quality is "Same as stream"; `canvas_fps` the picture's rate now, `fps` the
/// rate chosen; `selective`: some item that shows is for the stream only or the recording only
/// (`Stage::has_selective_items`), so the two pictures differ.
pub fn can_share(
    s: &StreamSettings,
    plan: &RecordPlan,
    same_as_stream: bool,
    canvas_fps: u32,
    fps: u32,
    selective: bool,
) -> Result<(), String> {
    if !same_as_stream {
        return Err("the recording's quality isn't Same as stream".into());
    }
    if selective {
        return Err("some sources are shown only on the stream or only in the recording".into());
    }
    if let Some((w, h)) = s.encode.scale {
        return Err(format!("the stream goes out at {w}×{h}, the recording at the full picture"));
    }
    // The bitrate isn't compared: the shared encode takes the stream's, live (as auto quality
    // and a bitrate changed while live do), so the recording follows the stream's.
    let mut stream_encode = s.encode.clone();
    stream_encode.scale = None;
    stream_encode.video_kbps = plan.encode.video_kbps;
    // The rate control is compared too: a stream held strictly under a platform's limit
    // (`strict_cbr`) that joined a recording's plain CBR encoder would lose its 1x VBV and burst
    // past the limit with nothing saying so.
    if stream_encode != plan.encode {
        return Err("the recording is encoded another way than the stream".into());
    }
    if plan.tracks != [1] {
        return Err(format!(
            "the recording keeps {} sound tracks: only track 1 can share the stream's",
            plan.tracks.len()
        ));
    }
    if s.vod_track.is_some() {
        return Err("the stream sends a VOD track for Twitch".into());
    }
    if !s.video_tracks.is_empty() {
        return Err("Twitch Enhanced Broadcasting sends more qualities".into());
    }
    if canvas_fps != fps {
        return Err(format!("the picture runs at {canvas_fps} fps and the stream at {fps}"));
    }
    Ok(())
}

/// Logs `what` the first time this run (a profile from before Ferrocore keeps its muxer
/// settings, with no box left to clear them: said once, not at every recording). Bounded so
/// varied warnings over weeks never grow unboundedly.
fn say_once(what: String) {
    const MAX_SAID: usize = 256;
    static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
        std::sync::Mutex::new(std::collections::VecDeque::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&what) {
        tracing::warn!("{what}");
        if said.len() >= MAX_SAID {
            said.pop_front();
        }
        said.push_back(what);
    }
}

/// Notes what a screen capture's window or display is, beside its number, so it's found
/// again when the number changes: the window's app and title (OBS's "owner_name" and
/// "window_name"; a game or browser gets a new window number each time it opens), on a Mac
/// the display's UUID ("display_uuid"), and on Windows its PnP device path ("monitor_id").
///
/// `picked`: a window was picked now. Otherwise (only the method changed) a window's names stay
/// as they are, even when its saved number isn't open any more: they're how it's found again.
fn remember_capture(kind: &str, set: &mut Value, picked: bool) {
    if !matches!(kind, "screen_capture" | "display_capture" | "monitor_capture" | "window_capture" | "game_capture") {
        return;
    }
    let ty = set.get("type").and_then(Value::as_u64).unwrap_or(0);
    let window = set.get("window").and_then(Value::as_u64).map(|w| w as u32);
    let is_window = !matches!(kind, "screen_capture" | "display_capture" | "monitor_capture") || ty == 1;
    let Some(id) = window.filter(|_| is_window) else {
        remember_display(kind, set);
        return;
    };
    if !picked {
        return;
    }
    // Names of a window picked before never find this one's app.
    if let Some(o) = set.as_object_mut() {
        o.remove("owner_name");
        o.remove("window_name");
    }
    if let Ok((_, windows)) = ferrocore::screen::shareable()
        && let Some(w) = windows.iter().find(|w| w.id == id)
    {
        set["owner_name"] = json!(w.app);
        set["window_name"] = json!(w.title);
    }
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn remember_display_id(set: &mut Value, index_key: &str, id_key: &str, lookup: impl FnOnce(u32) -> Option<String>) {
    if let Some(d) = set.get(index_key).and_then(Value::as_u64) {
        match lookup(d as u32) {
            Some(u) => set[id_key] = json!(u),
            None => {
                if let Some(o) = set.as_object_mut() {
                    o.remove(id_key);
                }
            }
        }
    }
}

/// Stamps or clears the OS display identifier (`display_uuid` on macOS, `monitor_id` on Windows)
/// alongside the numeric display index.
fn remember_display(kind: &str, set: &mut Value) {
    match kind {
        "monitor_capture" => {
            #[cfg(target_os = "windows")]
            remember_display_id(set, "monitor", "monitor_id", ferrocore::screen::display_uuid);
            #[cfg(not(target_os = "windows"))]
            let _ = set;
        }
        _ => {
            #[cfg(target_os = "macos")]
            remember_display_id(set, "display", "display_uuid", ferrocore::screen::display_uuid);
            #[cfg(not(target_os = "macos"))]
            let _ = set;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One encode for a stream and a recording "Same as stream" only when nothing differs, and
    /// why not, in words, when something does (the same reasons the budget warning gives).
    #[test]
    fn a_recording_shares_the_streams_encoder_only_when_nothing_differs() {
        let enc = crate::EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc");
        let s = StreamSettings {
            encode: enc.clone(),
            reconnect: None,
            delay_s: 0,
            vod_track: None,
            own_encoder: false,
            video_tracks: Vec::new(),
        };
        let plan = RecordPlan { tracks: vec![1], encode: enc.clone(), ..RecordPlan::default() };
        assert_eq!(can_share(&s, &plan, true, 60, 60, false), Ok(()));
        // Another bitrate still shares: the shared encode takes the stream's, live.
        let faster = StreamSettings { encode: crate::EncodeSettings { video_kbps: 9000, ..enc.clone() }, ..s.clone() };
        assert_eq!(can_share(&faster, &plan, true, 60, 60, false), Ok(()));
        let why =
            |s: &StreamSettings, p: &RecordPlan, same: bool, c: u32| can_share(s, p, same, c, 60, false).unwrap_err();
        assert!(why(&s, &plan, false, 60).contains("Same as stream"));
        let scaled =
            StreamSettings { encode: crate::EncodeSettings { scale: Some((1920, 1080)), ..enc.clone() }, ..s.clone() };
        assert!(why(&scaled, &plan, true, 60).contains("1920×1080"));
        let two = RecordPlan { tracks: vec![1, 2], ..plan.clone() };
        assert!(why(&s, &two, true, 60).contains("2 sound tracks"));
        assert!(why(&StreamSettings { vod_track: Some(2), ..s.clone() }, &plan, true, 60).contains("VOD"));
        assert!(
            why(&StreamSettings { video_tracks: vec![(1280, 720, 3000)], ..s.clone() }, &plan, true, 60)
                .contains("Enhanced")
        );
        assert!(why(&s, &plan, true, 30).contains("30 fps"));
        let high = RecordPlan { encode: crate::EncodeSettings { quality: Some(16), ..enc.clone() }, ..plan.clone() };
        assert!(why(&s, &high, true, 60).contains("another way"));
        // A stream held strictly under a platform's limit doesn't join a plain CBR encoder (it
        // would lose its 1x VBV and burst past the limit, unseen), and the other way round.
        let strict = crate::EncodeSettings { strict_cbr: true, ..enc };
        let strict_stream = StreamSettings { encode: strict.clone(), ..s.clone() };
        assert!(why(&strict_stream, &plan, true, 60).contains("another way"));
        let strict_plan = RecordPlan { encode: strict, ..plan };
        assert_eq!(can_share(&strict_stream, &strict_plan, true, 60, 60, false), Ok(()));
        assert!(why(&s, &strict_plan, true, 60).contains("another way"));
    }

    /// A picked display is noted by its UUID (found again when its number changes); a window's
    /// old names go when another window is picked.
    #[test]
    fn a_picked_display_or_window_is_noted_by_more_than_its_number() {
        #[cfg(target_os = "macos")]
        if let Some(&d) = ferrocore::screen::shareable().ok().and_then(|(ds, _)| ds.first().map(|x| x.id)).as_ref() {
            let mut set = json!({ "type": 0, "display": d });
            remember_capture("screen_capture", &mut set, true);
            assert_eq!(set["display_uuid"].as_str(), ferrocore::screen::display_uuid(d).as_deref());
        }
        // Windows `monitor_capture` stamps `monitor_id` beside `monitor` and clears it when unresolvable.
        let mut win_set = json!({ "monitor": 1, "capture_cursor": true });
        remember_display_id(&mut win_set, "monitor", "monitor_id", |d| {
            assert_eq!(d, 1);
            Some(r"\\?\DISPLAY#DELA0D4#5&1234&0&UID4353#{e6f07b5f}".into())
        });
        assert_eq!(win_set["monitor_id"], r"\\?\DISPLAY#DELA0D4#5&1234&0&UID4353#{e6f07b5f}");
        remember_display_id(&mut win_set, "monitor", "monitor_id", |_| None);
        assert!(win_set.get("monitor_id").is_none(), "{win_set}");
        let mut set = json!({ "type": 1, "window": 0xFFFF_FFF0u32, "owner_name": "Old", "window_name": "Old" });
        remember_capture("screen_capture", &mut set, true);
        assert!(set.get("owner_name").is_none() && set.get("window_name").is_none(), "{set}");
        // Only the method changed: a window's names stay, though its number isn't open now.
        let mut set = json!({ "type": 1, "window": 0xFFFF_FFF0u32, "owner_name": "Game", "window_name": "Game" });
        remember_capture("screen_capture", &mut set, false);
        assert_eq!(set["owner_name"], "Game");
        // Not a screen capture: left as it is.
        let mut other = json!({ "window": 3, "owner_name": "Keep" });
        remember_capture("image_source", &mut other, true);
        assert_eq!(other["owner_name"], "Keep");
    }

    #[test]
    fn a_paused_preview_is_clear_whatever_the_page_asks() {
        assert_eq!(view_opacity(false, 0.6), 0.6);
        assert_eq!(view_opacity(true, 1.0), 0.0);
        assert_eq!(view_opacity(true, 0.0), 0.0);
        // Let go again: what the page last asked for.
        assert_eq!(view_opacity(false, 1.0), 1.0);
    }

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

    /// A new shadow spills past the box: the item keeps its size (and its frame, the outline
    /// and handles the preview draws). An older one, not outside, grows it as before.
    #[test]
    fn a_new_shadow_leaves_the_items_box_alone() {
        let s = stage();
        ok(s.add_source("Main", "color_source_v3", "C", &json!({ "width": 200, "height": 100 }), false));
        assert_eq!(s.size_of("C"), (200, 100));
        s.add_filter("C", "ferrocast_shadow", "Shadow", &json!({ "blur": 30, "y": 0 }))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.size_of("C"), (200, 100), "outside: the box is the picture's");
        s.update_filter("C", "Shadow", &json!({ "outside": false })).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.size_of("C"), (260, 160), "inside: grown by the shadow on every side");
    }

    #[test]
    fn the_privacy_guard_is_kept_in_the_document_and_checked() {
        let s = stage();
        let mac = cfg!(target_os = "macos");
        assert_eq!(s.privacy(), json!({ "hide_notifications": true, "hide_apps": [], "supported": mac }));
        s.set_privacy(&json!({ "hide_notifications": false, "hide_apps": ["Mail", " com.hnc.Discord ", "mail", "\u{200E}WhatsApp", "WhatsApp"] }))
            .unwrap();
        let kept = json!({ "hide_notifications": false, "hide_apps": ["Mail", "com.hnc.Discord", "WhatsApp"], "supported": mac });
        assert_eq!(s.privacy(), kept);
        assert_eq!(ferrocore::obs::privacy_hide(&s.doc.borrow()), vec!["Mail", "com.hnc.Discord", "WhatsApp"]);
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
    fn a_fitted_source_stretches_from_a_side_handle() {
        // Owner's recording, 12 Nov: with the frame lock open, pulling a side handle of a
        // source added with Fit only grew or shrank it, its shape kept.
        let s = stage();
        let it = ok(s.add_source("Main", "color_source_v3", "Wide", &json!({ "width": 1600, "height": 900 }), true));
        assert_eq!(ok(s.transform("Main", it.id)).bounds, "fit");
        // Same shape (a corner, or the lock on): still Fit.
        ok(s.set_item_box("Main", it.id, 0.0, 0.0, 800.0, 450.0));
        assert_eq!(ok(s.transform("Main", it.id)).bounds, "fit");
        // Wider: the picture is that wide, not fitted back into its shape.
        let wide = ok(s.set_item_box("Main", it.id, 0.0, 0.0, 1200.0, 450.0));
        assert_eq!(wide.frame, [1200.0, 0.0, 0.0, 450.0, 0.0, 0.0], "stretched to the box");
        assert_eq!(ok(s.transform("Main", it.id)).bounds, "stretch");
        // A fitted source scaled twice as wide (CodeRabbit, 12 Nov): its shape is 32:9, so a
        // 32:9 box keeps it fitted.
        let wide = ok(s.add_source("Main", "color_source_v3", "Twice", &json!({ "width": 1600, "height": 900 }), true));
        let mut t = ok(s.transform("Main", wide.id));
        (t.scale_x, t.scale_y) = (2.0, 1.0);
        ok(s.set_transform("Main", wide.id, &t));
        ok(s.set_item_box("Main", wide.id, 0.0, 0.0, 800.0, 225.0));
        assert_eq!(ok(s.transform("Main", wide.id)).bounds, "fit");
    }

    /// Several items moved and sized at once (align, distribute, a group moved or resized): all
    /// of them or none, so one step can be undone whole.
    /// Sending an item its own box back changes nothing: not for a flipped item in bounds, whose
    /// drawn corner is not where the item's `x, y` say (Neon #7: a 1 pixel nudge threw it a whole
    /// width across).
    #[test]
    fn an_items_own_box_sent_back_leaves_it_where_it_is() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        for (flip_x, flip_y, bounds, align) in
            [(-1.0, 1.0, "fit", 0), (1.0, -1.0, "fit", 0), (-1.0, -1.0, "stretch", 5), (-1.0, 1.0, "fit", 5)]
        {
            let mut t = ok(s.transform("Main", a.id));
            t.bounds = bounds.into();
            (t.bounds_width, t.bounds_height, t.bounds_alignment) = (100.0, 100.0, align);
            (t.scale_x, t.scale_y) = (flip_x, flip_y);
            (t.x, t.y, t.alignment) = (100.0, 100.0, 5);
            ok(s.set_transform("Main", a.id, &t));
            let frame =
                |s: &Stage| ok(s.items("Main")).into_iter().find(|i| i.id == a.id).map(|i| i.frame).unwrap_or([0.0; 6]);
            let f = frame(&s);
            let (w, h) = (f[0].hypot(f[1]), f[2].hypot(f[3]));
            ok(s.set_item_box("Main", a.id, f[4], f[5], w, h));
            let g = frame(&s);
            for k in 0..6 {
                assert!((f[k] - g[k]).abs() < 0.01, "{bounds} {flip_x},{flip_y} align {align}: {f:?} became {g:?}");
            }
            // And a nudge of 1 pixel moves it by 1.
            ok(s.set_item_box("Main", a.id, f[4] + 1.0, f[5], w, h));
            let n = frame(&s);
            assert!((n[4] - f[4] - 1.0).abs() < 0.01 && (n[5] - f[5]).abs() < 0.01, "{bounds}: nudge {f:?} -> {n:?}");
        }
    }

    /// A group and a source inside it can't be asked for together: moving the source fits the
    /// group again, so the group's own box would then be applied to the changed group and move
    /// the source a second time (Neon #4).
    #[test]
    fn a_group_and_its_own_source_are_never_moved_in_one_batch() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 100, "height": 100 }), false));
        let g = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        let before = s.snapshot();
        let gb = ok(s.items("Main")).into_iter().find(|i| i.id == g.id).unwrap_or_else(|| panic!("the group"));
        let kid = gb.children[0].clone();
        for order in [[0, 1], [1, 0]] {
            let asks = [
                (g.id, gb.frame[4] + 100.0, gb.frame[5], gb.frame[0], gb.frame[3]),
                (kid.id, kid.frame[4] + 100.0, kid.frame[5], kid.frame[0], kid.frame[3]),
            ];
            let asks = [asks[order[0]], asks[order[1]]];
            let err = s.set_item_boxes("Main", &asks).unwrap_err().to_string();
            assert!(err.contains("together"), "{err}");
            assert_eq!(s.snapshot(), before, "nothing moved");
        }
        // Each alone is fine, and two sources of the group are too.
        ok(s.set_item_boxes("Main", &[(g.id, gb.frame[4] + 10.0, gb.frame[5], gb.frame[0], gb.frame[3])]));
        ok(s.set_item_boxes("Main", &[(kid.id, kid.frame[4], kid.frame[5], kid.frame[0], kid.frame[3])]));
    }

    #[test]
    fn several_items_are_moved_together_or_not_at_all() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 100, "height": 100 }), false));
        let c = ok(s.add_source("Main", "color_source_v3", "C", &json!({ "width": 100, "height": 100 }), false));
        let frames = |s: &Stage| -> Vec<[f32; 6]> {
            let list = ok(s.items("Main"));
            [a.id, b.id, c.id].iter().map(|id| list.iter().find(|i| i.id == *id).map(|i| i.frame).unwrap()).collect()
        };
        let boxes = [(a.id, 10.0, 20.0, 200.0, 100.0), (b.id, 300.0, 40.0, 50.0, 50.0), (c.id, 0.0, 0.0, 100.0, 100.0)];
        let moved = ok(s.set_item_boxes("Main", &boxes));
        assert_eq!(moved.iter().map(|i| i.id).collect::<Vec<_>>(), [a.id, b.id, c.id], "in the order asked");
        assert_eq!(
            frames(&s),
            [
                [200.0, 0.0, 0.0, 100.0, 10.0, 20.0],
                [50.0, 0.0, 0.0, 50.0, 300.0, 40.0],
                [100.0, 0.0, 0.0, 100.0, 0.0, 0.0]
            ]
        );
        // One that isn't there: nothing moves, and it says which.
        let before = s.snapshot();
        let bad = [(a.id, 500.0, 500.0, 10.0, 10.0), (999, 0.0, 0.0, 10.0, 10.0)];
        let err = s.set_item_boxes("Main", &bad).unwrap_err().to_string();
        assert!(err.contains("999"), "{err}");
        assert_eq!(s.snapshot(), before, "the first one wasn't moved either");
        // Nothing asked, nothing done.
        assert!(ok(s.set_item_boxes("Main", &[])).is_empty());

        // Items of a group move with the group's map applied, as one at a time does.
        let group = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        let inside = ok(s.set_item_boxes("Main", &[(a.id, 0.0, 0.0, 200.0, 100.0), (b.id, 400.0, 0.0, 50.0, 50.0)]));
        assert_eq!(inside.len(), 2);
        assert!(group.group);
        // What comes back is the final state, after the group has been fitted round the sources
        // moved in it (the first one's answer isn't from before the second moved).
        let after = ok(s.items("Main"));
        let now = |id: i64| {
            after.iter().flat_map(|i| std::iter::once(i.clone()).chain(i.children.clone())).find(|i| i.id == id)
        };
        for r in &inside {
            assert_eq!(Some(r.frame), now(r.id).map(|i| i.frame), "item {} as it ended up", r.id);
        }
    }

    /// Each item is shown on the stream, in the recording or both; both is the default, nothing
    /// is saved for it, and a collection from before this has every item shown everywhere.
    #[test]
    fn an_item_is_shown_on_the_stream_in_the_recording_or_both() {
        let s = stage();
        let red = ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "width": 200, "height": 100 }), false));
        let blue = ok(s.add_source("Main", "color_source_v3", "Blue", &json!({ "width": 50, "height": 50 }), false));
        let show = |s: &Stage, id: i64| ok(s.items("Main")).into_iter().find(|i| i.id == id).map(|i| i.show);
        assert_eq!(show(&s, red.id).as_deref(), Some("both"));
        assert!(!s.has_selective_items());
        ok(s.edit_item("Main", red.id, ItemEdit::RecordingOnly));
        assert_eq!(show(&s, red.id).as_deref(), Some("recording"));
        assert_eq!(show(&s, blue.id).as_deref(), Some("both"), "only that item");
        assert!(s.has_selective_items());
        // It is saved with the collection, and comes back.
        let saved = s.save("Test");
        let back = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &saved, "Main"));
        assert_eq!(show(&back, red.id).as_deref(), Some("recording"));
        ok(s.edit_item("Main", red.id, ItemEdit::StreamOnly));
        assert_eq!(show(&s, red.id).as_deref(), Some("stream"));
        // Both again: nothing stays in the file, so it is the collection it was.
        ok(s.edit_item("Main", red.id, ItemEdit::StreamAndRecording));
        assert_eq!(show(&s, red.id).as_deref(), Some("both"));
        assert!(!s.save("Test").to_string().contains("ferrocast_show"), "no trace of it");
        assert!(!s.has_selective_items());
        // A hidden item isn't on either picture, so it differs on neither.
        ok(s.edit_item("Main", red.id, ItemEdit::RecordingOnly));
        ok(s.edit_item("Main", red.id, ItemEdit::Hide));
        assert!(!s.has_selective_items());
        ok(s.edit_item("Main", red.id, ItemEdit::Show));
        assert!(s.has_selective_items());
        assert!(s.edit_item("Main", 999, ItemEdit::StreamOnly).is_err(), "no such item");
    }

    /// Taking a group apart, or copying an item, never changes where something is shown: ungrouped
    /// sources keep what the group had (a recording-only group's sources must not appear on the
    /// stream), and a copy shows where its original does.
    #[test]
    fn ungrouping_and_duplicating_keep_where_things_are_shown() {
        let s = stage();
        let show = |s: &Stage, name: &str| -> Option<(String, bool)> {
            ok(s.items("Main")).into_iter().find(|i| i.name == name).map(|i| (i.show, i.visible))
        };
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 50, "height": 50 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 50, "height": 50 }), false));
        ok(s.add_source("Main", "color_source_v3", "C", &json!({ "width": 50, "height": 50 }), false));
        // A recording-only group: both of its sources go on being recording-only.
        let g = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        ok(s.edit_item("Main", g.id, ItemEdit::RecordingOnly));
        ok(s.ungroup("Main", g.id));
        assert_eq!(show(&s, "A"), Some(("recording".into(), true)));
        assert_eq!(show(&s, "B"), Some(("recording".into(), true)));
        // A source with its own flag keeps it when the group was for both; two that conflict
        // (stream-only in a recording-only group) were shown nowhere, and stay out of sight.
        // (Ungrouping gives its sources new ids: they are looked up by name from here.)
        let id_of = |s: &Stage, name: &str| ok(s.items("Main")).into_iter().find(|i| i.name == name).map(|i| i.id);
        let (a_id, c_id) = (id_of(&s, "A").unwrap_or(0), id_of(&s, "C").unwrap_or(0));
        let g2 = ok(s.group_together("Main", &[a_id, c_id], "Other"));
        let c_inside =
            ok(s.items("Main")).into_iter().flat_map(|i| i.children).find(|i| i.name == "C").map_or(0, |i| i.id);
        ok(s.edit_item("Main", c_inside, ItemEdit::StreamOnly));
        ok(s.edit_item("Main", g2.id, ItemEdit::RecordingOnly));
        ok(s.ungroup("Main", g2.id));
        assert_eq!(show(&s, "A"), Some(("recording".into(), true)), "the group's flag is the same as its own");
        assert_eq!(show(&s, "C"), Some(("stream".into(), false)), "shown in neither before: hidden now");
        // A copy of a flagged item shows where the original does.
        let b_id = id_of(&s, "B").unwrap_or(0);
        ok(s.edit_item("Main", b_id, ItemEdit::StreamOnly));
        ok(s.edit_item("Main", b_id, ItemEdit::Duplicate));
        assert_eq!(show(&s, "B 2"), Some(("stream".into(), true)));
    }

    /// `has_selective_items` (it decides whether a recording can share the stream's encode) says
    /// what the engine draws: what is on air in Studio Mode, but not a hidden group, not the
    /// vertical picture's scenes, and not the copies OBS keeps of a group's items.
    #[test]
    fn selective_items_are_counted_as_the_engine_draws_them() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 50, "height": 50 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 50, "height": 50 }), false));
        let group = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        ok(s.edit_item("Main", a.id, ItemEdit::StreamOnly));
        assert!(s.has_selective_items(), "a flagged source in a shown group");
        // A hidden group shows nothing, whatever is flagged in it.
        ok(s.edit_item("Main", group.id, ItemEdit::Hide));
        assert!(!s.has_selective_items(), "a hidden group");
        ok(s.edit_item("Main", group.id, ItemEdit::Show));
        assert!(s.has_selective_items());
        ok(s.edit_item("Main", a.id, ItemEdit::StreamAndRecording));
        assert!(!s.has_selective_items());

        // The vertical picture has no recording of its own: its flags are not the wide one's.
        ok(s.start_vertical("Vertical"));
        let v = ok(s.add_source("Vertical", "color_source_v3", "V", &json!({ "width": 50, "height": 50 }), false));
        ok(s.edit_item("Vertical", v.id, ItemEdit::RecordingOnly));
        assert!(!s.has_selective_items(), "a flag on the vertical picture's scene");
        ok(s.edit_item("Main", b.id, ItemEdit::RecordingOnly));
        assert!(s.has_selective_items(), "the wide picture's own");
        ok(s.edit_item("Main", b.id, ItemEdit::StreamAndRecording));
        assert!(!s.has_selective_items());

        // Studio Mode: what is on air is a copy of the scene's items; resetting the preview's
        // item to both doesn't change the copy, which still carries the flag.
        let c = ok(s.add_source("Main", "color_source_v3", "C", &json!({ "width": 50, "height": 50 }), false)).id;
        ok(s.edit_item("Main", c, ItemEdit::RecordingOnly));
        s.set_studio_mode(true);
        ok(s.edit_item("Main", c, ItemEdit::StreamAndRecording));
        assert!(s.has_selective_items(), "the copy on air still has a recording-only source");
        s.set_studio_mode(false);
        assert!(!s.has_selective_items(), "and with Studio Mode off, it's gone");
    }

    /// The copies OBS keeps of a group's items in the scene (for versions before groups) are not
    /// drawn, so a flag on one is no flag.
    #[test]
    fn a_groups_backup_copies_are_not_counted() {
        let doc = json!({
            "sources": [
                { "id": "scene", "name": "Main", "settings": { "items": [
                    { "name": "Cam", "id": 1, "group_item_backup": true, "ferrocast_show": "stream", "visible": true },
                    { "name": "Cam", "id": 2, "visible": true },
                ] } },
                { "id": "color_source_v3", "name": "Cam", "settings": {} },
            ],
            "current_scene": "Main", "current_program_scene": "Main",
        });
        let s = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &doc, "Main"));
        assert!(!s.has_selective_items());
        let flagged = json!({ "sources": [
            { "id": "scene", "name": "Main", "settings": { "items": [{ "name": "Cam", "id": 2, "ferrocast_show": "stream" }] } },
            { "id": "color_source_v3", "name": "Cam", "settings": {} },
        ], "current_scene": "Main", "current_program_scene": "Main" });
        assert!(ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &flagged, "Main")).has_selective_items());
    }

    /// A flag on a group is the group's own (it covers the children when drawn); a child of a
    /// group keeps its own, and either makes the picture selective.
    #[test]
    fn a_group_and_its_children_each_keep_where_they_are_shown() {
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 50, "height": 50 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 50, "height": 50 }), false));
        let group = ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        assert_eq!(group.show, "both");
        assert!(!s.has_selective_items());
        // A child, reached through the scene as OBS's items are.
        ok(s.edit_item("Main", a.id, ItemEdit::StreamOnly));
        let kids = ok(s.items("Main"))[0].children.clone();
        assert_eq!(kids.iter().find(|k| k.id == a.id).map(|k| k.show.as_str()), Some("stream"));
        assert_eq!(kids.iter().find(|k| k.id == b.id).map(|k| k.show.as_str()), Some("both"));
        assert!(s.has_selective_items(), "a flagged child is enough");
        ok(s.edit_item("Main", a.id, ItemEdit::StreamAndRecording));
        assert!(!s.has_selective_items());
        // The group itself.
        ok(s.edit_item("Main", group.id, ItemEdit::RecordingOnly));
        assert_eq!(ok(s.items("Main"))[0].show, "recording");
        assert!(s.has_selective_items());
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

    fn color(s: &Stage, name: &str) -> ItemInfo {
        ok(s.add_source("Main", "color_source_v3", name, &json!({ "width": 200, "height": 100 }), false))
    }

    #[test]
    fn a_source_copied_in_one_collection_pastes_into_another_with_its_place() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        ok(a.set_item_box("Main", red.id, 100.0, 50.0, 400.0, 200.0));
        let clip = ok(a.copy_source("Main", red.id));
        let (info, notes) = ok(b.paste_source("Main", &clip));
        assert!(notes.is_empty());
        assert_eq!(info.frame, [400.0, 0.0, 0.0, 200.0, 100.0, 50.0], "the same place and size");
        assert!(b.sources().iter().any(|u| u.name == "Red"));
        // The copy has its own identity: another paste is "Red 2", and the first is left alone.
        let (second, _) = ok(b.paste_source("Main", &clip));
        assert!(b.sources().iter().any(|u| u.name == "Red 2"));
        assert_ne!(info.id, second.id);
        let (third, _) = ok(b.paste_source("Main", &clip));
        assert!(b.sources().iter().any(|u| u.name == "Red 3"));
        assert_ne!(second.id, third.id);
    }

    #[test]
    fn a_clip_survives_a_json_round_trip() {
        let a = stage();
        let red = color(&a, "Red");
        let clip = ok(a.copy_source("Main", red.id));
        assert_eq!(SourceClip::from_json(&clip.to_json()), Some(clip));
        assert!(SourceClip::from_json(&json!({ "item": {} })).is_none());
        assert!(FilterClip::from_json(&json!({ "filters": [] })).is_none());
    }

    #[test]
    fn a_group_copy_carries_its_children() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let clip = ok(a.copy_source("Main", g.id));
        assert_eq!(clip.sources.len(), 3, "the group and both its sources");
        // Blue is already in the destination: the pasted child takes another name.
        color(&b, "Blue");
        let (info, _) = ok(b.paste_source("Main", &clip));
        let names: Vec<String> = b.sources().iter().map(|u| u.name.clone()).collect();
        for n in ["Red", "Blue 2"] {
            assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
        }
        assert!(info.group, "pasted as a group");
        let parts = ok(b.group_parts("Main", info.id));
        assert_eq!(parts.0, "Pair");
        assert!(parts.1.iter().any(|n| n == "Blue 2") && parts.1.iter().any(|n| n == "Red"), "{parts:?}");
    }

    #[test]
    fn a_pasted_group_children_never_share_an_id_with_the_scene() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let clip = ok(a.copy_source("Main", g.id));
        // The destination already uses ids 1 and 2 (the children's ids in the copy).
        color(&b, "One");
        color(&b, "Two");
        let (info, _) = ok(b.paste_source("Main", &clip));
        let mut ids: Vec<i64> = Vec::new();
        for it in ok(b.items("Main")) {
            ids.push(it.id);
            ids.extend(it.children.iter().map(|c| c.id));
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "every id in the scene is its own: {ids:?}");
        assert_eq!(ok(b.items("Main")).iter().find(|i| i.id == info.id).map(|i| i.children.len()), Some(2));
        // Editing a child touches that child alone.
        let kid =
            ok(b.items("Main")).into_iter().find(|i| i.id == info.id).and_then(|i| i.children.first().map(|c| c.id));
        ok(b.edit_item("Main", kid.unwrap_or(0), ItemEdit::Hide));
        let after = ok(b.items("Main"));
        assert!(after.iter().filter(|i| !i.group).all(|i| i.visible), "the others still show");
        assert!(after.iter().find(|i| i.group).is_some_and(|g| g.children.iter().filter(|c| !c.visible).count() == 1));
    }

    #[test]
    fn a_source_copied_out_of_a_group_keeps_its_place_on_the_canvas() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        ok(a.set_item_box("Main", red.id, 300.0, 120.0, 200.0, 100.0));
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let kids = ok(a.items("Main")).into_iter().find(|i| i.id == g.id).map(|i| i.children).unwrap_or_default();
        let red_id = kids.iter().find(|c| c.name == "Red").map(|c| c.id).unwrap_or(0);
        let clip = ok(a.copy_source("Main", red_id));
        let (info, _) = ok(b.paste_source("Main", &clip));
        let t = ok(b.transform("Main", info.id));
        assert!((t.x - 300.0).abs() < 0.5 && (t.y - 120.0).abs() < 0.5, "on the canvas where it was: {} {}", t.x, t.y);
    }

    #[test]
    fn a_filter_this_computer_lacks_stops_a_source_paste_before_anything_is_added() {
        let (a, b) = (stage(), stage());
        color(&a, "Red");
        ok(a.add_filter("Red", "gain_filter", "Boost", &json!({})));
        let id = ok(a.items("Main"))[0].id;
        let mut clip = ok(a.copy_source("Main", id));
        clip.sources[0]["filters"][0]["versioned_id"] = json!("alien_filter_from_another_os");
        let e = b.paste_source("Main", &clip).unwrap_err().to_string();
        assert!(e.contains("can't run") && e.contains("Boost"), "{e}");
        assert!(b.sources().iter().all(|u| u.name != "Red"), "nothing was added");
        let mut over = ok(a.copy_source("Main", id));
        over.item["ferrocast_filters"] =
            json!([{ "name": "Odd", "versioned_id": "alien", "id": "alien", "settings": {} }]);
        assert!(b.paste_source("Main", &over).is_err(), "an item's own filters are checked too");
    }

    #[test]
    fn a_sidechain_named_by_uuid_is_kept_when_that_source_is_here() {
        let (a, b) = (stage(), stage());
        ok(a.add_source("Main", "color_source_v3", "Mic", &json!({}), false));
        let music_uuid = ok(b.add_source("Main", "color_source_v3", "Music", &json!({}), false));
        let _ = music_uuid;
        let uuid_of_music = b
            .sources_list()
            .iter()
            .find(|s| name_of(s) == "Music")
            .map(|s| s["uuid"].as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        ok(a.add_filter("Mic", "compressor_filter", "Duck", &json!({ "sidechain_source": uuid_of_music })));
        let clip = ok(a.copy_filters("Mic", None));
        ok(b.add_source("Main", "color_source_v3", "Voice", &json!({}), false));
        assert!(ok(b.paste_filter_values("Voice", None, &clip)).is_empty(), "a uuid that is here stays");
        let gone = ok(a.copy_filters("Mic", None));
        let mut other = gone.clone();
        other.filters[0]["settings"]["sidechain_source"] = json!("00000000-dead-beef");
        let notes = ok(b.paste_filter_values("Voice", None, &other));
        assert_eq!(notes.len(), 1, "a uuid that isn't here is cleared with a note: {notes:?}");
    }

    #[test]
    fn a_name_at_the_length_limit_still_gets_a_free_copy_name() {
        let a = stage();
        let long = "n".repeat(100);
        let red = ok(a.add_source("Main", "color_source_v3", &long, &json!({}), false));
        let clip = ok(a.copy_source("Main", red.id));
        // Used to loop forever: "<100 letters> 2" is longer than a name may be.
        let (info, _) = ok(a.paste_source("Main", &clip));
        assert!(info.name.chars().count() <= 100 && info.name != long, "{}", info.name);
        let (second, _) = ok(a.paste_source("Main", &clip));
        assert_ne!(second.name, info.name);
        assert_eq!(numbered("abc", 2), "abc 2");
        assert_eq!(numbered(&"x".repeat(99), 12).chars().count(), 100);
    }

    #[test]
    fn sources_without_uuids_get_uuids_of_their_own() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let mut clip = ok(a.copy_source("Main", g.id));
        for src in clip.sources.iter_mut() {
            src.as_object_mut().map(|o| o.remove("uuid"));
        }
        ok(b.paste_source("Main", &clip));
        let ids: Vec<String> = b.sources_list().iter().map(|s| s["uuid"].as_str().unwrap_or("").to_string()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "no two sources share a uuid: {ids:?}");
        assert!(ids.iter().all(|u| !u.is_empty()));
    }

    #[test]
    fn a_kind_the_collection_already_runs_is_not_refused_as_foreign() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let mut clip = ok(a.copy_source("Main", red.id));
        clip.sources[0]["versioned_id"] = json!("legacy_source_kind");
        clip.sources[0]["filters"] =
            json!([{ "name": "Old", "versioned_id": "legacy_filter_kind", "uuid": "f1", "settings": {} }]);
        assert!(b.paste_source("Main", &clip).is_err(), "unknown here: refused");
        b.edit(|d| {
            if let Some(l) = d["sources"].as_array_mut() {
                l.push(json!({ "id": "legacy_source_kind", "versioned_id": "legacy_source_kind", "name": "Have", "uuid": "u9",
                    "settings": {}, "filters": [{ "name": "X", "versioned_id": "legacy_filter_kind", "uuid": "f9", "settings": {} }] }));
            }
        });
        ok(b.paste_source("Main", &clip));
    }

    #[test]
    fn a_pasted_groups_children_filters_are_checked_and_get_new_ids() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let mut clip = ok(a.copy_source("Main", g.id));
        let kid = |c: &mut SourceClip| {
            c.sources[0]["settings"]["items"][0]["ferrocast_filters"] = json!([{ "name": "Own", "versioned_id": "gain_filter", "id": "gain_filter", "uuid": "same-uuid", "settings": {} }]);
        };
        kid(&mut clip);
        ok(b.paste_source("Main", &clip));
        let items = b.sources_list().into_iter().find(|s| name_of(s) == "Pair").unwrap_or_default();
        let own = &items["settings"]["items"][0]["ferrocast_filters"][0];
        assert_ne!(own["uuid"], json!("same-uuid"), "a new id");
        let mut bad = ok(a.copy_source("Main", g.id));
        kid(&mut bad);
        bad.sources[0]["settings"]["items"][0]["ferrocast_filters"][0]["versioned_id"] = json!("alien_filter");
        assert!(b.paste_source("Main", &bad).is_err(), "a child's own filter of a kind this computer lacks");
    }

    #[test]
    fn a_held_filter_key_changes_the_sources_filter_not_a_pictures_own_copy() {
        let a = stage();
        ok(a.add_source("Main", "color_source_v3", "Cam", &json!({}), false));
        ok(a.add_filter("Cam", "color_filter", "Boost", &json!({})));
        ok(a.add_source("Main", "color_source_v3", "Other", &json!({}), false));
        let it = ok(a.add_existing("Main", "Cam"));
        ok(a.set_item_filter_scope("Main", it.id, true));
        ok(a.set_source_filter_enabled("Cam", "Boost", false));
        let own = ok(a.item_filters("Main", it.id));
        assert!(own.iter().any(|f| f.name == "Boost" && f.enabled), "the picture's own copy is as it was");
        assert!(ok(a.filters("Cam")).iter().any(|f| f.name == "Boost" && !f.enabled));
    }

    #[test]
    fn a_source_added_after_a_group_paste_gets_an_id_no_child_has() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, blue.id], "Pair"));
        let clip = ok(a.copy_source("Main", g.id));
        ok(b.paste_source("Main", &clip));
        let later = color(&b, "Later");
        let mut ids = vec![later.id];
        for it in ok(b.items("Main")) {
            if it.id != later.id {
                ids.push(it.id);
            }
            ids.extend(it.children.iter().map(|c| c.id));
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "no id repeats: {ids:?}");
    }

    /// What the engine is shown while keys are held, from the stage.
    fn live_doc(s: &Stage) -> Value {
        s.live(|d| d.clone())
    }

    fn visible_in(doc: &Value, scene: &str, id: i64) -> Option<bool> {
        let src = doc["sources"].as_array()?.iter().find(|x| name_of(x) == scene)?;
        let items = src.pointer("/settings/items")?.as_array()?;
        items.iter().find(|i| i["id"].as_i64() == Some(id))?["visible"].as_bool()
    }

    #[test]
    fn a_held_item_is_only_in_the_engines_view_never_in_the_collection() {
        let s = stage();
        let red = color(&s, "Red");
        ok(s.edit_item("Main", red.id, ItemEdit::Hide));
        let before = s.snapshot();
        ok(s.hold_item("Main", red.id, Some(true)));
        assert_eq!(visible_in(&live_doc(&s), "Main", red.id), Some(true), "shown to the engine while held");
        // What a save, an autosave, an export and an undo snapshot see is the document.
        assert_eq!(visible_in(&s.snapshot(), "Main", red.id), Some(false));
        assert_eq!(s.snapshot(), before, "nothing changed in the collection");
        assert_eq!(visible_in(&s.save("Saved"), "Main", red.id), Some(false), "a save while held");
        // Released: back to the document.
        ok(s.hold_item("Main", red.id, None));
        assert_eq!(visible_in(&live_doc(&s), "Main", red.id), Some(false));
    }

    #[test]
    fn undo_across_a_hold_never_brings_the_held_state_back() {
        let s = stage();
        let red = color(&s, "Red");
        ok(s.edit_item("Main", red.id, ItemEdit::Hide));
        ok(s.hold_item("Main", red.id, Some(true)));
        let taken = s.snapshot(); // what the history takes while the key is down
        ok(s.edit_item("Main", red.id, ItemEdit::Lock));
        ok(s.hold_item("Main", red.id, None));
        s.restore_all(&taken);
        assert_eq!(visible_in(&s.snapshot(), "Main", red.id), Some(false), "the snapshot has the real state");
        assert_eq!(visible_in(&live_doc(&s), "Main", red.id), Some(false));
    }

    #[test]
    fn a_held_item_inside_a_group_and_a_held_filter_are_overlaid_too() {
        let s = stage();
        let red = color(&s, "Red");
        let blue = color(&s, "Blue");
        let g = ok(s.group_together("Main", &[red.id, blue.id], "Pair"));
        let kid = ok(s.items("Main"))
            .into_iter()
            .find(|i| i.id == g.id)
            .and_then(|i| i.children.first().map(|c| c.id))
            .unwrap_or(0);
        ok(s.edit_item("Main", kid, ItemEdit::Hide));
        ok(s.hold_item("Main", kid, Some(true)));
        assert_eq!(visible_in(&live_doc(&s), "Pair", kid), Some(true));
        assert_eq!(visible_in(&s.snapshot(), "Pair", kid), Some(false));
        ok(s.add_filter("Red", "color_filter", "Tint", &json!({})));
        ok(s.hold_filter("Red", "Tint", Some(false)));
        let on = |d: &Value| {
            d["sources"]
                .as_array()
                .and_then(|l| l.iter().find(|x| name_of(x) == "Red"))
                .and_then(|r| r["filters"][0]["enabled"].as_bool())
        };
        assert_eq!(on(&live_doc(&s)), Some(false));
        assert_eq!(on(&s.snapshot()), Some(true), "the collection keeps the filter on");
        assert!(s.hold_filter("Red", "Nope", Some(true)).is_err(), "no such filter");
        assert!(s.hold_item("Main", 9999, Some(true)).is_err(), "no such item");
    }

    #[test]
    fn a_reopened_collection_shows_nothing_held() {
        // A crash or a quit while held: what is on disk is the document, and a new stage has no holds.
        let a = stage();
        let red = color(&a, "Red");
        ok(a.edit_item("Main", red.id, ItemEdit::Hide));
        ok(a.hold_item("Main", red.id, Some(true)));
        let on_disk = a.snapshot();
        let b = stage();
        b.restore_all(&on_disk);
        assert_eq!(visible_in(&live_doc(&b), "Main", red.id), Some(false));
    }

    #[test]
    fn pasted_filter_names_stay_within_the_name_limit_and_sidechains_see_global_channels() {
        let a = stage();
        let long = "f".repeat(100);
        ok(a.add_source("Main", "color_source_v3", "Mic", &json!({}), false));
        ok(a.add_filter("Mic", "gain_filter", &long, &json!({})));
        let clip = ok(a.copy_filters("Mic", None));
        ok(a.paste_filter_values("Mic", None, &clip));
        assert!(ok(a.filters("Mic")).iter().all(|f| f.name.chars().count() <= 100), "names fit");
        assert_eq!(ok(a.filters("Mic")).len(), 2);
    }

    #[test]
    fn a_sound_bars_source_listens_to_the_pasted_copy_and_a_source_clone_is_remapped() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let bars = ok(a.add_source("Main", "ferrocast_visualizer", "Bars", &json!({ "source": "Red" }), false));
        let blue = color(&a, "Blue");
        let g = ok(a.group_together("Main", &[red.id, bars.id, blue.id], "Pair"));
        let clip = ok(a.copy_source("Main", g.id));
        color(&b, "Red"); // a name taken: the pasted one becomes "Red 2"
        ok(b.paste_source("Main", &clip));
        let src = |n: &str| b.sources_list().into_iter().find(|s| name_of(s) == n);
        assert_eq!(src("Bars").and_then(|s| s.pointer("/settings/source").cloned()), Some(json!("Red 2")));
    }

    #[test]
    fn a_scene_is_not_copied_as_a_source() {
        let a = stage();
        ok(a.add_scene("Two"));
        let it = ok(a.add_existing("Main", "Two"));
        assert!(a.copy_source("Main", it.id).is_err());
        assert!(a.copy_source("Main", 999).is_err(), "no such item");
        assert!(a.copy_source("Nope", 1).is_err(), "no such scene");
    }

    #[test]
    fn a_clone_needs_its_target_in_the_destination() {
        let (a, b) = (stage(), stage());
        color(&a, "Red");
        let copy = ok(a.add_source("Main", "ferrocast_clone", "Copy", &json!({ "clone": "Red" }), false));
        let clip = ok(a.copy_source("Main", copy.id));
        let e = b.paste_source("Main", &clip).unwrap_err().to_string();
        assert!(e.contains("Red") && e.contains("paste that source first"), "{e}");
        assert!(!b.sources().iter().any(|u| u.name == "Copy"), "nothing half-pasted");
        color(&b, "Red");
        ok(b.paste_source("Main", &clip));
        assert!(b.sources().iter().any(|u| u.name == "Copy"));
    }

    #[test]
    fn a_kind_this_computer_cannot_run_is_refused() {
        let (a, b) = (stage(), stage());
        let red = color(&a, "Red");
        let mut clip = ok(a.copy_source("Main", red.id));
        clip.sources[0]["versioned_id"] = json!("alien_capture_from_another_os");
        let e = b.paste_source("Main", &clip).unwrap_err().to_string();
        assert!(e.contains("can't run") && e.contains("Red"), "{e}");
        assert!(b.sources().iter().all(|u| u.name != "Red"));
        assert!(b.paste_source("Main", &SourceClip { item: json!({}), sources: vec![] }).is_err(), "an empty copy");
        assert!(b.paste_source("Nowhere", &ok(a.copy_source("Main", red.id))).is_err(), "no such scene");
    }

    #[test]
    fn filters_paste_across_collections_with_names_free_and_a_sidechain_set_to_none() {
        let (a, b) = (stage(), stage());
        let mic_a = ok(a.add_source("Main", "color_source_v3", "Mic", &json!({}), false));
        ok(a.add_filter("Mic", "gain_filter", "Boost", &json!({ "db": 3.0 })));
        ok(a.add_filter("Mic", "compressor_filter", "Duck", &json!({ "sidechain_source": "Music" })));
        let clip = ok(a.copy_filters("Mic", None));
        assert_eq!(clip.filters.len(), 2);
        assert_eq!(clip.from, "Mic");
        let _ = mic_a;
        ok(b.add_source("Main", "color_source_v3", "Voice", &json!({}), false));
        ok(b.add_filter("Voice", "gain_filter", "Boost", &json!({})));
        let notes = ok(b.paste_filter_values("Voice", None, &clip));
        let names: Vec<String> = ok(b.filters("Voice")).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["Boost", "Boost 2", "Duck"], "a taken name gets a number");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("Music") && notes[0].contains("None"), "{notes:?}");
        // Pasting twice does not clash.
        ok(b.paste_filter_values("Voice", None, &clip));
        assert_eq!(ok(b.filters("Voice")).len(), 5);
    }

    #[test]
    fn a_sidechain_to_a_source_that_is_there_is_kept() {
        let (a, b) = (stage(), stage());
        ok(a.add_source("Main", "color_source_v3", "Mic", &json!({}), false));
        ok(a.add_filter("Mic", "compressor_filter", "Duck", &json!({ "sidechain_source": "Music" })));
        let clip = ok(a.copy_filters("Mic", None));
        ok(b.add_source("Main", "color_source_v3", "Voice", &json!({}), false));
        ok(b.add_source("Main", "color_source_v3", "Music", &json!({}), false));
        assert!(ok(b.paste_filter_values("Voice", None, &clip)).is_empty());
    }

    #[test]
    fn filters_refuse_a_missing_target_an_unknown_kind_and_leave_nothing_behind() {
        let (a, b) = (stage(), stage());
        ok(a.add_source("Main", "color_source_v3", "Mic", &json!({}), false));
        ok(a.add_filter("Mic", "gain_filter", "Boost", &json!({})));
        let mut clip = ok(a.copy_filters("Mic", None));
        assert!(b.paste_filter_values("Ghost", None, &clip).is_err(), "no such source");
        ok(b.add_source("Main", "color_source_v3", "Voice", &json!({}), false));
        clip.filters[0]["versioned_id"] = json!("alien_filter_from_another_os");
        let e = b.paste_filter_values("Voice", None, &clip).unwrap_err().to_string();
        assert!(e.contains("can't run"), "{e}");
        assert!(ok(b.filters("Voice")).is_empty());
        assert!(a.copy_filters("Ghost", None).is_err());
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

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn plan(s: &Stage) -> std::collections::BTreeMap<String, Vec<String>> {
        super::cutout_plan(&s.sources_list(), &s.doc.borrow())
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn one_cutout_worker_serves_every_scene_a_camera_is_in() {
        let s = stage();
        let dev = json!({ "device": "cam-1" });
        ok(s.add_source("Main", "av_capture_input_v2", "Cam", &dev, false));
        ok(s.add_filter("Cam", "ferrocast_remove_background", "Cut", &json!({})));
        ok(s.add_scene("Other"));
        let other = ok(s.add_existing("Other", "Cam"));
        assert!(other.own_filters, "copy-on-add gave the item its own filters");
        ok(s.duplicate_scene("Other", "Third"));
        let p = plan(&s);
        assert_eq!(p.len(), 1, "one worker for one camera: {p:?}");
        let masks = &p["Cam"];
        assert_eq!(masks.len(), 3, "{masks:?}");
        assert!(masks.contains(&"Cam: Cut".to_string()));
        assert!(masks.iter().any(|m| m.starts_with("Other#")));
        assert!(masks.iter().any(|m| m.starts_with("Third#")));
        // A hidden item feeds nothing.
        ok(s.edit_item("Other", other.id, ItemEdit::Hide));
        assert_eq!(plan(&s)["Cam"].len(), 2);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn renaming_a_source_moves_its_cutout_worker() {
        let s = stage();
        let dev = json!({ "device": "cam-1" });
        ok(s.add_source("Main", "av_capture_input_v2", "Cam", &dev, false));
        ok(s.add_filter("Cam", "ferrocast_remove_background", "Cut", &json!({})));
        ok(s.add_scene("Other"));
        ok(s.add_existing("Other", "Cam"));
        ok(s.rename("Cam", "Webcam"));
        let p = plan(&s);
        assert_eq!(p.keys().collect::<Vec<_>>(), vec!["Webcam"], "keyed by the source, so the old worker stops");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn studio_mode_copies_of_the_scene_on_air_get_their_masks() {
        let s = stage();
        let dev = json!({ "device": "cam-1" });
        ok(s.add_source("Main", "av_capture_input_v2", "Cam", &dev, false));
        ok(s.add_filter("Cam", "ferrocast_remove_background", "Cut", &json!({})));
        ok(s.add_scene("Other"));
        let other = ok(s.add_existing("Other", "Cam"));
        let items = s.scene_items("Other").unwrap_or_else(|e| panic!("{e}"));
        s.doc.borrow_mut()["ferrocast"]["program_copy"] =
            json!({ "scene": "Other", "items": items, "private": PROGRAM_COPY[0] });
        let masks = plan(&s)["Cam"].clone();
        assert!(masks.contains(&format!("{}#{}: Cut", PROGRAM_COPY[0], other.id)), "{masks:?}");
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

    #[test]
    fn screens_windows_and_pages_scale_sharply() {
        // Their text stays sharp when Fit to screen makes them bigger or smaller.
        for kind in ["screen_capture", "window_capture", "monitor_capture", "browser_source"] {
            let it = new_item(&json!({ "id": kind, "name": "S" }), 1);
            assert_eq!(it["scale_filter"], "bicubic", "{kind}");
        }
        // The rest as OBS adds them.
        let it = new_item(&json!({ "id": "color_source_v3", "name": "C" }), 1);
        assert_eq!(it["scale_filter"], "disable");
    }

    /// Owner, 24 Nov: Flip sideways, Flip upside down and Turn left or right swung the picture
    /// away (out of the canvas): they turned it about its top-left corner. Each now turns it in
    /// place, about its own middle, whatever its size, scale, crop or group.
    #[test]
    fn flips_and_turns_keep_the_item_where_it_is() {
        let s = stage();
        let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 0.05 && (a.1 - b.1).abs() < 0.05;
        let centre = |s: &Stage, scene: &str, id: i64| ok(s.item_centre(scene, id));
        let edits = [ItemEdit::FlipH, ItemEdit::FlipV, ItemEdit::RotateCw, ItemEdit::RotateCcw, ItemEdit::Rotate180];
        // A wide picture away from the corner, scaled, and one that is also cropped.
        for (name, crop) in [("Plain", 0), ("Cropped", 20)] {
            let it = ok(s.add_source("Main", "color_source_v3", name, &json!({ "width": 200, "height": 100 }), false));
            let mut t = ok(s.transform("Main", it.id));
            (t.x, t.y, t.scale_x, t.scale_y) = (300.0, 120.0, 1.5, 0.75);
            (t.crop_left, t.crop_top) = (crop, crop / 2);
            ok(s.set_transform("Main", it.id, &t));
            let before = centre(&s, "Main", it.id);
            for edit in edits {
                // Twice: the second one turns it back or on again, from a changed state.
                for _ in 0..2 {
                    ok(s.edit_item("Main", it.id, edit));
                    assert!(near(centre(&s, "Main", it.id), before), "{name}: {edit:?} moved it off its place");
                }
            }
        }
        // Inside a group the same, through the group's own map.
        let a = ok(s.add_source("Main", "color_source_v3", "GA", &json!({ "width": 80, "height": 40 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "GB", &json!({ "width": 60, "height": 60 }), false));
        ok(s.set_item_boxes("Main", &[(a.id, 10.0, 10.0, 80.0, 40.0), (b.id, 200.0, 150.0, 60.0, 60.0)]));
        ok(s.group_together("Main", &[a.id, b.id], "Pair"));
        let before = centre(&s, "Main", a.id);
        for edit in edits {
            ok(s.edit_item("Main", a.id, edit));
            assert!(near(centre(&s, "Main", a.id), before), "in a group: {edit:?} moved it off its place");
        }
        // A flip still flips and a turn still turns.
        let t = ok(s.transform("Main", ok(s.items("Main"))[0].id));
        assert!(t.scale_x.abs() > 0.0);
    }

    /// Owner, 25 Nov: dragging a crop handle (Alt) from the left towards the right, or from the
    /// top down, moved the whole source: the engine wrote the crop and left the box where it was,
    /// so the picture slid with it. The picture that stays visible stays where it is.
    #[test]
    fn cropping_keeps_the_visible_picture_where_it_is_on_the_canvas() {
        let s = stage();
        let (w, h) = (200.0f32, 100.0f32);
        // Where source pixel (sx, sy) is on the canvas, with the crop the item has now.
        let canvas_of = |s: &Stage, scene: &str, id: i64, sx: f32, sy: f32| {
            let t = ok(s.transform(scene, id));
            let seen = (w - t.crop_left as f32 - t.crop_right as f32, h - t.crop_top as f32 - t.crop_bottom as f32);
            ok(s.item_point(scene, id, ((sx - t.crop_left as f32) / seen.0, (sy - t.crop_top as f32) / seen.1)))
        };
        let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 0.05 && (a.1 - b.1).abs() < 0.05;
        // (flip x, flip y, rotation): a plain item, a mirrored one and a turned one.
        for (i, (fx, fy, rot)) in
            [(1.0f32, 1.0f32, 0.0f32), (-1.0, 1.0, 0.0), (1.0, -1.0, 90.0), (-1.0, -1.0, 270.0)].into_iter().enumerate()
        {
            let name = format!("Crop{i}");
            let it = ok(s.add_source("Main", "color_source_v3", &name, &json!({ "width": 200, "height": 100 }), false));
            let mut t = ok(s.transform("Main", it.id));
            (t.x, t.y, t.scale_x, t.scale_y, t.rotation) = (300.0, 200.0, 1.5 * fx, 0.75 * fy, rot);
            ok(s.set_transform("Main", it.id, &t));
            // Crops that move every side, one side at a time and several together, in and out.
            for crop in [[40, 0, 0, 0], [40, 10, 0, 0], [40, 10, 20, 5], [0, 10, 20, 5], [60, 30, 30, 30], [0, 0, 0, 0]]
            {
                let probes = [(100.0, 50.0), (80.0, 40.0), (150.0, 60.0)];
                // Pixels in the part that stays visible both before and now.
                let (now, then) = (ok(s.transform("Main", it.id)), crop);
                let keeps = |sx: f32, sy: f32| {
                    sx >= now.crop_left.max(then[0]) as f32
                        && sx <= w - now.crop_right.max(then[2]) as f32
                        && sy >= now.crop_top.max(then[1]) as f32
                        && sy <= h - now.crop_bottom.max(then[3]) as f32
                };
                let before: Vec<_> = probes.iter().map(|p| canvas_of(&s, "Main", it.id, p.0, p.1)).collect();
                ok(s.set_item_crop("Main", it.id, crop));
                for (p, was) in probes.iter().zip(&before) {
                    if keeps(p.0, p.1) {
                        assert!(
                            near(canvas_of(&s, "Main", it.id, p.0, p.1), *was),
                            "{name} crop {crop:?}: source pixel {p:?} moved on the canvas"
                        );
                    }
                }
            }
        }
    }

    /// The first-run setup reads the mic's level with no preview on screen: asking for a level
    /// starts the engine that opens the sound (it never started, and the meter stayed flat).
    #[test]
    fn asking_for_a_level_starts_the_engine_that_hears_the_source() {
        let s = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        assert!(s.engine.borrow().is_none(), "no engine until something asks for one");
        // No names: nothing to measure, nothing started.
        assert!(s.levels(&[]).is_empty());
        assert!(s.engine.borrow().is_none(), "an empty ask starts nothing");
        let _ = s.levels(&["Mic".to_string()]);
        if s.engine.borrow().is_none() {
            // Where no engine can start at all (no graphics adapter), there is nothing to check.
            assert!(s.engine().is_err(), "an engine could start, but the level ask did not start it");
        }
    }

    #[test]
    fn a_source_of_odd_size_is_screenshot_pixel_for_pixel() {
        // Owner's OBS comparison, 12 Nov: a 1917-wide window's screenshot came out 1918 wide,
        // scaled, its text blurred. Odd sides are kept; a recording of it alone crops to even.
        let s = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        // It records: skipped where nothing encodes (GitHub's Windows runner has no FFmpeg H.264).
        if s.engine().is_err() || !(cfg!(target_os = "macos") || ferrocore::ffenc::h264_encoder().is_some()) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocast-odd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // One-pixel black and white stripes: any scaling greys them.
        let (w, h) = (401u32, 301u32);
        let px: Vec<u8> = (0..w * h).flat_map(|i| if i % w % 2 == 0 { [0, 0, 0, 255] } else { [255; 4] }).collect();
        let file = dir.join("stripes.png");
        ok(write_png(&file, &px, (w, h)));
        ok(s.add_source("Main", "image_source", "Stripes", &json!({ "file": file.to_string_lossy() }), false));
        let mut shot = None;
        for _ in 0..40 {
            if let Ok(r) = s.screenshot_pixels("Stripes")
                && r.1 == (w, h)
            {
                shot = Some(r.0);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let shot = shot.unwrap_or_else(|| panic!("a screenshot at its own size, {w}x{h}"));
        let row = (h / 2 * w) as usize;
        let worst = (10..w as usize - 10)
            .map(|x| (i32::from(shot[(row + x) * 4]) - i32::from(shot[(row + x + 1) * 4])).abs())
            .min()
            .unwrap_or(0);
        assert!(worst > 250, "every stripe black or white, none greyed: {worst}");
        // Recorded on its own: even sides for the encoder, by leaving the last column out.
        let plan = RecordPlan {
            folder: dir.clone(),
            format: "mkv".into(),
            encode: crate::EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc"),
            ..RecordPlan::default()
        };
        let rec = ok(s.record(&plan, Some("Stripes")));
        std::thread::sleep(std::time::Duration::from_millis(800));
        assert!(rec.last_error().is_none(), "{:?}", rec.last_error());
        drop(rec);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_group_is_screenshot_on_its_own_at_its_box() {
        // Owner, 12 Nov: a group's Screenshot said the engine "can't show it on its own yet".
        let s = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        if s.engine().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrocast-groupshot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (w, h) = (121u32, 81u32);
        let px: Vec<u8> = (0..w * h).flat_map(|i| if i % w % 2 == 0 { [0, 0, 0, 255] } else { [255; 4] }).collect();
        let file = dir.join("stripes.png");
        ok(write_png(&file, &px, (w, h)));
        let item =
            ok(s.add_source("Main", "image_source", "Stripes", &json!({ "file": file.to_string_lossy() }), false));
        // Grouped once the picture has its size, so the group's box is the picture's.
        for _ in 0..40 {
            if s.screenshot_pixels("Stripes").is_ok_and(|r| r.1 == (w, h)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        ok(s.group_together("Main", &[item.id], "Pair"));
        let mut shot = None;
        for _ in 0..40 {
            if let Ok(r) = s.screenshot_pixels("Pair") {
                shot = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let (px, size) = shot.unwrap_or_else(|| panic!("a screenshot of the group"));
        assert_eq!(size, (w, h), "the group's own box");
        let row = (h / 2 * w) as usize;
        let worst = (10..w as usize - 10)
            .map(|x| (i32::from(px[(row + x) * 4]) - i32::from(px[(row + x + 1) * 4])).abs())
            .min()
            .unwrap_or(0);
        assert!(worst > 250, "its sources drawn 1:1 inside it: {worst}");
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

    /// An OBS collection with scenes A and B, `current` as the default transition at `ms`, and the
    /// given custom transitions; scene A has the override `name` (at `dur`, if any).
    fn obs_with_override(current: &str, ms: u64, custom: Value, name: &str, dur: Option<u64>) -> Value {
        let mut ps = json!({ "transition": name });
        if let Some(d) = dur {
            ps["transition_duration"] = json!(d);
        }
        json!({
            "current_scene": "A",
            "current_transition": current,
            "transition_duration": ms,
            "transitions": custom,
            "sources": [
                { "id": "scene", "name": "A", "settings": { "items": [] }, "private_settings": ps },
                { "id": "scene", "name": "B", "settings": { "items": [] }, "private_settings": {} }
            ],
            "scene_order": [{ "name": "A" }, { "name": "B" }]
        })
    }

    fn rule_target(d: &Value, scene: &str) -> Option<NamedTransition> {
        let rules = transition_matrix_from_doc(d);
        let r = rules.iter().find(|r| r.from.is_none() && r.to.as_deref() == Some(scene))?;
        named_transitions_from_doc(d).into_iter().find(|t| t.name == r.transition)
    }

    #[test]
    fn undo_with_every_preview_candidate_gone_picks_a_scene_that_is_there() {
        let s = stage();
        let only_main = s.snapshot();
        ok(s.add_scene("B"));
        ok(s.set_current("B"));
        s.set_studio_mode(true);
        s.restore_all(&only_main);
        let preview = s.preview_scene().unwrap_or_else(|| panic!("a preview"));
        assert!(s.scenes().iter().any(|c| c.name == preview), "{preview} is a scene");
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
    }

    #[test]
    fn a_stopped_studio_and_a_running_one_read_the_same_matrix() {
        // BRB is a scene in `sources` but missing from `scene_order`; a rule targets it, and the
        // vertical canvas's scene is never a target.
        let doc = json!({
            "current_scene": "A",
            "sources": [
                { "id": "scene", "name": "A", "settings": { "items": [] } },
                { "id": "scene", "name": "BRB", "settings": { "items": [] } },
                { "id": "scene", "name": "V", "settings": { "items": [] } }
            ],
            "scene_order": [{ "name": "A" }],
            "ferrocast": {
                "vertical": { "scene_order": [{ "name": "V" }] },
                "transition_matrix": [
                    { "from": null, "to": "BRB", "transition": "Fade" },
                    { "from": null, "to": "V", "transition": "Fade" }
                ]
            }
        });
        let stopped = saved_transition_matrix(Some(&doc));
        let s = stage();
        s.restore_all(&doc);
        let running = s.transition_matrix();
        assert_eq!(stopped, running, "the same rules either way");
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].to.as_deref(), Some("BRB"));
    }

    #[test]
    fn an_obs_override_without_a_duration_is_300_ms_not_the_defaults() {
        // The default is Swipe at 750 ms; scene A names Swipe with no duration saved: OBS plays 300.
        let mut d = obs_with_override("Swipe", 750, json!([]), "Swipe", None);
        import_and_normalize_transitions(&mut d);
        let t = rule_target(&d, "A").unwrap_or_else(|| panic!("a rule for A"));
        assert_eq!((t.kind.as_str(), t.ms), ("swipe_transition", 300), "{t:?}");
        // Cut stays 0.
        let mut d = obs_with_override("Fade", 750, json!([]), "Cut", None);
        import_and_normalize_transitions(&mut d);
        assert_eq!(rule_target(&d, "A").map(|t| t.ms), Some(0));
    }

    #[test]
    fn an_obs_override_naming_a_removed_custom_transition_is_ignored() {
        let mut d =
            obs_with_override("Fade", 750, json!([{ "name": "Fade", "id": "fade_transition" }]), "Show Swipe", None);
        import_and_normalize_transitions(&mut d);
        assert!(transition_matrix_from_doc(&d).is_empty(), "OBS plays the default for it");
        assert!(named_transitions_from_doc(&d).iter().all(|t| t.name != "Show Swipe"), "no preset made up for it");
        // A built-in name OBS doesn't save is still understood.
        let mut d = obs_with_override("Fade", 300, json!([]), "Slide", Some(500));
        import_and_normalize_transitions(&mut d);
        assert_eq!(rule_target(&d, "A").map(|t| t.kind), Some("slide_transition".to_string()));
    }

    #[test]
    fn an_obs_override_is_never_bound_to_an_unrelated_preset_of_the_same_name() {
        // The collection already has a Cut preset called "Swipe (700ms)".
        let custom = json!([
            { "name": "Fade", "id": "fade_transition" },
            { "name": "Swipe (700ms)", "id": "cut_transition" }
        ]);
        let mut d = obs_with_override("Fade", 300, custom, "Swipe", Some(700));
        import_and_normalize_transitions(&mut d);
        let t = rule_target(&d, "A").unwrap_or_else(|| panic!("a rule for A"));
        assert_eq!((t.kind.as_str(), t.ms), ("swipe_transition", 700), "plays Swipe, not the Cut: {t:?}");
        assert_ne!(t.name, "Swipe (700ms)");
        // The one that was there is untouched.
        let cut = named_transitions_from_doc(&d).into_iter().find(|t| t.name == "Swipe (700ms)");
        assert_eq!(cut.map(|t| t.kind), Some("cut_transition".to_string()));
    }

    #[test]
    fn a_kinds_settings_are_written_to_the_preset_they_are_read_from() {
        let s = stage();
        // Fade is the default; a named Swipe preset (not the default) holds Swipe's settings.
        ok(s.add_named_transition(&NamedTransition {
            name: "Show Swipe".into(),
            kind: "swipe_transition".into(),
            ms: 700,
            settings: json!({ "direction": "left" }),
            default: false,
        }));
        assert_eq!(s.transition_settings("swipe_transition")["direction"], "left");
        ok(s.set_transition_settings("swipe_transition", &json!({ "direction": "right" })));
        assert_eq!(s.transition_settings("swipe_transition")["direction"], "right", "read back what was written");
        let all = s.named_transitions();
        let swipe = all.iter().find(|t| t.name == "Show Swipe").unwrap_or_else(|| panic!("the preset"));
        assert_eq!(swipe.settings["direction"], "right");
        let fade = all.iter().find(|t| t.kind == "fade_transition").unwrap_or_else(|| panic!("fade"));
        assert!(fade.settings.get("direction").is_none(), "other presets are left alone: {fade:?}");
    }

    #[test]
    fn undo_in_studio_mode_keeps_a_preview_and_the_sources_on_air() {
        let s = stage();
        s.set_studio_mode(true);
        // Undo of a scene that was the preview: the preview goes back to one that exists, and the
        // Transition button still works (CodeRabbit: it pointed at a scene that wasn't there).
        let before_scene = s.snapshot();
        ok(s.add_scene("B"));
        ok(s.set_preview("B"));
        s.restore_all(&before_scene);
        let preview = s.preview_scene().unwrap_or_else(|| panic!("a preview"));
        assert!(s.scenes().iter().any(|c| c.name == preview), "the preview {preview} is a scene");
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        // A source added and sent on air, then its adding undone: the copy on air still has its
        // source (the engine would get an item with none).
        let before_source = s.snapshot();
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "width": 10, "height": 10 }), false));
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        s.restore_all(&before_source);
        let doc = s.snapshot();
        let items = doc.pointer("/ferrocast/program_copy/items").and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(items.iter().any(|i| name_of(i) == "Red"), "the copy on air still shows Red: {items:?}");
        let sources = doc["sources"].as_array().cloned().unwrap_or_default();
        assert!(sources.iter().any(|x| name_of(x) == "Red"), "and its definition is kept");
        // A copy of a source added after the snapshot: the source it shows stays too.
        let before_pair = s.snapshot();
        ok(s.add_source("Main", "color_source_v3", "Blue", &json!({ "width": 10, "height": 10 }), false));
        ok(s.add_source("Main", "ferrocast_clone", "Echo", &json!({ "clone": "Blue" }), false));
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        s.restore_all(&before_pair);
        let sources = s.snapshot()["sources"].as_array().cloned().unwrap_or_default();
        for n in ["Echo", "Blue"] {
            assert!(sources.iter().any(|x| name_of(x) == n), "{n} is kept for the copy on air");
        }
        // The transitions after replace the copy (the old one is kept until the next), and the
        // source nobody uses goes.
        for _ in 0..2 {
            ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        }
        let sources = s.snapshot()["sources"].as_array().cloned().unwrap_or_default();
        assert!(!sources.iter().any(|x| name_of(x) == "Red"), "gone once it isn't on air");
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
    fn a_source_moved_into_a_group_stays_where_it_shows_and_comes_out_again() {
        // Owner's recording, 11 Nov: an image put into a new empty group vanished from the
        // canvas and from the list (the group had no size, and its sources weren't listed).
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 50, "height": 50 }), false));
        ok(s.set_item_box("Main", a.id, 100.0, 100.0, 100.0, 100.0));
        ok(s.set_item_box("Main", b.id, 300.0, 150.0, 50.0, 50.0));
        let g = ok(s.add_group("Main", "Box"));
        ok(s.set_group("Main", a.id, Some(g.id)));
        let items = ok(s.items("Main"));
        let group = items.iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        assert_eq!(group.children.len(), 1, "listed inside its group");
        let inside = &group.children[0];
        assert_eq!(inside.frame, [100.0, 0.0, 0.0, 100.0, 100.0, 100.0], "where it was on the canvas");
        assert_eq!(group.frame, [100.0, 0.0, 0.0, 100.0, 100.0, 100.0], "the group fits it");
        assert_ne!(inside.id, b.id, "its own id, not one of the scene's");
        // A second one: the group grows round both, and neither moves.
        ok(s.set_group("Main", b.id, Some(g.id)));
        let group = ok(s.items("Main")).into_iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        let frames: Vec<[f32; 6]> = group.children.iter().map(|c| c.frame).collect();
        assert!(frames.contains(&[50.0, 0.0, 0.0, 50.0, 300.0, 150.0]), "{frames:?}");
        assert_eq!(group.frame, [250.0, 0.0, 0.0, 100.0, 100.0, 100.0]);
        // A video's sound stays in the scene's mixer once it's in the group.
        let music = ok(s.add_source("Main", "ffmpeg_source", "Music", &json!({ "local_file": "/no/such.mp4" }), false));
        assert!(ok(s.audio("Main")).iter().any(|a| a.name == "Music"));
        ok(s.set_group("Main", music.id, Some(g.id)));
        assert!(ok(s.audio("Main")).iter().any(|a| a.name == "Music"), "a grouped video is still listed");
        // Edits reach a source inside: hidden, then dragged on the canvas.
        let b_in = group.children.iter().find(|c| c.name == "B").map(|c| c.id).unwrap_or_default();
        ok(s.edit_item("Main", b_in, ItemEdit::Hide));
        ok(s.set_item_box("Main", b_in, 400.0, 200.0, 50.0, 50.0));
        let group = ok(s.items("Main")).into_iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        let b_now = group.children.iter().find(|c| c.name == "B").unwrap_or_else(|| panic!("B left"));
        assert!(!b_now.visible);
        assert_eq!(b_now.frame, [50.0, 0.0, 0.0, 50.0, 400.0, 200.0], "dragged in canvas pixels");
        // Out again, still where it shows, just above its group.
        ok(s.set_group("Main", b_in, None));
        let items = ok(s.items("Main"));
        let out = items.iter().find(|i| i.name == "B").unwrap_or_else(|| panic!("B isn't back"));
        assert_eq!(out.frame, [50.0, 0.0, 0.0, 50.0, 400.0, 200.0]);
        assert_eq!(items.iter().position(|i| i.name == "B"), Some(0), "above the group");
        assert_eq!(items.iter().find(|i| i.group).map(|g| g.children.len()), Some(2), "A and Music stay");
        // Our engine reads it all.
        let got = ferrocore::obs::import(&s.save("x"), (1920, 1080));
        assert!(got.skipped.is_empty(), "{:?}", got.skipped);
    }

    #[test]
    fn edits_on_a_source_inside_a_group_work_as_on_the_canvas() {
        // Owner's "check more casualties", 11 Nov: Duplicate, Center, Fit, Duplicate scene and
        // Screenshot on sources inside groups.
        let s = stage();
        let (cw, ch) = (s.canvas().width as f32, s.canvas().height as f32);
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 400, "height": 300 }), false));
        ok(s.set_item_box("Main", a.id, 100.0, 100.0, 400.0, 300.0));
        let g = ok(s.group_together("Main", &[a.id], "Box"));
        let mut t = ok(s.transform("Main", g.id));
        (t.scale_x, t.scale_y) = (0.5, 0.5);
        ok(s.set_transform("Main", g.id, &t));
        let inside = |s: &Stage, n: &str| {
            ok(s.items("Main"))
                .into_iter()
                .flat_map(|i| i.children)
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("{n} isn't in the group"))
        };
        let a_in = inside(&s, "A").id;
        // Center: the middle of the canvas, through a half-size group.
        ok(s.edit_item("Main", a_in, ItemEdit::Center));
        let f = inside(&s, "A").frame;
        let (mx, my) = (f[4] + (f[0] + f[2]) / 2.0, f[5] + (f[1] + f[3]) / 2.0);
        assert!((mx - cw / 2.0).abs() < 1.0 && (my - ch / 2.0).abs() < 1.0, "centred: {mx}, {my}");
        // Fit: the canvas's box on the canvas.
        ok(s.edit_item("Main", a_in, ItemEdit::Stretch));
        let f = inside(&s, "A").frame;
        assert!((f[0] - cw).abs() < 1.0 && (f[3] - ch).abs() < 1.0 && f[4].abs() < 1.0 && f[5].abs() < 1.0, "{f:?}");
        // Duplicate: the copy is in the same group, above it, where it is.
        ok(s.edit_item("Main", a_in, ItemEdit::Duplicate));
        let group = ok(s.items("Main")).into_iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        assert_eq!(group.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["A 2", "A"]);
        assert_eq!(group.children[0].frame, group.children[1].frame);
        // Duplicate scene: the copy's group is its own.
        ok(s.duplicate_scene("Main", "Copy"));
        let copy_group = ok(s.items("Copy")).into_iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        assert_ne!(copy_group.name, "Box", "a group of its own");
        assert_eq!(copy_group.children.len(), 2, "holding the same sources");
        let a_copy = copy_group.children.iter().find(|c| c.name == "A").map(|c| c.id).unwrap_or_default();
        ok(s.edit_item("Copy", a_copy, ItemEdit::Hide));
        assert!(inside(&s, "A").visible, "hiding it in the copy leaves Main's alone");
        // Screenshot of a source on its own (any source, inside a group or not).
        if s.engine().is_ok() {
            let (_, size) = ok(s.screenshot_pixels("A"));
            assert_eq!(size, (400, 300), "at its own size");
        }
    }

    #[test]
    fn group_moves_keep_shapes_and_copies_keep_ids_apart() {
        // CodeRabbit, 11 Nov: a group scaled more one way changed a source's shape going in and
        // out; a copied group's sources shared ids with the original's.
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let b = ok(s.add_source("Main", "color_source_v3", "B", &json!({ "width": 100, "height": 50 }), false));
        ok(s.set_item_box("Main", a.id, 0.0, 0.0, 100.0, 100.0));
        ok(s.set_item_box("Main", b.id, 300.0, 300.0, 100.0, 50.0));
        let g = ok(s.group_together("Main", &[a.id], "Wide"));
        let mut t = ok(s.transform("Main", g.id));
        (t.scale_x, t.scale_y) = (2.0, 1.0);
        ok(s.set_transform("Main", g.id, &t));
        let before = ok(s.items("Main")).into_iter().find(|i| i.name == "B").map(|i| i.frame);
        ok(s.set_group("Main", b.id, Some(g.id)));
        let group = ok(s.items("Main")).into_iter().find(|i| i.group).unwrap_or_else(|| panic!("no group"));
        let inside = group.children.iter().find(|c| c.name == "B").map(|c| c.frame);
        let close = |x: [f32; 6], y: [f32; 6]| x.iter().zip(y).all(|(p, q)| (p - q).abs() < 0.5);
        assert!(close(inside.unwrap_or_default(), before.unwrap_or_default()), "{inside:?} vs {before:?}");
        // Out again: the same shape and place.
        let b_in = group.children.iter().find(|c| c.name == "B").map(|c| c.id).unwrap_or_default();
        ok(s.set_group("Main", b_in, None));
        let out = ok(s.items("Main")).into_iter().find(|i| i.name == "B").map(|i| i.frame);
        assert!(close(out.unwrap_or_default(), before.unwrap_or_default()), "{out:?} vs {before:?}");
        // A copy of the group: its sources' ids are its own.
        ok(s.edit_item("Main", g.id, ItemEdit::Duplicate));
        let ids: Vec<i64> = ok(s.items("Main"))
            .into_iter()
            .flat_map(|i| std::iter::once(i.id).chain(i.children.into_iter().map(|c| c.id)))
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "every id once: {ids:?}");
    }

    #[test]
    fn every_source_moved_into_a_group_is_drawn() {
        // Owner's check, 11 Nov: the second source put into a group vanished from the picture.
        let s = stage();
        let red = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Red",
            &json!({ "width": 400, "height": 300, "color": 0xFF00_00FFu32 }),
            false,
        ));
        let green = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Green",
            &json!({ "width": 300, "height": 300, "color": 0xFF00_FF00u32 }),
            false,
        ));
        ok(s.set_item_box("Main", red.id, 200.0, 200.0, 400.0, 300.0));
        ok(s.set_item_box("Main", green.id, 900.0, 300.0, 300.0, 300.0));
        let g = ok(s.add_group("Main", "Box"));
        ok(s.set_group("Main", red.id, Some(g.id)));
        ok(s.set_group("Main", green.id, Some(g.id)));
        if s.engine().is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (px, (w, _)) = s.screenshot_program_pixels().unwrap_or_else(|e| panic!("{e}"));
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2]]
        };
        assert_eq!(at(400, 350), [255, 0, 0], "red");
        assert_eq!(at(1050, 450), [0, 255, 0], "green");
    }

    #[test]
    fn a_group_fits_its_sources_after_one_is_resized_even_scaled() {
        // Owner, 11 Nov: after resizing an image in a group, the group's frame was too small
        // and a fitted image in a scaled group showed zoomed in.
        let s = stage();
        let a = ok(s.add_source("Main", "color_source_v3", "A", &json!({ "width": 100, "height": 100 }), false));
        let g = ok(s.group_together("Main", &[a.id], "Box"));
        // The group made half size, then the source inside fitted to a 300×200 box on the canvas.
        let mut t = ok(s.transform("Main", g.id));
        (t.scale_x, t.scale_y) = (0.5, 0.5);
        ok(s.set_transform("Main", g.id, &t));
        let a_in = ok(s.items("Main"))[0].children[0].id;
        let mut ta = ok(s.transform("Main", a_in));
        (ta.bounds, ta.bounds_width, ta.bounds_height) = ("stretch".into(), 400.0, 400.0);
        ok(s.set_transform("Main", a_in, &ta));
        let group = ok(s.items("Main")).into_iter().next().unwrap_or_else(|| panic!("no group"));
        let kid = group.children[0].frame;
        let gf = group.frame;
        // The group's frame covers its source on the canvas.
        assert!((gf[0] - kid[0]).abs() < 1.0 && (gf[3] - kid[3]).abs() < 1.0, "group {gf:?} source {kid:?}");
        assert_eq!(kid[0].round(), 200.0, "a 400 px box in a half-size group is 200 px on the canvas");
        // Out of the group: the same size on the canvas, its box now in canvas pixels.
        ok(s.set_group("Main", a_in, None));
        let out = ok(s.items("Main")).into_iter().find(|i| i.name == "A").unwrap_or_else(|| panic!("A isn't back"));
        assert_eq!(out.frame[0].round(), 200.0, "{:?}", out.frame);
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

    /// A capture card added again (as a Camera, or in another scene) is the same source: the
    /// device is never opened twice (17 Nov: both copies stopped and the card wouldn't reopen).
    #[test]
    fn a_camera_already_used_is_the_same_source_again() {
        let s = stage();
        ok(s.add_scene("Other"));
        let dev = json!({ "device": "no-such-card" });
        ok(s.add_source("Main", "av_capture_input_v2", "Card", &dev, false));
        // Again as a Camera in another scene: "Card" there too, one source.
        let again = ok(s.add_source("Other", "macos-avcapture", "Camera", &dev, false));
        assert_eq!(again.name, "Card");
        assert_eq!(s.sources().iter().filter(|x| x.kind.contains("capture")).count(), 1);
        // In the same scene twice: said, not added.
        assert!(s.add_source("Main", "av_capture_input_v2", "Card 2", &dev, false).is_err());
        // Another device is a source of its own.
        ok(s.add_source("Main", "av_capture_input_v2", "Phone", &json!({ "device": "phone" }), false));
        assert_eq!(s.sources().len(), 2);
        // The add says when it reused one (by the flag, not the name).
        let (_, reused) =
            ok(s.add_source_or_reuse("Other", "av_capture_input_v2", "Phone", &json!({ "device": "phone" }), false));
        assert!(reused, "the same name, still a reuse");
        assert_eq!(s.sources().len(), 2);
        // A copy (pasted or duplicated) is a source of its own: the engine shows it as a copy of
        // the first one's picture, never opening the device twice (ferrocore::obs's import).
        ok(s.paste_copy("Main", "Card"));
        let card = ok(s.items("Main")).into_iter().find(|i| i.name == "Card").map(|i| i.id).unwrap_or_default();
        ok(s.edit_item("Main", card, ItemEdit::Duplicate));
        assert_eq!(s.sources().len(), 4);
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
    fn set_current_switches_an_active_vertical_scene_leaving_main_alone() {
        let s = stage();
        let main_before = s.current();
        ok(s.add_scene("Main 2"));
        ok(s.start_vertical("Tall"));
        ok(s.add_vertical_scene("Tall 2"));
        ok(s.set_current("Tall 2"));
        assert_eq!(s.current(), main_before);
        let v = s.vertical().unwrap_or_else(|| panic!("vertical should be on"));
        assert_eq!(v.current, "Tall 2");
        s.stop_vertical();
        assert!(s.set_current("Tall").is_err(), "turned off vertical doesn't switch via set_current");
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

    /// The live watch judges only microphones: a capture card's sound (the owner's "USB2 Digital
    /// Audio", even as a Mic/Aux channel), the desktop's and an app's are not, unless the owner
    /// says so ("This is a mic"), and a real mic can be said not to be.
    #[test]
    fn only_microphones_are_judged_unless_the_owner_says_otherwise() {
        let s = stage();
        ok(s.set_channel(3, Some(("coreaudio_input_capture", "KREO KAST", &json!({})))));
        ok(s.set_channel(4, Some(("coreaudio_input_capture", "USB2 Digital Audio", &json!({})))));
        ok(s.set_channel(1, Some(("coreaudio_output_capture", "Desktop Audio", &json!({})))));
        ok(s.add_source(
            "Main",
            "coreaudio_input_capture",
            "Line",
            &json!({ "device_id": "AppleUSBAudioEngine:MACROSILICON:USB2 Video:15930196:4,3" }),
            false,
        ));
        ok(s.add_source("Main", "sck_audio_capture", "Spotify", &json!({}), false));
        let mic = |n: &str| ok(s.audio_of(n)).mic;
        assert!(mic("KREO KAST"));
        assert!(!mic("USB2 Digital Audio"), "a capture card by its name");
        assert!(!mic("Desktop Audio"));
        assert!(!mic("Line"), "a capture card by its device id");
        assert!(!mic("Spotify"), "an app's sound");
        // The owner's word wins, both ways, and can be taken back.
        ok(s.set_mic("USB2 Digital Audio", Some(true)));
        ok(s.set_mic("KREO KAST", Some(false)));
        assert!(mic("USB2 Digital Audio") && !mic("KREO KAST"));
        ok(s.set_mic("USB2 Digital Audio", None));
        assert!(!mic("USB2 Digital Audio"));
        // Kept in the collection.
        ok(s.set_mic("Line", Some(true)));
        let again = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &s.save("x"), "Scene"));
        assert!(ok(again.audio_of("Line")).mic);
    }

    /// A mic inside a scene shown inside this one is heard (the engine mixes it, OBS lists it) and
    /// is in the list; hidden anywhere above it, it is listed but not `shown`.
    #[test]
    fn a_mic_in_a_nested_scene_is_listed_and_a_hidden_one_is_not_shown() {
        let s = stage();
        ok(s.add_scene("Audio"));
        ok(s.add_source("Audio", "coreaudio_input_capture", "Mic", &json!({}), false));
        ok(s.add_source("Main", "ffmpeg_source", "Game", &json!({ "local_file": "/no/such.mp4" }), false));
        let nested = ok(s.add_existing("Main", "Audio"));
        let rows = || ok(s.audio("Main"));
        let mic = || rows().into_iter().find(|a| a.name == "Mic").unwrap_or_else(|| panic!("no Mic row"));
        assert!(mic().shown && mic().mic, "the nested mic is there, on air");
        assert!(rows().iter().any(|a| a.name == "Game" && a.shown));
        // The nested scene hidden: its mic is not heard.
        ok(s.edit_item("Main", nested.id, ItemEdit::Hide));
        assert!(!mic().shown);
        // A hidden item of its own is not shown either.
        let own = ok(s.add_source("Main", "coreaudio_input_capture", "Desk", &json!({}), false));
        ok(s.edit_item("Main", own.id, ItemEdit::Hide));
        assert!(!ok(s.audio("Main")).iter().find(|a| a.name == "Desk").unwrap_or_else(|| panic!("no Desk")).shown);
        // A scene inside itself doesn't loop.
        ok(s.add_existing("Audio", "Main"));
        assert!(!ok(s.audio("Main")).is_empty());
    }

    /// A sound placed twice is on air when any of its placements is: a hidden scene above that
    /// holds the mic too doesn't make the visible one read as hidden (the first one seen won).
    #[test]
    fn a_sound_is_shown_when_any_of_its_placements_is() {
        let s = stage();
        ok(s.add_source("Main", "coreaudio_input_capture", "Mic", &json!({}), false));
        ok(s.add_scene("Audio"));
        ok(s.add_existing("Audio", "Mic"));
        let nested = ok(s.add_existing("Main", "Audio"));
        ok(s.edit_item("Main", nested.id, ItemEdit::Hide));
        let rows = ok(s.audio("Main"));
        let mics: Vec<_> = rows.iter().filter(|a| a.name == "Mic").collect();
        assert_eq!(mics.len(), 1, "listed once");
        assert!(mics[0].shown, "on air through its own visible item");
    }

    /// Push-to-talk and push-to-mute are kept in the sound's own settings (OBS's keys), clamped to
    /// 5 s, and a key for a sound without them on answers false (the old mute shortcut then runs).
    #[test]
    fn a_sounds_push_to_talk_and_push_to_mute_are_kept_and_asked() {
        let s = stage();
        ok(s.set_channel(3, Some(("coreaudio_input_capture", "Mic/Aux", &json!({})))));
        let a = ok(s.audio_of("Mic/Aux"));
        assert!(!a.push_to_talk && !a.push_to_mute && a.push_to_talk_ms == 0);
        assert!(!ok(s.push_key("Mic/Aux", true, true)), "not switched on: the caller mutes as before");
        ok(s.set_push("Mic/Aux", true, Some(true), Some(250)));
        ok(s.set_push("Mic/Aux", false, Some(true), Some(99_999)));
        let a = ok(s.audio_of("Mic/Aux"));
        assert_eq!((a.push_to_talk, a.push_to_talk_ms), (true, 250));
        assert_eq!((a.push_to_mute, a.push_to_mute_ms), (true, 5_000), "at most 5 s");
        assert!(ok(s.push_key("Mic/Aux", true, true)));
        // Only the delay changes when only it is sent.
        ok(s.set_push("Mic/Aux", true, None, Some(100)));
        assert_eq!(ok(s.audio_of("Mic/Aux")).push_to_talk_ms, 100);
        assert!(ok(s.audio_of("Mic/Aux")).push_to_talk);
        // And they survive a save and a load.
        let again = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &s.save("x"), "Scene"));
        assert_eq!(ok(again.audio_of("Mic/Aux")).push_to_talk_ms, 100);
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
    fn switching_scene_clears_fade_to_black() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let Ok(e) = s.engine() else { return };
        ok(s.add_source("Main", "color_source_v3", "Red", &json!({ "color": 4278190335u32 }), false));
        ok(s.add_scene("Blue"));
        ok(s.add_source("Blue", "color_source_v3", "BlueSrc", &json!({ "color": 4294901760u32 }), false));
        ok(s.set_transition(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }));
        let px = || {
            e.engine.snapshot().map(|p| p[(90 * 320 + 160) * 4..(90 * 320 + 160) * 4 + 3].to_vec()).unwrap_or_default()
        };
        assert!(ok(s.fade_to_black()) && s.is_black());
        std::thread::sleep(std::time::Duration::from_millis(450));
        assert_eq!(px(), [0, 0, 0]);
        // Switching to another scene clears `seen.black` so edits don't restore black.
        ok(s.set_current("Blue"));
        assert!(!s.is_black(), "go_with cleared seen.black");
        ok(s.add_scene("Extra"));
        assert_eq!(px(), [0, 0, 255], "stays on Blue after edit");
        // Fading to black and re-selecting the current scene also brings it back.
        assert!(ok(s.fade_to_black()) && s.is_black());
        std::thread::sleep(std::time::Duration::from_millis(450));
        assert_eq!(px(), [0, 0, 0]);
        ok(s.set_current("Blue"));
        assert!(!s.is_black(), "set_current on same scene cleared seen.black");
        assert_eq!(px(), [0, 0, 255], "restored Blue");
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

    /// A second holder makes the solo scene (`show_alone`) and, an instant before it counts itself
    /// (`keep_alone`), the first holder lets go and removes it: the second must not be left
    /// holding a scene that is gone.
    #[test]
    fn a_holder_that_joins_as_the_last_one_leaves_still_has_its_scene() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let Ok(e) = s.engine() else { return };
        let _ = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Card",
            &json!({ "color": 4278190335u32, "width": 160, "height": 90 }),
            false,
        ));
        let (scene, _, _) = ok(own::show_alone(&e.engine, "Card", (320, 180)));
        e.keep_alone("Card"); // the first holder
        // The second holder's `show_alone` found the scene there; now the first one closes.
        let (again, _, _) = ok(own::show_alone(&e.engine, "Card", (320, 180)));
        assert_eq!(again, scene);
        e.release_alone("Card");
        assert!(e.engine.scene(&scene).is_none(), "the last holder took it away");
        // The second holder counts itself: its scene must be there.
        e.keep_alone("Card");
        assert!(e.engine.scene(&scene).is_some(), "a holder always has its solo scene");
        e.release_alone("Card");
        assert!(e.engine.scene(&scene).is_none());
    }

    #[test]
    fn solo_scenes_are_refcounted_and_removed_on_last_release() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        let Ok(e) = s.engine() else { return };
        let _ = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Card",
            &json!({ "color": 4278190335u32, "width": 160, "height": 90 }),
            false,
        ));
        let base_scenes = e.engine.scene_names().len();
        for _ in 0..1_000 {
            let (scene, _, _) = ok(own::show_alone(&e.engine, "Card", (320, 180)));
            e.keep_alone("Card");
            e.keep_alone("Card");
            assert_eq!(e.alone_count(), 1);
            assert!(e.engine.scene(&scene).is_some());
            e.release_alone("Card");
            assert_eq!(e.alone_count(), 1, "still held by second owner");
            assert!(e.engine.scene(&scene).is_some());
            e.release_alone("Card");
            assert_eq!(e.alone_count(), 0);
            assert!(e.engine.scene(&scene).is_none(), "hidden solo scene removed at refcount 0");
        }
        assert_eq!(e.engine.scene_names().len(), base_scenes);
    }

    /// "Duck under" lists what can key: a source with no sound track (mixers 0) can't, and a
    /// picture-only one never; a source whose sound is turned off again leaves the list.
    #[test]
    fn duck_under_lists_only_sources_that_have_sound() {
        let s = stage();
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Music", &json!({ "local_file": "/a.mp3" }), false));
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Voice", &json!({ "local_file": "/v.mp3" }), false));
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Silent clip", &json!({ "local_file": "/s.mp4" }), false));
        ok(s.set_tracks("Silent clip", &[]));
        ok(s.add_filter("Music", "compressor_filter", "Duck", &json!({})));
        let listed = |s: &Stage| {
            let opts = ok(s.filter_options("Music", "Duck"));
            opts.iter().find(|o| o.key == "sidechain_source").map(|o| o.choices.clone()).unwrap_or_default()
        };
        assert_eq!(listed(&s), vec!["None", "Voice"], "no sound track, no key");
        ok(s.set_tracks("Silent clip", &[1]));
        assert_eq!(listed(&s), vec!["None", "Voice", "Silent clip"]);
    }

    /// A filter added or pasted through an item takes a name free where it is written: a sound
    /// filter goes to the source's own list, which can hold a picture filter of that name the
    /// item's own list doesn't show (its copy removed). Two of a name there: a change by name
    /// edited the first, a removal took both.
    #[test]
    fn a_filter_added_through_an_item_takes_a_name_free_where_it_is_written() {
        let s = stage();
        let it = ok(s.add_source("Main", "color_source_v3", "Card", &json!({}), false));
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Clip", &json!({ "local_file": "/c.mp4" }), false));
        ok(s.add_filter("Card", "crop_filter", "Crop", &json!({})));
        ok(s.set_item_filter_scope("Main", it.id, true));
        ok(s.remove_item_filter("Main", it.id, "Crop"));
        assert!(
            s.add_item_filter("Main", it.id, "gain_filter", "Crop", &json!({})).is_err(),
            "taken in the source's list"
        );
        // Pasted the same way: renamed, never two of a name in the list it lands in.
        ok(s.add_filter("Clip", "gain_filter", "Crop", &json!({})));
        ok(s.paste_item_filters("Main", it.id, "Clip"));
        let names: Vec<String> = ok(s.filter_list("Card")).iter().map(|f| name_of(f).to_string()).collect();
        let crops = names.iter().filter(|n| *n == "Crop").count();
        assert_eq!(crops, 1, "one \"Crop\" in the source's list: {names:?}");
    }

    /// A source called "none" renamed leaves every compressor's "none" (no ducking, the default)
    /// alone: it turned all of them into ducking under the renamed source.
    #[test]
    fn renaming_a_source_called_none_leaves_no_ducking_alone() {
        let s = stage();
        let _ = ok(s.add_source("Main", "ffmpeg_source", "none", &json!({ "local_file": "/n.mp3" }), false));
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Music", &json!({ "local_file": "/a.mp3" }), false));
        ok(s.add_filter("Music", "compressor_filter", "Duck", &json!({})));
        assert_eq!(ok(s.filter_settings("Music", "Duck"))["sidechain_source"], json!("none"));
        ok(s.rename("none", "Voice"));
        assert_eq!(ok(s.filter_settings("Music", "Duck"))["sidechain_source"], json!("none"), "still no ducking");
    }

    #[test]
    fn compressor_filter_options_list_other_audio_sources_and_follow_renames() {
        let s = stage();
        ok(s.set_channel(3, Some(("coreaudio_input_capture", "Mic", &json!({ "device_id": "default" })))));
        let _ = ok(s.add_source("Main", "ffmpeg_source", "Music", &json!({ "local_file": "/a.mp3" }), false));
        let _ = ok(s.add_source("Main", "color_source_v3", "Back", &json!({}), false));
        ok(s.add_filter("Music", "compressor_filter", "Duck", &json!({})));

        let opts = ok(s.filter_options("Music", "Duck"));
        let sc = opts.iter().find(|o| o.key == "sidechain_source").expect("sidechain_source option");
        assert_eq!(sc.label, "Duck under");
        assert_eq!(sc.choices, vec!["None", "Mic"]);
        assert_eq!(sc.values, vec![json!("none"), json!("Mic")]);
        assert!(!sc.help.is_empty(), "Duck under carries (i) help text");

        // Point Music's compressor at Mic, then rename Mic -> Voice: reference follows.
        ok(s.update_filter("Music", "Duck", &json!({ "sidechain_source": "Mic" })));
        ok(s.rename("Mic", "Voice"));
        assert_eq!(ok(s.filter_settings("Music", "Duck"))["sidechain_source"], json!("Voice"));
        assert!(s.sidechain_cycles().is_empty());

        // Creating a mutual loop Voice -> Music is surfaced by sidechain_cycles().
        ok(s.add_filter("Voice", "compressor_filter", "DuckBack", &json!({ "sidechain_source": "Music" })));
        assert_eq!(s.sidechain_cycles(), vec![("Voice".into(), "Music".into())]);
    }

    #[test]
    fn adding_filtered_source_to_vertical_or_another_scene_copies_video_filters_only() {
        let s = stage();
        ok(s.start_vertical("Vertical"));
        ok(s.add_scene("Other"));
        let dev = json!({ "device": "cam-1" });
        let main_cam = ok(s.add_source("Main", "av_capture_input_v2", "Cam", &dev, false));
        assert!(!main_cam.own_filters);
        ok(s.add_filter("Cam", "ferrocast_rounded_corners", "Round", &json!({ "radius": 12 })));
        ok(s.add_filter("Cam", "gain_filter", "Boost", &json!({ "db": -3.0 })));

        // Reusing an already-filtered camera onto Vertical detaches its video filters automatically.
        let (vert_cam, reused) = ok(s.add_source_or_reuse("Vertical", "av_capture_input_v2", "Cam V", &dev, true));
        assert!(reused);
        assert!(vert_cam.own_filters, "Vertical gets its own copy of existing video filters");
        let vert_flts = ok(s.item_filters("Vertical", vert_cam.id));
        assert_eq!(vert_flts.len(), 2);
        assert_eq!(vert_flts[0].name, "Round");
        assert_eq!(vert_flts[0].scope.as_deref(), Some("item"));
        assert_eq!(vert_flts[1].name, "Boost");
        assert_eq!(vert_flts[1].scope.as_deref(), Some("source"));

        // Editing Vertical's Round filter & adding Tint leaves Main/Cam untouched.
        ok(s.update_item_filter("Vertical", vert_cam.id, "Round", &json!({ "radius": 4 })));
        ok(s.add_item_filter("Vertical", vert_cam.id, "ferrocast_border", "Edge", &json!({ "width": 6 })));
        assert_eq!(ok(s.item_filter_settings("Vertical", vert_cam.id, "Round"))["radius"], 4);
        assert_eq!(ok(s.filter_settings("Cam", "Round"))["radius"], 12);
        assert_eq!(ok(s.filters("Cam")).len(), 2);

        // Adding Cam to another normal scene also copies Cam's current video filters cleanly.
        let other_cam = ok(s.add_existing("Other", "Cam"));
        assert!(other_cam.own_filters);
        let other_flts = ok(s.item_filters("Other", other_cam.id));
        assert_eq!(other_flts.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Round", "Boost"]);
        assert_eq!(ok(s.item_filter_settings("Other", other_cam.id, "Round"))["radius"], 12);

        // Sources without video filters (even with audio filters) stay shared by default.
        ok(s.add_source("Main", "coreaudio_input_capture", "Mic", &json!({}), false));
        ok(s.add_filter("Mic", "gain_filter", "GateGain", &json!({ "db": 2.0 })));
        let vert_mic = ok(s.add_existing("Vertical", "Mic"));
        assert!(!vert_mic.own_filters);
    }

    #[test]
    fn a_source_filter_switch_reaches_item_copies_of_the_same_filter() {
        let s = stage();
        let dev = json!({ "device": "cam-1" });
        ok(s.add_source("Main", "av_capture_input_v2", "Cam", &dev, false));
        ok(s.add_filter("Cam", "ferrocast_rounded_corners", "Round", &json!({ "radius": 12 })));
        ok(s.add_scene("Other"));
        let other = ok(s.add_existing("Other", "Cam"));
        assert!(other.own_filters);
        // A different kind under the same name on another item is left alone.
        ok(s.add_scene("Third"));
        let third = ok(s.add_existing("Third", "Cam"));
        ok(s.remove_item_filter("Third", third.id, "Round"));
        ok(s.add_item_filter("Third", third.id, "ferrocast_border", "Round", &json!({ "width": 3 })));
        ok(s.set_filter_enabled("Cam", "Round", false));
        assert!(!ok(s.filters("Cam"))[0].enabled);
        assert!(!ok(s.item_filters("Other", other.id))[0].enabled, "the copy follows the source-level switch");
        assert!(ok(s.item_filters("Third", third.id))[0].enabled, "another kind is not touched");
        ok(s.set_filter_enabled("Cam", "Round", true));
        assert!(ok(s.item_filters("Other", other.id))[0].enabled);
    }

    #[test]
    fn item_filter_scope_switches_cleanly_while_audio_stays_shared() {
        let s = stage();
        ok(s.add_scene("Two"));
        let one = ok(s.add_source("Main", "ffmpeg_source", "Clip", &json!({ "local_file": "/a.mp4" }), false));
        let two = ok(s.add_existing("Two", "Clip"));
        assert!(!one.own_filters && !two.own_filters);

        ok(s.add_filter("Clip", "ferrocast_rounded_corners", "Round", &json!({ "radius": 20 })));
        assert_eq!(ok(s.item_filters("Two", two.id)).len(), 1);

        // Detach Two's item, delete Round on Two, add Border on Two; audio filter stays shared.
        let detached = ok(s.set_item_filter_scope("Two", two.id, true));
        assert!(detached.own_filters);
        ok(s.remove_item_filter("Two", two.id, "Round"));
        ok(s.add_item_filter("Two", two.id, "ferrocast_border", "Edge", &json!({ "width": 5 })));
        ok(s.add_item_filter("Two", two.id, "gain_filter", "Trim", &json!({ "db": -4.0 })));

        assert_eq!(
            ok(s.item_filters("Main", one.id)).iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["Round", "Trim"],
            "Main keeps Round and receives shared audio filter Trim"
        );
        assert_eq!(
            ok(s.item_filters("Two", two.id)).iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["Edge", "Trim"],
            "Two has only Edge + shared Trim"
        );

        // Switching Two back to shared restores Cam's shared video filters.
        let shared = ok(s.set_item_filter_scope("Two", two.id, false));
        assert!(!shared.own_filters);
        assert_eq!(
            ok(s.item_filters("Two", two.id)).iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["Round", "Trim"]
        );
    }

    #[test]
    fn item_override_crop_frames_independently_and_roundtrips_save_load() {
        let s = stage();
        ok(s.start_vertical("Vertical"));
        let main_card = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Card",
            &json!({ "color": 4278190335u32, "width": 1920, "height": 1080 }),
            false,
        ));
        let vert_card = ok(s.add_existing("Vertical", "Card"));
        let vert_card = ok(s.set_item_filter_scope("Vertical", vert_card.id, true));
        ok(s.add_item_filter("Vertical", vert_card.id, "crop_filter", "Crop", &json!({ "left": 200, "top": 100 })));

        let m = ok(s.items("Main")).into_iter().find(|i| i.id == main_card.id).unwrap();
        let v = ok(s.items("Vertical")).into_iter().find(|i| i.id == vert_card.id).unwrap();
        assert_eq!(m.frame[0..4], [1920.0, 0.0, 0.0, 1080.0]);
        assert_eq!(v.frame[0..4], [1720.0, 0.0, 0.0, 980.0]);
        let vt = ok(s.transform("Vertical", vert_card.id));
        assert_eq!((vt.width, vt.height), (1720, 980));

        // Save and reload preserves override and frame; old collection without override stays shared.
        let saved = s.save("Roundtrip");
        let imported = ferrocore::obs::import(&saved, (1920, 1080));
        let main_imp = imported.scenes.iter().find(|(n, _)| n == "Main").unwrap().1.clone();
        let vert_imp = imported.scenes.iter().find(|(n, _)| n == "Vertical").unwrap().1.clone();
        assert!(main_imp[0].effects.is_empty());
        assert_eq!(vert_imp[0].effects.len(), 1);

        let reloaded = ok(Stage::load(Canvas { width: 1920, height: 1080, fps: 30 }, &saved, "Main"));
        let rv = ok(reloaded.items("Vertical")).into_iter().find(|i| i.id == vert_card.id).unwrap();
        assert!(rv.own_filters);
        assert_eq!(rv.frame[0..4], [1720.0, 0.0, 0.0, 980.0]);
    }

    #[test]
    fn paste_move_duplicate_and_item_picture_respect_item_filter_overrides() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"));
        if s.engine().is_err() {
            return;
        }
        ok(s.start_vertical("Vertical"));
        let main_red = ok(s.add_source(
            "Main",
            "color_source_v3",
            "Red",
            &json!({ "color": 4278190335u32, "width": 320, "height": 180 }),
            false,
        ));
        ok(s.add_source("Main", "color_source_v3", "Donor", &json!({ "width": 64, "height": 36 }), false));
        ok(s.add_filter("Donor", "color_filter_v2", "Dim", &json!({ "brightness": -0.5 })));
        ok(s.add_filter("Donor", "ferrocast_rounded_corners", "Round", &json!({ "radius": 30 })));

        let vert_red = ok(s.add_existing("Vertical", "Red"));
        ok(s.set_item_filter_scope("Vertical", vert_red.id, true));
        ok(s.paste_item_filters("Vertical", vert_red.id, "Donor"));
        ok(s.move_item_filter("Vertical", vert_red.id, "Round", ItemEdit::Top));
        let flts = ok(s.item_filters("Vertical", vert_red.id));
        assert_eq!(flts.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Round", "Dim"]);
        ok(s.set_item_filter_enabled("Vertical", vert_red.id, "Round", false));
        assert!(!ok(s.item_filters("Vertical", vert_red.id))[0].enabled);

        // Main item picture has no filters; Vertical item picture shows Dim only (not Round).
        // Passing mismatched source name onto item picture fails cleanly.
        assert!(s.item_picture_pixels_for(Some("Donor"), "Vertical", vert_red.id, 160, true).is_err());
        let (main_px, _, _, _) = ok(s.item_picture_pixels("Main", main_red.id, 160, true)).unwrap();
        let (vert_px, vw, vh, _) = ok(s.item_picture_pixels("Vertical", vert_red.id, 160, true)).unwrap();
        let mid = ((vh as usize / 2) * vw as usize + (vw as usize / 2)) * 4;
        assert!(main_px[mid] > vert_px[mid] + 40, "item picture renders item override");

        // Paste filters directly from Vertical's item override back onto Main's item.
        ok(s.paste_filters_from("Red", "", Some(("Vertical", vert_red.id))));
        assert_eq!(ok(s.filters("Red")).len(), 2);

        // Duplicating Vertical item copies its active item filters onto the duplicated source.
        ok(s.edit_item("Vertical", vert_red.id, ItemEdit::Duplicate));
        let vert_items = ok(s.items("Vertical"));
        assert_eq!(vert_items.len(), 2);
        let dup = vert_items.iter().find(|i| i.id != vert_red.id).unwrap();
        assert!(!dup.own_filters);
        assert_eq!(ok(s.filters(&dup.name)).len(), 2);
    }

    #[test]
    fn named_transitions_crud_rename_duplicate_delete_and_default() {
        let s = stage();
        let initial = s.named_transitions();
        assert_eq!(initial.len(), 2);
        assert!(initial.iter().any(|t| t.name == "Fade" && t.default && t.ms == 300));
        assert!(initial.iter().any(|t| t.name == "Cut" && !t.default && t.ms == 0));

        // Add named transitions with custom settings.
        let swipe = ok(s.add_named_transition(&NamedTransition {
            name: "Fast Swipe".into(),
            kind: "swipe_transition".into(),
            ms: 250,
            settings: json!({ "direction": "right", "swipe_in": true }),
            default: false,
        }));
        assert_eq!(swipe.ms, 250);
        assert!(!swipe.default);

        // Duplicate name rejected.
        assert!(s.add_named_transition(&NamedTransition { name: "Fast Swipe".into(), ..Default::default() }).is_err());

        // Duplicate generates free copy with default=false.
        let dup = ok(s.duplicate_named_transition("Fast Swipe", None));
        assert_eq!(dup.name, "Fast Swipe 2");
        assert_eq!(dup.kind, "swipe_transition");
        assert_eq!(dup.ms, 250);
        assert_eq!(dup.settings["direction"], "right");
        assert!(!dup.default);

        // Update settings preserves independent instances; switching kind resets old settings cleanly.
        let edited = ok(s.update_named_transition(
            "Fast Swipe 2",
            None,
            Some(450),
            Some(&json!({ "direction": "up" })),
            Some(true),
        ));
        assert_eq!(edited.ms, 450);
        assert_eq!(edited.settings["direction"], "up");
        assert!(edited.default);
        assert_eq!(s.transition(), TransitionInfo { kind: "swipe_transition".into(), ms: 450 });
        assert_eq!(
            s.named_transitions().iter().find(|t| t.name == "Fast Swipe").unwrap().settings["direction"],
            "right"
        );

        // Delete default promotes Fade (or first remaining) safely; deleting last fails.
        ok(s.delete_named_transition("Fast Swipe 2"));
        assert!(s.named_transitions().iter().any(|t| t.default));
        ok(s.delete_named_transition("Fast Swipe"));
        ok(s.delete_named_transition("Cut"));
        assert_eq!(s.named_transitions().len(), 1);
        assert!(s.delete_named_transition("Fade").is_err(), "cannot delete last transition");
    }

    #[test]
    fn replacing_a_missing_file_keeps_the_other_stinger_settings() {
        let s = stage();
        ok(s.add_named_transition(&NamedTransition {
            name: "Intro Stinger".into(),
            kind: "obs_stinger_transition".into(),
            ms: 0,
            settings: json!({ "path": "/gone/old.webm", "transition_point": 450, "transition_point_type": 0 }),
            default: false,
        }));
        let m = crate::collection::MissingFile {
            source: "Intro Stinger".into(),
            parent: None,
            kind: "obs_stinger_transition".into(),
            key: "path".into(),
            label: "Video".into(),
            path: "/gone/old.webm".into(),
        };
        ok(s.replace_file(&m, "/new/stinger.webm"));
        let t = s.named_transitions().into_iter().find(|t| t.name == "Intro Stinger").unwrap();
        assert_eq!(t.settings["path"], "/new/stinger.webm");
        assert_eq!(t.settings["transition_point"], 450, "other keys survive: {}", t.settings);
        // A transition that was renamed or deleted meanwhile is an error, not a silent overwrite
        // of the default transition's settings.
        let before = s.named_transitions();
        ok(s.rename_named_transition("Intro Stinger", "Renamed"));
        assert!(s.replace_file(&m, "/other.webm").is_err());
        let after = s.named_transitions();
        assert_eq!(before.iter().find(|t| t.default), after.iter().find(|t| t.default));
    }

    #[test]
    fn setting_a_kinds_settings_merges_and_the_named_transition_wins_over_the_legacy_map() {
        let s = stage();
        ok(s.add_named_transition(&NamedTransition {
            name: "Wipe".into(),
            kind: "swipe_transition".into(),
            ms: 300,
            settings: json!({ "direction": "left", "swipe_in": false }),
            default: true,
        }));
        ok(s.set_transition_settings("swipe_transition", &json!({ "direction": "up" })));
        let cur = s.named_transitions().into_iter().find(|t| t.default).unwrap();
        assert_eq!(cur.settings["direction"], "up");
        assert_eq!(cur.settings["swipe_in"], false, "keys not sent are kept: {}", cur.settings);
        // A stale legacy map must not mask the named transition's own settings.
        s.edit(|d| {
            d["ferrocast"]["transition_settings"] = json!({ "swipe_transition": { "direction": "stale" } });
        });
        ok(s.update_named_transition("Wipe", None, None, Some(&json!({ "direction": "down" })), None));
        assert_eq!(s.transition_settings("swipe_transition")["direction"], "down");
    }

    #[test]
    fn imported_scene_overrides_cope_with_odd_scene_names_and_long_transition_names() {
        let long = "S".repeat(95);
        let mut d = json!({
            "sources": [
                { "id": "scene", "name": "Main / Camera", "private_settings": { "transition": "Swipe", "transition_duration": 700 } },
                { "id": "scene", "name": "BRB ~ 5m", "private_settings": { "transition": long.clone(), "transition_duration": 900 } },
                { "id": "scene", "name": "Plain", "private_settings": {} }
            ],
            "scene_order": [{ "name": "Main / Camera" }, { "name": "BRB ~ 5m" }, { "name": "Plain" }],
            "transitions": [
                { "id": "swipe_transition", "name": "Swipe", "settings": {} },
                { "id": "fade_transition", "name": long.clone(), "settings": {} },
                { "id": "move_transition", "name": "Mover", "settings": {} }
            ],
            "ferrocast": { "scene_transitions": { "Main / Camera": { "kind": "fade_transition", "ms": 100 } } }
        });
        import_and_normalize_transitions(&mut d);
        let rules = transition_matrix_from_doc(&d);
        // The legacy override on a scene with "/" in its name is found, so no rule replaces it.
        assert!(!rules.iter().any(|r| r.to.as_deref() == Some("Main / Camera")), "{rules:?}");
        // The generated "(900ms)" copy of a 95-character name still fits the 100-character limit.
        let list = named_transitions_from_doc(&d);
        assert!(
            list.iter().all(|t| t.name.chars().count() <= 100),
            "{:?}",
            list.iter().map(|t| t.name.len()).collect::<Vec<_>>()
        );
        assert!(rules.iter().any(|r| r.to.as_deref() == Some("BRB ~ 5m")));
        // Exeldro's Move Transition is one Ferrocast renders: not downgraded to Fade.
        assert_eq!(list.iter().find(|t| t.name == "Mover").unwrap().kind, "ferrocast_move");
    }

    #[test]
    fn undo_in_studio_mode_keeps_what_is_on_air_and_transition_edits_leave_the_graph_alone() {
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "A").unwrap_or_else(|e| panic!("{e}"));
        ok(s.add_scene("B"));
        s.set_studio_mode(true);
        let snap = s.snapshot();
        // Transition to B: B is on air now, A in the preview.
        ok(s.set_preview("B"));
        ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
        assert_eq!(s.current(), "B");
        let on_air = s.snapshot().pointer("/ferrocast/program_copy/scene").cloned();
        let preview = s.snapshot().pointer("/ferrocast/studio_mode").cloned();
        // Undoing an old edit (the snapshot from before the switch) must not put A back on air.
        s.restore_all(&snap);
        assert_eq!(s.current(), "B");
        assert_eq!(s.snapshot().pointer("/ferrocast/program_copy/scene").cloned(), on_air, "on-air copy kept");
        assert_eq!(s.snapshot().pointer("/ferrocast/studio_mode").cloned(), preview, "preview kept");

        // Transition and matrix edits are document-only: the scene graph is not reloaded.
        let before = s.graph_reloads();
        ok(s.add_named_transition(&NamedTransition {
            name: "Slow".into(),
            kind: "fade_transition".into(),
            ms: 2000,
            settings: json!({}),
            default: false,
        }));
        ok(s.update_named_transition("Slow", None, Some(2500), None, None));
        ok(s.set_matrix_rule(Some("A"), Some("B"), Some("Slow")));
        assert_eq!(s.graph_reloads(), before, "a transition edit must not reload the graph mid-transition");
    }

    #[test]
    fn a_rule_from_a_scene_to_itself_is_refused_and_dropped_on_load() {
        let s = stage();
        ok(s.add_scene("Other"));
        let name = s.order()[0].clone();
        assert!(s.set_matrix_rule(Some(&name), Some(&name), Some("Cut")).is_err());
        // Clearing is still fine, and Any -> scene is not a self pair.
        ok(s.set_matrix_rule(Some(&name), Some(&name), None));
        ok(s.set_matrix_rule(None, Some(&name), Some("Cut")));
        let mut d = s.snapshot();
        d["ferrocast"]["transition_matrix"] = json!([
            { "from": name, "to": name, "transition": "Cut" },
            { "from": null, "to": "Other", "transition": "Cut" }
        ]);
        import_and_normalize_transitions(&mut d);
        assert_eq!(transition_matrix_from_doc(&d).len(), 1);
    }

    #[test]
    fn transition_matrix_precedence_and_lifecycle_cleanup_and_renames() {
        let s = stage();
        ok(s.add_scene("BRB"));
        ok(s.add_scene("Game"));

        ok(s.add_named_transition(&NamedTransition {
            name: "Pair Swipe".into(),
            kind: "swipe_transition".into(),
            ms: 400,
            settings: json!({ "direction": "left" }),
            default: false,
        }));
        ok(s.add_named_transition(&NamedTransition {
            name: "To Game Slide".into(),
            kind: "slide_transition".into(),
            ms: 500,
            settings: json!({ "direction": "up" }),
            default: false,
        }));
        ok(s.add_named_transition(&NamedTransition {
            name: "From Main Color".into(),
            kind: "fade_to_color_transition".into(),
            ms: 600,
            settings: json!({ "color": 4278190080u32 }),
            default: false,
        }));

        ok(s.set_matrix_rule(Some("Main"), None, Some("From Main Color")));
        ok(s.set_matrix_rule(None, Some("Game"), Some("To Game Slide")));
        ok(s.set_matrix_rule(Some("Main"), Some("Game"), Some("Pair Swipe")));

        // 1. Exact pair beats both wildcards and default.
        let r = s.resolve_transition("Main", "Game");
        assert_eq!((r.name.as_str(), r.kind.as_str(), r.ms), ("Pair Swipe", "swipe_transition", 400));

        // 2. Any -> to beats from -> Any and default.
        let r = s.resolve_transition("BRB", "Game");
        assert_eq!((r.name.as_str(), r.kind.as_str(), r.ms), ("To Game Slide", "slide_transition", 500));

        // 3. from -> Any beats default.
        let r = s.resolve_transition("Main", "BRB");
        assert_eq!((r.name.as_str(), r.kind.as_str(), r.ms), ("From Main Color", "fade_to_color_transition", 600));

        // 4. Default when no rule matches.
        let r = s.resolve_transition("BRB", "Main");
        assert_eq!((r.name.as_str(), r.kind.as_str(), r.ms), ("Fade", "fade_transition", 300));

        // Renaming transition preserves all matrix references.
        ok(s.rename_named_transition("Pair Swipe", "Main-to-Game Swipe"));
        let r = s.resolve_transition("Main", "Game");
        assert_eq!(r.name, "Main-to-Game Swipe");

        // Renaming scene preserves matrix references and legacy overrides.
        ok(s.rename("Game", "Play"));
        let r = s.resolve_transition("Main", "Play");
        assert_eq!(r.name, "Main-to-Game Swipe");
        let r2 = s.resolve_transition("BRB", "Play");
        assert_eq!(r2.name, "To Game Slide");

        // Deleting transition cleans up exact rule and safely falls back to Any -> Play!
        ok(s.delete_named_transition("Main-to-Game Swipe"));
        assert!(!s.transition_matrix().iter().any(|r| r.transition == "Main-to-Game Swipe"));
        let fallback = s.resolve_transition("Main", "Play");
        assert_eq!(fallback.name, "To Game Slide");

        // Deleting scene cleans up all rules mentioning it without leaving dangling refs.
        ok(s.remove_scene("Play"));
        assert!(
            !s.transition_matrix().iter().any(|r| r.from.as_deref() == Some("Play") || r.to.as_deref() == Some("Play"))
        );
    }

    #[test]
    fn switching_paths_and_obs_import_and_old_collections_work() {
        // Old minimal collection loads unchanged with default transitions.
        let old_doc = json!({
            "name": "Legacy",
            "sources": [
                { "id": "scene", "name": "Intro", "settings": { "items": [] } },
                { "id": "scene", "name": "Live", "settings": { "items": [] } },
                { "id": "scene", "name": "BRB", "settings": { "items": [] } },
            ],
            "scene_order": [{ "name": "Intro" }, { "name": "Live" }, { "name": "BRB" }],
            "current_scene": "Intro",
            "current_program_scene": "Intro",
            "current_transition": "Fade",
            "transition_duration": 750,
            "ferrocast": {
                "transition_settings": { "swipe_transition": { "direction": "down" } },
                "scene_transitions": { "BRB": { "kind": "cut_transition", "ms": 0 } }
            }
        });
        let legacy = ok(Stage::load(Canvas { width: 320, height: 180, fps: 30 }, &old_doc, "Main"));
        assert_eq!(legacy.transition(), TransitionInfo { kind: "fade_transition".into(), ms: 750 });
        assert_eq!(legacy.resolve_transition("Intro", "Live").ms, 750);
        assert_eq!(legacy.resolve_transition("Intro", "BRB").kind, "cut_transition");

        // OBS collection with transitions array & scene private_settings transition override.
        let obs_doc = json!({
            "name": "OBS Export",
            "sources": [
                { "id": "scene", "name": "Start", "settings": { "items": [] } },
                {
                    "id": "scene",
                    "name": "Stage",
                    "settings": { "items": [] },
                    "private_settings": { "transition": "OBS Swipe", "transition_duration": 420 }
                }
            ],
            "scene_order": [{ "name": "Start" }, { "name": "Stage" }],
            "current_scene": "Start",
            "current_program_scene": "Start",
            "current_transition": "OBS Fade",
            "transition_duration": 350,
            "transitions": [
                { "id": "fade_transition", "name": "OBS Fade", "settings": {} },
                { "id": "swipe_transition", "name": "OBS Swipe", "settings": { "direction": "right" } }
            ]
        });
        let obs = ok(Stage::load(Canvas { width: 320, height: 180, fps: 30 }, &obs_doc, "Main"));
        let trs = obs.named_transitions();
        assert_eq!(trs.len(), 3, "OBS Fade + OBS Swipe + imported duration variant");
        let ov = obs.resolve_transition("Start", "Stage");
        assert_eq!((ov.kind.as_str(), ov.ms), ("swipe_transition", 420));
        assert_eq!(ov.settings["direction"], "right");
        let scene_row = obs.scenes().into_iter().find(|x| x.name == "Stage").unwrap();
        assert_eq!(scene_row.transition, Some(TransitionInfo { kind: "swipe_transition".into(), ms: 420 }));
        assert!(scene_row.transition_name.as_deref() == Some("OBS Swipe (420ms)"));

        // Direct switch, Studio Mode TransitionNow, and T-bar release all use resolved transition.
        let s = Stage::new(Canvas { width: 320, height: 180, fps: 30 }, "One").unwrap_or_else(|e| panic!("{e}"));
        ok(s.add_scene("Two"));
        ok(s.add_scene("Three"));
        ok(s.add_named_transition(&NamedTransition {
            name: "Instant Cut".into(),
            kind: "cut_transition".into(),
            ms: 0,
            settings: json!({}),
            default: false,
        }));
        ok(s.set_matrix_rule(Some("One"), Some("Two"), Some("Instant Cut")));
        if let Ok(e) = s.engine() {
            ok(s.add_source("One", "color_source_v3", "R", &json!({ "color": 4278190335u32 }), false));
            ok(s.add_source("Two", "color_source_v3", "B", &json!({ "color": 4294901760u32 }), false));
            ok(s.add_source("Three", "color_source_v3", "G", &json!({ "color": 4278255360u32 }), false));
            let mid = ((90 * 320 + 160) * 4) as usize;
            let px = || e.engine.snapshot().map(|p| p[mid..mid + 3].to_vec()).unwrap_or_default();

            // Direct switch One -> Two via Instant Cut completes in 0 ms immediately.
            ok(s.set_current("Two"));
            std::thread::sleep(std::time::Duration::from_millis(40));
            assert_eq!(px(), [0, 0, 255], "exact pair cut finished right away");

            // Back to One by an explicit quick Cut in Studio Mode, then TransitionNow & T-bar. The
            // default stays the 300 ms Fade: only the matrix makes the rest of these cuts, so the
            // assertions fail if either path ignores it.
            s.set_studio_mode(true);
            ok(s.set_preview("One"));
            ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
            ok(s.set_preview("Two"));
            assert!(s.scenes().into_iter().find(|x| x.name == "Two").unwrap().preview);
            ok(s.transition_now(None));
            std::thread::sleep(std::time::Duration::from_millis(40));
            assert_eq!(s.current(), "Two");
            assert_eq!(px(), [0, 0, 255], "Studio Mode transition_now resolved pair");

            // T-bar release from One -> Two.
            ok(s.set_preview("One"));
            ok(s.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 })));
            assert_eq!(s.current(), "One");
            assert_eq!(s.preview_scene().as_deref(), Some("Two"));
            let tb = ok(s.tbar(1.0, true));
            assert!(!tb.active);
            std::thread::sleep(std::time::Duration::from_millis(40));
            assert_eq!(s.current(), "Two");
            assert_eq!(px(), [0, 0, 255], "T-bar release used resolved cut transition");
        }
    }
}
