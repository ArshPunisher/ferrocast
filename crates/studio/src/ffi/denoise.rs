//! Strong noise removal: an audio filter that runs DeepFilterNet 3 (MIT/Apache-2.0,
//! github.com/Rikorose/DeepFilterNet) on the CPU through tract, its model built in. It takes out
//! what RNNoise leaves (keyboards, fans, a busy room) and keeps the voice natural.
//!
//! DeepFilterNet works on 10 ms blocks at 48 kHz: libobs's audio is gathered into blocks, and the
//! cleaned blocks are handed back one block later (10 ms, plus the model's own look ahead). At
//! another sample rate the sound passes as it is. Built only with the `deepfilter` feature (the
//! release builds), so day-to-day builds don't compile the model.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray15::Array2;

use super::shaders::OBS_SOURCE_TYPE_FILTER;
use super::{Api, AudioInfo2, GsEffect, OBS_SOURCE_AUDIO, ObsData, ObsProperties, ObsSource};

pub(crate) const KIND: &CStr = c"ferrocast_deepfilter";

/// The sample rate DeepFilterNet is made for.
const RATE: u32 = 48_000;

static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

fn api() -> Option<&'static Api> {
    // SAFETY: set once from the Studio's Api, which outlives every filter.
    unsafe { API.load(Ordering::Acquire).as_ref() }
}

/// How much quieter noise may get at a strength (1 to 100): 100 is as much as it can.
fn atten_db(strength: u32) -> f32 {
    // 0.6 dB a step, worked in whole numbers (50 is exactly 30 dB).
    if strength >= 100 { 100.0 } else { (strength.max(1) * 6) as f32 / 10.0 }
}

/// DeepFilterNet over a stream of planar samples: blocks in, cleaned blocks out, one block late.
pub(crate) struct Denoiser {
    df: DfTract,
    hop: usize,
    inp: Vec<VecDeque<f32>>,
    out: Vec<VecDeque<f32>>,
    noisy: Array2<f32>,
    clean: Array2<f32>,
    strength: u32,
}

impl Denoiser {
    /// For `channels` channels at 48 kHz.
    pub(crate) fn new(channels: usize, strength: u32) -> Result<Denoiser, String> {
        let channels = channels.clamp(1, 8);
        let rp = RuntimeParams::default_with_ch(channels).with_atten_lim(atten_db(strength));
        let df = DfTract::new(DfParams::default(), &rp).map_err(|e| format!("{e:#}"))?;
        let hop = df.hop_size;
        // One block of silence first: what's handed back is always there.
        let out = (0..channels).map(|_| std::iter::repeat_n(0.0, hop).collect()).collect();
        Ok(Denoiser {
            df,
            hop,
            inp: (0..channels).map(|_| VecDeque::with_capacity(hop * 4)).collect(),
            out,
            noisy: Array2::zeros((channels, hop)),
            clean: Array2::zeros((channels, hop)),
            strength,
        })
    }

    pub(crate) fn channels(&self) -> usize {
        self.inp.len()
    }

    pub(crate) fn set_strength(&mut self, strength: u32) {
        if strength != self.strength {
            self.df.set_atten_lim(atten_db(strength));
            self.strength = strength;
        }
    }

    /// Cleans the samples in place (every plane as long as the first; at most `channels`).
    pub(crate) fn run(&mut self, planes: &mut [&mut [f32]]) {
        let n = planes.first().map_or(0, |p| p.len());
        let ch = self.channels().min(planes.len());
        for (c, p) in planes.iter().enumerate().take(ch) {
            self.inp[c].extend(p.iter().copied());
        }
        // A channel the stream lacks is silence.
        for c in ch..self.channels() {
            self.inp[c].extend(std::iter::repeat_n(0.0, n));
        }
        while self.inp[0].len() >= self.hop {
            for c in 0..self.channels() {
                for (i, s) in self.inp[c].drain(..self.hop).enumerate() {
                    self.noisy[[c, i]] = s;
                }
            }
            if self.df.process(self.noisy.view(), self.clean.view_mut()).is_err() {
                // As it was rather than nothing.
                self.clean.assign(&self.noisy);
            }
            for c in 0..self.channels() {
                self.out[c].extend(self.clean.row(c).iter().copied());
            }
        }
        for (c, p) in planes.iter_mut().enumerate().take(ch) {
            for s in p.iter_mut() {
                *s = self.out[c].pop_front().unwrap_or(0.0);
            }
        }
        for c in ch..self.channels() {
            let k = n.min(self.out[c].len());
            self.out[c].drain(..k);
        }
    }
}

