//! The program as libobs finishes it: every video frame (NV12, the output size) and the mixed
//! sound (track 1, stereo float), handed to Ferrocast's own encoders (ENGINE.md step E1: libobs
//! mixes, our engine encodes and sends). Copied out on libobs's own threads into bounded queues:
//! a slow reader loses frames (counted), never stalls libobs.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

use super::{Api, AudioData};

/// libobs's `struct video_data`.
#[repr(C)]
#[derive(Debug)]
pub struct VideoData {
    pub data: [*mut u8; 8],
    pub linesize: [u32; 8],
    pub timestamp: u64,
}

pub type RawVideoCb = unsafe extern "C" fn(*mut c_void, *mut VideoData);
pub type RawAudioCb = unsafe extern "C" fn(*mut c_void, usize, *mut AudioData);

/// `VIDEO_FORMAT_NV12`.
pub const NV12: i32 = 2;
/// About a second of each queued before frames are dropped.
const VIDEO_QUEUE: usize = 60;
const AUDIO_QUEUE: usize = 100;

/// One frame: NV12 planes, rows of `stride` bytes. Time on libobs's clock, in nanoseconds.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
    pub timestamp_ns: u64,
}

/// A piece of the mixed sound: stereo, interleaved left, right. Same clock as the frames.
#[derive(Debug, Clone)]
pub struct SoundChunk {
    pub samples: Vec<f32>,
    pub timestamp_ns: u64,
}

struct Shared {
    video: Mutex<SyncSender<VideoFrame>>,
    audio: Mutex<SyncSender<SoundChunk>>,
    width: u32,
    height: u32,
    dropped: AtomicU64,
}

/// Taking the program; dropping it stops.
pub struct Feed {
    api: *const Api,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed").field("width", &self.shared.width).field("height", &self.shared.height).finish()
    }
}

impl Feed {
    /// Starts taking frames of `width`×`height` (libobs's output size, NV12).
    pub(crate) fn start(api: &Api, width: u32, height: u32) -> (Feed, Receiver<VideoFrame>, Receiver<SoundChunk>) {
        let (vt, vr) = sync_channel(VIDEO_QUEUE);
        let (at, ar) = sync_channel(AUDIO_QUEUE);
        let shared = Arc::new(Shared {
            video: Mutex::new(vt),
            audio: Mutex::new(at),
            width,
            height,
            dropped: AtomicU64::new(0),
        });
        let param = Arc::as_ptr(&shared) as *mut c_void;
        api.obs_add_raw_video_callback(std::ptr::null(), take_video, param);
        api.obs_add_raw_audio_callback(0, std::ptr::null(), take_audio, param);
        (Feed { api: api as *const Api, shared }, vr, ar)
    }

    /// Frames and sound pieces lost because the reader fell behind.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // SAFETY: the owner keeps the Studio, and so this Api, alive until after this drop.
        let api = unsafe { &*self.api };
        let param = Arc::as_ptr(&self.shared) as *mut c_void;
        api.obs_remove_raw_video_callback(take_video, param);
        api.obs_remove_raw_audio_callback(0, take_audio, param);
    }
}

fn send<T>(tx: &Mutex<SyncSender<T>>, v: T, dropped: &AtomicU64) {
    let Ok(tx) = tx.lock() else { return };
    if let Err(TrySendError::Full(_)) = tx.try_send(v) {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// libobs's raw video callback: one finished frame, NV12 (planes 0 and 1).
unsafe extern "C" fn take_video(param: *mut c_void, frame: *mut VideoData) {
    if param.is_null() || frame.is_null() {
        return;
    }
    // SAFETY: `param` is the Shared the Feed keeps alive until it removes this callback; `frame`
    // is valid for the call.
    let (sh, f) = unsafe { (&*(param as *const Shared), &*frame) };
    let (h, ys, uvs) = (sh.height as usize, f.linesize[0] as usize, f.linesize[1] as usize);
    if f.data[0].is_null() || f.data[1].is_null() || ys < sh.width as usize || uvs < sh.width as usize {
        return;
    }
    // SAFETY: NV12 from libobs: plane 0 is `h` rows of `linesize[0]` bytes, plane 1 `h / 2` rows
    // of `linesize[1]`, valid for the call.
    let (y, uv) = unsafe {
        (
            std::slice::from_raw_parts(f.data[0], ys * h).to_vec(),
            std::slice::from_raw_parts(f.data[1], uvs * h / 2).to_vec(),
        )
    };
    let v = VideoFrame {
        width: sh.width,
        height: sh.height,
        y,
        y_stride: ys,
        uv,
        uv_stride: uvs,
        timestamp_ns: f.timestamp,
    };
    send(&sh.video, v, &sh.dropped);
}

/// libobs's raw audio callback: track 1's mix, float planar; the first two planes interleaved
/// (a mono setup's one plane on both sides).
unsafe extern "C" fn take_audio(param: *mut c_void, _mix: usize, audio: *mut AudioData) {
    if param.is_null() || audio.is_null() {
        return;
    }
    // SAFETY: as take_video; `audio` is valid for the call.
    let (sh, a) = unsafe { (&*(param as *const Shared), &*audio) };
    let n = a.frames as usize;
    if n == 0 || a.data[0].is_null() {
        return;
    }
    // SAFETY: plane 0 holds `frames` floats for the call.
    let left = unsafe { std::slice::from_raw_parts(a.data[0] as *const f32, n) };
    let right = if a.data[1].is_null() {
        left
    } else {
        // SAFETY: plane 1, when there is one, is as long as plane 0.
        unsafe { std::slice::from_raw_parts(a.data[1] as *const f32, n) }
    };
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        samples.push(left[i]);
        samples.push(right[i]);
    }
    send(&sh.audio, SoundChunk { samples, timestamp_ns: a.timestamp }, &sh.dropped);
}
