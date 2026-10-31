//! The Studio's engine in the desktop app (2.1, Track B): libobs runs on the main thread (the
//! preview is a native view in this window, which AppKit allows only there). The dashboard
//! reaches it through the engine's HTTP routes; this is the engine's `StudioHost`.
//!
//! For the 2.1 preview, OBS's engine comes from the OBS installed on this computer; a later
//! version ships it inside Ferrocast.

use std::cell::RefCell;
use std::path::PathBuf;

use ferrocast_engine::studio_host::{
    CaptionsSet, FilePick, FilterEdit, FollowSet, ItemEdit as Edit, Monitor as Hear, PreviewRect, ProjectorReq,
    STOPPED, ShelfAct, StudioAct, StudioHost, StudioSettings, StudioStatus,
};
use ferrocast_studio::{
    AudioSetup, Canvas, Collection, EncodeSettings, ItemEdit, Monitor, Paths, Preview, Projector, ProjectorOf,
    Recording, Rect, StreamSettings, Streaming, Studio, StudioOptions, TransitionInfo,
};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

use crate::studio_history::History;
use crate::studio_library::{self as library, Kind};

/// Everything libobs-related, on the main thread only.
///
/// libobs loads once and stays until Ferrocast quits: it can't be shut down and started again
/// in one process (the second shutdown crashed in FreeType's cleanup, 25 Oct; OBS never does
/// it either). Stop takes the scenes and preview away; Start loads them back on the same engine.
#[derive(Default)]
struct Held {
    studio: Option<Studio>,
    paths: Option<Paths>,
    /// The sound setup the engine started with (it can't change until Ferrocast restarts).
    audio: Option<AudioSetup>,
    collection: Option<Collection>,
    preview: Option<Preview>,
    /// Where the preview is, to make it again when the canvas changes.
    preview_at: Option<(Rect, f64)>,
    stream: Option<Streaming>,
    /// The vertical picture's stream, beside the main one.
    vstream: Option<Streaming>,
    record: Option<(Recording, PathBuf, std::time::Instant)>,
    /// Sources recorded on their own (Source Record): name, recording, file.
    source_records: Vec<(String, Recording, PathBuf)>,
    replay: Option<ferrocast_studio::ReplayBuffer>,
    virtualcam: Option<ferrocast_studio::VirtualCamera>,
    settings: Option<StudioSettings>,
    renderer: &'static str,
    /// The item outlined in the preview (scene, id), kept when the preview is made again.
    selected: Option<(String, Vec<i64>)>,
    /// The scene whose sources the preview outlines, each in its colour.
    frames: Option<String>,
    /// Where the canvas is inside the preview's box (points), when the page placed it.
    preview_canvas: Option<(f64, f64, f64, f64)>,
    /// The page's colour round the canvas (0xAABBGGRR), when it said.
    preview_bg: Option<u32>,
    /// Where the page's menus and panels lie over the preview (points from its box's top left).
    preview_holes: Vec<(f64, f64, f64, f64, f64)>,
    /// How much of the preview shows, 0 to 1.
    preview_opacity: Option<f64>,
    /// The dialog backdrop's shade over the preview, 0 to 1.
    preview_shade: f64,
    /// The same for Studio Mode's picture of what's on air.
    program_bg: Option<u32>,
    /// Where the page's menus and panels lie over Studio Mode's picture of what's on air.
    program_holes: Vec<(f64, f64, f64, f64, f64)>,
    program_opacity: Option<f64>,
    program_shade: f64,
    /// Undo and redo for sources.
    history: History,
    /// Studio Mode's picture of what's on air, beside the preview, and where it goes.
    program_view: Option<Preview>,
    program_at: Option<(Rect, f64)>,
    /// Open projector windows, by id (they go when the Studio stops or the canvas changes).
    projectors: Vec<(u64, Projector)>,
    last_projector: u64,
    /// The vertical picture's preview, beside the main one.
    vertical: VerticalView,
    /// Live captions, and the sound they listen to.
    captions: Option<(Listening, String)>,
    /// Which run of captions the Whisper worker's words belong to.
    captions_run: u64,
    /// The vertical item following a face, and which run of the loop moves it.
    follow: Option<ferrocast_studio::FaceFollow>,
    follow_run: u64,
}

/// The vertical picture's preview and what the page last said about its box.
#[derive(Default)]
struct VerticalView {
    view: Option<Preview>,
    at: Option<(Rect, f64)>,
    canvas: Option<(f64, f64, f64, f64)>,
    bg: Option<u32>,
    holes: Vec<(f64, f64, f64, f64, f64)>,
    opacity: Option<f64>,
    shade: f64,
}

/// Makes the vertical picture's preview at its last place, when the picture is on and the page
/// has a box for it (main thread).
fn make_vertical_view(app: &AppHandle, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.vertical.at else { return Ok(()) };
    let Some(c) = h.collection.as_ref() else { return Ok(()) };
    if c.vertical().is_none() {
        h.vertical.view = None;
        return Ok(());
    }
    let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
    let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
    let mut p = c.vertical_preview(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
    pass_input(&window, &p);
    let v = &h.vertical;
    if let Some(area) = v.canvas {
        p.set_area(rect, area, scale);
    }
    p.select(h.selected.as_ref().map(|(s, i)| (s.as_str(), i.as_slice())));
    p.frames(h.frames.as_deref());
    if let Some(bg) = v.bg {
        p.background(bg);
    }
    p.set_holes(&v.holes);
    p.set_opacity(v.opacity.unwrap_or(1.0));
    p.set_shade(v.shade);
    h.vertical.view = Some(p);
    Ok(())
}

/// A multiview follows the scenes after anything that changes them.
/// How multiviews are laid out, from the settings.
fn multiview_look(s: &StudioSettings) -> ferrocast_studio::MultiviewLook {
    use ferrocast_studio::MultiviewLayout as L;
    ferrocast_studio::MultiviewLook {
        layout: match s.multiview_layout.as_str() {
            "bottom" => L::Bottom,
            "left" => L::Left,
            "right" => L::Right,
            "scenes_only" => L::ScenesOnly,
            _ => L::Top,
        },
        labels: s.multiview_labels,
        safe_areas: s.multiview_safe_areas,
    }
}

fn refresh_projectors(h: &Held) {
    if h.projectors.is_empty() {
        return;
    }
    if let Some(c) = &h.collection {
        let names: Vec<String> = c.scenes().into_iter().map(|s| s.name).collect();
        let preview = c.preview_scene();
        for (_, p) in &h.projectors {
            p.update(&names, preview.as_deref());
        }
    }
}

thread_local! {
    static HELD: RefCell<Held> = RefCell::default();
}

/// Where OBS's engine is (macOS): the one Ferrocast.app carries (deploy/obs-runtime-macos.sh),
/// else an installed OBS.app (dev builds).
fn obs_paths(metal: bool) -> Option<(Paths, &'static str)> {
    #[cfg(target_os = "macos")]
    {
        // …/Ferrocast.app/Contents/MacOS/ferrocast-desktop → …/Contents
        let own = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf()));
        let mut places: Vec<PathBuf> = own.into_iter().collect();
        places.push(PathBuf::from("/Applications/OBS.app/Contents"));
        if let Some(home) = dirs::home_dir() {
            places.push(home.join("Applications/OBS.app/Contents"));
        }
        // Metal is the default from 29 Oct (OpenGL crashed in gl_update once while previews
        // came and went); OpenGL stays in Settings. An OBS without Metal's renderer gets OpenGL.
        places.into_iter().find(|p| p.join("Frameworks/libobs.framework").exists()).map(|c| {
            let metal = metal && c.join("Frameworks/libobs-metal.dylib").exists();
            (Paths::from_obs_app(&c, metal), if metal { "metal" } else { "opengl" })
        })
    }
    #[cfg(windows)]
    {
        // The engine the installer puts beside Ferrocast (deploy/obs-runtime-windows.ps1),
        // else an installed OBS (dev builds). D3D11 is OBS's renderer on Windows.
        let _ = metal;
        let own = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("obs")));
        let mut places: Vec<PathBuf> = own.into_iter().collect();
        for var in ["ProgramFiles", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(var) {
                places.push(PathBuf::from(p).join("obs-studio"));
            }
        }
        places
            .into_iter()
            .find(|p| p.join("bin/64bit/obs.dll").exists())
            .map(|root| (Paths::from_obs_windows(&root), "d3d11"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = metal;
        None
    }
}

/// Passes mouse actions over a picture on to the page (Windows: the picture's own window takes
/// them, where on a Mac they go through to the page by themselves).
fn pass_input(window: &tauri::WebviewWindow, p: &Preview) {
    let w = window.clone();
    p.set_input(Box::new(move |i| {
        if let Ok(j) = serde_json::to_string(&i) {
            let _ = w.eval(format!("window.__ferrocastPreviewInput && window.__ferrocastPreviewInput({j})"));
        }
    }));
}

/// What the Studio streams into Go live with: the Mac's hardware H.264 when there is one,
/// else x264. Go live sends it on as it comes, so this is what viewers get (Twitch takes up
/// to 6000 kbps; Kick refuses over 8000, 25 Oct).
/// How the settings ask to encode: the Mac's hardware H.264 (VideoToolbox) or x264.
fn encode_of(studio: &Studio, s: &StudioSettings) -> EncodeSettings {
    // The Mac's VideoToolbox; on Windows the GPU's own (NVIDIA, AMD, then Intel), as OBS lists them.
    const HARDWARE: [&str; 5] = [
        "com.apple.videotoolbox.videoencoder.ave.avc",
        "obs_nvenc_h264_tex",
        "jim_nvenc",
        "h264_texture_amf",
        "obs_qsv11_v2",
    ];
    let kinds = studio.kinds();
    let hardware = (s.encoder == "hardware")
        .then(|| HARDWARE.into_iter().find(|id| kinds.encoders.iter().any(|e| e.id == *id)))
        .flatten();
    // A particular encoder (HEVC, AV1…) when it's on this computer.
    let chosen = s.video_encoder.as_ref().filter(|id| kinds.encoders.iter().any(|e| &e.id == *id));
    EncodeSettings {
        video_encoder: chosen.cloned().unwrap_or_else(|| hardware.unwrap_or("obs_x264").into()),
        video_kbps: s.video_kbps,
        vbr: s.vbr,
        quality: None,
        keyframe_s: s.keyframe_s,
        profile: s.profile.clone(),
        bframes: s.bframes,
        scale: s.scale.map(|[w, h]| (w, h)),
        audio_kbps: s.audio_kbps,
        extra: if chosen.is_some() || s.video_encoder.is_none() {
            s.encoder_settings.clone()
        } else {
            Default::default()
        },
    }
}

/// What the Studio streams into Go live with. Go live sends it on as it comes, so this is what
/// viewers get (Twitch takes up to 6000 kbps; Kick refuses over 8000, 25 Oct).
fn stream_settings(studio: &Studio, s: &StudioSettings) -> StreamSettings {
    StreamSettings {
        encode: encode_of(studio, s),
        reconnect: s.reconnect.then_some((s.reconnect_tries, s.reconnect_every_s)),
        delay_s: s.delay_s,
    }
}

/// What a recording uses: the stream's settings, or constant quality (high: near-lossless
/// with the same encoder; lossless: x264, much bigger files).
fn record_encode(studio: &Studio, s: &StudioSettings) -> EncodeSettings {
    let mut e = encode_of(studio, s);
    match s.record_quality.as_str() {
        "high" => e.quality = Some(16),
        "lossless" => {
            e.video_encoder = "obs_x264".into();
            e.quality = Some(0);
            e.extra = Default::default();
        }
        // Its own encoder and options, as OBS's Advanced recording settings.
        "custom" => {
            let here = |id: &String| studio.kinds().encoders.iter().any(|k| &k.id == id);
            match &s.record_encoder {
                Some(id) if here(id) => {
                    e.video_encoder = id.clone();
                    e.extra = s.record_encoder_settings.clone();
                }
                // Picked for you: the stream's encoder, with these options.
                None => e.extra = s.record_encoder_settings.clone(),
                // An encoder from another computer: the stream's, without its options.
                Some(_) => {}
            }
        }
        _ => {}
    }
    // A recording keeps the canvas's full size.
    e.scale = None;
    e
}

