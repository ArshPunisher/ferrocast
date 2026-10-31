//! Ferrocast's own picture filters, registered in libobs: each is a shader and a few settings.
//! Rounded corners, Border, Shadow and glow, Shape, Blur or pixelate an area, and Colour looks.
//! libobs turns the effects into Metal, D3D11 or OpenGL as the renderer needs; the `filters`
//! example checks each pixel by pixel.
//!
//! A filter may draw past its source's edges (a shadow): it says how many pixels with `pad`,
//! is that much bigger on every side, and its shader maps the output back to the source.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::{Api, GsEffect, ObsData, ObsProperties, ObsSource, Vec2, Vec4};

/// What a setting is, as the options form shows it.
pub(crate) enum Kind {
    /// A slider of whole numbers, with its unit.
    Int { min: i32, max: i32, default: i64, suffix: &'static CStr },
    /// A colour with transparency (0xAABBGGRR, as libobs keeps colours).
    Color(i64),
    /// A switch.
    Bool(bool),
    /// One of a list (its number goes to the shader).
    Choice(&'static [(&'static CStr, i64)], i64),
}

/// A setting: its key in the saved settings, its label, and the shader's uniform it feeds.
pub(crate) struct Param {
    key: &'static CStr,
    label: &'static CStr,
    kind: Kind,
    uniform: &'static CStr,
}

/// A filter kind.
pub(crate) struct Def {
    /// The kind's id, saved in scene collections.
    pub id: &'static CStr,
    pub name: &'static CStr,
    /// The pixel shader's body: `float4 PSMain(VertData v_in) : TARGET { … }` and helpers.
    body: &'static str,
    params: &'static [Param],
    /// How far it draws past the source on every side (pixels), from the settings' values.
    pad: fn(&[[f32; 4]]) -> f32,
}

const fn int(
    key: &'static CStr,
    label: &'static CStr,
    min: i32,
    max: i32,
    default: i64,
    suffix: &'static CStr,
) -> Param {
    Param { key, label, kind: Kind::Int { min, max, default, suffix }, uniform: key }
}

fn no_pad(_: &[[f32; 4]]) -> f32 {
    0.0
}

/// Every filter's shader starts with this: the uniforms libobs and the filter set, the vertex
/// shader, and helpers in source pixels.
const HEADER: &str = "
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float2 size;
uniform float2 out_size;
uniform float pad;

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

float2 src_px(float2 uv)
{
	return uv * out_size - float2(pad, pad);
}

float4 src_at(float2 px)
{
	float2 uv = px / size;
	float inside = step(0.0, uv.x) * step(0.0, uv.y) * step(uv.x, 1.0) * step(uv.y, 1.0);
	return image.Sample(textureSampler, uv) * inside;
}

float rbox(float2 px, float2 half_size, float r)
{
	float2 q = abs(px - half_size) - (half_size - float2(r, r));
	return length(max(q, float2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}
";

const FOOTER: &str = "
technique Draw
{
	pass
	{
		vertex_shader = VSDefault(v_in);
		pixel_shader  = PSMain(v_in);
	}
}
";

const ROUNDED: &str = "
uniform float radius;

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float2 h = size * 0.5;
	float d = rbox(p, h, min(radius, min(h.x, h.y)));
	return float4(c.rgb, c.a * saturate(0.5 - d));
}
";

const BORDER: &str = "
uniform float width;
uniform float4 color;
uniform float radius;

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float2 h = size * 0.5;
	float d = rbox(p, h, min(radius, min(h.x, h.y)));
	float shape = saturate(0.5 - d);
	float band = shape * saturate(d + width + 0.5);
	float k = band * color.a;
	return float4(lerp(c.rgb, color.rgb, k), lerp(c.a, 1.0, k) * shape);
}
";

