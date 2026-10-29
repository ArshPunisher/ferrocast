//! A Studio sound's audio, as libobs mixes it (after its filters), handed to anything that
//! listens (plan item 7b, 30 Oct: Whisper captions on every system). Mono float samples at the
//! Studio's rate, at most `MAX_SECONDS` kept.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use super::{Api, AudioData, ObsSource};

/// Samples kept when nobody takes them (a stalled listener can't grow memory).
const MAX_SECONDS: usize = 30;

struct Buf {
    samples: Mutex<VecDeque<f32>>,
    max: usize,
}

/// Listening to one source; dropping it stops.
pub struct SoundTap {
    /// The Studio's libobs: its owner keeps the Studio alive longer.
    api: *const Api,
    source: *mut ObsSource,
    buf: Arc<Buf>,
    rate: u32,
}

impl std::fmt::Debug for SoundTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundTap").field("rate", &self.rate).finish_non_exhaustive()
    }
}

impl SoundTap {
    /// Starts listening to `source` (the caller's reference is not taken: one is added).
    pub(crate) fn start(api: &Api, source: *mut ObsSource, rate: u32) -> SoundTap {
        let max = rate as usize * MAX_SECONDS;
        let buf = Arc::new(Buf { samples: Mutex::new(VecDeque::with_capacity(rate as usize * 4)), max });
        api.obs_source_get_ref(source);
        api.obs_source_add_audio_capture_callback(source, take_audio, Arc::as_ptr(&buf) as *mut c_void);
        SoundTap { api: api as *const Api, source, buf, rate }
    }

    /// Samples per second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// A handle to what's heard, for another thread.
    pub fn reader(&self) -> TapReader {
        TapReader(self.buf.clone())
    }
}

/// What a SoundTap hears, readable from any thread (empty once the tap is dropped).
#[derive(Clone)]
pub struct TapReader(Arc<Buf>);

impl std::fmt::Debug for TapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader").finish_non_exhaustive()
    }
}

impl TapReader {
    /// Everything heard since the last take (oldest first).
    pub fn take(&self) -> Vec<f32> {
        self.0.samples.lock().map(|mut s| s.drain(..).collect()).unwrap_or_default()
    }

    /// Whether the tap is gone (nothing more will come).
    pub fn closed(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}

impl Drop for SoundTap {
    fn drop(&mut self) {
        // SAFETY: the owner keeps the Studio, and so this Api, alive until after this drop.
        let api = unsafe { &*self.api };
        api.obs_source_remove_audio_capture_callback(self.source, take_audio, Arc::as_ptr(&self.buf) as *mut c_void);
        api.obs_source_release(self.source);
    }
}

/// libobs's audio capture callback: the first two planes (float planar) mixed to mono.
unsafe extern "C" fn take_audio(param: *mut c_void, _src: *mut ObsSource, audio: *const AudioData, muted: bool) {
    if param.is_null() || audio.is_null() {
        return;
    }
    // SAFETY: `param` is the Buf the SoundTap keeps alive until it removes this callback;
    // `audio` is valid for the call, each plane `frames` floats long.
    let (buf, audio) = unsafe { (&*(param as *const Buf), &*audio) };
    let n = audio.frames as usize;
    if n == 0 || audio.data[0].is_null() {
        return;
    }
    // SAFETY: plane 0 holds `frames` floats for the call.
    let left = unsafe { std::slice::from_raw_parts(audio.data[0] as *const f32, n) };
    let right = (!audio.data[1].is_null())
        // SAFETY: plane 1, when there is one, is as long as plane 0.
        .then(|| unsafe { std::slice::from_raw_parts(audio.data[1] as *const f32, n) });
    let Ok(mut s) = buf.samples.lock() else { return };
    for i in 0..n {
        // Muted sounds count as silence (the listener keeps its timing).
        let v = if muted {
            0.0
        } else {
            match right {
                Some(r) => (left[i] + r[i]) * 0.5,
                None => left[i],
            }
        };
        s.push_back(v);
    }
    let over = s.len().saturating_sub(buf.max);
    s.drain(..over);
}