/// The scene collection in use now. Switches happen on the main thread, so read it there.
/// The scene list from a saved collection (OBS's format: `scene_order`, `current_scene`), as
/// the Studio lists them when running, marked `saved` (nothing is on air).
fn saved_scenes(doc: Option<&Value>) -> Value {
    let names: Vec<&str> = doc
        .and_then(|d| d.get("scene_order"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .collect();
    let scenes: Vec<Value> =
        names.iter().map(|n| json!({ "name": n, "items": 0, "current": false, "preview": false })).collect();
    json!({
        "scenes": scenes,
        "current": doc.and_then(|d| d.get("current_scene")).and_then(Value::as_str).unwrap_or(""),
        "transition": null,
        "preview": null,
        "vertical": null,
        "saved": true,
    })
}

fn scenes_file(data: &std::path::Path) -> PathBuf {
    library::load(data).current_file(data, Kind::Collections)
}

/// The profile in use now.
fn settings_file(data: &std::path::Path) -> PathBuf {
    library::load(data).current_file(data, Kind::Profiles)
}

/// Takes back an entry that was just made, with its file (its switch failed).
fn forget(data: &std::path::Path, kind: Kind, id: &str) {
    let mut lib = library::load(data);
    lib.shelf_mut(kind).list.retain(|e| e.id != id);
    library::save(data, &lib);
    let _ = std::fs::remove_file(library::file(data, kind, id));
}

/// Saves the scenes in use to `from` and puts `to`'s in their place (a missing file is a new
/// set with one empty scene). If `to` won't load, the old ones come back.
fn swap_collection(h: &mut Held, from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    let Some(studio) = h.studio.clone().filter(|_| h.collection.is_some()) else {
        // Not running: the next Start loads it.
        return Ok(());
    };
    // libobs keeps one collection's sources at a time: the old ones go first, with the undo
    // steps that hold them and the projectors that show them.
    // The vertical preview and stream use the old collection's canvas: they go before it does.
    h.vertical.view = None;
    h.vstream = None;
    h.captions = None;
    // Put the following item back first, so neither collection is saved slid sideways.
    if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
        c.stop_follow(&f);
    }
    let old = h.collection.take().map(|c| {
        save(&c, from);
        c.save(COLLECTION)
    });
    h.history.clear();
    h.selected = None;
    h.frames = None;
    // The old scenes are going: the preview lets go of the one it shows (a scene of the same
    // name in the new set is another source).
    if let Some(p) = &h.preview {
        p.show_scene(None);
    }
    // Only a missing file is a new set; one that doesn't read is kept aside, never overwritten.
    let made = match std::fs::read(to).ok().map(|b| serde_json::from_slice::<Value>(&b)) {
        Some(Ok(d)) => Collection::load(&studio, &d, FIRST_SCENE),
        Some(Err(e)) => {
            tracing::warn!(error = %e, file = %to.display(), "a scene collection didn't read; kept aside");
            let _ = std::fs::rename(to, to.with_extension("json.unreadable"));
            Collection::new(&studio, FIRST_SCENE)
        }
        None => Collection::new(&studio, FIRST_SCENE),
    };
    match made {
        Ok(c) => {
            save(&c, to);
            c.set_true_peak(h.settings.as_ref().is_some_and(|s| s.true_peak));
            h.collection = Some(c);
            refresh_projectors(h);
            sync_views(h);
            Ok(())
        }
        Err(e) => {
            if let Some(old) = old {
                h.collection = Collection::load(&studio, &old, FIRST_SCENE).ok();
            }
            sync_views(h);
            Err(format!("That scene collection didn't load: {e}"))
        }
    }
}

/// The saved settings, or the defaults (an unreadable file is left as it is).
fn load_settings(data: &std::path::Path) -> StudioSettings {
    std::fs::read(settings_file(data))
        .ok()
        .and_then(|b| serde_json::from_slice::<StudioSettings>(&b).ok())
        .filter(|s| s.check().is_ok())
        .unwrap_or_default()
}

fn save_json(file: &std::path::Path, v: &Value) {
    let tmp = file.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(v)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, file));
    if let Err(e) = written {
        tracing::warn!(error = %e, file = %file.display(), "the Studio couldn't save");
    }
}

/// The folder recordings go to: the one set, or Movies/Ferrocast.
fn record_folder(s: &StudioSettings) -> PathBuf {
    s.record_folder.as_ref().map(PathBuf::from).unwrap_or_else(|| {
        dirs::video_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Movies")).join("Ferrocast")
    })
}

/// How the last remux went: `{ from, to, percent, done, error }`, or null.
static REMUX: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

/// Where the Studio's note of what it's doing is kept while it runs (`session.json`): still
/// there when Ferrocast opens means it didn't close normally, and the next Start puts things
/// back as they were.
static SESSION: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// What the note said at launch, for the next Start.
static CRASHED: std::sync::Mutex<Option<ferrocast_engine::studio_host::Recovered>> = std::sync::Mutex::new(None);
/// Set when Ferrocast quits normally, so the note isn't written again on the way out.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the note holds: what's on now (`None`: the Studio isn't running).
fn session_now(h: &Held) -> Option<ferrocast_engine::studio_host::Recovered> {
    h.collection.as_ref()?;
    Some(ferrocast_engine::studio_host::Recovered {
        record: h.record.is_some(),
        replay: h.replay.is_some(),
        live: h.stream.is_some(),
        ..Default::default()
    })
}

/// Writes the note (or removes it when the Studio isn't running).
fn write_session(now: Option<&ferrocast_engine::studio_host::Recovered>) {
    let Some(file) = SESSION.get() else { return };
    if QUITTING.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    match now {
        Some(s) => save_json(file, &serde_json::to_value(s).unwrap_or(Value::Null)),
        None => {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// Ferrocast is quitting normally: the note goes, so the next launch isn't taken for a crash.
pub fn end_session() {
    QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(file) = SESSION.get() {
        let _ = std::fs::remove_file(file);
    }
}

/// What live captions last heard, as the "Captions" text source shows it.
static HEARD: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// What's being said now (this sentence, as heard so far) and its number, for voice commands.
static SENTENCE: std::sync::Mutex<(u64, String)> = std::sync::Mutex::new((0, String::new()));
/// Sentences finished lately (number, when in Unix ms, words), for the transcript.
static DONE: std::sync::Mutex<std::collections::VecDeque<(u64, i64, String)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
/// Finished sentences kept for the engine to pick up (it looks every 0.7 s).
const DONE_KEPT: usize = 50;

/// Why captions last stopped by themselves, until they're turned on again.
static CAPTIONS_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Where words heard go, for one run of captions.
type Sink = (u64, Box<dyn Fn(String, bool)>);

thread_local! {
    /// Where Whisper's words go on the main thread, for the run listening now.
    static CAPTION_SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Captions from macOS's recognizer or from Whisper.
enum Listening {
    Apple(ferrocast_studio::Captions),
    Whisper { _tap: ferrocast_studio::SoundTap, listener: crate::whisper::WhisperListener },
}

impl Listening {
    fn engine(&self) -> &'static str {
        match self {
            Listening::Apple(_) => "apple",
            Listening::Whisper { .. } => "whisper",
        }
    }

    /// Why it stopped by itself, if it did.
    fn stopped(&self) -> Option<String> {
        match self {
            Listening::Apple(c) => {
                (!c.working()).then(|| "Speech recognition kept failing, so captions stopped.".to_string())
            }
            Listening::Whisper { listener, .. } => listener.failed(),
        }
    }
}

/// What to do with words heard (on the main thread): count "clip that", keep the end of what
/// was said, and write it into the "Captions" text source.
fn caption_sink() -> impl Fn(String, bool) + 'static {
    let done = RefCell::new(String::new());
    // "clip that"s already counted in this sentence (partial results repeat it).
    let counted = std::cell::Cell::new(0usize);
    move |said, last| {
        let calls = clip_calls(&said);
        if calls > counted.get() {
            CLIPS_SAID.fetch_add((calls - counted.get()) as u64, std::sync::atomic::Ordering::Relaxed);
            counted.set(calls);
        }
        if last {
            counted.set(0);
        }
        if let Ok(mut s) = SENTENCE.lock() {
            if last {
                // Finished: kept for the transcript, and the next sentence gets the next number.
                let text = said.trim().to_string();
                if !text.is_empty()
                    && let Ok(mut d) = DONE.lock()
                {
                    d.push_back((s.0, chrono::Utc::now().timestamp_millis(), text));
                    while d.len() > DONE_KEPT {
                        d.pop_front();
                    }
                }
                *s = (s.0 + 1, String::new());
            } else {
                s.1 = said.clone();
            }
        }
        let shown = caption_tail(&format!("{} {said}", done.borrow()), CAPTION_CHARS);
        if last {
            *done.borrow_mut() = shown.clone();
        }
        // Never inside a Studio call: macOS's handlers and run_on_main_thread come between them.
        HELD.with(|h| {
            if let Ok(h) = h.try_borrow()
                && let Some(c) = &h.collection
            {
                // Without a "Captions" text source there's nothing to write.
                let _ = c.update(CAPTIONS_SOURCE, &json!({ "text": shown }));
            }
        });
        if let Ok(mut heard) = HEARD.lock() {
            *heard = shown;
        }
    }
}

/// Times "clip that" was said while captions listened (the engine clips when it goes up).
static CLIPS_SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times `said` asks for a clip ("clip that", "clip it"), any case or punctuation.
fn clip_calls(said: &str) -> usize {
    let words: Vec<String> = said
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.windows(2).filter(|w| w[0] == "clip" && (w[1] == "that" || w[1] == "it")).count()
}

/// The text source live captions write into, when the scenes have one.
const CAPTIONS_SOURCE: &str = "Captions";

/// About two short lines of captions.
const CAPTION_CHARS: usize = 90;

/// The end of `said` that fits a captions box: at most `max` characters, starting at a word.
fn caption_tail(said: &str, max: usize) -> String {
    let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = said.chars().count();
    if n <= max {
        return said;
    }
    let cut: String = said.chars().skip(n - max).collect();
    // Start at the next whole word, unless the last word alone is longer than the box.
    match cut.find(' ') {
        Some(i) if i + 1 < cut.len() => cut[i + 1..].to_string(),
        _ => cut,
    }
}

fn set_remux(v: Value) {
    *REMUX.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
}

/// The MP4 a remux of `from` makes: beside it, " (2)"… when one of that name is there.
fn mp4_beside(from: &std::path::Path) -> PathBuf {
    let stem = from.file_stem().and_then(|s| s.to_str()).unwrap_or("Recording").to_string();
    let folder = from.parent().map(PathBuf::from).unwrap_or_default();
    let mut to = folder.join(format!("{stem}.mp4"));
    let mut n = 2;
    while to.exists() || to == from {
        to = folder.join(format!("{stem} ({n}).mp4"));
        n += 1;
    }
    to
}

/// Remuxes each file to an MP4 beside it, one after another on a thread of its own (a long
/// file needn't hold the app up) → the first new file.
fn remux_in_background(remuxer: ferrocast_studio::Remuxer, files: Vec<PathBuf>) -> Result<PathBuf, String> {
    if REMUX.lock().unwrap_or_else(|p| p.into_inner()).as_ref().is_some_and(|v| v["done"] == false) {
        return Err("A remux is already running; wait for it to end.".into());
    }
    let first = files.first().map(|f| mp4_beside(f)).ok_or("Nothing to remux.")?;
    let (f, t) = (files[0].display().to_string(), first.display().to_string());
    set_remux(json!({ "from": f, "to": t, "percent": 0.0, "done": false, "error": null }));
    std::thread::spawn(move || {
        let count = files.len();
        let mut last = json!(null);
        for (i, from) in files.into_iter().enumerate() {
            let to = mp4_beside(&from);
            let (f, t) = (from.display().to_string(), to.display().to_string());
            let out = remuxer.remux(&from, &to, |pct| {
                // Across all files: each is its share of the whole.
                let whole = (i as f32 * 100.0 + pct) / count as f32;
                set_remux(json!({ "from": f, "to": t, "percent": whole, "done": false, "error": null }));
                true
            });
            let error = out.err().map(|e| e.to_string());
            match &error {
                Some(e) => tracing::warn!(file = %from.display(), error = %e, "a remux failed"),
                None => tracing::info!(file = %to.display(), "remuxed to MP4"),
            }
            last = json!({ "from": f, "to": t, "percent": 100.0, "done": true, "error": error });
            if error.is_some() {
                break;
            }
        }
        set_remux(last);
    });
    Ok(first)
}

/// The parts of a recording that started at `since` in `first`'s folder (a split makes more),
/// oldest first.
fn recording_parts(first: &std::path::Path, since: std::time::SystemTime) -> Vec<PathBuf> {
    let ext = first.extension().map(|e| e.to_owned());
    let mut parts: Vec<(std::time::SystemTime, PathBuf)> = first
        .parent()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e.to_owned()) == ext)
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?;
            let at = m.created().or_else(|_| m.modified()).ok()?;
            (at >= since || p == first).then_some((at, p))
        })
        .collect();
    parts.sort();
    parts.into_iter().map(|(_, p)| p).collect()
}