const SHADOW: &str = "
uniform float4 color;
uniform float blur;
uniform float off_x;
uniform float off_y;

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float2 q = p - float2(off_x, off_y);
	float acc = 0.0;
	float total = 0.0;
	// A soft copy of the source's shape: 48 samples on a spiral, nearer ones counting more.
	for (int i = 0; i < 48; i++) {
		float t = (float(i) + 0.5) / 48.0;
		float ang = float(i) * 2.39996323;
		float2 o = float2(cos(ang), sin(ang)) * (sqrt(t) * blur);
		float w = 1.0 - t * 0.5;
		acc += src_at(q + o).a * w;
		total += w;
	}
	float sa = acc / total * color.a;
	float a = c.a + sa * (1.0 - c.a);
	float3 rgb = (c.rgb * c.a + color.rgb * sa * (1.0 - c.a)) / max(a, 0.0001);
	return float4(rgb, a);
}
";

fn shadow_pad(v: &[[f32; 4]]) -> f32 {
    // color, blur, x, y
    (v[1][0] + v[2][0].abs().max(v[3][0].abs())).ceil()
}

const SHAPE: &str = "
uniform float shape;
uniform float feather;
uniform float stretch;

float sd_hexagon(float2 p)
{
	p = abs(p);
	float2 k = float2(-0.866025404, 0.5);
	p -= 2.0 * min(dot(k, p), 0.0) * k;
	p -= float2(clamp(p.x, -0.577350269, 0.577350269), 1.0);
	return length(p) * sign(p.y);
}

float sd_star(float2 p)
{
	float2 k1 = float2(0.809016994375, -0.587785252292);
	float2 k2 = float2(-k1.x, k1.y);
	p.x = abs(p.x);
	p -= 2.0 * max(dot(k1, p), 0.0) * k1;
	p -= 2.0 * max(dot(k2, p), 0.0) * k2;
	p.x = abs(p.x);
	p.y -= 1.0;
	float2 ba = 0.45 * float2(-k1.y, k1.x) - float2(0.0, 1.0);
	float h = clamp(dot(p, ba) / dot(ba, ba), 0.0, 1.0);
	return length(p - ba * h) * sign(p.y * ba.x - p.x * ba.y);
}

float sd_heart(float2 p)
{
	p.x = abs(p.x);
	if (p.y + p.x > 1.0) {
		float2 a = p - float2(0.25, 0.75);
		return sqrt(dot(a, a)) - 0.353553391;
	}
	float2 b = p - float2(0.0, 1.0);
	float2 m = p - 0.5 * max(p.x + p.y, 0.0);
	return sqrt(min(dot(b, b), dot(m, m))) * sign(p.x - p.y);
}

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float2 h = size * 0.5;
	// Fitted in the middle (a circle in a 16:9 camera), or stretched to its edges.
	float2 r = stretch > 0.5 ? h : float2(min(h.x, h.y), min(h.x, h.y));
	float2 n = (p - h) / r;
	float s = min(r.x, r.y);
	float d;
	if (shape < 0.5) {
		d = length(n) - 1.0;
	} else if (shape < 1.5) {
		// Its points reach 1.155 across: scaled to fit the box.
		d = sd_hexagon(n * 1.1547) / 1.1547;
	} else if (shape < 2.5) {
		d = (abs(n.x) + abs(n.y) - 1.0) * 0.707106781;
	} else if (shape < 3.5) {
		d = sd_star(float2(n.x, -n.y) * 1.1);
	} else {
		// The heart's own space: its tip at (0, 0), about 1.2 wide and 1.1 high, y up.
		d = sd_heart(float2(n.x * 0.6, (1.0 - n.y) * 0.55)) / 0.58;
	}
	float edge = max(feather, 1.0);
	return float4(c.rgb, c.a * saturate(0.5 - d * s / edge));
}
";

