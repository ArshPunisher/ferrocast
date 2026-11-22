//! The Media source on macOS: plays video or sound files via AVFoundation's AVAssetReader
//! (H.264, HEVC, ProRes, AAC…), falling back to FFmpeg (`ffsrc`) when AVFoundation cannot open
//! or read the container/codec (MKV, WebM/VP9+alpha, OGG/Opus/FLAC…). A reader thread decodes a
//! little ahead and stamps every frame and sound piece with the host time it plays at, from the
//! file's own timestamps, so picture and sound stay together.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderOutput, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeRange, kCMTimePositiveInfinity};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use super::host_ns;
pub use crate::ffsrc::{Control, MediaOptions, MediaState};
use crate::ffsrc::{SpeedResampler, schedule_at};
use crate::{AvError, Picture};

/// How far ahead the reader decodes.
const AHEAD_NS: u64 = 300_000_000;
/// Frames kept waiting at most (a stalled engine can't grow memory).
const MAX_FRAMES: usize = 12;
/// Byte budget for buffered CPU BGRA frames (~64 MB caps 4K to 2 frames instead of ~400 MB).
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_SOUND: usize = 400;
/// Sound is handed to the mixer when it is this close to playing, not 300 ms before: what the
/// mixer already holds still plays after a pause, a seek or a cut to another scene (it can't be
/// taken back), so anything held here is what a pause can still stop.
const SOUND_HOLD_NS: u64 = 40_000_000;

#[derive(Debug, Default)]
struct CmdState {
    /// What the controls asked for, in order, for the reader to execute.
    queue: VecDeque<Control>,
    /// User-visible playback state mirror (updated immediately on control calls and on reader EOF/error).
    paused: bool,
    stopped: bool,
    ended: bool,
}

#[derive(Debug, Default)]
struct Shared {
    /// Frames waiting: the picture, the host time it shows at, and its place in the file (ns).
    frames: Mutex<VecDeque<(Picture, u64, u64)>>,
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    error: Mutex<Option<String>>,
    cmd: Mutex<CmdState>,
    wake: Condvar,
    /// Where it is in the file (ns): the last frame shown, or the paused/seeked place.
    position: AtomicU64,
}

#[derive(Debug)]
enum Backend {
    Apple {
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    },
    /// Fallback when AVFoundation cannot open or read the container/codec (MKV, WebM/VP9+alpha,
    /// OGG/Opus/FLAC, FLV, AVI, or explicit CPU decode).
    Ffmpeg(crate::ffsrc::Media),
}

/// A playing file; dropping it stops.
#[derive(Debug)]
pub struct Media {
    backend: Backend,
    pub has_video: bool,
    pub has_audio: bool,
    pub duration_ns: u64,
}

fn settings(keys: &[&str], values: Vec<Retained<NSNumber>>) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = keys.iter().map(|k| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&AnyObject> = values.iter().map(|v| -> &AnyObject { v.as_ref() }).collect();
    NSDictionary::from_slices(&key_refs, &value_refs)
}

/// A reader for one pass over the file, with its video and audio outputs.
struct Pass {
    reader: Retained<AVAssetReader>,
    video: Option<Retained<AVAssetReaderTrackOutput>>,
    audio: Option<Retained<AVAssetReaderTrackOutput>>,
}

fn open_pass(asset: &AVURLAsset, from_ns: u64) -> Result<Pass, String> {
    // SAFETY: AVFoundation objects made and used on this (the reader's) thread; the settings
    // dictionaries hold NSNumbers under AVFoundation's documented keys; `kCMTimePositiveInfinity`
    // is a CoreMedia constant static.
    unsafe {
        let reader =
            AVAssetReader::assetReaderWithAsset_error(asset).map_err(|e| e.localizedDescription().to_string())?;
        #[allow(deprecated)]
        let vt = AVMediaTypeVideo.map(|k| asset.tracksWithMediaType(k));
        #[allow(deprecated)]
        let at = AVMediaTypeAudio.map(|k| asset.tracksWithMediaType(k));
        let video = match vt.and_then(|t| t.firstObject()) {
            Some(track) => {
                let s = settings(&["PixelFormatType"], vec![NSNumber::new_u32(u32::from_be_bytes(*b"BGRA"))]);
                let out = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&s));
                out.setAlwaysCopiesSampleData(false);
                if reader.canAddOutput(&out) {
                    reader.addOutput(&out);
                    Some(out)
                } else {
                    None
                }
            }
            None => None,
        };
        let audio = match at.and_then(|t| t.firstObject()) {
            Some(track) => {
                let s = settings(
                    &[
                        "AVFormatIDKey",
                        "AVSampleRateKey",
                        "AVNumberOfChannelsKey",
                        "AVLinearPCMIsFloatKey",
                        "AVLinearPCMBitDepthKey",
                        "AVLinearPCMIsNonInterleaved",
                    ],
                    vec![
                        NSNumber::new_u32(u32::from_be_bytes(*b"lpcm")),
                        NSNumber::new_f64(48_000.0),
                        NSNumber::new_u32(2),
                        NSNumber::new_bool(true),
                        NSNumber::new_u32(32),
                        NSNumber::new_bool(false),
                    ],
                );
                let out = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&s));
                out.setAlwaysCopiesSampleData(false);
                if reader.canAddOutput(&out) {
                    reader.addOutput(&out);
                    Some(out)
                } else {
                    None
                }
            }
            None => None,
        };
        if video.is_none() && audio.is_none() {
            return Err("it has no picture or sound this Mac can play".into());
        }
        // From a place in the file (play after a pause, a seek): the rest of it.
        if from_ns > 0 {
            let start = CMTime::new(i64::try_from(from_ns / 1000).unwrap_or(i64::MAX), 1_000_000);
            reader.setTimeRange(CMTimeRange { start, duration: kCMTimePositiveInfinity });
        }
        if !reader.startReading() {
            return Err(reader
                .error()
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "it couldn't be read".into()));
        }
        Ok(Pass { reader, video, audio })
    }
}