fn record_stats(r: &(Recording, PathBuf, std::time::Instant)) -> Value {
    let mut v = serde_json::to_value(r.0.stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("file".into(), Value::String(r.1.display().to_string()));
        o.insert("seconds".into(), json!(r.2.elapsed().as_secs()));
        o.insert("error".into(), r.0.last_error().map_or(Value::Null, Value::String));
    }
    v
}

fn stats_of(s: &Streaming) -> Value {
    let mut v = serde_json::to_value(s.stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("error".into(), s.last_error().map_or(Value::Null, Value::String));
    }
    v
}

/// The scenes a new collection starts with, empty: the dashboard shows what to add.
const FIRST_SCENE: &str = "Main";
/// The collection's name in its file (OBS shows it when the file is imported there).
const COLLECTION: &str = "Ferrocast";

fn web_act(w: ferrocast_engine::studio_host::WebAct) -> ferrocast_studio::WebAct {
    use ferrocast_engine::studio_host::WebAct as W;
    match w {
        W::Interact => ferrocast_studio::WebAct::Interact,
        W::Stop => ferrocast_studio::WebAct::Stop,
        W::Reload => ferrocast_studio::WebAct::Reload,
    }
}

fn item_edit(e: Edit) -> ItemEdit {
    match e {
        Edit::Show => ItemEdit::Show,
        Edit::Hide => ItemEdit::Hide,
        Edit::Lock => ItemEdit::Lock,
        Edit::Unlock => ItemEdit::Unlock,
        Edit::Fit => ItemEdit::Fit,
        Edit::Remove => ItemEdit::Remove,
        Edit::Up => ItemEdit::Up,
        Edit::Down => ItemEdit::Down,
        Edit::Top => ItemEdit::Top,
        Edit::Bottom => ItemEdit::Bottom,
        Edit::Stretch => ItemEdit::Stretch,
        Edit::Center => ItemEdit::Center,
        Edit::Reset => ItemEdit::Reset,
        Edit::FlipH => ItemEdit::FlipH,
        Edit::FlipV => ItemEdit::FlipV,
        Edit::RotateCw => ItemEdit::RotateCw,
        Edit::RotateCcw => ItemEdit::RotateCcw,
        Edit::Rotate180 => ItemEdit::Rotate180,
        Edit::Duplicate => ItemEdit::Duplicate,
        Edit::BlendNormal => ItemEdit::BlendNormal,
        Edit::BlendAdditive => ItemEdit::BlendAdditive,
        Edit::BlendSubtract => ItemEdit::BlendSubtract,
        Edit::BlendScreen => ItemEdit::BlendScreen,
        Edit::BlendMultiply => ItemEdit::BlendMultiply,
        Edit::BlendLighten => ItemEdit::BlendLighten,
        Edit::BlendDarken => ItemEdit::BlendDarken,
        Edit::BlendMethodDefault => ItemEdit::BlendMethodDefault,
        Edit::BlendMethodSrgbOff => ItemEdit::BlendMethodSrgbOff,
        Edit::ScaleOff => ItemEdit::ScaleOff,
        Edit::ScalePoint => ItemEdit::ScalePoint,
        Edit::ScaleBicubic => ItemEdit::ScaleBicubic,
        Edit::ScaleBilinear => ItemEdit::ScaleBilinear,
        Edit::ScaleLanczos => ItemEdit::ScaleLanczos,
        Edit::ScaleArea => ItemEdit::ScaleArea,
    }
}

macro_rules! to_json {
    ($v:expr) => {
        serde_json::to_value($v).map_err(|e| e.to_string())
    };
}

/// Does one act on the collection → what it shows (null for a change).
fn run(c: &Collection, act: StudioAct) -> Result<Value, ferrocast_studio::StudioError> {
    use StudioAct as A;
    let done = |r: Result<(), ferrocast_studio::StudioError>| r.map(|()| Value::Null);
    let v = |x: Result<Value, String>| x.map_err(ferrocast_studio::StudioError::Failed);
    match act {
        A::Scenes => v(to_json!(json!({
            "scenes": c.scenes(),
            "current": c.current(),
            "transition": c.transition(),
            // Studio Mode's preview (null when it's off), its quick transitions and options.
            "preview": c.preview_scene(),
            "quick": c.quick_transitions(),
            "studio_options": c.studio_options(),
            "tbar": c.tbar_state(),
            "black": c.is_black(),
            "vertical": c.vertical(),
        }))),
        A::StartVertical { scene } => done(c.start_vertical(&scene)),
        A::StopVertical => {
            c.stop_vertical();
            Ok(Value::Null)
        }
        A::SetStudioOptions { options: o } => done(c.set_studio_options(StudioOptions {
            swap: o.swap,
            duplicate_scene: o.duplicate_scene,
            duplicate_sources: o.duplicate_sources,
            double_click: o.double_click,
        })),
        A::SwapScenes => done(c.swap_scenes()),
        A::MissingFiles => v(to_json!(c.missing_files())),
        A::AddGroup { scene, name } => v(to_json!(c.add_group(&scene, &name)?)),
        A::Ungroup { scene, id } => done(c.ungroup(&scene, id)),
        A::Thumbnail { scene, width } => Ok(json!({ "image": c.thumbnail(&scene, width)? })),
        A::GroupItems { scene, ids, name } => v(to_json!(c.group_together(&scene, &ids, &name)?)),
        A::ItemOrder { scene, id, to } => done(c.item_order(&scene, id, to)),
        A::SetGroup { scene, id, group } => done(c.set_group(&scene, id, group)),
        A::PasteCopy { scene, name } => v(to_json!(c.paste_copy(&scene, &name)?)),
        A::Media { name, what, seek_ms } => {
            use ferrocast_engine::studio_host::MediaAct as M;
            use ferrocast_studio::MediaAct as S;
            if let Some(ms) = seek_ms {
                c.media_seek(&name, ms)?;
            }
            let info = match what {
                None => c.media_state(&name)?,
                Some(w) => c.media(
                    &name,
                    match w {
                        M::Play => S::Play,
                        M::Pause => S::Pause,
                        M::PlayPause => S::PlayPause,
                        M::Restart => S::Restart,
                        M::Stop => S::Stop,
                        M::Next => S::Next,
                        M::Previous => S::Previous,
                    },
                )?,
            };
            v(to_json!(info))
        }
        A::ReplaceFile { source, parent, kind, key, path, to } => done(c.replace_file(
            &ferrocast_studio::MissingFile { source, parent, kind, key, label: String::new(), path },
            &to,
        )),
        A::SetItemTransition { scene, id, show, transition } => done(c.set_item_transition(
            &scene,
            id,
            show,
            transition.map(|t| TransitionInfo { kind: t.kind, ms: t.ms }).as_ref(),
        )),
        A::SetDeinterlace { name, mode, top_field_first } => done(c.set_deinterlace(&name, &mode, top_field_first)),
        A::FadeToBlack => Ok(json!({ "black": c.fade_to_black()? })),
        A::TBar { at, release } => v(to_json!(c.tbar(at, release)?)),
        A::StudioMode { on } => {
            c.set_studio_mode(on);
            Ok(Value::Null)
        }
        A::SetPreview { scene } => done(c.set_preview(&scene)),
        A::TransitionNow { quick } => {
            done(c.transition_now(quick.map(|t| TransitionInfo { kind: t.kind, ms: t.ms }).as_ref()))
        }
        A::SetQuickTransitions { list } => done(c.set_quick_transitions(
            &list.into_iter().map(|t| TransitionInfo { kind: t.kind, ms: t.ms }).collect::<Vec<_>>(),
        )),
        A::AddScene { name, vertical: false } => done(c.add_scene(&name)),
        A::AddScene { name, vertical: true } => done(c.add_vertical_scene(&name)),
        A::DuplicateScene { scene, name } => done(c.duplicate_scene(&scene, &name)),
        A::RemoveScene { scene } => done(c.remove_scene(&scene)),
        A::MoveScene { scene, to } => done(c.move_scene(&scene, to)),
        A::SetCurrent { scene } => done(c.set_current(&scene)),
        A::SetTransition { kind, ms } => done(c.set_transition(&TransitionInfo { kind, ms })),
        A::SetSceneTransition { scene, transition } => {
            done(c.set_scene_transition(&scene, transition.map(|t| TransitionInfo { kind: t.kind, ms: t.ms }).as_ref()))
        }
        A::Items { scene } => v(to_json!(c.items(&scene)?)),
        A::AddSource { scene, source, fit } => {
            let item = c.add_source(&scene, &source.kind, &source.name, &source.settings, fit)?;
            let settings = c.settings(&source.name)?;
            v(to_json!(json!({ "item": item, "settings": settings })))
        }
        A::AddExisting { scene, name } => v(to_json!(json!({ "item": c.add_existing(&scene, &name)? }))),
        A::EditItem { scene, id, edit } => done(c.edit_item(&scene, id, item_edit(edit))),
        A::Sources => v(to_json!(c.sources())),
        A::Rename { name, new_name } => done(c.rename(&name, &new_name)),
        A::Settings { name } => c.settings(&name),
        A::Options { name } => v(to_json!(c.options(&name)?)),
        A::Update { name, settings } => done(c.update(&name, &settings)),
        A::Audio { scene } => v(to_json!(c.audio(&scene)?)),
        A::SetAudio { name, change } => {
            if let Some(d) = change.volume_db {
                c.set_volume(&name, d)?;
            }
            if let Some(m) = change.muted {
                c.set_muted(&name, m)?;
            }
            if let Some(b) = change.balance {
                c.set_balance(&name, b)?;
            }
            if let Some(ms) = change.sync_ms {
                c.set_sync(&name, ms)?;
            }
            if let Some(m) = change.mono {
                c.set_mono(&name, m)?;
            }
            if let Some(t) = &change.tracks {
                c.set_tracks(&name, t)?;
            }
            if let Some(m) = change.monitor {
                let m = match m {
                    Hear::Off => Monitor::Off,
                    Hear::OnlyMe => Monitor::OnlyMe,
                    Hear::MeAndStream => Monitor::MeAndStream,
                };
                c.set_monitor(&name, m)?;
            }
            Ok(Value::Null)
        }
        A::SetChannel { channel, device } => {
            done(c.set_channel(channel, device.as_ref().map(|d| (d.kind.as_str(), d.name.as_str(), &d.settings))))
        }
        A::Levels { names } => v(to_json!(c.levels(&names))),
        A::Filters { name } => v(to_json!(c.filters(&name)?)),
        A::AddFilter { name, filter } => done(c.add_filter(&name, &filter.kind, &filter.name, &filter.settings)),
        A::RemoveFilter { name, filter } => done(c.remove_filter(&name, &filter)),
        A::PasteFilters { name, from } => done(c.paste_filters(&name, &from)),
        A::SetItemBox { scene, id, x, y, w, h, .. } => v(to_json!(c.set_item_box(&scene, id, x, y, w, h)?)),
        A::SetItemCrop { scene, id, crop, .. } => v(to_json!(c.set_item_crop(&scene, id, crop)?)),
        A::Transform { scene, id } => v(to_json!(c.transform(&scene, id)?)),
        A::TransitionSettings { kind } => Ok(c.transition_settings(&kind)),
        A::SetTransitionSettings { kind, settings } => done(c.set_transition_settings(&kind, &settings)),
        A::SetTransform { scene, id, transform: t } => {
            let t = ferrocast_studio::Transform {
                x: t.x,
                y: t.y,
                rotation: t.rotation,
                scale_x: t.scale_x,
                scale_y: t.scale_y,
                alignment: t.alignment,
                bounds: t.bounds,
                bounds_alignment: t.bounds_alignment,
                bounds_width: t.bounds_width,
                bounds_height: t.bounds_height,
                crop_left: t.crop_left,
                crop_top: t.crop_top,
                crop_right: t.crop_right,
                crop_bottom: t.crop_bottom,
                width: t.width,
                height: t.height,
            };
            v(to_json!(c.set_transform(&scene, id, &t)?))
        }
        // Handled in `act`, which holds the preview and the history.
        A::Select { .. } | A::Undo | A::Redo | A::History => Ok(Value::Null),
        A::Web { name, what } => done(c.web(&name, web_act(what))),
        // Handled in `act`, which knows where recordings go.
        A::Screenshot { .. } | A::ScreenshotProgram => Ok(Value::Null),
        A::EditFilter { name, filter, edit } => done(match edit {
            FilterEdit::Enable => c.set_filter_enabled(&name, &filter, true),
            FilterEdit::Disable => c.set_filter_enabled(&name, &filter, false),
            FilterEdit::Up => c.move_filter(&name, &filter, ItemEdit::Up),
            FilterEdit::Down => c.move_filter(&name, &filter, ItemEdit::Down),
            FilterEdit::Top => c.move_filter(&name, &filter, ItemEdit::Top),
            FilterEdit::Bottom => c.move_filter(&name, &filter, ItemEdit::Bottom),
        }),
        A::FilterSettings { name, filter } => c.filter_settings(&name, &filter),
        A::FilterOptions { name, filter } => v(to_json!(c.filter_options(&name, &filter)?)),
        A::UpdateFilter { name, filter, settings } => done(c.update_filter(&name, &filter, &settings)),
    }
}

