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
use objc2_core_media::CMSampleBuffer;
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
    frames: Mutex<VecDeque<(Picture, u64)>>,
    sound: Mutex<VecDeque<(Vec<f32>, u64)>>,
    ended: AtomicBool,
    error: Mutex<Option<String>>,
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

fn open_pass(asset: &AVURLAsset) -> Result<Pass, String> {
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
        while q.front().is_some_and(|(_, t)| *t <= now_ns) {
            chosen = q.pop_front().map(|(p, _)| p);
        }
        chosen
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

/// The reader thread: one pass after another (looping), each stamped from its start time.
fn read(url_text: &str, looping: bool, duration_ns: u64, shared: &Shared, stop: &AtomicBool) {
    // SAFETY: AVFoundation objects made and used on this thread only.
    let asset =
        unsafe { AVURLAsset::URLAssetWithURL_options(&NSURL::fileURLWithPath(&NSString::from_str(url_text)), None) };
    let mut start = host_ns() + 100_000_000;
    loop {
        let pass = match open_pass(&asset) {
            Ok(p) => p,
            Err(e) => {
                if let Ok(mut err) = shared.error.lock() {
                    *err = Some(format!("the media couldn't be played ({e})"));
                }
                return;
            }
        };
        let (mut v_next, mut a_next) =
            (pass.video.as_ref().and_then(|o| next(o)), pass.audio.as_ref().and_then(|o| next(o)));
        let mut last_end = 0u64;
        while (v_next.is_some() || a_next.is_some()) && !stop.load(Ordering::Acquire) {
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
                        q.push_back((Picture { premultiplied: false, ..p }, at));
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
            return;
        }
        // The next pass starts where this one's length ends, exactly.
        start += if duration_ns > 0 { duration_ns } else { last_end };
    }
}