/// Signed sample PTS in nanoseconds (preserves negative edit-list pre-roll timestamps).
fn pts_signed_ns(s: &CMSampleBuffer) -> i128 {
    // SAFETY: reading a live sample buffer's time.
    let t = unsafe { s.presentation_time_stamp() };
    if t.timescale <= 0 { 0 } else { i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale) }
}

fn next(out: &AVAssetReaderTrackOutput) -> Option<Retained<CMSampleBuffer>> {
    let out: &AVAssetReaderOutput = out;
    // SAFETY: the output belongs to a reading reader on this thread.
    unsafe { out.copyNextSampleBuffer() }
}

fn sound_of(s: &CMSampleBuffer) -> Option<Vec<f32>> {
    // SAFETY: a live sample buffer; its data is interleaved 32-bit float (the settings asked).
    unsafe {
        let block = s.data_buffer()?;
        let len = block.data_length();
        let mut bytes = vec![0u8; len];
        let dst = std::ptr::NonNull::new(bytes.as_mut_ptr().cast())?;
        if block.copy_data_bytes(0, len, dst) != 0 {
            return None;
        }
        Some(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_ne_bytes(*b)).collect())
    }
}

/// The sound in `q` that plays no later than `hold` after `now`, taken out in order; what plays
/// later stays (and goes if the file is paused, seeked or stopped meanwhile).
fn take_due(q: &mut VecDeque<(Vec<f32>, u64)>, now: u64, hold: u64) -> Vec<(Vec<f32>, u64)> {
    let mut out = Vec::new();
    while q.front().is_some_and(|(_, at)| *at <= now.saturating_add(hold)) {
        out.extend(q.pop_front());
    }
    out
}

fn frame_queue_has_room(q: &VecDeque<(Picture, u64, u64)>) -> bool {
    if q.len() >= MAX_FRAMES {
        return false;
    }
    if q.len() < 2 {
        return true;
    }
    q.iter().map(|(p, ..)| p.rgba.len()).sum::<usize>() < MAX_FRAME_BYTES
}

impl Shared {
    fn new(start_stopped: bool) -> Self {
        Self {
            cmd: Mutex::new(CmdState {
                queue: VecDeque::new(),
                paused: start_stopped,
                stopped: start_stopped,
                ended: false,
            }),
            ..Self::default()
        }
    }

    /// A file that can't be played ends here, with the reason (a playlist moves on to the next
    /// file; it waits for a control, and Play or Restart try it again).
    fn give_up(&self, why: &str) {
        if let Ok(mut err) = self.error.lock() {
            *err = Some(format!("the media couldn't be played ({why})"));
        }
        if let Ok(mut q) = self.frames.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.sound.lock() {
            q.clear();
        }
        if let Ok(mut st) = self.cmd.lock()
            && st.queue.is_empty()
        {
            st.ended = true;
            st.paused = false;
            st.stopped = false;
        }
    }

    /// Decoded to the end and everything decoded has been taken: only then is a file over (the
    /// reader runs 300 ms ahead, and a playlist that moved on at the reader's end lost the last
    /// 300 ms of the file's picture and sound).
    fn finished(&self) -> bool {
        let ended = self.cmd.lock().map(|st| st.ended).unwrap_or(false);
        ended
            && self.frames.lock().map(|q| q.is_empty()).unwrap_or(true)
            && self.sound.lock().map(|q| q.is_empty()).unwrap_or(true)
    }
}

/// Probes whether AVAssetReader can actually open and decode the first sample of `asset`.
/// Catches containers AVFoundation recognizes structurally (e.g. MP4/MOV carrying VP9 or Opus)
/// that fail as soon as `startReading()` or `copyNextSampleBuffer()` runs, so we fall back to FFmpeg.
fn can_read_with_avfoundation(asset: &AVURLAsset) -> Result<(), String> {
    let pass = open_pass(asset, 0)?;
    let got_sample = pass.video.as_ref().and_then(|o| next(o)).is_some()
        || (pass.video.is_none() && pass.audio.as_ref().and_then(|o| next(o)).is_some());
    // SAFETY: `pass.reader` was created and used solely on this thread.
    let status = unsafe { pass.reader.status() };
    let why = if status == AVAssetReaderStatus::Failed {
        // SAFETY: as above.
        unsafe { pass.reader.error() }.map(|e| e.localizedDescription().to_string())
    } else {
        None
    };
    // SAFETY: as above.
    unsafe { pass.reader.cancelReading() };
    if status == AVAssetReaderStatus::Failed {
        return Err(why.unwrap_or_else(|| "AVFoundation failed to read the file".into()));
    }
    if !got_sample {
        return Err("AVFoundation decoded no samples".into());
    }
    Ok(())
}

impl Media {
    /// Opens a file and starts playing it (its first frame shows about 100 ms from now).
    pub fn open(path: &Path, looping: bool) -> Result<Self, AvError> {
        Self::open_with(path, MediaOptions::looping(looping), false)
    }