/// Writes the collection next to the plugins' config, whole or not at all (a new file, then
/// renamed over the old one).
fn save(c: &Collection, file: &std::path::Path) {
    let doc = c.save(COLLECTION);
    let tmp = file.with_extension("json.tmp");
    // On a new install the collections folder isn't there yet (2.1 found every save failing).
    let folder = file.parent().map_or(Ok(()), std::fs::create_dir_all);
    let written = folder
        .and_then(|()| serde_json::to_vec_pretty(&doc).map_err(std::io::Error::other))
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, file));
    if let Err(e) = written {
        tracing::warn!(error = %e, "the Studio's scenes couldn't be saved");
    }
}

/// The preview shows Studio Mode's preview scene, or what's on air when it's off.
fn sync_views(h: &Held) {
    let want = h.collection.as_ref().and_then(Collection::preview_scene);
    if let Some(p) = &h.preview
        && p.shown_scene() != want
    {
        p.show_scene(want.as_deref());
    }
}

/// Makes Studio Mode's picture of what's on air at its last place (main thread).
fn make_program_view(app: &AppHandle, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.program_at else { return Ok(()) };
    let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
    let studio = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
    let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
    let p = studio.preview(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
    pass_input(&window, &p);
    // Made again (a new canvas size): the stage keeps the page's colour.
    if let Some(c) = h.program_bg {
        p.background(c);
    }
    p.set_holes(&h.program_holes);
    p.set_opacity(h.program_opacity.unwrap_or(1.0));
    p.set_shade(h.program_shade);
    h.program_view = Some(p);
    Ok(())
}

/// Makes the preview at its last place (main thread).
fn make_preview(app: &AppHandle, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.preview_at else { return Ok(()) };
    let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
    let studio = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
    let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
    let mut p = studio.preview(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
    pass_input(&window, &p);
    if let Some(c) = h.preview_canvas {
        p.set_area(rect, c, scale);
    }
    p.select(h.selected.as_ref().map(|(s, i)| (s.as_str(), i.as_slice())));
    p.frames(h.frames.as_deref());
    if let Some(c) = h.preview_bg {
        p.background(c);
    }
    p.set_holes(&h.preview_holes);
    p.set_opacity(h.preview_opacity.unwrap_or(1.0));
    p.set_shade(h.preview_shade);
    h.preview = Some(p);
    sync_views(h);
    Ok(())
}

const NO_OBS: &str = "The Studio's engine is missing from this copy of Ferrocast. Reinstall Ferrocast from ferrocast.clearcraftlabs.com (or install OBS 32 from obsproject.com), then try again.";

#[derive(Debug, Clone)]
pub struct DesktopStudio {
    app: AppHandle,
    data: PathBuf,
}

impl DesktopStudio {
    pub fn new(app: AppHandle, data: PathBuf) -> DesktopStudio {
        let file = data.join("studio").join("session.json");
        // A note left from last time: Ferrocast didn't close normally while the Studio ran.
        if let Some(r) = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<ferrocast_engine::studio_host::Recovered>(&b).ok())
        {
            tracing::warn!(?r, "Ferrocast didn't close normally last time; the next Start puts the Studio back");
            if let Ok(mut c) = CRASHED.lock() {
                *c = Some(r);
            }
        }
        let _ = std::fs::remove_file(&file);
        let _ = SESSION.set(file);
        let me = DesktopStudio { app, data };
        let watcher = DesktopStudio { app: me.app.clone(), data: me.data.clone() };
        tauri::async_runtime::spawn(async move { watcher.session_loop().await });
        me
    }

    /// Every 2 s, the note of what the Studio is doing, written when it changes.
    async fn session_loop(&self) {
        let mut last: Option<Option<ferrocast_engine::studio_host::Recovered>> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Ok(now) = self.on_main(|h| session_now(h)).await else { continue };
            if last.as_ref() != Some(&now) {
                write_session(now.as_ref());
                last = Some(now);
            }
        }
    }

    /// Runs `f` on the main thread with the libobs state and waits for its answer.
    async fn on_main<T: Send + 'static>(&self, f: impl FnOnce(&mut Held) -> T + Send + 'static) -> Result<T, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.app
            .run_on_main_thread(move || {
                let out = HELD.with(|h| f(&mut h.borrow_mut()));
                let _ = tx.send(out);
            })
            .map_err(|e| format!("The window isn't ready: {e}"))?;
        rx.await.map_err(|_| "The window closed.".to_string())
    }

    /// Puts collection or profile `id` in use. A running Studio swaps its scenes at once; a
    /// profile's settings are applied as Settings applies them (refused while live).
    async fn switch(&self, kind: Kind, id: &str) -> Result<(), String> {
        let data = self.data.clone();
        let mut lib = library::load(&data);
        if lib.shelf(kind).get(id).is_none() {
            return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
        }
        if lib.shelf(kind).current == id {
            return Ok(());
        }
        let from = lib.current_file(&data, kind);
        let before = lib.shelf(kind).current.clone();
        lib.shelf_mut(kind).current = id.to_string();
        let to = lib.current_file(&data, kind);
        match kind {
            Kind::Collections => {
                library::save(&data, &lib);
                let swapped = self.on_main(move |h| swap_collection(h, &from, &to)).await?;
                if let Err(e) = swapped {
                    lib.collections.current = before;
                    library::save(&data, &lib);
                    return Err(e);
                }
                Ok(())
            }
            Kind::Profiles => {
                let busy = self
                    .on_main(|h| {
                        h.stream.is_some() || h.record.is_some() || h.replay.is_some() || h.virtualcam.is_some()
                    })
                    .await?;
                if busy {
                    return Err("Stop streaming and recording before changing profiles.".into());
                }
                let next = std::fs::read(&to)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<StudioSettings>(&b).ok())
                    .filter(|s| s.check().is_ok())
                    .unwrap_or_default();
                library::save(&data, &lib);
                if let Err(e) = self.set_settings(next).await {
                    lib.profiles.current = before;
                    library::save(&data, &lib);
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    fn status_of(h: &Held) -> StudioStatus {
        let found = obs_paths(true).is_some();
        StudioStatus {
            available: found,
            running: h.collection.is_some(),
            version: h.collection.as_ref().and(h.studio.as_ref()).map(Studio::version),
            renderer: h.collection.as_ref().map(|_| h.renderer.to_string()),
            detail: (!found).then(|| NO_OBS.into()),
            recovered: None,
        }
    }
}

/// Where following stands → `{ on, scene, item, found, face, what }`.
fn follow_status(h: &Held) -> Value {
    match &h.follow {
        Some(f) => json!({
            "on": true, "scene": f.scene, "item": f.id,
            // `found`: what it follows was seen in the last look; `face`: a face was.
            "found": f.misses == 0, "face": f.misses == 0 && !f.action,
            "what": if f.action { "action" } else { "face" },
        }),
        None => json!({ "on": false, "scene": null, "item": null, "found": false, "face": false, "what": null }),
    }
}

impl DesktopStudio {
    /// Four times a second while `run` is the one following: a small picture on the main
    /// thread, faces found on another, then a step on the main thread. Ends when following
    /// stops or the item goes.
    async fn follow_loop(&self, run: u64) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let look = self
                .on_main(move |h| {
                    if h.follow_run != run {
                        return None;
                    }
                    let (Some(c), Some(f)) = (&h.collection, &h.follow) else { return None };
                    match c.follow_look(f) {
                        Ok(look) => Some(look.map(|l| (l, f.action))),
                        Err(_) => {
                            // The item or its scene went: stop (there's nothing to put back).
                            h.follow = None;
                            None
                        }
                    }
                })
                .await;
            let look = match look {
                Ok(Some(Some(look))) => look,
                Ok(Some(None)) => continue,
                _ => return,
            };
            let (look, action) = look;
            let face = tokio::task::spawn_blocking(move || {
                let find = if action { ferrocast_studio::find_busy } else { ferrocast_studio::find_faces };
                find(&look.rgba, look.width, look.height).first().copied()
            })
            .await
            .ok()
            .flatten();
            let go_on = self
                .on_main(move |h| {
                    if h.follow_run != run {
                        return false;
                    }
                    let Some(c) = h.collection.as_ref() else { return false };
                    let Some(f) = h.follow.as_mut() else { return false };
                    if c.follow_step(f, face).is_err() {
                        h.follow = None;
                        return false;
                    }
                    true
                })
                .await;
            if !matches!(go_on, Ok(true)) {
                return;
            }
        }
    }
}

#[async_trait::async_trait]
impl StudioHost for DesktopStudio {
    async fn status(&self) -> StudioStatus {
        self.on_main(|h| Self::status_of(h)).await.unwrap_or_default()
    }

