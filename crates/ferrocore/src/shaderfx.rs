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