    /// Opens a file with full playback options; when `start_stopped` is true, stays idle at 0
    /// until `Control::Play` or `Control::Restart`. Falls back to FFmpeg (`ffsrc`) when
    /// AVFoundation cannot open or read the container/codec (MKV, WebM/VP9+alpha, OGG/Opus…).
    pub fn open_with(path: &Path, options: MediaOptions, start_stopped: bool) -> Result<Self, AvError> {
        if !path.is_file() {
            return Err(AvError::Source(format!("{} isn't there", path.display())));
        }
        // When software decode is explicitly requested, route through FFmpeg if available.
        if !options.hw_decode {
            match crate::ffsrc::Media::open_for_card_with(path, options, start_stopped) {
                Ok(ff) => {
                    return Ok(Self {
                        has_video: ff.has_video,
                        has_audio: ff.has_audio,
                        duration_ns: ff.duration_ns,
                        backend: Backend::Ffmpeg(ff),
                    });
                }
                Err(e) => {
                    tracing::info!(file = %path.display(), error = %e, "software FFmpeg decode unavailable, trying AVFoundation");
                }
            }
        }
        let url_text = path.to_string_lossy().to_string();
        // Probe on this thread inside an autorelease pool so playlist item opens on raw Rust
        // threads never leak Objective-C temporaries.
        // SAFETY: AVFoundation objects made and read on this thread.
        let av_probe: Result<(bool, bool, u64), String> = objc2::rc::autoreleasepool(|_| unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&url_text));
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            #[allow(deprecated)]
            let v = AVMediaTypeVideo.is_some_and(|k| asset.tracksWithMediaType(k).count() > 0);
            #[allow(deprecated)]
            let a = AVMediaTypeAudio.is_some_and(|k| asset.tracksWithMediaType(k).count() > 0);
            let d = asset.duration();
            let dur = if d.timescale > 0 && d.value > 0 {
                (i128::from(d.value) * 1_000_000_000 / i128::from(d.timescale)) as u64
            } else {
                0
            };
            if !v && !a {
                Err("no playable AVFoundation tracks".into())
            } else {
                can_read_with_avfoundation(&asset).map(|()| (v, a, dur))
            }
        });
        let (has_video, has_audio, duration_ns) = match av_probe {
            Ok(info) => info,
            Err(av_why) => match crate::ffsrc::Media::open_for_card_with(path, options, start_stopped) {
                Ok(ff) => {
                    tracing::info!(
                        file = %path.display(),
                        reason = %av_why,
                        "AVFoundation couldn't open media; playing via FFmpeg fallback"
                    );
                    return Ok(Self {
                        has_video: ff.has_video,
                        has_audio: ff.has_audio,
                        duration_ns: ff.duration_ns,
                        backend: Backend::Ffmpeg(ff),
                    });
                }
                Err(ff_err) => {
                    return Err(AvError::Source(format!(
                        "{} couldn't be played ({av_why}; FFmpeg: {ff_err})",
                        path.display()
                    )));
                }
            },
        };
        let shared = Arc::new(Shared::new(start_stopped));
        let stop = Arc::new(AtomicBool::new(false));
        let speed = options.speed();
        let looping = options.looping;
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-media".into())
                .spawn(move || read(&url_text, looping, speed, duration_ns, start_stopped, &shared, &stop))
                .map_err(|e| AvError::Source(format!("the media couldn't start ({e})")))?
        };
        Ok(Self { backend: Backend::Apple { shared, stop, thread: Some(thread) }, has_video, has_audio, duration_ns })
    }

    /// True when playing through the FFmpeg fallback backend rather than AVFoundation.
    pub fn uses_ffmpeg(&self) -> bool {
        matches!(self.backend, Backend::Ffmpeg(_))
    }

    /// The newest NV12 picture when playing through the FFmpeg fallback on an opaque video.
    pub fn frame_nv12(&self) -> Option<crate::ffsrc::Nv12Picture> {
        match &self.backend {
            Backend::Apple { .. } => None,
            Backend::Ffmpeg(ff) => ff.frame_nv12(),
        }
    }

    /// The frame to show at `now_ns` (the newest whose time has come; older ones are dropped).
    pub fn frame_at(&self, now_ns: u64) -> Option<Picture> {
        match &self.backend {
            Backend::Apple { shared, .. } => {
                let mut q = shared.frames.lock().ok()?;
                let mut chosen = None;
                while q.front().is_some_and(|(_, t, _)| *t <= now_ns) {
                    chosen = q.pop_front().map(|(p, _, pts)| {
                        shared.position.store(pts, Ordering::Relaxed);
                        p
                    });
                }
                chosen
            }
            Backend::Ffmpeg(ff) => ff.frame_at(now_ns),
        }
    }

    /// Plays, pauses, restarts, stops or seeks (non-blocking; wakes the reader immediately).
    pub fn control(&self, c: Control) {
        match &self.backend {
            Backend::Apple { shared, .. } => {
                if let Ok(mut st) = shared.cmd.lock() {
                    // Coalesce back-to-back seeks so rapid scrubber drags jump straight to the latest target.
                    if matches!(c, Control::Seek(_)) && matches!(st.queue.back(), Some(Control::Seek(_))) {
                        st.queue.pop_back();
                    }
                    st.queue.push_back(c);
                    match c {
                        Control::Play | Control::Restart => {
                            st.paused = false;
                            st.stopped = false;
                            st.ended = false;
                            if matches!(c, Control::Restart) {
                                shared.position.store(0, Ordering::Relaxed);
                            }
                        }
                        Control::Pause => {
                            st.paused = true;
                        }
                        Control::Stop => {
                            st.paused = true;
                            st.stopped = true;
                            st.ended = false;
                            shared.position.store(0, Ordering::Relaxed);
                        }
                        Control::Seek(ns) => {
                            let clamped = if self.duration_ns > 0 { ns.min(self.duration_ns) } else { ns };
                            shared.position.store(clamped, Ordering::Relaxed);
                            if st.ended || st.stopped {
                                st.ended = false;
                                st.stopped = false;
                                st.paused = true;
                            }
                        }
                    }
                    shared.wake.notify_all();
                }
            }
            Backend::Ffmpeg(ff) => ff.control(c),
        }
    }

    /// Where it stands, and where it is in the file (ns).
    pub fn state(&self) -> (MediaState, u64) {
        match &self.backend {
            Backend::Apple { shared, .. } => {
                let (ended, stopped, paused) =
                    shared.cmd.lock().map(|st| (st.ended, st.stopped, st.paused)).unwrap_or((false, false, false));
                let state = if ended && shared.finished() {
                    MediaState::Ended
                } else if stopped {
                    MediaState::Stopped
                } else if paused {
                    MediaState::Paused
                } else {
                    MediaState::Playing
                };
                (state, shared.position.load(Ordering::Relaxed))
            }
            Backend::Ffmpeg(ff) => ff.state(),
        }
    }

    /// Sound decoded since the last call that is about to play, each piece with the host time it
    /// plays at (what plays later waits here, where a pause or a seek can still drop it).
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        match &self.backend {
            Backend::Apple { shared, .. } => {
                shared.sound.lock().map(|mut q| take_due(&mut q, host_ns(), SOUND_HOLD_NS)).unwrap_or_default()
            }
            Backend::Ffmpeg(ff) => ff.take_sound(),
        }
    }

    /// Played to the end (not looping), or it couldn't be played (`error` says why), and
    /// everything decoded has been taken.
    pub fn ended(&self) -> bool {
        match &self.backend {
            Backend::Apple { shared, .. } => shared.finished(),
            Backend::Ffmpeg(ff) => ff.ended(),
        }
    }

    pub fn error(&self) -> Option<String> {
        match &self.backend {
            Backend::Apple { shared, .. } => shared.error.lock().ok().and_then(|e| e.clone()),
            Backend::Ffmpeg(ff) => ff.error(),
        }
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        if let Backend::Apple { shared, stop, thread } = &mut self.backend {
            stop.store(true, Ordering::Release);
            if let Ok(_st) = shared.cmd.lock() {
                shared.wake.notify_all();
            }
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
        }
    }
}

