//! Remove background (macOS): a filter that finds the person in a picture with Apple's Vision
//! (person segmentation, on this Mac) and takes the background out, or blurs it. No green screen.
//!
//! About 15 times a second the graphics thread renders a small copy of the source and hands it
//! to a worker thread (only when the worker is free, so nothing waits); the worker asks Vision
//! for the person's mask; the next frame uploads the newest mask and a shader applies it. Until
//! the first mask arrives the picture shows as it is.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::shaders::{
    COMBO_FORMAT_INT, COMBO_LIST, FOOTER, HEADER, OBS_ALLOW_DIRECT_RENDERING, OBS_SOURCE_TYPE_FILTER, OBS_SOURCE_VIDEO,
    SourceInfo,
};
use super::{Api, GsEffect, GsTexture, ObsData, ObsProperties, ObsSource, Vec2};

pub(crate) const KIND: &CStr = c"ferrocast_remove_background";

/// The mask Vision is asked for is about this wide (height follows the picture's shape).
const MASK_W: u32 = 320;
/// How often a new mask is asked for.
const EVERY: Duration = Duration::from_millis(66);

const BODY: &str = "
uniform texture2d mask;
uniform float mode;
uniform float soft;
uniform float blur;
uniform float have_mask;

float person(float2 uv)
{
	if (have_mask < 0.5)
		return 1.0;
	float m = mask.Sample(textureSampler, uv).r;
	float e = max(soft, 0.01);
	return smoothstep(0.5 - e, 0.5 + e, m);
}

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float k = person(p / size);
	if (mode < 0.5)
		return float4(c.rgb, c.a * k);
	// Blur behind the person: the background's own pixels only, softly.
	float4 acc = float4(0.0, 0.0, 0.0, 0.0);
	for (int i = 0; i < 48; i++) {
		float t = (float(i) + 0.5) / 48.0;
		float ang = float(i) * 2.39996323;
		float2 q = clamp(p + float2(cos(ang), sin(ang)) * (sqrt(t) * blur), float2(0.0, 0.0), size - 1.0);
		acc += src_at(q);
	}
	return lerp(acc / 48.0, c, k);
}
";

static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

fn api() -> Option<&'static Api> {
    // SAFETY: set once from the Studio's Api, which outlives every filter.
    unsafe { API.load(Ordering::Acquire).as_ref() }
}

/// A mask from Vision: one byte a pixel, 255 where the person is.
struct Mask {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

struct Filter {
    source: *mut ObsSource,
    effect: *mut GsEffect,
    /// Graphics thread only.
    tex: *mut GsTexture,
    tex_size: (u32, u32),
    last_ask: Option<Instant>,
    /// Frames to the worker (one at a time).
    ask: Option<SyncSender<(Vec<u8>, u32, u32)>>,
    busy: Arc<AtomicBool>,
    mask: Arc<Mutex<Option<Mask>>>,
    mode: AtomicU32,
    soft: AtomicU32,
    blur: AtomicU32,
}

/// Vision's person mask of an RGBA picture, at its own size; None when it finds nothing.
fn person_mask(rgba: &[u8], w: u32, h: u32) -> Option<Mask> {
    use objc2::AnyThread;
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSData, NSDictionary};
    use objc2_vision::{
        VNGeneratePersonSegmentationRequest, VNGeneratePersonSegmentationRequestQualityLevel, VNImageRequestHandler,
        VNRequest,
    };
    if w == 0 || h == 0 || rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().ok()?.write_image_data(rgba).ok()?;
    }
    let data = NSData::with_bytes(&png_bytes);
    let handler =
        VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
    // SAFETY: a plain new request; its quality and output format set before it runs.
    let request = unsafe { VNGeneratePersonSegmentationRequest::new() };
    // SAFETY: setters on a live request, before it runs.
    unsafe {
        request.setQualityLevel(VNGeneratePersonSegmentationRequestQualityLevel::Balanced);
        // kCVPixelFormatType_OneComponent8 ('L008'): one byte a pixel.
        request.setOutputPixelFormat(0x4C30_3038);
    }
    let as_request: &VNRequest = &request;
    handler.performRequests_error(&NSArray::from_slice(&[as_request])).ok()?;
    // SAFETY: results of a request that has run, read on the same thread.
    let found = unsafe { request.results() }?;
    let obs = found.firstObject()?;
    // SAFETY: a getter on a live observation.
    let buf = unsafe { obs.pixelBuffer() };
    let (mw, mh) = (CVPixelBufferGetWidth(&buf), CVPixelBufferGetHeight(&buf));
    // SAFETY: locked for reading, read row by row within the buffer's own sizes, then unlocked.
    unsafe {
        if CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(&buf) as *const u8;
        let row = CVPixelBufferGetBytesPerRow(&buf);
        let mut px = Vec::with_capacity(mw * mh);
        if !base.is_null() && row >= mw {
            for y in 0..mh {
                px.extend_from_slice(std::slice::from_raw_parts(base.add(y * row), mw));
            }
        }
        CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly);
        (px.len() == mw * mh).then_some(Mask { w: mw as u32, h: mh as u32, px })
    }
}

