//! Streaming and recording with Ferrocast's own encoders (ENGINE.md E1, behind the "own
//! encoder" switch): libobs still draws and mixes, but its finished frames and sound go through
//! ferrocast-av's encoders, out over RTMP by Go live's own sender to the same server libobs
//! would send to (Go live's ingest on this computer), or into a crash-safe MP4. Everything after
//! looks the same to the rest of the app.
//!
//! With the engine drawing too (E7, `OwnStream::drawn`), libobs isn't in the picture at all:
//! the collection is opened in ferrocast-av's engine, which draws, mixes and encodes it, and
//! follows the Studio's scene changes and edits (`OwnStream::follow`).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ferrocast_av::TransitionKind;
use ferrocast_av::audio::AudioConfig;
use ferrocast_av::engine::{Canvas as AvCanvas, Engine, EngineConfig, OutputKind};
use ferrocast_av::pipeline::{Pipeline, Tag};
use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocast_golive::push::{self, Destination, Health, PushState, Retry};
use ferrocast_golive::{Hub, Target};

use crate::collection::VERTICAL_SIZE;
use crate::{EncodeSettings, ProgramFeed, RecordPlan, RecordStats, StreamSettings, StreamStats, Studio, StudioError};

/// x264 (and Lossless) run through FFmpeg, which Ferrocast fetches the first time it's needed.
const NO_FFMPEG: &str =
    "x264 or Lossless before FFmpeg is downloaded (Go live › Vertical › Cut it from your stream fetches it)";

/// Why this stream can't use our encoders (it then uses libobs's), or None when it can.
pub(crate) fn unsupported(s: &StreamSettings, main_canvas: bool) -> Option<&'static str> {
    if !main_canvas {
        Some("the vertical picture's stream")
    } else if s.vod_track.is_some() {
        Some("Twitch's VOD track")
    } else if s.encode.quality.is_some() && !cfg!(target_os = "macos") && !is_x264(&s.encode.video_encoder) {
        Some("constant quality")
    } else if !s.encode.extra.is_empty() {
        Some("an encoder's own options")
    } else if is_x264(&s.encode.video_encoder) && !ferrocast_av::ffenc::available() {
        Some(NO_FFMPEG)
    } else if !own_codec_ok(&s.encode.video_encoder) {
        Some("this encoder (ours is H.264 or HEVC on the Mac's hardware)")
    } else {
        system_unsupported(&s.encode.video_encoder)
    }
}

/// Why this recording can't use our encoders, or None when it can.
pub(crate) fn unsupported_recording(plan: &RecordPlan) -> Option<&'static str> {
    if !matches!(plan.format.as_str(), "mp4" | "fmp4" | "mkv") {
        Some("this format (ours writes MP4 and MKV)")
    } else if !plan.muxer.trim().is_empty() {
        Some("custom muxer settings")
    } else if plan.encode.quality.is_some() && !cfg!(target_os = "macos") && !is_x264(&plan.encode.video_encoder) {
        Some("constant quality")
    } else if !plan.encode.extra.is_empty() {
        Some("an encoder's own options")
    } else if is_x264(&plan.encode.video_encoder) && !ferrocast_av::ffenc::available() {
        Some(NO_FFMPEG)
    } else if !own_codec_ok(&plan.encode.video_encoder) {
        Some("this encoder (ours is H.264 or HEVC on the Mac's hardware)")
    } else {
        system_unsupported(&plan.encode.video_encoder)
    }
}

/// The rate our encoder runs at. Constant quality is libobs's 0 (lossless) … 51 scale: x264's
/// rate factor as it is, or VideoToolbox's 0-1 as libobs turns it (Mac only: FFmpeg's hardware
/// encoders keep a bitrate).
fn rate_of(e: &EncodeSettings) -> RateControl {
    match e.quality {
        Some(q) if is_x264(&e.video_encoder) => RateControl::Crf(q.min(51) as u8),
        Some(q) => RateControl::Quality((100 - q.min(51) * 100 / 51) as f32 / 100.0),
        None if e.vbr => RateControl::Average,
        None => RateControl::Cbr,
    }
}

/// Off a Mac our encoders go through FFmpeg: H.264 only, and only with an encoder that works.
fn system_unsupported(encoder_id: &str) -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        None
    } else if encoder_id.to_ascii_lowercase().contains("hevc") {
        Some("HEVC on this system (ours does H.264 here so far)")
    } else if ferrocast_av::ffenc::h264_encoder().is_none() {
        Some("this system without a working FFmpeg H.264 encoder")
    } else {
        None
    }
}