const BLUR_AREA: &str = "
uniform float mode;
uniform float area_x;
uniform float area_y;
uniform float area_w;
uniform float area_h;
uniform float strength;

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float2 lo = float2(area_x, area_y) * 0.01 * size;
	float2 hi = lo + float2(area_w, area_h) * 0.01 * size;
	float inside = step(lo.x, p.x) * step(lo.y, p.y) * step(p.x, hi.x) * step(p.y, hi.y);
	if (inside < 0.5)
		return c;
	if (mode > 0.5) {
		float block = max(strength, 1.0);
		return src_at((floor(p / block) + 0.5) * block);
	}
	float4 acc = float4(0.0, 0.0, 0.0, 0.0);
	for (int i = 0; i < 64; i++) {
		float t = (float(i) + 0.5) / 64.0;
		float ang = float(i) * 2.39996323;
		// Only the picture's own pixels: outside it would darken the edges.
		acc += src_at(clamp(p + float2(cos(ang), sin(ang)) * (sqrt(t) * strength), float2(0.0, 0.0), size - 1.0));
	}
	return acc / 64.0;
}
";

const LOOK: &str = "
uniform float look;
uniform float strength;

float4 PSMain(VertData v_in) : TARGET
{
	float2 p = src_px(v_in.uv);
	float4 c = src_at(p);
	float3 x = c.rgb;
	float l = dot(x, float3(0.2126, 0.7152, 0.0722));
	float3 g = x;
	if (look < 0.5) {
		g = x * float3(1.08, 1.0, 0.86);
	} else if (look < 1.5) {
		g = x * float3(0.88, 1.0, 1.1);
	} else if (look < 2.5) {
		g = lerp(float3(l, l, l), x, 1.15) + (l - 0.5) * float3(0.16, 0.02, -0.16);
	} else if (look < 3.5) {
		g = float3(l, l, l);
	} else if (look < 4.5) {
		g = float3(dot(x, float3(0.393, 0.769, 0.189)), dot(x, float3(0.349, 0.686, 0.168)), dot(x, float3(0.272, 0.534, 0.131)));
		g = g * 0.82 + 0.08;
	} else if (look < 5.5) {
		g = (x - 0.5) * 1.2 + 0.5;
		g = lerp(float3(l, l, l), g, 1.3);
	} else {
		g = lerp(float3(l, l, l), x, 0.8) * 0.85 + 0.1;
	}
	return float4(lerp(x, saturate(g), strength * 0.01), c.a);
}
";

const SHAPES: &[(&CStr, i64)] = &[(c"Circle", 0), (c"Hexagon", 1), (c"Diamond", 2), (c"Star", 3), (c"Heart", 4)];
const BLUR_MODES: &[(&CStr, i64)] = &[(c"Blur", 0), (c"Pixelate", 1)];
const LOOKS: &[(&CStr, i64)] = &[
    (c"Warm", 0),
    (c"Cool", 1),
    (c"Teal and orange", 2),
    (c"Black and white", 3),
    (c"Vintage", 4),
    (c"Punchy", 5),
    (c"Soft fade", 6),
];