fn worker(rx: Receiver<(Vec<u8>, u32, u32)>, busy: Arc<AtomicBool>, out: Arc<Mutex<Option<Mask>>>) {
    while let Ok((rgba, w, h)) = rx.recv() {
        // Vision makes Objective-C objects: each round in its own pool.
        let m = objc2::rc::autoreleasepool(|_| person_mask(&rgba, w, h));
        if let Some(m) = m
            && let Ok(mut slot) = out.lock()
        {
            *slot = Some(m);
        }
        busy.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Remove background".as_ptr()
}

unsafe extern "C" fn create(settings: *mut ObsData, source: *mut ObsSource) -> *mut c_void {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let text = CString::new(format!("{HEADER}{BODY}{FOOTER}")).unwrap_or_default();
    a.obs_enter_graphics();
    let mut err: *mut c_char = std::ptr::null_mut();
    let effect = a.gs_effect_create(text.as_ptr(), c"ferrocast_remove_background.effect".as_ptr(), &mut err);
    if !err.is_null() {
        // SAFETY: libobs filled `err` with a NUL-terminated message it allocated; read, then freed.
        let e = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        tracing::warn!(error = %e, "the remove background shader didn't build");
        a.bfree(err.cast());
    }
    // A 1×1 "all person" mask until Vision's first: the shader's mask is always bound (Metal
    // crashed drawing with it unbound; OpenGL warned).
    let all = [255u8];
    let ptr = all.as_ptr();
    let tex = a.gs_texture_create(1, 1, GS_R8, 1, &ptr, GS_DYNAMIC);
    a.obs_leave_graphics();
    let busy = Arc::new(AtomicBool::new(false));
    let mask = Arc::new(Mutex::new(None));
    let (tx, rx) = sync_channel(1);
    let (b, m) = (busy.clone(), mask.clone());
    let ask =
        std::thread::Builder::new().name("ferrocast-cutout".into()).spawn(move || worker(rx, b, m)).ok().map(|_| tx);
    let f = Box::new(Filter {
        source,
        effect,
        tex,
        tex_size: (1, 1),
        last_ask: None,
        ask,
        busy,
        mask,
        mode: AtomicU32::new(0),
        soft: AtomicU32::new(10),
        blur: AtomicU32::new(24),
    });
    let p = Box::into_raw(f);
    // SAFETY: `p` is the Filter just made.
    unsafe { update(p.cast(), settings) };
    p.cast()
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: `data` is the Box made in `create`, destroyed once by libobs. Dropping its sender
    // ends the worker after its current picture.
    let f = unsafe { Box::from_raw(data.cast::<Filter>()) };
    if let Some(a) = api()
        && (!f.effect.is_null() || !f.tex.is_null())
    {
        a.obs_enter_graphics();
        if !f.effect.is_null() {
            a.gs_effect_destroy(f.effect);
        }
        if !f.tex.is_null() {
            a.gs_texture_destroy(f.tex);
        }
        a.obs_leave_graphics();
    }
}

unsafe extern "C" fn get_defaults(s: *mut ObsData) {
    if let Some(a) = api() {
        a.obs_data_set_default_int(s, c"mode".as_ptr(), 0);
        a.obs_data_set_default_int(s, c"soft".as_ptr(), 10);
        a.obs_data_set_default_int(s, c"blur".as_ptr(), 24);
    }
}

unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut ObsProperties {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let p = a.obs_properties_create();
    let list = a.obs_properties_add_list(p, c"mode".as_ptr(), c"Background".as_ptr(), COMBO_LIST, COMBO_FORMAT_INT);
    if !list.is_null() {
        a.obs_property_list_add_int(list, c"Remove it".as_ptr(), 0);
        a.obs_property_list_add_int(list, c"Blur it".as_ptr(), 1);
    }
    let soft = a.obs_properties_add_int_slider(p, c"soft".as_ptr(), c"Soft edge".as_ptr(), 0, 40, 1);
    if !soft.is_null() {
        a.obs_property_int_set_suffix(soft, c"%".as_ptr());
    }
    let blur = a.obs_properties_add_int_slider(p, c"blur".as_ptr(), c"Blur".as_ptr(), 2, 100, 1);
    if !blur.is_null() {
        a.obs_property_int_set_suffix(blur, c" px".as_ptr());
    }
    p
}

unsafe extern "C" fn update(data: *mut c_void, s: *mut ObsData) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter.
    let f = unsafe { &*data.cast::<Filter>() };
    let get = |k: &CStr, lo: i64, hi: i64| a.obs_data_get_int(s, k.as_ptr()).clamp(lo, hi) as u32;
    f.mode.store(get(c"mode", 0, 1), Ordering::Relaxed);
    f.soft.store(get(c"soft", 0, 40), Ordering::Relaxed);
    f.blur.store(get(c"blur", 2, 100), Ordering::Relaxed);
}