/// Whether the picked encoder is one ours stands in for: the Mac's hardware H.264 or HEVC, or
/// x264 (through FFmpeg).
fn own_codec_ok(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !(id.contains("av1") || id.contains("aom") || id.contains("svt") || id.contains("prores"))
}

/// x264, OBS's software encoder (`obs_x264`): ours runs it through FFmpeg.
fn is_x264(id: &str) -> bool {
    id.to_ascii_lowercase().contains("x264")
}

#[derive(Debug, Default)]
struct Counts {
    bytes: AtomicU64,
    frames: AtomicU32,
    running: AtomicBool,
    error: Mutex<Option<String>>,
    /// A new bitrate asked for while live (0: none), for the encoder thread to take.
    new_kbps: AtomicU32,
}

/// Where a stream goes: the server, the key, and how it reconnects.
pub(crate) struct Send<'a> {
    pub server: &'a str,
    pub key: &'a str,
    pub reconnect: Option<(u32, u32)>,
}

/// A stream or recording through our encoders; dropping it ends it (the encoders flushed, the
/// server told, the file closed).
#[derive(Debug)]
pub(crate) struct OwnStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counts: Arc<Counts>,
    /// The sender's health and its stop, when streaming.
    health: Option<tokio::sync::watch::Receiver<Health>>,
    stop_push: Option<tokio::sync::watch::Sender<bool>>,
    runtime: Option<tokio::runtime::Runtime>,
    /// libobs's side of the hand-over (kept on this thread: it holds the Studio); its
    /// channels are with the encoder thread.
    feed: Option<ProgramFeed>,
    /// Our engine drawing the collection (instead of libobs's frames).
    drawn: Option<Drawn>,
    hub: Option<Arc<Hub>>,
}

/// Our engine with the collection open, and what it last opened.
#[derive(Debug)]
struct Drawn {
    engine: Engine,
    output: u64,
    /// The collection as last opened, without the scene on air: when it differs, it's opened
    /// again (at most twice a second).
    shown: serde_json::Value,
    opened: std::time::Instant,
    on_air: Option<String>,
    /// What couldn't come over.
    skipped: Vec<String>,
    /// A recording's plan (its names and when to split), and when the file now written began.
    plan: Option<(RecordPlan, std::time::Instant)>,
    /// The vertical picture's scene on air, when this draws the vertical picture.
    vertical: Option<Option<String>>,
    /// The scene or source shown on its own, when this is a Source Record.
    solo: Option<String>,
}

/// Where a replay buffer's saves stand: the last file written, or why the last save failed.
#[derive(Debug, Default)]
pub(crate) struct ReplaySaved {
    pub last: Option<std::path::PathBuf>,
    pub failed: Option<String>,
}

/// What a drawn output is: where it goes, how it's encoded, and which picture.
pub(crate) struct Drawing<'a> {
    pub send_to: Option<Send<'a>>,
    pub record: Option<&'a std::path::Path>,
    pub encode: &'a EncodeSettings,
    /// OBS's sound tracks (1 to 6) a recording carries; empty is track 1.
    pub tracks: &'a [u8],
    /// Seconds the stream is held back.
    pub delay_s: u32,
    /// The vertical picture (9:16, its own scenes) instead of the main one.
    pub vertical: bool,
    /// A replay buffer instead: seconds kept, and at most this many megabytes (0: no limit).
    pub replay: Option<(u32, u32)>,
    /// One scene or source on its own (OBS's Source Record) instead of what's on air.
    pub solo: Option<&'a str>,
}

/// The scene a source shown alone is drawn from (no collection names a scene like it).
const ALONE: &str = "\u{1}Ferrocast: on its own";