pub(crate) const DEFS: [Def; 6] = [
    Def {
        id: c"ferrocast_rounded_corners",
        name: c"Rounded corners",
        body: ROUNDED,
        params: &[int(c"radius", c"Corner radius", 0, 1000, 40, c" px")],
        pad: no_pad,
    },
    Def {
        id: c"ferrocast_border",
        name: c"Border",
        body: BORDER,
        params: &[
            int(c"width", c"Width", 1, 200, 8, c" px"),
            Param { key: c"color", label: c"Colour", kind: Kind::Color(0xFFFF_FFFF), uniform: c"color" },
            int(c"radius", c"Corner radius", 0, 1000, 0, c" px"),
        ],
        pad: no_pad,
    },
    Def {
        id: c"ferrocast_shadow",
        name: c"Shadow and glow",
        body: SHADOW,
        params: &[
            Param { key: c"color", label: c"Colour", kind: Kind::Color(0xB300_0000), uniform: c"color" },
            int(c"blur", c"Softness", 0, 200, 24, c" px"),
            Param {
                key: c"x",
                label: c"Across",
                kind: Kind::Int { min: -200, max: 200, default: 0, suffix: c" px" },
                uniform: c"off_x",
            },
            Param {
                key: c"y",
                label: c"Down",
                kind: Kind::Int { min: -200, max: 200, default: 12, suffix: c" px" },
                uniform: c"off_y",
            },
        ],
        pad: shadow_pad,
    },
    Def {
        id: c"ferrocast_shape",
        name: c"Shape",
        body: SHAPE,
        params: &[
            Param { key: c"shape", label: c"Shape", kind: Kind::Choice(SHAPES, 0), uniform: c"shape" },
            int(c"feather", c"Soft edge", 0, 200, 0, c" px"),
            Param { key: c"stretch", label: c"Stretch to the edges", kind: Kind::Bool(false), uniform: c"stretch" },
        ],
        pad: no_pad,
    },
    Def {
        id: c"ferrocast_blur_area",
        name: c"Blur or pixelate an area",
        body: BLUR_AREA,
        params: &[
            Param { key: c"mode", label: c"How", kind: Kind::Choice(BLUR_MODES, 0), uniform: c"mode" },
            int(c"strength", c"Strength", 1, 200, 24, c" px"),
            Param {
                key: c"area_x",
                label: c"From the left",
                kind: Kind::Int { min: 0, max: 100, default: 25, suffix: c"%" },
                uniform: c"area_x",
            },
            Param {
                key: c"area_y",
                label: c"From the top",
                kind: Kind::Int { min: 0, max: 100, default: 25, suffix: c"%" },
                uniform: c"area_y",
            },
            Param {
                key: c"area_w",
                label: c"Width",
                kind: Kind::Int { min: 1, max: 100, default: 50, suffix: c"%" },
                uniform: c"area_w",
            },
            Param {
                key: c"area_h",
                label: c"Height",
                kind: Kind::Int { min: 1, max: 100, default: 50, suffix: c"%" },
                uniform: c"area_h",
            },
        ],
        pad: no_pad,
    },
    Def {
        id: c"ferrocast_look",
        name: c"Colour look",
        body: LOOK,
        params: &[
            Param { key: c"look", label: c"Look", kind: Kind::Choice(LOOKS, 0), uniform: c"look" },
            int(c"strength", c"Strength", 0, 100, 100, c"%"),
        ],
        pad: no_pad,
    },
];

/// The libobs Api, set once by `register` (a filter's callbacks take no context of ours).
static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

fn api() -> Option<&'static Api> {
    // SAFETY: set once from the Studio's Api, which lives until libobs shuts down, after the
    // last filter is destroyed.
    unsafe { API.load(Ordering::Acquire).as_ref() }
}

/// One filter: its kind, its source, its effect (made on the graphics thread) and the values
/// its settings give the shader (a colour as four numbers, anything else in the first).
struct Filter {
    def: usize,
    source: *mut ObsSource,
    effect: *mut GsEffect,
    values: Mutex<Vec<[f32; 4]>>,
}

/// A colour libobs keeps as 0xAABBGGRR → red, green, blue, alpha from 0 to 1.
fn rgba(v: i64) -> [f32; 4] {
    let c = |shift: u32| ((v >> shift) & 0xff) as f32 / 255.0;
    [c(0), c(8), c(16), c(24)]
}

unsafe extern "C" fn get_name<const I: usize>(_type_data: *mut c_void) -> *const c_char {
    DEFS[I].name.as_ptr()
}

