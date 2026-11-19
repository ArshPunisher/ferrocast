//! The Media source on macOS: a video or sound file played by AVFoundation's AVAssetReader
//! (H.264, HEVC, ProRes, AAC… whatever macOS plays, hardware-decoded), looping or once. A
//! reader thread decodes a little ahead and stamps every frame and sound piece with the host
//! time it plays at, from the file's own timestamps, so picture and sound stay together.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderOutput, AVAssetReaderTrackOutput, AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeRange};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use super::host_ns;
use crate::{AvError, Picture};

/// How far ahead the reader decodes.
const AHEAD_NS: u64 = 300_000_000;
/// A frame kept waiting at most (a stalled engine can't grow memory).
const MAX_FRAMES: usize = 12;
const MAX_SOUND: usize = 400;

#[derive(Debug, Default)]
struct Shared {
    /// Frames waiting: the picture, the host time it shows at, and its place in the file (ns).
    frames: Mutex<VecDeque<(Picture, u64, u64)>>,
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    ended: AtomicBool,
    error: Mutex<Option<String>>,
    /// What the controls asked for, in order, for the reader to do.
    command: Mutex<VecDeque<Control>>,
    /// Where it is in the file (ns): the last frame shown, or the paused place.
    position: std::sync::atomic::AtomicU64,
    paused: AtomicBool,
    stopped: AtomicBool,
}

/// OBS's media controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Play,
    Pause,
    /// From the start, playing.
    Restart,
    /// Back to the start, showing nothing until played.
    Stop,
    /// To this place in the file (ns); playing or paused as it was.
    Seek(u64),
}

/// Where a file's playing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    Playing,
    Paused,
    Stopped,
    Ended,
}

/// A playing file; dropping it stops.
#[derive(Debug)]
pub struct Media {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
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
    // dictionaries hold NSNumbers under AVFoundation's documented keys.
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
            let rest = CMTime::new(i64::MAX / 2, 1_000_000);
            reader.setTimeRange(CMTimeRange { start, duration: rest });
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

fn pts_ns(s: &CMSampleBuffer) -> u64 {
    // SAFETY: reading a live sample buffer's time.
    let t = unsafe { s.presentation_time_stamp() };
    if t.timescale <= 0 || t.value < 0 {
        0
    } else {
        (i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale)) as u64
    }
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

impl Media {
    /// Opens a file and starts playing it (its first frame shows about 100 ms from now).
    pub fn open(path: &Path, looping: bool) -> Result<Self, AvError> {
        if !path.is_file() {
            return Err(AvError::Source(format!("{} isn't there", path.display())));
        }
        let url_text = path.to_string_lossy().to_string();
        // Probe on this thread, so a bad file says so at once.
        // SAFETY: AVFoundation objects made and read on this thread.
        let (has_video, has_audio, duration_ns) = unsafe {
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
            (v, a, dur)
        };
        if !has_video && !has_audio {
            return Err(AvError::Source(format!("{} has no picture or sound this Mac can play", path.display())));
        }
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("ferrocast-media".into())
                .spawn(move || read(&url_text, looping, duration_ns, &shared, &stop))
                .map_err(|e| AvError::Source(format!("the media couldn't start ({e})")))?
        };
        Ok(Self { shared, stop, thread: Some(thread), has_video, has_audio, duration_ns })
    }

    /// The frame to show at `now_ns` (the newest whose time has come; older ones are dropped).
    pub fn frame_at(&self, now_ns: u64) -> Option<Picture> {
        let mut q = self.shared.frames.lock().ok()?;
        let mut chosen = None;
        while q.front().is_some_and(|(_, t, _)| *t <= now_ns) {
            chosen = q.pop_front().map(|(p, _, pts)| {
                self.shared.position.store(pts, Ordering::Relaxed);
                p
            });
        }
        chosen
    }