/// The reader thread: one pass after another (looping), each stamped from its signed anchor time;
/// the controls stop a pass and start another from the place asked for.
fn read(
    url_text: &str,
    looping: bool,
    speed_percent: u32,
    duration_ns: u64,
    start_stopped: bool,
    shared: &Shared,
    stop: &AtomicBool,
) {
    // SAFETY: AVFoundation objects made and used on this thread only; wrap asset creation, each
    // pass setup, and each decoded sample in an autorelease pool so looping media never leaks
    // Objective-C temporaries on this raw Rust thread.
    let asset = objc2::rc::autoreleasepool(|_| unsafe {
        AVURLAsset::URLAssetWithURL_options(&NSURL::fileURLWithPath(&NSString::from_str(url_text)), None)
    });
    let clear = || {
        if let Ok(mut q) = shared.frames.lock() {
            q.clear();
        }
        if let Ok(mut q) = shared.sound.lock() {
            q.clear();
        }
    };
    let clamp = |n: u64| if duration_ns > 0 { n.min(duration_ns) } else { n };
    let pop_cmd = || shared.cmd.lock().ok().and_then(|mut st| st.queue.pop_front());
    let mut reader_state = if start_stopped { MediaState::Stopped } else { MediaState::Playing };
    let mut resampler = SpeedResampler::new(speed_percent);
    // Where the next pass starts in the file, and the host time its start plays at when it follows
    // the last pass on (a seamless loop).
    let (mut from, mut follows): (u64, Option<u64>) = (0, None);
    'passes: loop {
        // Paused, stopped or ended: wait for a control without spinning.
        while reader_state != MediaState::Playing {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let cmd = {
                let Ok(mut st) = shared.cmd.lock() else { return };
                while st.queue.is_empty() && !stop.load(Ordering::Acquire) {
                    let Ok((guard, _)) = shared.wake.wait_timeout(st, Duration::from_millis(50)) else { return };
                    st = guard;
                }
                st.queue.pop_front()
            };
            match cmd {
                Some(Control::Play) => {
                    if matches!(reader_state, MediaState::Stopped | MediaState::Ended) {
                        from = 0;
                    }
                    reader_state = MediaState::Playing;
                }
                Some(Control::Restart) => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    reader_state = MediaState::Playing;
                }
                Some(Control::Stop) => {
                    from = 0;
                    shared.position.store(0, Ordering::Relaxed);
                    clear();
                    reader_state = MediaState::Stopped;
                }
                Some(Control::Pause) => {
                    if reader_state != MediaState::Stopped {
                        reader_state = MediaState::Paused;
                    }
                }
                Some(Control::Seek(n)) => {
                    from = clamp(n);
                    shared.position.store(from, Ordering::Relaxed);
                    clear();
                    if matches!(reader_state, MediaState::Ended | MediaState::Stopped) {
                        reader_state = MediaState::Paused;
                    }
                }
                None => {}
            }
            follows = None;
        }
        let pass = match objc2::rc::autoreleasepool(|_| open_pass(&asset, from)) {
            Ok(p) => p,
            Err(e) => {
                shared.give_up(&e);
                reader_state = MediaState::Ended;
                follows = None;
                continue 'passes;
            }
        };
        if let Ok(mut err) = shared.error.lock() {
            *err = None;
        }
        resampler.reset();
        let base_host_ns = follows.unwrap_or_else(|| host_ns() + 100_000_000);
        let (mut v_next, mut a_next) = objc2::rc::autoreleasepool(|_| {
            (pass.video.as_ref().and_then(|o| next(o)), pass.audio.as_ref().and_then(|o| next(o)))
        });
        // Anchor on the first sample's PTS when playing from 0 so trimmed files with non-zero
        // container start_pts (e.g. 5 s) start immediately rather than waiting 5 s.
        let base_pts_ns: i128 = if from == 0 {
            match (v_next.as_ref().map(|s| pts_signed_ns(s)), a_next.as_ref().map(|s| pts_signed_ns(s))) {
                (Some(v), Some(a)) => v.min(a),
                (Some(v), None) => v,
                (None, Some(a)) => a,
                (None, None) => 0,
            }
        } else {
            i128::from(from)
        };
        let mut last_end: i128 = base_pts_ns;
        while (v_next.is_some() || a_next.is_some()) && !stop.load(Ordering::Acquire) {
            if let Some(c) = pop_cmd() {
                let now_at = shared.position.load(Ordering::Relaxed);
                match c {
                    Control::Play => {}
                    Control::Pause => {
                        from = now_at;
                        reader_state = MediaState::Paused;
                    }
                    Control::Seek(n) => {
                        from = clamp(n);
                        shared.position.store(from, Ordering::Relaxed);
                    }
                    Control::Restart => {
                        from = 0;
                        shared.position.store(0, Ordering::Relaxed);
                    }
                    Control::Stop => {
                        from = 0;
                        shared.position.store(0, Ordering::Relaxed);
                        reader_state = MediaState::Stopped;
                    }
                }
                if c != Control::Play {
                    clear();
                    // SAFETY: the reader belongs to this thread.
                    unsafe { pass.reader.cancelReading() };
                    follows = None;
                    continue 'passes;
                }
            }
            let horizon = host_ns() + AHEAD_NS;
            let moved = objc2::rc::autoreleasepool(|_| {
                let mut moved = false;
                if let Some(s) = &v_next {
                    let pts = pts_signed_ns(s);
                    let at = schedule_at(base_host_ns, base_pts_ns, pts, speed_percent);
                    let room = shared.frames.lock().map(|q| frame_queue_has_room(&q)).unwrap_or(false);
                    if at <= horizon && room {
                        // SAFETY: the decoded frame's pixel buffer is live with its sample buffer.
                        if let Some(img) = unsafe { s.image_buffer() }
                            && let Some(mut p) = super::screen::copy_bgra(&img)
                            && let Ok(mut q) = shared.frames.lock()
                        {
                            let pos_ns = pts.max(0) as u64;
                            p.premultiplied = false;
                            q.push_back((p, at, pos_ns));
                        }
                        last_end = last_end.max(pts);
                        v_next = pass.video.as_ref().and_then(|o| next(o));
                        moved = true;
                    }
                }
                if let Some(s) = &a_next {
                    let pts = pts_signed_ns(s);
                    let at = schedule_at(base_host_ns, base_pts_ns, pts, speed_percent);
                    let room = shared.sound.lock().map(|q| q.len() < MAX_SOUND).unwrap_or(false);
                    if at <= horizon && room {
                        if let Some(raw) = sound_of(s) {
                            let orig_frames = (raw.len() / 2) as i128;
                            last_end = last_end.max(pts + orig_frames * 1_000_000_000 / 48_000);
                            // A file with no picture keeps its place by its sound.
                            if pass.video.is_none() {
                                shared.position.store(pts.max(0) as u64, Ordering::Relaxed);
                            }
                            let pcm = resampler.process(raw);
                            if !pcm.is_empty()
                                && let Ok(mut q) = shared.sound.lock()
                            {
                                q.push_back((pcm, at));
                            }
                        }
                        a_next = pass.audio.as_ref().and_then(|o| next(o));
                        moved = true;
                    }
                }
                moved
            });
            if !moved
                && let Ok(st) = shared.cmd.lock()
                && st.queue.is_empty()
                && !stop.load(Ordering::Acquire)
            {
                let _ = shared.wake.wait_timeout(st, Duration::from_millis(5));
            }
        }
        // A reader that failed part way ends the pass as the end of the file does; say why.
        let failed = objc2::rc::autoreleasepool(|_| {
            // SAFETY: the reader belongs to this thread.
            let failed = unsafe { pass.reader.status() == AVAssetReaderStatus::Failed };
            if failed
                && !stop.load(Ordering::Acquire)
                && let Ok(mut err) = shared.error.lock()
            {
                // SAFETY: as above.
                let why = unsafe { pass.reader.error() }.map(|e| e.localizedDescription().to_string());
                *err = Some(format!(
                    "the media stopped being readable ({})",
                    why.unwrap_or_else(|| "no reason given".into())
                ));
            }
            // SAFETY: the reader belongs to this thread.
            unsafe { pass.reader.cancelReading() };
            failed
        });
        if stop.load(Ordering::Acquire) {
            return;
        }
        if !looping || failed {
            reader_state = MediaState::Ended;
            if let Ok(mut st) = shared.cmd.lock()
                && st.queue.is_empty()
            {
                st.ended = true;
                st.paused = false;
                st.stopped = false;
            }
            continue;
        }
        // The next pass starts where this one ends (matching container duration or actual decoded tail).
        let pass_end = if duration_ns > 0 { i128::from(duration_ns).max(last_end) } else { last_end };
        follows = Some(schedule_at(base_host_ns, base_pts_ns, pass_end, speed_percent));
        from = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 s test video (a counter on screen, a tone), made by FFmpeg; None without it.
    fn clip() -> Option<std::path::PathBuf> {
        let f = std::env::temp_dir().join("ferrocore-media-controls.mp4");
        if f.exists() {
            return Some(f);
        }
        // Made under a name of its own and renamed whole: tests run side by side, and one that
        // found the file half written (under load ffmpeg takes a while) played a short clip.
        let part = std::env::temp_dir().join(format!(
            "ferrocore-media-controls-{}-{:?}.part.mp4",
            std::process::id(),
            std::thread::current().id()
        ));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=4"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&part)
            .status()
            .is_ok_and(|s| s.success());
        if ok && std::fs::rename(&part, &f).is_ok() {
            return Some(f);
        }
        let _ = std::fs::remove_file(&part);
        f.exists().then_some(f)
    }

    /// Shows pictures as a player would (asking for the one due now every 5 ms) until `done`,
    /// however long a loaded machine takes: it waits on what the media reports, not on the clock.
    /// Panics with `what` (and the state) after 60 s.
    fn wait_for(m: &Media, what: &str, mut done: impl FnMut(&Media) -> bool) {
        let started = std::time::Instant::now();
        while !done(m) {
            assert!(started.elapsed() < Duration::from_secs(60), "gave up waiting for {what}: {:?}", m.state());
            let _ = m.frame_at(host_ns());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Seeking past system uptime (`from > host_ns`) must schedule frames at `host_ns + 100 ms`,
    /// never saturating to 0 and freezing for minutes (M8-03).
    #[test]
    fn schedule_time_does_not_freeze_when_seeking_past_uptime() {
        let s = |sec: i128| sec * 1_000_000_000;
        let host = 10_000_000_000u64;
        let base_host = host + 100_000_000;
        assert_eq!(schedule_at(base_host, s(3600), s(3600), 100), base_host);
        assert_eq!(schedule_at(base_host, s(3600), s(3601), 100), base_host + 1_000_000_000);
        // Keyframe slightly before seek target schedules slightly before base_host, without underflow.
        assert_eq!(schedule_at(base_host, s(3600), s(3600) - 200_000_000, 100), base_host - 200_000_000);
        // Speed scaling: 200% halves intervals, 50% doubles them.
        assert_eq!(schedule_at(base_host, s(3600), s(3602), 200), base_host + 1_000_000_000);
        assert_eq!(schedule_at(base_host, s(3600), s(3601), 50), base_host + 2_000_000_000);
    }

    #[test]
    fn speed_resampler_scales_pcm_continuously() {
        let mut r200 = SpeedResampler::new(200);
        let chunk: Vec<f32> = (0..200).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let a = r200.process(chunk.clone());
        let b = r200.process(chunk.clone());
        assert_eq!((a.len() + b.len()) / 2, 200, "200% speed halves sample count across chunks");

        let mut r50 = SpeedResampler::new(50);
        let a = r50.process(chunk.clone());
        let b = r50.process(chunk);
        // 1-sample lookahead holds the last frame of chunk 1 to interpolate into chunk 2.
        assert_eq!(b.len() / 2, 400, "steady-state 50% speed doubles chunk sample count");
        assert_eq!((a.len() + b.len()) / 2, 798);
        // Boundary continuity: b[0] is prev (199.0), b[1] is midpoint (199+0)/2 = 99.5, b[2] is 0.0.
        assert!((b[0] - 199.0).abs() < 1e-4 && (b[2] - 99.5).abs() < 1e-4 && b[4].abs() < 1e-4);
    }

    #[test]
    fn pause_seek_and_restart() {
        let Some(f) = clip() else { return };
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        let show = |ms: u64| {
            let end = std::time::Instant::now() + Duration::from_millis(ms);
            while std::time::Instant::now() < end {
                let _ = m.frame_at(host_ns());
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        // Each step waits for what it expects to see, not for a time to pass.
        wait_for(&m, "the first half second to play", |m| m.state().1 >= 300_000_000);
        assert_eq!(m.state().0, MediaState::Playing);
        m.control(Control::Pause);
        // Paused: pictures already due may still land, then it holds still.
        let mut last = (u64::MAX, std::time::Instant::now());
        wait_for(&m, "it to hold still", |m| {
            let (st, pos) = m.state();
            if pos != last.0 {
                last = (pos, std::time::Instant::now());
            }
            st == MediaState::Paused && last.1.elapsed() >= Duration::from_millis(200)
        });
        let at = m.state().1;
        assert!((300_000_000..2_500_000_000).contains(&at), "paused where it was, not run on: {at}");
        show(500);
        assert_eq!(m.state(), (MediaState::Paused, at), "still there");
        m.control(Control::Seek(2_500_000_000));
        m.control(Control::Play);
        wait_for(&m, "it to play on from 2.5 s", |m| {
            let (st, pos) = m.state();
            st == MediaState::Playing && pos >= 2_500_000_000
        });
        m.control(Control::Restart);
        // From the start again: a picture of the first second is shown (not the old place).
        wait_for(&m, "a picture from the start", |m| {
            let (_, pos) = m.state();
            pos > 0 && pos < 1_000_000_000
        });
        m.control(Control::Stop);
        wait_for(&m, "it to stop", |m| m.state() == (MediaState::Stopped, 0));
    }

    /// Stopping while paused, then playing, must restart from 0 (previously Stop was dropped in
    /// the paused loop and Play resumed from the old paused timestamp: M8-03).
    #[test]
    fn stop_while_paused_resets_to_zero_on_play() {
        let Some(f) = clip() else { return };
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        let show = |ms: u64| {
            let end = std::time::Instant::now() + Duration::from_millis(ms);
            while std::time::Instant::now() < end {
                let _ = m.frame_at(host_ns());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        m.control(Control::Seek(2_000_000_000));
        show(500);
        m.control(Control::Pause);
        show(150);
        assert!(m.state().1 >= 1_800_000_000, "paused past 2 s: {:?}", m.state());
        m.control(Control::Stop);
        m.control(Control::Play);
        show(450);
        let (state, pos) = m.state();
        assert_eq!(state, MediaState::Playing);
        assert!(pos < 1_000_000_000, "resumed from 0 after Stop while paused, not 2 s: {pos}");
        // Seeking while Stopped then playing must honour the seek target instead of resetting to 0.
        m.control(Control::Stop);
        show(100);
        m.control(Control::Seek(2_000_000_000));
        m.control(Control::Play);
        show(450);
        let (state, pos) = m.state();
        assert!(state == MediaState::Playing && pos >= 1_900_000_000, "Seek while Stopped then Play: {pos}");
    }

    /// A trimmed MP4 whose first sample PTS is 5 s must show its first frame promptly, not stall
    /// for 5 s waiting for PTS 5 s (M8-06).
    #[test]
    fn trimmed_clip_with_nonzero_start_pts_plays_immediately() {
        let f = std::env::temp_dir().join(format!("ferrocore-media-offset-{}.mp4", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=2"])
            .args(["-c:v", "libx264", "-output_ts_offset", "5"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            return;
        }
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        let started = std::time::Instant::now();
        let mut got = false;
        while started.elapsed() < Duration::from_millis(1200) {
            if m.frame_at(host_ns()).is_some() {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(m);
        let _ = std::fs::remove_file(&f);
        assert!(got, "first frame must arrive promptly even when start_pts is 5 s");
    }

    fn piece(at: u64) -> (Vec<f32>, u64) {
        (vec![0.0; 2048], at)
    }

    /// Only sound about to play is taken; the rest waits where a pause can still drop it.
    #[test]
    fn sound_is_taken_when_it_is_due_not_300_ms_ahead() {
        let ms = |m: u64| m * 1_000_000;
        let mut q: VecDeque<_> = (0..20).map(|i| piece(ms(1_000 + i * 21))).collect();
        // At 1,000 ms with a 40 ms hold: the pieces at 1,000 and 1,021 (1,042 plays after 1,040).
        let got = take_due(&mut q, ms(1_000), ms(40));
        assert_eq!(got.iter().map(|p| p.1).collect::<Vec<_>>(), vec![ms(1_000), ms(1_021)]);
        assert_eq!(q.len(), 18, "the rest waits");
        assert!(take_due(&mut q, ms(1_000), ms(40)).is_empty(), "nothing twice");
        // A pause drops what waits: nothing of it reaches the mixer later.
        q.clear();
        assert!(take_due(&mut q, ms(5_000), ms(40)).is_empty());
        // Everything late comes at once, in order.
        let mut q: VecDeque<_> = (0..5).map(|i| piece(ms(i * 21))).collect();
        assert_eq!(take_due(&mut q, ms(10_000), ms(40)).len(), 5);
    }

    /// A file is over when what was decoded has been taken, not when the reader (300 ms ahead of
    /// the screen) reaches the end.
    #[test]
    fn a_file_is_over_only_when_what_was_decoded_has_been_taken() {
        let sh = Shared::default();
        assert!(!sh.finished(), "still reading");
        sh.cmd.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
        assert!(sh.finished(), "read to the end, nothing waiting");
        sh.sound.lock().unwrap_or_else(|e| e.into_inner()).push_back(piece(1));
        assert!(!sh.finished(), "its last sound still waits");
        sh.sound.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert!(sh.finished());
    }

    /// Played as a playlist plays it (take what's due until `ended`), a file gives its whole
    /// sound: it used to end 300 ms early, with that much of the sound and picture still queued.
    #[test]
    fn a_playlist_style_reader_gets_the_whole_file() {
        let Some(f) = clip() else { return };
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        let mut sound_frames = 0usize;
        // Sound may come no earlier than SOUND_HOLD_NS before it plays. `now` is read after the
        // take, so a stall between reading the clock and taking (a loaded machine) can't make a
        // correct release look early; the most it hides is the length of the call itself.
        let mut most_ahead = 0u64;
        wait_for(&m, "the file to end", |m| {
            for (pcm, at) in m.take_sound() {
                sound_frames += pcm.len() / 2;
                most_ahead = most_ahead.max(at.saturating_sub(host_ns()));
            }
            m.ended()
        });
        assert!(m.ended(), "it ended");
        let secs = sound_frames as f64 / 48_000.0;
        assert!(secs > 3.85, "the whole 4 s of sound came, not the last 300 ms cut off: {secs:.2} s");
        // Which picture was last shown, not how many were: a reader that polls late skips
        // pictures that were already due, as a player does. The last of the 120 is at 3.97 s.
        let last = m.state().1;
        assert!(last >= 3_800_000_000, "the picture got to the end of the 4 s clip, not 300 ms short: {last} ns");
        assert!(most_ahead <= SOUND_HOLD_NS + 5_000_000, "sound came {} ms before it plays", most_ahead / 1_000_000);
    }

    /// A pass that can't open ends the file with its reason (it used to set the error and leave
    /// the file "playing" for good, and a playlist waited on it forever).
    #[test]
    fn a_file_that_cant_be_opened_is_over_and_says_why() {
        let sh = Shared::default();
        sh.frames.lock().unwrap_or_else(|e| e.into_inner()).push_back((Picture::solid(2, 2, [0, 0, 0, 255]), 1, 1));
        sh.sound.lock().unwrap_or_else(|e| e.into_inner()).push_back(piece(1));
        sh.give_up("it couldn't be read");
        assert!(sh.finished(), "over, with nothing waiting");
        let why = sh.error.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
        assert_eq!(why, "the media couldn't be played (it couldn't be read)");
    }

    /// A clip cut off part way (a download that stopped) ends instead of holding a playlist.
    #[test]
    fn a_cut_off_clip_ends() {
        let f = std::env::temp_dir().join(format!("ferrocore-media-cut-{}.mp4", std::process::id()));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=4"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .args(["-movflags", "+faststart"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            return;
        }
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(std::fs::write(&f, &bytes[..bytes.len() * 6 / 10]).is_ok());
        let Ok(m) = Media::open(&f, false) else {
            // Not even its header reads: said at once, nothing to hold.
            let _ = std::fs::remove_file(&f);
            return;
        };
        let started = std::time::Instant::now();
        while !m.ended() && started.elapsed() < Duration::from_secs(60) {
            let _ = m.frame_at(host_ns());
            let _ = m.take_sound();
            std::thread::sleep(Duration::from_millis(10));
        }
        let (ended, error) = (m.ended(), m.error());
        drop(m);
        let _ = std::fs::remove_file(&f);
        eprintln!("cut-off clip: ended {ended}, said {error:?}");
        assert!(ended, "a cut-off clip is over, so a playlist moves on");
    }

    fn ensure_ffmpeg() -> Option<std::path::PathBuf> {
        let p = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .map(Path::to_path_buf)
            .or_else(crate::ffenc::ffmpeg_path)?;
        crate::ffenc::set_ffmpeg(&p);
        Some(p)
    }

    /// MKV containers (including Ferrocast's own recordings) cannot be opened by AVURLAsset on
    /// macOS and must fall back seamlessly to `ffsrc` (M8-01).
    #[test]
    fn mkv_recording_falls_back_to_ffmpeg_and_plays() {
        let Some(ff) = ensure_ffmpeg() else { return };
        let f = std::env::temp_dir().join(format!("ferrocore-media-mkv-{}.mkv", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=1"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg generated MKV clip");
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.uses_ffmpeg(), "MKV must route through FFmpeg fallback on macOS");
        assert!(m.has_video && m.has_audio);
        let started = std::time::Instant::now();
        let (mut got_pic, mut got_snd) = (false, false);
        while !m.ended() && started.elapsed() < Duration::from_secs(60) {
            if m.frame_nv12().is_some() || m.frame_at(host_ns()).is_some() {
                got_pic = true;
            }
            if !m.take_sound().is_empty() {
                got_snd = true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ended = m.ended();
        drop(m);
        let _ = std::fs::remove_file(&f);
        assert!(got_pic && got_snd, "MKV fallback must produce both picture and sound");
        assert!(ended, "MKV fallback must finish cleanly at EOF");
    }

    /// Transparent VP9 WebM stingers fail in AVURLAsset on macOS and must fall back to FFmpeg
    /// preserving alpha=0 transparency (M8-01).
    #[test]
    fn webm_vp9_alpha_stinger_falls_back_and_preserves_alpha() {
        let Some(ff) = ensure_ffmpeg() else { return };
        let f = std::env::temp_dir().join(format!("ferrocore-media-stinger-{}.webm", std::process::id()));
        let ok = std::process::Command::new(&ff)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black@0.0:s=32x32:r=15:d=1,format=rgba",
                "-f",
                "lavfi",
                "-i",
                "color=c=0xff0000@1.0:s=32x32:r=15:d=1,format=rgba",
                "-filter_complex",
                "[0:v][1:v]hstack=inputs=2,format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
                "-auto-alt-ref",
                "0",
                "-deadline",
                "realtime",
            ])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = std::fs::remove_file(&f);
            return;
        }
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.uses_ffmpeg(), "WebM must route through FFmpeg fallback on macOS");
        let started = std::time::Instant::now();
        let mut pic = None;
        while started.elapsed() < Duration::from_secs(60) && pic.is_none() {
            assert!(m.frame_nv12().is_none(), "alpha WebM must deliver RGBA, not NV12");
            pic = m.frame_at(host_ns());
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(m);
        let _ = std::fs::remove_file(&f);
        let p = pic.unwrap_or_else(|| panic!("expected RGBA frame from WebM fallback"));
        let left = ((16 * 64 + 8) * 4) as usize;
        let right = ((16 * 64 + 48) * 4) as usize;
        assert_eq!(p.rgba[left + 3], 0, "left corner must be transparent: {:?}", &p.rgba[left..left + 4]);
        assert!(
            p.rgba[right + 3] > 240 && p.rgba[right] > 180,
            "right half must be opaque red: {:?}",
            &p.rgba[right..right + 4]
        );
    }
}
