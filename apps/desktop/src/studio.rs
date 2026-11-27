//! The Studio's engine in the desktop app (2.1, Track B): libobs runs on the main thread (the
//! preview is a native view in this window, which AppKit allows only there). The dashboard
//! reaches it through the engine's HTTP routes; this is the engine's `StudioHost`.
//!
//! For the 2.1 preview, OBS's engine comes from the OBS installed on this computer; a later
//! version ships it inside Ferrocast.

use std::cell::RefCell;
use std::path::PathBuf;

use ferrocast_engine::golive::{GROUP_LIGHT, GROUP_MAIN};
use ferrocast_engine::studio_host::{
    CaptionsSet, EncodeCanvas, EncodeCodec, EncodeGroupSpec, FilePick, FilterEdit, FollowSet, HostBudgetInfo, Hub,
    ItemEdit as Edit, Monitor as Hear, PreviewRect, ProjectorReq, STOPPED, ShelfAct, StreamCap, StudioAct, StudioHost,
    StudioSettings, StudioStatus,
};
// The Studio's scenes kept by Ferrocast's own engine (ENGINE.md step 3): no libobs.
use ferrocast_studio::stage::Stage as Collection;
use ferrocast_studio::stage::StageOutput as Streaming;
use ferrocast_studio::stage::StageOutput as Recording;
#[cfg(not(test))]
use ferrocast_studio::stage::StageView as Preview;
/// A projector window (Ferrocore draws them on a Mac and on Windows; none elsewhere yet).
#[cfg(all(not(test), any(target_os = "macos", target_os = "windows")))]
type ProjectorWin = ferrocast_studio::stage::StageProjector;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
type ProjectorWin = ();

#[derive(Clone, Copy)]
enum ViewCtx<'a> {
    #[cfg_attr(test, allow(dead_code))]
    App(&'a AppHandle),
    #[cfg(test)]
    Test,
}

#[cfg(test)]
type Holes = Vec<(f64, f64, f64, f64, f64)>;

#[cfg(test)]
#[derive(Debug)]
struct TestView {
    engine: ferrocast_studio::stage::EngineRef,
    rect: Rect,
    scale: f64,
    area: std::cell::RefCell<Option<(f64, f64, f64, f64)>>,
    vertical: bool,
    scene: std::cell::RefCell<Option<String>>,
    picked: std::cell::RefCell<Option<(String, Vec<i64>)>>,
    frames: std::cell::RefCell<Option<String>>,
    bg: std::cell::Cell<Option<u32>>,
    stripe: std::cell::Cell<Option<u32>>,
    holes: std::cell::RefCell<Holes>,
    opacity: std::cell::Cell<f64>,
    shade: std::cell::Cell<f64>,
    paused: std::cell::Cell<bool>,
}

#[cfg(test)]
#[derive(Debug)]
// Off macOS and Windows the real view is an empty stub, so the test variant is the big one.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(clippy::large_enum_variant))]
enum Preview {
    #[allow(dead_code)]
    Real(ferrocast_studio::stage::StageView),
    Test(TestView),
}

#[cfg(test)]
#[allow(dead_code)]
impl Preview {
    fn set_rect(&mut self, rect: Rect, scale: f64) {
        match self {
            Preview::Real(p) => p.set_rect(rect, scale),
            Preview::Test(p) => {
                p.rect = rect;
                p.scale = scale;
                *p.area.borrow_mut() = None;
            }
        }
    }

    fn set_area(&mut self, area: Rect, canvas: (f64, f64, f64, f64), scale: f64) {
        match self {
            Preview::Real(p) => p.set_area(area, canvas, scale),
            Preview::Test(p) => {
                p.rect = area;
                p.scale = scale;
                *p.area.borrow_mut() = Some(canvas);
            }
        }
    }

    fn show_scene(&self, scene: Option<&str>) {
        match self {
            Preview::Real(p) => p.show_scene(scene),
            Preview::Test(p) => *p.scene.borrow_mut() = scene.map(String::from),
        }
    }

    fn shown_scene(&self) -> Option<String> {
        match self {
            Preview::Real(p) => p.shown_scene(),
            Preview::Test(p) => p.scene.borrow().clone(),
        }
    }

    fn select(&self, pick: Option<(&str, &[i64])>) {
        match self {
            Preview::Real(p) => p.select(pick),
            Preview::Test(p) => *p.picked.borrow_mut() = pick.map(|(s, i)| (s.to_string(), i.to_vec())),
        }
    }

    fn frames(&self, scene: Option<&str>) {
        match self {
            Preview::Real(p) => p.frames(scene),
            Preview::Test(p) => *p.frames.borrow_mut() = scene.map(String::from),
        }
    }

    fn guides(&self, on: bool) {
        if let Preview::Real(p) = self {
            p.guides(on);
        }
    }

    fn background(&self, abgr: u32) {
        match self {
            Preview::Real(p) => p.background(abgr),
            Preview::Test(p) => p.bg.set(Some(abgr)),
        }
    }

    fn overflow_stripes(&self, abgr: Option<u32>) {
        match self {
            Preview::Real(p) => p.overflow_stripes(abgr),
            Preview::Test(p) => p.stripe.set(abgr),
        }
    }

    fn set_shade(&self, shade: f64) {
        match self {
            Preview::Real(p) => p.set_shade(shade),
            Preview::Test(p) => p.shade.set(shade),
        }
    }

    fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
        match self {
            Preview::Real(p) => p.set_holes(holes),
            Preview::Test(p) => *p.holes.borrow_mut() = holes.to_vec(),
        }
    }

    fn set_opacity(&self, opacity: f64) {
        match self {
            Preview::Real(p) => p.set_opacity(opacity),
            Preview::Test(p) => p.opacity.set(opacity),
        }
    }

    fn set_paused(&self, paused: bool) {
        match self {
            Preview::Real(p) => p.set_paused(paused),
            Preview::Test(p) => p.paused.set(paused),
        }
    }

    fn set_input(&self, cb: Box<dyn Fn(ferrocast_studio::PreviewInput)>) {
        if let Preview::Real(p) = self {
            p.set_input(cb);
        }
    }

    fn set_cursor(&self, css: &str) {
        if let Preview::Real(p) = self {
            p.set_cursor(css);
        }
    }

    fn follow(&self, stage: &Collection) {
        match self {
            Preview::Real(p) => p.follow(stage),
            Preview::Test(p) => {
                if !p.paused.get() && p.vertical {
                    *p.scene.borrow_mut() = stage.vertical().map(|v| v.current);
                }
            }
        }
    }

    fn engine_ref(&self) -> Option<ferrocast_studio::stage::EngineRef> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Preview::Real(p) => Some(p.engine_ref()),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Preview::Real(_) => None,
            Preview::Test(p) => Some(p.engine.clone()),
        }
    }

    fn test_view(&self) -> Option<&TestView> {
        match self {
            Preview::Real(_) => None,
            Preview::Test(p) => Some(p),
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
struct TestProjector {
    engine: ferrocast_studio::stage::EngineRef,
    what: String,
    name: Option<String>,
    names: std::cell::RefCell<Vec<String>>,
    preview: std::cell::RefCell<Option<String>>,
    look: std::cell::RefCell<ferrocast_studio::MultiviewLook>,
    on_top: std::cell::Cell<bool>,
    hide_cursor: std::cell::Cell<bool>,
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
enum ProjectorWin {
    #[allow(dead_code)]
    Real(ferrocast_studio::stage::StageProjector),
    Test(TestProjector),
}

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
#[allow(dead_code)]
impl ProjectorWin {
    fn update(&self, stage: &Collection, look: ferrocast_studio::MultiviewLook) {
        match self {
            ProjectorWin::Real(p) => p.update(stage, look),
            ProjectorWin::Test(p) => {
                if p.what == "multiview" {
                    *p.names.borrow_mut() = stage.scenes().into_iter().map(|s| s.name).collect();
                    *p.preview.borrow_mut() = stage.preview_scene();
                    *p.look.borrow_mut() = look;
                }
            }
        }
    }

    fn set_on_top(&self, on: bool) {
        match self {
            ProjectorWin::Real(p) => p.set_on_top(on),
            ProjectorWin::Test(p) => p.on_top.set(on),
        }
    }

    fn set_hide_cursor(&self, on: bool) {
        match self {
            ProjectorWin::Real(p) => p.set_hide_cursor(on),
            ProjectorWin::Test(p) => p.hide_cursor.set(on),
        }
    }

    fn engine_ref(&self) -> ferrocast_studio::stage::EngineRef {
        match self {
            ProjectorWin::Real(p) => p.engine_ref(),
            ProjectorWin::Test(p) => p.engine.clone(),
        }
    }

    fn test_proj(&self) -> Option<&TestProjector> {
        match self {
            ProjectorWin::Real(_) => None,
            ProjectorWin::Test(p) => Some(p),
        }
    }
}
use ferrocast_studio::{
    Canvas, EncodeSettings, ItemEdit, Monitor, Rect, StreamSettings, StudioOptions, TransitionInfo,
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
    preview: Option<Preview>,
    /// Where the preview is, to make it again when the canvas changes.
    preview_at: Option<(Rect, f64)>,
    stream: Option<Streaming>,
    /// Monotonic start time of the current main stream (for obs-websocket `GetStreamStatus`).
    stream_started: Option<std::time::Instant>,
    /// What the main stream was started with (a recording "Same as stream" may share its
    /// encoder: `can_share`).
    stream_with: Option<ferrocast_studio::StreamSettings>,
    /// What the main recording was started with, and whether its quality was "Same as stream"
    /// then (a stream may share its encoder; never by a choice saved since; CodeRabbit).
    record_with: Option<(ferrocast_studio::RecordPlan, bool)>,
    /// The vertical picture's stream, beside the main one.
    vstream: Option<Streaming>,
    /// The lighter copy of what's on air, for platforms that take it (720p).
    lstream: Option<Streaming>,
    /// Whether `lstream` was started/reshaped with `no_bframes` (WHIP simulcast).
    lstream_no_bframes: bool,
    /// Multi-encode group outputs keyed by group id (`"main"`, `"vertical"`, `"light"`, `"twitch_eb"`, `"g_<hash>"`).
    groups: std::collections::HashMap<String, GroupSlot>,
    record: Option<(Recording, PathBuf, std::time::Instant)>,
    /// Sources recorded on their own (Source Record): name, recording, file.
    source_records: Vec<(String, Recording, PathBuf)>,
    /// Guests here now (crates/guest): their name, their source, their connection.
    guests: Vec<(String, String, ferrocast_guest::Guest)>,
    replay: Option<ferrocast_studio::stage::StageOutput>,
    /// The last saved replay the Instant Replay sources were handed.
    replay_shown: Option<PathBuf>,
    /// While an instant replay plays in its scene: the scene to go back to, the replay's
    /// scene and source, and since when.
    replay_back: Option<(String, String, String, std::time::Instant)>,
    /// A replay handed to its source, waiting to play before its scene goes on air: the scene,
    /// the source, and since when.
    replay_wait: Option<(String, String, std::time::Instant)>,
    virtualcam: Option<ferrocast_studio::VirtualCamera>,
    /// What the virtual camera shows (None: what's on air): in the stats, and to start it again
    /// on a setup change.
    virtualcam_source: Option<String>,
    /// What's on air sent out as NDI.
    ndi: Option<ferrocast_studio::NdiOut>,
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
    /// The second colour of the stripes over what runs past the canvas (0xAABBGGRR).
    preview_stripe: Option<u32>,
    /// Where the page's menus and panels lie over the preview (points from its box's top left).
    preview_holes: Vec<(f64, f64, f64, f64, f64)>,
    /// How much of the preview shows, 0 to 1.
    preview_opacity: Option<f64>,
    /// Performance mode: the preview, Studio Mode's second picture and the vertical picture are
    /// not drawn (outputs and projectors are untouched). Kept here so a picture made again
    /// after a canvas change is paused too.
    preview_paused: bool,
    /// The dialog backdrop's shade over the preview, 0 to 1.
    preview_shade: f64,
    /// The same for Studio Mode's picture of what's on air.
    program_bg: Option<u32>,
    program_stripe: Option<u32>,
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
    projectors: Vec<(u64, ProjectorWin, ProjectorReq)>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
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
    /// Auto quality's share of the set bitrate on the stream now (None: all of it).
    live_pct: Option<u8>,
    /// Shared-stream ceiling pushed by Go live (Twitch 6,000 / Kick 8,000 kbps H.264 2 s).
    stream_cap: Option<StreamCap>,
    /// Kept LAST so Rust's implicit drop order always drops outputs before the collection.
    collection: Option<Collection>,
    /// Active collection generation bumped on every commit/stop so stale async opens drop safely.
    collection_gen: u64,
    /// True in Safe Mode when a temporary empty collection is running until the user confirms
    /// loading the saved collection; guards against overwriting the saved file on disk.
    collection_deferred: bool,
    /// Active collection ID & file path owned by the collection loaded in `Held`.
    collection_id: Option<String>,
    collection_file: Option<PathBuf>,
    /// Active profile ID & file path owned by the settings loaded in `Held`.
    profile_id: Option<String>,
    profile_file: Option<PathBuf>,
}

struct GroupSlot {
    spec: EncodeGroupSpec,
    output: Streaming,
    started: std::time::Instant,
    with: StreamSettings,
    live_pct: Option<u8>,
    cap_kbps: Option<u32>,
}

/// `kbps` at auto quality's share `pct` (the floor a stream never goes under: 300).
fn at_pct(kbps: u32, pct: Option<u8>) -> u32 {
    pct.map_or(kbps, |p| (kbps * u32::from(p.clamp(10, 100)) / 100).max(300))
}

/// Instant replay in a scene (OBS's Replay Source, built in), on the app's tick: a replay
/// just saved plays in every Instant Replay source; one that asked goes on air in its scene,
/// and when it ends (or after two minutes at most) the scene before comes back, unless
/// someone switched away meanwhile.
fn instant_replay(h: &mut Held) {
    let Some(c) = h.collection.as_ref() else { return };
    let last = h.replay.as_ref().and_then(|r| r.last_replay());
    if last.is_some() && last != h.replay_shown {
        h.replay_shown = last.clone();
        if let Some(file) = last {
            match c.show_replay(&file) {
                Ok(Some((source, scene))) => {
                    let back = c.current();
                    if let Some(rb) = h.replay_back.as_mut().filter(|rb| rb.1 == scene) {
                        // Another replay while one plays: its clock starts again.
                        rb.2 = source;
                        rb.3 = std::time::Instant::now();
                    } else if back != scene {
                        h.replay_wait = Some((scene, source, std::time::Instant::now()));
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("the instant replay couldn't play: {e}"),
            }
        }
    }
    // Its scene goes on air once the replay plays and its first picture is in, not before:
    // opening the file takes a moment (more on Windows, where FFmpeg looks at it first), and the
    // scene was black meanwhile (10 Nov: up to 10 s on stream). One that doesn't open in 10 s
    // doesn't go on air.
    if let Some((scene, source, since)) = h.replay_wait.as_ref() {
        let playing = c.media_state(source).is_ok_and(|m| m.state == "playing") && c.has_picture(source);
        if playing {
            let (scene, source) = (scene.clone(), source.clone());
            h.replay_wait = None;
            let back = c.current();
            if back != scene {
                match c.set_current(&scene) {
                    Ok(()) => {
                        // A Move transition is stepped like any scene change.
                        if c.moving() {
                            glide();
                        }
                        h.replay_back = Some((back, scene, source, std::time::Instant::now()));
                    }
                    Err(e) => tracing::warn!("the instant replay's scene couldn't go on air: {e}"),
                }
            }
        } else if since.elapsed() > std::time::Duration::from_secs(10) {
            tracing::warn!(source = %source, "the instant replay didn't open in 10 s: its scene stays off air");
            h.replay_wait = None;
        }
    }
    let Some((back, scene, source, since)) = h.replay_back.as_ref() else { return };
    let waited = since.elapsed();
    // A moment to open the file before its state means anything.
    if waited < std::time::Duration::from_millis(1500) {
        return;
    }
    // Not open yet is waiting, unless it never opens.
    let ended = match c.media_state(source) {
        Ok(m) => m.state == "ended" || m.state == "stopped",
        Err(_) => waited > std::time::Duration::from_secs(10),
    };
    if ended || waited > std::time::Duration::from_secs(120) {
        if c.current() == *scene {
            match c.set_current(back) {
                Ok(()) if c.moving() => glide(),
                Ok(()) => {}
                Err(e) => tracing::warn!("the scene before the instant replay couldn't come back: {e}"),
            }
        }
        h.replay_back = None;
    }
}

/// The vertical picture's preview and what the page last said about its box.
#[derive(Default)]
struct VerticalView {
    view: Option<Preview>,
    at: Option<(Rect, f64)>,
    canvas: Option<(f64, f64, f64, f64)>,
    bg: Option<u32>,
    stripe: Option<u32>,
    holes: Vec<(f64, f64, f64, f64, f64)>,
    opacity: Option<f64>,
    shade: f64,
}

fn create_stage_view(
    ctx: ViewCtx<'_>,
    c: &Collection,
    rect: Rect,
    scale: f64,
    vertical: bool,
) -> Result<Preview, String> {
    match ctx {
        ViewCtx::App(app) => {
            let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
            let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
            let p = if vertical {
                c.vertical_preview_drawn(handle.as_raw(), rect, scale)
            } else {
                c.preview_drawn(handle.as_raw(), rect, scale)
            }
            .map_err(|e| e.to_string())?;
            #[cfg(not(test))]
            let view = p;
            #[cfg(test)]
            let view = Preview::Real(p);
            pass_input(&window, &view);
            Ok(view)
        }
        #[cfg(test)]
        ViewCtx::Test => {
            let scene = if vertical {
                let v = c.vertical().ok_or_else(|| "The vertical picture is off.".to_string())?;
                Some(v.current)
            } else {
                None
            };
            let engine = c.engine_ref().map_err(|e| e.to_string())?;
            Ok(Preview::Test(TestView {
                engine,
                rect,
                scale,
                area: std::cell::RefCell::new(None),
                vertical,
                scene: std::cell::RefCell::new(scene),
                picked: std::cell::RefCell::new(None),
                frames: std::cell::RefCell::new(None),
                bg: std::cell::Cell::new(None),
                stripe: std::cell::Cell::new(None),
                holes: std::cell::RefCell::new(Vec::new()),
                opacity: std::cell::Cell::new(1.0),
                shade: std::cell::Cell::new(0.0),
                paused: std::cell::Cell::new(false),
            }))
        }
    }
}

/// Makes the vertical picture's preview at its last place, when the picture is on and the page
/// has a box for it (main thread).
fn make_vertical_view(ctx: ViewCtx<'_>, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.vertical.at else { return Ok(()) };
    let Some(c) = h.collection.as_ref() else { return Ok(()) };
    if c.vertical().is_none() {
        h.vertical.view = None;
        return Ok(());
    }
    if c.is_engine_opening() {
        return Ok(());
    }
    // Drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let mut p = create_stage_view(ctx, c, rect, scale, true)?;
    let v = &h.vertical;
    if let Some(area) = v.canvas {
        p.set_area(rect, area, scale);
    }
    p.select(h.selected.as_ref().map(|(s, i)| (s.as_str(), i.as_slice())));
    p.frames(h.frames.as_deref());
    if let Some(bg) = v.bg {
        p.background(bg);
    }
    p.overflow_stripes(v.stripe);
    p.set_holes(&v.holes);
    p.set_opacity(v.opacity.unwrap_or(1.0));
    p.set_shade(v.shade);
    p.set_paused(h.preview_paused);
    h.vertical.view = Some(p);
    Ok(())
}

/// How multiviews are laid out, from the settings.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
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

/// Multiviews follow the scenes, Studio Mode's preview and their settings (other projectors
/// follow by themselves; all open projectors take updated top-most and cursor settings).
fn refresh_projectors(h: &Held) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(c) = &h.collection {
        let settings = h.settings.clone().unwrap_or_default();
        let look = multiview_look(&settings);
        for (_, p, req) in &h.projectors {
            p.update(c, look);
            p.set_on_top(req.on_top || settings.projector_on_top);
            p.set_hide_cursor(settings.projector_hide_cursor);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = h;
}

/// Remove background's model on Windows (Robust Video Matting, 15 MB): used when it's here,
/// fetched when not (once per run, again after a failure), checked against its SHA-256.
#[cfg(windows)]
fn fetch_matte(data: std::path::PathBuf) {
    use std::sync::atomic::{AtomicBool, Ordering};

    use ferrocast_engine::speech_model::{MATTE, download_model, model_path};
    static ASKED: AtomicBool = AtomicBool::new(false);
    if ASKED.swap(true, Ordering::AcqRel) {
        return;
    }
    let path = model_path(&data, &MATTE);
    tauri::async_runtime::spawn(async move {
        // A model of one's own (FERROCAST_MATTE_MODEL) needs no download.
        let own = std::env::var_os("FERROCAST_MATTE_MODEL").is_some();
        if !own && !path.is_file() {
            if let Err(e) = download_model(&MATTE, &path, |_| {}).await {
                tracing::warn!(error = %e, "Remove background's model didn't download");
                ASKED.store(false, Ordering::Release);
                return;
            }
            tracing::info!("Remove background's model downloaded");
        }
        if !ferrocast_studio::use_matte_model(&path) {
            tracing::warn!("Remove background's model isn't usable");
        }
    });
}

thread_local! {
    static HELD: RefCell<Held> = RefCell::default();
}

const CAPTURE_RECORDING: u8 = 1 << 0;
const CAPTURE_REPLAY: u8 = 1 << 1;

static LOCAL_CAPTURE_FLAGS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn capture_flags_of(h: &Held) -> u8 {
    let rec = h.record.as_ref().is_some_and(|r| r.0.stats().active)
        || h.source_records.iter().any(|(_, r, _)| r.stats().active);
    let rep = h.replay.as_ref().is_some_and(|r| r.active());
    (if rec { CAPTURE_RECORDING } else { 0 }) | (if rep { CAPTURE_REPLAY } else { 0 })
}

fn sync_local_capture_flags(h: &Held) {
    LOCAL_CAPTURE_FLAGS.store(capture_flags_of(h), std::sync::atomic::Ordering::Release);
}

fn current_local_capture_block() -> ferrocast_engine::studio_host::LocalCaptureBlock {
    let flags = LOCAL_CAPTURE_FLAGS.load(std::sync::atomic::Ordering::Acquire);
    ferrocast_engine::studio_host::LocalCaptureBlock {
        recording: (flags & CAPTURE_RECORDING) != 0,
        replay: (flags & CAPTURE_REPLAY) != 0,
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
fn encode_of(s: &StudioSettings) -> EncodeSettings {
    // The Mac's VideoToolbox; elsewhere the graphics card's through FFmpeg.
    const HARDWARE: [&str; 2] = ["com.apple.videotoolbox.videoencoder.ave.avc", "ffmpeg_hardware"];
    let kinds = ferrocast_studio::kinds::kinds();
    let hardware = (s.encoder == "hardware")
        .then(|| HARDWARE.into_iter().find(|id| kinds.encoders.iter().any(|e| e.id == *id)))
        .flatten();
    // A particular encoder (HEVC, AV1…) when it's on this computer.
    let chosen = s.video_encoder.as_ref().filter(|id| kinds.encoders.iter().any(|e| &e.id == *id));
    EncodeSettings {
        video_encoder: chosen.cloned().unwrap_or_else(|| hardware.unwrap_or("obs_x264").into()),
        video_kbps: s.video_kbps,
        vbr: s.vbr,
        strict_cbr: false,
        quality: None,
        keyframe_s: s.keyframe_s,
        profile: s.profile.clone(),
        bframes: s.bframes,
        scale: s.scale.map(|[w, h]| (w, h)),
        scale_filter: s.scale_filter.clone(),
        audio_kbps: s.audio_kbps,
        extra: if chosen.is_some() || s.video_encoder.is_none() {
            s.encoder_settings.clone()
        } else {
            Default::default()
        },
    }
}

/// Whether any live output or capture prevents replacing the collection or reopening the canvas.
fn outputs_busy_for_switch(h: &Held) -> bool {
    any_stream_group_active(h)
        || h.record.as_ref().is_some_and(|r| r.0.stats().active)
        || h.source_records.iter().any(|(_, r, _)| r.stats().active)
        || h.replay.as_ref().is_some_and(|r| r.active())
        || h.virtualcam.is_some()
        || h.ndi.is_some()
}

/// Drops finished or errored output slots so no stale output keeps the previous engine alive
/// across a collection switch or canvas reopen.
fn drop_inactive_outputs(h: &mut Held) {
    if h.stream.as_ref().is_some_and(|s| !s.stats().active) {
        h.stream = None;
    }
    if h.vstream.as_ref().is_some_and(|s| !s.stats().active) {
        h.vstream = None;
    }
    if h.lstream.as_ref().is_some_and(|s| !s.stats().active) {
        h.lstream = None;
        h.lstream_no_bframes = false;
    }
    h.groups.retain(|_, g| g.output.stats().active);
    if h.stream.is_none() && h.groups.is_empty() {
        h.stream_started = None;
        h.stream_with = None;
        h.live_pct = None;
    }
    if h.record.as_ref().is_some_and(|r| !r.0.stats().active) {
        h.record = None;
        h.record_with = None;
    }
    h.source_records.retain(|(_, r, _)| r.stats().active);
    if h.replay.as_ref().is_some_and(|r| !r.active()) {
        h.replay = None;
        h.replay_shown = None;
        h.replay_back = None;
        h.replay_wait = None;
    }
}

fn take_projector_reqs(h: &mut Held) -> Vec<ProjectorReq> {
    h.projectors.drain(..).map(|(_, _, req)| req).collect()
}

/// Rebuilds cached preview, program, and vertical views plus open projector windows onto the
/// current collection and engine using their cached rects and look properties.
fn rebuild_views(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path, prev_projectors: Vec<ProjectorReq>) {
    if h.preview_at.is_some()
        && let Err(e) = make_preview(ctx, h)
    {
        tracing::warn!(error = %e, "the preview didn't rebuild");
    }
    if h.program_at.is_some()
        && let Err(e) = make_program_view(ctx, h)
    {
        tracing::warn!(error = %e, "the program view didn't rebuild");
    }
    if h.vertical.at.is_some()
        && let Err(e) = make_vertical_view(ctx, h)
    {
        tracing::warn!(error = %e, "the vertical preview didn't rebuild");
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if !prev_projectors.is_empty() {
        for req in prev_projectors {
            if let Err(e) = open_projector(ctx, h, &req, data) {
                tracing::warn!(error = %e, what = %req.what, "a projector didn't reopen");
            }
        }
        save_projectors(h, data);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = (data, prev_projectors);
}

fn finish_canvas_views(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    prev_projectors: Vec<ProjectorReq>,
    reopen_engine: bool,
) {
    rebuild_views(ctx, h, data, prev_projectors);
    if let Some(c) = &h.collection
        && reopen_engine
        && !c.has_engine()
        && let Err(e) = c.ensure_engine()
    {
        tracing::warn!(error = %e, "the engine didn't reopen after a canvas change");
    }
    sync_views(h);
}

fn try_apply_canvas(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    canvas: Canvas,
) -> Result<(), (String, Vec<ProjectorReq>, bool)> {
    let (old_canvas, had_engine) = {
        let Some(c) = h.collection.as_ref() else {
            return Err((STOPPED.into(), Vec::new(), false));
        };
        (c.canvas(), c.has_engine())
    };
    drop_inactive_outputs(h);
    let prev_projectors = take_projector_reqs(h);
    h.preview = None;
    h.program_view = None;
    h.vertical.view = None;
    // They send the old engine's picture: stopped (turned on again by hand).
    h.ndi = None;
    h.virtualcam = None;
    let changed = match h.collection.as_mut() {
        Some(c) => c.set_canvas(canvas),
        None => return Err((STOPPED.into(), prev_projectors, had_engine)),
    };
    if let Err(e) = changed {
        return Err((e.to_string(), prev_projectors, had_engine));
    }
    if u64::from(old_canvas.width) * u64::from(canvas.height) != u64::from(canvas.width) * u64::from(old_canvas.height)
    {
        // A different aspect ratio invalidates the cached preview canvas rect until the UI resizes.
        h.preview_canvas = None;
    }
    finish_canvas_views(ctx, h, data, prev_projectors, had_engine);
    Ok(())
}

/// Reopens the engine for `canvas` (or the current audio/NTSC format) after releasing all old
/// views and projectors, then rebuilds every cached view and open projector on the new engine.
fn apply_canvas(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path, canvas: Canvas) -> Result<(), String> {
    if let Err((e, prev_projectors, had_engine)) = try_apply_canvas(ctx, h, data, canvas) {
        finish_canvas_views(ctx, h, data, prev_projectors, had_engine);
        return Err(e);
    }
    Ok(())
}

fn apply_waiting_canvas_ctx(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path) {
    let Some(s) = h.settings.clone() else { return };
    let want = Canvas { width: s.width, height: s.height, fps: s.fps };
    if h.collection.as_ref().is_none_or(|c| c.canvas() == want) {
        return;
    }
    if outputs_busy_for_switch(h) {
        return;
    }
    if let Err(e) = apply_canvas(ctx, h, data, want) {
        tracing::warn!(error = %e, "the frame rate chosen while live couldn't be applied after the stream");
    }
}

/// A frame rate changed while live waited for the stream to end: the canvas takes it once
/// nothing that draws from it runs (called when a stream, recording or replay buffer stops, and
/// before a stream starts). The virtual camera and NDI would be stopped by it, so while one is
/// on it waits longer (the next stream still goes out at the chosen rate, see `stream_start`).
fn apply_waiting_canvas(app: &tauri::AppHandle, h: &mut Held, data: &std::path::Path) {
    apply_waiting_canvas_ctx(ViewCtx::App(app), h, data);
}

fn stop_source_record(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path, name: &str) -> Value {
    // Dropping it waits for the file to be finished.
    if let Some(i) = h.source_records.iter().position(|(n, _, _)| n == name) {
        h.source_records.remove(i);
    }
    apply_waiting_canvas_ctx(ctx, h, data);
    json!({ "file": null })
}

/// A Studio task the window's thread hasn't started in this long is given up on (the caller is
/// told the window is busy); one started this late is logged.
const MAIN_START_WITHIN: std::time::Duration = std::time::Duration::from_secs(15);
const MAIN_LATE: std::time::Duration = std::time::Duration::from_secs(2);

/// Encodes beside the stream and the recording that Go live or the replay buffer add.
#[derive(Debug, Clone, Copy, Default)]
struct Also {
    /// Twitch Enhanced Broadcasting is on for a ticked destination.
    enhanced: bool,
    /// A ticked destination takes the lighter stream (720p).
    light: bool,
    /// The replay buffer runs, or starts with the stream.
    replay: bool,
    /// Some source is shown only on the stream or only in the recording, so the recording can't
    /// share the stream's encode.
    selective: bool,
    /// Vertical picture (`width`, `height`) when active and included in budget estimates.
    vertical: Option<(u32, u32)>,
}

/// The lighter stream's size: 720 high at the canvas's shape (even sizes); None: the canvas
/// itself, when it's no taller.
fn light_size(width: u32, height: u32) -> Option<(u32, u32)> {
    (height > 720).then(|| {
        let w = (u64::from(width) * 720 / u64::from(height.max(1))) as u32;
        (w & !1, 720)
    })
}

/// Cross-OS capacity estimate when Apple Silicon engine count alone isn't used.
fn default_budget_mpx(
    os: &str,
    engines: Option<u32>,
    hw_backend: Option<&str>,
    cpu_cores: u32,
    encoder_id: &str,
) -> Option<f64> {
    let is_sw = ferrocast_engine::golive_plan::encoder_is_software(encoder_id);
    if is_sw {
        let cores = cpu_cores.max(1);
        return Some(if cores < 8 {
            125.0
        } else if cores < 12 {
            180.0
        } else {
            250.0
        });
    }
    if os == "macos" {
        return Some(engines.map_or(125.0, |n| f64::from(n) * ferrocast_studio::system::MPX_PER_ENGINE));
    }
    let _ = hw_backend;
    if matches!(os, "windows" | "linux") {
        return Some(250.0);
    }
    engines.map(|n| f64::from(n) * ferrocast_studio::system::MPX_PER_ENGINE)
}

#[cfg(test)]
fn encode_budget(s: &StudioSettings, cap: Option<StreamCap>, engines: Option<u32>, also: Also) -> Value {
    encode_budget_with_host(s, cap, engines, None, also)
}

fn encode_budget_with_host(
    s: &StudioSettings,
    cap: Option<StreamCap>,
    engines: Option<u32>,
    telem: Option<&ferrocast_studio::system::HardwareTelemetry>,
    also: Also,
) -> Value {
    let mpx = |w: u32, h: u32| f64::from(w) * f64::from(h) * f64::from(s.fps) / 1e6;
    let canvas = mpx(s.width, s.height);
    // Only the video encoder's own encodes count against HW unless stream uses software encoding.
    let hardware = |id: &str| !ferrocast_engine::golive_plan::encoder_is_software(id);
    let mut stream = stream_settings(s, cap);
    // Twitch Enhanced Broadcasting's qualities: Twitch's plan sets them when the stream starts;
    // about 720p and 480p beside the main one (projects-60).
    if also.enhanced {
        stream.video_tracks = vec![(1280, 720, 3000), (852, 480, 1500)];
    }
    let stream_sw = !hardware(&stream.encode.video_encoder);
    let mut stream_mpx = stream.encode.scale.map_or(canvas, |(w, h)| mpx(w, h));
    stream_mpx += stream.video_tracks.iter().map(|&(w, h, _)| mpx(w, h)).sum::<f64>();
    if let Some((vw, vh)) = also.vertical {
        stream_mpx += mpx(vw, vh);
    }
    if also.light {
        let (w, h) = light_size(s.width, s.height).unwrap_or((s.width, s.height));
        stream_mpx += mpx(w, h);
    }
    if stream_sw && telem.is_none() {
        stream_mpx = 0.0;
    }
    let plan = ferrocast_studio::RecordPlan {
        tracks: s.record_tracks.clone(),
        encode: record_encode_capped(s, cap),
        ..Default::default()
    };
    let shares =
        ferrocast_studio::stage::can_share(&stream, &plan, s.record_quality == "stream", s.fps, s.fps, also.selective)
            .map_err(|why| {
                // The hint belongs to the VOD track's own reason: turning it off brings nothing
                // back when something else is why.
                if stream.vod_track.is_some() && why.contains("VOD track") {
                    format!("{why} (turn the VOD track off when you're not on Twitch)")
                } else {
                    why
                }
            });
    let record_hw = hardware(&plan.encode.video_encoder);
    let mut record_mpx = if shares.is_ok() || (!record_hw && !(telem.is_some() && stream_sw)) { 0.0 } else { canvas };
    // The replay buffer is always an encode of its own (the recording's settings).
    if also.replay && (record_hw || (telem.is_some() && stream_sw)) {
        record_mpx += canvas;
    }
    let budget = match telem {
        Some(t) => {
            default_budget_mpx(&t.os, t.engines, t.hw_backend.as_deref(), t.cpu_cores, &stream.encode.video_encoder)
        }
        None => engines.map(|n| f64::from(n) * ferrocast_studio::system::MPX_PER_ENGINE),
    };
    let level = match budget {
        None => "unknown",
        Some(b) if stream_mpx + record_mpx > b * 1.0 => "over",
        Some(b) if stream_mpx + record_mpx > b * 0.9 => "limit",
        Some(_) => "ok",
    };
    json!({
        "replay": also.replay,
        "budget_mpx": budget.map(|b| b.round()),
        "stream_mpx": stream_mpx.round(),
        "record_mpx": record_mpx.round(),
        "shared": shares.is_ok(),
        "why_not_shared": shares.err(),
        "level": level,
    })
}

/// Replaces any HEVC/H.265 encoder id with its H.264 counterpart when a destination requires H.264.
fn h264_encoder_for(id: &str) -> String {
    let low = id.to_ascii_lowercase();
    if !low.contains("hevc") && !low.contains("h265") {
        return id.to_string();
    }
    if let Some(prefix) = id.strip_suffix(".hevc") {
        format!("{prefix}.avc")
    } else if cfg!(target_os = "macos") {
        "com.apple.videotoolbox.videoencoder.ave.avc".into()
    } else {
        "ffmpeg_hardware".into()
    }
}

/// Replaces any H.264 encoder id with its HEVC counterpart when an encode group asks for HEVC.
fn hevc_encoder_for(id: &str) -> String {
    let low = id.to_ascii_lowercase();
    if low.contains("hevc") || low.contains("h265") {
        return id.to_string();
    }
    if let Some(prefix) = id.strip_suffix(".avc") {
        format!("{prefix}.hevc")
    } else if let Some(suffix) = low.strip_prefix("h264_") {
        format!("hevc_{suffix}")
    } else if low.contains("nvenc") {
        "hevc_nvenc".into()
    } else if cfg!(target_os = "macos") {
        "com.apple.videotoolbox.videoencoder.ave.hevc".into()
    } else {
        id.to_string()
    }
}

/// Effective encode settings for the main stream after clamping to `cap` (if any) and `live_pct`.
fn effective_stream_encode(s: &StudioSettings, cap: Option<StreamCap>, live_pct: Option<u8>) -> EncodeSettings {
    let mut e = encode_of(s);
    if let Some(cap) = cap {
        if let Some(max_kbps) = cap.max_kbps {
            e.video_kbps = e.video_kbps.min(max_kbps);
            // Strict CBR with 1x VBV (FFmpeg) / DataRateLimits (VideoToolbox) prevents platform peaks.
            e.vbr = false;
            e.strict_cbr = true;
        }
        if cap.h264_only {
            e.video_encoder = h264_encoder_for(&e.video_encoder);
        }
        if cap.keyframe_secs > 0 {
            e.keyframe_s = cap.keyframe_secs;
        }
        if cap.no_bframes {
            e.bframes = false;
        }
    }
    e.video_kbps = at_pct(e.video_kbps, live_pct);
    e
}

fn light_stream_settings(s: &StudioSettings, canvas: Canvas, no_bframes: bool) -> StreamSettings {
    let mut settings = stream_settings(s, None);
    settings.encode.scale = light_size(canvas.width, canvas.height);
    settings.encode.video_kbps = settings.encode.video_kbps.min(3000);
    if no_bframes {
        settings.encode.bframes = false;
    }
    settings.vod_track = None;
    settings
}

fn shape_differs(a: &EncodeSettings, b: &EncodeSettings) -> bool {
    let normalize = |e: &EncodeSettings| EncodeSettings { video_kbps: 0, audio_kbps: 0, ..e.clone() };
    normalize(a) != normalize(b)
}

/// What the Studio streams into Go live with. Go live sends it on as it comes, so this is what
/// viewers get (capped when Twitch or Kick is active).
fn stream_settings(s: &StudioSettings, cap: Option<StreamCap>) -> StreamSettings {
    ferrocast_studio::set_delay_cap_mb(s.delay_max_mb);
    StreamSettings {
        encode: effective_stream_encode(s, cap, None),
        reconnect: s.reconnect.then_some((s.reconnect_tries, s.reconnect_every_s)),
        delay_s: s.delay_s,
        vod_track: (s.twitch_vod_track >= 2).then_some(s.twitch_vod_track),
        own_encoder: s.own_encoder,
        video_tracks: Vec::new(),
    }
}

/// Builds [`StreamSettings`] matching an [`EncodeGroupSpec`] without mutating saved Studio settings.
fn stream_settings_for_group(s: &StudioSettings, spec: &EncodeGroupSpec) -> StreamSettings {
    let mut settings = stream_settings(s, None);
    let mut enc = encode_of(s);
    enc.video_kbps = spec.key.kbps;
    if spec.key.strict_cbr {
        enc.vbr = false;
        enc.strict_cbr = true;
    }
    if spec.key.keyframe_s > 0 {
        enc.keyframe_s = spec.key.keyframe_s;
    }
    if spec.key.no_bframes {
        enc.bframes = false;
    }
    enc.audio_kbps = spec.key.audio_kbps;
    enc.video_encoder = match spec.key.codec {
        EncodeCodec::H264 => h264_encoder_for(&enc.video_encoder),
        EncodeCodec::Hevc => hevc_encoder_for(&enc.video_encoder),
        EncodeCodec::Av1 | EncodeCodec::Vp9 => enc.video_encoder,
    };
    let native = match spec.key.canvas {
        EncodeCanvas::Main => (s.width, s.height),
        EncodeCanvas::Vertical => ferrocast_studio::VERTICAL_SIZE,
    };
    enc.scale = ((spec.key.width, spec.key.height) != native).then_some((spec.key.width, spec.key.height));
    settings.encode = enc;
    settings.vod_track = if spec.vod_track { (s.twitch_vod_track >= 2).then_some(s.twitch_vod_track) } else { None };
    settings.video_tracks = spec.extra_tracks.clone();
    settings
}

fn any_main_stream_active(h: &Held) -> bool {
    h.stream.as_ref().is_some_and(|st| st.stats().active)
        || h.groups.values().any(|g| g.spec.key.canvas == EncodeCanvas::Main && g.output.stats().active)
}

fn any_stream_group_active(h: &Held) -> bool {
    h.stream.as_ref().is_some_and(|st| st.stats().active)
        || h.vstream.as_ref().is_some_and(|st| st.stats().active)
        || h.lstream.as_ref().is_some_and(|st| st.stats().active)
        || h.groups.values().any(|g| g.output.stats().active)
}

fn main_stream_stats(h: &Held) -> Option<(&Streaming, Option<std::time::Instant>)> {
    if let Some(s) = h.stream.as_ref() {
        return Some((s, h.stream_started));
    }
    if let Some(g) = h.groups.get(GROUP_MAIN) {
        return Some((&g.output, Some(g.started)));
    }
    h.groups
        .values()
        .find(|g| g.spec.key.canvas == EncodeCanvas::Main && g.output.stats().active)
        .map(|g| (&g.output, Some(g.started)))
        .or_else(|| h.vstream.as_ref().filter(|s| s.stats().active).map(|s| (s, h.stream_started)))
        .or_else(|| h.groups.values().find(|g| g.output.stats().active).map(|g| (&g.output, Some(g.started))))
        .or_else(|| h.lstream.as_ref().filter(|s| s.stats().active).map(|s| (s, h.stream_started)))
}

fn record_encode_for_group_spec(
    s: &StudioSettings,
    cap: Option<StreamCap>,
    main_spec: Option<&EncodeGroupSpec>,
) -> EncodeSettings {
    if s.record_quality == "stream"
        && let Some(spec) = main_spec
        && spec.extra_tracks.is_empty()
        && !spec.vod_track
        && spec.key.fps == s.fps
        && spec.key.fps_ntsc == s.fps_ntsc
    {
        let enc = stream_settings_for_group(s, spec).encode;
        if enc.scale.is_none() {
            return enc;
        }
    }
    record_encode_capped(s, cap)
}

fn record_encode_for_stream(
    s: &StudioSettings,
    cap: Option<StreamCap>,
    main_group: Option<&GroupSlot>,
) -> EncodeSettings {
    record_encode_for_group_spec(s, cap, main_group.filter(|g| g.output.stats().active).map(|g| &g.spec))
}

fn apply_main_slot_bitrate_on_spec(
    spec: &mut EncodeGroupSpec,
    cap_kbps: Option<u32>,
    live_pct: Option<u8>,
    video_kbps: u32,
) -> u32 {
    let base = cap_kbps.map_or(video_kbps, |c| video_kbps.min(c));
    spec.key.kbps = base;
    at_pct(base, live_pct)
}

fn main_slot_cap_kbps(spec: &EncodeGroupSpec, slot_cap: Option<u32>, was_video_kbps: Option<u32>) -> Option<u32> {
    (spec.id == GROUP_MAIN)
        .then(|| {
            slot_cap.or(spec.cap_kbps).or_else(|| was_video_kbps.filter(|&k| spec.key.kbps < k).map(|_| spec.key.kbps))
        })
        .flatten()
}

fn next_main_spec_for_settings(
    spec: &EncodeGroupSpec,
    s: &StudioSettings,
    cap_kbps: Option<u32>,
    was: Option<&StudioSettings>,
) -> EncodeGroupSpec {
    let mut next = spec.clone();
    if was.is_none_or(|o| spec.key.width == o.width && spec.key.height == o.height) {
        next.key.width = s.width;
        next.key.height = s.height;
    }
    if was.is_none_or(|o| spec.key.fps == o.fps && spec.key.fps_ntsc == o.fps_ntsc) || s.fps < spec.key.fps {
        next.key.fps = s.fps;
        next.key.fps_ntsc = s.fps_ntsc;
    }
    let effective_cap = main_slot_cap_kbps(spec, cap_kbps, was.map(|o| o.video_kbps));
    next.key.kbps = effective_cap.map_or(s.video_kbps, |c| s.video_kbps.min(c));
    next.cap_kbps = effective_cap;
    next
}

fn baseline_record_shares(s: &StudioSettings, cap: Option<StreamCap>, selective: bool) -> bool {
    let mut stream = stream_settings(s, cap);
    stream.vod_track = None;
    stream.video_tracks.clear();
    let plan = ferrocast_studio::RecordPlan {
        tracks: s.record_tracks.clone(),
        encode: record_encode_capped(s, cap),
        ..Default::default()
    };
    ferrocast_studio::stage::can_share(&stream, &plan, s.record_quality == "stream", s.fps, s.fps, selective).is_ok()
}

fn direct_uses_light_layer(simulcast: bool, lstream_active: bool, group_light_active: bool) -> bool {
    simulcast && (lstream_active || group_light_active)
}

fn main_stream_mut(h: &mut Held) -> Option<&mut Streaming> {
    if h.stream.as_ref().is_some_and(|s| s.stats().active) {
        return h.stream.as_mut();
    }
    if h.groups.get(GROUP_MAIN).is_some_and(|g| g.output.stats().active) {
        return h.groups.get_mut(GROUP_MAIN).map(|g| &mut g.output);
    }
    h.groups
        .values_mut()
        .find(|g| g.spec.key.canvas == EncodeCanvas::Main && g.output.stats().active)
        .map(|g| &mut g.output)
}

fn active_direct_stats(h: &Held, id: &str) -> Option<ferrocast_studio::StreamStats> {
    h.stream
        .as_ref()
        .and_then(|s| s.direct_stats(id))
        .or_else(|| h.groups.get(GROUP_MAIN).and_then(|g| g.output.direct_stats(id)))
        .or_else(|| h.groups.values().find_map(|g| g.output.direct_stats(id)))
}

/// What a recording uses: the stream's settings, or constant quality (high: near-lossless
/// with the same encoder; lossless: x264, much bigger files).
/// What a recording is encoded with: "Same as stream" is the stream's own encode, held to the
/// platform's limit when one is (so it can share the stream's encoder, in either order, and a
/// stream joining the recording's encoder is as strict as it asks); the others are as chosen.
fn record_encode_capped(s: &StudioSettings, cap: Option<StreamCap>) -> EncodeSettings {
    if s.record_quality == "stream"
        && let Some(cap) = cap
    {
        return EncodeSettings { scale: None, ..effective_stream_encode(s, Some(cap), None) };
    }
    record_encode(s)
}

fn record_encode(s: &StudioSettings) -> EncodeSettings {
    let mut e = encode_of(s);
    match s.record_quality.as_str() {
        "high" => e.quality = Some(16),
        // In between: libobs's 22 (x264 CRF 22; the Mac's matching 0-1 quality), about half of High's size.
        "balanced" => e.quality = Some(22),
        "lossless" => {
            e.video_encoder = "obs_x264".into();
            e.quality = Some(0);
            e.extra = Default::default();
        }
        // Its own encoder and options, as OBS's Advanced recording settings.
        "custom" => {
            let here = |id: &String| ferrocast_studio::kinds::kinds().encoders.iter().any(|k| &k.id == id);
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
    let named_transitions = ferrocast_studio::stage::saved_named_transitions(doc);
    let transition_matrix = ferrocast_studio::stage::saved_transition_matrix(doc);
    json!({
        "scenes": scenes,
        "current": doc.and_then(|d| d.get("current_scene")).and_then(Value::as_str).unwrap_or(""),
        // What a switch would use: the default named transition (the dashboard reads it).
        "transition": named_transitions
            .iter()
            .find(|t| t.default)
            .or(named_transitions.first())
            .map_or(json!({ "kind": "fade_transition", "ms": 300 }), |t| json!({ "kind": t.kind, "ms": t.ms })),
        "named_transitions": named_transitions,
        "transition_matrix": transition_matrix,
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

fn active_collection_file(h: &Held, data: &std::path::Path) -> PathBuf {
    h.collection_file.clone().unwrap_or_else(|| scenes_file(data))
}

fn active_profile_file(h: &Held, data: &std::path::Path) -> PathBuf {
    h.profile_file.clone().unwrap_or_else(|| settings_file(data))
}

fn load_prepared_collection(
    canvas: Canvas,
    file: &std::path::Path,
) -> Result<ferrocast_studio::stage::PreparedStage, ferrocast_studio::StudioError> {
    let load_one = |path: &std::path::Path| -> Option<
        Result<ferrocast_studio::stage::PreparedStage, Option<ferrocast_studio::StudioError>>,
    > {
        let bytes = match ferrocast_store::durable::read_patiently(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(error = %e, file = %path.display(), "couldn't read scene collection");
                return Some(Err(None));
            }
        };
        if bytes.is_empty() {
            tracing::warn!(file = %path.display(), "scene collection file is empty");
            return Some(Err(None));
        }
        let doc = match serde_json::from_slice::<Value>(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, file = %path.display(), "a scene collection didn't read");
                return Some(Err(None));
            }
        };
        Some(Collection::prepare_load(canvas, &doc, FIRST_SCENE).map_err(Some))
    };

    if let Some(res) = load_one(file) {
        match res {
            Ok(c) => return Ok(c),
            Err(schema_err) => {
                let _ = ferrocast_store::durable::rename_patiently(file, &file.with_extension("json.unreadable"));
                let bak = ferrocast_store::durable::backup_path(file);
                if let Some(Ok(c)) = load_one(&bak) {
                    tracing::warn!(file = %file.display(), "recovered scene collection from .bak");
                    return Ok(c);
                }
                if let Some(e) = schema_err {
                    return Err(e);
                }
                return Collection::prepare_load(canvas, &json!({ "sources": [] }), FIRST_SCENE);
            }
        }
    }

    let bak = ferrocast_store::durable::backup_path(file);
    if let Some(Ok(c)) = load_one(&bak) {
        tracing::warn!(file = %file.display(), "recovered scene collection from .bak");
        return Ok(c);
    }
    Collection::prepare_load(canvas, &json!({ "sources": [] }), FIRST_SCENE)
}

fn apply_collection_audio_prefs(c: &Collection, settings: Option<&StudioSettings>) {
    let s = settings.cloned().unwrap_or_default();
    c.set_true_peak(s.true_peak);
    c.set_low_latency(s.low_latency_audio);
    let (name, id) = s.monitoring.unwrap_or_else(|| ("Default".into(), "default".into()));
    if let Err(e) = c.set_monitoring_device(&name, &id) {
        tracing::warn!(error = %e, "the listening device is gone; using the system's");
    }
}

fn release_push_keys_on(c: &Collection, release_keys: &[String]) {
    for action in release_keys {
        let (name, talk) = if let Some(n) = action.strip_prefix("push_to_talk:") {
            (n, true)
        } else if let Some(n) = action.strip_prefix("push_to_mute:") {
            (n, false)
        } else {
            continue;
        };
        match c.push_key(name, talk, false) {
            Ok(true) => {}
            Ok(false) => {
                let _ = c.set_muted(name, talk);
            }
            Err(_) => {}
        }
    }
}

struct SwapPrecheck {
    canvas: Canvas,
    from: PathBuf,
}

struct PendingSwapPhase2 {
    teardown: Option<ferrocast_studio::stage::StageTeardown>,
    guests: Vec<(String, String, ferrocast_guest::Guest)>,
    prev_projectors: Vec<ProjectorReq>,
    open_spec: ferrocast_studio::stage::EngineOpenSpec,
    epoch_gen: u64,
}

struct FinishedSwapPhase3 {
    opened: Result<ferrocast_studio::stage::OpenedEngine, ferrocast_studio::StudioError>,
    prev_projectors: Vec<ProjectorReq>,
    epoch_gen: u64,
}

trait HeldBridge: Sync {
    fn call_main<F, R>(&self, f: F) -> impl std::future::Future<Output = Result<R, String>> + Send
    where
        F: FnOnce(ViewCtx<'_>, &mut Held) -> R + Send + 'static,
        R: Send + 'static;
}

impl HeldBridge for DesktopStudio {
    async fn call_main<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(ViewCtx<'_>, &mut Held) -> R + Send + 'static,
        R: Send + 'static,
    {
        let app = self.app.clone();
        self.on_main(move |h| f(ViewCtx::App(&app), h)).await
    }
}

fn swap_phase0_precheck(
    h: &mut Held,
    to_id: Option<&str>,
    from_fallback: &std::path::Path,
    to: &std::path::Path,
) -> Result<Option<SwapPrecheck>, String> {
    let Some(canvas) = h.collection.as_ref().map(Collection::canvas) else {
        // Not running: the next Start loads it.
        h.collection_file = Some(to.to_path_buf());
        if let Some(id) = to_id {
            h.collection_id = Some(id.to_string());
        }
        return Ok(None);
    };
    if outputs_busy_for_switch(h) {
        return Err("Stop streaming and recording before changing scene collections.".into());
    }
    let from = h.collection_file.clone().unwrap_or_else(|| from_fallback.to_path_buf());
    Ok(Some(SwapPrecheck { canvas, from }))
}

/// Commits the incoming collection onto `Held` and strips AppKit/Win32 UI views on the main
/// thread while moving heavy engine teardown, guest `.sdp` cleanup, and new engine creation into
/// `PendingSwapPhase2` so Phase 3 runs off the main thread.
fn swap_phase2_on_main(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    to_id: Option<&str>,
    from: &std::path::Path,
    to: &std::path::Path,
    prepared: ferrocast_studio::stage::PreparedStage,
    release: impl FnOnce(ViewCtx<'_>) -> Vec<String>,
) -> Result<Option<PendingSwapPhase2>, String> {
    let Some(canvas_now) = h.collection.as_ref().map(Collection::canvas) else {
        h.collection_file = Some(to.to_path_buf());
        if let Some(id) = to_id {
            h.collection_id = Some(id.to_string());
        }
        return Ok(None);
    };
    if outputs_busy_for_switch(h) {
        return Err("Stop streaming and recording before changing scene collections.".into());
    }
    if canvas_now != prepared.canvas() {
        return Err("The canvas size changed while loading the scene collection; try again.".into());
    }
    // Put the following item back first, so neither collection is saved slid sideways.
    if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
        c.stop_follow(&f);
    }
    h.follow_run = h.follow_run.saturating_add(1);
    // Release every view, projector, and stopped output holding the old engine before teardown.
    let mut prev_projectors = take_projector_reqs(h);
    // Source projectors belong to the outgoing collection only (OBS closes them on switch).
    prev_projectors.retain(|r| r.what != "source");
    h.preview = None;
    h.program_view = None;
    h.vertical.view = None;
    drop_inactive_outputs(h);
    stop_captions(h);
    h.replay_shown = None;
    h.replay_back = None;
    h.replay_wait = None;
    let guests = std::mem::take(&mut h.guests);
    // NDI and the virtual camera send the old collection's picture: they stop with it.
    h.ndi = None;
    h.virtualcam = None;
    h.history.clear();
    h.selected = None;
    h.frames = None;
    // Drain active shortcut holds only once the switch commits so a failed load leaves PTT/PTM
    // keys intact and responsive on the surviving collection.
    let release_keys = release(ctx);
    let teardown = if let Some(old_c) = h.collection.take() {
        release_push_keys_on(&old_c, &release_keys);
        save_confirmed(h.collection_deferred, &old_c, from);
        Some(old_c.into_teardown())
    } else {
        None
    };
    let made = Collection::from_prepared(prepared);
    made.set_engine_opening(true);
    h.collection_deferred = false;
    save_confirmed(false, &made, to);
    apply_collection_audio_prefs(&made, h.settings.as_ref());
    let open_spec = made.engine_open_spec();
    h.collection = Some(made);
    h.collection_file = Some(to.to_path_buf());
    if let Some(id) = to_id {
        h.collection_id = Some(id.to_string());
    }
    h.collection_gen = h.collection_gen.wrapping_add(1);
    Ok(Some(PendingSwapPhase2 { teardown, guests, prev_projectors, open_spec, epoch_gen: h.collection_gen }))
}

fn finish_phase3_off_main(p2: PendingSwapPhase2) -> FinishedSwapPhase3 {
    // 1. Drop outgoing engine/network/NDI and wait until all transient holders release it so two
    // engines never open an exclusive capture device at the same time.
    if let Some(td) = p2.teardown {
        td.finish_off_main();
    }
    // 2. Drop guest sources only after FFmpeg in the outgoing collection closed their .sdp files.
    drop(p2.guests);
    // 3. Open the incoming engine off the main thread.
    let opened = Collection::open_engine_off_main(&p2.open_spec);
    FinishedSwapPhase3 { opened, prev_projectors: p2.prev_projectors, epoch_gen: p2.epoch_gen }
}

fn swap_phase4_on_main(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path, p3: FinishedSwapPhase3) {
    if h.collection_gen != p3.epoch_gen || h.collection.is_none() {
        drop(p3.opened);
        return;
    }
    if let Some(c) = h.collection.as_ref() {
        match p3.opened {
            Ok(opened) => c.install_opened_engine(opened),
            Err(e) => {
                c.set_engine_opening(false);
                tracing::warn!(error = %e, "the engine couldn't open after switching scene collections");
            }
        }
    }
    rebuild_views(ctx, h, data, p3.prev_projectors);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    save_projectors(h, data);
    sync_views(h);
}

async fn swap_collection_off_main(
    bridge: &impl HeldBridge,
    data: PathBuf,
    to_id: Option<String>,
    from_fallback: PathBuf,
    to: PathBuf,
) -> Result<(), String> {
    let pre_id = to_id.clone();
    let pre_to = to.clone();
    let Some(pre) =
        bridge.call_main(move |_, h| swap_phase0_precheck(h, pre_id.as_deref(), &from_fallback, &pre_to)).await??
    else {
        return Ok(());
    };
    let canvas = pre.canvas;
    let to_read = to.clone();
    let prepared = tokio::task::spawn_blocking(move || load_prepared_collection(canvas, &to_read))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("That scene collection didn't load: {e}"))?;
    let p2_id = to_id;
    let p2_from = pre.from;
    let p2_to = to;
    let Some(p2) = bridge
        .call_main(move |ctx, h| {
            swap_phase2_on_main(ctx, h, p2_id.as_deref(), &p2_from, &p2_to, prepared, |c| match c {
                ViewCtx::App(app) => crate::studio_keys::release_active(app),
                #[cfg(test)]
                ViewCtx::Test => Vec::new(),
            })
        })
        .await??
    else {
        return Ok(());
    };
    let p3 = tokio::task::spawn_blocking(move || finish_phase3_off_main(p2)).await.map_err(|e| e.to_string())?;
    bridge.call_main(move |ctx, h| swap_phase4_on_main(ctx, h, &data, p3)).await?;
    Ok(())
}

#[cfg(test)]
fn swap_collection(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    from: &std::path::Path,
    to: &std::path::Path,
) -> Result<(), String> {
    swap_collection_to(ctx, h, data, None, from, to, &[])
}

/// Saves the scenes in use to `from` and puts `to`'s in their place (a missing file is a new
/// set with one empty scene). If `to` won't load, the old collection and all views remain intact.
#[cfg(test)]
fn swap_collection_to(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    to_id: Option<&str>,
    from: &std::path::Path,
    to: &std::path::Path,
    release_keys: &[String],
) -> Result<(), String> {
    swap_collection_releasing(ctx, h, data, to_id, from, to, || release_keys.to_vec())
}

#[cfg(test)]
fn swap_collection_releasing(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    to_id: Option<&str>,
    from: &std::path::Path,
    to: &std::path::Path,
    release: impl FnOnce() -> Vec<String>,
) -> Result<(), String> {
    let Some(pre) = swap_phase0_precheck(h, to_id, from, to)? else {
        return Ok(());
    };
    let prepared =
        load_prepared_collection(pre.canvas, to).map_err(|e| format!("That scene collection didn't load: {e}"))?;
    let Some(p2) = swap_phase2_on_main(ctx, h, to_id, &pre.from, to, prepared, |_| release())? else {
        return Ok(());
    };
    let p3 = finish_phase3_off_main(p2);
    swap_phase4_on_main(ctx, h, data, p3);
    Ok(())
}

/// The saved settings, falling back to `.bak` when unreadable, or the defaults.
/// The saved profile, as is: nothing is applied to the engine (a support report can read it
/// without changing the sound rate or broadcast timing of a running Studio).
fn read_settings_from(path: &std::path::Path) -> StudioSettings {
    ferrocast_store::durable::load_json_with_backup::<StudioSettings>(path, |s| s.check().is_ok())
        .map(|l| l.value)
        .unwrap_or_default()
}

fn read_settings(data: &std::path::Path) -> StudioSettings {
    read_settings_from(&settings_file(data))
}

fn load_settings_from(path: &std::path::Path) -> StudioSettings {
    let s = read_settings_from(path);
    apply_audio_format(&s);
    s
}

#[cfg(test)]
fn load_settings(data: &std::path::Path) -> StudioSettings {
    load_settings_from(&settings_file(data))
}

fn ensure_held_settings<'a>(h: &'a mut Held, data: &std::path::Path) -> &'a mut StudioSettings {
    let path = active_profile_file(h, data);
    h.settings.get_or_insert_with(|| load_settings_from(&path))
}

/// Reads the profile settings file `to` with `.bak` and queued-save fallback when switching
/// profiles. Returns factory defaults only when neither `to` nor `to.bak` exists yet; errors
/// on corrupt or invalid files so switching never silently overwrites them with defaults.
fn load_profile_settings(to: &std::path::Path) -> Result<StudioSettings, String> {
    if let Some(loaded) = ferrocast_store::durable::load_json_with_backup::<StudioSettings>(to, |s| s.check().is_ok()) {
        return Ok(loaded.value);
    }
    if to.exists() || ferrocast_store::durable::backup_path(to).exists() {
        return Err("That profile's settings couldn't be read cleanly.".into());
    }
    Ok(StudioSettings::default())
}

/// What the running engine can't take until Ferrocast restarts: its sound rate and its broadcast
/// timing are fixed when it opens. A line for the settings' answer (the dashboard offers Restart).
fn restart_note((rate, ntsc): (u32, bool), s: &StudioSettings) -> Option<String> {
    let want_ntsc = s.fps_ntsc && matches!(s.fps, 24 | 30 | 60);
    match (rate != s.sample_rate, ntsc != want_ntsc) {
        (false, false) => None,
        (true, false) => Some("The sound rate applies after Ferrocast restarts.".into()),
        (false, true) => Some("Broadcast timing applies after Ferrocast restarts.".into()),
        (true, true) => Some("The sound rate and broadcast timing apply after Ferrocast restarts.".into()),
    }
}

/// The sound's rate and the audio tracks' names, which the engine and the recording files read.
fn apply_audio_format(s: &StudioSettings) {
    ferrocast_studio::set_audio_format(s.sample_rate, s.audio_track_names.clone());
    ferrocast_studio::set_ntsc(s.fps_ntsc);
}

fn save_json(file: &std::path::Path, v: &Value) {
    // Queued: callers run on the main thread or the async runtime, which a locked file must not
    // stall. Later reads see the queued bytes.
    if let Err(e) = ferrocast_store::durable::queue_json_pretty(file, v) {
        tracing::warn!(error = %e, file = %file.display(), "the Studio couldn't save");
    }
}

/// The folder recordings go to: the one set, or Movies/Ferrocast.
pub(crate) fn record_folder(s: &StudioSettings) -> PathBuf {
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

/// The app, for the Move transition's steps (`glide`).
static APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();
/// A Move transition is being stepped.
static GLIDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Steps a Move transition about 60 times a second on the main thread until it ends (after
/// any scene switch: a click, a rule, a game, a multiview). Main thread or not.
fn glide() {
    use std::sync::atomic::Ordering;
    let Some(app) = APP.get().cloned() else { return };
    if GLIDING.swap(true, Ordering::AcqRel) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = app.run_on_main_thread(move || {
                // None: the Studio's state is busy this moment (tried again next frame).
                let more =
                    HELD.with(|h| h.try_borrow().ok().map(|h| h.collection.as_ref().is_some_and(|c| c.tick_moves())));
                let _ = tx.send(more);
            });
            if sent.is_err() {
                break;
            }
            match rx.await {
                Ok(Some(false)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        GLIDING.store(false, Ordering::Release);
        // A Move started while this one was ending found it still running: stepped now.
        let _ = app.run_on_main_thread(|| {
            let again =
                HELD.with(|h| h.try_borrow().ok().is_some_and(|h| h.collection.as_ref().is_some_and(|c| c.moving())));
            if again {
                glide();
            }
        });
    });
}

/// What the note holds: what's on now (`None`: the Studio isn't running).
fn session_now(h: &Held) -> Option<ferrocast_engine::studio_host::Recovered> {
    h.collection.as_ref()?;
    Some(ferrocast_engine::studio_host::Recovered {
        record: h.record.is_some(),
        replay: h.replay.is_some(),
        live: h.stream.is_some() || !h.groups.is_empty(),
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
        None => ferrocast_store::durable::queue_remove_with_backup(file),
    }
}

/// Maximum time `close()` / `stop()` waits for parallel off-main-thread output drains (exceeds
/// `OwnStream`'s internal 5 s tokio runtime shutdown timeout so normal drains finish cleanly).
const QUIT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// How long a *quit* waits for them. Windows ends an app that is closing its console, logging off
/// or shutting down about 5 s after asking it to (a stuck app is killed with the database
/// unfinished), so there the drain is held to 3 s: it, the engine's shutdown and the markers fit in
/// the grace. Elsewhere it is the usual bound.
fn quit_drain_for(windows: bool) -> std::time::Duration {
    if windows { std::time::Duration::from_secs(3) } else { QUIT_DRAIN_TIMEOUT }
}

fn push_opt_drain<T: Send + 'static>(jobs: &mut Vec<Box<dyn FnOnce() + Send>>, slot: &mut Option<T>) {
    if let Some(item) = slot.take() {
        jobs.push(Box::new(move || {
            let _dropped = item;
        }));
    }
}

fn push_vec_drain<T: Send + 'static>(jobs: &mut Vec<Box<dyn FnOnce() + Send>>, items: &mut Vec<T>) {
    for item in items.drain(..) {
        jobs.push(Box::new(move || {
            let _dropped = item;
        }));
    }
}

fn take_outputs_from_held(h: &mut Held) -> Vec<Box<dyn FnOnce() + Send>> {
    let mut jobs: Vec<Box<dyn FnOnce() + Send>> = Vec::new();
    h.stream_with = None;
    h.record_with = None;
    h.replay_shown = None;
    h.replay_back = None;
    h.replay_wait = None;
    h.live_pct = None;
    push_opt_drain(&mut jobs, &mut h.vstream);
    push_opt_drain(&mut jobs, &mut h.lstream);
    push_opt_drain(&mut jobs, &mut h.stream);
    let mut group_outputs: Vec<Streaming> = h.groups.drain().map(|(_, g)| g.output).collect();
    push_vec_drain(&mut jobs, &mut group_outputs);
    push_opt_drain(&mut jobs, &mut h.record);
    push_vec_drain(&mut jobs, &mut h.source_records);
    push_opt_drain(&mut jobs, &mut h.replay);
    push_opt_drain(&mut jobs, &mut h.virtualcam);
    push_opt_drain(&mut jobs, &mut h.ndi);
    sync_local_capture_flags(h);
    jobs
}

struct DrainGuard {
    tx: std::sync::mpsc::Sender<bool>,
    ok: bool,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.ok);
    }
}

/// Drops each extracted output on its own worker thread in parallel off the main thread, returning
/// `true` iff every output finished within `timeout` without panicking.
fn drain_outputs_bounded(jobs: Vec<Box<dyn FnOnce() + Send>>, timeout: std::time::Duration) -> bool {
    if jobs.is_empty() {
        return true;
    }
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let mut started = 0usize;
    let mut all_ok = true;
    for job in jobs {
        let tx = tx.clone();
        match std::thread::Builder::new().name("ferrocast-drain".into()).spawn(move || {
            let mut guard = DrainGuard { tx, ok: false };
            job();
            guard.ok = true;
        }) {
            Ok(_) => started += 1,
            Err(err) => {
                tracing::warn!(%err, "failed to spawn Studio output drain thread");
                all_ok = false;
            }
        }
    }
    drop(tx);
    let deadline = std::time::Instant::now() + timeout;
    let mut finished = 0usize;
    while finished < started {
        let now = std::time::Instant::now();
        if now >= deadline {
            tracing::warn!(
                finished,
                total = started,
                timeout_ms = timeout.as_millis() as u64,
                "Studio outputs exceeded shutdown drain deadline"
            );
            return false;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(ok) => {
                finished += 1;
                all_ok &= ok;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    finished,
                    total = started,
                    timeout_ms = timeout.as_millis() as u64,
                    "Studio outputs exceeded shutdown drain deadline"
                );
                return false;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    all_ok && finished == started
}

fn stop_captions(h: &mut Held) {
    h.captions = None;
    h.captions_run = h.captions_run.saturating_add(1);
    CAPTION_SINK.with(|s| s.borrow_mut().take());
    if let Ok(mut heard) = HEARD.lock() {
        heard.clear();
    }
    if let Ok(mut s) = SENTENCE.lock() {
        *s = (s.0.saturating_add(1), String::new());
    }
    if let Ok(mut d) = DONE.lock() {
        d.clear();
    }
    if let Ok(mut e) = CAPTIONS_ERROR.lock() {
        *e = None;
    }
}

fn deliver_caption(run: u64, said: String, last: bool) {
    CAPTION_SINK.with(|s| {
        if let Some((r, f)) = s.borrow().as_ref()
            && *r == run
        {
            f(said, last);
        }
    });
}

fn clear_non_output_held(h: &mut Held) {
    h.preview = None;
    h.preview_at = None;
    h.program_view = None;
    h.program_at = None;
    h.vertical = VerticalView::default();
    h.projectors.clear();
    h.history.clear();
    stop_captions(h);
    if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
        c.stop_follow(&f);
    }
}

/// Prevents session persistence updates from clearing `session.json` prematurely while outputs are
/// stopping during an application quit. Call `end_session()` strictly after clean close completes.
pub fn mark_quitting() {
    QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Ferrocast is quitting normally: the note goes, so the next launch isn't taken for a crash.
pub fn end_session() {
    mark_quitting();
    if let Some(file) = SESSION.get() {
        ferrocast_store::durable::remove_with_backup(file);
    }
}

/// Closes the Studio cleanly before exit on the main thread while AppKit/GPU state is still whole:
/// stops and drains active outputs in parallel off the main thread (bounded) BEFORE dropping the
/// scene collection. Returns `true` when outputs drained cleanly within the bound.
pub fn close() -> bool {
    // Taken out first, then drained/dropped: nothing borrows HELD while the engine closes.
    let Some(mut held) = HELD.with(|h| h.try_borrow_mut().ok().map(|mut h| std::mem::take(&mut *h))) else {
        tracing::warn!("Studio state is still borrowed on close");
        return false;
    };
    let jobs = take_outputs_from_held(&mut held);
    let clean = drain_outputs_bounded(jobs, quit_drain_for(cfg!(windows)));
    clear_non_output_held(&mut held);
    let guests = std::mem::take(&mut held.guests);
    drop(held.collection.take());
    drop(guests);
    drop(held);
    // The queued scene saves land before the process goes (bounded).
    if !ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(6)) {
        tracing::warn!("a scene save was still waiting at exit");
    }
    clean
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
fn remux_in_background(files: Vec<PathBuf>) -> Result<PathBuf, String> {
    let ffmpeg = ferrocore::ffenc::path()
        .ok_or("Remuxing to MP4 needs FFmpeg: pick x264 or Lossless once in Settings and Ferrocast fetches it.")?;
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
            // Each file is its share of the whole; FFmpeg copies the packets as they are.
            let whole = i as f32 * 100.0 / count as f32;
            set_remux(json!({ "from": f, "to": t, "percent": whole, "done": false, "error": null }));
            let out = ferrocore::ffenc::hidden(std::process::Command::new(&ffmpeg))
                .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(&from)
                .args(["-map", "0", "-c", "copy", "-movflags", "+faststart"])
                .arg(&to)
                .stdin(std::process::Stdio::null())
                .output();
            let error = match out {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect::<String>()),
                Err(e) => Some(e.to_string()),
            };
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
    let mut v = serde_json::to_value(r.0.record_stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("file".into(), Value::String(r.1.display().to_string()));
        o.insert("seconds".into(), json!(r.2.elapsed().as_secs()));
        // Why it stopped by itself (a full disk), else what the output says (its own error).
        let stopped = o.get("error").and_then(Value::as_str).map(|why| format!("The recording stopped: {why}."));
        o.insert("error".into(), stopped.or_else(|| r.0.last_error()).map_or(Value::Null, Value::String));
        o.insert("encoder".into(), r.0.encoder().map_or(Value::Null, Value::String));
    }
    v
}

fn stats_of(s: &Streaming, started: Option<std::time::Instant>) -> Value {
    let mut v = serde_json::to_value(s.stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        if let Some(st) = started {
            o.insert("seconds".into(), json!(st.elapsed().as_secs_f64()));
        }
        o.insert("error".into(), s.last_error().map_or(Value::Null, Value::String));
        o.insert("encoder".into(), s.encoder().map_or(Value::Null, Value::String));
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

/// A recording sharing the stream's encode carries the stream's picture, so choosing something
/// to show only on the stream or only in the recording can't work while they do: it says so
/// rather than quietly leaving the recording with the stream's picture.
fn refuses_while_shared(act: &StudioAct, shared: bool) -> Option<&'static str> {
    let flags = matches!(act, StudioAct::EditItem { edit: Edit::StreamOnly | Edit::RecordingOnly, .. });
    (flags && shared).then_some(
        "The recording shares the stream's encoder (Same as stream), so it can't show something different from the stream. Stop the recording, then choose where this shows.",
    )
}

/// Whether the running stream and recording share one encode.
fn shares_encoder(h: &Held) -> bool {
    h.stream.as_ref().is_some_and(|s| s.shares_encoder())
        || h.groups.values().any(|g| g.output.shares_encoder())
        || h.record.as_ref().is_some_and(|(r, ..)| r.shares_encoder())
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
        Edit::StreamAndRecording => ItemEdit::StreamAndRecording,
        Edit::StreamOnly => ItemEdit::StreamOnly,
        Edit::RecordingOnly => ItemEdit::RecordingOnly,
    }
}

macro_rules! to_json {
    ($v:expr) => {
        serde_json::to_value($v).map_err(|e| e.to_string())
    };
}

/// The scenes the Studio shows: what `Scenes` answers.
fn scenes_json(c: &Collection) -> Result<Value, String> {
    to_json!(json!({
        "scenes": c.scenes(),
        "current": c.current(),
        "transition": c.transition(),
        "named_transitions": c.named_transitions(),
        "transition_matrix": c.transition_matrix(),
        // Studio Mode's preview (null when it's off), its quick transitions and options.
        "preview": c.preview_scene(),
        "quick": c.quick_transitions(),
        "studio_options": c.studio_options(),
        "tbar": c.tbar_state(),
        "black": c.is_black(),
        "vertical": c.vertical(),
        "top_layer": c.top_layer(),
    }))
}

/// The names of the scenes shown inside these items (a scene item, or one inside a group).
fn nested_scenes(items: &Value, found: &mut impl FnMut(&str)) {
    for item in items.as_array().into_iter().flatten() {
        if item["visible"].as_bool() == Some(false) {
            continue;
        }
        if item["kind"].as_str() == Some("scene")
            && let Some(n) = item["name"].as_str()
        {
            found(n);
        }
        nested_scenes(&item["children"], found);
    }
}

/// Does one act on the collection → what it shows (null for a change).
fn run(c: &Collection, act: StudioAct) -> Result<Value, ferrocast_studio::StudioError> {
    use StudioAct as A;
    let done = |r: Result<(), ferrocast_studio::StudioError>| r.map(|()| Value::Null);
    let v = |x: Result<Value, String>| x.map_err(ferrocast_studio::StudioError::Failed);
    match act {
        A::Scenes => v(scenes_json(c)),
        A::WatchSnapshot => v((|| {
            let scenes = scenes_json(c)?;
            let current = scenes["current"].as_str().filter(|n| !n.is_empty()).map(String::from);
            let audio = match &current {
                Some(scene) => to_json!(c.audio(scene).map_err(|e| e.to_string())?)?,
                None => Value::Null,
            };
            // The scene on air and every scene shown inside it (through groups), once each.
            let mut items = serde_json::Map::new();
            let mut todo: Vec<(String, usize)> = current.into_iter().map(|s| (s, 0)).collect();
            while let Some((scene, depth)) = todo.pop() {
                if items.contains_key(&scene) {
                    continue;
                }
                let Ok(list) = c.items(&scene) else { continue };
                let list = to_json!(list)?;
                if depth < 8 {
                    nested_scenes(&list, &mut |n| todo.push((n.to_string(), depth + 1)));
                }
                items.insert(scene, list);
            }
            Ok::<Value, String>(json!({ "scenes": scenes, "audio": audio, "items": Value::Object(items) }))
        })()),
        A::SetTopLayer { scene } => done(c.set_top_layer(scene.as_deref())),
        A::Refresh => {
            c.refresh();
            Ok(Value::Null)
        }
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
        A::SourceStatus { .. }
        | A::ScreenStill { .. }
        | A::SourceStill { .. }
        | A::VstPlugins { .. }
        | A::Vst { .. } => Err(ferrocast_studio::StudioError::Failed("read off the main thread".into())),
        A::SourcePicture { name, width, filters, item } => {
            let label = item.as_ref().map_or_else(|| name.clone(), |it| it.scene.clone());
            let pixels = match item {
                Some(it) => c.item_picture_pixels_for(
                    (!name.is_empty()).then_some(name.as_str()),
                    &it.scene,
                    it.id,
                    width,
                    filters,
                )?,
                None => c.source_picture_pixels(&name, width, filters)?,
            };
            Ok(match pixels {
                Some((px, w, h, (sw, sh))) => {
                    json!({ "image": ferrocast_studio::png_data_url(&px, w, h, &label)?, "width": sw, "height": sh })
                }
                None => json!({ "image": null }),
            })
        }
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
        A::SetCurrent { scene } => {
            let r = done(c.set_current(&scene));
            if c.moving() {
                glide();
            }
            r
        }
        A::SetTransition { kind, ms } => done(c.set_transition(&TransitionInfo { kind, ms })),
        A::SetSceneTransition { scene, transition } => {
            done(c.set_scene_transition(&scene, transition.map(|t| TransitionInfo { kind: t.kind, ms: t.ms }).as_ref()))
        }
        A::AddNamedTransition { transition: t } => {
            let created = c.add_named_transition(&ferrocast_studio::NamedTransition {
                name: t.name,
                kind: t.kind,
                ms: t.ms,
                settings: t.settings,
                default: t.default,
            })?;
            v(to_json!(created))
        }
        A::UpdateNamedTransition { name, patch } => {
            let updated = c.update_named_transition(
                &name,
                patch.kind.as_deref(),
                patch.ms,
                patch.settings.as_ref(),
                patch.default,
            )?;
            v(to_json!(updated))
        }
        A::RenameNamedTransition { name, new_name } => v(to_json!(c.rename_named_transition(&name, &new_name)?)),
        A::DuplicateNamedTransition { name, new_name } => {
            v(to_json!(c.duplicate_named_transition(&name, new_name.as_deref())?))
        }
        A::DeleteNamedTransition { name } => done(c.delete_named_transition(&name)),
        A::SetDefaultNamedTransition { name } => done(c.set_default_named_transition(&name)),
        A::SetTransitionMatrixRule { from, to, transition } => {
            done(c.set_matrix_rule(from.as_deref(), to.as_deref(), transition.as_deref()))
        }
        A::Items { scene } => v(to_json!(c.items(&scene)?)),
        A::AddSource { scene, source, fit } => {
            // `reused`: a camera or capture card already used here, shown again (never opened twice).
            let (item, reused) = c.add_source_or_reuse(&scene, &source.kind, &source.name, &source.settings, fit)?;
            let settings = c.settings(&item.name)?;
            v(to_json!(json!({ "item": item, "settings": settings, "reused": reused })))
        }
        A::AddExisting { scene, name } => v(to_json!(json!({ "item": c.add_existing(&scene, &name)? }))),
        A::EditItem { scene, id, edit } => done(c.edit_item(&scene, id, item_edit(edit))),
        // A key held down: shown or hidden for as long as it is held, never saved (the engine
        // puts it back on release).
        A::PushItem { scene, id, visible, release, .. } => {
            // The engine's view only: nothing is written to the collection.
            done(c.hold_item(&scene, id, (!release).then_some(visible)))
        }
        A::PushFilter { name, filter, enabled, release, .. } => {
            done(c.hold_filter(&name, &filter, (!release).then_some(enabled)))
        }
        A::Sources => v(to_json!(c.sources())),
        A::Rename { name, new_name } => done(c.rename(&name, &new_name)),
        A::Settings { name } => c.settings(&name),
        A::Options { name } => v(to_json!(c.options(&name)?)),
        A::Update { name, settings } => done(c.update(&name, &settings)),
        A::Audio { scene } => v(to_json!(c.audio(&scene)?)),
        A::AudioByName { name } => v(to_json!(c.audio_of(&name)?)),
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
            if let Some(m) = change.mic {
                c.set_mic(&name, Some(m))?;
            }
            if change.push_to_talk.is_some() || change.push_to_talk_ms.is_some() {
                c.set_push(&name, true, change.push_to_talk, change.push_to_talk_ms)?;
            }
            if change.push_to_mute.is_some() || change.push_to_mute_ms.is_some() {
                c.set_push(&name, false, change.push_to_mute, change.push_to_mute_ms)?;
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
        A::PushKey { name, talk, down, .. } => v(Ok(json!(c.push_key(&name, talk, down)?))),
        A::Look { names } => v(to_json!(c.look(&names)?)),
        A::AudioCare => v(Ok(c.audio_care())),
        A::SetAudioCare { care } => done(c.set_audio_care(&care)),
        A::Privacy => v(Ok(c.privacy())),
        A::SetPrivacy { privacy } => done(c.set_privacy(&privacy)),
        A::Filters { name, item } => match item {
            Some(it) => v(to_json!(c.item_filters(&it.scene, it.id)?)),
            None => v(to_json!(c.filters(&name)?)),
        },
        A::AddFilter { name, filter, item } => done(match item {
            Some(it) => c.add_item_filter(&it.scene, it.id, &filter.kind, &filter.name, &filter.settings),
            None => c.add_filter(&name, &filter.kind, &filter.name, &filter.settings),
        }),
        A::RemoveFilter { name, filter, item } => done(match item {
            Some(it) => c.remove_item_filter(&it.scene, it.id, &filter),
            None => c.remove_filter(&name, &filter),
        }),
        A::PasteFilters { name, from, item, from_item } => {
            let src_item = from_item.as_ref().map(|f| (f.scene.as_str(), f.id));
            done(match item {
                Some(it) => c.paste_item_filters_from(&it.scene, it.id, &from, src_item),
                None => c.paste_filters_from(&name, &from, src_item),
            })
        }
        A::CopySource { scene, id } => v(Ok(c.copy_source(&scene, id)?.to_json())),
        A::CopyFilters { name, item } => {
            let item = item.as_ref().map(|i| (i.scene.as_str(), i.id));
            v(Ok(c.copy_filters(&name, item)?.to_json()))
        }
        A::PasteSource { scene, clip } => {
            let clip = ferrocast_studio::stage::SourceClip::from_json(&clip)
                .ok_or_else(|| ferrocast_studio::StudioError::Failed("That isn't a copied source.".into()))?;
            let (item, notes) = c.paste_source(&scene, &clip)?;
            v(to_json!(json!({ "item": item, "notes": notes })))
        }
        A::PasteFilterClip { name, item, clip } => {
            let clip = ferrocast_studio::stage::FilterClip::from_json(&clip)
                .ok_or_else(|| ferrocast_studio::StudioError::Failed("That isn't a copy of filters.".into()))?;
            let item = item.as_ref().map(|i| (i.scene.as_str(), i.id));
            v(Ok(json!({ "notes": c.paste_filter_values(&name, item, &clip)? })))
        }
        A::SetItemFilterScope { scene, id, own } => v(to_json!(c.set_item_filter_scope(&scene, id, own)?)),
        A::SetItemBox { scene, id, x, y, w, h, .. } => v(to_json!(c.set_item_box(&scene, id, x, y, w, h)?)),
        A::SetItemBoxes { scene, items, .. } => {
            let boxes: Vec<_> = items.iter().map(|b| (b.id, b.x, b.y, b.w, b.h)).collect();
            v(to_json!(c.set_item_boxes(&scene, &boxes)?))
        }
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
        A::EditFilter { name, filter, edit, item } => {
            let mv = match edit {
                FilterEdit::Up => Some(ItemEdit::Up),
                FilterEdit::Down => Some(ItemEdit::Down),
                FilterEdit::Top => Some(ItemEdit::Top),
                FilterEdit::Bottom => Some(ItemEdit::Bottom),
                FilterEdit::Enable | FilterEdit::Disable => None,
            };
            done(match (item, mv) {
                (Some(it), Some(m)) => c.move_item_filter(&it.scene, it.id, &filter, m),
                (Some(it), None) => c.set_item_filter_enabled(&it.scene, it.id, &filter, edit == FilterEdit::Enable),
                (None, Some(m)) => c.move_filter(&name, &filter, m),
                (None, None) => c.set_filter_enabled(&name, &filter, edit == FilterEdit::Enable),
            })
        }
        A::FilterSettings { name, filter, item } => match item {
            Some(it) => c.item_filter_settings(&it.scene, it.id, &filter),
            None => c.filter_settings(&name, &filter),
        },
        A::FilterOptions { name, filter, item } => match item {
            Some(it) => v(to_json!(c.item_filter_options(&it.scene, it.id, &filter)?)),
            None => v(to_json!(c.filter_options(&name, &filter)?)),
        },
        A::UpdateFilter { name, filter, settings, item } => done(match item {
            Some(it) => c.update_item_filter(&it.scene, it.id, &filter, &settings),
            None => c.update_filter(&name, &filter, &settings),
        }),
    }
}

/// Writes the collection atomically with `.bak` backup unless collection loading is deferred in
/// Safe Mode (so the temporary blank collection never overwrites saved scenes on disk).
fn save_confirmed(deferred: bool, c: &Collection, file: &std::path::Path) {
    if deferred {
        return;
    }
    let doc = c.save(COLLECTION);
    // Queued, not written here: this runs on the main thread, and a locked file (antivirus,
    // indexer) would make the write wait out its retries (~5.7 s) with the UI frozen.
    if let Err(e) = ferrocast_store::durable::queue_json_pretty(file, &doc) {
        tracing::warn!(error = %e, file = %file.display(), "the Studio's scenes couldn't be saved");
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
    if let (Some(c), Some(v)) = (&h.collection, &h.vertical.view) {
        v.follow(c);
    }
    // A multiview's preview box and scene list too.
    refresh_projectors(h);
}

/// Makes Studio Mode's picture of what's on air at its last place (main thread).
fn make_program_view(ctx: ViewCtx<'_>, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.program_at else { return Ok(()) };
    // What's on air, drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let c = h.collection.as_ref().ok_or(STOPPED)?;
    if c.preview_scene().is_none() {
        h.program_view = None;
        return Ok(());
    }
    if c.is_engine_opening() {
        return Ok(());
    }
    let p = create_stage_view(ctx, c, rect, scale, false)?;
    // Made again (a new canvas size): the stage keeps the page's colour.
    if let Some(c) = h.program_bg {
        p.background(c);
    }
    p.overflow_stripes(h.program_stripe);
    p.set_holes(&h.program_holes);
    p.set_opacity(h.program_opacity.unwrap_or(1.0));
    p.set_shade(h.program_shade);
    p.set_paused(h.preview_paused);
    h.program_view = Some(p);
    Ok(())
}

/// Makes the preview at its last place (main thread).
fn make_preview(ctx: ViewCtx<'_>, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.preview_at else { return Ok(()) };
    // Drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let c = h.collection.as_ref().ok_or(STOPPED)?;
    if c.is_engine_opening() {
        return Ok(());
    }
    let mut p = create_stage_view(ctx, c, rect, scale, false)?;
    if let Some(c) = h.preview_canvas {
        p.set_area(rect, c, scale);
    }
    p.select(h.selected.as_ref().map(|(s, i)| (s.as_str(), i.as_slice())));
    p.frames(h.frames.as_deref());
    if let Some(c) = h.preview_bg {
        p.background(c);
    }
    p.overflow_stripes(h.preview_stripe);
    p.set_holes(&h.preview_holes);
    p.set_opacity(h.preview_opacity.unwrap_or(1.0));
    p.set_shade(h.preview_shade);
    p.set_paused(h.preview_paused);
    h.preview = Some(p);
    sync_views(h);
    Ok(())
}

fn format_settings_note(mut notes: Vec<String>, failed_shortcuts: &[String]) -> Option<String> {
    if !failed_shortcuts.is_empty() {
        notes.push(format!(
            "These shortcuts are used by another app or aren't valid here: {}.",
            failed_shortcuts.join(", ")
        ));
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}

fn shelf_action_result(id: &str, note: Option<String>) -> Value {
    let mut out = json!({ "id": id });
    if let Some(n) = note
        && let Some(o) = out.as_object_mut()
    {
        o.insert("note".into(), json!(n));
    }
    out
}

fn obsws_snapshot_json(mut base: Value, lib: &library::Library, col_id: Option<&str>, prof_id: Option<&str>) -> Value {
    let mut collections = lib.collections.clone();
    if let Some(id) = col_id {
        collections.current = id.to_string();
    }
    let mut profiles = lib.profiles.clone();
    if let Some(id) = prof_id {
        profiles.current = id.to_string();
    }
    if let Some(o) = base.as_object_mut() {
        o.insert("collections".into(), json!(collections));
        o.insert("profiles".into(), json!(profiles));
    }
    base
}

#[derive(Debug, Clone)]
pub struct DesktopStudio {
    app: AppHandle,
    data: PathBuf,
    shelf_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    library_cache: std::sync::Arc<std::sync::Mutex<Option<library::Library>>>,
}

fn act_shortcut_epoch(act: &StudioAct) -> Option<u64> {
    match act {
        StudioAct::PushKey { epoch, .. } | StudioAct::PushItem { epoch, .. } | StudioAct::PushFilter { epoch, .. } => {
            *epoch
        }
        StudioAct::SetAudio { change, .. } => change.shortcut_epoch,
        _ => None,
    }
}

fn shelf_switch_is_noop(h: &Held, kind: Kind, current: &str, id: &str) -> bool {
    if current != id {
        return false;
    }
    match kind {
        Kind::Collections => {
            h.collection.as_ref().is_none_or(|_| !h.collection_deferred && h.collection_id.as_deref() == Some(id))
        }
        Kind::Profiles => true,
    }
}

impl DesktopStudio {
    pub fn new(app: AppHandle, data: PathBuf) -> DesktopStudio {
        let _ = APP.set(app.clone());
        let file = data.join("studio").join("session.json");
        // A note left from last time: Ferrocast didn't close normally while the Studio ran.
        if let Some(loaded) =
            ferrocast_store::durable::load_json_with_backup::<ferrocast_engine::studio_host::Recovered>(&file, |_| true)
        {
            let r = loaded.value;
            tracing::warn!(?r, "Ferrocast didn't close normally last time; the next Start puts the Studio back");
            if let Ok(mut c) = CRASHED.lock() {
                *c = Some(r);
            }
        }
        ferrocast_store::durable::remove_with_backup(&file);
        let _ = SESSION.set(file);
        let me = DesktopStudio {
            app,
            data,
            shelf_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            library_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let watcher = me.clone();
        tauri::async_runtime::spawn(async move { watcher.session_loop().await });
        let follower = me.clone();
        tauri::async_runtime::spawn(async move { follower.own_engine_loop().await });
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

    /// Ten times a second, what Ferrocast's own engine draws (the preview, streams,
    /// recordings, the replay buffer) catches up with the Studio: scene changes go on at once,
    /// edits within a tenth of a second.
    async fn own_engine_loop(&self) {
        loop {
            // Ten times a second: the preview shows edits (a drag) through it.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = self
                .on_main(|h| {
                    let Some(c) = h.collection.as_ref() else { return };
                    for s in [h.stream.as_mut(), h.vstream.as_mut()]
                        .into_iter()
                        .flatten()
                        .chain(h.groups.values_mut().map(|g| &mut g.output))
                        .filter(|s| s.drawn())
                    {
                        s.follow(c);
                    }
                    for p in
                        [h.preview.as_ref(), h.program_view.as_ref(), h.vertical.view.as_ref()].into_iter().flatten()
                    {
                        p.follow(c);
                    }
                    if let Some(r) = h.replay.as_mut().filter(|r| r.drawn()) {
                        r.follow(c);
                    }
                    for (_, r, file) in h.source_records.iter_mut().filter(|(_, r, _)| r.drawn()) {
                        r.follow(c);
                        if r.path() != file.as_path() {
                            *file = r.path().to_path_buf();
                        }
                    }
                    if let Some((r, file, _)) = h.record.as_mut().filter(|(r, ..)| r.drawn()) {
                        r.follow(c);
                        // A split by the plan's minutes or megabytes moves on to a new file.
                        if r.path() != file.as_path() {
                            *file = r.path().to_path_buf();
                        }
                    }
                    instant_replay(h);
                })
                .await;
        }
    }

    /// Runs `f` on the main thread with the libobs state and waits for its answer.
    /// A kind's form, its device lists read off the window's thread: listing cameras can take
    /// seconds (on Windows FFmpeg asks DirectShow), and on the main thread that froze the
    /// window and held up the live picture's place, so it stayed over the Add dialog for
    /// seconds (owner, 8 Nov). Only the displays' names (AppKit) are read on the main thread.
    async fn read_options(&self, kind: String) -> Result<Option<Vec<ferrocast_studio::OptionInfo>>, String> {
        #[cfg(target_os = "macos")]
        if kind == "screen_capture" {
            self.on_main(|_| ferrocast_studio::kinds::read_screen_names()).await?;
        }
        tokio::task::spawn_blocking(move || ferrocast_studio::kinds::options_cached(&kind))
            .await
            .map_err(|e| format!("The list couldn't be read: {e}"))
    }

    async fn on_main<T: Send + 'static>(&self, f: impl FnOnce(&mut Held) -> T + Send + 'static) -> Result<T, String> {
        use std::sync::atomic::{AtomicU8, Ordering};
        // 0 waiting, 1 started, 2 given up: a task given up on never runs later (the caller was
        // told it failed), and one that started is waited for however long it takes.
        let state = std::sync::Arc::new(AtomicU8::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let posted = std::time::Instant::now();
        let st = state.clone();
        self.app
            .run_on_main_thread(move || {
                if st.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
                    return;
                }
                let late = posted.elapsed();
                if late > MAIN_LATE {
                    tracing::warn!(
                        waited_ms = late.as_millis() as u64,
                        "the window's thread took a while to get to a Studio task"
                    );
                }
                let _ = started_tx.send(());
                let out = HELD.with(|h| {
                    let mut h = h.borrow_mut();
                    let out = f(&mut h);
                    sync_local_capture_flags(&h);
                    out
                });
                let _ = tx.send(out);
            })
            .map_err(|e| format!("The window isn't ready: {e}"))?;
        // Started within the time, or the window is stuck: said, never a silent wait (Windows
        // runner, 19 Nov: every call through the window's thread waited forever). A task that
        // started may take its time (finishing a recording waits for its file).
        if tokio::time::timeout(MAIN_START_WITHIN, started_rx).await.is_err()
            && state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire).is_ok()
        {
            tracing::warn!(seconds = MAIN_START_WITHIN.as_secs(), "the window's thread didn't get to a Studio task");
            return Err(format!(
                "The window is busy: it didn't get to this in {} s. Try again, or restart Ferrocast if it stays stuck.",
                MAIN_START_WITHIN.as_secs()
            ));
        }
        rx.await.map_err(|_| "The window closed.".to_string())
    }

    async fn cached_library(&self) -> library::Library {
        if let Some(lib) = self.library_cache.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return lib;
        }
        let data = self.data.clone();
        let lib = tokio::task::spawn_blocking(move || library::load(&data)).await.unwrap_or_else(|_| {
            let mut slot = self.library_cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(cached) = slot.clone() {
                return cached;
            }
            let loaded = library::load(&self.data);
            *slot = Some(loaded.clone());
            loaded
        });
        *self.library_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(lib.clone());
        lib
    }

    async fn load_library_off_main(&self) -> Result<library::Library, String> {
        let data = self.data.clone();
        let lib = tokio::task::spawn_blocking(move || library::load(&data)).await.map_err(|e| e.to_string())?;
        *self.library_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(lib.clone());
        Ok(lib)
    }

    async fn save_library_off_main(&self, lib: library::Library) -> Result<(), String> {
        *self.library_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(lib.clone());
        let data = self.data.clone();
        tokio::task::spawn_blocking(move || library::save(&data, &lib)).await.map_err(|e| e.to_string())
    }

    async fn forget_off_main(&self, kind: Kind, id: String) {
        let data = self.data.clone();
        if let Ok(lib) = tokio::task::spawn_blocking(move || {
            let mut lib = library::load(&data);
            lib.shelf_mut(kind).list.retain(|e| e.id != id);
            library::save(&data, &lib);
            ferrocast_store::durable::remove_with_backup(&library::file(&data, kind, &id));
            lib
        })
        .await
        {
            *self.library_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(lib);
        }
    }

    /// Puts collection or profile `id` in use. Refused while any output is active (stream,
    /// recording, source record, replay, virtual camera, NDI) so switching never orphans or
    /// blacks out live outputs (M9-01). Callers must hold `self.shelf_lock`.
    async fn switch_locked(&self, kind: Kind, id: &str) -> Result<Option<String>, String> {
        let data = self.data.clone();
        let lib = self.load_library_off_main().await?;
        if lib.shelf(kind).get(id).is_none() {
            return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
        }
        let current = lib.shelf(kind).current.clone();
        let target_id = id.to_string();
        if self.on_main(move |h| shelf_switch_is_noop(h, kind, &current, &target_id)).await? {
            return Ok(None);
        }
        let busy = self.on_main(|h| outputs_busy_for_switch(h)).await?;
        if busy {
            let noun = match kind {
                Kind::Collections => "scene collections",
                Kind::Profiles => "profiles",
            };
            return Err(format!("Stop streaming and recording before changing {noun}."));
        }
        let from_fallback = lib.current_file(&data, kind);
        let to = library::file(&data, kind, id);
        match kind {
            Kind::Collections => {
                swap_collection_off_main(self, data.clone(), Some(id.to_string()), from_fallback, to).await?;
                let mut lib = self.load_library_off_main().await?;
                lib.collections.current = id.to_string();
                self.save_library_off_main(lib).await?;
                Ok(None)
            }
            Kind::Profiles => {
                let to_read = to.clone();
                let next = tokio::task::spawn_blocking(move || load_profile_settings(&to_read))
                    .await
                    .map_err(|e| e.to_string())??;
                let note = self.set_settings_for(next, Some((id.to_string(), to))).await?;
                let mut lib = self.load_library_off_main().await?;
                lib.profiles.current = id.to_string();
                self.save_library_off_main(lib).await?;
                Ok(note)
            }
        }
    }

    async fn set_settings_for(
        &self,
        s: StudioSettings,
        target: Option<(String, PathBuf)>,
    ) -> Result<Option<String>, String> {
        let data = self.data.clone();
        let app = self.app.clone();
        let keys = s.hotkeys.clone();
        let notes =
            self.on_main(move |h| apply_settings_for_profile(ViewCtx::App(&app), h, &data, s, target)).await??;
        let failed = crate::studio_keys::apply(&self.app, &keys);
        Ok(format_settings_note(notes, &failed))
    }

    fn status_of(h: &Held) -> StudioStatus {
        // Ferrocast's own engine is always here: nothing to install.
        StudioStatus {
            available: true,
            running: h.collection.is_some(),
            version: h.collection.as_ref().map(|_| format!("Ferrocore {}", env!("CARGO_PKG_VERSION"))),
            renderer: h.collection.as_ref().map(|_| h.renderer.to_string()),
            detail: None,
            recovered: None,
            safe_mode: None,
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
            // Ten times a second: the preview shows edits (a drag) through it.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

/// Two projectors on the same display: both full screen, on the same one.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn same_screen(a: Option<u32>, b: Option<u32>) -> bool {
    a.is_some() && a == b
}

/// Where the open projectors are kept for "Restore projectors on start" (one file, apart from
/// the settings, so a settings save made from an older page can't undo it).
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn projectors_file(data: &std::path::Path) -> PathBuf {
    data.join("studio").join("projectors.json")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn create_projector_win(
    ctx: ViewCtx<'_>,
    c: &Collection,
    p: &ProjectorReq,
    of: &ferrocast_studio::ProjectorOf,
    look: ferrocast_studio::MultiviewLook,
    on_close: Box<dyn Fn()>,
    on_pick: Box<dyn Fn(String)>,
) -> Result<ProjectorWin, String> {
    match ctx {
        ViewCtx::App(_) => {
            let made =
                c.projector(of, p.screen.map(|s| s as usize), look, on_close, on_pick).map_err(|e| e.to_string())?;
            #[cfg(not(test))]
            {
                Ok(made)
            }
            #[cfg(test)]
            {
                Ok(ProjectorWin::Real(made))
            }
        }
        #[cfg(test)]
        ViewCtx::Test => {
            use ferrocast_studio::ProjectorOf as P;
            let engine = c.engine_ref().map_err(|e| e.to_string())?;
            if let P::Source(name) = of {
                let has_source = c.sources().iter().any(|s| s.name == *name);
                let has_scene = c.scene_index(name).is_ok();
                if !has_source && !has_scene {
                    return Err(format!("There's no source called \"{name}\"."));
                }
            }
            let (names, preview) = if matches!(of, P::Multiview) {
                (c.scenes().into_iter().map(|s| s.name).collect(), c.preview_scene())
            } else {
                (Vec::new(), None)
            };
            Ok(ProjectorWin::Test(TestProjector {
                engine,
                what: p.what.clone(),
                name: p.name.clone(),
                names: std::cell::RefCell::new(names),
                preview: std::cell::RefCell::new(preview),
                look: std::cell::RefCell::new(look),
                on_top: std::cell::Cell::new(false),
                hide_cursor: std::cell::Cell::new(false),
            }))
        }
    }
}

/// Opens a projector: one per screen when the setting says so, above other windows when it says
/// so. → its id. The caller saves the list.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn open_projector(ctx: ViewCtx<'_>, h: &mut Held, p: &ProjectorReq, data: &std::path::Path) -> Result<u64, String> {
    let settings = h.settings.clone().unwrap_or_default();
    let c = h.collection.as_ref().ok_or(STOPPED)?;
    let of = match p.what.as_str() {
        "program" => ferrocast_studio::ProjectorOf::Program,
        "multiview" => ferrocast_studio::ProjectorOf::Multiview,
        _ => ferrocast_studio::ProjectorOf::Source(p.name.clone().unwrap_or_default()),
    };
    h.last_projector += 1;
    let id = h.last_projector;
    let data = data.to_path_buf();
    // Closed by its own button or Esc: out of the list (and dropped) after the borrow.
    let on_close = Box::new(move || {
        let gone = HELD.with(|h| {
            let mut h = h.try_borrow_mut().ok()?;
            let i = h.projectors.iter().position(|(pid, _, _)| *pid == id)?;
            let gone = h.projectors.remove(i);
            save_projectors(&h, &data);
            Some(gone)
        });
        drop(gone);
    });
    // A click on a multiview scene puts it on air, through its transition; in Studio
    // Mode it goes to the preview instead, as in OBS.
    let on_pick = Box::new(|scene: String| {
        HELD.with(|h| {
            let Ok(h) = h.try_borrow() else { return };
            let Some(c) = &h.collection else { return };
            let done = if c.preview_scene().is_some() { c.set_preview(&scene) } else { c.set_current(&scene) };
            if c.moving() {
                glide();
            }
            if let Err(e) = done {
                tracing::warn!(error = %e, "a multiview click couldn't switch scenes");
            }
            sync_views(&h);
        });
    });
    let look = multiview_look(&settings);
    let made = create_projector_win(ctx, c, p, &of, look, on_close, on_pick)?;
    if p.on_top || settings.projector_on_top {
        made.set_on_top(true);
    }
    made.set_hide_cursor(settings.projector_hide_cursor);
    // One full-screen projector per display: the one already on it goes (after the new one is
    // up, so the picture never leaves the screen).
    if settings.projector_one_per_screen && p.screen.is_some() {
        h.projectors.retain(|(_, _, r)| !same_screen(r.screen, p.screen));
    }
    h.projectors.push((id, made, p.clone()));
    Ok(id)
}

/// Keeps the open projectors for the next Start, when that is asked for.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn save_projectors(h: &Held, data: &std::path::Path) {
    if !h.settings.as_ref().is_some_and(|s| s.projector_restore) {
        return;
    }
    let list: Vec<&ProjectorReq> = h.projectors.iter().map(|(_, _, r)| r).filter(|r| r.what != "source").collect();
    save_json(&projectors_file(data), &serde_json::to_value(list).unwrap_or(Value::Null));
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn load_saved_projectors(data: &std::path::Path, settings: &StudioSettings) -> Vec<ProjectorReq> {
    if !settings.projector_restore {
        return Vec::new();
    }
    ferrocast_store::durable::load_json_with_backup::<Vec<ProjectorReq>>(&projectors_file(data), |l| {
        l.len() <= 16 && l.iter().all(|p| p.validate().is_ok())
    })
    .map(|l| l.value.into_iter().filter(|p| p.what != "source").collect())
    .unwrap_or_default()
}

/// The projectors kept from last time, opened again (a source that is gone or a display that is
/// not there is skipped, with a note in the log).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn restore_projectors_from_list(ctx: ViewCtx<'_>, h: &mut Held, data: &std::path::Path, list: Vec<ProjectorReq>) {
    if !h.settings.as_ref().is_some_and(|s| s.projector_restore) {
        return;
    }
    for p in list {
        if p.what == "source" {
            continue;
        }
        if let Err(e) = open_projector(ctx, h, &p, data) {
            tracing::warn!(error = %e, what = %p.what, "a saved projector didn't open");
        }
    }
}

#[cfg(test)]
fn apply_settings_on_main(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    s: StudioSettings,
) -> Result<Vec<String>, String> {
    apply_settings_for_profile(ctx, h, data, s, None)
}

fn apply_settings_for_profile(
    ctx: ViewCtx<'_>,
    h: &mut Held,
    data: &std::path::Path,
    s: StudioSettings,
    target: Option<(String, PathBuf)>,
) -> Result<Vec<String>, String> {
    let mut notes: Vec<String> = Vec::new();
    let prev_settings = h.settings.clone();
    // The sound rate and broadcast timing are read when the engine opens (a new canvas or format
    // reopens it just below): set first, so reopening takes them, and store the incoming settings
    // so any rebuilt multiview/projector reads them immediately (rolled back if reopening fails).
    apply_audio_format(&s);
    h.settings = Some(s.clone());
    // Streaming now: what Optimize changes (frame rate, size, encoder) goes to the
    // stream on air at once (owner, 15 Nov: "I have to end and stream again").
    let live = any_main_stream_active(h);
    let any_live = any_stream_group_active(h);
    if let Some(c) = h.collection.as_ref() {
        let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
        let now = c.canvas();
        // Only the frame rate, while live: the stream takes it below; the canvas
        // follows when the stream ends (`apply_waiting_canvas`).
        let rate_only = live && (canvas.width, canvas.height) == (now.width, now.height);
        // Changing sample rate or broadcast timing on an idle engine takes effect immediately by
        // reopening the canvas/engine once no active output holds it.
        let format_changed = c.engine_format().is_some_and(|running| restart_note(running, &s).is_some());
        let busy = outputs_busy_for_switch(h);
        if (canvas != now && !rate_only) || (!busy && format_changed) {
            if busy {
                notes.push(
                    "The new size and frame rate apply once streaming, recording, replay, NDI and the virtual camera are off."
                        .into(),
                );
            } else if let Err((e, prev_projectors, had_engine)) = try_apply_canvas(ctx, h, data, canvas) {
                h.settings = prev_settings.clone();
                apply_audio_format(prev_settings.as_ref().unwrap_or(&StudioSettings::default()));
                finish_canvas_views(ctx, h, data, prev_projectors, had_engine);
                return Err(e);
            }
        } else if rate_only && canvas.fps > now.fps {
            notes.push(format!("The stream goes up to {} fps once it ends.", canvas.fps));
        }
        if !busy {
            drop_inactive_outputs(h);
        }
        let c = h.collection.as_ref().ok_or(STOPPED)?;
        let (name, id) = s.monitoring.clone().unwrap_or(("Default".into(), "default".into()));
        if c.set_monitoring_device(&name, &id).is_err() {
            notes.push(format!("{name} can't play sound; the system's device is used."));
        }
        c.set_true_peak(s.true_peak);
        c.set_low_latency(s.low_latency_audio);
    }
    if let Some(running) = h.collection.as_ref().and_then(|c| c.engine_format())
        && let Some(note) = restart_note(running, &s)
    {
        notes.push(note);
    }
    // The stream on air takes a new encoder, size or frame rate at once, without
    // stopping; a new bitrate alone just changes the bitrate.
    let was = prev_settings;
    let was_encode = was.as_ref().map(|o| effective_stream_encode(o, h.stream_cap, h.live_pct));
    let encode = effective_stream_encode(&s, h.stream_cap, h.live_pct);
    // What changed besides the bitrate: the encoder, its options, the size, the rate.
    let shape_changed =
        was.as_ref().is_some_and(|o| was_encode.as_ref().is_some_and(|a| shape_differs(a, &encode)) || o.fps != s.fps);
    let kbps = encode.video_kbps;
    let reshaped = if let Some(st) = h.stream.as_ref().filter(|_| live && shape_changed) {
        match st.reshape(&encode, s.fps) {
            Ok(()) => {
                // What it runs with now: a recording "Same as stream" started
                // next compares with this (a 1080p stream reshaped to 720p was
                // still taken as unscaled).
                if let Some(w) = h.stream_with.as_mut() {
                    w.encode = encode;
                }
                true
            }
            Err(e) => {
                notes.push(format!("{e} The next stream uses the new settings."));
                false
            }
        }
    } else if live
        && shape_changed
        && let Some(slot) = h.groups.get_mut(GROUP_MAIN).filter(|g| g.output.stats().active)
    {
        let inferred_cap = main_slot_cap_kbps(&slot.spec, slot.cap_kbps, was.as_ref().map(|o| o.video_kbps));
        let next_spec = next_main_spec_for_settings(&slot.spec, &s, inferred_cap, was.as_ref());
        let mut next_group_settings = stream_settings_for_group(&s, &next_spec);
        let live_kbps = at_pct(next_spec.key.kbps, slot.live_pct);
        next_group_settings.encode.video_kbps = live_kbps;
        match slot.output.reshape(&next_group_settings.encode, s.fps) {
            Ok(()) => {
                if slot.cap_kbps.is_none() {
                    slot.cap_kbps = inferred_cap;
                }
                slot.spec = next_spec;
                slot.with = next_group_settings;
                true
            }
            Err(e) => {
                notes.push(format!("{e} The next stream uses the new settings."));
                false
            }
        }
    } else if any_live && shape_changed {
        notes.push("The next stream uses the new settings.".into());
        false
    } else {
        false
    };
    if live && was.as_ref().is_some_and(|o| o.audio_kbps != s.audio_kbps) {
        notes.push("The new sound bitrate starts with the next stream.".into());
    }
    let was_kbps = was.as_ref().map(|o| o.video_kbps);
    if was_kbps.is_some_and(|k| k != s.video_kbps) {
        if !reshaped && let Some(st) = h.stream.as_mut() {
            st.set_video_kbps(kbps);
        }
        if let Some(st) = h.vstream.as_mut() {
            st.set_video_kbps(at_pct(s.video_kbps, h.live_pct));
        }
        if !reshaped && let Some(slot) = h.groups.get_mut(GROUP_MAIN) {
            let cap = main_slot_cap_kbps(&slot.spec, slot.cap_kbps, was.as_ref().map(|o| o.video_kbps));
            if slot.cap_kbps.is_none() {
                slot.cap_kbps = cap;
            }
            let k = apply_main_slot_bitrate_on_spec(&mut slot.spec, cap, slot.live_pct, s.video_kbps);
            slot.output.set_video_kbps(k);
            slot.with.encode.video_kbps = k;
        }
    }
    if let Some((id, file)) = target {
        h.profile_id = Some(id);
        h.profile_file = Some(file);
    }
    let profile_path = active_profile_file(h, data);
    save_json(&profile_path, &serde_json::to_value(&s).unwrap_or(Value::Null));
    // Open multiviews and projectors take updated layout, on-top, and cursor visibility at once.
    refresh_projectors(h);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    save_projectors(h, data);
    Ok(notes)
}

#[async_trait::async_trait]
impl StudioHost for DesktopStudio {
    async fn status(&self) -> StudioStatus {
        self.on_main(|h| Self::status_of(h)).await.unwrap_or_default()
    }

    async fn start(&self) -> Result<StudioStatus, String> {
        self.start_with_mode(true).await
    }

    async fn start_with_mode(&self, load_collection: bool) -> Result<StudioStatus, String> {
        let _shelf_lock = self.shelf_lock.lock().await;
        #[cfg(windows)]
        fetch_matte(self.data.clone());
        if let Some(status) = self.on_main(|h| h.collection.as_ref().map(|_| Self::status_of(h))).await? {
            return Ok(status);
        }
        let config = self.data.join("studio");
        let data = self.data.clone();
        let cache = self.library_cache.clone();
        let prep = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let lib = library::load(&data);
            *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(lib.clone());
            let col_id = lib.collections.current.clone();
            let file = lib.current_file(&data, Kind::Collections);
            let prof_id = lib.profiles.current.clone();
            let prof_file = lib.current_file(&data, Kind::Profiles);
            let s = read_settings_from(&prof_file);
            let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
            let prepared = if load_collection {
                Some(load_prepared_collection(canvas, &file).map_err(|e| e.to_string())?)
            } else {
                None
            };
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let projectors = load_saved_projectors(&data, &s);
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let projectors: Vec<ProjectorReq> = Vec::new();
            Ok((col_id, file, prof_id, prof_file, s, canvas, prepared, projectors))
        })
        .await
        .map_err(|e| e.to_string())??;
        let (col_id, file, prof_id, prof_file, s, canvas, prepared, projectors) = prep;
        let need_projectors = !projectors.is_empty();
        let _ = &config;
        enum StartStep {
            AlreadyRunning(StudioStatus),
            Ready(StudioStatus),
            OpenEngine(ferrocast_studio::stage::EngineOpenSpec, u64),
        }
        let step = self
            .on_main(move |h| -> Result<StartStep, String> {
                if h.collection.is_some() {
                    return Ok(StartStep::AlreadyRunning(Self::status_of(h)));
                }
                apply_audio_format(&s);
                h.collection_id = Some(col_id);
                h.collection_file = Some(file.clone());
                h.profile_id = Some(prof_id);
                h.profile_file = Some(prof_file);
                h.settings = Some(s);
                h.renderer = "ferrocast";
                let collection = match prepared {
                    Some(p) => {
                        h.collection_deferred = false;
                        Collection::from_prepared(p)
                    }
                    None => {
                        // Safe Mode: start cleanly without loading the saved collection until confirmed.
                        h.collection_deferred = true;
                        Collection::new(canvas, FIRST_SCENE).map_err(|e| e.to_string())?
                    }
                };
                apply_collection_audio_prefs(&collection, h.settings.as_ref());
                save_confirmed(h.collection_deferred, &collection, &file);
                let open_spec = if need_projectors {
                    collection.set_engine_opening(true);
                    Some(collection.engine_open_spec())
                } else {
                    None
                };
                h.collection = Some(collection);
                h.collection_gen = h.collection_gen.wrapping_add(1);
                if let Some(spec) = open_spec {
                    return Ok(StartStep::OpenEngine(spec, h.collection_gen));
                }
                write_session(session_now(h).as_ref());
                let recovered = CRASHED.lock().ok().and_then(|mut c| c.take());
                Ok(StartStep::Ready(StudioStatus { recovered, ..Self::status_of(h) }))
            })
            .await??;
        match step {
            StartStep::AlreadyRunning(status) | StartStep::Ready(status) => Ok(status),
            StartStep::OpenEngine(spec, epoch_gen) => {
                let opened = tokio::task::spawn_blocking(move || Collection::open_engine_off_main(&spec))
                    .await
                    .map_err(|e| e.to_string())?;
                let app = self.app.clone();
                let data = self.data.clone();
                self.on_main(move |h| {
                    if h.collection_gen != epoch_gen || h.collection.is_none() {
                        drop(opened);
                        return Self::status_of(h);
                    }
                    if let Some(c) = h.collection.as_ref() {
                        match opened {
                            Ok(eng) => c.install_opened_engine(eng),
                            Err(e) => {
                                c.set_engine_opening(false);
                                tracing::warn!(error = %e, "the engine didn't open cleanly when starting Studio");
                            }
                        }
                    }
                    #[cfg(any(target_os = "macos", target_os = "windows"))]
                    restore_projectors_from_list(ViewCtx::App(&app), h, &data, projectors);
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    let _ = (&app, &data, projectors);
                    write_session(session_now(h).as_ref());
                    let recovered = CRASHED.lock().ok().and_then(|mut c| c.take());
                    StudioStatus { recovered, ..Self::status_of(h) }
                })
                .await
            }
        }
    }

    async fn confirm_collection(&self) -> Result<StudioStatus, String> {
        let _shelf_lock = self.shelf_lock.lock().await;
        let data = self.data.clone();
        let (running, from) = self
            .on_main({
                let data = data.clone();
                move |h| (h.collection.is_some(), active_collection_file(h, &data))
            })
            .await?;
        if !running {
            return self.on_main(|h| Self::status_of(h)).await;
        }
        let lib = self.load_library_off_main().await?;
        let to_id = lib.collections.current.clone();
        let to = lib.current_file(&data, Kind::Collections);
        swap_collection_off_main(self, data, Some(to_id), from.clone(), to).await?;
        self.on_main(|h| {
            write_session(session_now(h).as_ref());
            Self::status_of(h)
        })
        .await
    }

    async fn stop(&self) -> Result<StudioStatus, String> {
        let _shelf_lock = self.shelf_lock.lock().await;
        // Extract active outputs on main thread without blocking UI, then drain them in parallel
        // off the main thread BEFORE saving and dropping the scene collection.
        let jobs = self.on_main(take_outputs_from_held).await?;
        if !jobs.is_empty() {
            let _ = tokio::task::spawn_blocking(move || drain_outputs_bounded(jobs, QUIT_DRAIN_TIMEOUT)).await;
        }
        let data = self.data.clone();
        let (status, teardown, guests) = self
            .on_main(move |h| {
                let file = active_collection_file(h, &data);
                let deferred = h.collection_deferred;
                clear_non_output_held(h);
                let guests = std::mem::take(&mut h.guests);
                h.collection_gen = h.collection_gen.wrapping_add(1);
                let teardown = if let Some(c) = h.collection.take() {
                    save_confirmed(deferred, &c, &file);
                    Some(c.into_teardown())
                } else {
                    None
                };
                h.collection_deferred = false;
                h.collection_id = None;
                h.collection_file = None;
                h.profile_id = None;
                h.profile_file = None;
                write_session(None);
                (Self::status_of(h), teardown, guests)
            })
            .await?;
        // Tear down the outgoing engine off the main thread, unlink guest .sdp files once
        // FFmpeg closes, and flush queued scene saves before returning.
        let landed = tokio::task::spawn_blocking(move || {
            if let Some(td) = teardown {
                td.finish_off_main();
            }
            drop(guests);
            ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10))
        })
        .await
        .unwrap_or(false);
        if !landed {
            tracing::warn!("the Studio's scenes were still saving when it stopped");
        }
        Ok(status)
    }

    async fn kinds(&self) -> Result<Value, String> {
        // The kinds Ferrocast's own engine runs.
        serde_json::to_value(ferrocast_studio::kinds::kinds()).map_err(|e| e.to_string())
    }

    async fn options(&self, kind: String) -> Result<Value, String> {
        let opts = self
            .read_options(kind.clone())
            .await?
            .ok_or_else(|| format!("Ferrocast's own engine doesn't run \"{kind}\" yet."))?;
        serde_json::to_value(opts).map_err(|e| e.to_string())
    }

    async fn set_preview_paused(&self, paused: bool) -> Result<(), String> {
        self.on_main(move |h| {
            h.preview_paused = paused;
            // Only the three views over the page: projectors and every output draw on.
            for p in [h.preview.as_ref(), h.program_view.as_ref(), h.vertical.view.as_ref()].into_iter().flatten() {
                p.set_paused(paused);
            }
        })
        .await
    }

    async fn preview_paused(&self) -> bool {
        self.on_main(|h| h.preview_paused).await.unwrap_or(false)
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
                make_program_view(ViewCtx::App(&app), h)?;
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
            h.program_stripe = r.stripe_abgr();
            if let (Some(p), Some(c)) = (h.program_view.as_ref(), h.program_bg) {
                p.background(c);
            }
            if let Some(p) = h.program_view.as_ref() {
                p.overflow_stripes(h.program_stripe);
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
            v.stripe = r.stripe_abgr();
            v.holes = r.holes.iter().map(|c| (c.x, c.y, c.w, c.h, c.r)).collect();
            v.opacity = r.opacity;
            v.shade = r.shade.unwrap_or(0.0);
            let Some(p) = v.view.as_mut() else { return make_vertical_view(ViewCtx::App(&app), h) };
            if let Some(bg) = v.bg {
                p.background(bg);
            }
            p.overflow_stripes(v.stripe);
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
            h.preview_stripe = r.stripe_abgr();
            h.preview_holes = r.holes.iter().map(|c| (c.x, c.y, c.w, c.h, c.r)).collect();
            h.preview_opacity = r.opacity;
            h.preview_shade = r.shade.unwrap_or(0.0);
            if let Some(p) = h.preview.as_mut() {
                if let Some(c) = h.preview_bg {
                    p.background(c);
                }
                p.overflow_stripes(h.preview_stripe);
                match h.preview_canvas {
                    Some(c) => p.set_area(rect, c, scale),
                    None => p.set_rect(rect, scale),
                }
                p.set_holes(&h.preview_holes);
                p.set_opacity(h.preview_opacity.unwrap_or(1.0));
                p.set_shade(h.preview_shade);
                return Ok(());
            }
            make_preview(ViewCtx::App(&app), h)
        })
        .await?
    }

    async fn defaults(&self, kind: String) -> Result<Value, String> {
        ferrocast_studio::kinds::defaults(&kind)
            .ok_or_else(|| format!("Ferrocast's own engine doesn't run \"{kind}\" yet."))
    }

    fn capture_keys(&self, on: bool) {
        crate::studio_keys::capture(&self.app, on);
    }

    fn local_capture_block(&self) -> ferrocast_engine::studio_host::LocalCaptureBlock {
        current_local_capture_block()
    }

    fn levels_now(&self, names: &[String], fast: bool) -> Option<Value> {
        serde_json::to_value(ferrocast_studio::stage::levels_now(names, fast)?).ok()
    }

    async fn act(&self, act: StudioAct) -> Result<Value, String> {
        // A source's form: its kind found on the main thread, the form read off it (devices).
        if let StudioAct::Options { name } = &act {
            let n = name.clone();
            let kind = self
                .on_main(move |h| -> Result<String, String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    c.kind(&n).map_err(|e| e.to_string())
                })
                .await??;
            let mut opts = self
                .read_options(kind)
                .await?
                .ok_or_else(|| format!("Ferrocast's own engine can't show \"{name}\" yet."))?;
            // The choices it takes from the collection (a clone's sources, what sound bars hear):
            // a kind's form read on its own has them empty (owner, 10 Nov: "Nothing to choose").
            let n = name.clone();
            let opts = self
                .on_main(move |h| -> Result<_, String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    c.fill_choices(&n, &mut opts).map_err(|e| e.to_string())?;
                    Ok(opts)
                })
                .await??;
            return serde_json::to_value(opts).map_err(|e| e.to_string());
        }
        // Unified picker still (screen, window, camera, capture card) on every platform:
        // 1) skips fast-path reuse and invalidates 15 s cache when a camera is waiting or gone;
        // 2) reuses an already-open live stage source (zero hardware contention / camera LED flash);
        // 3) deduplicates concurrent asks and returns `{ image: null, waiting: true }` for
        //    transient camera disconnects/timeouts while keeping `error` for permission/in-use.
        if matches!(&act, StudioAct::ScreenStill { .. } | StudioAct::SourceStill { .. }) {
            let (target_kind, key_name, val_json, target, width, fresh) = match &act {
                StudioAct::ScreenStill { id, window, width, fresh } => {
                    let w = (*width).clamp(64, 320);
                    let tk = if *window { "window_capture" } else { "screen_capture" }.to_string();
                    let kn = if *window { "window" } else { "display" }.to_string();
                    let tgt = if *window {
                        source_still::StillTarget::Window(*id, w)
                    } else {
                        source_still::StillTarget::Display(*id, w)
                    };
                    (tk, kn, json!(*id), tgt, w, *fresh)
                }
                StudioAct::SourceStill { kind, key, value, width, fresh } => {
                    let w = (*width).clamp(64, 320);
                    let tgt = if matches!(key.as_str(), "window") || kind.contains("window") {
                        let Some(id) = value.as_u64().and_then(|n| u32::try_from(n).ok()) else {
                            return Ok(json!({ "image": null }));
                        };
                        source_still::StillTarget::Window(id, w)
                    } else if matches!(key.as_str(), "display" | "monitor_id")
                        || kind.contains("screen")
                        || kind.contains("monitor")
                        || kind == "xshm_input"
                    {
                        let Some(id) = value.as_u64().and_then(|n| u32::try_from(n).ok()) else {
                            return Ok(json!({ "image": null }));
                        };
                        source_still::StillTarget::Display(id, w)
                    } else {
                        let Some(id) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                            return Ok(json!({ "image": null }));
                        };
                        source_still::StillTarget::Camera(id.to_string(), w)
                    };
                    (kind.clone(), key.clone(), value.clone(), tgt, w, *fresh)
                }
                _ => unreachable!(),
            };
            return source_still::resolve_fresh(
                source_still::global_cache(),
                source_still::global_flights(),
                source_still::global_turns(),
                target,
                fresh,
                || {
                    let tk = target_kind.clone();
                    let kn = key_name.clone();
                    let vj = val_json.clone();
                    async move {
                        self.on_main(move |h| stage_still_check(h.collection.as_ref(), &tk, &kn, &vj, width))
                            .await
                            .unwrap_or((ferrocast_studio::stage::MatchingCameraWait::LiveOrAbsent, None))
                    }
                },
                ferrocore::camera::hotplug_epoch,
                |cam_id| async move {
                    tokio::task::spawn_blocking(move || ferrocore::camera::is_connected(&cam_id)).await.unwrap_or(false)
                },
                |capture_target, flag, permit| async move {
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        source_still::probe_still(capture_target, &flag)
                    })
                    .await
                    .unwrap_or_else(|e| source_still::StillAnswer::error(e.to_string()))
                },
            )
            .await;
        }
        // How a source is doing: its settings read on the main thread, a Browser source's
        // address tried off it (the app's web client, its time limit).
        if let StudioAct::SourceStatus { name } = &act {
            let n = name.clone();
            let (kind, settings, camera_status) = self
                .on_main(move |h| -> Result<(String, Value, Option<ferrocore::engine::CameraStatus>), String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    Ok((
                        c.kind(&n).map_err(|e| e.to_string())?,
                        c.settings(&n).map_err(|e| e.to_string())?,
                        c.camera_status(&n),
                    ))
                })
                .await??;
            // Trimmed, as the engine keeps them (a Web text's reads are noted under these).
            let text = |k: &str| settings.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
            if let Some(camera_json) = camera_source_status_json(&kind, camera_status) {
                return Ok(camera_json);
            }
            return Ok(match kind.as_str() {
                "ferrocast_web_text" => match ferrocore::source::webtext::status(&text("url"), &text("path")) {
                    Some(s) => json!({ "web_text": { "at_ms": s.at_ms, "value": s.value, "error": s.error } }),
                    None => json!({ "web_text": null }),
                },
                "browser_source" | "ferrocast_web"
                    if !settings.get("is_local_file").and_then(Value::as_bool).unwrap_or(false) && {
                        let u = text("url").to_ascii_lowercase();
                        u.starts_with("http://") || u.starts_with("https://")
                    } =>
                {
                    let url = text("url");
                    ferrocast_studio::webfetch::install();
                    let r = tokio::task::spawn_blocking(move || ferrocore::source::webtext::check(&url))
                        .await
                        .map_err(|e| e.to_string())?;
                    json!({ "page": { "ok": r.is_ok(), "error": r.err() } })
                }
                // An app capture: the displays the app shows windows on, so the drawer can say
                // when it has none on the one picked (the picture is then empty).
                #[cfg(target_os = "macos")]
                "screen_capture" if settings.get("type").and_then(Value::as_u64) == Some(2) => {
                    let app = text("application");
                    if app.is_empty() {
                        json!({})
                    } else {
                        let on = tokio::task::spawn_blocking(move || ferrocore::screen::app_displays(&app))
                            .await
                            .map_err(|e| e.to_string())?
                            .unwrap_or_default();
                        json!({ "app_on": on })
                    }
                }
                _ => json!({}),
            });
        }
        // VST plugins: found, and each filter's plugin asked, off the main thread (a helper can
        // take a moment); its state saved into the filter on the main thread.
        if let StudioAct::VstPlugins { rescan } = &act {
            let rescan = *rescan;
            let why = ferrocast_vsthost::available().err();
            let plugins = tokio::task::spawn_blocking(move || ferrocast_vsthost::plugins(rescan))
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({ "available": why.is_none(), "why": why, "plugins": plugins }));
        }
        if let StudioAct::Vst { name, filter, what } = &act {
            // Slots stay when a filter's plugin is changed: the newest one running (CodeRabbit).
            let live = ferrocore::afx::plugin_specs()
                .into_iter()
                .filter(|(_, p)| p.source == *name && p.filter == *filter)
                .rev()
                .find_map(|(slot, _)| ferrocast_vsthost::live(slot));
            let not_running = || {
                ferrocast_vsthost::available()
                    .err()
                    .unwrap_or_else(|| "Its plugin isn't running: pick one, and put the source in a scene.".into())
            };
            return match what.as_str() {
                "status" => Ok(match &live {
                    Some(l) => json!({ "running": l.running(), "problem": l.problem(), "changed": l.changed() }),
                    None => json!({ "running": false, "problem": not_running(), "changed": false }),
                }),
                "open" | "close" => {
                    let l = live.ok_or_else(not_running)?;
                    let open = what == "open";
                    tokio::task::spawn_blocking(move || l.window(open)).await.map_err(|e| e.to_string())??;
                    Ok(json!({}))
                }
                _ => {
                    let l = live.ok_or_else(not_running)?;
                    let got = tokio::task::spawn_blocking(move || l.state()).await.map_err(|e| e.to_string())?;
                    let Some(bytes) = got else { return Ok(json!({ "saved": false })) };
                    let state = ferrocast_vsthost::encode_state(&bytes);
                    let (n, f) = (name.clone(), filter.clone());
                    let data = self.data.clone();
                    self.on_main(move |h| -> Result<(), String> {
                        let c = h.collection.as_ref().ok_or(STOPPED)?;
                        c.update_filter(&n, &f, &json!({ "state": state })).map_err(|e| e.to_string())?;
                        // Written now: a crash right after keeps what was saved (CodeRabbit).
                        save_confirmed(h.collection_deferred, c, &active_collection_file(h, &data));
                        Ok(())
                    })
                    .await??;
                    Ok(json!({ "saved": true }))
                }
            };
        }
        // Pictures: drawn on the main thread, turned into a PNG off it (a 1080p PNG took tens of
        // milliseconds there, and the scene grid asks for one every few seconds).
        // A source's own picture (the crop editor's): the same, without its filters.
        if let StudioAct::SourcePicture { name, width, filters, item } = &act {
            let (n, w, f, it) = (name.clone(), *width, *filters, item.clone());
            let label = it.as_ref().map_or_else(|| n.clone(), |t| t.scene.clone());
            // Only what's needed is read on the main thread; the drawing, the read back from the
            // graphics card and the PNG happen off it (they held the window up each time).
            type PicJob = Box<dyn FnOnce() -> Option<(Vec<u8>, u32, u32, (u32, u32))> + Send>;
            let job = self
                .on_main(move |h| -> Result<Option<PicJob>, String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    match &it {
                        Some(target) => c
                            .item_picture_job_for((!n.is_empty()).then_some(n.as_str()), &target.scene, target.id, w, f)
                            .map(|opt| opt.map(|j| Box::new(j) as PicJob))
                            .map_err(|e| e.to_string()),
                        None => c
                            .source_picture_job(&n, w, f)
                            .map(|opt| opt.map(|j| Box::new(j) as PicJob))
                            .map_err(|e| e.to_string()),
                    }
                })
                .await??;
            let Some(job) = job else { return Ok(json!({ "image": null })) };
            let got = tokio::task::spawn_blocking(move || {
                job().map(|(px, pw, ph, size)| ferrocast_studio::png_data_url(&px, pw, ph, &label).map(|i| (i, size)))
            })
            .await
            .map_err(|e| e.to_string())?;
            let Some(made) = got else { return Ok(json!({ "image": null })) };
            let (image, (sw, sh)) = made.map_err(|e| e.to_string())?;
            return Ok(json!({ "image": image, "width": sw, "height": sh }));
        }
        if let StudioAct::Thumbnail { scene, width } = &act {
            let (s, w) = (scene.clone(), *width);
            let job = self
                .on_main(move |h| -> Result<_, String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    c.thumbnail_job(&s, w).map_err(|e| e.to_string())
                })
                .await??;
            let scene = scene.clone();
            let image = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let (px, pw, ph) = job().map_err(|e| e.to_string())?;
                ferrocast_studio::png_data_url(&px, pw, ph, &scene).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())??;
            return Ok(json!({ "image": image }));
        }
        if matches!(act, StudioAct::Screenshot { .. } | StudioAct::ScreenshotProgram) {
            let data = self.data.clone();
            let shot = act.clone();
            let (folder, job) = self
                .on_main(move |h| -> Result<(PathBuf, ScreenshotJob), String> {
                    // Next to the recordings, as OBS does; only capture folder + job handle on
                    // the main thread so sleep, GPU render/readback, and file I/O run on a worker.
                    let folder = record_folder(ensure_held_settings(h, &data));
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    let job: ScreenshotJob = match &shot {
                        StudioAct::Screenshot { name, item: Some(target) } => {
                            let j = c
                                .screenshot_item_job_for(
                                    (!name.is_empty()).then_some(name.as_str()),
                                    &target.scene,
                                    target.id,
                                )
                                .map_err(|e| e.to_string())?;
                            Box::new(move || j().map_err(|e| e.to_string()))
                        }
                        StudioAct::Screenshot { name, item: None } => {
                            let j = c.screenshot_job(name).map_err(|e| e.to_string())?;
                            Box::new(move || j().map_err(|e| e.to_string()))
                        }
                        _ => {
                            let j = c.screenshot_program_job().map_err(|e| e.to_string())?;
                            Box::new(move || j().map_err(|e| e.to_string()))
                        }
                    };
                    Ok((folder, job))
                })
                .await??;
            return finish_screenshot_job(folder, job).await;
        }
        if act == StudioAct::Scenes {
            let lib = self.cached_library().await;
            let data = self.data.clone();
            enum ScenesSource {
                Live(Value),
                Stopped(PathBuf, String),
            }
            match self
                .on_main(move |h| match &h.collection {
                    Some(c) => {
                        let col_id = h.collection_id.clone().unwrap_or(lib.collections.current);
                        let mut v = run(c, StudioAct::Scenes).map_err(|e| e.to_string())?;
                        if let Some(o) = v.as_object_mut() {
                            o.insert("collection_id".into(), json!(col_id));
                        }
                        Ok(ScenesSource::Live(v))
                    }
                    None => {
                        let file =
                            h.collection_file.clone().unwrap_or_else(|| lib.current_file(&data, Kind::Collections));
                        let col_id = h.collection_id.clone().unwrap_or(lib.collections.current);
                        Ok(ScenesSource::Stopped(file, col_id))
                    }
                })
                .await?
            {
                Ok(ScenesSource::Live(v)) => return Ok(v),
                Ok(ScenesSource::Stopped(file, col_id)) => {
                    return tokio::task::spawn_blocking(move || {
                        let saved =
                            ferrocast_store::durable::load_json_with_backup::<Value>(&file, |_| true).map(|l| l.value);
                        let mut v = saved_scenes(saved.as_ref());
                        if let Some(o) = v.as_object_mut() {
                            o.insert("collection_id".into(), json!(col_id));
                        }
                        v
                    })
                    .await
                    .map_err(|e| e.to_string());
                }
                Err(e) => return Err(e),
            }
        }
        let data = self.data.clone();
        let app = self.app.clone();
        let cache = self.library_cache.clone();
        self.on_main(move |h| {
            if let Some(ep) = act_shortcut_epoch(&act)
                && !crate::studio_keys::epoch_matches(&app, ep)
            {
                return Ok(Value::Null);
            }
            let file = h.collection_file.clone().unwrap_or_else(|| {
                cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .unwrap_or_else(|| library::load(&data))
                    .current_file(&data, Kind::Collections)
            });
            if let StudioAct::Select { scene, id, frames, also, guides } = &act {
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
                    p.guides(*guides && h.selected.is_some());
                }
                return Ok(Value::Null);
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let changes = act.changes();
            // An edit lands on items in their own place, never half way through a Move.
            if changes {
                c.finish_moves();
            }
            if act == StudioAct::History {
                let (undo, redo) = h.history.list();
                return Ok(json!({ "undo": undo, "redo": redo }));
            }
            if matches!(act, StudioAct::Undo | StudioAct::Redo) {
                let done = if act == StudioAct::Undo { h.history.undo(c) } else { h.history.redo(c) };
                // A scene that came back or went changes what the pictures show.
                sync_views(h);
                let done = done?;
                save_confirmed(h.collection_deferred, c, &file);
                refresh_projectors(h);
                let (undo, redo) = h.history.labels();
                return Ok(json!({ "done": done, "undo": undo, "redo": redo }));
            }
            if let Some(why) = refuses_while_shared(&act, shares_encoder(h)) {
                return Err(why.into());
            }
            let before = h.history.before(c, &act);
            let turns_vertical = matches!(act, StudioAct::StartVertical { .. } | StudioAct::StopVertical);
            if turns_vertical {
                // The preview goes before its canvas does; the vertical stream too when the
                // picture goes (turning it on when it's on changes nothing).
                h.vertical.view = None;
                if act == StudioAct::StopVertical {
                    h.vstream = None;
                    h.groups.retain(|_, g| g.spec.key.canvas != EncodeCanvas::Vertical);
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
                let _ = make_vertical_view(ViewCtx::App(&app), h);
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            if let Some(b) = before {
                h.history.after(c, b, &out);
            }
            sync_views(h);
            if changes && out.is_ok() {
                save_confirmed(h.collection_deferred, c, &file);
                refresh_projectors(h);
            }
            out
        })
        .await?
    }

    async fn settings(&self) -> StudioSettings {
        let data = self.data.clone();
        let mut s = self.on_main(move |h| ensure_held_settings(h, &data).clone()).await.unwrap_or_default();
        s.record_folder_default = record_folder(&StudioSettings::default()).display().to_string();
        s
    }

    async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String> {
        let _lock = self.shelf_lock.lock().await;
        self.set_settings_for(s, None).await
    }

    async fn stats(&self) -> Result<Value, String> {
        let data = self.data.clone();
        let cache = self.library_cache.clone();
        self.on_main(move |h| {
            let (collection_file, col_id, prof_id) = match (&h.collection_file, &h.collection_id, &h.profile_id) {
                (Some(f), Some(c), Some(p)) => (f.clone(), c.clone(), p.clone()),
                _ => {
                    let lib =
                        cache.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or_else(|| library::load(&data));
                    (
                        h.collection_file.clone().unwrap_or_else(|| lib.current_file(&data, Kind::Collections)),
                        h.collection_id.clone().unwrap_or(lib.collections.current),
                        h.profile_id.clone().unwrap_or(lib.profiles.current),
                    )
                }
            };
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
            // The page asks for stats every second, so scenes held for the grid's pictures
            // are let go soon after it closes.
            if let Some(c) = &h.collection {
                c.let_thumbs_go(false);
                // Sources added before their first frame get their snug fit now.
                c.settle_fits();
                // A captured window resized keeps its shape on the canvas (and stays so after
                // a restart).
                if c.follow_sizes() {
                    save_confirmed(h.collection_deferred, c, &collection_file);
                }
            }
            let disk_free = {
                let folder = record_folder(&h.settings.clone().unwrap_or_default());
                let at = folder.ancestors().find(|p| p.exists()).map(PathBuf::from).unwrap_or(folder);
                ferrocast_studio::system::free_disk_mb(&at)
            };
            let mut out = json!({
                "engine": studio.stats(),
                // The scene on air, so the page notices switches made elsewhere (shortcuts,
                // a multiview click).
                "scene": h.collection.as_ref().map(|c| c.current()),
                // What Undo and Redo would do (labels for the buttons).
                "history": { "undo": h.history.labels().0, "redo": h.history.labels().1 },
                "canvas": studio.canvas(),
                "stream": main_stream_stats(h).map(|(s, started)| stats_of(s, started)),
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
                "virtualcam": h.virtualcam.as_ref().map(|c| json!({
                    "active": c.active(),
                    "every_app": c.every_app(),
                    "source": h.virtualcam_source,
                })),
                "ndi": h.ndi.as_ref().filter(|n| n.active()).map(|n| json!({ "active": true, "name": n.name() })),
                // Captures on air that show nothing (the black screen fixer).
                "captures": h.collection.as_ref().map(|c| c.capture_problems()).unwrap_or_default(),
                // Pictures each source has sent to the card, and their size: the page works out
                // each one's rate between two reads ("What's slowing my stream?").
                "sources": h.collection.as_ref().map(|c| c.source_load()).unwrap_or_default().into_iter()
                    .map(|l| json!({
                        "name": l.name,
                        "sent": l.sent,
                        "width": l.width,
                        "height": l.height,
                        "waiting": l.waiting,
                    }))
                    .collect::<Vec<_>>(),
                // The last remux (OBS's Remux, or MKV made into MP4 after recording).
                "remux": REMUX.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            });
            let cycles = h.collection.as_ref().map(|c| c.sidechain_cycles()).unwrap_or_default();
            if let Some(o) = out.as_object_mut() {
                o.insert("collection_id".into(), json!(col_id));
                o.insert("profile_id".into(), json!(prof_id));
                if !cycles.is_empty() {
                    o.insert(
                        "sidechain_cycles".into(),
                        json!(
                            cycles
                                .into_iter()
                                .map(|(source, key)| json!({ "source": source, "key": key }))
                                .collect::<Vec<_>>()
                        ),
                    );
                }
            }
            Ok(out)
        })
        .await?
    }

    async fn obsws_snapshot(&self) -> Option<Result<Value, String>> {
        let (base, col_id, prof_id) = match self
            .on_main(|h| -> Result<_, String> {
                let c = h.collection.as_ref().ok_or(STOPPED)?;
                let program = c.current();
                let preview = c.preview_scene();
                let mut scene_items = Vec::new();
                if !program.is_empty()
                    && let Ok(items) = c.items(&program)
                {
                    scene_items.push(json!({ "scene": program, "items": items }));
                }
                if let Some(p) = &preview
                    && p != &program
                    && let Ok(items) = c.items(p)
                {
                    scene_items.push(json!({ "scene": p, "items": items }));
                }
                Ok((
                    json!({
                        "program": program,
                        "preview": preview,
                        "stream": any_stream_group_active(h),
                        "record": h.record.as_ref().map(|r| {
                            let st = r.0.record_stats();
                            json!({
                                "active": st.active,
                                "paused": st.paused,
                                "file": r.1.display().to_string(),
                            })
                        }),
                        "replay": h.replay.as_ref().map(|r| json!({
                            "active": r.active(),
                            "last": r.last_replay().map(|p| p.display().to_string()),
                        })),
                        "vcam": h.virtualcam.as_ref().is_some_and(|vc| vc.active()),
                        "inputs": c.all_audio(),
                        "scene_items": scene_items,
                    }),
                    h.collection_id.clone(),
                    h.profile_id.clone(),
                ))
            })
            .await
        {
            Ok(Ok(parts)) => parts,
            Ok(Err(e)) => return Some(Err(e)),
            Err(e) => return Some(Err(e)),
        };
        let lib = self.cached_library().await;
        Some(Ok(obsws_snapshot_json(base, &lib, col_id.as_deref(), prof_id.as_deref())))
    }

    async fn stalled_sources(&self) -> Vec<(String, bool)> {
        self.on_main(|h| h.collection.as_ref().map(|c| c.stalled_pages()).unwrap_or_default()).await.unwrap_or_default()
    }

    async fn take_notices(&self) -> Vec<(String, String, String)> {
        self.on_main(|h| {
            h.collection
                .as_ref()
                .map(|c| c.take_notices().into_iter().map(|n| (n.key.to_string(), n.line, n.detail)).collect())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    async fn record_stats(&self) -> Result<Value, String> {
        self.on_main(|h| {
            h.record.as_ref().map_or(Value::Null, |r| {
                let mut v = record_stats(r);
                if let Some(o) = v.as_object_mut() {
                    // What to do about it, for behind the (i); and the room left where it writes.
                    if let Some((_, detail)) = r.0.record_problem() {
                        o.insert("error_detail".into(), Value::String(detail));
                    }
                    let folder = r.1.parent().map(PathBuf::from).unwrap_or_default();
                    let at = folder.ancestors().find(|p| p.exists()).map(PathBuf::from).unwrap_or(folder);
                    o.insert("disk_free_mb".into(), json!(ferrocast_studio::system::free_disk_mb(&at)));
                }
                v
            })
        })
        .await
    }

    async fn record(&self, on: bool) -> Result<Value, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        tracing::info!(on, "the recording is asked to {}", if on { "start" } else { "stop" });
        self.on_main(move |h| {
            if !on {
                tracing::info!("the recording's stop reached the main thread");
                // Waits for the file to be finished; MKV, FLV and TS then become MP4 too.
                let ended = h.record.take();
                let s = h.settings.clone().unwrap_or_default();
                if let Some((rec, file, started)) = ended {
                    drop(rec);
                    // By the file's own type (the setting may have changed while recording).
                    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                    if s.record_remux && ["mkv", "flv", "ts"].contains(&ext.as_str()) {
                        // Every part, when it was split (a second's slack for the clock).
                        let since =
                            std::time::SystemTime::now() - started.elapsed() - std::time::Duration::from_secs(1);
                        if let Err(e) = remux_in_background(recording_parts(&file, since)) {
                            // Said in the stats (as a remux that ended with a problem) and the log.
                            tracing::warn!(error = %e, "the recording wasn't remuxed to MP4");
                            let f = file.display().to_string();
                            set_remux(json!({ "from": f, "to": null, "percent": 0.0, "done": true, "error": e }));
                        }
                    }
                }
                apply_waiting_canvas(&app, h, &data);
                return Ok(Value::Null);
            }
            if let Some(r) = h.record.as_ref().filter(|r| r.0.stats().active) {
                return Ok(record_stats(r));
            }
            h.record = None;
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
            let s = h.settings.clone().unwrap_or_default();
            let folder = record_folder(&s);
            std::fs::create_dir_all(&folder)
                .map_err(|e| format!("The recordings folder {} can't be made: {e}", folder.display()))?;
            let main_group = if h.stream.is_none() { h.groups.get(GROUP_MAIN) } else { None };
            let plan = ferrocast_studio::RecordPlan {
                folder,
                name: s.record_name.clone(),
                format: s.record_format.clone(),
                spaces: s.record_spaces,
                overwrite: s.record_overwrite,
                split_minutes: s.record_split_minutes,
                split_mb: s.record_split_mb,
                tracks: s.record_tracks.clone(),
                encode: record_encode_for_stream(&s, h.stream_cap, main_group),
                muxer: s.record_muxer.clone(),
                own_encoder: s.own_encoder,
            };
            let file = studio.record_path(&plan);
            // Ferrocast's own engine draws, mixes and encodes every recording; what it can't do
            // yet is refused with the reason (libobs no longer stands in).
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            // "Same as stream" while streaming: one encode for both (the Mac's one video
            // encoder can't do two at 4K60: 31 of 60 fps, 19 Nov), when they can share.
            let live =
                h.stream.as_ref().filter(|st| st.stats().active).zip(h.stream_with.as_ref()).or_else(|| {
                    h.groups.get(GROUP_MAIN).filter(|g| g.output.stats().active).map(|g| (&g.output, &g.with))
                });
            let rec = match live.map(|(st, with)| {
                let can = if c.started(st) {
                    ferrocast_studio::stage::can_share(
                        with,
                        &plan,
                        s.record_quality == "stream",
                        c.canvas().fps,
                        s.fps,
                        c.has_selective_items(),
                    )
                } else {
                    Err("the stream runs in another collection".to_string())
                };
                (st, can)
            }) {
                Some((st, Ok(()))) => c.record_sharing(st, &file, &plan),
                Some((_, Err(why))) => {
                    tracing::info!("the recording has its own encoder: {why}");
                    c.record_drawn(&file, &plan)
                }
                None => c.record_drawn(&file, &plan),
            }
            .map_err(|e| e.to_string())?;
            tracing::info!(file = %file.display(), "the Studio is recording");
            let r = (rec, file, std::time::Instant::now());
            let v = record_stats(&r);
            h.record = Some(r);
            h.record_with = Some((plan, s.record_quality == "stream"));
            Ok(v)
        })
        .await?
    }

    async fn guest_join(&self, name: String, offer: String) -> Result<String, String> {
        /// Guests at once, at most.
        const MAX_GUESTS: usize = 8;
        static JOINS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = JOINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Each join its own file, so a guest coming back reopens their source on the new one.
        let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let sdp = self.data.join("studio").join("guests").join(format!("{slug}-{n}.sdp"));
        let (answer, guest) = tokio::task::spawn_blocking(move || ferrocast_guest::Guest::accept(&offer, &sdp, None))
            .await
            .map_err(|_| "The Studio couldn't take the guest.".to_string())?
            .map_err(|e| e.to_string())?;
        let data = self.data.clone();
        self.on_main(move |h| -> Result<String, String> {
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            h.guests.retain(|(_, _, g)| !g.status().ended);
            if h.guests.len() >= MAX_GUESTS {
                return Err(format!(
                    "The Studio has {MAX_GUESTS} guests already. Ask the streamer to hang up on one first."
                ));
            }
            // A name someone here now has gets a source of its own: a newcomer never takes over a
            // guest's source (which may be on air). One whose guest has gone (no media coming
            // in: they left, even if the connection hasn't noticed yet) is theirs again.
            let taken = |src: &str| h.guests.iter().any(|(_, s, g)| s == src && g.status().receiving);
            let mut source = format!("Guest: {name}");
            let mut n = 2;
            while taken(&source) {
                source = format!("Guest: {name} {n}");
                n += 1;
            }
            let settings = ferrocast_guest::source_settings(guest.sdp());
            if c.sources().iter().any(|s| s.name == source) {
                c.update(&source, &settings).map_err(|e| e.to_string())?;
            } else {
                // Hidden at first: a guest never appears on air until the streamer shows them.
                let scene = c.current();
                let item =
                    c.add_source(&scene, "ffmpeg_source", &source, &settings, false).map_err(|e| e.to_string())?;
                c.edit_item(&scene, item.id, ItemEdit::Hide).map_err(|e| e.to_string())?;
            }
            save_confirmed(h.collection_deferred, c, &active_collection_file(h, &data));
            // The connection whose source this was, if any, is over.
            h.guests.retain(|(_, s, _)| *s != source);
            h.guests.push((name, source, guest));
            Ok(answer)
        })
        .await?
    }

    async fn guests(&self) -> Result<Value, String> {
        self.on_main(|h| {
            // The ones that ended go (their source stays).
            h.guests.retain(|(_, _, g)| !g.status().ended);
            let list: Vec<Value> = h
                .guests
                .iter()
                .map(|(name, source, g)| {
                    let s = g.status();
                    json!({ "name": name, "source": source, "connected": s.connected, "video": s.video, "audio": s.audio })
                })
                .collect();
            Value::Array(list)
        })
        .await
    }

    async fn guest_leave(&self, name: String) -> Result<(), String> {
        self.on_main(move |h| {
            let before = h.guests.len();
            // By their source (one guest), else by name (two guests may share one).
            if h.guests.iter().any(|(_, s, _)| *s == name) {
                h.guests.retain(|(_, s, _)| *s != name);
            } else {
                h.guests.retain(|(n, _, _)| *n != name);
            }
            if h.guests.len() == before { Err(format!("{name} isn't here.")) } else { Ok(()) }
        })
        .await?
    }

    async fn record_source(&self, name: String, on: bool) -> Result<Value, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            let at = h.source_records.iter().position(|(n, _, _)| *n == name);
            if !on {
                return Ok(stop_source_record(ViewCtx::App(&app), h, &data, &name));
            }
            if let Some(i) = at {
                if h.source_records[i].1.stats().active {
                    return Ok(json!({ "file": h.source_records[i].2.display().to_string() }));
                }
                // It stopped by itself (a full disk…): started again below.
                h.source_records.remove(i);
            }
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
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
                encode: record_encode(&s),
                muxer: s.record_muxer.clone(),
                own_encoder: false,
            };
            let file = studio.record_path(&plan);
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let rec = c.record_source_drawn(&name, &file, &plan).map_err(|e| e.to_string())?;
            tracing::info!(source = %name, file = %file.display(), "the Studio is recording a source on its own");
            let v = json!({ "file": file.display().to_string() });
            h.source_records.push((name, rec, file));
            Ok(v)
        })
        .await?
    }

    async fn replay(&self, on: bool) -> Result<Value, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            if !on {
                h.replay = None;
                apply_waiting_canvas(&app, h, &data);
                return Ok(json!({ "active": false }));
            }
            if h.replay.as_ref().is_some_and(|r| r.active()) {
                return Ok(json!({ "active": true }));
            }
            h.replay = None;
            h.collection.as_ref().ok_or(STOPPED)?;
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
                mb: s.replay_cap_mb(),
                tracks: s.record_tracks.clone(),
                encode: record_encode(&s),
            };
            // Kept by Ferrocast's own engine (libobs no longer stands in).
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let rb = c.replay_drawn(&plan).map_err(|e| e.to_string())?;
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
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            h.virtualcam = None;
            if !on {
                // A frame rate chosen while it ran can be applied now (review).
                apply_waiting_canvas(&app, h, &data);
                return Ok(json!({ "active": false }));
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                let cam = c.virtual_camera(source.as_deref()).map_err(|e| e.to_string())?;
                let every_app = cam.every_app();
                h.virtualcam = Some(cam);
                h.virtualcam_source = source;
                Ok(json!({ "active": true, "every_app": every_app }))
            }
            // A Mac needs a camera extension signed by an Apple developer account.
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = (c, &source);
                Err::<Value, String>(
                    "The virtual camera works on Windows and Linux so far: a Mac needs a camera signed with an Apple developer account, which comes later."
                        .into(),
                )
            }
        })
        .await?
    }

    async fn virtual_camera_setup(&self) -> Result<Value, String> {
        #[cfg(target_os = "windows")]
        return serde_json::to_value(ferrocast_studio::wincam::setup()).map_err(|e| e.to_string());
        #[cfg(not(target_os = "windows"))]
        Ok(json!({ "every_app_possible": false, "every_app_on": false, "store": false }))
    }

    async fn set_virtual_camera_every_app(&self, on: bool) -> Result<Value, String> {
        #[cfg(target_os = "windows")]
        {
            // Waits for the UAC prompt (up to two minutes), off the main thread.
            let setup = tokio::task::spawn_blocking(move || ferrocast_studio::wincam::set_every_app(on))
                .await
                .map_err(|e| e.to_string())??;
            // A camera that's on moves to the new kind straight away; if it can't start again,
            // say so rather than report the setup alone.
            self.on_main(|h| -> Result<(), String> {
                if h.virtualcam.take().is_some() {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    let cam = c
                        .virtual_camera(h.virtualcam_source.as_deref())
                        .map_err(|e| format!("The camera's setup changed, but the camera couldn't start again: {e}"))?;
                    h.virtualcam = Some(cam);
                }
                Ok(())
            })
            .await??;
            serde_json::to_value(setup).map_err(|e| e.to_string())
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = on;
            Err("Only Windows has a camera to set up for every app.".into())
        }
    }

    async fn ndi_output(&self, on: bool, name: Option<String>) -> Result<Value, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            h.ndi = None;
            if !on {
                // A frame rate chosen while it ran can be applied now (review).
                apply_waiting_canvas(&app, h, &data);
                return Ok(json!({ "active": false, "name": null }));
            }
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let name = name.unwrap_or_else(|| "Ferrocast".into());
            let out = c.ndi_output(&name).map_err(|e| e.to_string())?;
            h.ndi = Some(out);
            Ok(json!({ "active": true, "name": name }))
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
                stop_captions(h);
                if let Ok(mut e) = CAPTIONS_ERROR.lock() {
                    *e = Some(why);
                }
            }
            match set {
                Some(CaptionsSet { ask: true, .. }) => ferrocast_studio::ask_captions_access(),
                Some(CaptionsSet { on: false, .. }) => {
                    stop_captions(h);
                }
                Some(set @ CaptionsSet { on: true, .. }) => {
                    let source = set.source.clone().unwrap_or_default().trim().to_string();
                    // The old listener goes first: one at a time.
                    stop_captions(h);
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    let run = h.captions_run;
                    let listening = if set.engine.as_deref() == Some("whisper") {
                        let model = set.model_path.clone().ok_or("The speech model isn't downloaded yet.")?;
                        let tap = c.tap_sound(&source).map_err(|e| e.to_string())?;
                        let app_cb = app.clone();
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
                                let _ = app_cb.run_on_main_thread(move || deliver_caption(run, said, last));
                            },
                        );
                        Listening::Whisper { _tap: tap, listener }
                    } else {
                        let app_cb = app.clone();
                        Listening::Apple(
                            c.start_captions(&source, move |said, last| {
                                let _ = app_cb.run_on_main_thread(move || deliver_caption(run, said, last));
                            })
                            .map_err(|e| e.to_string())?,
                        )
                    };
                    CAPTION_SINK.with(|s| *s.borrow_mut() = Some((run, Box::new(caption_sink()))));
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
            let me = self.clone();
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
            let r = h.record.as_mut().ok_or("Nothing is recording.")?;
            r.0.split().map_err(|e| e.to_string())?;
            // The file written from now on.
            r.1 = r.0.path().to_path_buf();
            Ok(())
        })
        .await?
    }

    async fn focus(&self, pictures: Vec<Vec<u8>>, w: u32, h: u32) -> Result<Vec<Option<f32>>, String> {
        // Vision takes a moment per picture: off the async threads.
        tokio::task::spawn_blocking(move || {
            pictures.iter().map(|p| ferrocast_studio::focus_x(p, w, h)).collect::<Vec<_>>()
        })
        .await
        .map_err(|e| e.to_string())
    }

    async fn set_stream_cap(&self, cap: Option<StreamCap>) -> Result<(), String> {
        self.on_main(move |h| -> Result<(), String> {
            if h.stream_cap == cap {
                return Ok(());
            }
            h.stream_cap = cap;
            if !h.stream.as_ref().is_some_and(|s| s.stats().active) {
                return Ok(());
            }
            let s = h.settings.clone().unwrap_or_default();
            let target = effective_stream_encode(&s, cap, h.live_pct);
            let cur_encode = h.stream_with.as_ref().map(|w| &w.encode);
            if cur_encode.is_some_and(|prev| shape_differs(prev, &target)) {
                if let Some(st) = h.stream.as_ref() {
                    // Its encoder is swapped on the run (a new header and keyframe, no stop). It
                    // can refuse (the encoder is shared with a recording): that is told, never
                    // swallowed, as the stream then isn't held to the limit.
                    match st.reshape(&target, s.fps) {
                        Ok(()) => {
                            if let Some(w) = h.stream_with.as_mut() {
                                w.encode = target;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "couldn't reshape stream for updated platform cap");
                            return Err(e.to_string());
                        }
                    }
                }
            } else if cur_encode.is_none_or(|prev| prev.video_kbps != target.video_kbps)
                && let Some(st) = h.stream.as_mut()
            {
                st.set_video_kbps(target.video_kbps);
                if let Some(w) = h.stream_with.as_mut() {
                    w.encode.video_kbps = target.video_kbps;
                }
            }
            let no_bframes = cap.is_some_and(|c| c.no_bframes);
            if s.bframes && no_bframes {
                tracing::info!("B-frames turned off for this stream: a WebRTC (WHIP) destination can't carry them");
            }
            if h.lstream.as_ref().is_some_and(|l| l.stats().active)
                && h.lstream_no_bframes != no_bframes
                && let Some(canvas) = h.collection.as_ref().map(|c| c.canvas())
                && let Some(ls) = h.lstream.as_ref()
            {
                let l_encode = light_stream_settings(&s, canvas, no_bframes).encode;
                ls.reshape(&l_encode, s.fps).map_err(|e| e.to_string())?;
                h.lstream_no_bframes = no_bframes;
            }
            Ok(())
        })
        .await?
    }

    async fn set_live_quality(&self, pct: u8) -> Result<(), String> {
        self.on_main(move |h| {
            let set = h.settings.as_ref().map_or(6000, |s| s.video_kbps);
            let capped = h.stream_cap.and_then(|c| c.max_kbps).map_or(set, |max| set.min(max));
            h.live_pct = (pct < 100).then_some(pct);
            let kbps = at_pct(capped, h.live_pct);
            if let Some(st) = h.stream.as_mut() {
                st.set_video_kbps(kbps);
            }
            if let Some(st) = h.vstream.as_mut() {
                st.set_video_kbps(at_pct(set, h.live_pct));
            }
            // The lighter stream (3,000 kbps at most) comes down with it.
            if let Some(l) = h.lstream.as_mut() {
                l.set_video_kbps((set.min(3000) * u32::from(pct.clamp(10, 100)) / 100).max(300));
            }
            tracing::info!(pct, kbps, "auto quality: the stream's bitrate now");
        })
        .await
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
            h.collection.as_ref().ok_or(STOPPED)?;
            let from = PathBuf::from(&file);
            // Any part of the recording under way (a split writes more beside the first).
            if h.record
                .as_ref()
                .is_some_and(|r| r.1 == from || (r.1.parent() == from.parent() && r.1.extension() == from.extension()))
            {
                return Err("Wait until the recording ends to remux files of it.".to_string());
            }
            let to = remux_in_background(vec![from])?;
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
            // Our encoders take most settings from the Studio's own (bitrate, keyframes, profile);
            // x264 also has its preset and tune.
            let _ = h.collection.as_ref().ok_or(STOPPED)?;
            Ok(json!({ "options": ferrocast_studio::kinds::own_encoder_options(&id), "values": settings }))
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
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
            let devices: Vec<Value> =
                studio.monitoring_devices().into_iter().map(|(name, id)| json!({ "name": name, "id": id })).collect();
            let (name, id) =
                h.settings.as_ref().and_then(|s| s.monitoring.clone()).unwrap_or(("Default".into(), "default".into()));
            Ok(json!({ "devices": devices, "current": { "name": name, "id": id } }))
        })
        .await?
    }

    async fn obs_collections(&self) -> Result<Value, String> {
        let Some(dir) = ferrocast_studio::import_preview::obs_dir() else { return Ok(json!([])) };
        let list = tokio::task::spawn_blocking(move || ferrocast_studio::import::list(&dir))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    async fn preview_obs(&self, id: String) -> Result<Value, String> {
        let dir =
            ferrocast_studio::import_preview::obs_dir().ok_or("OBS's scene collections aren't on this computer.")?;
        let preview = tokio::task::spawn_blocking(move || {
            let (inputs, filters) = ferrocast_studio::import_preview::supported_kinds();
            let web = cfg!(target_os = "macos").then_some(ferrocast_studio::WEB_PAGE_KIND);
            ferrocast_studio::import_preview::preview_obs(&dir, &id, web, Some(&inputs), Some(&filters))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        serde_json::to_value(preview).map_err(|e| e.to_string())
    }

    async fn import_obs(&self, id: String) -> Result<Value, String> {
        let dir =
            ferrocast_studio::import_preview::obs_dir().ok_or("OBS's scene collections aren't on this computer.")?;
        // A file of up to 64 MB is read and parsed off the async worker.
        let read_id = id.clone();
        let doc = tauri::async_runtime::spawn_blocking(move || ferrocast_studio::import::read(&dir, &read_id))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        // A new scene collection of the same name (the Studio's own stay as they are).
        let name = doc.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
        self.shelf("collections".into(), ShelfAct::Import { name, doc }).await
    }

    async fn streamlabs_collections(&self) -> Result<Value, String> {
        let Some(dir) = ferrocast_studio::import_preview::streamlabs_dir() else { return Ok(json!([])) };
        let list = tokio::task::spawn_blocking(move || ferrocast_studio::streamlabs::list(&dir))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(list).map_err(|e| e.to_string())
    }

    async fn preview_streamlabs(&self, id: String) -> Result<Value, String> {
        let dir = ferrocast_studio::import_preview::streamlabs_dir()
            .ok_or("Streamlabs' scene collections aren't on this computer.")?;
        let preview = tokio::task::spawn_blocking(move || {
            let (inputs, filters) = ferrocast_studio::import_preview::supported_kinds();
            let web = cfg!(target_os = "macos").then_some(ferrocast_studio::WEB_PAGE_KIND);
            ferrocast_studio::import_preview::preview_streamlabs(&dir, &id, web, Some(&inputs), Some(&filters))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        serde_json::to_value(preview).map_err(|e| e.to_string())
    }

    async fn import_streamlabs(&self, id: String) -> Result<Value, String> {
        let dir = ferrocast_studio::import_preview::streamlabs_dir()
            .ok_or("Streamlabs' scene collections aren't on this computer.")?;
        let read_id = id.clone();
        let doc = tauri::async_runtime::spawn_blocking(move || ferrocast_studio::streamlabs::read(&dir, &read_id))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let name = doc.get("name").and_then(Value::as_str).unwrap_or("Streamlabs scenes").to_string();
        // From here on it's an OBS collection: the same import, web pages and missing kinds.
        self.shelf("collections".into(), ShelfAct::Import { name, doc }).await
    }

    async fn shelf(&self, kind: String, act: ShelfAct) -> Result<Value, String> {
        let _shelf_lock = self.shelf_lock.lock().await;
        let kind = Kind::parse(&kind).ok_or("kind: collections or profiles.")?;
        let data = self.data.clone();
        let missing = |id: &str| format!("There's no {} with the id \"{id}\".", kind.noun());
        match act {
            ShelfAct::List => Ok(json!(self.load_library_off_main().await?.shelf(kind))),
            ShelfAct::Rename { id, name } => {
                let name = library::check_name(&name)?;
                let mut lib = self.load_library_off_main().await?;
                let shelf = lib.shelf_mut(kind);
                if shelf.list.iter().any(|e| e.id != id && e.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
                }
                let entry = shelf.list.iter_mut().find(|e| e.id == id).ok_or_else(|| missing(&id))?;
                entry.name = name;
                self.save_library_off_main(lib).await?;
                Ok(Value::Null)
            }
            ShelfAct::Remove { id } => {
                let mut lib = self.load_library_off_main().await?;
                let shelf = lib.shelf_mut(kind);
                let entry = shelf.get(&id).ok_or_else(|| missing(&id))?.clone();
                if shelf.current == id {
                    return Err(format!("\"{}\" is in use now.", entry.name));
                }
                shelf.list.retain(|e| e.id != id);
                // Kept aside, dated, not deleted. The renames wait out Windows file locks, so they
                // run off the async runtime; the answer still waits for them (it says "kept").
                let file = library::file(&data, kind, &id);
                let _ = tokio::task::spawn_blocking(move || {
                    if file.exists() {
                        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                        let _ = ferrocast_store::durable::rename_patiently(
                            &file,
                            &file.with_extension(format!("json.removed-{stamp}")),
                        );
                    }
                    let _ = ferrocast_store::durable::remove_patiently(&ferrocast_store::durable::backup_path(&file));
                })
                .await;
                self.save_library_off_main(lib).await?;
                Ok(Value::Null)
            }
            ShelfAct::Export { id } => {
                let lib = self.load_library_off_main().await?;
                let entry = lib.shelf(kind).get(&id).ok_or_else(|| missing(&id))?.clone();
                let file = library::file(&data, kind, &id);
                if kind == Kind::Collections && lib.collections.current == id {
                    // The one in use is saved as it is now.
                    let fallback = file.clone();
                    self.on_main(move |h| {
                        if let Some(c) = &h.collection {
                            let target = h.collection_file.as_ref().unwrap_or(&fallback);
                            save_confirmed(h.collection_deferred, c, target);
                        }
                    })
                    .await?;
                }
                let doc = tokio::task::spawn_blocking(move || -> Result<Option<Value>, String> {
                    let doc =
                        ferrocast_store::durable::load_json_with_backup::<Value>(&file, |_| true).map(|l| l.value);
                    if doc.is_none() && (file.exists() || ferrocast_store::durable::backup_path(&file).exists()) {
                        return Err(format!("Couldn't read the {} file cleanly.", kind.noun()));
                    }
                    Ok(doc)
                })
                .await
                .map_err(|e| e.to_string())??;
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
                let mut lib = self.load_library_off_main().await?;
                if lib.shelf(kind).list.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
                }
                let id = lib.shelf(kind).new_id(&name);
                if copy {
                    let from = lib.current_file(&data, kind);
                    if kind == Kind::Collections {
                        let fallback = from.clone();
                        self.on_main(move |h| {
                            if let Some(c) = &h.collection {
                                let target = h.collection_file.as_ref().unwrap_or(&fallback);
                                save_confirmed(h.collection_deferred, c, target);
                            }
                        })
                        .await?;
                    }
                    let to = library::file(&data, kind, &id);
                    let noun = kind.noun();
                    tokio::task::spawn_blocking(move || -> Result<(), String> {
                        if let Some(loaded) = ferrocast_store::durable::load_json_with_backup::<Value>(&from, |_| true)
                        {
                            let bytes = serde_json::to_vec_pretty(&loaded.value).map_err(|e| e.to_string())?;
                            // Off the async runtime (a locked file is waited out), and still
                            // synchronous: the copy is switched to right after, and a failure is
                            // this request's answer.
                            ferrocast_store::durable::write_json_bytes(&to, &bytes)
                                .map_err(|e| format!("The copy couldn't be made: {e}"))?;
                        } else if from.exists() || ferrocast_store::durable::backup_path(&from).exists() {
                            return Err(format!("Couldn't read the {noun} file cleanly."));
                        }
                        Ok(())
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                }
                lib.shelf_mut(kind).list.push(library::Entry { id: id.clone(), name });
                self.save_library_off_main(lib).await?;
                let note = match self.switch_locked(kind, &id).await {
                    Ok(note) => note,
                    Err(e) => {
                        // Not half made: a retry with the same name works.
                        self.forget_off_main(kind, id).await;
                        return Err(e);
                    }
                };
                Ok(shelf_action_result(&id, note))
            }
            ShelfAct::Switch { id } => {
                let note = self.switch_locked(kind, &id).await?;
                Ok(shelf_action_result(&id, note))
            }
            ShelfAct::Import { name, mut doc } => {
                let name = library::check_name(&name)?;
                let mut lib = self.load_library_off_main().await?;
                let previous_id = lib.collections.current.clone();
                let name = lib.collections.free_name(&name);
                let id = lib.collections.new_id(&name);
                let file = library::file(&data, Kind::Collections, &id);
                let import_name = name.clone();
                let report = tokio::task::spawn_blocking(move || {
                    // OBS's Browser sources become Web page sources here; note kinds this OBS lacks.
                    let (known_inputs, _) = ferrocast_studio::import_preview::supported_kinds();
                    let web = cfg!(target_os = "macos").then_some(ferrocast_studio::WEB_PAGE_KIND);
                    let mut report = ferrocast_studio::import::convert(&mut doc, web, Some(&known_inputs));
                    report.name = import_name.clone();
                    if let Some(o) = doc.as_object_mut() {
                        o.insert("name".into(), json!(import_name));
                    }
                    save_json(&file, &doc);
                    report
                })
                .await
                .map_err(|e| e.to_string())?;
                lib.collections.list.push(library::Entry { id: id.clone(), name });
                self.save_library_off_main(lib).await?;
                if let Err(e) = self.switch_locked(Kind::Collections, &id).await {
                    self.forget_off_main(Kind::Collections, id).await;
                    return Err(e);
                }
                let mut v = serde_json::to_value(&report).map_err(|e| e.to_string())?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("id".into(), json!(id));
                    o.insert("previous_id".into(), json!(previous_id));
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
        let tail = tokio::task::spawn_blocking(move || log_tail_across_rotation(&path, lines))
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

    async fn diagnostics_studio(&self) -> Option<Value> {
        let data = self.data.clone();
        self.on_main(move |h| {
            // Read, don't apply: this runs for a support report, which must not change the engine.
            let s = h.settings.clone().unwrap_or_else(|| read_settings(&data));
            let stats = h.collection.as_ref().map(|c| c.stats());
            let encoders: Vec<Value> = ferrocast_studio::kinds::kinds()
                .encoders
                .into_iter()
                .filter_map(|e| {
                    let mut v = serde_json::to_value(&e).ok()?;
                    if let Some(obj) = v.as_object_mut() {
                        let hw = e.id != "obs_x264";
                        obj.insert("hardware".into(), Value::Bool(hw));
                    }
                    Some(v)
                })
                .collect();
            json!({
                "studio_running": h.collection.is_some(),
                "renderer": h.renderer,
                "encoders": encoders,
                "video_encoder": encode_of(&s).video_encoder,
                "sample_rate": s.sample_rate,
                "monitoring": s.monitoring.as_ref().map(|(n, _)| n.clone()),
                "canvas": format!("{}x{}", s.width, s.height),
                "fps": s.fps,
                "stats": stats,
            })
        })
        .await
        .ok()
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

    async fn encode_budget(&self, draft: Option<Value>, enhanced: bool, light: bool) -> Result<Value, String> {
        let data = self.data.clone();
        let telem = tokio::task::spawn_blocking(ferrocast_studio::system::hardware_telemetry)
            .await
            .map_err(|e| format!("hardware telemetry task failed: {e}"))?;
        // The saved profile (loaded if nothing asked for it yet), and a replay buffer that runs
        // (one that failed isn't an encode; CodeRabbit).
        let (saved, replay, cap, selective, vertical_on) = self
            .on_main(move |h| {
                let s = ensure_held_settings(h, &data).clone();
                let selective = h.collection.as_ref().is_some_and(|c| c.has_selective_items());
                let vertical_on = h.collection.as_ref().is_some_and(|c| c.vertical().is_some());
                (s, h.replay.as_ref().is_some_and(|r| r.active()), h.stream_cap, selective, vertical_on)
            })
            .await?;
        let s = match draft {
            Some(d) => serde_json::from_value::<StudioSettings>(d).map_err(|e| format!("settings: {e}"))?,
            None => saved,
        };
        let also = Also {
            enhanced,
            light,
            replay: replay || s.replay_when_streaming,
            selective,
            vertical: vertical_on.then_some(ferrocast_studio::VERTICAL_SIZE),
        };
        Ok(encode_budget_with_host(&s, cap, telem.engines, Some(telem), also))
    }

    async fn host_budget_info(&self) -> Option<HostBudgetInfo> {
        let data = self.data.clone();
        let telem = tokio::task::spawn_blocking(ferrocast_studio::system::hardware_telemetry).await.ok()?;
        let (s, replay_active, record_active, cap, selective, vertical_on) = self
            .on_main(move |h| {
                let s = ensure_held_settings(h, &data).clone();
                let replay_active = h.replay.as_ref().is_some_and(|r| r.active());
                let record_active = h.record.as_ref().is_some_and(|r| r.0.stats().active);
                let selective = h.collection.as_ref().is_some_and(|c| c.has_selective_items());
                let vertical_on = h.collection.as_ref().is_some_and(|c| c.vertical().is_some());
                (s, replay_active, record_active, h.stream_cap, selective, vertical_on)
            })
            .await
            .ok()?;
        let stream = stream_settings(&s, cap);
        let plan_encode = record_encode_capped(&s, cap);
        let shares = baseline_record_shares(&s, cap, selective);
        let mpx_canvas = f64::from(s.width) * f64::from(s.height) * f64::from(s.fps) / 1e6;
        let stream_sw = ferrocast_engine::golive_plan::encoder_is_software(&stream.encode.video_encoder);
        let rec_sw = ferrocast_engine::golive_plan::encoder_is_software(&plan_encode.video_encoder);
        let mut extra_mpx = 0.0;
        let mut extra_sessions = 0u32;
        let record_needed = record_active || s.record_when_streaming;
        let rec_shares_main = record_needed && shares;
        if record_needed && !shares && (!rec_sw || stream_sw) {
            extra_mpx += mpx_canvas;
            extra_sessions += 1;
        }
        if (replay_active || s.replay_when_streaming) && (!rec_sw || stream_sw) {
            extra_mpx += mpx_canvas;
            extra_sessions += 1;
        }
        Some(HostBudgetInfo {
            os: telem.os.clone(),
            encoder_id: stream.encode.video_encoder,
            engines: telem.engines,
            cpu_cores: telem.cpu_cores,
            nvidia_driver_major: telem.nvidia_driver.map(|d| d.0),
            nvidia_driver_minor: telem.nvidia_driver.map(|d| d.1),
            hw_backend: telem.hw_backend.clone(),
            record_extra_mpx_s: extra_mpx,
            record_extra_sessions: extra_sessions,
            record_shares_main: rec_shares_main,
            vertical_on,
            vertical_size: vertical_on.then_some(ferrocast_studio::VERTICAL_SIZE),
            host_sig: telem.host_sig.clone(),
        })
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
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let data = self.data.clone();
            let app = self.app.clone();
            self.on_main(move |h| {
                let id = open_projector(ViewCtx::App(&app), h, &p, &data)?;
                save_projectors(h, &data);
                Ok(json!({ "id": id }))
            })
            .await?
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = p;
            Err("Projectors are drawn on a Mac and on Windows so far.".into())
        }
    }

    async fn close_projectors(&self) -> Result<(), String> {
        // Dropped on the main thread, where their windows live. Closed on purpose: the saved
        // list is empty too (only the Studio stopping or Ferrocast quitting keeps it).
        let data = self.data.clone();
        self.on_main(move |h| {
            h.projectors.clear();
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            save_projectors(h, &data);
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let _ = &data;
        })
        .await
    }

    async fn screens(&self) -> Result<Value, String> {
        self.on_main(|_| serde_json::to_value(ferrocast_studio::screens()).map_err(|e| e.to_string())).await?
    }

    async fn machine_load(&self) -> (Option<f64>, Option<f64>) {
        let cpu = ferrocast_studio::system::machine_cpu_pct();
        (Some(cpu), None)
    }

    async fn restart(&self) -> Result<(), String> {
        let busy = self
            .on_main(|h| {
                any_stream_group_active(h) || h.record.is_some() || h.replay.is_some() || h.virtualcam.is_some()
            })
            .await?;
        if busy {
            return Err("Stop streaming and recording before Ferrocast opens again.".into());
        }
        let app = self.app.clone();
        // A moment for the answer to reach the page first.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            // On macOS reopen through LaunchServices after exit (as the update restart does), so
            // the pinned Dock icon is reused instead of a second, unpinned one appearing.
            #[cfg(target_os = "macos")]
            {
                crate::RELAUNCH_AFTER_EXIT.store(true, std::sync::atomic::Ordering::SeqCst);
                app.exit(0);
            }
            #[cfg(not(target_os = "macos"))]
            app.restart();
        });
        Ok(())
    }

    async fn stream_start(&self, server: String, key: String, tracks: Vec<(u32, u32, u32)>) -> Result<Value, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            h.collection.as_ref().ok_or(STOPPED)?;
            if let Some(s) = h.stream.as_ref().filter(|s| s.stats().active) {
                return Ok(stats_of(s, h.stream_started));
            }
            // A stopped or failed one goes first: one output at a time.
            h.stream = None;
            h.stream_started = None;
            h.live_pct = None;
            // A frame rate chosen during the last stream, still waiting.
            apply_waiting_canvas(&app, h, &data);
            let s = h.settings.clone().unwrap_or_default();
            if s.bframes && h.stream_cap.is_some_and(|c| c.no_bframes) {
                tracing::info!("B-frames turned off for this stream: a WebRTC (WHIP) destination can't carry them");
            }
            let mut settings = stream_settings(&s, h.stream_cap);
            settings.video_tracks = tracks;
            tracing::info!(encoder = %settings.encode.video_encoder, kbps = settings.encode.video_kbps, qualities = settings.video_tracks.len() + 1, "the Studio starts streaming into Go live");
            // Ferrocast's own engine draws what goes out; what it can't do yet is refused with
            // the reason (libobs no longer stands in).
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            // A recording "Same as stream" already running: the stream joins its encoder (one
            // encode for both) when they can share.
            // Against what the recording really runs with (settings changed since apply from its
            // next start), never a paused one (the stream would get nothing; projects-60).
            let recording = h.record.as_ref().map(|r| &r.0).filter(|r| r.stats().active).zip(h.record_with.as_ref());
            let shared = recording.map(|(r, (plan, same))| {
                let can = if r.record_stats().paused {
                    Err("the recording is paused".to_string())
                } else if !c.started(r) {
                    Err("the recording runs in another collection".to_string())
                } else {
                    ferrocast_studio::stage::can_share(&settings, plan, *same, c.canvas().fps, s.fps, c.has_selective_items())
                };
                (r, can)
            });
            let stream = match shared {
                Some((r, Ok(()))) => c.stream_sharing(r, &server, &key, &settings),
                Some((_, Err(why))) => {
                    tracing::info!("the stream has its own encoder: {why}");
                    c.stream_drawn(&server, &key, &settings)
                }
                None => c.stream_drawn(&server, &key, &settings),
            }
            .map_err(|e| e.to_string())?;
            // The canvas still at another rate (the virtual camera kept it): the stream goes out
            // at the chosen one anyway.
            if c.canvas().fps != s.fps
                && let Err(e) = stream.reshape(&settings.encode, s.fps)
            {
                tracing::warn!(error = %e, fps = s.fps, "the stream went out at the canvas's frame rate");
            }
            let now = std::time::Instant::now();
            let v = stats_of(&stream, Some(now));
            h.stream = Some(stream);
            h.stream_started = Some(now);
            h.stream_with = Some(settings);
            Ok(v)
        })
        .await?
    }

    fn delay_drain(&self) -> Option<(f64, f64)> {
        ferrocast_studio::delay_drain()
    }

    async fn discard_delay(&self) {
        ferrocast_studio::discard_delay();
    }

    async fn stream_stop(&self) -> Result<(), String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| {
            h.groups.clear();
            h.vstream = None;
            h.lstream = None;
            h.lstream_no_bframes = false;
            h.stream = None;
            h.stream_started = None;
            h.stream_with = None;
            apply_waiting_canvas(&app, h, &data);
        })
        .await
    }

    fn supports_encode_groups(&self) -> bool {
        true
    }

    async fn start_encode_group(&self, spec: EncodeGroupSpec, hub: std::sync::Arc<Hub>) -> Result<bool, String> {
        let app = self.app.clone();
        let data = self.data.clone();
        self.on_main(move |h| -> Result<bool, String> {
            h.collection.as_ref().ok_or(STOPPED)?;
            if let Some(existing) = h.groups.get(&spec.id)
                && existing.spec == spec
                && existing.output.stats().active
            {
                return Ok(true);
            }
            if !any_stream_group_active(h) {
                apply_waiting_canvas(&app, h, &data);
            }
            h.groups.remove(&spec.id);
            let s = h.settings.clone().unwrap_or_default();
            let settings = stream_settings_for_group(&s, &spec);
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            let output = if spec.key.canvas == EncodeCanvas::Vertical {
                if c.vertical().is_none() {
                    return Ok(false);
                }
                c.vertical_group_to_hub(hub, &settings, Some(spec.key.fps)).map_err(|e| e.to_string())?
            } else if spec.id == GROUP_MAIN {
                let recording =
                    h.record.as_ref().map(|r| &r.0).filter(|r| r.stats().active).zip(h.record_with.as_ref());
                let shared = recording.map(|(r, (plan, same))| {
                    let can = if r.record_stats().paused {
                        Err("the recording is paused".to_string())
                    } else if !c.started(r) {
                        Err("the recording runs in another collection".to_string())
                    } else {
                        ferrocast_studio::stage::can_share(
                            &settings,
                            plan,
                            *same,
                            c.canvas().fps,
                            spec.key.fps,
                            c.has_selective_items(),
                        )
                    };
                    (r, can)
                });
                match shared {
                    Some((r, Ok(()))) => c.stream_sharing_to_hub(r, hub, &settings),
                    Some((_, Err(why))) => {
                        tracing::info!(group = %spec.id, "encode group uses its own encoder: {why}");
                        c.stream_group_to_hub(hub, &settings, Some(spec.key.fps))
                    }
                    None => c.stream_group_to_hub(hub, &settings, Some(spec.key.fps)),
                }
                .map_err(|e| e.to_string())?
            } else {
                c.stream_group_to_hub(hub, &settings, Some(spec.key.fps)).map_err(|e| e.to_string())?
            };
            let now = std::time::Instant::now();
            tracing::info!(
                group = %spec.id,
                width = spec.key.width,
                height = spec.key.height,
                fps = spec.key.fps,
                kbps = spec.key.kbps,
                "started encode group directly into Hub"
            );
            let cap_kbps = main_slot_cap_kbps(&spec, None, Some(s.video_kbps));
            h.groups.insert(
                spec.id.clone(),
                GroupSlot { spec, output, started: now, with: settings, live_pct: None, cap_kbps },
            );
            Ok(true)
        })
        .await?
    }

    async fn stop_encode_group(&self, id: &str) -> Result<(), String> {
        let app = self.app.clone();
        let data = self.data.clone();
        let group_id = id.to_string();
        self.on_main(move |h| {
            h.groups.remove(&group_id);
            if !any_stream_group_active(h) {
                h.stream_started = None;
                h.stream_with = None;
                apply_waiting_canvas(&app, h, &data);
            }
        })
        .await
    }

    async fn abort_encode_group(&self, id: &str) -> Result<(), String> {
        let app = self.app.clone();
        let data = self.data.clone();
        let group_id = id.to_string();
        self.on_main(move |h| {
            if let Some(slot) = h.groups.remove(&group_id) {
                slot.output.abort_without_drain();
            }
            if !any_stream_group_active(h) {
                h.stream_started = None;
                h.stream_with = None;
                apply_waiting_canvas(&app, h, &data);
            }
        })
        .await
    }

    async fn set_group_live_quality(&self, id: &str, pct: u8) -> Result<(), String> {
        let group_id = id.to_string();
        let handled = self
            .on_main(move |h| {
                let Some(slot) = h.groups.get_mut(&group_id) else {
                    return false;
                };
                slot.live_pct = (pct < 100).then_some(pct);
                let kbps = at_pct(slot.spec.key.kbps, slot.live_pct);
                slot.output.set_video_kbps(kbps);
                slot.with.encode.video_kbps = kbps;
                tracing::info!(group = %slot.spec.id, pct, kbps, "auto quality: group bitrate updated");
                true
            })
            .await?;
        if !handled && id == GROUP_MAIN {
            return self.set_live_quality(pct).await;
        }
        Ok(())
    }

    async fn group_stats(&self) -> Result<Option<Value>, String> {
        fn with_encode_metrics(mut val: Value, st: &Streaming) -> Value {
            if let Some(obj) = val.as_object_mut() {
                if let Some(ms) = st.encode_ms() {
                    obj.insert("encode_ms".into(), json!((ms * 10.0).round() / 10.0));
                }
                obj.insert("engine_skipped".into(), json!(st.engine_skipped()));
            }
            val
        }
        self.on_main(|h| {
            let mut map = serde_json::Map::new();
            for (id, slot) in &h.groups {
                map.insert(id.clone(), with_encode_metrics(stats_of(&slot.output, Some(slot.started)), &slot.output));
            }
            if !map.contains_key(GROUP_MAIN)
                && let Some(st) = h.stream.as_ref()
            {
                map.insert(GROUP_MAIN.to_string(), with_encode_metrics(stats_of(st, h.stream_started), st));
            }
            if map.is_empty() { None } else { Some(Value::Object(map)) }
        })
        .await
    }

    async fn live_group_key(&self, id: &str) -> Option<ferrocast_engine::studio_host::EncodeKey> {
        let group_id = id.to_string();
        self.on_main(move |h| h.groups.get(&group_id).map(|g| g.spec.key.clone())).await.ok().flatten()
    }

    async fn light_stream_start(&self, server: String, key: String) -> Result<(), String> {
        self.on_main(move |h| {
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
            if h.lstream.as_ref().is_some_and(|s| s.stats().active) {
                return Ok(());
            }
            h.lstream = None;
            h.lstream_no_bframes = false;
            let s = h.settings.clone().unwrap_or_default();
            let no_bframes = h.stream_cap.is_some_and(|c| c.no_bframes);
            if s.bframes && no_bframes {
                tracing::info!(
                    "B-frames turned off for the lighter stream: a WebRTC (WHIP) destination can't carry them"
                );
            }
            let settings = light_stream_settings(&s, studio.canvas(), no_bframes);
            tracing::info!(kbps = settings.encode.video_kbps, "the Studio's lighter stream starts into Go live");
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            h.lstream = Some(c.stream_drawn(&server, &key, &settings).map_err(|e| e.to_string())?);
            h.lstream_no_bframes = no_bframes;
            Ok(())
        })
        .await?
    }

    async fn light_stream_stop(&self) -> Result<(), String> {
        self.on_main(|h| {
            h.lstream = None;
            h.lstream_no_bframes = false;
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
            if c.vertical().is_none() {
                return Ok(false);
            }
            if h.vstream.as_ref().is_some_and(|s| s.stats().active) {
                return Ok(true);
            }
            h.vstream = None;
            let s = h.settings.clone().unwrap_or_default();
            let settings = stream_settings(&s, None);
            tracing::info!(
                kbps = settings.encode.video_kbps,
                "the Studio's vertical picture starts streaming into Go live"
            );
            h.vstream = Some(c.vertical_stream_drawn(&server, &key, &settings).map_err(|e| e.to_string())?);
            Ok(true)
        })
        .await?
    }

    async fn stream_stats(&self) -> Result<Value, String> {
        self.on_main(|h| main_stream_stats(h).map_or(Value::Null, |(s, started)| stats_of(s, started))).await
    }

    async fn direct_start(&self, id: String, url: String, key: String, simulcast: bool) -> Result<(), String> {
        self.on_main(move |h| {
            let lstream_active = h.lstream.as_ref().is_some_and(|l| l.stats().active);
            let group_light_active = h.groups.get(GROUP_LIGHT).is_some_and(|g| g.output.stats().active);
            if direct_uses_light_layer(simulcast, lstream_active, group_light_active) {
                if let Some(l) = h.lstream.take() {
                    let res = main_stream_mut(h)
                        .ok_or_else(|| "The Studio isn't streaming.".to_string())
                        .and_then(|s| s.add_direct_layered(&id, &url, &key, &l).map_err(|e| e.to_string()));
                    h.lstream = Some(l);
                    return res;
                }
                if let Some(lg) = h.groups.remove(GROUP_LIGHT) {
                    let res = main_stream_mut(h)
                        .ok_or_else(|| "The Studio isn't streaming.".to_string())
                        .and_then(|s| s.add_direct_layered(&id, &url, &key, &lg.output).map_err(|e| e.to_string()));
                    h.groups.insert(GROUP_LIGHT.to_string(), lg);
                    return res;
                }
            }
            let s = main_stream_mut(h).ok_or("The Studio isn't streaming.")?;
            tracing::info!(dest = %id, simulcast, "the Studio sends straight to a server (SRT, RIST or WebRTC)");
            s.add_direct(&id, &url, &key).map_err(|e| e.to_string())
        })
        .await?
    }

    async fn direct_stop(&self, id: String) -> Result<(), String> {
        self.on_main(move |h| {
            if let Some(s) = h.stream.as_mut() {
                s.remove_direct(&id);
            }
            for slot in h.groups.values_mut() {
                slot.output.remove_direct(&id);
            }
        })
        .await
    }

    async fn direct_stats(&self, id: String) -> Result<Value, String> {
        self.on_main(move |h| active_direct_stats(h, &id).map_or(Value::Null, |st| json!(st))).await
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

/// As [`log_tail`], and when the log was just rotated (the file is short) the lines before it come
/// from the rotated `ferrocast.1.log` beside it, so a look right after a rotation isn't empty.
fn log_tail_across_rotation(path: &std::path::Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut tail = log_tail(path, n)?;
    if tail.len() < n
        && let Ok(older) = log_tail(&path.with_file_name("ferrocast.1.log"), n - tail.len())
    {
        tail.splice(0..0, older);
    }
    Ok(tail)
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

fn camera_source_status_json(kind: &str, camera_status: Option<ferrocore::engine::CameraStatus>) -> Option<Value> {
    match kind {
        "av_capture_input"
        | "av_capture_input_v2"
        | "macos-avcapture"
        | "macos-avcapture-fast"
        | "dshow_input"
        | "v4l2_input" => Some(match camera_status {
            Some(st) => json!({
                "camera": {
                    "state": st.state,
                    "message": st.message,
                    "tries": st.tries,
                }
            }),
            None => json!({}),
        }),
        _ => None,
    }
}

fn stage_still_check(
    collection: Option<&ferrocast_studio::stage::Stage>,
    tk: &str,
    kn: &str,
    vj: &Value,
    width: u32,
) -> (ferrocast_studio::stage::MatchingCameraWait, Option<source_still::ReuseJob>) {
    let Some(c) = collection else {
        return (ferrocast_studio::stage::MatchingCameraWait::LiveOrAbsent, None);
    };
    let wait = c.matching_camera_wait(tk, kn, vj);
    if !matches!(wait, ferrocast_studio::stage::MatchingCameraWait::LiveOrAbsent) {
        return (wait, None);
    }
    if ferrocast_studio::matching_camera_holds_session(c, tk, kn, vj) {
        return (ferrocast_studio::stage::MatchingCameraWait::WaitingWithSession, None);
    }
    let job = c.matching_device_picture_job(tk, kn, vj, width).map(|j| Box::new(j) as source_still::ReuseJob);
    (ferrocast_studio::stage::MatchingCameraWait::LiveOrAbsent, job)
}

mod source_still {
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use ferrocast_studio::stage::MatchingCameraWait;
    use ferrocore::CameraStill;
    use serde_json::{Value, json};
    use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

    const KEEP: Duration = Duration::from_secs(15);
    const MAX_KEPT: usize = 128;

    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub(super) enum StillTarget {
        Display(u32, u32),
        Window(u32, u32),
        Camera(String, u32),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct StillAnswer {
        pub image: Option<String>,
        pub waiting: bool,
        pub error: Option<String>,
    }

    impl StillAnswer {
        pub(super) fn image(image: String) -> Self {
            Self { image: Some(image), waiting: false, error: None }
        }
        pub(super) fn waiting() -> Self {
            Self { image: None, waiting: true, error: None }
        }
        pub(super) fn error(error: String) -> Self {
            Self { image: None, waiting: false, error: Some(error) }
        }
        pub(super) fn blank() -> Self {
            Self { image: None, waiting: false, error: None }
        }
        pub(super) fn into_json(self) -> Value {
            let mut obj = serde_json::Map::new();
            obj.insert("image".into(), json!(self.image));
            if self.waiting {
                obj.insert("waiting".into(), json!(true));
            }
            if let Some(err) = self.error {
                obj.insert("error".into(), json!(err));
            }
            Value::Object(obj)
        }
    }

    #[derive(Clone)]
    struct KeptEntry {
        at: Instant,
        epoch: u64,
        image: String,
    }

    #[derive(Default)]
    pub(super) struct StillCache {
        kept: Mutex<Option<HashMap<StillTarget, KeptEntry>>>,
    }

    impl StillCache {
        pub(super) fn evict(&self, target: &StillTarget) {
            if let Ok(mut guard) = self.kept.lock()
                && let Some(map) = guard.as_mut()
            {
                map.remove(target);
            }
        }

        pub(super) fn insert(&self, target: StillTarget, epoch: u64, image: String) {
            let mut guard = self.kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let map = guard.get_or_insert_default();
            map.retain(|_, e| e.at.elapsed() < KEEP);
            if map.len() >= MAX_KEPT {
                map.clear();
            }
            map.insert(target, KeptEntry { at: Instant::now(), epoch, image });
        }

        fn take_candidate(&self, target: &StillTarget, current_epoch: u64) -> Option<String> {
            let mut guard = self.kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let map = guard.get_or_insert_default();
            let entry = map.get(target)?;
            if entry.at.elapsed() >= KEEP {
                map.remove(target);
                return None;
            }
            if matches!(target, StillTarget::Camera(..)) && entry.epoch != current_epoch {
                map.remove(target);
                return None;
            }
            Some(entry.image.clone())
        }

        pub(super) async fn get_if_fresh<ConnectFut>(
            &self,
            target: &StillTarget,
            hotplug_epoch: &impl Fn() -> u64,
            stage_waiting: bool,
            camera_connected: impl FnOnce(String) -> ConnectFut,
        ) -> Option<String>
        where
            ConnectFut: Future<Output = bool>,
        {
            if matches!(target, StillTarget::Camera(..)) && stage_waiting {
                self.evict(target);
                return None;
            }
            let checked_epoch = hotplug_epoch();
            let _candidate = self.take_candidate(target, checked_epoch)?;
            if let StillTarget::Camera(id, _) = target {
                if !camera_connected(id.clone()).await {
                    self.evict(target);
                    return None;
                }
                // Revalidate against current epoch and TTL after the awaited connectivity check so a
                // disconnect+reconnect under the same ID cannot serve an old connection's still.
                let after_epoch = hotplug_epoch();
                if after_epoch != checked_epoch {
                    self.evict(target);
                    return None;
                }
                return self.take_candidate(target, after_epoch);
            }
            Some(_candidate)
        }
    }

    #[derive(Clone)]
    pub(super) enum FlightState {
        Pending,
        Done(u64, StillAnswer),
        Cancelled,
    }

    pub(super) type Flight = Arc<(Notify, Mutex<FlightState>)>;
    pub(super) type FlightMap = HashMap<StillTarget, Flight>;
    pub(super) type ReuseJob = Box<dyn FnOnce() -> Option<(Vec<u8>, u32, u32)> + Send>;

    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    pub(super) fn global_cache() -> &'static StillCache {
        static KEPT: OnceLock<StillCache> = OnceLock::new();
        KEPT.get_or_init(StillCache::default)
    }

    pub(super) fn global_flights() -> &'static Mutex<Option<FlightMap>> {
        static IN_FLIGHT: OnceLock<Mutex<Option<FlightMap>>> = OnceLock::new();
        IN_FLIGHT.get_or_init(|| Mutex::new(None))
    }

    pub(super) fn global_turns() -> &'static Arc<Semaphore> {
        static TURNS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        TURNS.get_or_init(|| Arc::new(Semaphore::new(2)))
    }

    pub(super) fn probe_still(capture_target: StillTarget, flag: &AtomicBool) -> StillAnswer {
        match capture_target {
            StillTarget::Display(id, w) => {
                ferrocore::screen_still_cancel(ferrocore::screen::Target::Display(id), w, flag)
                    .ok()
                    .and_then(|p| {
                        ferrocast_studio::jpeg_data_url(&p.rgba, p.width, p.height, p.premultiplied, "picker").ok()
                    })
                    .map_or_else(StillAnswer::blank, StillAnswer::image)
            }
            StillTarget::Window(id, w) => {
                ferrocore::screen_still_cancel(ferrocore::screen::Target::Window(id), w, flag)
                    .ok()
                    .and_then(|p| {
                        ferrocast_studio::jpeg_data_url(&p.rgba, p.width, p.height, p.premultiplied, "picker").ok()
                    })
                    .map_or_else(StillAnswer::blank, StillAnswer::image)
            }
            StillTarget::Camera(id, w) => match ferrocore::camera_still_status_cancel(&id, w, flag) {
                CameraStill::Picture(p) => {
                    ferrocast_studio::jpeg_data_url(&p.rgba, p.width, p.height, p.premultiplied, "picker")
                        .ok()
                        .map_or_else(StillAnswer::blank, StillAnswer::image)
                }
                CameraStill::Waiting => {
                    if flag.load(Ordering::Relaxed) {
                        StillAnswer::blank()
                    } else {
                        StillAnswer::waiting()
                    }
                }
                CameraStill::Error(e) => {
                    if flag.load(Ordering::Relaxed) {
                        StillAnswer::blank()
                    } else {
                        StillAnswer::error(e)
                    }
                }
            },
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resolve<StageFut, ConnectFut, ProbeFut>(
        cache: &StillCache,
        flights: &Mutex<Option<FlightMap>>,
        turns: &Arc<Semaphore>,
        target: StillTarget,
        stage_check: impl FnMut() -> StageFut,
        hotplug_epoch: impl Fn() -> u64,
        camera_connected: impl Fn(String) -> ConnectFut,
        probe: impl FnMut(StillTarget, Arc<AtomicBool>, OwnedSemaphorePermit) -> ProbeFut,
    ) -> Result<Value, String>
    where
        StageFut: Future<Output = (MatchingCameraWait, Option<ReuseJob>)>,
        ConnectFut: Future<Output = bool>,
        ProbeFut: Future<Output = StillAnswer>,
    {
        resolve_inner(cache, flights, turns, target, false, stage_check, hotplug_epoch, camera_connected, probe, || {})
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resolve_fresh<StageFut, ConnectFut, ProbeFut>(
        cache: &StillCache,
        flights: &Mutex<Option<FlightMap>>,
        turns: &Arc<Semaphore>,
        target: StillTarget,
        fresh: bool,
        stage_check: impl FnMut() -> StageFut,
        hotplug_epoch: impl Fn() -> u64,
        camera_connected: impl Fn(String) -> ConnectFut,
        probe: impl FnMut(StillTarget, Arc<AtomicBool>, OwnedSemaphorePermit) -> ProbeFut,
    ) -> Result<Value, String>
    where
        StageFut: Future<Output = (MatchingCameraWait, Option<ReuseJob>)>,
        ConnectFut: Future<Output = bool>,
        ProbeFut: Future<Output = StillAnswer>,
    {
        resolve_inner(cache, flights, turns, target, fresh, stage_check, hotplug_epoch, camera_connected, probe, || {})
            .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resolve_with_hook<StageFut, ConnectFut, ProbeFut>(
        cache: &StillCache,
        flights: &Mutex<Option<FlightMap>>,
        turns: &Arc<Semaphore>,
        target: StillTarget,
        stage_check: impl FnMut() -> StageFut,
        hotplug_epoch: impl Fn() -> u64,
        camera_connected: impl Fn(String) -> ConnectFut,
        probe: impl FnMut(StillTarget, Arc<AtomicBool>, OwnedSemaphorePermit) -> ProbeFut,
        before_follower_await: impl FnMut(),
    ) -> Result<Value, String>
    where
        StageFut: Future<Output = (MatchingCameraWait, Option<ReuseJob>)>,
        ConnectFut: Future<Output = bool>,
        ProbeFut: Future<Output = StillAnswer>,
    {
        resolve_inner(
            cache,
            flights,
            turns,
            target,
            false,
            stage_check,
            hotplug_epoch,
            camera_connected,
            probe,
            before_follower_await,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_inner<StageFut, ConnectFut, ProbeFut>(
        cache: &StillCache,
        flights: &Mutex<Option<FlightMap>>,
        turns: &Arc<Semaphore>,
        target: StillTarget,
        fresh: bool,
        mut stage_check: impl FnMut() -> StageFut,
        hotplug_epoch: impl Fn() -> u64,
        camera_connected: impl Fn(String) -> ConnectFut,
        mut probe: impl FnMut(StillTarget, Arc<AtomicBool>, OwnedSemaphorePermit) -> ProbeFut,
        mut before_follower_await: impl FnMut(),
    ) -> Result<Value, String>
    where
        StageFut: Future<Output = (MatchingCameraWait, Option<ReuseJob>)>,
        ConnectFut: Future<Output = bool>,
        ProbeFut: Future<Output = StillAnswer>,
    {
        loop {
            let is_camera = matches!(target, StillTarget::Camera(..));
            if !fresh
                && !is_camera
                && let Some(image) = cache.get_if_fresh(&target, &hotplug_epoch, false, |_| async { true }).await
            {
                return Ok(StillAnswer::image(image).into_json());
            }

            let (wait_state, reuse_job) = stage_check().await;
            match wait_state {
                MatchingCameraWait::WaitingWithSession => {
                    // Stage still owns the interrupted/0-frame camera session: report waiting without
                    // opening a competing capture session against the stage's open stream.
                    cache.evict(&target);
                    return Ok(StillAnswer::waiting().into_json());
                }
                MatchingCameraWait::WaitingWithoutSession => {
                    cache.evict(&target);
                }
                MatchingCameraWait::LiveOrAbsent => {
                    if let Some(job) = reuse_job {
                        let start_epoch = hotplug_epoch();
                        let image = tokio::task::spawn_blocking(move || {
                            job().and_then(|(px, pw, ph)| {
                                ferrocast_studio::jpeg_data_url(&px, pw, ph, true, "picker").ok()
                            })
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                        if let Some(img) = image {
                            if !is_camera || hotplug_epoch() == start_epoch {
                                cache.insert(target, start_epoch, img.clone());
                                return Ok(StillAnswer::image(img).into_json());
                            }
                            cache.evict(&target);
                        }
                    } else if !fresh
                        && is_camera
                        && let Some(image) = cache.get_if_fresh(&target, &hotplug_epoch, false, &camera_connected).await
                    {
                        return Ok(StillAnswer::image(image).into_json());
                    }
                }
            }
            let stage_waiting = !matches!(wait_state, MatchingCameraWait::LiveOrAbsent);

            // Single-flight deduplication + early cancellation if caller disconnects.
            let req_epoch = hotplug_epoch();
            let (flight, is_leader) = {
                let mut map_guard = flights.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let map = map_guard.get_or_insert_default();
                if let Some(existing) = map.get(&target) {
                    (existing.clone(), false)
                } else {
                    let created = Arc::new((Notify::new(), Mutex::new(FlightState::Pending)));
                    map.insert(target.clone(), created.clone());
                    (created, true)
                }
            };
            if !is_leader {
                // Pin and enable before reading flight state so no notification is missed between
                // checking the mutex and awaiting `Notify`.
                let notified = flight.0.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let deliver = |flight_epoch: u64, ans: StillAnswer| {
                    if is_camera
                        && ans.image.is_some()
                        && (flight_epoch != req_epoch || hotplug_epoch() != flight_epoch)
                    {
                        StillAnswer::waiting()
                    } else {
                        ans
                    }
                };
                let state_before = flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                match state_before {
                    FlightState::Done(ep, ans) => return Ok(deliver(ep, ans).into_json()),
                    FlightState::Cancelled => continue,
                    FlightState::Pending => {}
                }
                before_follower_await();
                notified.await;
                let state_after = flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                match state_after {
                    FlightState::Done(ep, ans) => return Ok(deliver(ep, ans).into_json()),
                    FlightState::Cancelled => continue,
                    FlightState::Pending => {
                        return Ok(if is_camera { StillAnswer::waiting() } else { StillAnswer::blank() }.into_json());
                    }
                }
            }

            struct FlightCleanup<'a> {
                flights: &'a Mutex<Option<FlightMap>>,
                target: StillTarget,
                flight: Flight,
            }
            impl Drop for FlightCleanup<'_> {
                fn drop(&mut self) {
                    {
                        let mut map = self.flights.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(m) = map.as_mut()
                            && m.get(&self.target).is_some_and(|cur| Arc::ptr_eq(cur, &self.flight))
                        {
                            m.remove(&self.target);
                        }
                    }
                    {
                        let mut state = self.flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if matches!(*state, FlightState::Pending) {
                            *state = FlightState::Cancelled;
                        }
                    }
                    self.flight.0.notify_waiters();
                }
            }
            let _flight_cleanup = FlightCleanup { flights, target: target.clone(), flight: flight.clone() };
            let cancel_flag = Arc::new(AtomicBool::new(false));
            let _cancel_guard = CancelOnDrop(cancel_flag.clone());

            let permit = turns.clone().acquire_owned().await.map_err(|e| e.to_string())?;
            if !fresh
                && !stage_waiting
                && let Some(image) = cache.get_if_fresh(&target, &hotplug_epoch, false, &camera_connected).await
            {
                let ep = hotplug_epoch();
                let ans = StillAnswer::image(image);
                *flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                    FlightState::Done(ep, ans.clone());
                return Ok(ans.into_json());
            }

            let probe_epoch = hotplug_epoch();
            let mut ans = probe(target.clone(), cancel_flag, permit).await;
            if let Some(img) = &ans.image {
                if is_camera && hotplug_epoch() != probe_epoch {
                    cache.evict(&target);
                    ans = StillAnswer::waiting();
                } else if !stage_waiting {
                    cache.insert(target.clone(), probe_epoch, img.clone());
                }
            } else {
                cache.evict(&target);
            }
            *flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                FlightState::Done(probe_epoch, ans.clone());
            return Ok(ans.into_json());
        }
    }
}

type ScreenshotJob = Box<dyn FnOnce() -> Result<(Vec<u8>, (u32, u32)), String> + Send>;

fn write_unique_screenshot(
    folder: &std::path::Path,
    px: &[u8],
    size: (u32, u32),
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|e| format!("The screenshot folder couldn't be made: {e}"))?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    let mut n = 1;
    let path = loop {
        let candidate = if n == 1 {
            folder.join(format!("Screenshot {stamp}.png"))
        } else {
            folder.join(format!("Screenshot {stamp} ({n}).png"))
        };
        match std::fs::File::create_new(&candidate) {
            Ok(_) => break candidate,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(format!("The screenshot couldn't be saved: {e}")),
        }
    };
    if let Err(e) = ferrocast_studio::stage::write_png(&path, px, size) {
        let _ = std::fs::remove_file(&path);
        return Err(e.to_string());
    }
    Ok(path)
}

async fn finish_screenshot_job(folder: std::path::PathBuf, job: ScreenshotJob) -> Result<Value, String> {
    let path = tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf, String> {
        let (px, size) = job()?;
        write_unique_screenshot(&folder, &px, size)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(json!({ "file": path.display().to_string() }))
}

#[cfg(test)]
mod log_tests {
    use super::*;

    /// Balanced recording sits between High (16) and the stream: libobs's quality 22, which is
    /// x264's CRF 22 and the Mac's matching 0-1 quality.
    #[test]
    fn the_balanced_recording_tier_is_a_constant_quality_of_22() {
        let s = StudioSettings { record_quality: "balanced".into(), ..StudioSettings::default() };
        assert_eq!(record_encode(&s).quality, Some(22));
        let high = StudioSettings { record_quality: "high".into(), ..StudioSettings::default() };
        assert_eq!(record_encode(&high).quality, Some(16));
        assert!(StudioSettings { record_quality: "balanced".into(), ..StudioSettings::default() }.check().is_ok());
    }

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

    #[test]
    fn a_log_just_rotated_still_shows_the_lines_before_it() {
        let dir = std::env::temp_dir().join(format!("fc-log-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ferrocast.1.log"), "old 1\nold 2\nold 3\n").unwrap();
        std::fs::write(dir.join("ferrocast.log"), "new 1\nnew 2\n").unwrap();
        let tail = log_tail_across_rotation(&dir.join("ferrocast.log"), 4).unwrap();
        assert_eq!(tail, ["old 2", "old 3", "new 1", "new 2"]);
        // Enough lines in the new file: the old one isn't read.
        assert_eq!(log_tail_across_rotation(&dir.join("ferrocast.log"), 2).unwrap(), ["new 1", "new 2"]);
        // No backup yet: just the log.
        std::fs::remove_file(dir.join("ferrocast.1.log")).unwrap();
        assert_eq!(log_tail_across_rotation(&dir.join("ferrocast.log"), 4).unwrap(), ["new 1", "new 2"]);
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

    #[test]
    fn a_stopped_studio_still_answers_with_a_transition_and_only_valid_matrix_rules() {
        // No file: the dashboard still gets a transition object, never null.
        let v = saved_scenes(None);
        assert_eq!(v["transition"]["kind"], "fade_transition");
        assert_eq!(v["transition"]["ms"], 300);
        // A collection whose matrix names a deleted scene and a missing transition.
        let doc = json!({
            "current_scene": "Main",
            // The scenes as a real collection has them: in `sources` too (a running Studio and a
            // stopped one read the same scenes).
            "sources": [
                { "id": "scene", "name": "Main", "settings": { "items": [] } },
                { "id": "scene", "name": "BRB", "settings": { "items": [] } }
            ],
            "scene_order": [{ "name": "Main" }, { "name": "BRB" }],
            "transitions": [{ "id": "swipe_transition", "name": "Wipe", "settings": {} }],
            "current_transition": "Wipe",
            "transition_duration": 450,
            "ferrocast": {
                "named_transitions_v2": true,
                "transition_matrix": [
                    { "from": null, "to": "BRB", "transition": "Wipe" },
                    { "from": "Gone", "to": "BRB", "transition": "Wipe" },
                    { "from": "Main", "to": "BRB", "transition": "Nope" }
                ]
            }
        });
        let v = saved_scenes(Some(&doc));
        let rules = v["transition_matrix"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "dangling rules are dropped: {rules:?}");
        assert_eq!(v["transition"]["kind"], "swipe_transition");
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

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// The lighter stream counts at the size it really goes out at (CodeRabbit: a 3840×1080
    /// canvas goes out at 2560×720, not 1280×720; one no taller than 720 at its own size).
    #[test]
    fn the_lighter_stream_counts_at_its_real_size() {
        assert_eq!(light_size(3840, 2160), Some((1280, 720)));
        assert_eq!(light_size(3840, 1080), Some((2560, 720)));
        assert_eq!(light_size(1080, 1920), Some((404, 720)));
        assert_eq!(light_size(1280, 720), None);
        assert_eq!(light_size(854, 480), None);
    }

    /// The encoder budget from today's M4 numbers (19 Nov): 4K60 streamed and recorded at High
    /// is over (31 of 60 fps), "Same as stream" shares one encode (at the limit, holding 60),
    /// 4K30 × 2 is at the limit (it held 30), 1440p60 × 2 fits; one that can't share says why.
    #[test]
    fn the_encoder_budget_follows_what_the_mac_did() {
        let at = |w: u32, h: u32, fps: u32, quality: &str| StudioSettings {
            width: w,
            height: h,
            fps,
            record_quality: quality.into(),
            ..StudioSettings::default()
        };
        let level = |s: &StudioSettings| {
            encode_budget(s, None, Some(1), Also::default())["level"].as_str().unwrap_or("").to_string()
        };
        assert_eq!(level(&at(3840, 2160, 60, "high")), "over");
        // One 4K60 encode is all one engine does (1.0x): at the limit, but holding its 60.
        let shared = encode_budget(&at(3840, 2160, 60, "stream"), None, Some(1), Also::default());
        assert_eq!((shared["shared"].as_bool(), shared["level"].as_str()), (Some(true), Some("limit")), "{shared}");
        assert_eq!(level(&at(3840, 2160, 30, "high")), "limit");
        assert_eq!(level(&at(2560, 1440, 60, "high")), "ok");
        // A Max's two engines: two 4K60 encodes fill them, two 4K30 fit.
        assert_eq!(encode_budget(&at(3840, 2160, 60, "high"), None, Some(2), Also::default())["level"], "limit");
        assert_eq!(encode_budget(&at(3840, 2160, 30, "high"), None, Some(2), Also::default())["level"], "ok");
        assert_eq!(encode_budget(&at(3840, 2160, 60, "high"), None, None, Also::default())["level"], "unknown");
        let mut two = at(3840, 2160, 60, "stream");
        two.record_tracks = vec![1, 2];
        let b = encode_budget(&two, None, Some(1), Also::default());
        assert_eq!(b["level"], "over");
        assert!(b["why_not_shared"].as_str().is_some_and(|w| w.contains("2 sound tracks")), "{b}");
        // projects-60's gaps: Twitch's qualities can't share and add encodes; the replay buffer
        // is its own encode; x264 (Lossless) isn't the video encoder's.
        let same = at(3840, 2160, 60, "stream");
        let eb = encode_budget(&same, None, Some(1), Also { enhanced: true, ..Also::default() });
        assert_eq!((eb["shared"].as_bool(), eb["level"].as_str()), (Some(false), Some("over")), "{eb}");
        let replay = encode_budget(&same, None, Some(1), Also { replay: true, ..Also::default() });
        assert_eq!(replay["level"], "over", "{replay}");
        let lossless = encode_budget(&at(3840, 2160, 60, "lossless"), None, Some(1), Also::default());
        assert_eq!(
            (lossless["record_mpx"].as_f64(), lossless["level"].as_str()),
            (Some(0.0), Some("limit")),
            "{lossless}"
        );
    }

    #[test]
    fn cross_os_encode_budget_accounts_for_vertical_nvenc_intel_and_cpu() {
        let at = |width, height, fps, rec: &str| StudioSettings {
            width,
            height,
            fps,
            record_quality: rec.into(),
            ..StudioSettings::default()
        };
        let win_nv = ferrocast_studio::system::HardwareTelemetry {
            os: "windows".into(),
            cpu_cores: 8,
            engines: None,
            hw_backend: Some("nvenc".into()),
            gpu_name: "NVIDIA RTX 4070".into(),
            nvidia_driver: Some((551, 23)),
            host_sig: "windows|nv=551.23".into(),
        };
        let mut s_1080 = at(1920, 1080, 60, "stream");
        s_1080.video_encoder = Some("ffmpeg_nvenc".into());
        // Main 1080p60 (124.4) + Vertical 1080x1920@60 (124.4) = ~249 against 250 Mpx/s -> limit.
        let v_limit = encode_budget_with_host(
            &s_1080,
            None,
            None,
            Some(&win_nv),
            Also { vertical: Some((1080, 1920)), ..Also::default() },
        );
        assert_eq!(v_limit["budget_mpx"], 250.0);
        assert_eq!(v_limit["level"], "limit", "{v_limit}");

        let intel_mac = ferrocast_studio::system::HardwareTelemetry {
            os: "macos".into(),
            cpu_cores: 8,
            engines: None,
            hw_backend: None,
            gpu_name: String::new(),
            nvidia_driver: None,
            host_sig: "macos|intel".into(),
        };
        let s_4k = at(3840, 2160, 30, "stream");
        let over = encode_budget_with_host(&s_4k, None, None, Some(&intel_mac), Also::default());
        assert_eq!((over["budget_mpx"].as_f64(), over["level"].as_str()), (Some(125.0), Some("over")), "{over}");

        let mut s_sw = at(1920, 1080, 60, "stream");
        s_sw.encoder = "software".into();
        s_sw.video_encoder = Some("obs_x264".into());
        let cpu8 = ferrocast_studio::system::HardwareTelemetry {
            os: "linux".into(),
            cpu_cores: 8,
            engines: None,
            hw_backend: None,
            gpu_name: String::new(),
            nvidia_driver: None,
            host_sig: "linux|cores=8".into(),
        };
        let sw_b = encode_budget_with_host(&s_sw, None, None, Some(&cpu8), Also::default());
        assert_eq!((sw_b["budget_mpx"].as_f64(), sw_b["level"].as_str()), (Some(180.0), Some("ok")), "{sw_b}");
    }

    #[test]
    fn stream_settings_for_group_isolates_custom_and_vertical_profiles() {
        let s = StudioSettings {
            width: 2560,
            height: 1440,
            fps: 60,
            video_kbps: 18_000,
            audio_kbps: 160,
            keyframe_s: 4,
            bframes: true,
            twitch_vod_track: 2,
            ..StudioSettings::default()
        };
        let twitch_spec = EncodeGroupSpec {
            id: "g_twitch".into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 6_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: true,
                audio_kbps: 128,
            },
            extra_tracks: Vec::new(),
            vod_track: true,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let t_out = stream_settings_for_group(&s, &twitch_spec);
        assert_eq!(t_out.encode.scale, Some((1920, 1080)));
        assert_eq!(t_out.encode.video_kbps, 6_000);
        assert!(t_out.encode.strict_cbr);
        assert!(!t_out.encode.bframes);
        assert_eq!(t_out.encode.keyframe_s, 2);
        assert_eq!(t_out.encode.audio_kbps, 128);
        assert_eq!(t_out.vod_track, Some(2));

        let vert_spec = EncodeGroupSpec {
            id: "g_vert".into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Vertical,
                width: 720,
                height: 1280,
                fps: 30,
                fps_ntsc: false,
                kbps: 3_500,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let v_out = stream_settings_for_group(&s, &vert_spec);
        assert_eq!(v_out.encode.scale, Some((720, 1280)));
        assert_eq!(v_out.vod_track, None);
    }

    /// A save that changes what only a restart can (the sound rate, broadcast timing) says so; a
    /// save that doesn't, or whose engine already runs that way, says nothing.
    #[test]
    fn a_save_the_running_engine_cannot_take_yet_says_it_applies_after_a_restart() {
        let at =
            |rate, ntsc, fps| StudioSettings { sample_rate: rate, fps_ntsc: ntsc, fps, ..StudioSettings::default() };
        assert_eq!(restart_note((48_000, false), &at(48_000, false, 60)), None);
        assert!(restart_note((48_000, false), &at(44_100, false, 60)).unwrap().starts_with("The sound rate applies"));
        assert!(restart_note((48_000, false), &at(48_000, true, 60)).unwrap().starts_with("Broadcast timing applies"));
        assert!(restart_note((48_000, false), &at(44_100, true, 30)).unwrap().contains("and broadcast timing apply"));
        // Already running that way (it restarted, or the canvas change reopened the engine).
        assert_eq!(restart_note((44_100, true), &at(44_100, true, 60)), None);
        // A rate with no broadcast form (50 fps) is not asked for.
        assert_eq!(restart_note((48_000, false), &at(48_000, true, 50)), None);
    }

    #[test]
    fn truncated_settings_and_session_load_their_bak() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let settings_path = settings_file(dir.path());
        let first = StudioSettings { width: 1280, height: 720, fps: 30, ..StudioSettings::default() };
        let second = StudioSettings { width: 2560, height: 1440, fps: 60, ..StudioSettings::default() };

        save_json(&settings_path, &serde_json::to_value(&first).unwrap_or_else(|e| panic!("{e}")));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        save_json(&settings_path, &serde_json::to_value(&second).unwrap_or_else(|e| panic!("{e}")));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        assert!(ferrocast_store::durable::backup_path(&settings_path).exists());

        let bytes = std::fs::read(&settings_path).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&settings_path, &bytes[..bytes.len() / 2]).unwrap_or_else(|e| panic!("{e}"));

        let loaded = load_settings(dir.path());
        assert_eq!((loaded.width, loaded.height, loaded.fps), (1280, 720, 30));
        let read = read_settings(dir.path());
        assert_eq!((read.width, read.height, read.fps), (1280, 720, 30), "reads the same profile");

        // Corrupt existing settings must not clobber `.bak` on a save_json overwrite.
        save_json(&settings_path, &serde_json::to_value(&second).unwrap_or_else(|e| panic!("{e}")));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        assert_eq!((load_settings(dir.path()).width, load_settings(dir.path()).fps), (2560, 60));
    }

    /// An undo or redo storm saves after every step: through the queue the file ends as the last
    /// state, whole, and no step waits on a disk write (GAPS #22).
    #[test]
    fn a_storm_of_steps_leaves_the_last_state_on_disk() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let file = dir.path().join("scenes.json");
        let Ok(c) = Collection::new(Canvas { width: 640, height: 360, fps: 30 }, FIRST_SCENE) else { return };
        let started = std::time::Instant::now();
        for i in 0..100 {
            let _ = c.add_scene(&format!("Scene {i}"));
            save_confirmed(false, &c, &file);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the steps didn't wait for the disk");
        assert!(ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(20)));
        let on_disk: Value = serde_json::from_slice(&std::fs::read(&file).unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(on_disk, c.save(COLLECTION), "the last state is what is on disk");
    }

    #[test]
    fn deferred_safe_mode_collection_never_overwrites_saved_scenes() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let scenes_path = scenes_file(dir.path());
        let saved_doc = json!({
            "current_scene": "Main Show",
            "scene_order": [{ "name": "Main Show" }]
        });
        save_json(&scenes_path, &saved_doc);
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));

        if let Ok(temp_blank) = Collection::new(Canvas { width: 640, height: 360, fps: 30 }, FIRST_SCENE) {
            // While `collection_deferred` is true (Safe Mode before user confirm), save_confirmed is a no-op.
            save_confirmed(true, &temp_blank, &scenes_path);
            let still_on_disk: Value =
                serde_json::from_slice(&std::fs::read(&scenes_path).unwrap_or_else(|e| panic!("{e}")))
                    .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(still_on_disk["current_scene"], "Main Show");
        }
    }

    /// The quit's drain on Windows must leave room inside the ~5 s the system grants a session end
    /// (shutdown, logoff, console close) for the engine's own shutdown and the markers.
    #[test]
    fn a_windows_quit_drain_fits_the_five_second_session_end_grace() {
        let grace = std::time::Duration::from_secs(5);
        assert!(quit_drain_for(true) + std::time::Duration::from_millis(1500) <= grace);
        assert_eq!(quit_drain_for(false), QUIT_DRAIN_TIMEOUT);
    }

    #[test]
    fn drain_outputs_bounded_runs_off_main_in_parallel_and_bounds_stalls() {
        use std::sync::{Arc, Barrier, Condvar, Mutex};

        let caller_tid = std::thread::current().id();
        let barrier = Arc::new(Barrier::new(2));
        let worker_tids = Arc::new(Mutex::new(Vec::new()));

        let jobs: Vec<Box<dyn FnOnce() + Send>> = (0..2)
            .map(|_| {
                let b = barrier.clone();
                let tids = worker_tids.clone();
                Box::new(move || {
                    tids.lock().unwrap_or_else(|e| e.into_inner()).push(std::thread::current().id());
                    // Barrier proves both drains execute concurrently off the caller thread.
                    b.wait();
                }) as Box<dyn FnOnce() + Send>
            })
            .collect();

        assert!(drain_outputs_bounded(jobs, std::time::Duration::from_secs(2)));
        let tids = worker_tids.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(tids.len(), 2);
        assert!(tids.iter().all(|&tid| tid != caller_tid));
        assert_ne!(tids[0], tids[1]);

        // Stalled drain returns false at the bound instead of wedging the main thread.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_worker = gate.clone();
        let stalled: Vec<Box<dyn FnOnce() + Send>> = vec![Box::new(move || {
            let g = gate_worker.0.lock().unwrap_or_else(|e| e.into_inner());
            drop(gate_worker.1.wait_while(g, |done| !*done));
        })];
        let started = std::time::Instant::now();
        assert!(!drain_outputs_bounded(stalled, std::time::Duration::from_millis(40)));
        assert!(started.elapsed() < std::time::Duration::from_millis(400));
        *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        gate.1.notify_all();
    }

    #[test]
    fn take_outputs_keeps_collection_alive_until_drain_and_close_clears_held() {
        if let Ok(collection) = Collection::new(Canvas { width: 640, height: 360, fps: 30 }, FIRST_SCENE) {
            HELD.with(|h| {
                let mut h = h.borrow_mut();
                h.collection = Some(collection);
            });
            HELD.with(|h| {
                let mut h = h.borrow_mut();
                let jobs = take_outputs_from_held(&mut h);
                assert!(jobs.is_empty());
                assert!(h.collection.is_some(), "collection stays intact until outputs finish");
            });
            assert!(close());
            assert!(HELD.with(|h| h.borrow().collection.is_none()));
        }
    }

    /// "Same as stream" is the stream's own encode, held to the platform's limit: so the stream and
    /// the recording can share one encoder in either order, and a stream joining a recording's
    /// encoder is as strict as it asks (it used to join a plain CBR one, past Twitch's limit, with
    /// nothing saying so).
    #[test]
    fn a_same_as_stream_recording_follows_the_platform_cap_so_the_two_share_strictly() {
        let s = StudioSettings {
            video_kbps: 14_000,
            vbr: false,
            keyframe_s: 2,
            record_quality: "stream".into(),
            ..StudioSettings::default()
        };
        let twitch = StreamCap { max_kbps: Some(6_000), h264_only: true, keyframe_secs: 2, no_bframes: false };
        let stream = stream_settings(&s, Some(twitch));
        let capped = record_encode_capped(&s, Some(twitch));
        assert!(capped.strict_cbr && capped.scale.is_none());
        let plan = ferrocast_studio::RecordPlan { tracks: vec![1], encode: capped, ..Default::default() };
        assert_eq!(ferrocast_studio::stage::can_share(&stream, &plan, true, 60, 60, false), Ok(()));
        // Without a cap, nothing changes: the plain stream and the plain recording share.
        let plain = ferrocast_studio::RecordPlan {
            tracks: vec![1],
            encode: record_encode_capped(&s, None),
            ..Default::default()
        };
        assert_eq!(ferrocast_studio::stage::can_share(&stream_settings(&s, None), &plain, true, 60, 60, false), Ok(()));
        // A recording that began plain can't be joined by a strict stream: it gets its own encoder.
        assert!(ferrocast_studio::stage::can_share(&stream, &plain, true, 60, 60, false).is_err());
        // Sources shown only on the stream or only in the recording make two pictures: no sharing.
        let why = ferrocast_studio::stage::can_share(&stream_settings(&s, None), &plain, true, 60, 60, true);
        assert!(why.unwrap_err().contains("only on the stream or only in the recording"));
        // The other qualities are as chosen, cap or not.
        let high = StudioSettings { record_quality: "high".into(), ..s.clone() };
        assert_eq!(record_encode_capped(&high, Some(twitch)), record_encode(&high));
    }

    /// The encoder budget says why a recording can't share: for sources shown on one side only,
    /// that and nothing about a Twitch VOD track (turning it off wouldn't bring sharing back).
    #[test]
    fn the_reason_for_not_sharing_is_the_real_one() {
        let s = StudioSettings { record_quality: "stream".into(), twitch_vod_track: 2, ..StudioSettings::default() };
        let why = |selective| {
            let b = encode_budget(&s, None, Some(1), Also { selective, ..Also::default() });
            b["why_not_shared"].as_str().map(String::from)
        };
        let selective = why(true).unwrap_or_else(|| panic!("not shared"));
        assert!(selective.contains("only on the stream or only in the recording"), "{selective}");
        assert!(!selective.contains("VOD"), "no unrelated advice: {selective}");
        // Without them the VOD track is the reason, and the hint to turn it off is right.
        let vod = why(false).unwrap_or_else(|| panic!("not shared"));
        assert!(vod.contains("VOD") && vod.contains("turn the VOD track off"), "{vod}");
    }

    /// While a recording shares the stream's encode, choosing where an item shows is refused with
    /// a plain reason; anything else, and any edit when they don't share, goes ahead.
    #[test]
    fn a_flag_is_refused_while_the_recording_shares_the_streams_encoder() {
        let edit = |edit| StudioAct::EditItem { scene: "Main".into(), id: 3, edit };
        for flag in [Edit::StreamOnly, Edit::RecordingOnly] {
            let why = refuses_while_shared(&edit(flag), true).unwrap_or_else(|| panic!("{flag:?} is refused"));
            assert!(why.contains("Stop the recording"), "says what to do: {why}");
            assert!(refuses_while_shared(&edit(flag), false).is_none(), "{flag:?} alone is fine");
        }
        // Back to both is always fine: it can't make the pictures differ.
        assert!(refuses_while_shared(&edit(Edit::StreamAndRecording), true).is_none());
        assert!(refuses_while_shared(&edit(Edit::Hide), true).is_none());
        assert!(refuses_while_shared(&StudioAct::Scenes, true).is_none());
    }

    #[test]
    fn twitch_cap_clamps_encode_forces_h264_2s_strict_cbr_and_clamps_auto_quality() {
        let mut s = StudioSettings {
            video_kbps: 14_000,
            vbr: true,
            keyframe_s: 4,
            video_encoder: Some("com.apple.videotoolbox.videoencoder.ave.hevc".into()),
            ..StudioSettings::default()
        };
        let twitch = StreamCap { max_kbps: Some(6_000), h264_only: true, keyframe_secs: 2, no_bframes: false };
        let capped = effective_stream_encode(&s, Some(twitch), None);
        assert_eq!(capped.video_kbps, 6_000);
        assert!(!capped.vbr, "CBR forced when capped");
        assert!(capped.strict_cbr, "strict 1x VBV / DataRateLimits enabled");
        assert_eq!(capped.keyframe_s, 2);
        assert!(!capped.video_encoder.to_ascii_lowercase().contains("hevc"), "{}", capped.video_encoder);

        // Auto quality scales the capped ceiling (6,000), never 14,000, and recovery returns to 6,000.
        assert_eq!(effective_stream_encode(&s, Some(twitch), Some(75)).video_kbps, 4_500);
        assert_eq!(effective_stream_encode(&s, Some(twitch), Some(100)).video_kbps, 6_000);

        // A user setting already below the cap scales from its own bitrate.
        s.video_kbps = 4_500;
        assert_eq!(effective_stream_encode(&s, Some(twitch), Some(50)).video_kbps, 2_250);
        assert_eq!(effective_stream_encode(&s, None, None).video_kbps, 4_500);

        // WebRTC (WHIP) turns off B-frames on both main and light streams without forcing CBR.
        s.bframes = true;
        s.vbr = true;
        let whip = StreamCap { max_kbps: None, h264_only: false, keyframe_secs: 0, no_bframes: true };
        let whip_main = effective_stream_encode(&s, Some(whip), None);
        assert!(!whip_main.bframes && whip_main.vbr && !whip_main.strict_cbr);
        let whip_light = light_stream_settings(&s, Canvas { width: 1920, height: 1080, fps: 60 }, true);
        assert!(!whip_light.encode.bframes);
    }

    #[test]
    fn desktop_budget_mpx_and_group_hevc_and_sw_classifiers() {
        // F8.5: Windows/Linux HW encoders (QSV/AMF/NVENC) default to 250 Mpx/s.
        assert_eq!(default_budget_mpx("windows", None, Some("qsv"), 8, "obs_qsv11"), Some(250.0));
        assert_eq!(default_budget_mpx("linux", None, Some("vaapi"), 8, "h264_vaapi"), Some(250.0));
        // N8: SVT-AV1 is classified as software (uses CPU core tier: <8 cores -> 125 Mpx/s).
        assert_eq!(default_budget_mpx("linux", None, Some("vaapi"), 4, "libsvtav1"), Some(125.0));

        // F9.3: EncodeCodec::Hevc rewrites an H.264 encoder id into its HEVC counterpart.
        let s = StudioSettings {
            video_encoder: Some("com.apple.videotoolbox.videoencoder.ave.avc".into()),
            ..StudioSettings::default()
        };
        let hevc_spec = EncodeGroupSpec {
            id: "g_hevc".into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 8_000,
                codec: EncodeCodec::Hevc,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let gs = stream_settings_for_group(&s, &hevc_spec);
        // The VideoToolbox id only stays itself on a Mac (elsewhere it becomes the generic
        // hardware encoder, which has no HEVC id), so the mapping is checked there.
        assert!(
            !cfg!(target_os = "macos") || gs.encode.video_encoder.to_ascii_lowercase().contains("hevc"),
            "HEVC encode group must map H.264 encoder to HEVC: {}",
            gs.encode.video_encoder
        );

        // N5: Baseline share ignores twitch_vod_track (GROUP_MAIN bills VOD separately in planner).
        let vod_s = StudioSettings {
            record_quality: "stream".into(),
            record_tracks: vec![1],
            twitch_vod_track: 2,
            ..StudioSettings::default()
        };
        assert!(baseline_record_shares(&vod_s, None, false));
        assert!(!baseline_record_shares(&vod_s, None, true));

        // N8: GROUP_LIGHT must not attach to WHIP when simulcast == false.
        assert!(!direct_uses_light_layer(false, false, true));
        assert!(direct_uses_light_layer(true, false, true));

        // F10.1: `record_encode_for_stream` matches `GROUP_MAIN` encode shape when active, else falls back cleanly.
        assert_eq!(record_encode_for_stream(&vod_s, None, None), record_encode_capped(&vod_s, None));
    }
    #[test]
    fn record_encode_skips_scaled_or_vod_main_group() {
        let mut s = StudioSettings {
            width: 2560,
            height: 1440,
            fps: 60,
            video_kbps: 15_000,
            record_quality: "stream".into(),
            ..StudioSettings::default()
        };
        let expected_fallback = record_encode_capped(&s, None);
        let scaled_spec = EncodeGroupSpec {
            id: GROUP_MAIN.into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 6_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        assert_eq!(
            record_encode_for_group_spec(&s, None, Some(&scaled_spec)),
            expected_fallback,
            "scaled GROUP_MAIN must not override unscaled recording with capped stream shape"
        );
        s.width = 1920;
        s.height = 1080;
        let vod_spec = EncodeGroupSpec { vod_track: true, ..scaled_spec };
        assert_eq!(
            record_encode_for_group_spec(&s, None, Some(&vod_spec)),
            record_encode_capped(&s, None),
            "vod_track GROUP_MAIN cannot share with plain recording"
        );
    }

    #[test]
    fn apply_main_slot_settings_updates_group_spec_key() {
        let was = StudioSettings { width: 1920, height: 1080, fps: 60, video_kbps: 6_000, ..StudioSettings::default() };
        let next = StudioSettings { width: 1280, height: 720, fps: 30, video_kbps: 4_500, ..was.clone() };
        let mut spec = EncodeGroupSpec {
            id: GROUP_MAIN.into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 6_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: false,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let reshaped = next_main_spec_for_settings(&spec, &next, None, Some(&was));
        assert_eq!((reshaped.key.width, reshaped.key.height, reshaped.key.fps), (1280, 720, 30));
        let applied_kbps = apply_main_slot_bitrate_on_spec(&mut spec, None, None, 5_000);
        assert_eq!(applied_kbps, 5_000);
        assert_eq!(spec.key.kbps, 5_000);
    }

    #[test]
    fn main_slot_bitrate_preserves_pass2_cap_and_auto_quality_base() {
        let was =
            StudioSettings { width: 1920, height: 1080, fps: 60, video_kbps: 10_000, ..StudioSettings::default() };
        let next = StudioSettings { video_kbps: 9_000, ..was.clone() };
        let mut spec = EncodeGroupSpec {
            id: GROUP_MAIN.into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 6_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let cap = Some(6_000);
        let live_kbps = apply_main_slot_bitrate_on_spec(&mut spec, cap, Some(75), next.video_kbps);
        assert_eq!(spec.key.kbps, 6_000, "Pass-2 cap and 100% base bitrate must stay intact on spec.key.kbps");
        assert_eq!(live_kbps, at_pct(6_000, Some(75)), "output gets 75% of capped 6,000 kbps");
        assert_eq!(at_pct(spec.key.kbps, None), 6_000, "recovery to 100% restores 6,000 kbps");

        // Subsequent step down to 4,000 and back up to 9,000 keeps remembering the 6,000 cap.
        let low_live = apply_main_slot_bitrate_on_spec(&mut spec, cap, Some(75), 4_000);
        assert_eq!(spec.key.kbps, 4_000);
        assert_eq!(low_live, at_pct(4_000, Some(75)));
        let raised_live = apply_main_slot_bitrate_on_spec(&mut spec, cap, Some(75), 9_000);
        assert_eq!(spec.key.kbps, 6_000);
        assert_eq!(raised_live, at_pct(6_000, Some(75)));

        let next_reshape = next_main_spec_for_settings(&spec, &next, cap, Some(&was));
        assert_eq!(next_reshape.key.kbps, 6_000, "reshape spec must keep 100% capped base bitrate");
    }

    #[test]
    fn f56_main_group_started_at_equal_video_kbps_keeps_cap_kbps_on_raise() {
        let was = StudioSettings { width: 1920, height: 1080, fps: 60, video_kbps: 6_000, ..StudioSettings::default() };
        let raised = StudioSettings { width: 1280, height: 720, fps: 60, video_kbps: 12_000, ..was.clone() };
        let mut spec = EncodeGroupSpec {
            id: GROUP_MAIN.into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 1920,
                height: 1080,
                fps: 60,
                fps_ntsc: false,
                kbps: 6_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: true,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: Some(6_000),
        };
        let cap = main_slot_cap_kbps(&spec, None, Some(was.video_kbps));
        assert_eq!(cap, Some(6_000), "explicit cap_kbps on spec must persist even when start bitrate equals cap");
        let applied = apply_main_slot_bitrate_on_spec(&mut spec, cap, None, raised.video_kbps);
        assert_eq!(applied, 6_000);
        assert_eq!(spec.key.kbps, 6_000, "raising Settings.video_kbps must remain clamped to spec.cap_kbps");
        let reshaped = next_main_spec_for_settings(&spec, &raised, cap, Some(&was));
        assert_eq!(reshaped.key.kbps, 6_000, "reshaping after equal-bitrate start must keep cap_kbps");
        assert_eq!(reshaped.cap_kbps, Some(6_000));
    }

    #[test]
    fn synthesized_main_group_without_cap_kbps_allows_bitrate_raise() {
        let was =
            StudioSettings { width: 2560, height: 1440, fps: 60, video_kbps: 18_000, ..StudioSettings::default() };
        let updated = StudioSettings { video_kbps: 15_000, ..was.clone() };
        let raised = StudioSettings { video_kbps: 22_000, ..was.clone() };
        let mut spec = EncodeGroupSpec {
            id: GROUP_MAIN.into(),
            key: ferrocast_engine::studio_host::EncodeKey {
                canvas: EncodeCanvas::Main,
                width: 2560,
                height: 1440,
                fps: 60,
                fps_ntsc: false,
                kbps: 18_000,
                codec: EncodeCodec::H264,
                keyframe_s: 2,
                strict_cbr: false,
                no_bframes: false,
                audio_kbps: 160,
            },
            extra_tracks: Vec::new(),
            vod_track: false,
            is_light: false,
            is_twitch_eb: false,
            cap_kbps: None,
        };
        let cap = main_slot_cap_kbps(&spec, None, Some(was.video_kbps));
        assert_eq!(cap, None);
        assert_eq!(apply_main_slot_bitrate_on_spec(&mut spec, cap, None, updated.video_kbps), 15_000);
        assert_eq!(next_main_spec_for_settings(&spec, &raised, cap, Some(&updated)).key.kbps, 22_000);
    }
}

#[cfg(test)]
mod projector_tests {
    use super::*;

    #[test]
    fn only_full_screen_projectors_on_the_same_display_clash() {
        assert!(same_screen(Some(1), Some(1)));
        assert!(!same_screen(Some(1), Some(2)));
        // A window (no display) never clashes with another window.
        assert!(!same_screen(None, None));
        assert!(!same_screen(Some(0), None));
    }

    #[test]
    fn the_saved_list_is_the_requests_as_they_were_and_old_ones_still_read() {
        let list = vec![
            ProjectorReq { what: "multiview".into(), name: None, screen: Some(1), on_top: false },
            ProjectorReq { what: "source".into(), name: Some("Camera".into()), screen: None, on_top: true },
        ];
        let v = serde_json::to_value(&list).unwrap();
        let back: Vec<ProjectorReq> = serde_json::from_value(v).unwrap();
        assert_eq!(back, list);
        // The settings file of an older version has none of the new fields: today's behaviour.
        let old: StudioSettings = serde_json::from_str(r#"{ "video_kbps": 4500 }"#).unwrap();
        assert!(
            !old.projector_on_top
                && !old.projector_one_per_screen
                && !old.projector_restore
                && !old.projector_hide_cursor
        );
    }
}

#[cfg(test)]
mod import_tests {
    fn write_default_fixtures(obs_dir: &std::path::Path, sl_dir: &std::path::Path) {
        let obs_main = serde_json::json!({
            "name": "Main OBS Fixture",
            "current_scene": "Main Stage",
            "scene_order": [{"name": "Main Stage"}, {"name": "BRB"}],
            "sources": [
                {
                    "id": "macos-avcapture",
                    "name": "DeskCam",
                    "settings": {"device": "cam0"},
                    "filters": []
                },
                {
                    "id": "browser_source",
                    "name": "AlertBox",
                    "settings": {"url": "http://127.0.0.1:4040/alerts"},
                    "filters": []
                },
                {
                    "id": "spout_capture",
                    "name": "SpoutFeed",
                    "settings": {},
                    "filters": []
                },
                {
                    "id": "coreaudio_input_capture",
                    "name": "Mic",
                    "settings": {"device_id": "default"},
                    "filters": [{"id": "vst_filter", "name": "GateVST", "settings": {}}]
                },
                {
                    "id": "scene",
                    "name": "Main Stage",
                    "settings": {
                        "items": [
                            {"name": "DeskCam", "pos": {"x": 0.0, "y": 0.0}},
                            {"name": "AlertBox", "pos": {"x": 100.0, "y": 100.0}}
                        ]
                    }
                },
                {
                    "id": "scene",
                    "name": "BRB",
                    "settings": {"items": []}
                }
            ]
        });
        if let Err(e) = std::fs::write(obs_dir.join("main_setup.json"), obs_main.to_string()) {
            panic!("write obs fixture: {e}");
        }
        if let Err(e) = std::fs::write(obs_dir.join("broken_backup.json"), "{broken json!!") {
            panic!("write broken fixture: {e}");
        }
        let manifest = serde_json::json!({
            "collections": [{"id": "sl_neon", "name": "Streamlabs Neon"}]
        });
        if let Err(e) = std::fs::write(sl_dir.join("manifest.json"), manifest.to_string()) {
            panic!("write sl manifest: {e}");
        }
        let sl_doc = serde_json::json!({
            "nodeType": "RootNode",
            "sources": {
                "items": [
                    {
                        "id": "src-cam",
                        "name": "WinCam",
                        "type": "dshow_input",
                        "settings": {"video_device_id": "win-cam-1"},
                        "filters": {"items": []}
                    },
                    {
                        "id": "src-alerts",
                        "name": "SL Alerts",
                        "type": "browser_source",
                        "settings": {"url": "https://streamlabs.com/alert-box/v3/demo"},
                        "filters": {"items": []}
                    },
                    {
                        "id": "src-wheel",
                        "name": "SpinWheel",
                        "type": "streamlabs_spin_wheel",
                        "settings": {},
                        "filters": {"items": []}
                    }
                ]
            },
            "scenes": {
                "items": [
                    {
                        "id": "sc-main",
                        "name": "Live Scene",
                        "sceneItems": {
                            "items": [
                                {"sourceId": "src-cam", "x": 0.0, "y": 0.0, "scaleX": 1.0, "scaleY": 1.0},
                                {"sourceId": "src-alerts", "x": 40.0, "y": 40.0, "scaleX": 1.0, "scaleY": 1.0}
                            ]
                        }
                    }
                ]
            }
        });
        if let Err(e) = std::fs::write(sl_dir.join("sl_neon.json"), sl_doc.to_string()) {
            panic!("write sl fixture: {e}");
        }
    }

    #[tokio::test]
    async fn real_importer_preview_import_and_undo_with_ferrocast_home() {
        let scratch = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        // Only with a variable made for it: the test writes a collection and removes it again, so it
        // never runs on the real home just because FERROCAST_HOME happens to be set.
        let from_env = std::env::var_os("FERROCAST_IMPORT_TEST_USE_HOME").filter(|v| !v.is_empty()).and_then(|_| {
            let obs = ferrocast_studio::import_preview::obs_dir()?;
            let sl = ferrocast_studio::import_preview::streamlabs_dir()?;
            Some((crate::data_dir(), obs, sl))
        });
        let (home, obs_dir, sl_dir) = match from_env {
            Some(paths) => paths,
            None => {
                let home = scratch.path().join("home");
                let obs = scratch.path().join("obs");
                let sl = scratch.path().join("sl");
                for dir in [&home, &obs, &sl] {
                    if let Err(e) = std::fs::create_dir_all(dir) {
                        panic!("mkdir: {e}");
                    }
                }
                write_default_fixtures(&obs, &sl);
                (home, obs, sl)
            }
        };

        let engine = ferrocast_engine::Engine::start_with_mode(&home, false)
            .await
            .unwrap_or_else(|e| panic!("engine start: {e}"));
        let (inputs, filters) = ferrocast_studio::import_preview::supported_kinds();
        let web = cfg!(target_os = "macos").then_some(ferrocast_studio::WEB_PAGE_KIND);

        let obs_list = ferrocast_studio::import::list(&obs_dir);
        assert!(obs_list.iter().any(|c| c.id == "main_setup"), "{obs_list:?}");

        let obs_preview =
            ferrocast_studio::import_preview::preview_obs(&obs_dir, "main_setup", web, Some(&inputs), Some(&filters))
                .unwrap_or_else(|e| panic!("obs preview: {e}"));
        assert_eq!(obs_preview.name, "Main OBS Fixture");
        // Browser sources only become Web page sources where the Studio has them (a Mac).
        assert_eq!(obs_preview.web_pages >= 1, web.is_some(), "{obs_preview:?}");
        assert!(obs_preview.skipped.iter().any(|s| s.kind == "spout_capture"), "{obs_preview:?}");
        assert!(obs_preview.skipped.iter().any(|s| s.kind == "vst_filter"), "{obs_preview:?}");

        let broken_err = ferrocast_studio::import_preview::preview_obs(
            &obs_dir,
            "broken_backup",
            web,
            Some(&inputs),
            Some(&filters),
        )
        .err()
        .unwrap_or_else(|| panic!("expected broken json error"));
        assert!(broken_err.to_string().contains("JSON is damaged"), "{broken_err}");

        let mut obs_doc =
            ferrocast_studio::import::read(&obs_dir, "main_setup").unwrap_or_else(|e| panic!("obs read: {e}"));
        let obs_report = ferrocast_studio::import::convert(&mut obs_doc, web, Some(&inputs));

        let mut lib = crate::studio_library::load(&home);
        let previous_id = lib.shelf(crate::studio_library::Kind::Collections).current.clone();
        let imported_name = {
            let shelf = lib.shelf(crate::studio_library::Kind::Collections);
            let checked =
                crate::studio_library::check_name("Main OBS Fixture").unwrap_or_else(|e| panic!("check_name: {e}"));
            shelf.free_name(&checked)
        };
        let imported_id = lib.shelf(crate::studio_library::Kind::Collections).new_id(&imported_name);
        let imported_path = crate::studio_library::file(&home, crate::studio_library::Kind::Collections, &imported_id);
        if let Some(parent) = imported_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            panic!("mkdir collection parent: {e}");
        }
        let body = serde_json::to_string_pretty(&obs_doc).unwrap_or_else(|e| panic!("json: {e}"));
        if let Err(e) = std::fs::write(&imported_path, body) {
            panic!("write imported collection: {e}");
        }
        {
            let shelf = lib.shelf_mut(crate::studio_library::Kind::Collections);
            shelf.list.push(crate::studio_library::Entry { id: imported_id.clone(), name: imported_name });
            shelf.current = imported_id.clone();
        }
        crate::studio_library::save(&home, &lib);

        let reloaded = crate::studio_library::load(&home);
        assert_eq!(reloaded.shelf(crate::studio_library::Kind::Collections).current, imported_id);
        assert!(imported_path.is_file());
        assert_eq!(obs_report.web_pages, usize::from(web.is_some()));

        // Undo the import: restore previous_id and remove imported file and library entry.
        let mut undone = crate::studio_library::load(&home);
        {
            let shelf = undone.shelf_mut(crate::studio_library::Kind::Collections);
            shelf.current = previous_id.clone();
            shelf.list.retain(|item| item.id != imported_id);
        }
        crate::studio_library::save(&home, &undone);
        if let Err(e) = std::fs::remove_file(&imported_path) {
            panic!("remove undone collection: {e}");
        }
        let after_undo = crate::studio_library::load(&home);
        assert_eq!(after_undo.shelf(crate::studio_library::Kind::Collections).current, previous_id);
        assert!(!imported_path.exists());

        // Streamlabs preview & import under FERROCAST_HOME.
        let sl_list = ferrocast_studio::streamlabs::list(&sl_dir);
        assert_eq!(sl_list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["sl_neon"]);
        let sl_preview = ferrocast_studio::import_preview::preview_streamlabs(
            &sl_dir,
            "sl_neon",
            web,
            Some(&inputs),
            Some(&filters),
        )
        .unwrap_or_else(|e| panic!("sl preview: {e}"));
        assert_eq!(sl_preview.web_pages, usize::from(web.is_some()));
        // Cameras and sound devices are only asked for again where the import resets them (a Mac).
        assert_eq!(!sl_preview.device_resets.is_empty(), cfg!(target_os = "macos"));
        assert!(sl_preview.skipped.iter().any(|s| s.kind == "streamlabs_spin_wheel"));

        let mut sl_doc =
            ferrocast_studio::streamlabs::read(&sl_dir, "sl_neon").unwrap_or_else(|e| panic!("sl read: {e}"));
        let sl_report = ferrocast_studio::import::convert(&mut sl_doc, web, Some(&inputs));
        assert_eq!(sl_report.web_pages, usize::from(web.is_some()));
        let sl_path = crate::studio_library::file(&home, crate::studio_library::Kind::Collections, "sl-neon");
        if let Err(e) = std::fs::write(&sl_path, sl_doc.to_string()) {
            panic!("write sl imported: {e}");
        }
        assert!(sl_path.is_file());
        println!(
            "real importer verified under {}: obs(scenes={}, sources={}, web_pages={}, skipped={}), sl(scenes={}, sources={}, web_pages={}, resets={}, skipped={})",
            home.display(),
            obs_preview.scenes,
            obs_preview.sources,
            obs_report.web_pages,
            obs_preview.skipped.len(),
            sl_preview.scenes,
            sl_preview.sources,
            sl_report.web_pages,
            sl_preview.device_resets.len(),
            sl_preview.skipped.len()
        );

        drop(engine);
    }

    #[test]
    fn studio_run_and_scenes_json_expose_and_mutate_named_transitions_and_matrix() {
        use ferrocast_engine::studio_host::{NamedTransitionInput, StudioAct};
        use serde_json::json;

        let c =
            ferrocast_studio::stage::Stage::new(ferrocast_studio::Canvas { width: 320, height: 180, fps: 30 }, "Main")
                .unwrap_or_else(|e| panic!("{e}"));
        c.add_scene("BRB").unwrap();

        let created = super::run(
            &c,
            StudioAct::AddNamedTransition {
                transition: NamedTransitionInput {
                    name: "Scene Swipe".into(),
                    kind: "swipe_transition".into(),
                    ms: 350,
                    settings: json!({ "direction": "left" }),
                    default: false,
                },
            },
        )
        .unwrap();
        assert_eq!(created["name"], "Scene Swipe");

        super::run(
            &c,
            StudioAct::SetTransitionMatrixRule {
                from: Some("Main".into()),
                to: Some("BRB".into()),
                transition: Some("Scene Swipe".into()),
            },
        )
        .unwrap();
        super::run(&c, StudioAct::RenameNamedTransition { name: "Scene Swipe".into(), new_name: "Quick Swipe".into() })
            .unwrap();

        let doc = super::scenes_json(&c).unwrap();
        assert!(doc["named_transitions"].as_array().unwrap().iter().any(|t| t["name"] == "Quick Swipe"));
        assert_eq!(doc["transition_matrix"][0]["transition"], "Quick Swipe");
        assert_eq!(doc["transition_matrix"][0]["from"], "Main");
        assert_eq!(doc["transition_matrix"][0]["to"], "BRB");
    }

    #[test]
    fn screenshot_job_runs_on_worker_thread_and_skips_empty_file_on_error() {
        let dir = std::env::temp_dir().join(format!("fc-shot-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let caller = std::thread::current().id();

        let dir_err = dir.clone();
        let err_res = rt.block_on(super::finish_screenshot_job(
            dir_err,
            Box::new(move || {
                assert_ne!(std::thread::current().id(), caller, "screenshot job must not run on caller thread");
                Err("item missing".into())
            }),
        ));
        assert!(err_res.is_err());
        assert!(!dir.exists(), "failed screenshot job must not leave an empty png or directory");

        let first = rt
            .block_on(super::finish_screenshot_job(
                dir.clone(),
                Box::new(move || {
                    assert_ne!(std::thread::current().id(), caller);
                    Ok((vec![255u8; 4 * 4 * 4], (4, 4)))
                }),
            ))
            .unwrap();
        let second = rt
            .block_on(super::finish_screenshot_job(dir.clone(), Box::new(|| Ok((vec![255u8; 4 * 4 * 4], (4, 4))))))
            .unwrap();
        let p1 = std::path::PathBuf::from(first["file"].as_str().unwrap());
        let p2 = std::path::PathBuf::from(second["file"].as_str().unwrap());
        assert_ne!(p1, p2);
        assert!(p1.exists() && p2.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_still_cache_evicts_waiting_or_gone_camera_and_returns_structured_waiting() {
        use super::source_still::{FlightMap, ReuseJob, StillAnswer, StillCache, StillTarget, resolve};
        use ferrocast_studio::stage::MatchingCameraWait;
        use serde_json::json;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        use tokio::sync::Semaphore;

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let cache = StillCache::default();
        let flights = Mutex::new(None::<FlightMap>);
        let turns = Arc::new(Semaphore::new(2));
        let target = StillTarget::Camera("iphone-cam".into(), 240);

        // Connected camera caches picture and serves it while same epoch, connected, and not waiting.
        cache.insert(target.clone(), 1, "data:image/jpeg;base64,FRESH".into());
        let hit = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                |_, _, _| async { panic!("must not probe on connected cache hit") },
            ))
            .unwrap();
        assert_eq!(hit, json!({ "image": "data:image/jpeg;base64,FRESH" }));

        // WaitingWithSession (e.g. Continuity Camera in interruption grace) returns waiting WITHOUT probing hardware.
        let session_res = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::WaitingWithSession, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                |_, _, _| async { panic!("must not open competing probe while stage holds session") },
            ))
            .unwrap();
        assert_eq!(session_res, json!({ "image": null, "waiting": true }));

        // WaitingWithoutSession skips reuse fast path, evicts KEPT, and still allows probing.
        let fast_path_ignored = Arc::new(AtomicBool::new(false));
        let fast_path_flag = fast_path_ignored.clone();
        let waiting_res = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::WaitingWithoutSession, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                |_, _, permit| {
                    let fast_path_flag = fast_path_flag.clone();
                    async move {
                        drop(permit);
                        fast_path_flag.store(true, Ordering::Release);
                        StillAnswer::waiting()
                    }
                },
            ))
            .unwrap();
        assert!(fast_path_ignored.load(Ordering::Acquire));
        assert_eq!(waiting_res, json!({ "image": null, "waiting": true }));
        assert!(waiting_res.get("error").is_none(), "transient wait must omit error: {waiting_res}");

        // (a) Gone camera or hotplug epoch change during awaited camera_connected check invalidates KEPT.
        cache.insert(target.clone(), 2, "data:image/jpeg;base64,STALE_MID_CHECK".into());
        let epoch = AtomicU64::new(2);
        let epoch_bump_mid_check = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || epoch.load(Ordering::Acquire),
                |_| {
                    let e = &epoch;
                    async move {
                        e.store(3, Ordering::Release);
                        true
                    }
                },
                |_, _, _| async { StillAnswer::waiting() },
            ))
            .unwrap();
        assert_eq!(epoch_bump_mid_check, json!({ "image": null, "waiting": true }));

        // Hotplug epoch changing during an active probe does not deliver stale pre-hotplug picture.
        let epoch2 = AtomicU64::new(3);
        let stale_probe = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || epoch2.load(Ordering::Acquire),
                |_| async { true },
                |_, _, _| {
                    let e = &epoch2;
                    async move {
                        e.store(4, Ordering::Release);
                        StillAnswer::image("data:image/jpeg;base64,PRE_HOTPLUG".into())
                    }
                },
            ))
            .unwrap();
        assert_eq!(stale_probe, json!({ "image": null, "waiting": true }));

        // Even with unchanged epoch (Windows/Linux), `camera_connected == false` evicts KEPT.
        cache.insert(target.clone(), 4, "data:image/jpeg;base64,STALE2".into());
        let unplugged_res = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 4,
                |_| async { false },
                |_, _, _| async { StillAnswer::waiting() },
            ))
            .unwrap();
        assert_eq!(unplugged_res, json!({ "image": null, "waiting": true }));

        // (c) Hard error (permission / in use) preserves `error` and omits `waiting`.
        let err_res = rt
            .block_on(resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 4,
                |_| async { true },
                |_, _, _| async { StillAnswer::error("the camera couldn't open (in use)".into()) },
            ))
            .unwrap();
        assert_eq!(err_res, json!({ "image": null, "error": "the camera couldn't open (in use)" }));
        assert!(err_res.get("waiting").is_none());

        // Dropping a cancelled caller still keeps the OwnedSemaphorePermit inside the worker until exit.
        let probe_started = Arc::new(tokio::sync::Notify::new());
        let release_probe = Arc::new(tokio::sync::Notify::new());
        let ps = probe_started.clone();
        let rp = release_probe.clone();
        let permit_held = Arc::new(Mutex::new(None));
        let ph = permit_held.clone();
        rt.block_on(async {
            let fut = resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 4,
                |_| async { true },
                |_, _, permit| {
                    let ph = ph.clone();
                    let ps = ps.clone();
                    let rp = rp.clone();
                    async move {
                        *ph.lock().unwrap() = Some(permit);
                        ps.notify_one();
                        rp.notified().await;
                        StillAnswer::waiting()
                    }
                },
            );
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => unreachable!(),
                _ = probe_started.notified() => {}
            }
            // Drop `fut` (simulates caller disconnect) while worker holds permit.
            assert_eq!(turns.available_permits(), 1, "permit must stay held by worker after caller drop");
            *permit_held.lock().unwrap() = None;
            assert_eq!(turns.available_permits(), 2);
            release_probe.notify_one();
        });

        // Single-flight concurrent follower receives `{ image: null, waiting: true }` when leader is waiting.
        let entered = Arc::new(tokio::sync::Notify::new());
        let follower_ready = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let entered_l = entered.clone();
        let gate_l = gate.clone();
        let follower_ready_f = follower_ready.clone();
        let (r1, r2, ()) = rt.block_on(async {
            let t1 = resolve(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 4,
                |_| async { true },
                |_, _, _| {
                    let entered_l = entered_l.clone();
                    let gate_l = gate_l.clone();
                    async move {
                        entered_l.notify_one();
                        gate_l.notified().await;
                        StillAnswer::waiting()
                    }
                },
            );
            let t2 = async {
                entered.notified().await;
                resolve(
                    &cache,
                    &flights,
                    &turns,
                    target.clone(),
                    || {
                        let f = follower_ready_f.clone();
                        async move {
                            f.notify_one();
                            (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)
                        }
                    },
                    || 4,
                    |_| async { true },
                    |_, _, _| async { panic!("follower must share leader flight") },
                )
                .await
            };
            let releaser = async {
                follower_ready.notified().await;
                tokio::task::yield_now().await;
                gate.notify_one();
            };
            tokio::join!(t1, t2, releaser)
        });
        assert_eq!(r1.unwrap(), json!({ "image": null, "waiting": true }));
        assert_eq!(r2.unwrap(), json!({ "image": null, "waiting": true }));
    }

    #[test]
    fn source_still_follower_does_not_lose_wakeup_before_await() {
        use super::source_still::{FlightMap, ReuseJob, StillAnswer, StillCache, StillTarget, resolve_with_hook};
        use ferrocast_studio::stage::MatchingCameraWait;
        use serde_json::json;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll, Wake, Waker};
        use tokio::sync::Semaphore;

        struct FlagWake(AtomicBool);
        impl Wake for FlagWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let cache = StillCache::default();
        let flights = Mutex::new(None::<FlightMap>);
        let turns = Arc::new(Semaphore::new(2));
        let target = StillTarget::Camera("cam-lost-wakeup".into(), 240);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let release_rx_slot = Mutex::new(Some(release_rx));

        let follower_result = std::thread::scope(|s| {
            let cache_ref = &cache;
            let flights_ref = &flights;
            let turns_ref = &turns;
            let tgt_l = target.clone();
            s.spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let res = rt.block_on(resolve_with_hook(
                    cache_ref,
                    flights_ref,
                    turns_ref,
                    tgt_l,
                    || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
                    || 1,
                    |_| std::future::ready(true),
                    |_, _, _permit| {
                        let entered_tx = entered_tx.clone();
                        let rx = release_rx_slot.lock().unwrap().take().unwrap();
                        async move {
                            entered_tx.send(()).unwrap();
                            rx.recv().unwrap();
                            StillAnswer::image("data:image/jpeg;base64,WAKE_OK".into())
                        }
                    },
                    || {},
                ));
                done_tx.send(res).unwrap();
            });

            entered_rx.recv().unwrap();

            let mut release_tx = Some(release_tx);
            let mut done_rx = Some(done_rx);
            let mut follower_fut = Box::pin(resolve_with_hook(
                &cache,
                &flights,
                &turns,
                target.clone(),
                || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
                || 1,
                |_| std::future::ready(true),
                |_, _, _| async { panic!("follower must share leader flight") },
                move || {
                    if let Some(tx) = release_tx.take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = done_rx.take() {
                        let leader_res = rx.recv().unwrap().unwrap();
                        assert_eq!(leader_res, json!({ "image": "data:image/jpeg;base64,WAKE_OK" }));
                    }
                },
            ));

            let wake = Arc::new(FlagWake(AtomicBool::new(false)));
            let waker = Waker::from(wake.clone());
            let mut cx = Context::from_waker(&waker);
            let mut poll = follower_fut.as_mut().poll(&mut cx);
            while matches!(poll, Poll::Pending) && wake.0.swap(false, Ordering::AcqRel) {
                poll = follower_fut.as_mut().poll(&mut cx);
            }
            poll
        });

        match follower_result {
            Poll::Ready(Ok(val)) => assert_eq!(val, json!({ "image": "data:image/jpeg;base64,WAKE_OK" })),
            Poll::Ready(Err(e)) => panic!("follower errored unexpectedly: {e}"),
            Poll::Pending => panic!("follower lost Notify wakeup after leader completed"),
        }
    }

    #[test]
    fn source_still_cancelled_leader_does_not_serve_blank_to_followers() {
        use super::source_still::{FlightMap, ReuseJob, StillAnswer, StillCache, StillTarget, resolve_with_hook};
        use ferrocast_studio::stage::MatchingCameraWait;
        use serde_json::json;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll, Wake, Waker};
        use tokio::sync::Semaphore;

        struct FlagWake(AtomicBool);
        impl Wake for FlagWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let cache = StillCache::default();
        let flights = Mutex::new(None::<FlightMap>);
        let turns = Arc::new(Semaphore::new(2));
        let target = StillTarget::Camera("cam-cancelled-leader".into(), 240);
        let probe_calls = Arc::new(AtomicUsize::new(0));

        // Case A: follower is already awaiting `Notify` when leader is dropped/cancelled.
        let mut leader_fut = Some(Box::pin(resolve_with_hook(
            &cache,
            &flights,
            &turns,
            target.clone(),
            || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
            || 1,
            |_| std::future::ready(true),
            {
                let calls = probe_calls.clone();
                move |_, _, _permit| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::AcqRel);
                        std::future::pending::<StillAnswer>().await
                    }
                }
            },
            || {},
        )));
        let leader_wake = Arc::new(FlagWake(AtomicBool::new(false)));
        let leader_waker = Waker::from(leader_wake);
        let mut cx_l = Context::from_waker(&leader_waker);
        assert!(matches!(leader_fut.as_mut().unwrap().as_mut().poll(&mut cx_l), Poll::Pending));
        assert_eq!(probe_calls.load(Ordering::Acquire), 1);

        let mut follower_fut = Box::pin(resolve_with_hook(
            &cache,
            &flights,
            &turns,
            target.clone(),
            || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
            || 1,
            |_| std::future::ready(true),
            {
                let calls = probe_calls.clone();
                move |_, _, _permit| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::AcqRel);
                        StillAnswer::image("data:image/jpeg;base64,RETRY_A_OK".into())
                    }
                }
            },
            || {},
        ));
        let wake_a = Arc::new(FlagWake(AtomicBool::new(false)));
        let waker_a = Waker::from(wake_a.clone());
        let mut cx_a = Context::from_waker(&waker_a);
        assert!(matches!(follower_fut.as_mut().poll(&mut cx_a), Poll::Pending));

        drop(leader_fut.take());
        assert!(wake_a.0.swap(false, Ordering::AcqRel), "follower must be woken on leader cancellation");
        let mut poll_a = follower_fut.as_mut().poll(&mut cx_a);
        while matches!(poll_a, Poll::Pending) && wake_a.0.swap(false, Ordering::AcqRel) {
            poll_a = follower_fut.as_mut().poll(&mut cx_a);
        }
        match poll_a {
            Poll::Ready(Ok(val)) => assert_eq!(val, json!({ "image": "data:image/jpeg;base64,RETRY_A_OK" })),
            Poll::Ready(Err(e)) => panic!("follower errored unexpectedly: {e}"),
            Poll::Pending => panic!("follower stalled after leader cancellation"),
        }
        assert_eq!(probe_calls.load(Ordering::Acquire), 2);

        // Case B: leader is cancelled after follower reads Pending but before follower awaits `Notify`.
        cache.evict(&target);
        let mut leader_b = Some(Box::pin(resolve_with_hook(
            &cache,
            &flights,
            &turns,
            target.clone(),
            || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
            || 1,
            |_| std::future::ready(true),
            {
                let calls = probe_calls.clone();
                move |_, _, _permit| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::AcqRel);
                        std::future::pending::<StillAnswer>().await
                    }
                }
            },
            || {},
        )));
        let wake_b_l = Arc::new(FlagWake(AtomicBool::new(false)));
        let waker_b_l = Waker::from(wake_b_l);
        let mut cx_b_l = Context::from_waker(&waker_b_l);
        assert!(matches!(leader_b.as_mut().unwrap().as_mut().poll(&mut cx_b_l), Poll::Pending));
        assert_eq!(probe_calls.load(Ordering::Acquire), 3);

        let mut follower_b = Box::pin(resolve_with_hook(
            &cache,
            &flights,
            &turns,
            target,
            || std::future::ready((MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)),
            || 1,
            |_| std::future::ready(true),
            {
                let calls = probe_calls.clone();
                move |_, _, _permit| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::AcqRel);
                        StillAnswer::image("data:image/jpeg;base64,RETRY_B_OK".into())
                    }
                }
            },
            move || {
                drop(leader_b.take());
            },
        ));
        let wake_b = Arc::new(FlagWake(AtomicBool::new(false)));
        let waker_b = Waker::from(wake_b.clone());
        let mut cx_b = Context::from_waker(&waker_b);
        let mut poll_b = follower_b.as_mut().poll(&mut cx_b);
        while matches!(poll_b, Poll::Pending) && wake_b.0.swap(false, Ordering::AcqRel) {
            poll_b = follower_b.as_mut().poll(&mut cx_b);
        }
        match poll_b {
            Poll::Ready(Ok(val)) => assert_eq!(val, json!({ "image": "data:image/jpeg;base64,RETRY_B_OK" })),
            Poll::Ready(Err(e)) => panic!("follower B errored unexpectedly: {e}"),
            Poll::Pending => panic!("follower B lost wakeup or stalled on pre-await leader cancel"),
        }
        assert_eq!(probe_calls.load(Ordering::Acquire), 4);
    }

    #[test]
    fn source_status_treats_macos_avcapture_variants_as_camera_kinds() {
        use ferrocore::engine::CameraStatus;
        use serde_json::json;

        let waiting = CameraStatus { state: "waiting", message: Some("looking for camera"), tries: 2 };
        for kind in [
            "av_capture_input",
            "av_capture_input_v2",
            "macos-avcapture",
            "macos-avcapture-fast",
            "dshow_input",
            "v4l2_input",
        ] {
            assert_eq!(
                super::camera_source_status_json(kind, Some(waiting.clone())),
                Some(json!({
                    "camera": {
                        "state": "waiting",
                        "message": "looking for camera",
                        "tries": 2,
                    }
                })),
                "{kind} must expose camera status"
            );
            assert_eq!(super::camera_source_status_json(kind, None), Some(json!({})));
        }
        assert_eq!(super::camera_source_status_json("color_source", Some(waiting)), None);
    }

    #[test]
    fn source_still_fresh_bypasses_cache_but_keeps_single_flight_and_permit() {
        use super::source_still::{FlightMap, ReuseJob, StillAnswer, StillCache, StillTarget, resolve_fresh};
        use ferrocast_studio::stage::MatchingCameraWait;
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use tokio::sync::{Notify, Semaphore};

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let cache = StillCache::default();
        let flights = Mutex::new(None::<FlightMap>);
        let turns = Arc::new(Semaphore::new(2));

        // 1. Camera: warm cache serves cached frame on fresh=false, bypasses cache on fresh=true,
        // deduplicates overlapping fresh=true callers through single-flight, and holds semaphore permit.
        let cam = StillTarget::Camera("iphone-fresh".into(), 240);
        cache.insert(cam.clone(), 1, "data:image/jpeg;base64,CACHED".into());

        let hit = rt
            .block_on(resolve_fresh(
                &cache,
                &flights,
                &turns,
                cam.clone(),
                false,
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                |_, _, _| async { panic!("fresh=false must hit warm cache") },
            ))
            .unwrap();
        assert_eq!(hit, json!({ "image": "data:image/jpeg;base64,CACHED" }));

        let bypassed = rt
            .block_on(resolve_fresh(
                &cache,
                &flights,
                &turns,
                cam.clone(),
                true,
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                |_, _, _| async { StillAnswer::image("data:image/jpeg;base64,FRESH_FIRST".into()) },
            ))
            .unwrap();
        assert_eq!(bypassed, json!({ "image": "data:image/jpeg;base64,FRESH_FIRST" }));

        let probe_calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let follower_ready = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (r1, r2, ()) = rt.block_on(async {
            let calls = probe_calls.clone();
            let ent = entered.clone();
            let gt = gate.clone();
            let t_permits = turns.clone();
            let t1 = resolve_fresh(
                &cache,
                &flights,
                &turns,
                cam.clone(),
                true,
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 1,
                |_| async { true },
                move |_, _, permit| {
                    let calls = calls.clone();
                    let ent = ent.clone();
                    let gt = gt.clone();
                    let t_permits = t_permits.clone();
                    async move {
                        calls.fetch_add(1, Ordering::AcqRel);
                        assert_eq!(t_permits.available_permits(), 1, "fresh probe must acquire permit");
                        ent.notify_one();
                        gt.notified().await;
                        drop(permit);
                        StillAnswer::image("data:image/jpeg;base64,FRESH_CAM".into())
                    }
                },
            );
            let fr = follower_ready.clone();
            let t2 = async {
                entered.notified().await;
                resolve_fresh(
                    &cache,
                    &flights,
                    &turns,
                    cam.clone(),
                    true,
                    || {
                        let fr = fr.clone();
                        async move {
                            fr.notify_one();
                            (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>)
                        }
                    },
                    || 1,
                    |_| async { true },
                    |_, _, _| async { panic!("concurrent fresh follower must join single-flight") },
                )
                .await
            };
            let releaser = async {
                follower_ready.notified().await;
                tokio::task::yield_now().await;
                gate.notify_one();
            };
            tokio::join!(t1, t2, releaser)
        });
        assert_eq!(probe_calls.load(Ordering::Acquire), 1);
        assert_eq!(r1.unwrap(), json!({ "image": "data:image/jpeg;base64,FRESH_CAM" }));
        assert_eq!(r2.unwrap(), json!({ "image": "data:image/jpeg;base64,FRESH_CAM" }));

        // 2. Display target also bypasses StillCache upfront check when fresh=true.
        let disp = StillTarget::Display(1, 240);
        cache.insert(disp.clone(), 0, "data:image/jpeg;base64,CACHED_DISP".into());
        let disp_res = rt
            .block_on(resolve_fresh(
                &cache,
                &flights,
                &turns,
                disp,
                true,
                || async { (MatchingCameraWait::LiveOrAbsent, None::<ReuseJob>) },
                || 0,
                |_| async { true },
                |_, _, _| async { StillAnswer::image("data:image/jpeg;base64,FRESH_DISP".into()) },
            ))
            .unwrap();
        assert_eq!(disp_res, json!({ "image": "data:image/jpeg;base64,FRESH_DISP" }));
    }

    #[test]
    fn local_capture_block_syncs_and_resets_cleanly() {
        use std::sync::Mutex;
        use std::sync::atomic::Ordering;

        use super::{
            CAPTURE_RECORDING, CAPTURE_REPLAY, Held, LOCAL_CAPTURE_FLAGS, current_local_capture_block,
            sync_local_capture_flags,
        };

        static FLAG_LOCK: Mutex<()> = Mutex::new(());
        let _g = FLAG_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = LOCAL_CAPTURE_FLAGS.load(Ordering::Acquire);

        LOCAL_CAPTURE_FLAGS.store(CAPTURE_RECORDING, Ordering::Release);
        let block_rec = current_local_capture_block();
        assert!(block_rec.recording && !block_rec.replay);

        LOCAL_CAPTURE_FLAGS.store(CAPTURE_REPLAY, Ordering::Release);
        let block_rep = current_local_capture_block();
        assert!(!block_rep.recording && block_rep.replay);

        LOCAL_CAPTURE_FLAGS.store(CAPTURE_RECORDING | CAPTURE_REPLAY, Ordering::Release);
        sync_local_capture_flags(&Held::default());
        let cleared = current_local_capture_block();
        assert!(!cleared.recording && !cleared.replay);

        LOCAL_CAPTURE_FLAGS.store(prev, Ordering::Release);
    }

    #[test]
    fn desktop_machine_load_reports_valid_cpu_percentage() {
        let cpu = ferrocast_studio::system::machine_cpu_pct();
        assert!((0.0..=100.0).contains(&cpu) && cpu > 0.0, "unexpected machine CPU pct: {cpu}");
    }
}

#[cfg(test)]
mod collection_profile_tests {
    use super::*;

    static ENGINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_engine() -> std::sync::MutexGuard<'static, ()> {
        ENGINE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct ResetAudioFormat;
    impl Drop for ResetAudioFormat {
        fn drop(&mut self) {
            apply_audio_format(&StudioSettings::default());
        }
    }

    fn wait_for_frames(engine: &ferrocast_studio::stage::EngineRef) -> u64 {
        for _ in 0..400 {
            let n = engine.frames();
            if n > 0 {
                return n;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        engine.frames()
    }

    fn make_collection_doc(
        canvas: Canvas,
        main_scene: &str,
        preview_scene: &str,
        vertical_scene: Option<&str>,
        extra_source: &str,
    ) -> Value {
        let c = Collection::new(canvas, main_scene).unwrap_or_else(|e| panic!("new collection: {e}"));
        c.add_scene(preview_scene).unwrap_or_else(|e| panic!("add preview scene: {e}"));
        c.add_source(main_scene, "color_source_v3", extra_source, &json!({}), false)
            .unwrap_or_else(|e| panic!("add extra source: {e}"));
        c.add_source(preview_scene, "color_source_v3", &format!("{extra_source}_p"), &json!({}), false)
            .unwrap_or_else(|e| panic!("add preview source: {e}"));
        c.set_studio_mode(true);
        c.set_preview(preview_scene).unwrap_or_else(|e| panic!("set_preview: {e}"));
        if let Some(vs) = vertical_scene {
            c.start_vertical(vs).unwrap_or_else(|e| panic!("start_vertical: {e}"));
            c.add_source(vs, "color_source_v3", &format!("{extra_source}_v"), &json!({}), false)
                .unwrap_or_else(|e| panic!("add vertical source: {e}"));
        }
        c.save(COLLECTION)
    }

    fn write_json_sync(path: &std::path::Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
        }
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap_or_else(|e| panic!("json: {e}")))
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    fn populate_cached_views(h: &mut Held, data: &std::path::Path, paused: bool) {
        h.preview_at = Some((Rect { x: 10.0, y: 20.0, w: 640.0, h: 360.0, vh: 800.0 }, 2.0));
        h.preview_canvas = Some((12.0, 22.0, 636.0, 356.0));
        h.preview_bg = Some(0xFF112233);
        h.preview_stripe = Some(0xFF445566);
        h.preview_holes = vec![(1.0, 2.0, 30.0, 40.0, 6.0)];
        h.preview_opacity = Some(0.85);
        h.preview_shade = 0.25;
        h.preview_paused = paused;
        h.selected = Some(("MainA".into(), vec![1]));
        h.frames = Some("MainA".into());

        h.program_at = Some((Rect { x: 660.0, y: 20.0, w: 320.0, h: 180.0, vh: 800.0 }, 2.0));
        h.program_bg = Some(0xFF778899);
        h.program_stripe = Some(0xFF121314);
        h.program_holes = vec![(5.0, 6.0, 20.0, 20.0, 4.0)];
        h.program_opacity = Some(0.9);
        h.program_shade = 0.15;

        h.vertical.at = Some((Rect { x: 990.0, y: 20.0, w: 180.0, h: 320.0, vh: 800.0 }, 1.5));
        h.vertical.canvas = Some((992.0, 24.0, 176.0, 312.0));
        h.vertical.bg = Some(0xFFAABBCC);
        h.vertical.stripe = Some(0xFFDDEEFF);
        h.vertical.holes = vec![(9.0, 10.0, 12.0, 14.0, 2.0)];
        h.vertical.opacity = Some(0.75);
        h.vertical.shade = 0.4;

        make_preview(ViewCtx::Test, h).unwrap_or_else(|e| panic!("make_preview: {e}"));
        make_program_view(ViewCtx::Test, h).unwrap_or_else(|e| panic!("make_program_view: {e}"));
        make_vertical_view(ViewCtx::Test, h).unwrap_or_else(|e| panic!("make_vertical_view: {e}"));

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            for req in [
                ProjectorReq { what: "multiview".into(), name: None, screen: None, on_top: false },
                ProjectorReq { what: "program".into(), name: None, screen: None, on_top: true },
                ProjectorReq { what: "source".into(), name: Some("SharedColor".into()), screen: None, on_top: false },
            ] {
                open_projector(ViewCtx::Test, h, &req, data).unwrap_or_else(|e| panic!("open_projector: {e}"));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = data;
    }

    fn assert_preview_look(p: &Preview, h: &Held) {
        let tv = p.test_view().expect("test view");
        let (rect, scale) = h.preview_at.expect("preview_at");
        assert_eq!(
            (tv.rect.x, tv.rect.y, tv.rect.w, tv.rect.h, tv.rect.vh, tv.scale),
            (rect.x, rect.y, rect.w, rect.h, rect.vh, scale)
        );
        assert_eq!(*tv.area.borrow(), h.preview_canvas);
        assert_eq!(tv.bg.get(), h.preview_bg);
        assert_eq!(tv.stripe.get(), h.preview_stripe);
        assert_eq!(*tv.holes.borrow(), h.preview_holes);
        assert!((tv.opacity.get() - h.preview_opacity.unwrap_or(1.0)).abs() < 1e-6);
        assert!((tv.shade.get() - h.preview_shade).abs() < 1e-6);
        assert_eq!(tv.paused.get(), h.preview_paused);
    }

    fn assert_program_look(p: &Preview, h: &Held) {
        let tv = p.test_view().expect("test program view");
        let (rect, scale) = h.program_at.expect("program_at");
        assert_eq!(
            (tv.rect.x, tv.rect.y, tv.rect.w, tv.rect.h, tv.rect.vh, tv.scale),
            (rect.x, rect.y, rect.w, rect.h, rect.vh, scale)
        );
        assert_eq!(tv.bg.get(), h.program_bg);
        assert_eq!(tv.stripe.get(), h.program_stripe);
        assert_eq!(*tv.holes.borrow(), h.program_holes);
        assert!((tv.opacity.get() - h.program_opacity.unwrap_or(1.0)).abs() < 1e-6);
        assert!((tv.shade.get() - h.program_shade).abs() < 1e-6);
        assert_eq!(tv.paused.get(), h.preview_paused);
    }

    fn assert_vertical_look(p: &Preview, h: &Held) {
        let tv = p.test_view().expect("test vertical view");
        let (rect, scale) = h.vertical.at.expect("vertical.at");
        assert_eq!(
            (tv.rect.x, tv.rect.y, tv.rect.w, tv.rect.h, tv.rect.vh, tv.scale),
            (rect.x, rect.y, rect.w, rect.h, rect.vh, scale)
        );
        assert_eq!(*tv.area.borrow(), h.vertical.canvas);
        assert_eq!(tv.bg.get(), h.vertical.bg);
        assert_eq!(tv.stripe.get(), h.vertical.stripe);
        assert_eq!(*tv.holes.borrow(), h.vertical.holes);
        assert!((tv.opacity.get() - h.vertical.opacity.unwrap_or(1.0)).abs() < 1e-6);
        assert!((tv.shade.get() - h.vertical.shade).abs() < 1e-6);
        assert_eq!(tv.paused.get(), h.preview_paused);
    }

    #[test]
    fn swap_collection_releases_old_engine_rebuilds_views_and_opens_new_engine_with_fps() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");

        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", Some("VertB"), "SharedColor");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let settings = StudioSettings {
            multiview_layout: "bottom".into(),
            multiview_labels: false,
            multiview_safe_areas: true,
            projector_on_top: false,
            projector_hide_cursor: true,
            ..StudioSettings::default()
        };
        let mut h = Held { settings: Some(settings), collection: Some(col_a), ..Held::default() };
        populate_cached_views(&mut h, dir.path(), false);

        let old_engine = h.collection.as_ref().expect("col A").engine_ref().expect("engine A");
        let old_weak = old_engine.downgrade();
        assert!(h.preview.as_ref().expect("preview").engine_ref().expect("ref").same(&old_engine));
        assert!(h.program_view.as_ref().expect("program").engine_ref().expect("ref").same(&old_engine));
        assert!(h.vertical.view.as_ref().expect("vertical").engine_ref().expect("ref").same(&old_engine));
        assert!(ferrocast_studio::stage::levels_now(&["SharedColor".into()], false).is_some());
        drop(old_engine);

        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));

        assert!(!old_weak.is_alive(), "old Stage own::Shared must be completely released after swap");
        let col_b = h.collection.as_ref().expect("col B");
        assert!(col_b.has_engine(), "new collection must open its engine immediately during swap");
        let eng_b = col_b.engine_ref().expect("engine B");
        assert!(wait_for_frames(&eng_b) > 0, "new engine must produce frames after swap");
        assert!(col_b.stats().fps > 0.0, "new engine stats().fps must be > 0 after swap");
        assert!(
            ferrocast_studio::stage::levels_now(&["SharedColor".into()], false).is_some(),
            "LIVE_ENGINE must track the new engine after swap"
        );
        assert!(h.selected.is_none());
        assert!(h.frames.is_none());

        let preview = h.preview.as_ref().expect("rebuilt preview");
        assert!(preview.engine_ref().expect("preview engine").same(&eng_b), "preview must hold new engine");
        assert_eq!(preview.shown_scene().as_deref(), Some("PreviewB"));
        assert_preview_look(preview, &h);
        assert!(preview.test_view().expect("tv").picked.borrow().is_none());
        assert!(preview.test_view().expect("tv").frames.borrow().is_none());

        let program = h.program_view.as_ref().expect("rebuilt program view");
        assert!(program.engine_ref().expect("program engine").same(&eng_b), "program view must hold new engine");
        assert_program_look(program, &h);

        let vertical = h.vertical.view.as_ref().expect("rebuilt vertical view");
        assert!(vertical.engine_ref().expect("vertical engine").same(&eng_b), "vertical view must hold new engine");
        assert_eq!(vertical.shown_scene().as_deref(), Some("VertB"));
        assert_vertical_look(vertical, &h);

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(
                h.projectors.len(),
                2,
                "source projectors close on collection switch; multiview & program rebuild"
            );
            for (_, p, req) in &h.projectors {
                assert_ne!(req.what, "source");
                assert!(p.engine_ref().same(&eng_b), "projector {} must hold new engine", req.what);
                let tp = p.test_proj().expect("test projector");
                assert_eq!(tp.on_top.get(), req.on_top);
                assert!(tp.hide_cursor.get());
                if req.what == "multiview" {
                    let names = tp.names.borrow().clone();
                    assert!(
                        names.contains(&"GameB".to_string()) && names.contains(&"PreviewB".to_string()),
                        "{names:?}"
                    );
                    assert!(!names.contains(&"MainA".to_string()), "{names:?}");
                    assert_eq!(*tp.preview.borrow(), Some("PreviewB".to_string()));
                    assert!(matches!(tp.look.borrow().layout, ferrocast_studio::MultiviewLayout::Bottom));
                    assert!(!tp.look.borrow().labels);
                    assert!(tp.look.borrow().safe_areas);
                }
            }
        }

        // Swapping back B -> A must release B's engine and open A's cleanly as well.
        let weak_b = eng_b.downgrade();
        drop(eng_b);
        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_b, &scenes_a)
            .unwrap_or_else(|e| panic!("swap B->A: {e}"));
        assert!(!weak_b.is_alive(), "collection B engine must be released on switch back");
        let col_a2 = h.collection.as_ref().expect("col A restored");
        let eng_a2 = col_a2.engine_ref().expect("engine A2");
        assert!(wait_for_frames(&eng_a2) > 0);
        assert!(col_a2.stats().fps > 0.0);
        assert_eq!(h.preview.as_ref().expect("preview").shown_scene().as_deref(), Some("PreviewA"));
        assert_eq!(h.vertical.view.as_ref().expect("vertical").shown_scene().as_deref(), Some("VertA"));

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn swap_collection_failed_load_restores_old_collection_and_all_views() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_bad = dir.path().join("collections").join("bad.json");

        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        // Valid JSON referencing `"Main"` in sources while having no scenes triggers add_scene("Main") failure.
        let bad_doc = json!({
            "ferrocast": { "program_copy": { "scene": "X", "items": [{ "name": "Main" }] } },
            "sources": [{ "id": "color_source_v3", "name": "Main" }],
            "scene_order": []
        });
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_bad, &bad_doc);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mut h = Held {
            settings: Some(StudioSettings::default()),
            collection: Some(col_a),
            collection_deferred: true,
            ..Held::default()
        };
        populate_cached_views(&mut h, dir.path(), false);

        let old_engine = h.collection.as_ref().expect("col A").engine_ref().expect("engine A");
        let old_weak = old_engine.downgrade();

        let err = swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_bad)
            .expect_err("invalid collection must fail");
        assert!(err.contains("didn't load"), "unexpected error: {err}");

        assert!(old_weak.is_alive(), "failed load must keep the old engine alive");
        let kept = h.collection.as_ref().expect("old collection restored");
        assert!(kept.engine_ref().expect("kept engine").same(&old_engine));
        assert!(h.collection_deferred, "deferred flag must remain untouched on failed swap");
        assert_eq!(h.selected, Some(("MainA".into(), vec![1])));
        assert_eq!(h.frames.as_deref(), Some("MainA"));

        let preview = h.preview.as_ref().expect("preview preserved");
        assert!(preview.engine_ref().expect("ref").same(&old_engine));
        assert_eq!(preview.shown_scene().as_deref(), Some("PreviewA"));
        assert!(h.program_view.as_ref().expect("program preserved").engine_ref().expect("ref").same(&old_engine));
        let vert = h.vertical.view.as_ref().expect("vertical preserved on failed swap");
        assert!(vert.engine_ref().expect("ref").same(&old_engine));
        assert_eq!(vert.shown_scene().as_deref(), Some("VertA"));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(h.projectors.len(), 3, "all projectors must stay attached to old engine on failed swap");

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn swap_collection_paused_vertical_off_and_missing_source_projector() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");

        // Collection B has NO vertical enabled and NO `"SharedColor"` source.
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", None, "OnlyB");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mut h = Held { settings: Some(StudioSettings::default()), collection: Some(col_a), ..Held::default() };
        populate_cached_views(&mut h, dir.path(), true);

        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));

        assert!(h.preview.as_ref().expect("preview").test_view().expect("tv").paused.get());
        assert!(h.program_view.as_ref().expect("program").test_view().expect("tv").paused.get());
        assert!(h.vertical.view.is_none(), "vertical off in new collection must clear vertical view");
        assert!(h.vertical.at.is_some(), "cached vertical rect must be kept when vertical is off");
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(
                h.projectors.len(),
                2,
                "missing source projector skips cleanly while multiview & program rebuild"
            );
            assert!(h.projectors.iter().all(|(_, _, r)| r.what != "source"));
        }

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn profile_switch_canvas_audio_format_projectors_and_durable_read() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let s0 = StudioSettings { width: 1920, height: 1080, fps: 60, ..StudioSettings::default() };
        let canvas = Canvas { width: s0.width, height: s0.height, fps: s0.fps };
        let doc = make_collection_doc(canvas, "Main", "Preview", Some("Vert"), "SharedColor");
        let col = Collection::load(canvas, &doc, FIRST_SCENE).unwrap_or_else(|e| panic!("load: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col.engine_ref().is_err() {
            return;
        }
        let mut h = Held { settings: Some(s0.clone()), collection: Some(col), ..Held::default() };
        populate_cached_views(&mut h, dir.path(), false);

        // 1. Same-aspect resolution/fps change (1920x1080@60 -> 1280x720@30): releases old engine,
        // rebuilds ALL views INCLUDING open projectors, and keeps preview_canvas.
        let eng0 = h.collection.as_ref().expect("col").engine_ref().expect("eng0");
        let weak0 = eng0.downgrade();
        drop(eng0);

        let s1 = StudioSettings { width: 1280, height: 720, fps: 30, ..s0.clone() };
        let notes1 =
            apply_settings_on_main(ViewCtx::Test, &mut h, dir.path(), s1.clone()).unwrap_or_else(|e| panic!("s1: {e}"));
        assert!(notes1.is_empty(), "idle canvas switch should have no restart note: {notes1:?}");
        assert!(!weak0.is_alive(), "old engine must be released after canvas change");
        assert!(h.preview_canvas.is_some(), "same aspect ratio keeps cached preview_canvas");
        let eng1 = h.collection.as_ref().expect("col").engine_ref().expect("eng1");
        assert!(wait_for_frames(&eng1) > 0);
        assert!(h.collection.as_ref().expect("col").stats().fps > 0.0);
        assert!(h.preview.as_ref().expect("preview").engine_ref().expect("ref").same(&eng1));
        assert!(h.program_view.as_ref().expect("program").engine_ref().expect("ref").same(&eng1));
        assert!(h.vertical.view.as_ref().expect("vertical").engine_ref().expect("ref").same(&eng1));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(h.projectors.len(), 3, "open projectors must be rebuilt on canvas change");
            for (_, p, _) in &h.projectors {
                assert!(p.engine_ref().same(&eng1), "rebuilt projector must hold new engine");
            }
        }

        // 2. Sample rate & broadcast timing change while idle at SAME canvas reopens engine immediately!
        let weak1 = eng1.downgrade();
        drop(eng1);
        let s2 = StudioSettings {
            sample_rate: 44_100,
            fps_ntsc: true,
            multiview_layout: "left".into(),
            projector_on_top: true,
            projector_hide_cursor: true,
            ..s1
        };
        let notes2 =
            apply_settings_on_main(ViewCtx::Test, &mut h, dir.path(), s2.clone()).unwrap_or_else(|e| panic!("s2: {e}"));
        assert!(
            notes2.is_empty(),
            "idle audio/ntsc switch must reopen engine cleanly instead of warning restart: {notes2:?}"
        );
        assert!(!weak1.is_alive(), "old engine must be released when audio rate / NTSC changes while idle");
        let eng2 = h.collection.as_ref().expect("col").engine_ref().expect("eng2");
        assert_eq!(h.collection.as_ref().expect("col").engine_format(), Some((44_100, true)));
        assert!(h.preview.as_ref().expect("preview").engine_ref().expect("ref").same(&eng2));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(h.projectors.len(), 3);
            for (_, p, _) in &h.projectors {
                let tp = p.test_proj().expect("tp");
                assert!(tp.on_top.get(), "projector_on_top from profile must apply");
                assert!(tp.hide_cursor.get(), "projector_hide_cursor from profile must apply");
                if tp.what == "source" {
                    assert_eq!(tp.name.as_deref(), Some("SharedColor"));
                }
                if tp.what == "multiview" {
                    assert!(matches!(tp.look.borrow().layout, ferrocast_studio::MultiviewLayout::Left));
                }
            }
        }

        // 3. Aspect ratio change (1280x720 -> 1080x1920) clears stale preview_canvas.
        let weak2 = eng2.downgrade();
        drop(eng2);
        let s3 = StudioSettings { width: 1080, height: 1920, fps: 60, fps_ntsc: false, ..s2 };
        let _ =
            apply_settings_on_main(ViewCtx::Test, &mut h, dir.path(), s3.clone()).unwrap_or_else(|e| panic!("s3: {e}"));
        assert!(!weak2.is_alive());
        assert!(h.preview_canvas.is_none(), "aspect-ratio change must clear stale preview_canvas");

        // 4. Failed canvas change (engine held externally) rolls back settings and restores all views.
        let pin = h.collection.as_ref().expect("col").engine_ref().expect("pin");
        let s_bad = StudioSettings { width: 1920, height: 1080, fps: 30, sample_rate: 44_100, ..s3.clone() };
        assert!(apply_settings_on_main(ViewCtx::Test, &mut h, dir.path(), s_bad).is_err());
        assert_eq!(h.settings.as_ref().map(|s| (s.width, s.height)), Some((1080, 1920)));
        assert!(h.preview.as_ref().expect("preview restored").engine_ref().expect("ref").same(&pin));
        assert!(h.program_view.as_ref().expect("program restored").engine_ref().expect("ref").same(&pin));
        assert!(h.vertical.view.as_ref().expect("vertical restored").engine_ref().expect("ref").same(&pin));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(h.projectors.len(), 3, "projectors must restore on failed canvas change");
        drop(pin);

        // 5. load_profile_settings uses durable read (.bak fallback when main file is missing/corrupt).
        let profile_path = dir.path().join("profiles").join("p2.json");
        let profile_bak = ferrocast_store::durable::backup_path(&profile_path);
        let custom =
            StudioSettings { width: 1600, height: 900, fps: 50, video_kbps: 4200, ..StudioSettings::default() };
        write_json_sync(&profile_bak, &serde_json::to_value(&custom).unwrap_or(Value::Null));
        let loaded = load_profile_settings(&profile_path).unwrap_or_else(|e| panic!("bak load: {e}"));
        assert_eq!((loaded.width, loaded.height, loaded.fps, loaded.video_kbps), (1600, 900, 50, 4200));

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col05_swap_collection_clears_caption_state_and_rejects_stale_callbacks() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", None, "A");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", None, "B");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mut h = Held {
            settings: Some(StudioSettings::default()),
            collection: Some(col_a),
            captions_run: 7,
            ..Held::default()
        };
        let old_run = h.captions_run;
        CAPTION_SINK.with(|s| *s.borrow_mut() = Some((old_run, Box::new(|said, _last| *HEARD.lock().unwrap() = said))));
        *HEARD.lock().unwrap() = "stale caption".into();
        let prev_sentence_id = {
            let mut s = SENTENCE.lock().unwrap();
            s.0 = s.0.max(10);
            s.1 = "stale sentence".into();
            s.0
        };
        DONE.lock().unwrap().push_back((10, 123, "stale done".into()));
        *CAPTIONS_ERROR.lock().unwrap() = Some("stale error".into());

        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));

        assert!(h.captions_run > old_run, "captions_run must advance on collection swap");
        assert!(CAPTION_SINK.with(|s| s.borrow().is_none()), "CAPTION_SINK must be cleared");
        assert!(HEARD.lock().unwrap().is_empty(), "HEARD must clear on collection swap");
        {
            let s = SENTENCE.lock().unwrap();
            assert!(s.0 > prev_sentence_id, "SENTENCE id must increment monotonically, never reset to 0");
            assert!(s.1.is_empty(), "SENTENCE text must clear");
        }
        assert!(DONE.lock().unwrap().is_empty(), "DONE must clear on collection swap");
        assert!(CAPTIONS_ERROR.lock().unwrap().is_none(), "CAPTIONS_ERROR must clear");

        // Stale callback from pre-switch run must be ignored even if invoked after swap.
        CAPTION_SINK.with(|s| {
            if let Some((r, f)) = s.borrow().as_ref()
                && *r == old_run
            {
                f("leaked after swap".into(), true);
            }
        });
        assert!(HEARD.lock().unwrap().is_empty());

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col06_swap_collection_clears_replay_timers() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", None, "A");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", None, "B");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mut h = Held {
            settings: Some(StudioSettings::default()),
            collection: Some(col_a),
            replay_shown: Some(PathBuf::from("/tmp/replay.mp4")),
            replay_back: Some(("MainA".into(), "ReplayScene".into(), "ReplaySrc".into(), std::time::Instant::now())),
            replay_wait: Some(("ReplayScene".into(), "ReplaySrc".into(), std::time::Instant::now())),
            ..Held::default()
        };

        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));

        assert!(h.replay_shown.is_none(), "replay_shown must clear on collection switch");
        assert!(h.replay_back.is_none(), "replay_back must clear on collection switch");
        assert!(h.replay_wait.is_none(), "replay_wait must clear on collection switch");

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn col13_source_projectors_excluded_from_global_save_and_closed_on_collection_switch() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", Some("VertB"), "SharedColor");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        let mut h = Held {
            settings: Some(StudioSettings { projector_restore: true, ..StudioSettings::default() }),
            collection: Some(col_a),
            ..Held::default()
        };
        populate_cached_views(&mut h, dir.path(), false);
        assert_eq!(h.projectors.len(), 3);

        // 1. Global projectors.json must not persist source-specific projectors.
        save_projectors(&h, dir.path());
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        let saved: Vec<ProjectorReq> =
            ferrocast_store::durable::load_json_with_backup(&projectors_file(dir.path()), |_| true)
                .map(|l| l.value)
                .unwrap_or_default();
        assert!(saved.iter().all(|r| r.what != "source"), "source projectors must not be saved globally: {saved:?}");

        // 2. Switching collections closes source projectors even when B happens to have a same-named source.
        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));
        assert_eq!(h.projectors.len(), 2, "only program and multiview projectors carry over");
        assert!(h.projectors.iter().all(|(_, _, r)| r.what != "source"));

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col01_held_file_bindings_isolate_collection_and_profile_from_library() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let col_a = library::file(dir.path(), Kind::Collections, "a");
        let col_b = library::file(dir.path(), Kind::Collections, "b");
        let bad = library::file(dir.path(), Kind::Collections, "broken");
        let prof_a = library::file(dir.path(), Kind::Profiles, "pa");
        let prof_b = library::file(dir.path(), Kind::Profiles, "pb");
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        write_json_sync(&col_a, &make_collection_doc(canvas, "MainA", "PreviewA", None, "A"));
        write_json_sync(&col_b, &make_collection_doc(canvas, "MainB", "PreviewB", None, "B"));
        write_json_sync(
            &bad,
            &json!({
                "ferrocast": { "program_copy": { "scene": "X", "items": [{ "name": "Main" }] } },
                "sources": [{ "id": "color_source_v3", "name": "Main" }],
                "scene_order": []
            }),
        );

        let mut lib = library::load(dir.path());
        lib.collections.list.push(library::Entry { id: "a".into(), name: "A".into() });
        lib.collections.list.push(library::Entry { id: "b".into(), name: "B".into() });
        lib.collections.current = "b".into();
        lib.profiles.list.push(library::Entry { id: "pa".into(), name: "PA".into() });
        lib.profiles.list.push(library::Entry { id: "pb".into(), name: "PB".into() });
        lib.profiles.current = "pb".into();
        library::save(dir.path(), &lib);

        let col_doc_a = ferrocast_store::durable::load_json_with_backup::<Value>(&col_a, |_| true).unwrap().value;
        let col = Collection::load(canvas, &col_doc_a, FIRST_SCENE).unwrap();
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col.engine_ref().is_err() {
            return;
        }
        let mut h = Held {
            settings: Some(StudioSettings::default()),
            collection: Some(col),
            collection_id: Some("a".into()),
            collection_file: Some(col_a.clone()),
            profile_id: Some("pa".into()),
            profile_file: Some(prof_a.clone()),
            ..Held::default()
        };

        // Even when library.json points to "b"/"pb", active_* readers stick to Held's actual loaded paths.
        assert_eq!(active_collection_file(&h, dir.path()), col_a);
        assert_eq!(active_profile_file(&h, dir.path()), prof_a);

        // Failed swap leaves Held's collection_id and collection_file pointing at "a".
        assert!(swap_collection_to(ViewCtx::Test, &mut h, dir.path(), Some("broken"), &col_a, &bad, &[]).is_err());
        assert_eq!(h.collection_id.as_deref(), Some("a"));
        assert_eq!(active_collection_file(&h, dir.path()), col_a);

        // Successful swap atomically updates Held's collection_id and collection_file to "b".
        swap_collection_to(ViewCtx::Test, &mut h, dir.path(), Some("b"), &col_a, &col_b, &[])
            .unwrap_or_else(|e| panic!("swap a->b: {e}"));
        assert_eq!(h.collection_id.as_deref(), Some("b"));
        assert_eq!(active_collection_file(&h, dir.path()), col_b);

        // Profile apply updates Held's profile_id & profile_file and saves to pb (not pa).
        let next = StudioSettings { video_kbps: 4500, ..StudioSettings::default() };
        apply_settings_for_profile(ViewCtx::Test, &mut h, dir.path(), next, Some(("pb".into(), prof_b.clone())))
            .unwrap_or_else(|e| panic!("apply pb: {e}"));
        assert_eq!(h.profile_id.as_deref(), Some("pb"));
        assert_eq!(active_profile_file(&h, dir.path()), prof_b);
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        assert!(prof_b.exists(), "profile settings must save to the switched target path");
    }

    #[test]
    fn col02_load_profile_settings_errors_when_corrupt_without_valid_bak() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let missing = dir.path().join("profiles").join("brand-new.json");
        assert_eq!(
            load_profile_settings(&missing).unwrap(),
            StudioSettings::default(),
            "brand new profile without file defaults cleanly"
        );

        let corrupt = dir.path().join("profiles").join("corrupt.json");
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, b"{not valid json").unwrap();
        assert!(load_profile_settings(&corrupt).is_err(), "corrupt profile must fail instead of resetting to defaults");

        let invalid = dir.path().join("profiles").join("invalid.json");
        std::fs::write(&invalid, br#"{"width": 1}"#).unwrap();
        assert!(load_profile_settings(&invalid).is_err(), "invalid profile must fail instead of resetting to defaults");
    }

    #[test]
    fn col07_swap_collection_preserves_monitoring_device() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        write_json_sync(&scenes_a, &make_collection_doc(canvas, "MainA", "PreviewA", None, "A"));
        write_json_sync(&scenes_b, &make_collection_doc(canvas, "GameB", "PreviewB", None, "B"));

        let doc_a = ferrocast_store::durable::load_json_with_backup::<Value>(&scenes_a, |_| true).unwrap().value;
        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap();
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let s = StudioSettings {
            monitoring: Some(("Headphones".into(), "Headphones-Dev-Id".into())),
            true_peak: true,
            low_latency_audio: true,
            ..StudioSettings::default()
        };
        apply_collection_audio_prefs(&col_a, Some(&s));
        let mut h = Held { settings: Some(s), collection: Some(col_a), ..Held::default() };

        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));
        let mon = h.collection.as_ref().unwrap().monitoring_device();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(mon.as_deref(), Some("Headphones-Dev-Id"));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert!(mon.is_none());

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col09_swap_collection_releases_push_keys_before_saving_outgoing_collection() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", None, "A");
        let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", None, "B");
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap();
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mic_kind = if cfg!(target_os = "macos") {
            "coreaudio_input_capture"
        } else if cfg!(windows) {
            "wasapi_input_capture"
        } else {
            "pulse_input_capture"
        };
        col_a.add_source("MainA", mic_kind, "Mic", &json!({}), false).unwrap();
        col_a.set_muted("Mic", false).unwrap();
        let mut h = Held { settings: Some(StudioSettings::default()), collection: Some(col_a), ..Held::default() };

        swap_collection_to(
            ViewCtx::Test,
            &mut h,
            dir.path(),
            Some("b"),
            &scenes_a,
            &scenes_b,
            &["push_to_talk:Mic".into()],
        )
        .unwrap_or_else(|e| panic!("swap A->B: {e}"));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));

        let saved_a: Value = ferrocast_store::durable::load_json_with_backup(&scenes_a, |_| true).unwrap().value;
        let mic = saved_a["sources"].as_array().unwrap().iter().find(|s| s["name"] == "Mic").unwrap();
        assert_eq!(mic["muted"], json!(true), "held PTT mic must release to muted before saving outgoing collection");
    }

    #[test]
    fn col10_apply_settings_for_profile_reopens_cleanly_and_returns_notes() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dflt = StudioSettings::default();
        let canvas = Canvas { width: dflt.width, height: dflt.height, fps: dflt.fps };
        let doc = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        let col = Collection::load(canvas, &doc, FIRST_SCENE).unwrap_or_else(|e| panic!("load: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col.engine_ref().is_err() {
            return;
        }
        let mut h = Held { settings: Some(dflt), collection: Some(col), ..Held::default() };
        populate_cached_views(&mut h, dir.path(), false);

        // Changing sample_rate while idle reopens the engine cleanly (no stale restart warning)
        // and updates Held's active profile bindings.
        let prof_b = library::file(dir.path(), Kind::Profiles, "b");
        let s = StudioSettings { sample_rate: 44_100, ..StudioSettings::default() };
        let notes =
            apply_settings_for_profile(ViewCtx::Test, &mut h, dir.path(), s, Some(("b".into(), prof_b.clone())))
                .unwrap_or_else(|e| panic!("apply: {e}"));
        assert_eq!(h.profile_id.as_deref(), Some("b"));
        assert_eq!(active_profile_file(&h, dir.path()), prof_b);
        assert!(notes.is_empty(), "idle profile switch reopens engine cleanly: {notes:?}");
        assert_eq!(h.collection.as_ref().and_then(|c| c.engine_format()), Some((44_100, false)));

        let formatted = format_settings_note(vec!["Heads up.".into()], &["Ctrl+Shift+X".into()]);
        let reply = shelf_action_result("b", formatted);
        assert_eq!(reply["id"], "b");
        assert!(reply["note"].as_str().is_some_and(|n| n.contains("Heads up.") && n.contains("Ctrl+Shift+X")));

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col14_failed_profile_apply_restores_projectors_with_previous_settings() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let initial = StudioSettings {
            width: 1280,
            height: 720,
            fps: 30,
            multiview_layout: "top".into(),
            multiview_labels: true,
            multiview_safe_areas: false,
            projector_on_top: false,
            projector_hide_cursor: false,
            projector_restore: false,
            ..StudioSettings::default()
        };
        let canvas = Canvas { width: initial.width, height: initial.height, fps: initial.fps };
        let doc = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
        let col = Collection::load(canvas, &doc, FIRST_SCENE).unwrap_or_else(|e| panic!("load: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col.engine_ref().is_err() {
            return;
        }
        let prof_a = library::file(dir.path(), Kind::Profiles, "pa");
        let prof_bad = library::file(dir.path(), Kind::Profiles, "pbad");
        let mut h = Held {
            settings: Some(initial.clone()),
            collection: Some(col),
            profile_id: Some("pa".into()),
            profile_file: Some(prof_a.clone()),
            ..Held::default()
        };
        populate_cached_views(&mut h, dir.path(), false);

        // 1. Invalid canvas (width: 0) in incoming profile must roll back before views/projectors rebuild.
        let s_invalid = StudioSettings {
            width: 0,
            height: 1080,
            fps: 60,
            multiview_layout: "left".into(),
            multiview_labels: false,
            multiview_safe_areas: true,
            projector_on_top: true,
            projector_hide_cursor: true,
            projector_restore: true,
            ..initial.clone()
        };
        assert!(
            apply_settings_for_profile(ViewCtx::Test, &mut h, dir.path(), s_invalid, Some(("pbad".into(), prof_bad)))
                .is_err()
        );
        assert_eq!(h.settings.as_ref(), Some(&initial));
        assert_eq!(h.profile_id.as_deref(), Some("pa"));
        assert!(
            !projectors_file(dir.path()).exists(),
            "rolled back projector_restore=false must not write projectors.json"
        );
        assert!(h.preview.is_some());
        assert!(h.program_view.is_some());
        assert!(h.vertical.view.is_some());
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(h.projectors.len(), 3);
            for (_, p, req) in &h.projectors {
                let tp = p.test_proj().expect("test projector");
                assert_eq!(tp.on_top.get(), req.on_top, "on_top must reflect rolled back settings for {}", req.what);
                assert!(!tp.hide_cursor.get(), "hide_cursor must roll back for {}", req.what);
                if req.what == "multiview" {
                    assert!(matches!(tp.look.borrow().layout, ferrocast_studio::MultiviewLayout::Top));
                    assert!(tp.look.borrow().labels);
                    assert!(!tp.look.borrow().safe_areas);
                }
            }
        }

        // 2. Pinned engine failure must also rebuild projectors using rolled back settings.
        let pin = h.collection.as_ref().expect("col").engine_ref().expect("pin");
        let prof_pin = library::file(dir.path(), Kind::Profiles, "ppin");
        let s_pinned = StudioSettings {
            width: 1920,
            height: 1080,
            fps: 60,
            multiview_layout: "right".into(),
            projector_on_top: true,
            projector_hide_cursor: true,
            ..initial.clone()
        };
        assert!(
            apply_settings_for_profile(ViewCtx::Test, &mut h, dir.path(), s_pinned, Some(("ppin".into(), prof_pin)))
                .is_err()
        );
        assert_eq!(h.settings.as_ref(), Some(&initial));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert_eq!(h.projectors.len(), 3);
            for (_, p, req) in &h.projectors {
                let tp = p.test_proj().expect("test projector");
                assert_eq!(tp.on_top.get(), req.on_top);
                assert!(!tp.hide_cursor.get());
                if req.what == "multiview" {
                    assert!(matches!(tp.look.borrow().layout, ferrocast_studio::MultiviewLayout::Top));
                }
            }
        }
        drop(pin);

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col15_swap_studio_mode_on_to_off_clears_program_view() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let scenes_b = dir.path().join("collections").join("b.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", None, "SharedColor");
        let col_b_builder = Collection::new(canvas, "MainB").unwrap_or_else(|e| panic!("{e}"));
        col_b_builder.set_studio_mode(false);
        let doc_b = col_b_builder.save(COLLECTION);
        write_json_sync(&scenes_a, &doc_a);
        write_json_sync(&scenes_b, &doc_b);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap_or_else(|e| panic!("load A: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mut h = Held { settings: Some(StudioSettings::default()), collection: Some(col_a), ..Held::default() };
        populate_cached_views(&mut h, dir.path(), false);
        assert!(h.program_view.is_some(), "Studio Mode ON collection opens program_view");
        let saved_program_at = h.program_at;
        assert!(saved_program_at.is_some());

        // Switching to a collection with Studio Mode OFF must drop program_view while preserving program_at.
        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_a, &scenes_b)
            .unwrap_or_else(|e| panic!("swap A->B: {e}"));
        assert_eq!(h.collection.as_ref().and_then(Collection::preview_scene), None);
        assert!(h.program_view.is_none(), "Studio Mode OFF collection must not keep an orphaned program_view");
        assert_eq!(
            h.program_at.map(|(r, s)| ((r.x, r.y, r.w, r.h, r.vh), s)),
            saved_program_at.map(|(r, s)| ((r.x, r.y, r.w, r.h, r.vh), s))
        );

        // Switching back to a Studio Mode ON collection rebuilds program_view at the preserved rect.
        swap_collection(ViewCtx::Test, &mut h, dir.path(), &scenes_b, &scenes_a)
            .unwrap_or_else(|e| panic!("swap B->A: {e}"));
        assert!(h.program_view.is_some(), "switching back to Studio Mode ON collection rebuilds program_view");

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col16_stop_source_record_applies_waiting_canvas() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let initial = Canvas { width: 1280, height: 720, fps: 30 };
        let doc = make_collection_doc(initial, "MainA", "PreviewA", None, "SharedColor");
        let col = Collection::load(initial, &doc, FIRST_SCENE).unwrap_or_else(|e| panic!("load: {e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col.engine_ref().is_err() {
            return;
        }
        let target = Canvas { width: 1920, height: 1080, fps: 60 };
        let mut h = Held {
            settings: Some(StudioSettings {
                width: target.width,
                height: target.height,
                fps: target.fps,
                ..StudioSettings::default()
            }),
            collection: Some(col),
            ..Held::default()
        };
        let _ = stop_source_record(ViewCtx::Test, &mut h, dir.path(), "SharedColor");
        assert_eq!(h.collection.as_ref().map(Collection::canvas), Some(target));
    }

    #[test]
    fn col17_safe_mode_switch_to_active_collection_reloads_saved() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = library::file(dir.path(), Kind::Collections, "a");
        let saved_doc = make_collection_doc(canvas, "SavedMain", "SavedPreview", None, "SavedColor");
        write_json_sync(&scenes_a, &saved_doc);

        let temp_col = Collection::new(canvas, "SafeTemp").unwrap_or_else(|e| panic!("{e}"));
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && temp_col.engine_ref().is_err() {
            return;
        }
        let mut h = Held {
            settings: Some(StudioSettings::default()),
            collection: Some(temp_col),
            collection_id: Some("a".into()),
            collection_file: Some(scenes_a.clone()),
            collection_deferred: true,
            ..Held::default()
        };

        assert!(
            !shelf_switch_is_noop(&h, Kind::Collections, "a", "a"),
            "Safe Mode switch to active collection id must not short-circuit while deferred"
        );
        swap_collection_to(ViewCtx::Test, &mut h, dir.path(), Some("a"), &scenes_a, &scenes_a, &[])
            .unwrap_or_else(|e| panic!("reload saved active collection: {e}"));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));

        assert!(!h.collection_deferred, "reload must clear collection_deferred");
        let names: Vec<String> = h.collection.as_ref().unwrap().scenes().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"SavedMain".to_string()), "expected SavedMain after reload, got {names:?}");
        assert!(!names.contains(&"SafeTemp".to_string()));
    }

    #[test]
    fn col18_failed_collection_load_keeps_held_ptt_keys() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let canvas = Canvas { width: 1280, height: 720, fps: 30 };
        let scenes_a = dir.path().join("collections").join("a.json");
        let bad_b = dir.path().join("collections").join("bad.json");
        let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", None, "A");
        write_json_sync(&scenes_a, &doc_a);
        let bad_doc = json!({
            "ferrocast": { "program_copy": { "scene": "X", "items": [{ "name": "Main" }] } },
            "sources": [{ "id": "color_source_v3", "name": "Main" }],
            "scene_order": []
        });
        write_json_sync(&bad_b, &bad_doc);

        let col_a = Collection::load(canvas, &doc_a, FIRST_SCENE).unwrap();
        if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
            return;
        }
        let mic_kind = if cfg!(target_os = "macos") {
            "coreaudio_input_capture"
        } else if cfg!(windows) {
            "wasapi_input_capture"
        } else {
            "pulse_input_capture"
        };
        col_a.add_source("MainA", mic_kind, "Mic", &json!({}), false).unwrap();
        col_a.set_muted("Mic", false).unwrap();
        let mut h = Held { settings: Some(StudioSettings::default()), collection: Some(col_a), ..Held::default() };

        let mut pk = crate::studio_keys::PressedKeys::default();
        assert!(pk.down("push_to_talk:Mic", "F8"));

        let res = swap_collection_releasing(ViewCtx::Test, &mut h, dir.path(), Some("bad"), &scenes_a, &bad_b, || {
            pk.release_on_switch()
        });
        assert!(res.is_err(), "corrupt target collection must fail to load");
        assert!(pk.up("push_to_talk:Mic", "F8"), "failed load must not drain held keys or suppress key-up");
        assert!(!h.collection.as_ref().unwrap().audio_of("Mic").unwrap().muted);

        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
    }

    #[test]
    fn col19_stale_shortcut_acts_carry_epoch_for_main_thread_filtering() {
        let pkey = StudioAct::PushKey { name: "Mic".into(), talk: true, down: true, epoch: Some(3) };
        let pitem = StudioAct::PushItem { scene: "Main".into(), id: 1, visible: true, release: false, epoch: Some(4) };
        let pflt = StudioAct::PushFilter {
            name: "Cam".into(),
            filter: "Blur".into(),
            enabled: true,
            release: false,
            epoch: Some(5),
        };
        let paudio = StudioAct::SetAudio {
            name: "Mic".into(),
            change: ferrocast_engine::studio_host::AudioChange {
                muted: Some(false),
                by_shortcut: true,
                shortcut_epoch: Some(6),
                ..Default::default()
            },
        };
        assert_eq!(act_shortcut_epoch(&pkey), Some(3));
        assert_eq!(act_shortcut_epoch(&pitem), Some(4));
        assert_eq!(act_shortcut_epoch(&pflt), Some(5));
        assert_eq!(act_shortcut_epoch(&paudio), Some(6));
        assert_eq!(act_shortcut_epoch(&StudioAct::Scenes), None);
    }

    #[test]
    fn col20_take_outputs_defers_guests_until_collection_teardown() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let sdp = dir.path().join("guest-1.sdp");
        let guest = ferrocast_guest::Guest::test_with_sdp(&sdp).unwrap_or_else(|e| panic!("guest: {e}"));
        assert!(sdp.is_file());
        let col = Collection::new(Canvas { width: 640, height: 360, fps: 30 }, FIRST_SCENE).unwrap();
        let mut h =
            Held { collection: Some(col), guests: vec![("Alex".into(), "Alex".into(), guest)], ..Held::default() };

        let jobs = take_outputs_from_held(&mut h);
        drain_outputs_bounded(jobs, std::time::Duration::from_secs(2));
        assert_eq!(
            h.guests.len(),
            1,
            "guests must remain intact while outputs drain so .sdp stays open until collection teardown"
        );
        assert!(sdp.is_file(), ".sdp must not be unlinked before outgoing collection drops");
    }

    #[test]
    fn col21_swap_collection_off_main_waits_for_transient_holder_and_cleans_guests() {
        let _guard = lock_engine();
        let _reset = ResetAudioFormat;
        apply_audio_format(&StudioSettings::default());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
            let canvas = Canvas { width: 1280, height: 720, fps: 30 };
            let scenes_a = dir.path().join("collections").join("a.json");
            let scenes_b = dir.path().join("collections").join("b.json");
            let bad = dir.path().join("collections").join("bad.json");
            let doc_a = make_collection_doc(canvas, "MainA", "PreviewA", Some("VertA"), "SharedColor");
            let doc_b = make_collection_doc(canvas, "GameB", "PreviewB", Some("VertB"), "SharedColor");
            let bad_doc = json!({
                "ferrocast": { "program_copy": { "scene": "X", "items": [{ "name": "Main" }] } },
                "sources": [{ "id": "color_source_v3", "name": "Main" }],
                "scene_order": []
            });
            write_json_sync(&scenes_a, &doc_a);
            write_json_sync(&scenes_b, &doc_b);
            write_json_sync(&bad, &bad_doc);

            let sdp = dir.path().join("guest-async.sdp");
            let guest = ferrocast_guest::Guest::test_with_sdp(&sdp).unwrap_or_else(|e| panic!("guest: {e}"));

            // Spawn a dedicated mock main thread owning `!Send` Held (just like the app UI thread).
            type MainJob = Box<dyn FnOnce(ViewCtx<'_>, &mut Held) + Send>;
            let (ui_tx, ui_rx) = std::sync::mpsc::channel::<MainJob>();
            let (init_tx, init_rx) = std::sync::mpsc::channel::<Option<ferrocast_studio::stage::EngineRef>>();
            let dir_ui = dir.path().to_path_buf();
            let scenes_a_ui = scenes_a.clone();
            let ui_handle = std::thread::spawn(move || {
                let Ok(col_a) = Collection::load(canvas, &doc_a, FIRST_SCENE) else {
                    let _ = init_tx.send(None);
                    return;
                };
                if cfg!(not(any(target_os = "macos", target_os = "windows"))) && col_a.engine_ref().is_err() {
                    let _ = init_tx.send(None);
                    return;
                }
                let mut held = Held {
                    settings: Some(StudioSettings::default()),
                    collection: Some(col_a),
                    collection_id: Some("a".into()),
                    collection_file: Some(scenes_a_ui),
                    guests: vec![("Guest".into(), "Guest".into(), guest)],
                    ..Held::default()
                };
                populate_cached_views(&mut held, &dir_ui, false);
                let transient = held.collection.as_ref().unwrap().engine_ref().ok();
                let _ = init_tx.send(transient);
                while let Ok(job) = ui_rx.recv() {
                    job(ViewCtx::Test, &mut held);
                }
                drop(held);
            });
            let Some(transient) = init_rx.recv().unwrap() else {
                drop(ui_tx);
                let _ = ui_handle.join();
                return;
            };
            let weak_a = transient.downgrade();

            struct UiBridge(std::sync::mpsc::Sender<MainJob>);
            impl HeldBridge for UiBridge {
                async fn call_main<F, R>(&self, f: F) -> Result<R, String>
                where
                    F: FnOnce(ViewCtx<'_>, &mut Held) -> R + Send + 'static,
                    R: Send + 'static,
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    self.0
                        .send(Box::new(move |ctx, held| {
                            let _ = tx.send(f(ctx, held));
                        }))
                        .map_err(|_| "mock UI thread exited".to_string())?;
                    rx.await.map_err(|_| "mock UI task dropped".to_string())
                }
            }

            // Background holder releases its EngineRef only once Phase 2 clears LIVE_ENGINE, ensuring
            // Phase 3 finish_off_main genuinely waits for transient readers before opening B's engine.
            let holder = std::thread::spawn(move || {
                while ferrocast_studio::stage::levels_now(&["SharedColor".into()], false).is_some() {
                    std::thread::yield_now();
                }
                drop(transient);
            });

            let bridge = UiBridge(ui_tx.clone());
            swap_collection_off_main(
                &bridge,
                dir.path().to_path_buf(),
                Some("b".into()),
                scenes_a.clone(),
                scenes_b.clone(),
            )
            .await
            .unwrap_or_else(|e| panic!("async swap A->B: {e}"));
            holder.join().unwrap();
            assert!(!weak_a.is_alive(), "outgoing engine must be fully dropped before swap completes");
            assert!(!sdp.exists(), "guest .sdp must be unlinked off-main after outgoing collection tears down");

            let (eng_b_id, col_b_ok) = bridge
                .call_main(|_, held| {
                    let id = held.collection_id.clone();
                    let ok = held.collection.as_ref().is_some_and(Collection::has_engine) && held.preview.is_some();
                    (id, ok)
                })
                .await
                .unwrap();
            assert_eq!(eng_b_id.as_deref(), Some("b"));
            assert!(col_b_ok);

            // Failed Phase 1 load leaves collection B and views intact.
            assert!(
                swap_collection_off_main(&bridge, dir.path().to_path_buf(), Some("bad".into()), scenes_b, bad)
                    .await
                    .is_err()
            );
            let id_after_fail = bridge.call_main(|_, held| held.collection_id.clone()).await.unwrap();
            assert_eq!(id_after_fail.as_deref(), Some("b"));

            drop(bridge);
            drop(ui_tx);
            ui_handle.join().unwrap();
            ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        });
    }

    #[test]
    fn col22_obsws_snapshot_includes_collections_and_profiles_with_held_current() {
        let lib = library::Library {
            collections: library::Shelf {
                current: "a".into(),
                list: vec![
                    library::Entry { id: "a".into(), name: "Alpha".into() },
                    library::Entry { id: "b".into(), name: "Beta".into() },
                ],
            },
            profiles: library::Shelf {
                current: "p1".into(),
                list: vec![
                    library::Entry { id: "p1".into(), name: "MainProfile".into() },
                    library::Entry { id: "p2".into(), name: "AltProfile".into() },
                ],
            },
        };

        let base = json!({ "program": "Main", "preview": null });
        let out = obsws_snapshot_json(base, &lib, Some("b"), Some("p2"));
        assert_eq!(out["collections"]["current"], json!("b"));
        assert_eq!(out["collections"]["list"].as_array().unwrap().len(), 2);
        assert_eq!(out["profiles"]["current"], json!("p2"));
        assert_eq!(out["profiles"]["list"].as_array().unwrap().len(), 2);
    }
}