    /// Plays, pauses, restarts, stops or seeks (it happens on the reader's next turn).
    pub fn control(&self, c: Control) {
        if let Ok(mut cmd) = self.shared.command.lock() {
            cmd.push_back(c);
        }
        // Said at once, so a second press reads the new state.
        match c {
            Control::Play | Control::Restart => {
                self.shared.paused.store(false, Ordering::Relaxed);
                self.shared.stopped.store(false, Ordering::Relaxed);
                self.shared.ended.store(false, Ordering::Relaxed);
            }
            Control::Pause => self.shared.paused.store(true, Ordering::Relaxed),
            Control::Stop => {
                self.shared.paused.store(true, Ordering::Relaxed);
                self.shared.stopped.store(true, Ordering::Relaxed);
            }
            Control::Seek(ns) => self.shared.position.store(ns.min(self.duration_ns), Ordering::Relaxed),
        }
    }

    /// Where it stands, and where it is in the file (ns).
    pub fn state(&self) -> (MediaState, u64) {
        let s = &self.shared;
        let state = if s.ended.load(Ordering::Relaxed) {
            MediaState::Ended
        } else if s.stopped.load(Ordering::Relaxed) {
            MediaState::Stopped
        } else if s.paused.load(Ordering::Relaxed) {
            MediaState::Paused
        } else {
            MediaState::Playing
        };
        (state, s.position.load(Ordering::Relaxed))
    }

    /// Sound decoded since the last call, each piece with the host time it plays at.
    pub fn take_sound(&self) -> Vec<(Vec<f32>, u64)> {
        self.shared.sound.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }

    /// Played to the end (not looping).
    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|e| e.clone())
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The reader thread: one pass after another (looping), each stamped from its start time;
/// the controls stop a pass and start another from the place asked for.
fn read(url_text: &str, looping: bool, duration_ns: u64, shared: &Shared, stop: &AtomicBool) {
    // SAFETY: AVFoundation objects made and used on this thread only.
    let asset =
        unsafe { AVURLAsset::URLAssetWithURL_options(&NSURL::fileURLWithPath(&NSString::from_str(url_text)), None) };
    let clear = || {
        if let Ok(mut q) = shared.frames.lock() {
            q.clear();
        }
        if let Ok(mut q) = shared.sound.lock() {
            q.clear();
        }
    };
    let take = || shared.command.lock().ok().and_then(|mut c| c.pop_front());
    // Where the next pass starts in the file, and the host time its 0 plays at when it follows
    // the last pass on (a seamless loop).
    let (mut from, mut follows): (u64, Option<u64>) = (0, None);
    'passes: loop {
        // Paused, stopped or ended: wait for a control.
        while shared.paused.load(Ordering::Relaxed) || shared.ended.load(Ordering::Relaxed) {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match take() {
                Some(Control::Play) => {
                    if shared.stopped.swap(false, Ordering::Relaxed) || shared.ended.swap(false, Ordering::Relaxed) {
                        from = 0;
                    }
                    shared.paused.store(false, Ordering::Relaxed);
                }
                Some(Control::Restart) => {
                    from = 0;
                    shared.paused.store(false, Ordering::Relaxed);
                    shared.ended.store(false, Ordering::Relaxed);
                }
                Some(Control::Seek(n)) => from = n.min(duration_ns),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
            follows = None;
        }
        let pass = match open_pass(&asset, from) {
            Ok(p) => p,
            Err(e) => {
                if let Ok(mut err) = shared.error.lock() {
                    *err = Some(format!("the media couldn't be played ({e})"));
                }
                return;
            }
        };
        let start = follows.unwrap_or_else(|| (host_ns() + 100_000_000).saturating_sub(from));
        let (mut v_next, mut a_next) =
            (pass.video.as_ref().and_then(|o| next(o)), pass.audio.as_ref().and_then(|o| next(o)));
        let mut last_end = 0u64;
        while (v_next.is_some() || a_next.is_some()) && !stop.load(Ordering::Acquire) {
            if let Some(c) = take() {
                let now_at = shared.position.load(Ordering::Relaxed);
                match c {
                    Control::Play => {}
                    Control::Pause => {
                        from = now_at;
                        shared.paused.store(true, Ordering::Relaxed);
                    }
                    Control::Seek(n) => from = n.min(duration_ns),
                    Control::Restart => from = 0,
                    Control::Stop => {
                        from = 0;
                        shared.position.store(0, Ordering::Relaxed);
                        shared.paused.store(true, Ordering::Relaxed);
                        shared.stopped.store(true, Ordering::Relaxed);
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
            let mut moved = false;
            if let Some(s) = &v_next {
                let at = start + pts_ns(s);
                let room = shared.frames.lock().map(|q| q.len() < MAX_FRAMES).unwrap_or(false);
                if at <= horizon && room {
                    // SAFETY: the decoded frame's pixel buffer is live with its sample buffer.
                    if let Some(img) = unsafe { s.image_buffer() }
                        && let Some(p) = super::screen::copy_bgra(&img)
                        && let Ok(mut q) = shared.frames.lock()
                    {
                        q.push_back((Picture { premultiplied: false, ..p }, at, pts_ns(s)));
                    }
                    last_end = last_end.max(pts_ns(s));
                    v_next = pass.video.as_ref().and_then(|o| next(o));
                    moved = true;
                }
            }
            if let Some(s) = &a_next {
                let at = start + pts_ns(s);
                let room = shared.sound.lock().map(|q| q.len() < MAX_SOUND).unwrap_or(false);
                if at <= horizon && room {
                    if let Some(pcm) = sound_of(s)
                        && let Ok(mut q) = shared.sound.lock()
                    {
                        last_end = last_end.max(pts_ns(s) + (pcm.len() / 2) as u64 * 1_000_000_000 / 48_000);
                        // A file with no picture keeps its place by its sound.
                        if pass.video.is_none() {
                            shared.position.store(pts_ns(s), Ordering::Relaxed);
                        }
                        q.push_back((pcm, at));
                    }
                    a_next = pass.audio.as_ref().and_then(|o| next(o));
                    moved = true;
                }
            }
            if !moved {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // SAFETY: the reader belongs to this thread.
        unsafe { pass.reader.cancelReading() };
        if stop.load(Ordering::Acquire) {
            return;
        }
        if !looping {
            shared.ended.store(true, Ordering::Relaxed);
            continue;
        }
        // The next pass starts where this one's length ends, exactly.
        from = 0;
        follows = Some(start + if duration_ns > 0 { duration_ns } else { last_end });
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
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x90:rate=30:duration=4"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        ok.then_some(f)
    }

    #[test]
    fn pause_seek_and_restart() {
        let Some(f) = clip() else { return };
        let m = Media::open(&f, false).unwrap_or_else(|e| panic!("{e}"));
        let show = |ms: u64| {
            let end = std::time::Instant::now() + Duration::from_millis(ms);
            while std::time::Instant::now() < end {
                let _ = m.frame_at(host_ns());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        show(700);
        assert_eq!(m.state().0, MediaState::Playing);
        m.control(Control::Pause);
        show(150);
        let at = m.state().1;
        assert!((300_000_000..900_000_000).contains(&at), "paused about half a second in: {at}");
        show(500);
        assert_eq!(m.state(), (MediaState::Paused, at), "still there");
        m.control(Control::Seek(2_500_000_000));
        m.control(Control::Play);
        show(500);
        let (state, after) = m.state();
        assert!(state == MediaState::Playing && after >= 2_500_000_000, "on from 2.5 s: {after}");
        m.control(Control::Restart);
        show(400);
        assert!(m.state().1 < 1_000_000_000, "from the start again: {}", m.state().1);
        m.control(Control::Stop);
        show(100);
        assert_eq!(m.state(), (MediaState::Stopped, 0));
    }
}