/// `struct obs_audio_data`.
#[repr(C)]
struct AudioData {
    data: [*mut u8; 8],
    frames: u32,
    timestamp: u64,
}

/// `struct obs_source_info` up to `filter_audio` (libobs takes the size it's given).
#[repr(C)]
struct AudioFilterInfo {
    id: *const c_char,
    kind: c_int,
    output_flags: u32,
    get_name: Option<unsafe extern "C" fn(*mut c_void) -> *const c_char>,
    create: Option<unsafe extern "C" fn(*mut ObsData, *mut ObsSource) -> *mut c_void>,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    get_width: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    get_height: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    get_defaults: Option<unsafe extern "C" fn(*mut ObsData)>,
    get_properties: Option<unsafe extern "C" fn(*mut c_void) -> *mut ObsProperties>,
    update: Option<unsafe extern "C" fn(*mut c_void, *mut ObsData)>,
    activate: Option<unsafe extern "C" fn(*mut c_void)>,
    deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
    show: Option<unsafe extern "C" fn(*mut c_void)>,
    hide: Option<unsafe extern "C" fn(*mut c_void)>,
    video_tick: Option<unsafe extern "C" fn(*mut c_void, f32)>,
    video_render: Option<unsafe extern "C" fn(*mut c_void, *mut GsEffect)>,
    filter_video: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void>,
    filter_audio: Option<unsafe extern "C" fn(*mut c_void, *mut AudioData) -> *mut AudioData>,
}

struct Filter {
    /// Audio thread only; None at another sample rate, or when the model wouldn't start.
    den: Option<Denoiser>,
    strength: AtomicU32,
}

/// The sound's channel count and whether it's at 48 kHz.
fn audio_setup(a: &Api) -> (usize, bool) {
    let mut info = AudioInfo2 { samples_per_sec: 0, speakers: 2, max_buffering_ms: 0, fixed_buffering: false };
    if !a.obs_get_audio_info2(&mut info) {
        return (2, true);
    }
    // enum speaker_layout: 1 mono … 6 5.1, 8 7.1.
    let ch = match info.speakers {
        1..=6 => info.speakers as usize,
        8 => 8,
        _ => 2,
    };
    (ch, info.samples_per_sec == RATE)
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Strong noise removal".as_ptr()
}

unsafe extern "C" fn create(settings: *mut ObsData, _source: *mut ObsSource) -> *mut c_void {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let strength = (a.obs_data_get_int(settings, c"strength".as_ptr()).clamp(1, 100)) as u32;
    let (ch, at_48k) = audio_setup(a);
    let den = if at_48k {
        Denoiser::new(ch, strength)
            .inspect_err(|e| tracing::warn!(error = %e, "strong noise removal didn't start"))
            .ok()
    } else {
        tracing::warn!("strong noise removal needs 48 kHz sound: it passes the sound as it is");
        None
    };
    Box::into_raw(Box::new(Filter { den, strength: AtomicU32::new(strength) })).cast()
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: `data` is the Box made in `create`, destroyed once by libobs.
    drop(unsafe { Box::from_raw(data.cast::<Filter>()) });
}

unsafe extern "C" fn get_defaults(s: *mut ObsData) {
    if let Some(a) = api() {
        a.obs_data_set_default_int(s, c"strength".as_ptr(), 100);
    }
}

unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut ObsProperties {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let p = a.obs_properties_create();
    let s = a.obs_properties_add_int_slider(p, c"strength".as_ptr(), c"Strength".as_ptr(), 1, 100, 1);
    if !s.is_null() {
        a.obs_property_int_set_suffix(s, c"%".as_ptr());
    }
    p
}