/// Shows a scene, or a source alone, for a Source Record → the scene to draw and the size: a
/// scene at `canvas`; a source at its own size (waited for up to 2 s while it starts; `canvas`
/// if it has none by then), even-sided for the encoder.
fn show_alone(engine: &Engine, name: &str, canvas: (u32, u32)) -> Result<(String, u32, u32), StudioError> {
    if engine.scene_names().iter().any(|s| s == name) {
        return Ok((name.to_string(), canvas.0, canvas.1));
    }
    let fail = || StudioError::Failed(format!("Ferrocast's own engine can't show \"{name}\" on its own yet."));
    let started = std::time::Instant::now();
    let (id, size) = loop {
        let (id, size) = engine.source_named(name).ok_or_else(fail)?;
        if size.is_some() || started.elapsed() >= Duration::from_secs(2) {
            break (id, size);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (w, h) = size.map_or(canvas, |(w, h)| ((w.max(2) & !1), (h.max(2) & !1)));
    let (w, h) = if (w, h) == (0, 0) { canvas } else { (w, h) };
    let item = ferrocast_av::engine::SceneItem::new(
        id,
        ferrocast_av::Placement {
            bounds: ferrocast_av::Bounds::Fit,
            bounds_width: w as f32,
            bounds_height: h as f32,
            ..Default::default()
        },
    );
    engine.set_scene(ALONE, vec![item]);
    Ok((ALONE.to_string(), w, h))
}

/// The vertical picture's scene on air in a saved collection.
fn vertical_on_air(doc: &serde_json::Value) -> Option<String> {
    doc.pointer("/ferrocast/vertical/current_scene")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// The stream on its way to Go live's hub, held back `delay` first (OBS's stream delay: what
/// you do shows that much later, so viewers can't snipe). Ending sends what's still held, so
/// the last seconds aren't lost.
struct Delayed {
    hub: Arc<Hub>,
    counts: Arc<Counts>,
    delay: Duration,
    held: std::collections::VecDeque<(std::time::Instant, Tag)>,
}

impl Delayed {
    fn send(&mut self, t: Tag) {
        if self.delay.is_zero() {
            return self.pass(t);
        }
        self.held.push_back((std::time::Instant::now(), t));
        while self.held.front().is_some_and(|(at, _)| at.elapsed() >= self.delay) {
            if let Some((_, t)) = self.held.pop_front() {
                self.pass(t);
            }
        }
    }

    fn pass(&self, t: Tag) {
        match t {
            Tag::Video { ms, body } => {
                self.counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                self.hub.video(ms, body.into());
            }
            Tag::Audio { ms, body } => {
                self.counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                self.hub.audio(ms, body.into());
            }
        }
    }
}

impl Drop for Delayed {
    fn drop(&mut self) {
        while let Some((_, t)) = self.held.pop_front() {
            self.pass(t);
        }
    }
}

/// At most how often edits reopen the collection in a drawn stream (edits in between go with
/// the next one; sources that didn't change keep running).
const REOPEN_EVERY: Duration = Duration::from_millis(500);

/// A collection without which scene is on air (what a reopen depends on).
fn without_on_air(doc: &serde_json::Value) -> serde_json::Value {
    let mut d = doc.clone();
    if let Some(m) = d.as_object_mut() {
        m.remove("current_scene");
        m.remove("current_program_scene");
    }
    d
}

/// The engine's transition for the Studio's (by OBS's id); the rest fade for now.
fn transition_of(doc: &serde_json::Value) -> (TransitionKind, u64) {
    let ms = doc.get("transition_duration").and_then(serde_json::Value::as_u64).unwrap_or(300);
    let name = doc.get("current_transition").and_then(serde_json::Value::as_str).unwrap_or("Fade");
    let id = doc
        .get("transitions")
        .and_then(serde_json::Value::as_array)
        .and_then(|t| t.iter().find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some(name)))
        .and_then(|t| t.get("id").and_then(serde_json::Value::as_str))
        .unwrap_or(if name == "Cut" { "cut_transition" } else { "fade_transition" });
    match id {
        "cut_transition" => (TransitionKind::Cut, 0),
        "ferrocast_move" | "move_transition" => (TransitionKind::Move, ms),
        _ => (TransitionKind::Fade, ms),
    }
}

impl OwnStream {
    /// A stream as `s` says.
    pub(crate) fn stream(studio: &Studio, server: &str, key: &str, s: &StreamSettings) -> Result<Self, StudioError> {
        Self::start(studio, Some(Send { server, key, reconnect: s.reconnect }), None, &s.encode)
    }

    /// A recording to `path` (a fragmented MP4: it plays even after a crash).
    pub(crate) fn record(studio: &Studio, path: &std::path::Path, plan: &RecordPlan) -> Result<Self, StudioError> {
        Self::start(studio, None, Some(path), &plan.encode)
    }

    fn start(
        studio: &Studio,
        send_to: Option<Send<'_>>,
        record: Option<&std::path::Path>,
        e: &EncodeSettings,
    ) -> Result<Self, StudioError> {
        let fail = |e: String| StudioError::Failed(format!("Ferrocast's own encoder couldn't start: {e}"));
        let target = match &send_to {
            Some(d) => Some(Target::parse(d.server).map_err(|e| fail(e.to_string()))?),
            None => None,
        };
        let mut feed = studio.program_feed()?;
        // The channels go to the encoder thread; the feed itself stays here.
        let frames = std::mem::replace(&mut feed.video, std::sync::mpsc::sync_channel(0).1);
        let sound = std::mem::replace(&mut feed.sound, std::sync::mpsc::sync_channel(0).1);
        let codec =
            if e.video_encoder.to_ascii_lowercase().contains("hevc") { VideoCodec::Hevc } else { VideoCodec::H264 };
        let video = VideoConfig {
            codec,
            input: PixelFormat::Nv12,
            width: feed.width,
            height: feed.height,
            fps_num: feed.fps_num,
            fps_den: feed.fps_den.max(1),
            bitrate_kbps: e.video_kbps.max(100),
            rate: rate_of(e),
            keyframe_secs: if e.keyframe_s == 0 { 2 } else { e.keyframe_s },
            software: is_x264(&e.video_encoder),
        };
        let audio =
            AudioConfig { sample_rate: feed.sample_rate, channels: 2, bitrate_kbps: e.audio_kbps.clamp(64, 320) };
        let mut pipe = Pipeline::new(video, audio, record).map_err(|e| fail(e.to_string()))?;

        let hub = Hub::new();
        let (runtime, health, stop_push) = sender(send_to, target, &hub).map_err(fail)?;

        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new(Counts { running: AtomicBool::new(true), ..Default::default() });
        let thread = {
            let (stop, counts) = (stop.clone(), counts.clone());
            std::thread::Builder::new()
                .name("ferrocast-own-encoder".into())
                .spawn(move || {
                    hub.start();
                    let send = |tags: Vec<Tag>, counts: &Counts| {
                        for t in tags {
                            match t {
                                Tag::Video { ms, body } => {
                                    counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                    hub.video(ms, body.into());
                                }
                                Tag::Audio { ms, body } => {
                                    counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                    hub.audio(ms, body.into());
                                }
                            }
                        }
                    };
                    // The first error shows at once (the app polls last_error) and ends the stream.
                    let fail_now = |e: String| {
                        if let Ok(mut err) = counts.error.lock()
                            && err.is_none()
                        {
                            *err = Some(e);
                        }
                    };
                    while !stop.load(Ordering::Acquire) {
                        let kbps = counts.new_kbps.swap(0, Ordering::AcqRel);
                        if kbps > 0
                            && let Err(e) = pipe.set_video_kbps(kbps)
                        {
                            tracing::warn!("Ferrocast's own encoder kept its bitrate: {e}");
                        }
                        if let Ok(f) = frames.recv_timeout(Duration::from_millis(100)) {
                            let frame =
                                Frame::Nv12 { y: &f.y, y_stride: f.y_stride, uv: &f.uv, uv_stride: f.uv_stride };
                            match pipe.video(frame, f.timestamp_ns) {
                                Ok(t) => {
                                    send(t, &counts);
                                    counts.frames.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    fail_now(e.to_string());
                                    break;
                                }
                            }
                        }
                        let mut sound_failed = false;
                        while let Ok(snd) = sound.try_recv() {
                            match pipe.audio(&snd.samples, snd.timestamp_ns) {
                                Ok(t) => send(t, &counts),
                                Err(e) => {
                                    fail_now(e.to_string());
                                    sound_failed = true;
                                    break;
                                }
                            }
                        }
                        if sound_failed {
                            break;
                        }
                        if pipe.recording_failed() {
                            fail_now("the recording couldn't be written (is the disk full?)".into());
                            break;
                        }
                    }
                    if let Ok(t) = pipe.finish() {
                        send(t, &counts);
                    }
                    hub.end();
                    counts.running.store(false, Ordering::Release);
                })
                .map_err(|e| fail(e.to_string()))?
        };
        tracing::info!(
            ?codec,
            kbps = video.bitrate_kbps,
            recording = record.is_some(),
            "the Studio uses Ferrocast's own encoder"
        );
        Ok(Self {
            stop,
            thread: Some(thread),
            counts,
            health,
            stop_push,
            runtime,
            feed: Some(feed),
            drawn: None,
            hub: None,
        })
    }

    /// A stream or recording drawn by our engine from the collection `doc` (as
    /// `Collection::save` gives it), as `how` says.
    pub(crate) fn drawn(studio: &Studio, doc: &serde_json::Value, how: Drawing<'_>) -> Result<Self, StudioError> {
        let Drawing { send_to, record, encode: e, tracks, delay_s, vertical, replay, solo } = how;
        let fail = |e: String| StudioError::Failed(format!("Ferrocast's own engine couldn't start: {e}"));
        let target = match &send_to {
            Some(d) => Some(Target::parse(d.server).map_err(|e| fail(e.to_string()))?),
            None => None,
        };
        let canvas = studio.canvas();
        let codec =
            if e.video_encoder.to_ascii_lowercase().contains("hevc") { VideoCodec::Hevc } else { VideoCodec::H264 };
        let video = VideoConfig {
            codec,
            input: PixelFormat::Nv12,
            width: canvas.width,
            height: canvas.height,
            fps_num: canvas.fps.max(1),
            fps_den: 1,
            bitrate_kbps: e.video_kbps.max(100),
            rate: rate_of(e),
            keyframe_secs: if e.keyframe_s == 0 { 2 } else { e.keyframe_s },
            software: is_x264(&e.video_encoder),
        };
        let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: e.audio_kbps.clamp(64, 320) };
        let config = EngineConfig {
            width: canvas.width,
            height: canvas.height,
            fps: canvas.fps.max(1),
            ..EngineConfig::default()
        };
        let engine = Engine::new(config, video, audio).map_err(|e| fail(e.to_string()))?;
        let skipped = engine.load_obs(doc);
        for s in &skipped {
            tracing::info!("Ferrocast's own engine doesn't bring this over yet: {s}");
        }
        let hub = Hub::new();
        let (runtime, health, stop_push) = sender(send_to, target, &hub).map_err(fail)?;
        let counts = Arc::new(Counts { running: AtomicBool::new(true), ..Default::default() });
        hub.start();
        let kind = match (record, replay) {
            (_, Some((seconds, mb))) => OutputKind::Replay { seconds, mb },
            (Some(p), None) => OutputKind::Record(p.to_path_buf()),
            (None, None) => {
                let mut out = Delayed {
                    hub: hub.clone(),
                    counts: counts.clone(),
                    delay: Duration::from_secs(u64::from(delay_s)),
                    held: std::collections::VecDeque::new(),
                };
                OutputKind::Stream(Box::new(move |t: Tag| out.send(t)))
            }
        };
        // OBS's output resolution: the whole canvas, scaled (sides kept even for the encoder).
        let mut out = match (vertical, e.scale) {
            // The vertical picture: its own size and its own scene on air.
            (true, _) => AvCanvas {
                width: VERTICAL_SIZE.0,
                height: VERTICAL_SIZE.1,
                scene: vertical_on_air(doc),
                ..AvCanvas::main(config)
            },
            (false, Some((w, h))) => AvCanvas::scaled(config, w.max(2) & !1, h.max(2) & !1),
            (false, None) => AvCanvas::main(config),
        };
        if let Some(name) = solo {
            let (scene, w, h) = show_alone(&engine, name, (config.width, config.height))?;
            (out.scene, out.width, out.height, out.scaled_from) = (Some(scene), w, h, None);
        }
        // OBS's tracks (1 to 6): the first is the file's first sound track, the rest follow.
        let mut picked = tracks.iter().filter(|t| (1..=6).contains(*t)).map(|t| usize::from(*t) - 1);
        if let Some(first) = picked.next() {
            out.track = first;
            out.more_tracks = picked.collect();
        }
        let output = engine.start_output_on(kind, out).map_err(|e| fail(e.to_string()))?;
        tracing::info!(?codec, kbps = video.bitrate_kbps, recording = record.is_some(), "Ferrocast's own engine draws");
        let on_air = doc.get("current_program_scene").and_then(serde_json::Value::as_str).map(String::from);
        Ok(Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            counts,
            health,
            stop_push,
            runtime,
            feed: None,
            drawn: Some(Drawn {
                engine,
                output,
                shown: without_on_air(doc),
                opened: std::time::Instant::now(),
                on_air,
                skipped,
                plan: None,
                vertical: vertical.then(|| vertical_on_air(doc)),
                solo: solo.map(String::from),
            }),
            hub: Some(hub),
        })
    }

    /// Keeps a drawn stream with the Studio: a new scene on air goes on through the Studio's
    /// transition; any other change opens the collection again (only changed sources restart). Returns
    /// whether it was opened again. Nothing for a stream libobs draws.
    pub(crate) fn follow(&mut self, doc: &serde_json::Value) -> bool {
        let Some(d) = self.drawn.as_mut() else { return false };
        // The vertical picture follows its own scene on air (a cut).
        if let Some(shown) = d.vertical.as_mut() {
            let now = vertical_on_air(doc);
            if now != *shown {
                if let Err(e) = d.engine.set_output_scene(d.output, now.clone()) {
                    tracing::warn!("Ferrocast's own engine couldn't follow the vertical scene: {e}");
                }
                *shown = now;
            }
        }
        let bare = without_on_air(doc);
        if bare != d.shown && d.opened.elapsed() >= REOPEN_EVERY {
            d.skipped = d.engine.load_obs(doc);
            // Opening again clears the scenes: a source shown alone gets its own scene back.
            if let Some(name) = &d.solo
                && let Err(e) = show_alone(&d.engine, name, (0, 0))
            {
                tracing::warn!("Ferrocast's own engine lost the source it records: {e}");
            }
            d.shown = bare;
            d.opened = std::time::Instant::now();
            d.on_air = doc.get("current_program_scene").and_then(serde_json::Value::as_str).map(String::from);
            return true;
        }
        let now = doc.get("current_program_scene").and_then(serde_json::Value::as_str).map(String::from);
        if now != d.on_air
            && let Some(name) = &now
        {
            let (kind, ms) = transition_of(doc);
            if let Err(e) = d.engine.go_with(name, kind, ms) {
                tracing::warn!("Ferrocast's own engine couldn't follow the scene change: {e}");
            }
            d.on_air = now;
        }
        false
    }

    /// A new video bitrate while live (the stream doctor's lighter one, a manual change).
    pub(crate) fn set_video_kbps(&self, kbps: u32) {
        match &self.drawn {
            Some(d) => {
                if let Err(e) = d.engine.set_output_kbps(d.output, kbps) {
                    tracing::warn!("Ferrocast's own engine kept its bitrate: {e}");
                }
            }
            None => self.counts.new_kbps.store(kbps, Ordering::Release),
        }
    }

    /// Keeps a recording's plan: what the next file is called, and when it splits on its own.
    pub(crate) fn set_plan(&mut self, plan: &RecordPlan) {
        if let Some(d) = self.drawn.as_mut() {
            d.plan = Some((plan.clone(), std::time::Instant::now()));
        }
    }

    pub(crate) fn plan(&self) -> Option<&RecordPlan> {
        self.drawn.as_ref().and_then(|d| d.plan.as_ref()).map(|(p, _)| p)
    }

    /// Saves what a replay buffer holds to `path`, written on a thread of its own; `last`
    /// is set to the file once it's there (or `failed` to why not).
    pub(crate) fn save_replay(&self, path: &std::path::Path, done: Arc<Mutex<ReplaySaved>>) -> Result<(), StudioError> {
        let d = self.drawn.as_ref().ok_or_else(|| StudioError::Failed("The replay buffer couldn't save.".into()))?;
        let clip = d.engine.replay_clip(d.output).map_err(|e| StudioError::Failed(e.to_string()))?;
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("ferrocast-replay-save".into())
            .spawn(move || {
                let result = clip.write(&path);
                let mut d = done.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(()) => {
                        tracing::info!(file = %path.display(), "a replay was saved");
                        d.last = Some(path);
                    }
                    Err(e) => d.failed = Some(e.to_string()),
                }
            })
            .map_err(|e| StudioError::Failed(format!("The replay couldn't be saved: {e}")))?;
        Ok(())
    }

    /// Pauses or resumes the recording (no gap in the file).
    pub(crate) fn pause(&self, on: bool) -> Result<(), StudioError> {
        let d = self.drawn.as_ref().ok_or_else(|| StudioError::Failed("This recording can't pause.".into()))?;
        d.engine.pause_output(d.output, on).map_err(|e| StudioError::Failed(e.to_string()))
    }

    /// Goes on in a new file at `path` from the next frame.
    pub(crate) fn split(&mut self, path: &std::path::Path) -> Result<(), StudioError> {
        let d = self.drawn.as_mut().ok_or_else(|| StudioError::Failed("This recording can't be split.".into()))?;
        d.engine.split_output(d.output, path).map_err(|e| StudioError::Failed(e.to_string()))?;
        if let Some((_, since)) = d.plan.as_mut() {
            *since = std::time::Instant::now();
        }
        tracing::info!(file = %path.display(), "the recording goes on in a new file");
        Ok(())
    }

    /// Whether the plan says it's time for a new file (its minutes or megabytes are reached).
    pub(crate) fn split_due(&self) -> bool {
        let Some(d) = &self.drawn else { return false };
        let Some((plan, since)) = &d.plan else { return false };
        let Ok(state) = d.engine.output_record_state(d.output) else { return false };
        if state.splitting {
            return false;
        }
        let bytes = state.bytes;
        (plan.split_minutes > 0 && since.elapsed() >= Duration::from_secs(u64::from(plan.split_minutes) * 60))
            || (plan.split_mb > 0 && bytes >= u64::from(plan.split_mb) * 1024 * 1024)
    }

    /// Whether our engine draws this (not libobs).
    pub(crate) fn is_drawn(&self) -> bool {
        self.drawn.is_some()
    }

    /// What our engine couldn't bring over (empty when libobs draws).
    pub(crate) fn skipped(&self) -> &[String] {
        self.drawn.as_ref().map_or(&[], |d| &d.skipped)
    }

    pub(crate) fn stats(&self) -> StreamStats {
        let state = self.health.as_ref().map(|h| h.borrow().state.clone());
        if let Some(d) = &self.drawn {
            let s = d.engine.stats();
            return StreamStats {
                active: d.engine.error().is_none(),
                reconnecting: matches!(state, Some(PushState::Reconnecting { .. } | PushState::Connecting)),
                bytes: self.counts.bytes.load(Ordering::Relaxed),
                frames: s.frames.min(u64::from(u32::MAX)) as u32,
                dropped: s.skipped.min(u64::from(u32::MAX)) as u32,
            };
        }
        StreamStats {
            active: self.counts.running.load(Ordering::Acquire),
            reconnecting: matches!(state, Some(PushState::Reconnecting { .. } | PushState::Connecting)),
            bytes: self.counts.bytes.load(Ordering::Relaxed),
            frames: self.counts.frames.load(Ordering::Relaxed),
            dropped: self.feed.as_ref().map_or(0, |f| f.dropped()).min(u64::from(u32::MAX)) as u32,
        }
    }

    pub(crate) fn record_stats(&self) -> RecordStats {
        let s = self.stats();
        let paused =
            self.drawn.as_ref().is_some_and(|d| d.engine.output_record_state(d.output).is_ok_and(|r| r.paused));
        RecordStats { active: s.active, paused, bytes: s.bytes, frames: s.frames }
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        if let Some(e) = self.drawn.as_ref().and_then(|d| d.engine.error()) {
            return Some(e);
        }
        if let Some(e) = self.counts.error.lock().ok().and_then(|e| e.clone()) {
            return Some(e);
        }
        match self.health.as_ref().map(|h| h.borrow().state.clone()) {
            Some(PushState::Reconnecting { reason, .. }) => Some(reason),
            _ => None,
        }
    }
}

