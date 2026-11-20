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
    STOPPED, ShelfAct, StreamCap, StudioAct, StudioHost, StudioSettings, StudioStatus,
};
// The Studio's scenes kept by Ferrocast's own engine (ENGINE.md step 3): no libobs.
use ferrocast_studio::stage::Stage as Collection;
use ferrocast_studio::stage::StageOutput as Streaming;
use ferrocast_studio::stage::StageOutput as Recording;
use ferrocast_studio::stage::StageView as Preview;
/// A projector window (Ferrocore draws them on a Mac and on Windows; none elsewhere yet).
#[cfg(any(target_os = "macos", target_os = "windows"))]
type ProjectorWin = ferrocast_studio::stage::StageProjector;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
type ProjectorWin = ();
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
    projectors: Vec<(u64, ProjectorWin)>,
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
    /// True in Safe Mode when a temporary empty collection is running until the user confirms
    /// loading the saved collection; guards against overwriting the saved file on disk.
    collection_deferred: bool,
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
    // Drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let mut p = c.vertical_preview_drawn(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
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
/// follow by themselves).
fn refresh_projectors(h: &Held) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(c) = &h.collection {
        let look = multiview_look(&h.settings.clone().unwrap_or_default());
        for (_, p) in &h.projectors {
            p.update(c, look);
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

/// The canvas to `canvas` (its size or frame rate), the previews made again for it.
fn apply_canvas(app: &tauri::AppHandle, h: &mut Held, canvas: Canvas) -> Result<(), String> {
    // Refused while live or recording: nothing is saved then. The previews
    // must let go of the old engine first: each holds it, so with one still
    // alive the change was refused as "streaming or recording" when nothing
    // was. They're dropped here, and made again below either way.
    let had = (h.preview.is_some(), h.program_view.is_some(), h.vertical.view.is_some());
    h.preview = None;
    h.program_view = None;
    h.vertical.view = None;
    h.projectors.clear();
    // They send the old engine's picture: stopped (turned on again by hand).
    h.ndi = None;
    h.virtualcam = None;
    let c = h.collection.as_mut().ok_or(STOPPED)?;
    let changed = c.set_canvas(canvas);
    // Made again (at the new size when it changed).
    if had.0 {
        let _ = make_preview(app, h);
    }
    if had.1 {
        let _ = make_program_view(app, h);
    }
    if had.2 {
        let _ = make_vertical_view(app, h);
    }
    changed.map_err(|e| e.to_string())
}

/// A frame rate changed while live waited for the stream to end: the canvas takes it once
/// nothing that draws from it runs (called when a stream, recording or replay buffer stops, and
/// before a stream starts). The virtual camera and NDI would be stopped by it, so while one is
/// on it waits longer (the next stream still goes out at the chosen rate, see `stream_start`).
fn apply_waiting_canvas(app: &tauri::AppHandle, h: &mut Held) {
    let Some(s) = h.settings.clone() else { return };
    let want = Canvas { width: s.width, height: s.height, fps: s.fps };
    if h.collection.as_ref().is_none_or(|c| c.canvas() == want) {
        return;
    }
    let busy = h.stream.as_ref().is_some_and(|st| st.stats().active)
        || h.record.as_ref().is_some_and(|r| r.0.stats().active)
        || h.replay.as_ref().is_some_and(|r| r.active())
        || h.ndi.is_some()
        || h.virtualcam.is_some();
    if busy {
        return;
    }
    if let Err(e) = apply_canvas(app, h, want) {
        tracing::warn!(error = %e, "the frame rate chosen while live couldn't be applied after the stream");
    }
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
}

/// What streaming and recording at once ask of the Mac's video encoder (its engines × about one
/// 4K60 each), from the settings: the stream (at its output size), and the recording unless
/// "Same as stream" shares the stream's encode (`can_share`, whose reason is given when it
/// can't). `level`: ok under 90%, limit up to 100% (4K30 × 2 held its 30, 19 Nov), over past
/// it (frames drop: 4K60 × 2 made 31 of 60).
/// The lighter stream's size: 720 high at the canvas's shape (even sizes); None: the canvas
/// itself, when it's no taller.
fn light_size(width: u32, height: u32) -> Option<(u32, u32)> {
    (height > 720).then(|| {
        let w = (u64::from(width) * 720 / u64::from(height.max(1))) as u32;
        (w & !1, 720)
    })
}

fn encode_budget(s: &StudioSettings, cap: Option<StreamCap>, engines: Option<u32>, also: Also) -> Value {
    let mpx = |w: u32, h: u32| f64::from(w) * f64::from(h) * f64::from(s.fps) / 1e6;
    let canvas = mpx(s.width, s.height);
    // Only the video encoder's own encodes count: x264 (software, Lossless) runs on the CPU.
    let hardware = |id: &str| !id.to_ascii_lowercase().contains("x264");
    let mut stream = stream_settings(s, cap);
    // Twitch Enhanced Broadcasting's qualities: Twitch's plan sets them when the stream starts;
    // about 720p and 480p beside the main one (projects-60).
    if also.enhanced {
        stream.video_tracks = vec![(1280, 720, 3000), (852, 480, 1500)];
    }
    let mut stream_mpx = stream.encode.scale.map_or(canvas, |(w, h)| mpx(w, h));
    stream_mpx += stream.video_tracks.iter().map(|&(w, h, _)| mpx(w, h)).sum::<f64>();
    if also.light {
        let (w, h) = light_size(s.width, s.height).unwrap_or((s.width, s.height));
        stream_mpx += mpx(w, h);
    }
    if !hardware(&stream.encode.video_encoder) {
        stream_mpx = 0.0;
    }
    let plan = ferrocast_studio::RecordPlan {
        tracks: s.record_tracks.clone(),
        encode: record_encode_capped(s, cap),
        ..Default::default()
    };
    let shares = ferrocast_studio::stage::can_share(&stream, &plan, s.record_quality == "stream", s.fps, s.fps)
        .map_err(|why| {
            if stream.vod_track.is_some() {
                format!("{why} (turn the VOD track off when you're not on Twitch)")
            } else {
                why
            }
        });
    let record_hw = hardware(&plan.encode.video_encoder);
    let mut record_mpx = if shares.is_ok() || !record_hw { 0.0 } else { canvas };
    // The replay buffer is always an encode of its own (the recording's settings).
    if also.replay && record_hw {
        record_mpx += canvas;
    }
    let budget = engines.map(|n| f64::from(n) * ferrocast_studio::system::MPX_PER_ENGINE);
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

/// Effective encode settings for the main stream after clamping to `cap` (if any) and `live_pct`.
fn effective_stream_encode(s: &StudioSettings, cap: Option<StreamCap>, live_pct: Option<u8>) -> EncodeSettings {
    let mut e = encode_of(s);
    if let Some(cap) = cap {
        e.video_kbps = e.video_kbps.min(cap.max_kbps);
        if cap.h264_only {
            e.video_encoder = h264_encoder_for(&e.video_encoder);
        }
        if cap.keyframe_secs > 0 {
            e.keyframe_s = cap.keyframe_secs;
        }
        // Strict CBR with 1x VBV (FFmpeg) / DataRateLimits (VideoToolbox) prevents platform peaks.
        e.vbr = false;
        e.strict_cbr = true;
    }
    e.video_kbps = at_pct(e.video_kbps, live_pct);
    e
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
    ferrocast_store::durable::remove_with_backup(&library::file(data, kind, id));
}

fn load_collection_doc(canvas: Canvas, file: &std::path::Path) -> Result<Collection, ferrocast_studio::StudioError> {
    let load_one = |path: &std::path::Path| -> Option<Result<Collection, Option<ferrocast_studio::StudioError>>> {
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
        Some(Collection::load(canvas, &doc, FIRST_SCENE).map_err(Some))
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
                return Collection::new(canvas, FIRST_SCENE);
            }
        }
    }

    let bak = ferrocast_store::durable::backup_path(file);
    if let Some(Ok(c)) = load_one(&bak) {
        tracing::warn!(file = %file.display(), "recovered scene collection from .bak");
        return Ok(c);
    }
    Collection::new(canvas, FIRST_SCENE)
}

/// Saves the scenes in use to `from` and puts `to`'s in their place (a missing file is a new
/// set with one empty scene). If `to` won't load, the old ones come back.
fn swap_collection(h: &mut Held, from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    let Some(canvas) = h.collection.as_ref().map(Collection::canvas) else {
        // Not running: the next Start loads it.
        return Ok(());
    };
    // libobs keeps one collection's sources at a time: the old ones go first, with the undo
    // steps that hold them and the projectors that show them.
    // The vertical preview and stream use the old collection's canvas: they go before it does.
    h.vertical.view = None;
    h.vstream = None;
    h.captions = None;
    // NDI and the virtual camera send the old collection's picture: they stop with it.
    h.ndi = None;
    h.virtualcam = None;
    // Put the following item back first, so neither collection is saved slid sideways.
    if let (Some(f), Some(c)) = (h.follow.take(), &h.collection) {
        c.stop_follow(&f);
    }
    let old = h.collection.take().map(|c| {
        save_confirmed(h.collection_deferred, &c, from);
        c.save(COLLECTION)
    });
    let was_deferred = h.collection_deferred;
    h.history.clear();
    h.selected = None;
    h.frames = None;
    // The old scenes are going: the preview lets go of the one it shows (a scene of the same
    // name in the new set is another source).
    if let Some(p) = &h.preview {
        p.show_scene(None);
    }
    // Only a missing file (or corrupt JSON without a working `.bak`) starts fresh; valid JSON that
    // fails collection loading restores the old collection.
    let made = load_collection_doc(canvas, to);
    match made {
        Ok(c) => {
            h.collection_deferred = false;
            save_confirmed(false, &c, to);
            c.set_true_peak(h.settings.as_ref().is_some_and(|s| s.true_peak));
            c.set_low_latency(h.settings.as_ref().is_some_and(|s| s.low_latency_audio));
            h.collection = Some(c);
            refresh_projectors(h);
            sync_views(h);
            Ok(())
        }
        Err(e) => {
            h.collection_deferred = was_deferred;
            if let Some(old) = old {
                h.collection = Collection::load(canvas, &old, FIRST_SCENE).ok();
            }
            sync_views(h);
            Err(format!("That scene collection didn't load: {e}"))
        }
    }
}

/// The saved settings, falling back to `.bak` when unreadable, or the defaults.
fn load_settings(data: &std::path::Path) -> StudioSettings {
    let s =
        ferrocast_store::durable::load_json_with_backup::<StudioSettings>(&settings_file(data), |s| s.check().is_ok())
            .map(|l| l.value)
            .unwrap_or_default();
    apply_audio_format(&s);
    s
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
    push_opt_drain(&mut jobs, &mut h.record);
    push_vec_drain(&mut jobs, &mut h.source_records);
    push_opt_drain(&mut jobs, &mut h.replay);
    push_opt_drain(&mut jobs, &mut h.virtualcam);
    push_opt_drain(&mut jobs, &mut h.ndi);
    push_vec_drain(&mut jobs, &mut h.guests);
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

fn clear_non_output_held(h: &mut Held) {
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

fn stats_of(s: &Streaming) -> Value {
    let mut v = serde_json::to_value(s.stats()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
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
            "top_layer": c.top_layer(),
        }))),
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
        A::SourcePicture { name, width, filters } => Ok(match c.source_picture_pixels(&name, width, filters)? {
            Some((px, w, h, (sw, sh))) => {
                json!({ "image": ferrocast_studio::png_data_url(&px, w, h, &name)?, "width": sw, "height": sh })
            }
            None => json!({ "image": null }),
        }),
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
        A::Items { scene } => v(to_json!(c.items(&scene)?)),
        A::AddSource { scene, source, fit } => {
            // `reused`: a camera or capture card already used here, shown again (never opened twice).
            let (item, reused) = c.add_source_or_reuse(&scene, &source.kind, &source.name, &source.settings, fit)?;
            let settings = c.settings(&item.name)?;
            v(to_json!(json!({ "item": item, "settings": settings, "reused": reused })))
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
        A::Look { names } => v(to_json!(c.look(&names)?)),
        A::AudioCare => v(Ok(c.audio_care())),
        A::SetAudioCare { care } => done(c.set_audio_care(&care)),
        A::Privacy => v(Ok(c.privacy())),
        A::SetPrivacy { privacy } => done(c.set_privacy(&privacy)),
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
    // A multiview's preview box and scene list too.
    refresh_projectors(h);
}

/// Makes Studio Mode's picture of what's on air at its last place (main thread).
fn make_program_view(app: &AppHandle, h: &mut Held) -> Result<(), String> {
    let Some((rect, scale)) = h.program_at else { return Ok(()) };
    let window = app.get_webview_window("main").ok_or("The Ferrocast window isn't open.")?;
    let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
    // What's on air, drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let c = h.collection.as_ref().ok_or(STOPPED)?;
    let p = c.preview_drawn(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
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
    let handle = raw_window_handle::HasWindowHandle::window_handle(&window).map_err(|e| e.to_string())?;
    // Drawn by Ferrocast's own engine (on a Mac so far; elsewhere it says so).
    let c = h.collection.as_ref().ok_or(STOPPED)?;
    let mut p = c.preview_drawn(handle.as_raw(), rect, scale).map_err(|e| e.to_string())?;
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

#[derive(Debug, Clone)]
pub struct DesktopStudio {
    app: AppHandle,
    data: PathBuf,
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
        let me = DesktopStudio { app, data };
        let watcher = DesktopStudio { app: me.app.clone(), data: me.data.clone() };
        tauri::async_runtime::spawn(async move { watcher.session_loop().await });
        let follower = DesktopStudio { app: me.app.clone(), data: me.data.clone() };
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
                    for s in [h.stream.as_mut(), h.vstream.as_mut()].into_iter().flatten().filter(|s| s.drawn()) {
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
                let out = HELD.with(|h| f(&mut h.borrow_mut()));
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

    /// Puts collection or profile `id` in use. Refused while any output is active (stream,
    /// recording, source record, replay, virtual camera, NDI) so switching never orphans or
    /// blacks out live outputs (M9-01).
    async fn switch(&self, kind: Kind, id: &str) -> Result<(), String> {
        let data = self.data.clone();
        let mut lib = library::load(&data);
        if lib.shelf(kind).get(id).is_none() {
            return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
        }
        if lib.shelf(kind).current == id {
            return Ok(());
        }
        let busy = self
            .on_main(|h| {
                h.stream.is_some()
                    || h.vstream.is_some()
                    || h.lstream.is_some()
                    || h.record.is_some()
                    || !h.source_records.is_empty()
                    || h.replay.is_some()
                    || h.virtualcam.is_some()
                    || h.ndi.is_some()
            })
            .await?;
        if busy {
            let noun = match kind {
                Kind::Collections => "scene collections",
                Kind::Profiles => "profiles",
            };
            return Err(format!("Stop streaming and recording before changing {noun}."));
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

#[async_trait::async_trait]
impl StudioHost for DesktopStudio {
    async fn status(&self) -> StudioStatus {
        self.on_main(|h| Self::status_of(h)).await.unwrap_or_default()
    }

    async fn start(&self) -> Result<StudioStatus, String> {
        self.start_with_mode(true).await
    }

    async fn start_with_mode(&self, load_collection: bool) -> Result<StudioStatus, String> {
        #[cfg(windows)]
        fetch_matte(self.data.clone());
        let config = self.data.join("studio");
        let data = self.data.clone();
        self.on_main(move |h| {
            // Read here, on the main thread, so a switch can't slip in between (CodeRabbit).
            let file = scenes_file(&data);
            if h.collection.is_some() {
                return Ok(Self::status_of(h));
            }
            // Ferrocast's own engine: no libobs to load.
            let s = h.settings.get_or_insert_with(|| load_settings(&data)).clone();
            let _ = &config;
            h.renderer = "ferrocast";
            let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
            let collection = if load_collection {
                h.collection_deferred = false;
                load_collection_doc(canvas, &file).map_err(|e| e.to_string())?
            } else {
                // Safe Mode: start cleanly without loading the saved collection until confirmed.
                h.collection_deferred = true;
                Collection::new(canvas, FIRST_SCENE).map_err(|e| e.to_string())?
            };
            if let Some((name, id)) = &s.monitoring
                && let Err(e) = collection.set_monitoring_device(name, id)
            {
                tracing::warn!(error = %e, "the listening device is gone; using the system's");
            }
            save_confirmed(h.collection_deferred, &collection, &file);
            collection.set_true_peak(h.settings.as_ref().is_some_and(|s| s.true_peak));
            collection.set_low_latency(h.settings.as_ref().is_some_and(|s| s.low_latency_audio));
            h.collection = Some(collection);
            write_session(session_now(h).as_ref());
            // The first Start after a crash says what was on, for the engine to put back.
            let recovered = CRASHED.lock().ok().and_then(|mut c| c.take());
            Ok(StudioStatus { recovered, ..Self::status_of(h) })
        })
        .await?
    }

    async fn confirm_collection(&self) -> Result<StudioStatus, String> {
        let data = self.data.clone();
        self.on_main(move |h| {
            let file = scenes_file(&data);
            if h.collection.is_some() {
                swap_collection(h, &file, &file)?;
                write_session(session_now(h).as_ref());
            }
            Ok(Self::status_of(h))
        })
        .await?
    }

    async fn stop(&self) -> Result<StudioStatus, String> {
        // Extract active outputs on main thread without blocking UI, then drain them in parallel
        // off the main thread BEFORE saving and dropping the scene collection.
        let jobs = self.on_main(take_outputs_from_held).await?;
        if !jobs.is_empty() {
            let _ = tokio::task::spawn_blocking(move || drain_outputs_bounded(jobs, QUIT_DRAIN_TIMEOUT)).await;
        }
        let data = self.data.clone();
        let status = self
            .on_main(move |h| {
                let file = scenes_file(&data);
                let deferred = h.collection_deferred;
                clear_non_output_held(h);
                if let Some(c) = h.collection.take() {
                    save_confirmed(deferred, &c, &file);
                }
                h.collection_deferred = false;
                write_session(None);
                Self::status_of(h)
            })
            .await?;
        // The scenes' save was queued: wait for it here, off the main thread, so Stop still means
        // "saved" without the UI waiting on a locked file.
        let landed =
            tokio::task::spawn_blocking(|| ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10)))
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
        ferrocast_studio::kinds::defaults(&kind)
            .ok_or_else(|| format!("Ferrocast's own engine doesn't run \"{kind}\" yet."))
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
        // 1) reuses an already-open stage source (zero hardware contention / camera LED flash);
        // 2) serves 15s cached JPEG stills and deduplicates concurrent asks;
        // 3) cancels queued/in-flight captures via Drop guard when the dialog closes.
        if matches!(&act, StudioAct::ScreenStill { .. } | StudioAct::SourceStill { .. }) {
            use std::collections::HashMap;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex, OnceLock};
            use std::time::{Duration, Instant};
            use tokio::sync::{Notify, Semaphore};

            struct CancelOnDrop(Arc<AtomicBool>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }

            #[derive(Clone, Hash, PartialEq, Eq)]
            enum StillTarget {
                Display(u32, u32),
                Window(u32, u32),
                Camera(String, u32),
            }

            type Kept = HashMap<StillTarget, (Instant, String)>;
            type FlightMap = HashMap<StillTarget, Arc<(Notify, Mutex<Option<Option<String>>>)>>;
            static TURNS: OnceLock<Semaphore> = OnceLock::new();
            static KEPT: Mutex<Option<Kept>> = Mutex::new(None);
            static IN_FLIGHT: Mutex<Option<FlightMap>> = Mutex::new(None);
            const KEEP: Duration = Duration::from_secs(15);
            const MAX_KEPT: usize = 128;

            let (target_kind, key_name, val_json, target, width) = match &act {
                StudioAct::ScreenStill { id, window, width } => {
                    let w = (*width).clamp(64, 320);
                    let tk = if *window { "window_capture" } else { "screen_capture" }.to_string();
                    let kn = if *window { "window" } else { "display" }.to_string();
                    let tgt = if *window { StillTarget::Window(*id, w) } else { StillTarget::Display(*id, w) };
                    (tk, kn, json!(*id), tgt, w)
                }
                StudioAct::SourceStill { kind, key, value, width } => {
                    let w = (*width).clamp(64, 320);
                    let tgt = if matches!(key.as_str(), "window") || kind.contains("window") {
                        let Some(id) = value.as_u64().and_then(|n| u32::try_from(n).ok()) else {
                            return Ok(json!({ "image": null }));
                        };
                        StillTarget::Window(id, w)
                    } else if matches!(key.as_str(), "display" | "monitor_id")
                        || kind.contains("screen")
                        || kind.contains("monitor")
                        || kind == "xshm_input"
                    {
                        let Some(id) = value.as_u64().and_then(|n| u32::try_from(n).ok()) else {
                            return Ok(json!({ "image": null }));
                        };
                        StillTarget::Display(id, w)
                    } else {
                        let Some(id) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                            return Ok(json!({ "image": null }));
                        };
                        StillTarget::Camera(id.to_string(), w)
                    };
                    (kind.clone(), key.clone(), value.clone(), tgt, w)
                }
                _ => unreachable!(),
            };

            let read_kept = |k: &StillTarget| {
                KEPT.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert_default()
                    .get(k)
                    .filter(|(at, _)| at.elapsed() < KEEP)
                    .map(|(_, img)| img.clone())
            };
            if let Some(image) = read_kept(&target) {
                return Ok(json!({ "image": image }));
            }

            // Fast path: reuse an already-open stage source on the GPU (zero hardware contend).
            let reuse_job = self
                .on_main(move |h| {
                    h.collection
                        .as_ref()
                        .and_then(|c| c.matching_device_picture_job(&target_kind, &key_name, &val_json, width))
                })
                .await
                .ok()
                .flatten();
            if let Some(job) = reuse_job {
                let image = tokio::task::spawn_blocking(move || {
                    job().and_then(|(px, pw, ph)| ferrocast_studio::jpeg_data_url(&px, pw, ph, true, "picker").ok())
                })
                .await
                .map_err(|e| e.to_string())?;
                if let Some(img) = &image {
                    let mut all = KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let map = all.get_or_insert_default();
                    map.retain(|_, (at, _)| at.elapsed() < KEEP);
                    if map.len() >= MAX_KEPT {
                        map.clear();
                    }
                    map.insert(target, (Instant::now(), img.clone()));
                    return Ok(json!({ "image": image }));
                }
            }

            // Single-flight deduplication + early cancellation if caller disconnects.
            let (flight, is_leader) = {
                let mut map_guard = IN_FLIGHT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let map = map_guard.get_or_insert_default();
                if let Some(existing) = map.get(&target) {
                    (existing.clone(), false)
                } else {
                    let created = Arc::new((Notify::new(), Mutex::new(None)));
                    map.insert(target.clone(), created.clone());
                    (created, true)
                }
            };
            if !is_leader {
                let notified = flight.0.notified();
                let cached_before = flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                if let Some(res) = cached_before {
                    return Ok(json!({ "image": res }));
                }
                notified.await;
                let res = flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().flatten();
                return Ok(json!({ "image": res }));
            }

            struct FlightCleanup {
                target: StillTarget,
                flight: Arc<(Notify, Mutex<Option<Option<String>>>)>,
            }
            impl Drop for FlightCleanup {
                fn drop(&mut self) {
                    let mut done = self.flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if done.is_none() {
                        *done = Some(None);
                    }
                    drop(done);
                    if let Ok(mut map) = IN_FLIGHT.lock()
                        && let Some(m) = map.as_mut()
                    {
                        m.remove(&self.target);
                    }
                    self.flight.0.notify_waiters();
                }
            }
            let _flight_cleanup = FlightCleanup { target: target.clone(), flight: flight.clone() };
            let cancel_flag = Arc::new(AtomicBool::new(false));
            let _cancel_guard = CancelOnDrop(cancel_flag.clone());

            let _turn = TURNS.get_or_init(|| Semaphore::new(2)).acquire().await.map_err(|e| e.to_string())?;
            if let Some(image) = read_kept(&target) {
                *flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Some(image.clone()));
                return Ok(json!({ "image": image }));
            }

            let capture_target = target.clone();
            let flag = cancel_flag.clone();
            let image = tokio::task::spawn_blocking(move || {
                let pic = match capture_target {
                    StillTarget::Display(id, w) => {
                        ferrocore::screen_still_cancel(ferrocore::screen::Target::Display(id), w, &flag).ok()
                    }
                    StillTarget::Window(id, w) => {
                        ferrocore::screen_still_cancel(ferrocore::screen::Target::Window(id), w, &flag).ok()
                    }
                    StillTarget::Camera(id, w) => ferrocore::camera_still_cancel(&id, w, &flag).ok(),
                };
                pic.and_then(|p| {
                    ferrocast_studio::jpeg_data_url(&p.rgba, p.width, p.height, p.premultiplied, "picker").ok()
                })
            })
            .await
            .map_err(|e| e.to_string())?;

            if let Some(img) = &image {
                let mut all = KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let map = all.get_or_insert_default();
                map.retain(|_, (at, _)| at.elapsed() < KEEP);
                if map.len() >= MAX_KEPT {
                    map.clear();
                }
                map.insert(target, (Instant::now(), img.clone()));
            }
            *flight.1.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(image.clone());
            return Ok(json!({ "image": image }));
        }
        // How a source is doing: its settings read on the main thread, a Browser source's
        // address tried off it (the app's web client, its time limit).
        if let StudioAct::SourceStatus { name } = &act {
            let n = name.clone();
            let (kind, settings) = self
                .on_main(move |h| -> Result<(String, Value), String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    Ok((c.kind(&n).map_err(|e| e.to_string())?, c.settings(&n).map_err(|e| e.to_string())?))
                })
                .await??;
            // Trimmed, as the engine keeps them (a Web text's reads are noted under these).
            let text = |k: &str| settings.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
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
                        save_confirmed(h.collection_deferred, c, &scenes_file(&data));
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
        if let StudioAct::SourcePicture { name, width, filters } = &act {
            let (n, w, f) = (name.clone(), *width, *filters);
            // Only what's needed is read on the main thread; the drawing, the read back from the
            // graphics card and the PNG happen off it (they held the window up each time).
            let job = self
                .on_main(move |h| -> Result<_, String> {
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    c.source_picture_job(&n, w, f).map_err(|e| e.to_string())
                })
                .await??;
            let Some(job) = job else { return Ok(json!({ "image": null })) };
            let name = name.clone();
            let got = tokio::task::spawn_blocking(move || {
                job().map(|(px, pw, ph, size)| ferrocast_studio::png_data_url(&px, pw, ph, &name).map(|i| (i, size)))
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
            let (path, px, size) = self
                .on_main(move |h| -> Result<_, String> {
                    // Next to the recordings, as OBS does.
                    let folder = record_folder(h.settings.get_or_insert_with(|| load_settings(&data)));
                    let c = h.collection.as_ref().ok_or(STOPPED)?;
                    let (px, size) = match &shot {
                        StudioAct::Screenshot { name } => c.screenshot_pixels(name),
                        _ => c.screenshot_program_pixels(),
                    }
                    .map_err(|e| e.to_string())?;
                    std::fs::create_dir_all(&folder)
                        .map_err(|e| format!("The screenshot folder couldn't be made: {e}"))?;
                    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
                    // Two in the same second get their own files: the name is taken here (the
                    // file made empty), so a second shot before this one is written can't pick it.
                    let mut n = 1;
                    loop {
                        let path = if n == 1 {
                            folder.join(format!("Screenshot {stamp}.png"))
                        } else {
                            folder.join(format!("Screenshot {stamp} ({n}).png"))
                        };
                        match std::fs::File::create_new(&path) {
                            Ok(_) => return Ok((path, px, size)),
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                            Err(e) => return Err(format!("The screenshot couldn't be saved: {e}")),
                        }
                    }
                })
                .await??;
            let at = path.clone();
            let written = tokio::task::spawn_blocking(move || ferrocast_studio::stage::write_png(&at, &px, size))
                .await
                .map_err(|e| e.to_string())?;
            if let Err(e) = written {
                let _ = std::fs::remove_file(&path);
                return Err(e.to_string());
            }
            return Ok(json!({ "file": path.display().to_string() }));
        }
        let data = self.data.clone();
        let app = self.app.clone();
        self.on_main(move |h| {
            let file = scenes_file(&data);
            // The Studio off: its scenes from the saved file, so the show, BRB and the scene
            // switcher can still be set up (owner, 30 Oct: "it's not showing my screens").
            if act == StudioAct::Scenes && h.collection.is_none() {
                let saved = ferrocast_store::durable::load_json_with_backup::<Value>(&file, |_| true).map(|l| l.value);
                return Ok(saved_scenes(saved.as_ref()));
            }
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
                save_confirmed(h.collection_deferred, c, &file);
                refresh_projectors(h);
            }
            out
        })
        .await?
    }

    async fn settings(&self) -> StudioSettings {
        let data = self.data.clone();
        let mut s = self
            .on_main(move |h| h.settings.get_or_insert_with(|| load_settings(&data)).clone())
            .await
            .unwrap_or_default();
        s.record_folder_default = record_folder(&StudioSettings::default()).display().to_string();
        s
    }

    async fn set_settings(&self, s: StudioSettings) -> Result<Option<String>, String> {
        let data = self.data.clone();
        let app = self.app.clone();
        let keys = s.hotkeys.clone();
        let note = self
            .on_main(move |h| {
                let mut notes: Vec<String> = Vec::new();
                // The sound rate and broadcast timing are read when the engine opens (a new canvas opens
                // it again just below): set first, so that reopening takes them.
                apply_audio_format(&s);
                // Streaming now: what Optimize changes (frame rate, size, encoder) goes to the
                // stream on air at once (owner, 15 Nov: "I have to end and stream again").
                let live = h.stream.as_ref().is_some_and(|st| st.stats().active);
                if let Some(c) = h.collection.as_mut() {
                    let canvas = Canvas { width: s.width, height: s.height, fps: s.fps };
                    let now = c.canvas();
                    // Only the frame rate, while live: the stream takes it below; the canvas
                    // follows when the stream ends (`apply_waiting_canvas`).
                    let rate_only = live && (canvas.width, canvas.height) == (now.width, now.height);
                    // Something draws from the picture (a recording, the replay buffer, NDI, the
                    // virtual camera, a stream at another size): the new size waits for it to
                    // stop, and the rest of the settings are saved now. Before, the virtual
                    // camera and NDI were turned off first and then the change was refused, and
                    // the whole save with it (peer, 18 Nov).
                    let busy = live
                        || h.record.as_ref().is_some_and(|r| r.0.stats().active)
                        || h.replay.as_ref().is_some_and(|r| r.active())
                        || h.ndi.is_some()
                        || h.virtualcam.is_some();
                    if canvas != now && !rate_only {
                        if busy {
                            notes.push(
                                "The new size and frame rate apply once streaming, recording, replay, NDI and the virtual camera are off."
                                    .into(),
                            );
                        } else {
                            apply_canvas(&app, h, canvas)?;
                        }
                    } else if rate_only && canvas.fps > now.fps {
                        notes.push(format!("The stream goes up to {} fps once it ends.", canvas.fps));
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
                let was = h.settings.clone();
                let was_encode = was.as_ref().map(|o| effective_stream_encode(o, h.stream_cap, h.live_pct));
                let encode = effective_stream_encode(&s, h.stream_cap, h.live_pct);
                // What changed besides the bitrate: the encoder, its options, the size, the rate.
                let shape_changed = was.as_ref().is_some_and(|o| {
                    was_encode.as_ref().is_some_and(|a| shape_differs(a, &encode)) || o.fps != s.fps
                });
                let kbps = encode.video_kbps;
                let reshaped = match h.stream.as_ref() {
                    Some(st) if live && shape_changed => {
                        match st.reshape(&encode, s.fps) {
                            Ok(()) => {
                                // What it runs with now: a recording "Same as stream" started
                                // next compares with this (CodeRabbit: 1080p reshaped to 720p
                                // was still taken as unscaled).
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
                    }
                    _ => false,
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
                }
                save_json(&settings_file(&data), &serde_json::to_value(&s).unwrap_or(Value::Null));
                apply_audio_format(&s);
                h.settings = Some(s);
                // Open multiviews take a new layout at once.
                refresh_projectors(h);
                Ok::<_, String>(notes)
            })
            .await??;
        let failed = crate::studio_keys::apply(&self.app, &keys);
        let mut all = note;
        if !failed.is_empty() {
            all.push(format!("These shortcuts are used by another app or aren't valid here: {}.", failed.join(", ")));
        }
        Ok((!all.is_empty()).then(|| all.join(" ")))
    }

    async fn stats(&self) -> Result<Value, String> {
        let data = self.data.clone();
        self.on_main(move |h| {
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
                    save_confirmed(h.collection_deferred, c, &scenes_file(&data));
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
                "virtualcam": h.virtualcam.as_ref().map(|c| json!({
                    "active": c.active(),
                    "every_app": c.every_app(),
                    "source": h.virtualcam_source,
                })),
                "ndi": h.ndi.as_ref().filter(|n| n.active()).map(|n| json!({ "name": n.name() })),
                // Captures on air that show nothing (the black screen fixer).
                "captures": h.collection.as_ref().map(|c| c.capture_problems()).unwrap_or_default(),
                // Pictures each source has sent to the card, and their size: the page works out
                // each one's rate between two reads ("What's slowing my stream?").
                "sources": h.collection.as_ref().map(|c| c.source_load()).unwrap_or_default().into_iter()
                    .map(|l| json!({ "name": l.name, "sent": l.sent, "width": l.width, "height": l.height }))
                    .collect::<Vec<_>>(),
                // The last remux (OBS's Remux, or MKV made into MP4 after recording).
                "remux": REMUX.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            });
            let cycles = h.collection.as_ref().map(|c| c.sidechain_cycles()).unwrap_or_default();
            if !cycles.is_empty()
                && let Some(o) = out.as_object_mut()
            {
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
            Ok(out)
        })
        .await?
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
                apply_waiting_canvas(&app, h);
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
            let plan = ferrocast_studio::RecordPlan {
                folder,
                name: s.record_name.clone(),
                format: s.record_format.clone(),
                spaces: s.record_spaces,
                overwrite: s.record_overwrite,
                split_minutes: s.record_split_minutes,
                split_mb: s.record_split_mb,
                tracks: s.record_tracks.clone(),
                encode: record_encode_capped(&s, h.stream_cap),
                muxer: s.record_muxer.clone(),
                own_encoder: s.own_encoder,
            };
            let file = studio.record_path(&plan);
            // Ferrocast's own engine draws, mixes and encodes every recording; what it can't do
            // yet is refused with the reason (libobs no longer stands in).
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            // "Same as stream" while streaming: one encode for both (the Mac's one video
            // encoder can't do two at 4K60: 31 of 60 fps, 19 Nov), when they can share.
            let live = h.stream.as_ref().filter(|st| st.stats().active).zip(h.stream_with.as_ref());
            let rec = match live.map(|(st, with)| {
                let can = if c.started(st) {
                    ferrocast_studio::stage::can_share(with, &plan, s.record_quality == "stream", c.canvas().fps, s.fps)
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
            save_confirmed(h.collection_deferred, c, &scenes_file(&data));
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
        self.on_main(move |h| {
            if !on {
                h.replay = None;
                apply_waiting_canvas(&app, h);
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
        self.on_main(move |h| {
            h.virtualcam = None;
            if !on {
                // A frame rate chosen while it ran can be applied now (review).
                apply_waiting_canvas(&app, h);
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
        self.on_main(move |h| {
            h.ndi = None;
            if !on {
                // A frame rate chosen while it ran can be applied now (review).
                apply_waiting_canvas(&app, h);
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
            Ok(())
        })
        .await?
    }

    async fn set_live_quality(&self, pct: u8) -> Result<(), String> {
        self.on_main(move |h| {
            let set = h.settings.as_ref().map_or(6000, |s| s.video_kbps);
            let capped = h.stream_cap.map_or(set, |c| set.min(c.max_kbps));
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
                            save_confirmed(h.collection_deferred, c, &f);
                        }
                    })
                    .await?;
                }
                let doc = ferrocast_store::durable::load_json_with_backup::<Value>(&file, |_| true).map(|l| l.value);
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
                                save_confirmed(h.collection_deferred, c, &f);
                            }
                        })
                        .await?;
                    }
                    let to = library::file(&data, kind, &id);
                    if let Some(loaded) = ferrocast_store::durable::load_json_with_backup::<Value>(&from, |_| true) {
                        let bytes = serde_json::to_vec_pretty(&loaded.value).map_err(|e| e.to_string())?;
                        // Off the async runtime (a locked file is waited out), and still
                        // synchronous: the copy is switched to right after, and a failure is
                        // this request's answer.
                        tokio::task::spawn_blocking(move || ferrocast_store::durable::write_json_bytes(&to, &bytes))
                            .await
                            .map_err(|e| e.to_string())?
                            .map_err(|e| format!("The copy couldn't be made: {e}"))?;
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
                        h.collection.as_ref().map(|_| {
                            ferrocast_studio::kinds::kinds()
                                .inputs
                                .into_iter()
                                .map(|k| k.id)
                                .collect::<std::collections::BTreeSet<_>>()
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
        // The saved profile (loaded if nothing asked for it yet), and a replay buffer that runs
        // (one that failed isn't an encode; CodeRabbit).
        let (saved, replay, cap) = self
            .on_main(move |h| {
                let s = h.settings.get_or_insert_with(|| load_settings(&data)).clone();
                (s, h.replay.as_ref().is_some_and(|r| r.active()), h.stream_cap)
            })
            .await?;
        let s = match draft {
            Some(d) => serde_json::from_value::<StudioSettings>(d).map_err(|e| format!("settings: {e}"))?,
            None => saved,
        };
        let also = Also { enhanced, light, replay: replay || s.replay_when_streaming };
        Ok(encode_budget(&s, cap, ferrocast_studio::system::video_engines(), also))
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
            self.on_main(move |h| {
                let c = h.collection.as_ref().ok_or(STOPPED)?;
                let of = match p.what.as_str() {
                    "program" => ferrocast_studio::ProjectorOf::Program,
                    "multiview" => ferrocast_studio::ProjectorOf::Multiview,
                    _ => ferrocast_studio::ProjectorOf::Source(p.name.clone().unwrap_or_default()),
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
                        let Some(c) = &h.collection else { return };
                        let done =
                            if c.preview_scene().is_some() { c.set_preview(&scene) } else { c.set_current(&scene) };
                        if c.moving() {
                            glide();
                        }
                        if let Err(e) = done {
                            tracing::warn!(error = %e, "a multiview click couldn't switch scenes");
                        }
                        sync_views(&h);
                    });
                });
                let look = multiview_look(&h.settings.clone().unwrap_or_default());
                let made = c
                    .projector(&of, p.screen.map(|s| s as usize), look, on_close, on_pick)
                    .map_err(|e| e.to_string())?;
                if p.on_top {
                    made.set_on_top(true);
                }
                h.projectors.push((id, made));
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

    async fn stream_start(&self, server: String, key: String, tracks: Vec<(u32, u32, u32)>) -> Result<Value, String> {
        let app = self.app.clone();
        self.on_main(move |h| {
            h.collection.as_ref().ok_or(STOPPED)?;
            if let Some(s) = h.stream.as_ref().filter(|s| s.stats().active) {
                return Ok(stats_of(s));
            }
            // A stopped or failed one goes first: one output at a time.
            h.stream = None;
            h.live_pct = None;
            // A frame rate chosen during the last stream, still waiting.
            apply_waiting_canvas(&app, h);
            let s = h.settings.clone().unwrap_or_default();
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
                    ferrocast_studio::stage::can_share(&settings, plan, *same, c.canvas().fps, s.fps)
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
            let v = stats_of(&stream);
            h.stream = Some(stream);
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
        self.on_main(move |h| {
            h.vstream = None;
            h.lstream = None;
            h.stream = None;
            apply_waiting_canvas(&app, h);
        })
        .await
    }

    async fn light_stream_start(&self, server: String, key: String) -> Result<(), String> {
        self.on_main(move |h| {
            let studio = h.collection.as_ref().ok_or(STOPPED)?;
            if h.lstream.as_ref().is_some_and(|s| s.stats().active) {
                return Ok(());
            }
            h.lstream = None;
            let s = h.settings.clone().unwrap_or_default();
            let mut settings = stream_settings(&s, None);
            // 720 high at the canvas's shape (even sizes), 3,000 kbps at most; one sound.
            let canvas = studio.canvas();
            settings.encode.scale = light_size(canvas.width, canvas.height);
            settings.encode.video_kbps = settings.encode.video_kbps.min(3000);
            settings.vod_track = None;
            tracing::info!(kbps = settings.encode.video_kbps, "the Studio's lighter stream starts into Go live");
            let c = h.collection.as_ref().ok_or(STOPPED)?;
            h.lstream = Some(c.stream_drawn(&server, &key, &settings).map_err(|e| e.to_string())?);
            Ok(())
        })
        .await?
    }

    async fn light_stream_stop(&self) -> Result<(), String> {
        self.on_main(|h| {
            h.lstream = None;
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
        self.on_main(|h| h.stream.as_ref().map_or(Value::Null, stats_of)).await
    }

    async fn direct_start(&self, id: String, url: String, key: String, simulcast: bool) -> Result<(), String> {
        self.on_main(move |h| {
            let s = h.stream.as_mut().filter(|s| s.stats().active).ok_or("The Studio isn't streaming.")?;
            tracing::info!(dest = %id, simulcast, "the Studio sends straight to a server (SRT, RIST or WebRTC)");
            // Simulcast's lower layer is the lighter stream, when it runs.
            match h.lstream.as_ref().filter(|l| simulcast && l.stats().active) {
                Some(lower) => s.add_direct_layered(&id, &url, &key, lower),
                None => s.add_direct(&id, &url, &key),
            }
            .map_err(|e| e.to_string())
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

        // Corrupt existing settings must not clobber `.bak` on a save_json overwrite.
        save_json(&settings_path, &serde_json::to_value(&second).unwrap_or_else(|e| panic!("{e}")));
        ferrocast_store::durable::flush_saves(std::time::Duration::from_secs(10));
        assert_eq!((load_settings(dir.path()).width, load_settings(dir.path()).fps), (2560, 60));
    }

    /// An undo or redo storm saves after every step: through the queue the file ends as the last
    /// state, whole, and no step waits on a disk write (agy GAPS #22).
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
        let twitch = StreamCap { max_kbps: 6_000, h264_only: true, keyframe_secs: 2 };
        let stream = stream_settings(&s, Some(twitch));
        let capped = record_encode_capped(&s, Some(twitch));
        assert!(capped.strict_cbr && capped.scale.is_none());
        let plan = ferrocast_studio::RecordPlan { tracks: vec![1], encode: capped, ..Default::default() };
        assert_eq!(ferrocast_studio::stage::can_share(&stream, &plan, true, 60, 60), Ok(()));
        // Without a cap, nothing changes: the plain stream and the plain recording share.
        let plain = ferrocast_studio::RecordPlan {
            tracks: vec![1],
            encode: record_encode_capped(&s, None),
            ..Default::default()
        };
        assert_eq!(ferrocast_studio::stage::can_share(&stream_settings(&s, None), &plain, true, 60, 60), Ok(()));
        // A recording that began plain can't be joined by a strict stream: it gets its own encoder.
        assert!(ferrocast_studio::stage::can_share(&stream, &plain, true, 60, 60).is_err());
        // The other qualities are as chosen, cap or not.
        let high = StudioSettings { record_quality: "high".into(), ..s.clone() };
        assert_eq!(record_encode_capped(&high, Some(twitch)), record_encode(&high));
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
        let twitch = StreamCap { max_kbps: 6_000, h264_only: true, keyframe_secs: 2 };
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
    }
}
