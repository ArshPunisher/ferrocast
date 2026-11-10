//! Shader effects of one's own (Ferrocast's plugin filters, as OBS's Shaderfilter): a WGSL
//! function `effect(uv) -> vec4<f32>` that returns a straight colour for each point of the
//! picture, with helpers to read it. Checked before it's used: a shader that doesn't compile is
//! left out with why, never a broken frame.
//!
//! What an effect can use:
//! - `pic(uv)`: the picture at `uv` (0 to 1 each way), straight colour.
//! - `size()`: the picture's size in pixels.
//! - `time()`: seconds since the scene came on.
//! - `params()`: the filter's four sliders (0 to 1 unless set otherwise).

/// Shaders longer than this aren't taken.
pub const MAX_LEN: usize = 64 * 1024;

/// What every effect is compiled with (the same layout as `effects.wgsl`).
pub const PRELUDE: &str = r"
struct F {
    size: vec2<f32>,
    out_size: vec2<f32>,
    pad: f32,
    p0: f32,
    p1: f32,
    p2: f32,
    p3: f32,
    p4: f32,
    p5: f32,
    _pad: f32,
    color: vec4<f32>,
    q: vec4<f32>,
    r: vec4<f32>,
    m: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> f: F;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var pick: sampler;
@group(0) @binding(3) var second: texture_2d<f32>;
@group(0) @binding(4) var cube: texture_3d<f32>;

struct FxOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn fx_vs(@builtin(vertex_index) i: u32) -> FxOut {
    let uv = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    var o: FxOut;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn pic(uv: vec2<f32>) -> vec4<f32> {
    // Read as Ferrocast's own filters read it (straight, as libobs hands filters a picture).
    return textureSampleLevel(image, pick, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

fn size() -> vec2<f32> { return f.size; }
fn time() -> f32 { return f.p0; }
fn params() -> vec4<f32> { return f.q; }

@fragment
fn fx_fs(i: FxOut) -> @location(0) vec4<f32> {
    let c = clamp(effect(i.uv), vec4<f32>(0.0), vec4<f32>(1.0));
    return vec4<f32>(c.rgb * c.a, c.a);
}
";

/// Ready-made effects (the gallery, as OBS users get from Shaderfilter's examples): id, name
/// and code. Slider 1 is how strong it is, slider 2 a size or a speed.
pub const PRESETS: &[(&str, &str, &str)] = &[
    (
        "crt",
        "CRT screen",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let k = p.x * 0.004;
    let c = pic(uv);
    let rgb = vec3<f32>(pic(uv + vec2<f32>(k, 0.0)).r, c.g, pic(uv - vec2<f32>(k, 0.0)).b);
    let rows = mix(size().y * 0.25, size().y * 0.5, p.y);
    let line = 0.5 + 0.5 * sin(uv.y * rows * 6.28318);
    let d = uv - vec2<f32>(0.5);
    let shade = mix(1.0, 0.7 + 0.3 * line, p.x) * (1.0 - dot(d, d) * p.x * 1.4);
    return vec4<f32>(rgb * shade, c.a);
}",
    ),
    (
        "pixelate",
        "Pixelate",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let block = mix(4.0, 64.0, params().y);
    let cells = max(size() / block, vec2<f32>(1.0));
    let at = (floor(uv * cells) + vec2<f32>(0.5)) / cells;
    return mix(pic(uv), pic(at), params().x);
}",
    ),
    (
        "vignette",
        "Vignette",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let c = pic(uv);
    let inner = mix(0.15, 0.55, params().y);
    let v = 1.0 - smoothstep(inner, inner + 0.45, distance(uv, vec2<f32>(0.5)));
    return vec4<f32>(c.rgb * mix(1.0, v, params().x), c.a);
}",
    ),
    (
        "chromatic",
        "Colour split",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let o = (uv - vec2<f32>(0.5)) * params().x * mix(0.01, 0.06, params().y);
    let c = pic(uv);
    return vec4<f32>(pic(uv + o).r, c.g, pic(uv - o).b, c.a);
}",
    ),
    (
        "glitch",
        "Glitch",
        r"fn noise(x: f32) -> f32 {
    return fract(sin(x * 127.1) * 43758.547);
}

fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let tick = floor(time() * mix(2.0, 20.0, p.y));
    let band = floor(uv.y * 24.0);
    let on = step(1.0 - p.x * 0.5, noise(band + tick * 13.0));
    let shift = (noise(band * 3.1 + tick) - 0.5) * 0.08 * on;
    let x = fract(uv.x + shift);
    let c = pic(vec2<f32>(x, uv.y));
    let s = 0.006 * on;
    return vec4<f32>(pic(vec2<f32>(fract(x + s), uv.y)).r, c.g, pic(vec2<f32>(fract(x - s), uv.y)).b, c.a);
}",
    ),
    (
        "wave",
        "Wave",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let dx = sin(uv.y * mix(6.0, 40.0, p.y) + time() * 3.0) * p.x * 0.03;
    return pic(vec2<f32>(clamp(uv.x + dx, 0.0, 1.0), uv.y));
}",
    ),
    (
        "old_film",
        "Old film",
        r"fn grain(uv: vec2<f32>, t: f32) -> f32 {
    return fract(sin(dot(uv * 997.0 + vec2<f32>(t, t * 1.7), vec2<f32>(12.9898, 78.233))) * 43758.547);
}

fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let c = pic(uv);
    let y = dot(c.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let sepia = vec3<f32>(y * 1.07, y * 0.88, y * 0.68);
    let flicker = 1.0 - 0.06 * p.x * sin(time() * 23.0);
    let noise = (grain(uv, floor(time() * 24.0)) - 0.5) * 0.25 * p.y;
    let rgb = mix(c.rgb, sepia, p.x) * flicker + vec3<f32>(noise);
    return vec4<f32>(rgb, c.a);
}",
    ),
    (
        "night_vision",
        "Night vision",
        r"fn effect(uv: vec2<f32>) -> vec4<f32> {
    let p = params();
    let c = pic(uv);
    let y = dot(c.rgb, vec3<f32>(0.299, 0.587, 0.114));
    let lit = clamp(y * mix(1.2, 2.5, p.y), 0.0, 1.0);
    let green = vec3<f32>(lit * 0.25, lit, lit * 0.3);
    let d = uv - vec2<f32>(0.5);
    let ring = 1.0 - smoothstep(0.3, 0.75, length(d));
    return vec4<f32>(mix(c.rgb, green * ring, p.x), c.a);
}",
    ),
];

/// A ready-made effect's code, by its id.
pub fn preset(id: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(i, ..)| *i == id).map(|(.., code)| *code)
}

/// An effect a plugin brings (a checked WGSL `effect`, the same rules as one's own).
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEffect {
    /// The plugin's id ("com.example.crt").
    pub id: String,
    pub name: String,
    pub code: std::sync::Arc<str>,
}

/// The installed plugins' effects that are on; the app sets them at start and after a change.
static PLUGINS: std::sync::RwLock<Vec<PluginEffect>> = std::sync::RwLock::new(Vec::new());

/// What a plugin effect is called in a filter's settings (`preset`).
pub fn plugin_preset(id: &str) -> String {
    format!("plugin:{id}")
}

/// Replaces the plugin effects (the ones that are on).
pub fn set_plugins(list: Vec<PluginEffect>) {
    *PLUGINS.write().unwrap_or_else(|e| e.into_inner()) = list;
}