unsafe extern "C" fn create<const I: usize>(settings: *mut ObsData, source: *mut ObsSource) -> *mut c_void {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let def = &DEFS[I];
    let text = CString::new(format!("{HEADER}{}{FOOTER}", def.body)).unwrap_or_default();
    let file = CString::new(format!("{}.effect", def.id.to_string_lossy())).unwrap_or_default();
    a.obs_enter_graphics();
    let mut err: *mut c_char = std::ptr::null_mut();
    let effect = a.gs_effect_create(text.as_ptr(), file.as_ptr(), &mut err);
    if !err.is_null() {
        // SAFETY: libobs filled `err` with a NUL-terminated message it allocated; read, then freed.
        let e = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        tracing::warn!(filter = %def.id.to_string_lossy(), error = %e, "a filter's shader didn't build");
        a.bfree(err.cast());
    }
    a.obs_leave_graphics();
    let f = Box::new(Filter { def: I, source, effect, values: Mutex::new(vec![[0.0; 4]; def.params.len()]) });
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

unsafe extern "C" fn get_defaults<const I: usize>(s: *mut ObsData) {
    let Some(a) = api() else { return };
    for p in DEFS[I].params {
        match p.kind {
            Kind::Int { default, .. } | Kind::Color(default) | Kind::Choice(_, default) => {
                a.obs_data_set_default_int(s, p.key.as_ptr(), default);
            }
            Kind::Bool(default) => a.obs_data_set_default_bool(s, p.key.as_ptr(), default),
        }
    }
}

unsafe extern "C" fn get_properties<const I: usize>(_data: *mut c_void) -> *mut ObsProperties {
    let Some(a) = api() else { return std::ptr::null_mut() };
    let props = a.obs_properties_create();
    for p in DEFS[I].params {
        match p.kind {
            Kind::Int { min, max, suffix, .. } => {
                let prop = a.obs_properties_add_int_slider(props, p.key.as_ptr(), p.label.as_ptr(), min, max, 1);
                if !prop.is_null() {
                    a.obs_property_int_set_suffix(prop, suffix.as_ptr());
                }
            }
            Kind::Color(_) => {
                a.obs_properties_add_color_alpha(props, p.key.as_ptr(), p.label.as_ptr());
            }
            Kind::Bool(_) => {
                a.obs_properties_add_bool(props, p.key.as_ptr(), p.label.as_ptr());
            }
            Kind::Choice(choices, _) => {
                let list =
                    a.obs_properties_add_list(props, p.key.as_ptr(), p.label.as_ptr(), COMBO_LIST, COMBO_FORMAT_INT);
                if !list.is_null() {
                    for (name, value) in choices {
                        a.obs_property_list_add_int(list, name.as_ptr(), *value);
                    }
                }
            }
        }
    }
    props
}

unsafe extern "C" fn update(data: *mut c_void, s: *mut ObsData) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter from `create`.
    let f = unsafe { &*data.cast::<Filter>() };
    let values: Vec<[f32; 4]> = DEFS[f.def]
        .params
        .iter()
        .map(|p| match p.kind {
            Kind::Int { min, max, .. } => {
                let v = a.obs_data_get_int(s, p.key.as_ptr()).clamp(i64::from(min), i64::from(max));
                [v as f32, 0.0, 0.0, 0.0]
            }
            Kind::Color(_) => rgba(a.obs_data_get_int(s, p.key.as_ptr())),
            Kind::Bool(_) => [if a.obs_data_get_bool(s, p.key.as_ptr()) { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            Kind::Choice(..) => [a.obs_data_get_int(s, p.key.as_ptr()) as f32, 0.0, 0.0, 0.0],
        })
        .collect();
    if let Ok(mut v) = f.values.lock() {
        *v = values;
    }
}

/// The source's size and the filter's padding (pixels on every side).
fn sizes(a: &Api, f: &Filter) -> (u32, u32, f32) {
    let target = a.obs_filter_get_target(f.source);
    if target.is_null() {
        return (0, 0, 0.0);
    }
    let pad = f.values.lock().map(|v| (DEFS[f.def].pad)(&v)).unwrap_or(0.0).clamp(0.0, 1000.0);
    (a.obs_source_get_base_width(target), a.obs_source_get_base_height(target), pad)
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    let Some(a) = api() else { return 0 };
    // SAFETY: `data` is the live Filter from `create`.
    let (w, _, pad) = sizes(a, unsafe { &*data.cast::<Filter>() });
    if w == 0 { 0 } else { w + 2 * pad as u32 }
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    let Some(a) = api() else { return 0 };
    // SAFETY: as for get_width.
    let (_, h, pad) = sizes(a, unsafe { &*data.cast::<Filter>() });
    if h == 0 { 0 } else { h + 2 * pad as u32 }
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut GsEffect) {
    let Some(a) = api() else { return };
    // SAFETY: `data` is the live Filter; libobs renders on its graphics thread.
    let f = unsafe { &*data.cast::<Filter>() };
    let (w, h, pad) = sizes(a, f);
    if f.effect.is_null() || w == 0 || h == 0 {
        a.obs_source_skip_video_filter(f.source);
        return;
    }
    let Ok(values) = f.values.lock().map(|v| v.clone()) else {
        a.obs_source_skip_video_filter(f.source);
        return;
    };
    if !a.obs_source_process_filter_begin(f.source, super::GS_RGBA, OBS_ALLOW_DIRECT_RENDERING) {
        return;
    }
    let (ow, oh) = (w + 2 * pad as u32, h + 2 * pad as u32);
    let set2 = |name: &CStr, v: Vec2| a.gs_effect_set_vec2(a.gs_effect_get_param_by_name(f.effect, name.as_ptr()), &v);
    set2(c"size", Vec2 { x: w as f32, y: h as f32 });
    set2(c"out_size", Vec2 { x: ow as f32, y: oh as f32 });
    a.gs_effect_set_float(a.gs_effect_get_param_by_name(f.effect, c"pad".as_ptr()), pad);
    for (p, v) in DEFS[f.def].params.iter().zip(&values) {
        let param = a.gs_effect_get_param_by_name(f.effect, p.uniform.as_ptr());
        match p.kind {
            Kind::Color(_) => a.gs_effect_set_vec4(param, &Vec4 { x: v[0], y: v[1], z: v[2], w: v[3] }),
            _ => a.gs_effect_set_float(param, v[0]),
        }
    }
    a.obs_source_process_filter_end(f.source, f.effect, ow, oh);
}

/// `enum obs_allow_direct_render`: the source may draw straight into the filter.
const OBS_ALLOW_DIRECT_RENDERING: c_int = 1;
/// `enum obs_combo_type` OBS_COMBO_TYPE_LIST, `enum obs_combo_format` OBS_COMBO_FORMAT_INT.
const COMBO_LIST: c_int = 2;
const COMBO_FORMAT_INT: c_int = 1;
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

fn register_one<const I: usize>(api: &Api) {
    let info = SourceInfo {
        id: DEFS[I].id.as_ptr(),
        kind: OBS_SOURCE_TYPE_FILTER,
        output_flags: OBS_SOURCE_VIDEO,
        get_name: Some(get_name::<I>),
        create: Some(create::<I>),
        destroy: Some(destroy),
        get_width: Some(get_width),
        get_height: Some(get_height),
        get_defaults: Some(get_defaults::<I>),
        get_properties: Some(get_properties::<I>),
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

/// Registers every kind (once per process, after the plugins load).
pub(crate) fn register(api: &Api) {
    if !API.load(Ordering::Acquire).is_null() {
        return;
    }
    API.store(api as *const Api as *mut Api, Ordering::Release);
    register_one::<0>(api);
    register_one::<1>(api);
    register_one::<2>(api);
    register_one::<3>(api);
    register_one::<4>(api);
    register_one::<5>(api);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_registered_once_with_its_settings() {
        let ids: std::collections::HashSet<_> = DEFS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DEFS.len(), "ids are unique");
        for d in &DEFS {
            let keys: std::collections::HashSet<_> = d.params.iter().map(|p| p.key).collect();
            assert_eq!(keys.len(), d.params.len(), "{:?}: keys are unique", d.id);
            for p in d.params {
                let u = p.uniform.to_str().unwrap();
                let declared = [format!("uniform float {u};"), format!("uniform float4 {u};")];
                assert!(declared.iter().any(|x| d.body.contains(x.as_str())), "{:?} declares {u}", d.id);
            }
        }
    }

    #[test]
    fn colours_come_apart_as_libobs_keeps_them() {
        assert_eq!(rgba(0xB300_0000), [0.0, 0.0, 0.0, 179.0 / 255.0]);
        assert_eq!(rgba(0xFF00_00FF), [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_shadow_pads_by_its_softness_and_offset() {
        let v = [[0.0; 4], [24.0, 0.0, 0.0, 0.0], [-5.0, 0.0, 0.0, 0.0], [12.0, 0.0, 0.0, 0.0]];
        assert_eq!(shadow_pad(&v), 36.0);
    }
}