    async fn start(&self) -> Result<StudioStatus, String> {
        let config = self.data.join("studio");
        let data = self.data.clone();
        self.on_main(move |h| {
            // Read here, on the main thread, so a switch can't slip in between (CodeRabbit).
            let file = scenes_file(&data);
            if h.collection.is_some() {
                return Ok(Self::status_of(h));
            }
            if h.studio.is_none() {
                let s = h.settings.get_or_insert_with(|| load_settings(&data)).clone();
                // FERROCAST_RENDERER (opengl or metal) overrides it for a test run.
                let want = std::env::var("FERROCAST_RENDERER")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| s.renderer.clone());
                let (paths, renderer) = obs_paths(want != "opengl").ok_or(NO_OBS)?;
                let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
                let layout = s.speakers.as_deref().and_then(ferrocast_studio::Surround::from_name);
                let audio = AudioSetup {
                    sample_rate: s.sample_rate,
                    stereo: s.stereo,
                    layout,
                    low_latency: s.low_latency_audio,
                };
                let studio = Studio::start_with(&paths, canvas, audio, &config).map_err(|e| e.to_string())?;
                tracing::info!(version = %studio.version(), renderer, "the Studio's engine loaded");
                studio.set_multiview_look(multiview_look(&s));
                if let Some((name, id)) = &s.monitoring
                    && let Err(e) = studio.set_monitoring_device(name, id)
                {
                    tracing::warn!(error = %e, "the listening device is gone; using the system's");
                }
                h.renderer = renderer;
                h.paths = Some(paths);
                h.audio = Some(audio);
                h.studio = Some(studio);
            }
            let studio = h.studio.as_ref().ok_or("The Studio's engine isn't loaded.")?;
            // The saved scenes, or one empty scene the first time. A file that doesn't read is
            // kept aside (never overwritten) and the Studio starts fresh.
            let saved = std::fs::read(&file).ok().map(|b| serde_json::from_slice::<Value>(&b));
            let collection = match saved {
                Some(Ok(doc)) => Collection::load(studio, &doc, FIRST_SCENE),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "the Studio's scenes didn't read; starting with a new set");
                    let _ = std::fs::rename(&file, file.with_extension("json.unreadable"));
                    Collection::new(studio, FIRST_SCENE)
                }
                None => Collection::new(studio, FIRST_SCENE),
            }
            .map_err(|e| e.to_string())?;
            save(&collection, &file);
            collection.set_true_peak(h.settings.as_ref().is_some_and(|s| s.true_peak));
            h.collection = Some(collection);
            write_session(session_now(h).as_ref());
            // The first Start after a crash says what was on, for the engine to put back.
            let recovered = CRASHED.lock().ok().and_then(|mut c| c.take());
            Ok(StudioStatus { recovered, ..Self::status_of(h) })
        })
        .await?
    }

    async fn stop(&self) -> Result<StudioStatus, String> {
        let data = self.data.clone();
        self.on_main(move |h| {
            let file = scenes_file(&data);
            // The stream, the recording, the preview and the scenes go (saved first); the engine
            // stays loaded for the next Start.
            h.vstream = None;
            h.stream = None;
            h.record = None;
            h.source_records.clear();
            h.replay = None;
            h.virtualcam = None;
            h.preview = None;
            h.preview_at = None;
            h.program_view = None;
            h.program_at = None;
            h.vertical = VerticalView::default();
            h.projectors.clear();
            h.history.clear();
            h.captions = None;
            if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
                c.stop_follow(&f);
            }
            if let Some(c) = h.collection.take() {
                save(&c, &file);
            }
            write_session(None);
            Self::status_of(h)
        })
        .await
    }

    async fn kinds(&self) -> Result<Value, String> {
        self.on_main(|h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            serde_json::to_value(s.kinds()).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn options(&self, kind: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            let opts = s.options(&kind).map_err(|e| e.to_string())?;
            serde_json::to_value(opts).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn program_view(&self, rect: Option<PreviewRect>) -> Result<(), String> {
        let app = self.app.clone();
        self.on_main(move |h| {
            let Some(r) = rect else {
                h.program_view = None;
                h.program_at = None;
                return Ok(());
            };
            let rect = Rect { x: r.x, y: r.y, w: r.w, h: r.h, vh: r.vh };
            let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
            let scale = window.scale_factor().unwrap_or(1.0);
            h.program_at = Some((rect, scale));
            if let Some(p) = h.program_view.as_mut() {
                p.set_rect(rect, scale);
            } else {
                make_program_view(&app, h)?;
            }
            h.program_holes = r.holes.iter().map(|c| (c.x, c.y, c.w, c.h, c.r)).collect();
            h.program_opacity = r.opacity;
            h.program_shade = r.shade.unwrap_or(0.0);
            if let Some(p) = h.program_view.as_ref() {
                p.set_holes(&h.program_holes);
                p.set_opacity(h.program_opacity.unwrap_or(1.0));
                p.set_shade(h.program_shade);
            }
            // The stage round it matches the page's theme.
            h.program_bg = r.bg_abgr();
            if let (Some(p), Some(c)) = (h.program_view.as_ref(), h.program_bg) {
                p.background(c);
            }
            Ok(())
        })
        .await?
    }

    async fn preview_cursor(&self, cursor: String) -> Result<(), String> {
        self.on_main(move |h| {
            for p in [h.preview.as_ref(), h.program_view.as_ref(), h.vertical.view.as_ref()].into_iter().flatten() {
                p.set_cursor(&cursor);
            }
        })
        .await
    }

    async fn vertical_view(&self, rect: Option<PreviewRect>) -> Result<(), String> {
        let app = self.app.clone();
        self.on_main(move |h| {
            let Some(r) = rect else {
                h.vertical.view = None;
                h.vertical.at = None;
                return Ok(());
            };
            let rect = Rect { x: r.x, y: r.y, w: r.w, h: r.h, vh: r.vh };
            let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
            let scale = window.scale_factor().unwrap_or(1.0);
            let v = &mut h.vertical;
            v.at = Some((rect, scale));
            v.canvas = r.canvas.map(|c| (c.x, c.y, c.w, c.h));
            v.bg = r.bg_abgr();
            v.holes = r.holes.iter().map(|c| (c.x, c.y, c.w, c.h, c.r)).collect();
            v.opacity = r.opacity;
            v.shade = r.shade.unwrap_or(0.0);
            let Some(p) = v.view.as_mut() else { return make_vertical_view(&app, h) };
            if let Some(bg) = v.bg {
                p.background(bg);
            }
            match v.canvas {
                Some(c) => p.set_area(rect, c, scale),
                None => p.set_rect(rect, scale),
            }
            p.set_holes(&v.holes);
            p.set_opacity(v.opacity.unwrap_or(1.0));
            p.set_shade(v.shade);
            Ok(())
        })
        .await?
    }

    async fn preview(&self, rect: Option<PreviewRect>) -> Result<(), String> {
        let app = self.app.clone();
        self.on_main(move |h| {
            let Some(r) = rect else {
                h.preview = None;
                h.preview_at = None;
                return Ok(());
            };
            let rect = Rect { x: r.x, y: r.y, w: r.w, h: r.h, vh: r.vh };
            let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
            let scale = window.scale_factor().unwrap_or(1.0);
            h.preview_at = Some((rect, scale));
            h.preview_canvas = r.canvas.map(|c| (c.x, c.y, c.w, c.h));
            h.preview_bg = r.bg_abgr();
            h.preview_holes = r.holes.iter().map(|c| (c.x, c.y, c.w, c.h, c.r)).collect();
            h.preview_opacity = r.opacity;
            h.preview_shade = r.shade.unwrap_or(0.0);
            if let Some(p) = h.preview.as_mut() {
                if let Some(c) = h.preview_bg {
                    p.background(c);
                }
                match h.preview_canvas {
                    Some(c) => p.set_area(rect, c, scale),
                    None => p.set_rect(rect, scale),
                }
                p.set_holes(&h.preview_holes);
                p.set_opacity(h.preview_opacity.unwrap_or(1.0));
                p.set_shade(h.preview_shade);
                return Ok(());
            }
            make_preview(&app, h)
        })
        .await?
    }

    async fn defaults(&self, kind: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let s = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            s.defaults(&kind).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn act(&self, act: StudioAct) -> Result<Value, String> {
        let data = self.data.clone();
        let app = self.app.clone();
        self.on_main(move |h| {
            let file = scenes_file(&data);
            // The Studio off: its scenes from the saved file, so the show, BRB and the scene
            // switcher can still be set up (owner, 30 Oct: "it's not showing my screens").
            if act == StudioAct::Scenes && h.collection.is_none() {
                let saved = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                return Ok(saved_scenes(saved.as_ref()));
            }
            if matches!(act, StudioAct::Screenshot { .. } | StudioAct::ScreenshotProgram) {
                // Next to the recordings, as OBS does.
                let folder = record_folder(h.settings.get_or_insert_with(|| load_settings(&data)));
                let c = h.collection.as_ref().ok_or(STOPPED)?;
                std::fs::create_dir_all(&folder).map_err(|e| format!("The screenshot folder couldn't be made: {e}"))?;
                let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
                // Two in the same second get their own files.
                let mut path = folder.join(format!("Screenshot {stamp}.png"));
                let mut n = 2;
                while path.exists() {
                    path = folder.join(format!("Screenshot {stamp} ({n}).png"));
                    n += 1;
                }
                match &act {
                    StudioAct::Screenshot { name } => c.screenshot(name, &path),
                    _ => c.screenshot_program(&path),
                }
                .map_err(|e| e.to_string())?;
                return Ok(json!({ "file": path.display().to_string() }));
            }
            if let StudioAct::Select { scene, id, frames, also } = &act {
                h.selected = scene.clone().zip(*id).map(|(s, id)| {
                    let mut ids = vec![id];
                    ids.extend(also.iter().filter(|a| **a != id));
                    (s, ids)
                });
                h.frames = if *frames { scene.clone() } else { None };
                tracing::debug!(selected = ?h.selected, "picked in the preview");
                // Each picture outlines only its own scenes' sources (the other finds no such scene).
                for p in h.preview.iter().chain(h.vertical.view.iter()) {
                    p.select(h.selected.as_ref().map(|(s, i)| (s.as_str(), i.as_slice())));
                    p.frames(h.frames.as_deref());
                }
                return Ok(Value::Null);
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let changes = act.changes();
            if act == StudioAct::History {
                let (undo, redo) = h.history.list();
                return Ok(json!({ "undo": undo, "redo": redo }));
            }
            if matches!(act, StudioAct::Undo | StudioAct::Redo) {
                let done = if act == StudioAct::Undo { h.history.undo(c) } else { h.history.redo(c) };
                // A scene that came back or went changes what the pictures show.
                sync_views(h);
                let done = done?;
                save(c, &file);
                refresh_projectors(h);
                let (undo, redo) = h.history.labels();
                return Ok(json!({ "done": done, "undo": undo, "redo": redo }));
            }
            let before = h.history.before(c, &act);
            let turns_vertical = matches!(act, StudioAct::StartVertical { .. } | StudioAct::StopVertical);
            if turns_vertical {
                // The preview goes before its canvas does; the vertical stream too when the
                // picture goes (turning it on when it's on changes nothing).
                h.vertical.view = None;
                if act == StudioAct::StopVertical {
                    h.vstream = None;
                    // A vertical item following a face goes back first (it's saved with the
                    // picture).
                    if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
                        c.stop_follow(&f);
                    }
                }
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let out = run(c, act).map_err(|e| e.to_string());
            if turns_vertical {
                let _ = make_vertical_view(&app, h);
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            if let Some(b) = before {
                h.history.after(c, b, &out);
            }
            sync_views(h);
            if changes && out.is_ok() {
                save(c, &file);
                refresh_projectors(h);
            }
            out
        })
        .await?
    }

    async fn settings(&self) -> StudioSettings {
        let data = self.data.clone();
        self.on_main(move |h| h.settings.get_or_insert_with(|| load_settings(&data)).clone()).await.unwrap_or_default()
    }

    async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String> {
        let data = self.data.clone();
        let app = self.app.clone();
        let keys = s.hotkeys.clone();
        let note = self
            .on_main(move |h| {
                let mut notes: Vec<String> = Vec::new();
                if let Some(studio) = h.studio.as_ref() {
                    let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
                    if canvas != studio.canvas() {
                        let renderer = h.paths.as_ref().map(|p| p.renderer.clone()).unwrap_or_default();
                        // Refused while live or recording: nothing is saved then.
                        studio.set_canvas(canvas, &renderer).map_err(|e| e.to_string())?;
                        // The preview was made for the old size; projectors close.
                        if h.preview.take().is_some() {
                            let _ = make_preview(&app, h);
                        }
                        if h.program_view.take().is_some() {
                            let _ = make_program_view(&app, h);
                        }
                        // Its frame rate follows the main picture's.
                        if h.vertical.view.take().is_some() {
                            let _ = make_vertical_view(&app, h);
                        }
                        h.projectors.clear();
                    }
                    let studio = h.studio.as_ref().ok_or("The Studio's engine isn't loaded.")?;
                    let (name, id) = s.monitoring.clone().unwrap_or(("Default".into(), "default".into()));
                    if studio.set_monitoring_device(&name, &id).is_err() {
                        notes.push(format!("{name} can't play sound; the system's device is used."));
                    }
                    if let Some(c) = &h.collection {
                        c.set_true_peak(s.true_peak);
                    }
                    studio.set_multiview_look(multiview_look(&s));
                    if h.audio.is_some_and(|a| {
                        a.sample_rate != s.sample_rate
                            || a.stereo != s.stereo
                            || a.layout != s.speakers.as_deref().and_then(ferrocast_studio::Surround::from_name)
                            || a.low_latency != s.low_latency_audio
                    }) {
                        notes.push("Sound changes apply after Ferrocast restarts.".into());
                    }
                    if h.studio.is_some() && cfg!(target_os = "macos") && h.renderer != s.renderer {
                        notes.push("The new way of drawing applies after Ferrocast restarts.".into());
                    }
                }
                // A new bitrate reaches the stream on air at once; the encoder, its size and the
                // rest wait for the next start.
                let was_kbps = h.settings.as_ref().map(|o| o.video_kbps);
                if was_kbps.is_some_and(|k| k != s.video_kbps) {
                    // The vertical picture's stream uses the same bitrate (CodeRabbit).
                    for st in [h.stream.as_mut(), h.vstream.as_mut()].into_iter().flatten() {
                        st.set_video_kbps(s.video_kbps);
                    }
                }
                save_json(&settings_file(&data), &serde_json::to_value(&s).unwrap_or(Value::Null));
                h.settings = Some(s);
                // Open multiviews take a new layout at once.
                refresh_projectors(h);
                Ok::<_, String>(notes)
            })
            .await??;
        let failed = crate::studio_keys::apply(&self.app, &keys);
        let mut all = note;
        if !failed.is_empty() {
            all.push(format!(
                "These shortcuts are used by another app or aren't keys this Mac knows: {}.",
                failed.join(", ")
            ));
        }
        Ok((!all.is_empty()).then(|| all.join(" ")))
    }

    async fn stats(&self) -> Result<Value, String> {
        let data = self.data.clone();
        self.on_main(move |h| {
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            // The page asks for stats every second, so scenes held for the grid's pictures
            // are let go soon after it closes.
            if let Some(c) = &h.collection {
                c.let_thumbs_go(false);
                // Sources added before their first frame get their snug fit now.
                c.settle_fits();
                // A captured window resized keeps its shape on the canvas (and stays so after
                // a restart).
                if c.follow_sizes() {
                    save(c, &scenes_file(&data));
                }
            }
            let disk_free = {
                let folder = record_folder(&h.settings.clone().unwrap_or_default());
                let at = folder.ancestors().find(|p| p.exists()).map(PathBuf::from).unwrap_or(folder);
                studio.free_disk_mb(&at)
            };
            Ok(json!({
                "engine": studio.stats(),
                // The scene on air, so the page notices switches made elsewhere (shortcuts,
                // a multiview click).
                "scene": h.collection.as_ref().map(|c| c.current()),
                // What Undo and Redo would do (labels for the buttons).
                "history": { "undo": h.history.labels().0, "redo": h.history.labels().1 },
                "canvas": studio.canvas(),
                "stream": h.stream.as_ref().map(stats_of),
                "record": h.record.as_ref().map(record_stats),
                // Sources recorded on their own.
                "source_records": h.source_records.iter().map(|(n, _, f)| json!({
                    "name": n,
                    "file": f.display().to_string(),
                })).collect::<Vec<_>>(),
                // Free space where recordings go (OBS's Stats shows it).
                "disk_free_mb": disk_free,
                "replay": h.replay.as_ref().map(|r| json!({
                    "active": r.active(),
                    "last": r.last_replay().map(|p| p.display().to_string()),
                    "error": r.last_error(),
                })),
                "virtualcam": h.virtualcam.as_ref().map(|c| json!({ "active": c.active() })),
                // Captures on air that show nothing (the black screen fixer).
                "captures": h.collection.as_ref().map(|c| c.capture_problems()).unwrap_or_default(),
                // The last remux (OBS's Remux, or MKV made into MP4 after recording).
                "remux": REMUX.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            }))
        })
        .await?
    }

    async fn record(&self, on: bool) -> Result<Value, String> {
        self.on_main(move |h| {
            if !on {
                // Waits for the file to be finished; MKV, FLV and TS then become MP4 too.
                let ended = h.record.take();
                let s = h.settings.clone().unwrap_or_default();
                if let (Some((rec, file, started)), Some(studio)) = (ended, h.studio.as_ref()) {
                    drop(rec);
                    // By the file's own type (the setting may have changed while recording).
                    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                    if s.record_remux && ["mkv", "flv", "ts"].contains(&ext.as_str()) {
                        // Every part, when it was split (a second's slack for the clock).
                        let since =
                            std::time::SystemTime::now() - started.elapsed() - std::time::Duration::from_secs(1);
                        let _ = remux_in_background(studio.remuxer(), recording_parts(&file, since));
                    }
                }
                return Ok(Value::Null);
            }
            if let Some(r) = h.record.as_ref().filter(|r| r.0.stats().active) {
                return Ok(record_stats(r));
            }
            h.record = None;
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            let s = h.settings.clone().unwrap_or_default();
            let folder = record_folder(&s);
            std::fs::create_dir_all(&folder)
                .map_err(|e| format!("The recordings folder {} can't be made: {e}", folder.display()))?;
            let plan = ferrocast_studio::RecordPlan {
                folder,
                name: s.record_name.clone(),
                format: s.record_format.clone(),
                spaces: s.record_spaces,
                overwrite: s.record_overwrite,
                split_minutes: s.record_split_minutes,
                split_mb: s.record_split_mb,
                tracks: s.record_tracks.clone(),
                encode: record_encode(studio, &s),
                muxer: s.record_muxer.clone(),
            };
            let file = studio.record_path(&plan);
            let rec = studio.record_to(&file, &plan).map_err(|e| e.to_string())?;
            tracing::info!(file = %file.display(), "the Studio is recording");
            let r = (rec, file, std::time::Instant::now());
            let v = record_stats(&r);
            h.record = Some(r);
            Ok(v)
        })
        .await?
    }

    async fn record_source(&self, name: String, on: bool) -> Result<Value, String> {
        self.on_main(move |h| {
            let at = h.source_records.iter().position(|(n, _, _)| *n == name);
            if !on {
                // Dropping it waits for the file to be finished.
                if let Some(i) = at {
                    h.source_records.remove(i);
                }
                return Ok(json!({ "file": null }));
            }
            if let Some(i) = at {
                if h.source_records[i].1.stats().active {
                    return Ok(json!({ "file": h.source_records[i].2.display().to_string() }));
                }
                // It stopped by itself (a full disk…): started again below.
                h.source_records.remove(i);
            }
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            let s = h.settings.clone().unwrap_or_default();
            let folder = record_folder(&s);
            std::fs::create_dir_all(&folder)
                .map_err(|e| format!("The recordings folder {} can't be made: {e}", folder.display()))?;
            // The usual name pattern, after the source's name ("Camera 2026-10-31 17-20-05").
            let plan = ferrocast_studio::RecordPlan {
                folder,
                // Characters no file name may hold (on any system) become dashes.
                name: format!(
                    "{} {}",
                    name.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-"),
                    s.record_name
                ),
                format: s.record_format.clone(),
                spaces: s.record_spaces,
                overwrite: s.record_overwrite,
                split_minutes: s.record_split_minutes,
                split_mb: s.record_split_mb,
                tracks: s.record_tracks.clone(),
                encode: record_encode(studio, &s),
                muxer: s.record_muxer.clone(),
            };
            let file = studio.record_path(&plan);
            let rec = studio.record_source_to(&name, &file, &plan).map_err(|e| e.to_string())?;
            tracing::info!(source = %name, file = %file.display(), "the Studio is recording a source on its own");
            let v = json!({ "file": file.display().to_string() });
            h.source_records.push((name, rec, file));
            Ok(v)
        })
        .await?
    }

    async fn replay(&self, on: bool) -> Result<Value, String> {
        self.on_main(move |h| {
            if !on {
                h.replay = None;
                return Ok(json!({ "active": false }));
            }
            if h.replay.as_ref().is_some_and(|r| r.active()) {
                return Ok(json!({ "active": true }));
            }
            h.replay = None;
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            let s = h.settings.clone().unwrap_or_default();
            let folder = record_folder(&s);
            std::fs::create_dir_all(&folder)
                .map_err(|e| format!("The recordings folder {} can't be made: {e}", folder.display()))?;
            let plan = ferrocast_studio::ReplayPlan {
                folder,
                name: format!("Replay {}", s.record_name),
                format: s.replay_format.clone(),
                spaces: s.record_spaces,
                seconds: s.replay_seconds,
                mb: s.replay_mb,
                tracks: s.record_tracks.clone(),
                encode: record_encode(studio, &s),
            };
            let rb = studio.replay_buffer(&plan).map_err(|e| e.to_string())?;
            tracing::info!(seconds = s.replay_seconds, "the Studio's replay buffer is on");
            h.replay = Some(rb);
            Ok(json!({ "active": true }))
        })
        .await?
    }

    async fn replay_save(&self) -> Result<Value, String> {
        self.on_main(|h| {
            let r = h.replay.as_ref().filter(|r| r.active()).ok_or("The replay buffer is off: turn it on first.")?;
            r.save().map_err(|e| e.to_string())?;
            // Written in the background: stats name it (`replay.last`) once it's there.
            Ok(json!({ "file": null }))
        })
        .await?
    }

    async fn virtual_camera(&self, on: bool, source: Option<String>) -> Result<Value, String> {
        self.on_main(move |h| {
            h.virtualcam = None;
            if !on {
                return Ok(json!({ "active": false }));
            }
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            let shows = source.map_or(ferrocast_studio::CameraShows::Program, ferrocast_studio::CameraShows::Source);
            let cam = studio.virtual_camera(&shows).map_err(|e| {
                let e = e.to_string();
                // macOS lets only OBS itself turn its camera on: say so plainly.
                if cfg!(target_os = "macos") && e.contains("not installed") {
                    "On a Mac the virtual camera works from OBS itself for now: macOS lets only OBS \
                     turn its camera on. Start the virtual camera in OBS instead."
                        .to_string()
                } else {
                    e
                }
            })?;
            h.virtualcam = Some(cam);
            Ok(json!({ "active": true }))
        })
        .await?
    }

    async fn captions(&self, set: Option<CaptionsSet>) -> Result<Value, String> {
        if let Some(set) = &set {
            set.check()?;
        }
        let app = self.app.clone();
        self.on_main(move |h| {
            // Captions that stopped by themselves (recognition kept failing, the model didn't
            // load) read as off, with why.
            if let Some((l, _)) = &h.captions
                && let Some(why) = l.stopped()
            {
                if let Ok(mut e) = CAPTIONS_ERROR.lock() {
                    *e = Some(why);
                }
                h.captions = None;
            }
            match set {
                Some(CaptionsSet { ask: true, .. }) => ferrocast_studio::ask_captions_access(),
                Some(CaptionsSet { on: false, .. }) => {
                    h.captions = None;
                    CAPTION_SINK.with(|s| s.borrow_mut().take());
                    if let Ok(mut heard) = HEARD.lock() {
                        heard.clear();
                    }
                }
                Some(set @ CaptionsSet { on: true, .. }) => {
                    let source = set.source.clone().unwrap_or_default().trim().to_string();
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    // The old listener goes first: one at a time.
                    h.captions = None;
                    h.captions_run += 1;
                    let run = h.captions_run;
                    if let Ok(mut e) = CAPTIONS_ERROR.lock() {
                        *e = None;
                    }
                    let listening = if set.engine.as_deref() == Some("whisper") {
                        let model = set.model_path.clone().ok_or("The speech model isn't downloaded yet.")?;
                        let tap = c.tap_sound(&source).map_err(|e| e.to_string())?;
                        CAPTION_SINK.with(|s| *s.borrow_mut() = Some((run, Box::new(caption_sink()))));
                        // Words come from the worker: they're shown on the main thread, and only
                        // while this run is the one listening.
                        let listener = crate::whisper::start(
                            tap.reader(),
                            tap.rate(),
                            crate::whisper::Options {
                                model,
                                language: set.language.clone().unwrap_or_else(|| "auto".into()),
                                translate: set.translate,
                            },
                            move |said, last| {
                                let _ = app.run_on_main_thread(move || {
                                    CAPTION_SINK.with(|s| {
                                        if let Some((r, f)) = s.borrow().as_ref()
                                            && *r == run
                                        {
                                            f(said, last);
                                        }
                                    });
                                });
                            },
                        );
                        Listening::Whisper { _tap: tap, listener }
                    } else {
                        Listening::Apple(c.start_captions(&source, caption_sink()).map_err(|e| e.to_string())?)
                    };
                    if let Ok(mut heard) = HEARD.lock() {
                        heard.clear();
                    }
                    h.captions = Some((listening, source));
                }
                None => {}
            }
            Ok(json!({
                "access": ferrocast_studio::captions_access(),
                "on": h.captions.is_some(),
                "engine": h.captions.as_ref().map(|(l, _)| l.engine()),
                "source": h.captions.as_ref().map(|(_, s)| s.clone()),
                "text": HEARD.lock().map(|t| t.clone()).unwrap_or_default(),
                "error": CAPTIONS_ERROR.lock().ok().and_then(|e| e.clone()),
                "clips_said": CLIPS_SAID.load(std::sync::atomic::Ordering::Relaxed),
                // For voice commands and the transcript (the engine reads these).
                "sentence": SENTENCE.lock().map(|s| json!({ "id": s.0, "said": s.1 })).unwrap_or(Value::Null),
                "done": DONE.lock().map(|d| d.iter().map(|(id, at, text)| json!({ "id": id, "at": at, "text": text })).collect::<Vec<_>>()).unwrap_or_default(),
                // Shown only when a scene has it (a removed one lingers for undo).
                "box": h.collection.as_ref().is_some_and(|c| {
                    c.sources().iter().any(|s| s.name == CAPTIONS_SOURCE && !s.scenes.is_empty())
                }),
            }))
        })
        .await?
    }

    async fn follow_face(&self, set: Option<FollowSet>) -> Result<Value, String> {
        if let Some(set) = &set {
            set.check()?;
        }
        let started = self
            .on_main(move |h| {
                let mut run = None;
                if let Some(set) = set {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    if let Some(f) = h.follow.take() {
                        c.stop_follow(&f);
                    }
                    h.follow_run += 1;
                    let action = set.what.as_deref() == Some("action");
                    if let (Some(scene), Some(item)) = (set.scene, set.item) {
                        if !cfg!(target_os = "macos") {
                            return Err("Following your face works on a Mac for now.".to_string());
                        }
                        if !c.vertical().is_some_and(|v| v.scenes.iter().any(|s| s.name == scene)) {
                            return Err(format!("There's no scene called \"{scene}\" in the vertical picture."));
                        }
                        h.follow = Some(c.start_follow(scene.trim(), item, action).map_err(|e| e.to_string())?);
                        run = Some(h.follow_run);
                    }
                }
                Ok((run, follow_status(h)))
            })
            .await??;
        if let (Some(run), status) = started {
            let me = DesktopStudio { app: self.app.clone(), data: self.data.clone() };
            tauri::async_runtime::spawn(async move { me.follow_loop(run).await });
            return Ok(status);
        }
        Ok(started.1)
    }

    async fn transcribe(
        &self,
        sound: Vec<f32>,
        how: ferrocast_engine::studio_host::Transcribe,
    ) -> Result<Vec<ferrocast_engine::studio_host::Said>, String> {
        tokio::task::spawn_blocking(move || crate::whisper::transcribe(&sound, &how))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn apps(&self, running: bool) -> Result<Value, String> {
        // AppKit answers on the main thread.
        self.on_main(move |_| {
            let list = if running { ferrocast_studio::running_apps() } else { Vec::new() };
            json!({ "front": ferrocast_studio::front_app(), "running": list })
        })
        .await
    }

    async fn record_split(&self) -> Result<(), String> {
        self.on_main(|h| {
            let r = h.record.as_ref().ok_or("Nothing is recording.")?;
            r.0.split().map_err(|e| e.to_string())
        })
        .await?
    }

    async fn record_chapter(&self, name: String) -> Result<(), String> {
        self.on_main(move |h| {
            let r = h.record.as_ref().ok_or("Nothing is recording.")?;
            r.0.chapter(&name).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn remux(&self, file: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let studio = h.studio.as_ref().ok_or(STOPPED)?;
            let from = PathBuf::from(&file);
            // Any part of the recording under way (a split writes more beside the first).
            if h.record
                .as_ref()
                .is_some_and(|r| r.1 == from || (r.1.parent() == from.parent() && r.1.extension() == from.extension()))
            {
                return Err("Wait until the recording ends to remux files of it.".to_string());
            }
            let to = remux_in_background(studio.remuxer(), vec![from])?;
            Ok(json!({ "to": to.display().to_string() }))
        })
        .await?
    }

    async fn find_missing(&self, folder: String) -> Result<Value, String> {
        let names: Vec<String> = self
            .on_main(|h| {
                h.collection
                    .as_ref()
                    .map(|c| c.missing_files().into_iter().map(|m| m.path).collect())
                    .unwrap_or_default()
            })
            .await?;
        // A folder walk: off the main thread.
        let found =
            tokio::task::spawn_blocking(move || ferrocast_studio::find_files(std::path::Path::new(&folder), &names))
                .await
                .map_err(|e| e.to_string())?;
        let found: serde_json::Map<String, Value> =
            found.into_iter().map(|(k, v)| (k, Value::String(v.display().to_string()))).collect();
        Ok(json!({ "found": found }))
    }

    async fn encoder_options(&self, id: String, settings: serde_json::Map<String, Value>) -> Result<Value, String> {
        self.on_main(move |h| {
            let studio = h.studio.as_ref().ok_or(STOPPED)?;
            let (options, values) = studio.encoder_options(&id, &settings).map_err(|e| e.to_string())?;
            Ok(json!({ "options": options, "values": values }))
        })
        .await?
    }

    async fn record_pause(&self, on: bool) -> Result<(), String> {
        self.on_main(move |h| {
            let r = h.record.as_ref().ok_or("Nothing is recording.")?;
            r.0.pause(on).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn monitoring(&self) -> Result<Value, String> {
        self.on_main(|h| {
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?;
            let devices: Vec<Value> =
                studio.monitoring_devices().into_iter().map(|(name, id)| json!({ "name": name, "id": id })).collect();
            let (name, id) = studio.monitoring_device();
            Ok(json!({ "devices": devices, "current": { "name": name, "id": id } }))
        })
        .await?
    }

    async fn obs_collections(&self) -> Result<Value, String> {
        let Some(dir) = ferrocast_studio::import::obs_scenes_dir() else { return Ok(json!([])) };
        let list = tokio::task::spawn_blocking(move || ferrocast_studio::import::list(&dir))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    async fn import_obs(&self, id: String) -> Result<Value, String> {
        let dir =
            ferrocast_studio::import::obs_scenes_dir().ok_or("OBS's scene collections aren't on this computer.")?;
        let doc = ferrocast_studio::import::read(&dir, &id).map_err(|e| e.to_string())?;
        // A new scene collection of the same name (the Studio's own stay as they are).
        let name = doc.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
        self.shelf("collections".into(), ShelfAct::Import { name, doc }).await
    }

    async fn streamlabs_collections(&self) -> Result<Value, String> {
        let Some(dir) = ferrocast_studio::streamlabs::scenes_dir() else { return Ok(json!([])) };
        let list = tokio::task::spawn_blocking(move || ferrocast_studio::streamlabs::list(&dir))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    async fn import_streamlabs(&self, id: String) -> Result<Value, String> {
        let dir = ferrocast_studio::streamlabs::scenes_dir()
            .ok_or("Streamlabs' scene collections aren't on this computer.")?;
        let doc = ferrocast_studio::streamlabs::read(&dir, &id).map_err(|e| e.to_string())?;
        let name = doc.get("name").and_then(Value::as_str).unwrap_or("Streamlabs scenes").to_string();
        // From here on it's an OBS collection: the same import, web pages and missing kinds.
        self.shelf("collections".into(), ShelfAct::Import { name, doc }).await
    }

    async fn shelf(&self, kind: String, act: ShelfAct) -> Result<Value, String> {
        let kind = Kind::parse(&kind).ok_or("kind: collections or profiles.")?;
        let data = self.data.clone();
        let missing = |id: &str| format!("There's no {} with the id \"{id}\".", kind.noun());
        match act {
            ShelfAct::List => Ok(json!(library::load(&data).shelf(kind))),
            ShelfAct::Rename { id, name } => {
                let name = library::check_name(&name)?;
                let mut lib = library::load(&data);
                let shelf = lib.shelf_mut(kind);
                if shelf.list.iter().any(|e| e.id != id && e.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
                }
                let entry = shelf.list.iter_mut().find(|e| e.id == id).ok_or_else(|| missing(&id))?;
                entry.name = name;
                library::save(&data, &lib);
                Ok(Value::Null)
            }
            ShelfAct::Remove { id } => {
                let mut lib = library::load(&data);
                let shelf = lib.shelf_mut(kind);
                let entry = shelf.get(&id).ok_or_else(|| missing(&id))?.clone();
                if shelf.current == id {
                    return Err(format!("\"{}\" is in use now.", entry.name));
                }
                shelf.list.retain(|e| e.id != id);
                // Kept aside, dated, not deleted.
                let file = library::file(&data, kind, &id);
                if file.exists() {
                    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                    let _ = std::fs::rename(&file, file.with_extension(format!("json.removed-{stamp}")));
                }
                library::save(&data, &lib);
                Ok(Value::Null)
            }
            ShelfAct::Export { id } => {
                let lib = library::load(&data);
                let entry = lib.shelf(kind).get(&id).ok_or_else(|| missing(&id))?.clone();
                let file = library::file(&data, kind, &id);
                if kind == Kind::Collections && lib.collections.current == id {
                    // The one in use is saved as it is now.
                    let f = file.clone();
                    self.on_main(move |h| {
                        if let Some(c) = &h.collection {
                            save(c, &f);
                        }
                    })
                    .await?;
                }
                let doc = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                Ok(match (kind, doc) {
                    (_, Some(mut d)) => {
                        if kind == Kind::Collections
                            && let Some(o) = d.as_object_mut()
                        {
                            o.insert("name".into(), json!(entry.name));
                        }
                        d
                    }
                    (Kind::Collections, None) => json!({ "name": entry.name, "sources": [] }),
                    (Kind::Profiles, None) => {
                        serde_json::to_value(StudioSettings::default()).map_err(|e| e.to_string())?
                    }
                })
            }
            ShelfAct::New { name, copy } => {
                let name = library::check_name(&name)?;
                let mut lib = library::load(&data);
                if lib.shelf(kind).list.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
                }
                let id = lib.shelf(kind).new_id(&name);
                if copy {
                    let from = lib.current_file(&data, kind);
                    if kind == Kind::Collections {
                        let f = from.clone();
                        self.on_main(move |h| {
                            if let Some(c) = &h.collection {
                                save(c, &f);
                            }
                        })
                        .await?;
                    }
                    let to = library::file(&data, kind, &id);
                    if from.exists() {
                        if let Some(dir) = to.parent() {
                            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                        }
                        std::fs::copy(&from, &to).map_err(|e| format!("The copy couldn't be made: {e}"))?;
                    }
                }
                lib.shelf_mut(kind).list.push(library::Entry { id: id.clone(), name });
                library::save(&data, &lib);
                if let Err(e) = self.switch(kind, &id).await {
                    // Not half made: a retry with the same name works.
                    forget(&data, kind, &id);
                    return Err(e);
                }
                Ok(json!({ "id": id }))
            }
            ShelfAct::Switch { id } => {
                self.switch(kind, &id).await?;
                Ok(json!({ "id": id }))
            }
            ShelfAct::Import { name, mut doc } => {
                let name = library::check_name(&name)?;
                let mut lib = library::load(&data);
                let name = lib.collections.free_name(&name);
                let id = lib.collections.new_id(&name);
                // OBS's Browser sources become Web page sources here; note kinds this OBS lacks.
                let known = self
                    .on_main(|h| {
                        h.studio.as_ref().filter(|_| h.collection.is_some()).map(|s| {
                            s.kinds().inputs.into_iter().map(|k| k.id).collect::<std::collections::BTreeSet<_>>()
                        })
                    })
                    .await?;
                let web = cfg!(target_os = "macos").then_some(ferrocast_studio::WEB_PAGE_KIND);
                let mut report = ferrocast_studio::import::convert(&mut doc, web, known.as_ref());
                report.name = name.clone();
                if let Some(o) = doc.as_object_mut() {
                    o.insert("name".into(), json!(name));
                }
                let file = library::file(&data, Kind::Collections, &id);
                save_json(&file, &doc);
                lib.collections.list.push(library::Entry { id: id.clone(), name });
                library::save(&data, &lib);
                if let Err(e) = self.switch(Kind::Collections, &id).await {
                    forget(&data, Kind::Collections, &id);
                    return Err(e);
                }
                let mut v = serde_json::to_value(&report).map_err(|e| e.to_string())?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("id".into(), json!(id));
                }
                Ok(v)
            }
        }
    }

    async fn pick_file(&self, pick: FilePick) -> Result<Value, String> {
        use tauri_plugin_dialog::DialogExt;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut d = self.app.dialog().file();
        if let Some(w) = self.app.get_webview_window("main") {
            d = d.set_parent(&w);
        }
        for (name, exts) in pick.filters() {
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            d = d.add_filter(name, &exts);
        }
        let text =
            |p: tauri_plugin_dialog::FilePath| p.into_path().ok().map(|p| Value::String(p.display().to_string()));
        let one = move |p: Option<tauri_plugin_dialog::FilePath>| {
            let _ = tx.send(p.and_then(text).unwrap_or(Value::Null));
        };
        match pick.kind.as_str() {
            "save" => d.save_file(one),
            "folder" => d.pick_folder(one),
            "files" => {
                let (many_tx, many_rx) = tokio::sync::oneshot::channel();
                d.pick_files(move |ps| {
                    let v = ps.map_or(Value::Null, |ps| Value::Array(ps.into_iter().filter_map(text).collect()));
                    let _ = many_tx.send(v);
                });
                return many_rx.await.map_err(|_| "The file dialog closed without an answer.".to_string());
            }
            _ => d.pick_file(one),
        }
        rx.await.map_err(|_| "The file dialog closed without an answer.".to_string())
    }

    async fn log(&self, lines: usize) -> Result<Value, String> {
        let file = self.data.join("logs").join("ferrocast.log");
        let path = file.clone();
        let tail = tokio::task::spawn_blocking(move || log_tail(&path, lines))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("The log can't be read: {e}"))?;
        Ok(json!({ "file": file.display().to_string(), "lines": tail }))
    }

    async fn show_log(&self) -> Result<(), String> {
        use tauri_plugin_opener::OpenerExt;
        let file = self.data.join("logs").join("ferrocast.log");
        self.app.opener().reveal_item_in_dir(&file).map_err(|e| format!("The log folder didn't open: {e}"))
    }

    async fn web_window(&self, name: String, url: String) -> Result<(), String> {
        use std::hash::{Hash, Hasher};
        // One window per page, so it keeps its place (the window-state plugin keys on this).
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        let label = format!("web-{:016x}", h.finish());
        if let Some(w) = self.app.get_webview_window(&label) {
            let _ = w.unminimize();
            let _ = w.show();
            return w.set_focus().map_err(|e| e.to_string());
        }
        let page: tauri::Url = url.parse().map_err(|_| "That address isn't a web page.".to_string())?;
        let opener = self.app.clone();
        // A plain web page: no Ferrocast key and no app commands (remote pages get no IPC).
        // Links that want a new window go to the browser.
        tauri::WebviewWindowBuilder::new(&self.app, label, tauri::WebviewUrl::External(page))
            .title(name)
            .inner_size(420.0, 680.0)
            .min_inner_size(260.0, 200.0)
            .on_new_window(move |u, _features| {
                if crate::is_web(&u) {
                    crate::open_in_browser(&opener, &u);
                }
                tauri::webview::NewWindowResponse::Deny
            })
            .build()
            .map(|_| ())
            .map_err(|e| format!("The window didn't open: {e}"))
    }

    async fn screen_access(&self) -> Result<Value, String> {
        Ok(json!({ "granted": ferrocast_studio::screen_access() }))
    }

    async fn fix_screen_access(&self) -> Result<Value, String> {
        #[cfg(target_os = "macos")]
        {
            // System Settings can show Ferrocast switched on for an older build (each unsigned
            // update is a new app to macOS); forgetting that answer lets macOS ask this one.
            let id = self.app.config().identifier.clone();
            let reset = std::process::Command::new("/usr/bin/tccutil").args(["reset", "ScreenCapture", &id]).output();
            if let Err(e) = reset {
                tracing::warn!(error = %e, "tccutil couldn't run");
            }
            let granted = self.on_main(|_| ferrocast_studio::request_screen_access()).await?;
            Ok(json!({ "granted": granted }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(json!({ "granted": true }))
        }
    }

    async fn projector(&self, p: ProjectorReq) -> Result<Value, String> {
        self.on_main(move |h| {
            let studio = h.studio.as_ref().filter(|_| h.collection.is_some()).ok_or(STOPPED)?.clone();
            let scenes: Vec<String> =
                h.collection.as_ref().map(|c| c.scenes().into_iter().map(|s| s.name).collect()).unwrap_or_default();
            let of = match p.what.as_str() {
                "program" => ProjectorOf::Program,
                "multiview" => ProjectorOf::Multiview,
                _ => ProjectorOf::Source(p.name.clone().unwrap_or_default()),
            };
            h.last_projector += 1;
            let id = h.last_projector;
            // Closed by its own button or Esc: out of the list (and dropped) after the borrow.
            let on_close = Box::new(move || {
                let gone = HELD.with(|h| {
                    let mut h = h.try_borrow_mut().ok()?;
                    let i = h.projectors.iter().position(|(pid, _)| *pid == id)?;
                    Some(h.projectors.remove(i))
                });
                drop(gone);
            });
            // A click on a multiview scene puts it on air, through its transition; in Studio
            // Mode it goes to the preview instead, as in OBS.
            let on_pick = Box::new(|scene: String| {
                HELD.with(|h| {
                    let Ok(h) = h.try_borrow() else { return };
                    if let Some(c) = &h.collection {
                        let done =
                            if c.preview_scene().is_some() { c.set_preview(&scene) } else { c.set_current(&scene) };
                        if let Err(e) = done {
                            tracing::warn!(error = %e, "a multiview click couldn't switch scenes");
                        }
                        let names: Vec<String> = c.scenes().into_iter().map(|s| s.name).collect();
                        let preview = c.preview_scene();
                        for (_, p) in &h.projectors {
                            p.update(&names, preview.as_deref());
                        }
                    }
                });
            });
            let screen = p.screen.map(|s| s as usize);
            let made = studio.projector(&of, &scenes, screen, on_close, on_pick).map_err(|e| e.to_string())?;
            if p.on_top {
                made.set_on_top(true);
            }
            // A multiview shows Studio Mode's preview as soon as it opens.
            if let Some(c) = &h.collection {
                made.update(&scenes, c.preview_scene().as_deref());
            }
            h.projectors.push((id, made));
            Ok(json!({ "id": id }))
        })
        .await?
    }

    async fn close_projectors(&self) -> Result<(), String> {
        // Dropped on the main thread, where their windows live.
        self.on_main(|h| h.projectors.clear()).await
    }

    async fn screens(&self) -> Result<Value, String> {
        self.on_main(|_| serde_json::to_value(ferrocast_studio::screens()).map_err(|e| e.to_string())).await?
    }

    async fn restart(&self) -> Result<(), String> {
        let busy = self
            .on_main(|h| h.stream.is_some() || h.record.is_some() || h.replay.is_some() || h.virtualcam.is_some())
            .await?;
        if busy {
            return Err("Stop streaming and recording before Ferrocast opens again.".into());
        }
        let app = self.app.clone();
        // A moment for the answer to reach the page first.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            app.restart();
        });
        Ok(())
    }

    async fn stream_start(&self, server: String, key: String) -> Result<Value, String> {
        self.on_main(move |h| {
            let studio = h.collection.as_ref().and(h.studio.as_ref()).ok_or(STOPPED)?;
            if let Some(s) = h.stream.as_ref().filter(|s| s.stats().active) {
                return Ok(stats_of(s));
            }
            // A stopped or failed one goes first: one output at a time.
            h.stream = None;
            let s = h.settings.clone().unwrap_or_default();
            let settings = stream_settings(studio, &s);
            tracing::info!(encoder = %settings.encode.video_encoder, kbps = settings.encode.video_kbps, "the Studio starts streaming into Go live");
            let stream = studio.stream(&server, &key, &settings).map_err(|e| e.to_string())?;
            let v = stats_of(&stream);
            h.stream = Some(stream);
            Ok(v)
        })
        .await?
    }

    async fn stream_stop(&self) -> Result<(), String> {
        self.on_main(|h| {
            h.vstream = None;
            h.stream = None;
        })
        .await
    }

    async fn vertical_stream_stop(&self) -> Result<(), String> {
        self.on_main(|h| {
            h.vstream = None;
        })
        .await
    }

    async fn vertical_stream_start(&self, server: String, key: String) -> Result<bool, String> {
        self.on_main(move |h| {
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let studio = h.studio.as_ref().ok_or(STOPPED)?;
            if c.vertical().is_none() {
                return Ok(false);
            }
            if h.vstream.as_ref().is_some_and(|s| s.stats().active) {
                return Ok(true);
            }
            h.vstream = None;
            let s = h.settings.clone().unwrap_or_default();
            let settings = stream_settings(studio, &s);
            tracing::info!(
                kbps = settings.encode.video_kbps,
                "the Studio's vertical picture starts streaming into Go live"
            );
            h.vstream = Some(c.vertical_stream(&server, &key, &settings).map_err(|e| e.to_string())?);
            Ok(true)
        })
        .await?
    }

    async fn stream_stats(&self) -> Result<Value, String> {
        self.on_main(|h| h.stream.as_ref().map_or(Value::Null, stats_of)).await
    }

    async fn direct_start(&self, id: String, url: String, key: String) -> Result<(), String> {
        self.on_main(move |h| {
            let s = h.stream.as_mut().filter(|s| s.stats().active).ok_or("The Studio isn't streaming.")?;
            tracing::info!(dest = %id, "the Studio sends straight to a server (SRT, RIST or WebRTC)");
            s.add_direct(&id, &url, &key).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn direct_stop(&self, id: String) -> Result<(), String> {
        self.on_main(move |h| {
            if let Some(s) = h.stream.as_mut() {
                s.remove_direct(&id);
            }
        })
        .await
    }

    async fn direct_stats(&self, id: String) -> Result<Value, String> {
        self.on_main(move |h| h.stream.as_ref().and_then(|s| s.direct_stats(&id)).map_or(Value::Null, |st| json!(st)))
            .await
    }
}

/// The last `n` lines of a log file, reading only its end (logs grow to 5 MB): ANSI colour
/// codes out, invalid UTF-8 shown as replacement characters.
fn log_tail(path: &std::path::Path, n: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // About 400 bytes a line is plenty; never more than 2 MB.
    let want = (n as u64).saturating_mul(400).min(2 * 1024 * 1024).min(len);
    // One byte before where reading starts: a newline there means a whole first line.
    let start = len - want;
    let from_line = if start == 0 {
        true
    } else {
        f.seek(SeekFrom::Start(start - 1))?;
        let mut b = [0u8];
        f.read_exact(&mut b)?;
        b[0] == b'\n'
    };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(want as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut all: Vec<&str> = text.lines().collect();
    // Reading from the middle of a line: that part line is left out.
    if !from_line && !all.is_empty() {
        all.remove(0);
    }
    let from = all.len().saturating_sub(n);
    Ok(all[from..].iter().map(|l| strip_ansi(l)).collect())
}

/// Removes terminal colour codes (`ESC [ ... m`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn reads_the_end_of_the_log_without_colour_codes() {
        let dir = std::env::temp_dir().join(format!("fc-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ferrocast.log");
        let body: String = (1..=3000).map(|i| format!("\u{1b}[2mline\u{1b}[0m {i}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let tail = log_tail(&path, 3).unwrap();
        assert_eq!(tail, ["line 2998", "line 2999", "line 3000"]);
        assert_eq!(log_tail(&path, 5000).unwrap().len(), 3000);
        assert!(log_tail(&dir.join("missing.log"), 3).is_err());
        // Reading starts right after a newline: the first line read is whole and kept.
        let even = dir.join("even.log");
        std::fs::write(&even, format!("{}\n{}\n", "a".repeat(399), "b".repeat(399))).unwrap();
        assert_eq!(log_tail(&even, 1).unwrap(), ["b".repeat(399)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod saved_scene_tests {
    use super::saved_scenes;
    use serde_json::json;

    #[test]
    fn a_saved_collection_lists_its_scenes_in_order() {
        let doc = json!({ "current_scene": "Main", "scene_order": [{ "name": "Starting" }, { "name": "Main" }, { "name": "Ending" }] });
        let v = saved_scenes(Some(&doc));
        let names: Vec<&str> = v["scenes"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Starting", "Main", "Ending"]);
        assert_eq!(v["current"], "Main");
        assert_eq!(v["saved"], true);
        // No file, or one without scenes: an empty list, never an error.
        assert_eq!(saved_scenes(None)["scenes"], json!([]));
        assert_eq!(saved_scenes(Some(&json!({ "scene_order": "nope" })))["scenes"], json!([]));
    }
}

#[cfg(test)]
mod caption_tests {
    use super::caption_tail;

    #[test]
    fn clip_that_is_heard_in_any_case_and_punctuation() {
        use super::clip_calls;
        assert_eq!(clip_calls("Oh wow, clip that!"), 1);
        assert_eq!(clip_calls("CLIP IT. clip that"), 2);
        assert_eq!(clip_calls("Clip... that"), 1);
        // Near misses aren't clips.
        assert_eq!(clip_calls("clipping that was fun"), 0);
        assert_eq!(clip_calls("that clip"), 0);
        assert_eq!(clip_calls("eclipse that"), 0);
        assert_eq!(clip_calls(""), 0);
    }

    #[test]
    fn short_captions_show_whole() {
        assert_eq!(caption_tail("  hello   there \n", 90), "hello there");
        assert_eq!(caption_tail("", 90), "");
    }

    #[test]
    fn long_captions_keep_the_end_from_a_whole_word() {
        let said = "one two three four five six seven eight nine ten";
        let t = caption_tail(said, 20);
        assert!(t.chars().count() <= 20, "{t}");
        assert!(said.ends_with(&t));
        assert!(said.contains(&format!(" {t}")), "starts at a word: {t}");
    }

    #[test]
    fn one_huge_word_is_cut_not_dropped() {
        let t = caption_tail(&"a".repeat(200), 90);
        assert_eq!(t.chars().count(), 90);
    }

    #[test]
    fn non_latin_text_is_cut_on_characters() {
        let said = "नमस्ते दोस्तों आज हम खेलेंगे ".repeat(10);
        let t = caption_tail(&said, 30);
        assert!(t.chars().count() <= 30);
    }
}