/// The plugin effects that are on, in the order they were set.
pub fn plugins() -> Vec<PluginEffect> {
    PLUGINS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The code of a filter's `preset`: a ready-made effect, or a plugin's (`plugin:<id>`) that is
/// installed and on.
pub fn effect_code(preset_id: &str) -> Option<std::sync::Arc<str>> {
    if let Some(id) = preset_id.strip_prefix("plugin:") {
        return PLUGINS.read().unwrap_or_else(|e| e.into_inner()).iter().find(|p| p.id == id).map(|p| p.code.clone());
    }
    preset(preset_id).map(std::sync::Arc::from)
}

/// The whole shader for an effect's code.
pub fn source(code: &str) -> String {
    format!("{PRELUDE}\n{code}\n")
}

/// Whether `code` can run, or why not (in words, the first problem).
pub fn check(code: &str) -> Result<(), String> {
    if code.trim().is_empty() {
        return Err("the shader is empty".into());
    }
    if code.len() > MAX_LEN {
        return Err(format!("the shader is longer than {} KB", MAX_LEN / 1024));
    }
    // The bindings and entry points are the prelude's: an effect only adds functions.
    for word in ["@group", "@binding", "@vertex", "@fragment", "@compute", "enable "] {
        if code.contains(word) {
            return Err(format!(
                "a shader effect can't use {}: write `fn effect(uv: vec2<f32>) -> vec4<f32>`",
                word.trim()
            ));
        }
    }
    let full = source(code);
    let module = naga::front::wgsl::parse_str(&full).map_err(|e| first_line(&e.emit_to_string(&full)))?;
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .map_err(|e| first_line(&e.emit_to_string(&full)))?;
    Ok(())
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("it doesn't compile")
        .trim_start_matches("error: ")
        .to_string()
}

/// A stable number for a shader's code (its pipeline is kept by it).
pub fn key(code: &str) -> u64 {
    // FNV-1a: steady between runs and builds.
    code.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVERT: &str =
        "fn effect(uv: vec2<f32>) -> vec4<f32> { let c = pic(uv); return vec4<f32>(1.0 - c.rgb, c.a); }";

    /// Every ready-made effect compiles (a broken one would be left out of the picture).
    #[test]
    fn every_ready_made_effect_compiles() {
        assert!(PRESETS.len() >= 8);
        for (id, name, code) in PRESETS {
            assert_eq!(check(code), Ok(()), "{name} ({id})");
            assert_eq!(preset(id), Some(*code));
        }
        let ids: std::collections::HashSet<_> = PRESETS.iter().map(|(i, ..)| *i).collect();
        assert_eq!(ids.len(), PRESETS.len(), "ids are unique");
        assert_eq!(preset("custom"), None);
        assert_eq!(preset("nope"), None);
    }

    #[test]
    fn a_good_effect_passes() {
        assert_eq!(check(INVERT), Ok(()));
        let wave = "fn effect(uv: vec2<f32>) -> vec4<f32> { let p = params(); \
                    return pic(uv + vec2<f32>(sin(uv.y * 20.0 + time()) * p.x * 0.05, 0.0)); }";
        assert_eq!(check(wave), Ok(()));
    }

    #[test]
    fn a_bad_one_says_why() {
        let e = check("fn effect(uv: vec2<f32>) -> vec4<f32> { return nope; }").unwrap_err();
        assert!(e.contains("nope"), "{e}");
        assert!(check("fn other() {}").is_err(), "no effect function");
        assert!(check("").is_err());
        assert!(check("@group(0) @binding(9) var<uniform> x: f32;").unwrap_err().contains("@group"));
        assert!(check(&"x".repeat(MAX_LEN + 1)).is_err());
    }

    #[test]
    fn keys_follow_the_code() {
        assert_eq!(key(INVERT), key(INVERT));
        assert_ne!(key(INVERT), key("x"));
    }
}

#[cfg(test)]
mod examples {
    /// The example effects shipped in docs/shaders all compile.
    #[test]
    fn the_examples_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/shaders");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x == "wgsl") {
                let code = std::fs::read_to_string(e.path()).unwrap_or_default();
                assert_eq!(super::check(&code), Ok(()), "{}", e.path().display());
                n += 1;
            }
        }
        assert!(n >= 5, "found {n}");
    }
}