/// Go live's sender to the server, when streaming: its runtime, health and stop.
#[allow(clippy::type_complexity)]
fn sender(
    send_to: Option<Send<'_>>,
    target: Option<Target>,
    hub: &Arc<Hub>,
) -> Result<
    (
        Option<tokio::runtime::Runtime>,
        Option<tokio::sync::watch::Receiver<Health>>,
        Option<tokio::sync::watch::Sender<bool>>,
    ),
    String,
> {
    let (Some(d), Some(target)) = (send_to, target) else { return Ok((None, None, None)) };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("ferrocast-own-stream")
        .build()
        .map_err(|e| e.to_string())?;
    let retry = match d.reconnect {
        Some((_, every)) => Retry { first: Duration::from_secs(u64::from(every.max(1))), ..Retry::default() },
        None => Retry::default(),
    };
    let dest = Destination { id: "studio".into(), target, key: d.key.to_string(), retry, extra_audio: false };
    let (health_tx, h) =
        tokio::sync::watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
    let (sp, stop_rx) = tokio::sync::watch::channel(false);
    rt.spawn(push::run(dest, hub.clone(), health_tx, stop_rx));
    Ok((Some(rt), Some(h), Some(sp)))
}

impl Drop for OwnStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // libobs stops handing frames over; our engine closes its output (encoders flushed).
        self.feed = None;
        if let Some(d) = self.drawn.take() {
            if let Err(e) = d.engine.stop_output(d.output) {
                tracing::warn!("Ferrocast's own engine didn't close its output cleanly: {e}");
            }
            if let Some(hub) = self.hub.take() {
                hub.end();
            }
        }
        // A moment for the last packets to leave, then the sender stops.
        if let Some(sp) = self.stop_push.take() {
            std::thread::sleep(Duration::from_millis(300));
            let _ = sp.send(true);
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EncodeSettings;

    fn settings(id: &str) -> StreamSettings {
        StreamSettings {
            encode: EncodeSettings::with_encoder(id),
            reconnect: None,
            delay_s: 0,
            vod_track: None,
            own_encoder: true,
        }
    }

    #[test]
    fn a_drawn_stream_follows_the_studios_transition() {
        let doc = serde_json::json!({
            "current_transition": "Swoosh",
            "transition_duration": 700,
            "transitions": [{ "name": "Swoosh", "id": "cut_transition" }],
        });
        assert_eq!(transition_of(&doc), (TransitionKind::Cut, 0));
        let fade = serde_json::json!({ "current_transition": "Fade", "transition_duration": 500 });
        assert_eq!(transition_of(&fade), (TransitionKind::Fade, 500));
        // Only which scene is on air changed: not a reopen.
        let a = serde_json::json!({ "current_program_scene": "A", "current_scene": "A", "sources": [1] });
        let b = serde_json::json!({ "current_program_scene": "B", "current_scene": "B", "sources": [1] });
        assert_eq!(without_on_air(&a), without_on_air(&b));
    }

    #[test]
    fn a_delayed_stream_holds_back_then_sends_the_rest_at_the_end() {
        let counts = Arc::new(Counts::default());
        let mut out = Delayed {
            hub: Hub::new(),
            counts: counts.clone(),
            delay: Duration::from_millis(200),
            held: std::collections::VecDeque::new(),
        };
        let sent = || counts.bytes.load(Ordering::Relaxed);
        out.send(Tag::Video { ms: 0, body: vec![0; 10] });
        assert_eq!(sent(), 0, "held back");
        std::thread::sleep(Duration::from_millis(250));
        out.send(Tag::Audio { ms: 250, body: vec![0; 5] });
        assert_eq!(sent(), 10, "the first goes once it's 200 ms old; the second waits");
        drop(out);
        assert_eq!(sent(), 15, "ending sends what's left");
    }

    #[test]
    fn only_what_ours_does_goes_through_ours() {
        let hw = "com.apple.videotoolbox.videoencoder.ave.avc";
        if cfg!(target_os = "macos") {
            assert_eq!(unsupported(&settings(hw), true), None);
            assert_eq!(unsupported(&settings("com.apple.videotoolbox.videoencoder.ave.hevc"), true), None);
        }
        assert!(unsupported(&settings(hw), false).is_some(), "vertical");
        // x264 runs through FFmpeg, wherever FFmpeg is.
        assert_eq!(unsupported(&settings("obs_x264"), true).is_none(), ferrocast_av::ffenc::available());
        assert!(unsupported(&settings("av1_texture_amf"), true).is_some());
        let mut extra = settings(hw);
        extra.encode.extra.insert("preset".into(), serde_json::json!("fast"));
        assert!(unsupported(&extra, true).is_some());
        assert!(unsupported(&StreamSettings { vod_track: Some(2), ..settings(hw) }, true).is_some());
    }

    #[test]
    fn recordings_ours_can_write() {
        let plan = RecordPlan {
            encode: EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc"),
            ..RecordPlan::default()
        };
        if cfg!(target_os = "macos") {
            assert_eq!(unsupported_recording(&plan), None);
        }
        assert!(unsupported_recording(&RecordPlan { format: "flv".into(), ..plan.clone() }).is_some());
        if cfg!(target_os = "macos") {
            assert_eq!(unsupported_recording(&RecordPlan { format: "mkv".into(), ..plan.clone() }), None);
        }
        assert!(
            unsupported_recording(&RecordPlan { split_minutes: 10, ..plan.clone() }).is_none()
                == cfg!(target_os = "macos")
        );
        // Recordings default to constant quality: ours does it on the Mac.
        let mut cq = plan.clone();
        cq.encode.quality = Some(16);
        assert_eq!(unsupported_recording(&cq).is_none(), cfg!(target_os = "macos"));
        assert_eq!(rate_of(&cq.encode), RateControl::Quality(0.69));
        cq.encode.quality = None;
        assert_eq!(rate_of(&cq.encode), RateControl::Cbr);
        // Lossless: x264 at rate factor 0.
        cq.encode.video_encoder = "obs_x264".into();
        cq.encode.quality = Some(0);
        assert_eq!(rate_of(&cq.encode), RateControl::Crf(0));
    }
}
