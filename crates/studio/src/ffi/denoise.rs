//! Strong noise removal: an audio filter that runs DeepFilterNet 3 (MIT/Apache-2.0,
//! github.com/Rikorose/DeepFilterNet) on the CPU through tract, its model built in. It takes out
//! what RNNoise leaves (keyboards, fans, a busy room) and keeps the voice natural.
//!
//! DeepFilterNet works on 10 ms blocks at 48 kHz: libobs's audio is gathered into blocks, and the
//! cleaned blocks are handed back one block later (10 ms, plus the model's own look ahead). At
//! another sample rate the sound passes as it is. Built only with the `deepfilter` feature (the
//! release builds), so day-to-day builds don't compile the model.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::denoiser::Denoiser;

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
