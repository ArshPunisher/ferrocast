//! Ferrocast's Rounded corners filter: a filter kind registered in libobs that rounds a source's
//! corners (a webcam in a rounded box, as streamers do with a mask image in OBS). The corners are
//! cut by a shader that measures each pixel's distance from a rounded box, so they're smooth at
//! any size; libobs turns the effect into Metal, D3D11 or OpenGL as the renderer needs.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

use super::{Api, GsEffect, ObsData, ObsProperties, ObsSource, Vec2};

/// The kind's id, saved in scene collections.
pub(crate) const KIND: &CStr = c"ferrocast_rounded_corners";

/// The shader: the source's picture, faded out past a rounded box the size of the source,
/// with a pixel of soft edge so the curve isn't jagged.
const EFFECT: &CStr = c"
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float2 size;
uniform float radius;

sampler_state textureSampler {
	Filter    = Linear;
	AddressU  = Clamp;
	AddressV  = Clamp;
};

struct VertData {
	float4 pos : POSITION;
	float2 uv  : TEXCOORD0;
};

VertData VSDefault(VertData v_in)
{
	VertData vert_out;
	vert_out.pos = mul(float4(v_in.pos.xyz, 1.0), ViewProj);
	vert_out.uv  = v_in.uv;
	return vert_out;
}

float4 PSRounded(VertData v_in) : TARGET
{
	float4 c = image.Sample(textureSampler, v_in.uv);
	float2 half_size = size * 0.5;
	float r = min(radius, min(half_size.x, half_size.y));
	float2 q = abs(v_in.uv * size - half_size) - (half_size - r);
	float d = length(max(q, float2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
	return float4(c.rgb, c.a * saturate(0.5 - d));
}

technique Draw
{
	pass
	{
		vertex_shader = VSDefault(v_in);
		pixel_shader  = PSRounded(v_in);
	}
}
";

/// The libobs Api, set once by `register` (a filter's callbacks take no context of ours).
static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

fn api() -> Option<&'static Api> {
    // SAFETY: set once from the Studio's Api, which lives until libobs shuts down, after the
    // last filter is destroyed.
    unsafe { API.load(Ordering::Acquire).as_ref() }
}

/// One filter: its source, its effect (made on the graphics thread) and the radius.
struct Filter {
    source: *mut ObsSource,
    effect: *mut GsEffect,
    radius: std::sync::atomic::AtomicU32,
}

unsafe extern "C" fn get_name(_type_data: *mut c_void) -> *const c_char {
    c"Rounded corners".as_ptr()
}

unsafe extern "C" fn create(settings: *mut ObsData, source: *mut ObsSource) -> *mut c_void {
    let Some(a) = api() else { return std::ptr::null_mut() };
    a.obs_enter_graphics();
    let mut err: *mut c_char = std::ptr::null_mut();
    let effect = a.gs_effect_create(EFFECT.as_ptr(), c"ferrocast_rounded_corners.effect".as_ptr(), &mut err);
    if !err.is_null() {
        // SAFETY: libobs filled `err` with a NUL-terminated message it allocated; read, then freed.
        tracing::warn!(error = %unsafe { CStr::from_ptr(err) }.to_string_lossy(), "the rounded corners shader didn't build");
        a.bfree(err.cast());
    }
    a.obs_leave_graphics();
    let f = Box::new(Filter { source, effect, radius: std::sync::atomic::AtomicU32::new(0) });
    let p = Box::into_raw(f);
    // SAFETY: `p` is the Filter just made; `update` only reads the settings libobs passes.
    unsafe { update(p.cast(), settings) };
    p.cast()
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: `data` is the Box made in `create`, destroyed once by libobs.
    let f = unsafe { Box::from_raw(data.cast::<Filter>()) };
    if !f.effect.is_null()
        && let Some(a) = api()
    {
        a.obs_enter_graphics();
        a.gs_effect_destroy(f.effect);
        a.obs_leave_graphics();
    }
}

unsafe extern "C" fn get_defaults(s: *mut ObsData) {
    if let Some(a) = api() {
        a.obs_data_set_default_int(s, c"radius".as_ptr(), 40);
    }
}

unsafe extern "C" fn get_properties(_data: *mut c_void) -> *mut ObsProperties {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let p = a.obs_properties_create();
    let r = a.obs_properties_add_int_slider(p, c"radius".as_ptr(), c"Corner radius".as_ptr(), 0, 1000, 1);
    if !r.is_null() {
        a.obs_property_int_set_suffix(r, c" px".as_ptr());
    }
    p
}

unsafe extern "C" fn update(data: *mut c_void, s: *mut ObsData) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter from `create`.
    let f = unsafe { &*data.cast::<Filter>() };
    let r = a.obs_data_get_int(s, c"radius".as_ptr()).clamp(0, 10_000) as u32;
    f.radius.store(r, Ordering::Relaxed);
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut GsEffect) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter; libobs renders on its graphics thread.
    let f = unsafe { &*data.cast::<Filter>() };
    let radius = f.radius.load(Ordering::Relaxed);
    let target = a.obs_filter_get_target(f.source);
    let (w, h) = if target.is_null() {
        (0, 0)
    } else {
        (a.obs_source_get_base_width(target), a.obs_source_get_base_height(target))
    };
    // Nothing to round: the picture goes on untouched.
    if f.effect.is_null() || radius == 0 || w == 0 || h == 0 {
        a.obs_source_skip_video_filter(f.source);
        return;
    }
    if !a.obs_source_process_filter_begin(f.source, super::GS_RGBA, OBS_ALLOW_DIRECT_RENDERING) {
        return;
    }
    let size = Vec2 { x: w as f32, y: h as f32 };
    a.gs_effect_set_vec2(a.gs_effect_get_param_by_name(f.effect, c"size".as_ptr()), &size);
    a.gs_effect_set_float(a.gs_effect_get_param_by_name(f.effect, c"radius".as_ptr()), radius as f32);
    a.obs_source_process_filter_end(f.source, f.effect, w, h);
}

/// `enum obs_allow_direct_render`: the source may draw straight into the filter.
const OBS_ALLOW_DIRECT_RENDERING: c_int = 1;
/// `enum obs_source_type` OBS_SOURCE_TYPE_FILTER; `OBS_SOURCE_VIDEO`.
const OBS_SOURCE_TYPE_FILTER: c_int = 1;
const OBS_SOURCE_VIDEO: u32 = 1 << 0;

/// `struct obs_source_info` up to `video_render` (libobs 32.2's order); libobs takes the size
/// given and zero-fills the rest.
#[repr(C)]
struct SourceInfo {
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
}

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
        video_tick: None,
        video_render: Some(video_render),
    };
    api.obs_register_source_s((&info as *const SourceInfo).cast(), std::mem::size_of::<SourceInfo>());
}