unsafe extern "C" fn update(data: *mut c_void, s: *mut ObsData) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter; `strength` is atomic.
    let f = unsafe { &*data.cast::<Filter>() };
    f.strength.store(a.obs_data_get_int(s, c"strength".as_ptr()).clamp(1, 100) as u32, Ordering::Relaxed);
}

unsafe extern "C" fn filter_audio(data: *mut c_void, audio: *mut AudioData) -> *mut AudioData {
    // SAFETY: `data` is the live Filter; libobs calls this on its audio thread only, the one
    // thread that touches `den`.
    let f = unsafe { &mut *data.cast::<Filter>() };
    let Some(den) = f.den.as_mut() else { return audio };
    // SAFETY: libobs hands a live obs_audio_data for this call.
    let Some(ad) = (unsafe { audio.as_mut() }) else { return audio };
    den.set_strength(f.strength.load(Ordering::Relaxed));
    let frames = ad.frames as usize;
    let planes: Vec<*mut u8> = ad.data.iter().copied().take(den.channels()).take_while(|p| !p.is_null()).collect();
    // SAFETY: libobs's audio is float planar: each non-null plane holds `frames` f32s, the planes
    // apart from one another, all live for this call.
    let mut planes: Vec<&mut [f32]> =
        planes.into_iter().map(|p| unsafe { std::slice::from_raw_parts_mut(p.cast::<f32>(), frames) }).collect();
    den.run(&mut planes);
    audio
}

/// Registers the kind (once per process, after the plugins load).
pub(crate) fn register(api: &Api) {
    if !API.load(Ordering::Acquire).is_null() {
        return;
    }
    API.store(api as *const Api as *mut Api, Ordering::Release);
    let info = AudioFilterInfo {
        id: KIND.as_ptr(),
        kind: OBS_SOURCE_TYPE_FILTER,
        output_flags: OBS_SOURCE_AUDIO,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_width: None,
        get_height: None,
        get_defaults: Some(get_defaults),
        get_properties: Some(get_properties),
        update: Some(update),
        activate: None,
        deactivate: None,
        show: None,
        hide: None,
        video_tick: None,
        video_render: None,
        filter_video: None,
        filter_audio: Some(filter_audio),
    };
    api.obs_register_source_s((&info as *const AudioFilterInfo).cast(), std::mem::size_of::<AudioFilterInfo>());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady hiss (a fan), the same every run.
    fn hiss(n: usize, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
            })
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_hiss_goes_and_every_sample_comes_back() {
        let mut d = Denoiser::new(2, 100).expect("the built-in model starts");
        let mut seed = 7;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        // Two seconds in libobs's own odd-sized pieces (1024 frames), stereo.
        for _ in 0..94 {
            let mut l = hiss(1024, &mut seed);
            let mut r = l.clone();
            before.extend_from_slice(&l);
            d.run(&mut [&mut l, &mut r]);
            assert_eq!(l, r, "both channels alike");
            after.extend_from_slice(&l);
        }
        assert_eq!(before.len(), after.len(), "as many samples out as in");
        // The last second, once it has settled.
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        assert!(a < b * 0.1, "the hiss is at least 20 dB quieter: {b} → {a}");
    }

    #[test]
    fn a_lighter_touch_leaves_some_of_it() {
        let mut d = Denoiser::new(1, 20).expect("the built-in model starts");
        let mut seed = 9;
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for _ in 0..94 {
            let mut m = hiss(1024, &mut seed);
            before.extend_from_slice(&m);
            d.run(&mut [&mut m]);
            after.extend_from_slice(&m);
        }
        let (b, a) = (rms(&before[48_000..]), rms(&after[48_000..]));
        // 20% is at most 12 dB less: a quarter of the level or more.
        assert!(a > b * 0.2, "some of it stays: {b} → {a}");
    }

    #[test]
    fn strength_maps_to_a_limit() {
        assert_eq!(atten_db(100), 100.0);
        assert_eq!(atten_db(50), 30.0);
        assert_eq!(atten_db(0), 0.6);
    }
}