/// Before each frame is drawn (never while drawing: Metal crashed when the snapshot was taken
/// inside the filter's draw): a small copy of the source to the worker (when it's free and a
/// moment has passed), and the newest mask into the texture.
fn feed(a: &Api, f: &mut Filter, target: *mut ObsSource, w: u32, h: u32) {
    let due = f.last_ask.is_none_or(|t| t.elapsed() >= EVERY);
    if due && !f.busy.load(Ordering::Acquire) {
        let sw = MASK_W.min(w);
        let sh = ((u64::from(h) * u64::from(sw)) / u64::from(w)).max(1) as u32;
        if let Some(rgba) = super::snapshot_scaled(a, target, (w, h), sw, sh)
            && let Some(ask) = &f.ask
        {
            f.busy.store(true, Ordering::Release);
            if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = ask.try_send((rgba, sw, sh)) {
                f.busy.store(false, Ordering::Release);
            }
        }
        f.last_ask = Some(Instant::now());
    }
    let newest = f.mask.try_lock().ok().and_then(|mut m| m.take());
    if let Some(m) = newest {
        let ptr = m.px.as_ptr();
        a.obs_enter_graphics();
        if f.tex.is_null() || f.tex_size != (m.w, m.h) {
            if !f.tex.is_null() {
                a.gs_texture_destroy(f.tex);
            }
            f.tex = a.gs_texture_create(m.w, m.h, GS_R8, 1, &ptr, GS_DYNAMIC);
            f.tex_size = (m.w, m.h);
        } else {
            a.gs_texture_set_image(f.tex, ptr, m.w, false);
        }
        a.obs_leave_graphics();
    }
}

unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter; libobs ticks sources on its graphics thread, the only
    // thread that touches `tex`, `tex_size` and `last_ask`.
    let f = unsafe { &mut *data.cast::<Filter>() };
    let target = a.obs_filter_get_target(f.source);
    if target.is_null() || f.effect.is_null() {
        return;
    }
    let (w, h) = (a.obs_source_get_base_width(target), a.obs_source_get_base_height(target));
    if w > 0 && h > 0 {
        feed(a, f, target, w, h);
    }
}

unsafe extern "C" fn video_render(data: *mut c_void, _: *mut GsEffect) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter; libobs renders on its graphics thread.
    let f = unsafe { &*data.cast::<Filter>() };
    let target = a.obs_filter_get_target(f.source);
    let (w, h) = if target.is_null() {
        (0, 0)
    } else {
        (a.obs_source_get_base_width(target), a.obs_source_get_base_height(target))
    };
    // Without the shader or a mask texture (never bound empty: Metal crashed drawing so), it's
    // drawn as it is.
    if f.effect.is_null() || f.tex.is_null() || w == 0 || h == 0 {
        a.obs_source_skip_video_filter(f.source);
        return;
    }
    if !a.obs_source_process_filter_begin(f.source, super::GS_RGBA, OBS_ALLOW_DIRECT_RENDERING) {
        return;
    }
    let e = f.effect;
    let set = |n: &CStr, v: f32| a.gs_effect_set_float(a.gs_effect_get_param_by_name(e, n.as_ptr()), v);
    let size = Vec2 { x: w as f32, y: h as f32 };
    a.gs_effect_set_vec2(a.gs_effect_get_param_by_name(e, c"size".as_ptr()), &size);
    a.gs_effect_set_vec2(a.gs_effect_get_param_by_name(e, c"out_size".as_ptr()), &size);
    set(c"pad", 0.0);
    set(c"mode", f.mode.load(Ordering::Relaxed) as f32);
    set(c"soft", f.soft.load(Ordering::Relaxed) as f32 / 100.0);
    set(c"blur", f.blur.load(Ordering::Relaxed) as f32);
    set(c"have_mask", if f.tex_size == (1, 1) { 0.0 } else { 1.0 });
    a.gs_effect_set_texture(a.gs_effect_get_param_by_name(e, c"mask".as_ptr()), f.tex);
    a.obs_source_process_filter_end(f.source, e, w, h);
}

/// `enum gs_color_format` GS_R8; a texture libobs may update.
const GS_R8: std::ffi::c_int = 2;
const GS_DYNAMIC: u32 = 1 << 1;

/// Registers the kind (once per process, after the plugins load).
pub(crate) fn register(api: &Api) {
    if !API.load(Ordering::Acquire).is_null() {
        return;
    }
    API.store(api as *const Api as *mut Api, Ordering::Release);
    let info = SourceInfo {
        id: KIND.as_ptr(),
        kind: OBS_SOURCE_TYPE_FILTER,
        output_flags: OBS_SOURCE_VIDEO,
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
        video_tick: Some(video_tick),
        video_render: Some(video_render),
    };
    api.obs_register_source_s((&info as *const SourceInfo).cast(), std::mem::size_of::<SourceInfo>());
}
