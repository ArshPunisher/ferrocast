//! Opening an OBS scene collection in our engine (ENGINE.md E7): the JSON OBS (and Ferrocast's
//! Studio) saves becomes the engine's sources and scenes. Every source kind, filter and setting
//! the engine has is carried over; anything else is listed by name in `skipped`, so the app can
//! say what still needs OBS's engine.

use serde_json::Value;

use crate::afx::AudioEffect;
use crate::engine::{SceneItem, SourceKind};
use crate::mix::{Monitoring, SourceSettings};
use crate::source::text::{TextAlign, TextStyle};
use crate::{Blend, Bounds, Effect, Filter, MaskKind, Placement};

/// A source as the engine takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSource {
    pub id: u64,
    pub name: String,
    pub kind: SourceKind,
    /// Its sound settings, for kinds with sound.
    pub sound: Option<SourceSettings>,
    pub sound_effects: Vec<AudioEffect>,
    /// Heard whatever is on air: OBS's Desktop Audio and Mic/Aux channels.
    pub global: bool,
}

/// Where OBS keeps its global sound sources in a collection.
const CHANNELS: [&str; 6] = [
    "DesktopAudioDevice1",
    "DesktopAudioDevice2",
    "AuxAudioDevice1",
    "AuxAudioDevice2",
    "AuxAudioDevice3",
    "AuxAudioDevice4",
];

/// A collection as the engine takes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Imported {
    pub sources: Vec<ImportedSource>,
    /// Scenes in OBS's order, each with its items bottom first.
    pub scenes: Vec<(String, Vec<SceneItem>)>,
    pub current: Option<String>,
    /// Which of `scenes` are groups (kept with the scenes so items can show them; not listed
    /// as scenes in the app).
    pub groups: Vec<String>,
    /// What couldn't come over yet ("Browser: OBS's Browser source"), for the app to say.
    pub skipped: Vec<String>,
}

fn f(v: &Value, k: &str, d: f64) -> f32 {
    v.get(k).and_then(Value::as_f64).unwrap_or(d) as f32
}

fn color(v: u64) -> [u8; 4] {
    let v = v as u32;
    [(v & 0xff) as u8, ((v >> 8) & 0xff) as u8, ((v >> 16) & 0xff) as u8, ((v >> 24) & 0xff) as u8]
}

/// A source's picture filter, from OBS's id and settings; None for one the engine lacks.
pub fn effect_from(kind: &str, s: &Value) -> Option<Effect> {
    let n = |k: &str, d: f64| f(s, k, d);
    let key_of = |types: &[(&str, u32)], d: u32| -> [u8; 3] {
        let t = s.get("key_color_type").and_then(Value::as_str).unwrap_or("green");
        let k = types.iter().find(|(name, _)| *name == t).map(|(_, c)| *c).unwrap_or_else(|| {
            if t == "custom" { s.get("key_color").and_then(Value::as_u64).unwrap_or(u64::from(d)) as u32 } else { d }
        });
        [(k & 0xff) as u8, ((k >> 8) & 0xff) as u8, ((k >> 16) & 0xff) as u8]
    };
    Some(match kind {
        "ferrocast_rounded_corners" => Effect::RoundedCorners { radius: n("radius", 40.0) },
        "ferrocast_border" => Effect::Border {
            width: n("width", 8.0),
            rgba: Effect::rgba(s.get("color").and_then(Value::as_u64).unwrap_or(0xFFFF_FFFF) as u32),
            radius: n("radius", 0.0),
        },
        "ferrocast_shadow" => Effect::Shadow {
            rgba: Effect::rgba(s.get("color").and_then(Value::as_u64).unwrap_or(0xB300_0000) as u32),
            blur: n("blur", 24.0),
            x: n("x", 0.0),
            y: n("y", 12.0),
        },
        "ferrocast_shape" => Effect::Shape {
            shape: n("shape", 0.0) as u32,
            feather: n("feather", 0.0),
            stretch: s.get("stretch").and_then(Value::as_bool).unwrap_or(false),
        },
        "ferrocast_blur_area" => Effect::BlurArea {
            pixelate: n("mode", 0.0) > 0.5,
            strength: n("strength", 24.0),
            area: [n("area_x", 25.0), n("area_y", 25.0), n("area_w", 50.0), n("area_h", 50.0)],
        },
        "ferrocast_look" => Effect::Look { look: n("look", 0.0) as u32, strength: n("strength", 100.0) },
        "chroma_key_filter_v2" => Effect::ChromaKey {
            key: key_of(&[("green", 0x00FF00), ("blue", 0xFF9900), ("magenta", 0xFF00FF)], 0x00FF00),
            similarity: n("similarity", 400.0),
            smoothness: n("smoothness", 80.0),
            spill: n("spill", 100.0),
            opacity: n("opacity", 1.0),
            contrast: n("contrast", 0.0),
            brightness: n("brightness", 0.0),
            gamma: n("gamma", 0.0),
        },
        "color_key_filter_v2" => Effect::ColorKey {
            key: key_of(&[("green", 0x00FF00), ("blue", 0xFF0000), ("red", 0x0000FF), ("magenta", 0xFF00FF)], 0x00FF00),
            similarity: n("similarity", 80.0),
            smoothness: n("smoothness", 50.0),
            opacity: n("opacity", 1.0),
            contrast: n("contrast", 0.0),
            brightness: n("brightness", 0.0),
            gamma: n("gamma", 0.0),
        },
        "color_filter_v2" => Effect::ColorCorrection {
            gamma: n("gamma", 0.0),
            contrast: n("contrast", 0.0),
            brightness: n("brightness", 0.0),
            saturation: n("saturation", 0.0),
            hue_shift: n("hue_shift", 0.0),
            opacity: n("opacity", 1.0),
            multiply: s.get("color_multiply").and_then(Value::as_u64).unwrap_or(0x00FF_FFFF) as u32,
            add: s.get("color_add").and_then(Value::as_u64).unwrap_or(0) as u32,
        },
        "luma_key_filter_v2" => Effect::LumaKey {
            max: n("luma_max", 1.0),
            min: n("luma_min", 0.0),
            max_smooth: n("luma_max_smooth", 0.0),
            min_smooth: n("luma_min_smooth", 0.0),
        },
        "sharpness_filter_v2" => Effect::Sharpen { sharpness: n("sharpness", 0.08) },
        "scroll_filter" => {
            let size = |on: &str, k: &str| {
                s.get(on)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(|| s.get(k).and_then(Value::as_u64).unwrap_or(100) as u32)
            };
            Effect::Scroll {
                speed_x: n("speed_x", 0.0),
                speed_y: n("speed_y", 0.0),
                width: size("limit_cx", "cx"),
                height: size("limit_cy", "cy"),
                looping: s.get("loop").and_then(Value::as_bool).unwrap_or(true),
                elapsed_ms: 0,
            }
        }
        // Its table is read here; one that won't read leaves the filter out (listed as skipped).
        "clut_filter" => Effect::Lut {
            lut: std::sync::Arc::new(
                crate::lut::Lut::open(std::path::Path::new(s.get("image_path").and_then(Value::as_str)?)).ok()?,
            ),
            amount: n("clut_amount", 1.0).clamp(0.0, 1.0),
            passthrough_alpha: s.get("passthrough_alpha").and_then(Value::as_bool).unwrap_or(false),
        },
        "crop_filter" => {
            let i = |k: &str| s.get(k).and_then(Value::as_i64).unwrap_or(0).clamp(-16_384, 16_384) as i32;
            let relative = s.get("relative").and_then(Value::as_bool).unwrap_or(true);
            Effect::Crop {
                left: i("left"),
                top: i("top"),
                right: i("right"),
                bottom: i("bottom"),
                size: (!relative).then(|| (i("cx").max(0) as u32, i("cy").max(0) as u32)),
            }
        }
        _ => return None,
    })
}

/// OBS's Image Mask/Blend with its picture under `image`, and the picture's file; None without
/// one. v2 (OBS 27 on) takes its tint as sRGB and its opacity from 0 to 1; v1 neither.
fn mask_from(kind: &str, s: &Value, image: u64) -> Option<(Effect, std::path::PathBuf)> {
    let path = s.get("image_path").and_then(Value::as_str).filter(|p| !p.is_empty())?;
    let v2 = kind == "mask_filter_v2";
    let mask = match s.get("type").and_then(Value::as_str).unwrap_or("mask_color_filter.effect") {
        "mask_alpha_filter.effect" => MaskKind::Alpha,
        "blend_mul_filter.effect" => MaskKind::Multiply,
        "blend_add_filter.effect" => MaskKind::Add,
        "blend_sub_filter.effect" => MaskKind::Subtract,
        _ => MaskKind::Color,
    };
    let c = Effect::rgba(s.get("color").and_then(Value::as_u64).unwrap_or(0x00FF_FFFF) as u32);
    let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let opacity = if v2 { f(s, "opacity", 1.0) } else { f(s, "opacity", 100.0) / 100.0 };
    let rgba = if v2 { [lin(c[0]), lin(c[1]), lin(c[2]), opacity] } else { [c[0], c[1], c[2], opacity] };
    let stretch = s.get("stretch").and_then(Value::as_bool).unwrap_or(false);
    Some((Effect::Mask { kind: mask, image, rgba, stretch }, path.into()))
}

/// A source's sound filter, from OBS's id and settings.
/// A shader effect of one's own from its filter's settings: the WGSL in `shader_file` (a file)
/// or else `shader_text`, and four sliders `param1`…`param4` (0 to 1). Err says why it can't run.
pub fn shader_from(s: &Value) -> Result<Effect, String> {
    let file = s.get("shader_file").and_then(Value::as_str).unwrap_or_default().trim();
    let code = if file.is_empty() {
        s.get("shader_text").and_then(Value::as_str).unwrap_or_default().to_string()
    } else {
        let bytes = crate::read_file(std::path::Path::new(file), crate::shaderfx::MAX_LEN as u64).map_err(|e| {
            if e.to_string().contains("bigger") {
                format!("{file} is longer than {} KB", crate::shaderfx::MAX_LEN / 1024)
            } else {
                format!("{file} can't be read ({e})")
            }
        })?;
        String::from_utf8(bytes).map_err(|_| format!("{file} isn't text"))?
    };
    crate::shaderfx::check(&code)?;
    let n = |k: &str| f(s, k, 0.5).clamp(-1000.0, 1000.0);
    Ok(Effect::Custom {
        code: code.into(),
        params: [n("param1"), n("param2"), n("param3"), n("param4")],
        elapsed_ms: 0,
    })
}

pub fn audio_effect_from(kind: &str, s: &Value) -> Option<AudioEffect> {
    let n = |k: &str, d: f64| f(s, k, d);
    Some(match kind {
        "gain_filter" => AudioEffect::Gain { db: n("db", 0.0) },
        "invert_polarity_filter" => AudioEffect::InvertPolarity,
        "noise_gate_filter" => AudioEffect::NoiseGate {
            open_db: n("open_threshold", -26.0),
            close_db: n("close_threshold", -32.0),
            attack_ms: n("attack_time", 25.0),
            hold_ms: n("hold_time", 200.0),
            release_ms: n("release_time", 150.0),
        },
        "compressor_filter" => AudioEffect::Compressor {
            ratio: n("ratio", 10.0),
            threshold_db: n("threshold", -18.0),
            attack_ms: n("attack_time", 6.0),
            release_ms: n("release_time", 60.0),
            output_db: n("output_gain", 0.0),
        },
        "limiter_filter" => {
            AudioEffect::Limiter { threshold_db: n("threshold", -6.0), release_ms: n("release_time", 60.0) }
        }
        "expander_filter" => {
            let gate = s.get("presets").and_then(Value::as_str) == Some("gate");
            AudioEffect::Expander {
                ratio: n("ratio", if gate { 10.0 } else { 2.0 }),
                threshold_db: n("threshold", -40.0),
                attack_ms: n("attack_time", 10.0),
                release_ms: n("release_time", if gate { 125.0 } else { 50.0 }),
                output_db: n("output_gain", 0.0),
                rms: s.get("detector").and_then(Value::as_str) != Some("peak"),
            }
        }
        "upward_compressor_filter" => AudioEffect::UpwardCompressor {
            ratio: n("ratio", 0.5),
            threshold_db: n("threshold", -20.0),
            attack_ms: n("attack_time", 10.0),
            release_ms: n("release_time", 50.0),
            output_db: n("output_gain", 0.0),
            knee_db: n("knee_width", 10.0),
        },
        "basic_eq_filter" => AudioEffect::Eq3 { low_db: n("low", 0.0), mid_db: n("mid", 0.0), high_db: n("high", 0.0) },
        // Strong noise removal where the app has it; the light one elsewhere.
        "ferrocast_deepfilter" => AudioEffect::Denoise {
            strength: s.get("strength").and_then(Value::as_u64).unwrap_or(100).clamp(1, 100) as u32,
        },
        // OBS's Noise Suppression (RNNoise or Speex) becomes it too: as deep as OBS's level
        // (-30 dB by default), 0.6 dB a step.
        "noise_suppress_filter" | "noise_suppress_filter_v2" => {
            let level = s.get("suppress_level").and_then(Value::as_f64).unwrap_or(-30.0).abs();
            AudioEffect::Denoise { strength: (level / 0.6).round().clamp(1.0, 100.0) as u32 }
        }
        _ => return None,
    })
}

/// A source kind the engine plays, from OBS's id and settings; Err with what it is otherwise.
/// macOS's notification banners (and Notification Center, and the desktop's widgets), by bundle id.
pub const NOTIFICATIONS: &str = "com.apple.notificationcenterui";

/// What a display capture leaves out, from the document's `ferrocast.privacy`: notifications
/// (unless `hide_notifications` is false) and the apps in `hide_apps`.
pub fn privacy_hide(doc: &Value) -> Vec<String> {
    let p = &doc["ferrocast"]["privacy"];
    let mut hide: Vec<String> = Vec::new();
    if p["hide_notifications"].as_bool() != Some(false) {
        hide.push(NOTIFICATIONS.into());
    }
    hide.extend(
        p["hide_apps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|a| !a.is_empty())
            .map(String::from),
    );
    hide
}

/// The files in an OBS file list (`[{ value, hidden }]`), a folder's files with one of `exts`
/// in its place (sorted by name), hidden ones left out.
fn files_of(list: Option<&Value>, exts: &[&str]) -> Vec<std::path::PathBuf> {
    let has_ext = |p: &std::path::Path| {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
    };
    let mut out = Vec::new();
    for e in list.and_then(Value::as_array).into_iter().flatten() {
        if e.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(v) = e.get("value").and_then(Value::as_str).filter(|v| !v.is_empty()) else { continue };
        let p = std::path::PathBuf::from(v);
        if p.is_dir() {
            let mut inside: Vec<_> = std::fs::read_dir(&p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|f| f.path())
                .filter(|f| f.is_file() && has_ext(f))
                .collect();
            inside.sort();
            out.extend(inside);
        } else {
            out.push(p);
        }
    }
    out
}

const PICTURES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const MEDIA: &[&str] =
    &["mp4", "m4v", "mov", "mkv", "webm", "flv", "ts", "avi", "mp3", "m4a", "aac", "wav", "ogg", "opus", "flac"];

fn kind_from(id: &str, s: &Value, canvas: (u32, u32), hide: &[String]) -> Result<(SourceKind, bool), String> {
    let text_of = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(match id {
        "color_source" | "color_source_v2" | "color_source_v3" => (
            SourceKind::Color {
                // 0 (or none) is the canvas's size, as OBS's defaults have it.
                width: s.get("width").and_then(Value::as_u64).filter(|v| *v > 0).map_or(canvas.0, |v| v as u32),
                height: s.get("height").and_then(Value::as_u64).filter(|v| *v > 0).map_or(canvas.1, |v| v as u32),
                rgba: color(s.get("color").and_then(Value::as_u64).unwrap_or(0xFFFF_FFFF)),
            },
            false,
        ),
        "image_source" => (SourceKind::Image(text_of("file").into()), false),
        // OBS's Image Slide Show: its files (a folder's pictures), slide time and fade.
        "slideshow" | "slideshow_v2" | "image_slide_show" => {
            let files = files_of(s.get("files"), PICTURES);
            if files.is_empty() {
                return Err("a slideshow with no pictures".into());
            }
            let n = |k: &str, d: u64| s.get(k).and_then(Value::as_u64).unwrap_or(d);
            let cut = s.get("transition").and_then(Value::as_str) == Some("cut");
            let slides = crate::source::series::Slides {
                files,
                every_ms: n("slide_time", 8000).clamp(50, 3_600_000),
                fade_ms: if cut { 0 } else { n("transition_speed", 700).min(10_000) },
                looping: s.get("loop").and_then(Value::as_bool).unwrap_or(true),
                random: s.get("randomize").and_then(Value::as_bool).unwrap_or(false),
            };
            (SourceKind::Slides(slides), false)
        }
        // OBS's VLC Video Source: its playlist, one file after another.
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        "vlc_source" => {
            let files = files_of(s.get("playlist"), MEDIA);
            if files.is_empty() {
                return Err("a playlist with no files".into());
            }
            let p = crate::source::series::Playlist {
                files,
                looping: s.get("loop").and_then(Value::as_bool).unwrap_or(true),
                random: s.get("shuffle").and_then(Value::as_bool).unwrap_or(false),
            };
            (SourceKind::Playlist(p), true)
        }
        "text_ft2_source" | "text_ft2_source_v2" | "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" => {
            let font = s.get("font").cloned().unwrap_or(Value::Null);
            let flags = font.get("flags").and_then(Value::as_u64).unwrap_or(0);
            let gdi = id.starts_with("text_gdiplus");
            let c1 = color(s.get(if gdi { "color" } else { "color1" }).and_then(Value::as_u64).unwrap_or(0xFFFF_FFFF));
            let c2 = (!gdi).then(|| color(s.get("color2").and_then(Value::as_u64).unwrap_or(0xFFFF_FFFF)));
            let style = TextStyle {
                font: font.get("face").and_then(Value::as_str).unwrap_or_default().to_string(),
                size: font.get("size").and_then(Value::as_f64).unwrap_or(48.0) as f32,
                bold: flags & 1 != 0,
                italic: flags & 2 != 0,
                color: c1,
                gradient: c2.filter(|c| *c != c1),
                outline: s.get("outline").and_then(Value::as_bool).unwrap_or(false).then_some((2.0, [0, 0, 0, 255])),
                shadow: s
                    .get("drop_shadow")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then_some(((4, 4), [0, 0, 0, 255])),
                wrap_width: s
                    .get("word_wrap")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(|| s.get("custom_width").and_then(Value::as_u64).unwrap_or(0) as u32)
                    .filter(|w| *w > 0),
                align: match s.get("align").and_then(Value::as_str) {
                    Some("center") => TextAlign::Center,
                    Some("right") => TextAlign::Right,
                    _ => TextAlign::Left,
                },
                ..TextStyle::default()
            };
            (SourceKind::Text { text: text_of("text"), style }, false)
        }
        "ffmpeg_source" => {
            // A network address (a guest's camera, an RTMP or HLS stream): the app reads it
            // with FFmpeg and hands its pictures in, fitted into 1280x720.
            if s.get("is_local_file").and_then(Value::as_bool) == Some(false) {
                return Ok((SourceKind::Fed { width: 1280, height: 720 }, true));
            }
            (
                SourceKind::Media {
                    path: text_of("local_file").into(),
                    looping: s.get("looping").and_then(Value::as_bool).unwrap_or(false),
                },
                true,
            )
        }
        #[cfg(target_os = "macos")]
        "screen_capture" | "display_capture" => {
            let n = |k: &str| s.get(k).and_then(Value::as_u64).map(|v| v as u32);
            match (s.get("type").and_then(Value::as_u64).unwrap_or(0), n("window")) {
                // Window Capture, by its window number (as OBS keeps it).
                (1, Some(w)) => (SourceKind::Window(w), false),
                (1, None) => return Err("a window capture with no window picked".into()),
                (0, _) => {
                    // The display picked (by number), else the first (OBS keeps a display's UUID).
                    let first = || crate::screen::shareable().ok().and_then(|(d, _)| d.first().map(|d| d.id));
                    let display = n("display").or_else(first).unwrap_or(0);
                    (SourceKind::Screen { display, sound: false, hide: hide.to_vec() }, false)
                }
                // Application Capture: one app's windows (its bundle id) on a display.
                (2, _) => {
                    let first = || crate::screen::shareable().ok().and_then(|(d, _)| d.first().map(|d| d.id));
                    let display = n("display").or_else(first).unwrap_or(0);
                    let app = text_of("application");
                    if app.is_empty() {
                        return Err("an Application Capture with no app picked".into());
                    }
                    (SourceKind::App { display, app, sound: false }, false)
                }
                _ => return Err("an unknown screen capture method".into()),
            }
        }
        #[cfg(target_os = "macos")]
        "av_capture_input" | "av_capture_input_v2" | "macos-avcapture" | "macos-avcapture-fast" => {
            (SourceKind::Camera(text_of("device")), false)
        }
        // What the Mac plays: all of it (an app's own sound isn't separated yet).
        #[cfg(target_os = "macos")]
        "sck_audio_capture" | "coreaudio_output_capture" => {
            match s.get("type").and_then(Value::as_i64).unwrap_or(0) {
                0 => (SourceKind::DesktopSound, true),
                // Application Audio Capture: one app's sound (its bundle id).
                1 => {
                    let app = text_of("application");
                    if app.is_empty() {
                        return Err("an Application Audio Capture with no app picked".into());
                    }
                    (SourceKind::AppSound(app), true)
                }
                _ => return Err("an unknown audio capture method".into()),
            }
        }
        #[cfg(target_os = "macos")]
        "coreaudio_input_capture" => {
            let dev = text_of("device_id");
            let dev = if dev.is_empty() || dev == "default" {
                crate::camera::microphones().first().map(|d| d.id.clone()).unwrap_or_default()
            } else {
                dev
            };
            (SourceKind::Microphone(dev), true)
        }
        // Windows: OBS's own source kinds.
        #[cfg(target_os = "windows")]
        "monitor_capture" => {
            // OBS 30 keeps the monitor's device path ("monitor_id"), older ones its place
            // ("monitor"): the place, else the first.
            let display = s.get("monitor").and_then(Value::as_u64).unwrap_or(0) as u32;
            (SourceKind::Screen { display, sound: false, hide: hide.to_vec() }, false)
        }
        #[cfg(target_os = "windows")]
        "window_capture" | "game_capture" => {
            // Game Capture of one window is a window here; "any full-screen game" isn't yet.
            if id == "game_capture"
                && s.get("capture_mode").and_then(Value::as_str).unwrap_or("any_fullscreen") != "window"
            {
                return Err("Game Capture of any full-screen game (use Display Capture)".into());
            }
            let w = match s.get("window") {
                Some(Value::Number(n)) => n.as_u64().map(|v| v as u32),
                // OBS's "title:class:exe" (its colons written #3A): the open window of that title.
                Some(Value::String(t)) => {
                    let title = t.split(':').next().unwrap_or("").replace("#3A", ":");
                    crate::screen::shareable()
                        .ok()
                        .and_then(|(_, ws)| ws.into_iter().find(|w| w.title == title).map(|w| w.id))
                }
                _ => None,
            };
            match w {
                Some(w) => (SourceKind::Window(w), false),
                None => return Err("a window capture whose window isn't open".into()),
            }
        }
        #[cfg(target_os = "windows")]
        "dshow_input" => (SourceKind::Camera(text_of("video_device_id")), false),
        #[cfg(target_os = "windows")]
        "wasapi_output_capture" | "wasapi_process_output_capture" => {
            if id == "wasapi_process_output_capture" {
                return Err("one app's sound (Application Audio Capture)".into());
            }
            (SourceKind::DesktopSound, true)
        }
        #[cfg(target_os = "windows")]
        "wasapi_input_capture" => {
            // OBS keeps Windows' endpoint id ("{0.0.1…}"): the system's microphone stands in.
            let dev = text_of("device_id");
            let dev = if dev.is_empty() || dev.starts_with('{') { "default".to_string() } else { dev };
            (SourceKind::Microphone(dev), true)
        }
        // Linux: OBS's own source kinds (X11 screen, Video4Linux, PulseAudio).
        #[cfg(target_os = "linux")]
        "xshm_input" | "xshm_input_v2" => {
            let display = s.get("screen").and_then(Value::as_u64).unwrap_or(0) as u32;
            (SourceKind::Screen { display, sound: false, hide: hide.to_vec() }, false)
        }
        #[cfg(target_os = "linux")]
        "v4l2_input" => (SourceKind::Camera(text_of("device_id")), false),
        #[cfg(target_os = "linux")]
        "pulse_output_capture" => (SourceKind::DesktopSound, true),
        #[cfg(target_os = "linux")]
        "pulse_input_capture" => {
            let dev = text_of("device_id");
            (SourceKind::Microphone(if dev.is_empty() { "default".into() } else { dev }), true)
        }
        // An NDI source (DistroAV's kind): the app reads it from the network and hands it in.
        "ndi_source" => (SourceKind::Fed { width: 1920, height: 1080 }, true),
        // A web page (OBS's Browser source, or Ferrocast's own): the app renders it and hands
        // its pictures in. OBS's own size keys; 800x600 when it says none.
        "browser_source" | "ferrocast_web" => {
            let n = |k: &str, d: u32| s.get(k).and_then(Value::as_u64).filter(|v| *v > 0).map_or(d, |v| v as u32);
            (SourceKind::Fed { width: n("width", 800), height: n("height", 600) }, true)
        }
        other => return Err(other.to_string()),
    })
}

pub fn placement_of(it: &Value) -> Placement {
    let xy = |k: &str, d: f32| {
        let v = it.get(k);
        (v.map_or(d, |v| f(v, "x", f64::from(d))), v.map_or(d, |v| f(v, "y", f64::from(d))))
    };
    let (x, y) = xy("pos", 0.0);
    let (scale_x, scale_y) = xy("scale", 1.0);
    let (bw, bh) = xy("bounds", 0.0);
    let crop = |k: &str| it.get(k).and_then(Value::as_i64).unwrap_or(0).max(0) as u32;
    Placement {
        x,
        y,
        rotation: f(it, "rot", 0.0),
        scale_x,
        scale_y,
        alignment: it.get("align").and_then(Value::as_u64).unwrap_or(5) as u32,
        bounds: match it.get("bounds_type").and_then(Value::as_u64).unwrap_or(0) {
            1 => Bounds::Stretch,
            2 => Bounds::Fit,
            3 => Bounds::Fill,
            4 => Bounds::Width,
            5 => Bounds::Height,
            6 => Bounds::Max,
            _ => Bounds::None,
        },
        bounds_alignment: it.get("bounds_align").and_then(Value::as_u64).unwrap_or(0) as u32,
        bounds_width: bw,
        bounds_height: bh,
        crop: [crop("crop_left"), crop("crop_top"), crop("crop_right"), crop("crop_bottom")],
    }
}

/// Reads a collection. `canvas` sizes colour sources that don't say.
pub fn import(doc: &Value, canvas: (u32, u32)) -> Imported {
    let mut out = Imported {
        current: doc.get("current_program_scene").and_then(Value::as_str).map(String::from),
        ..Default::default()
    };
    let sources = doc.get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut ids: Vec<(String, u64, Vec<crate::Effect>, bool)> = Vec::new();
    let mut next_id = 1u64;
    let is_scene = |s: &Value| matches!(s.get("id").and_then(Value::as_str), Some("scene" | "group"));
    let scene_names: Vec<String> = sources
        .iter()
        .filter(|s| is_scene(s))
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    // Groups and their boxes.
    let groups: Vec<(String, (u32, u32))> = sources
        .iter()
        .filter(|s| s.get("id").and_then(Value::as_str) == Some("group"))
        .filter_map(|s| {
            let set = s.get("settings")?;
            let size = |k: &str| set.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            Some((s.get("name")?.as_str()?.to_string(), (size("cx"), size("cy"))))
        })
        .collect();
    let channels: Vec<Value> = CHANNELS.iter().filter_map(|k| doc.get(*k).cloned()).collect();
    let hide = privacy_hide(doc);
    for (s, global) in sources.iter().map(|s| (s, false)).chain(channels.iter().map(|s| (s, true))) {
        let id = s.get("versioned_id").or_else(|| s.get("id")).and_then(Value::as_str).unwrap_or_default();
        let name = s.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        if id == "scene" || id == "group" {
            continue;
        }
        let settings = s.get("settings").cloned().unwrap_or(Value::Null);
        let (mut effects, mut sound_effects) = (Vec::new(), Vec::new());
        for flt in s.get("filters").and_then(Value::as_array).into_iter().flatten() {
            if flt.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let kind = flt.get("versioned_id").or_else(|| flt.get("id")).and_then(Value::as_str).unwrap_or_default();
            let fs = flt.get("settings").cloned().unwrap_or(Value::Null);
            if kind == "mask_filter" || kind == "mask_filter_v2" {
                // Its picture becomes a source of its own, shown in no scene.
                match mask_from(kind, &fs, next_id) {
                    Some((e, path)) => {
                        let fname = flt.get("name").and_then(Value::as_str).unwrap_or(kind);
                        out.sources.push(ImportedSource {
                            id: next_id,
                            name: format!("{name}: {fname}"),
                            kind: SourceKind::Image(path),
                            sound: None,
                            sound_effects: Vec::new(),
                            global: false,
                        });
                        next_id += 1;
                        effects.push(e);
                    }
                    None => out.skipped.push(format!("{name}: the filter {kind} (no picture chosen)")),
                }
            } else if kind == "ferrocast_remove_background" && !cfg!(any(target_os = "macos", target_os = "windows")) {
                // Nothing finds the person here yet: left out, not a picture cut away.
                out.skipped.push(format!("{name}: the filter {kind} (on a Mac and Windows only so far)"));
            } else if kind == "ferrocast_remove_background" {
                // The person mask comes in as a picture of its own (the app finds the person
                // and hands it in), shown in no scene.
                let fname = flt.get("name").and_then(Value::as_str).unwrap_or(kind);
                out.sources.push(ImportedSource {
                    id: next_id,
                    name: format!("{name}: {fname}"),
                    kind: SourceKind::Fed { width: 320, height: 180 },
                    sound: None,
                    sound_effects: Vec::new(),
                    global: false,
                });
                let int =
                    |k: &str, d: i64, lo: i64, hi: i64| fs.get(k).and_then(Value::as_i64).unwrap_or(d).clamp(lo, hi);
                effects.push(Effect::Cutout {
                    mask: next_id,
                    blur: int("mode", 0, 0, 1) == 1,
                    blur_px: int("blur", 24, 2, 100) as f32,
                    soft: int("soft", 10, 0, 40) as f32 / 100.0,
                });
                next_id += 1;
            } else if kind == "ferrocast_shader" {
                let fname = flt.get("name").and_then(Value::as_str).unwrap_or(kind);
                match shader_from(&fs) {
                    Ok(e) => effects.push(e),
                    Err(why) => out.skipped.push(format!("{name}: the shader effect \"{fname}\" ({why})")),
                }
            } else if let Some(e) = effect_from(kind, &fs) {
                effects.push(e);
            } else if let Some(e) = audio_effect_from(kind, &fs) {
                sound_effects.push(e);
            } else {
                out.skipped.push(format!("{name}: the filter {kind}"));
            }
        }
        match kind_from(id, &settings, canvas, &hide) {
            Ok((kind, has_sound)) => {
                let flags = s.get("flags").and_then(Value::as_u64).unwrap_or(0);
                let sound = has_sound.then(|| SourceSettings {
                    volume: f(s, "volume", 1.0),
                    muted: s.get("muted").and_then(Value::as_bool).unwrap_or(false),
                    balance: f(s, "balance", 0.5),
                    mono: flags & 2 != 0,
                    sync_offset_ns: s.get("sync").and_then(Value::as_i64).unwrap_or(0),
                    tracks: s.get("mixers").and_then(Value::as_u64).map_or(1, |m| m as u8),
                    monitoring: match s.get("monitoring_type").and_then(Value::as_u64) {
                        Some(1) => Monitoring::Only,
                        Some(2) => Monitoring::AndOutput,
                        _ => Monitoring::Off,
                    },
                });
                out.sources.push(ImportedSource {
                    id: next_id,
                    name: name.clone(),
                    kind,
                    sound,
                    sound_effects,
                    global,
                });
                ids.push((name, next_id, effects, true));
                next_id += 1;
            }
            Err(what) => out.skipped.push(format!("{name}: {what}")),
        }
    }
    // Scenes and groups: their items, bottom first, with each source's filters; nested scenes
    // and groups by name.
    for s in sources.iter().filter(|s| is_scene(s)) {
        let name = s.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut items = Vec::new();
        for it in s.get("settings").and_then(|v| v.get("items")).and_then(Value::as_array).into_iter().flatten() {
            // Copies of a group's items OBS keeps in the scene for versions before groups.
            if it.get("group_item_backup").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let src = it.get("name").and_then(Value::as_str).unwrap_or_default();
            let mut si = if let Some((_, id, effects, _)) = ids.iter().find(|(n, ..)| n == src) {
                let mut si = SceneItem::new(*id, placement_of(it));
                si.effects = effects.clone();
                si
            } else if scene_names.iter().any(|n| n == src) {
                let mut si = SceneItem::new(0, placement_of(it));
                si.scene = Some(src.to_string());
                si.group = groups.iter().find(|(g, _)| g == src).map(|(_, size)| *size);
                si
            } else {
                continue;
            };
            si.visible = it.get("visible").and_then(Value::as_bool).unwrap_or(true);
            si.filter = Filter::from_name(it.get("scale_filter").and_then(Value::as_str).unwrap_or("disable"));
            si.blend = Blend::from_name(it.get("blend_type").and_then(Value::as_str).unwrap_or("normal"));
            items.push(si);
        }
        if groups.iter().any(|(g, _)| *g == name) {
            out.groups.push(name.clone());
        }
        out.scenes.push((name, items));
    }
    // OBS's scene list order where it's saved.
    if let Some(order) = doc.get("scene_order").and_then(Value::as_array) {
        let names: Vec<&str> = order.iter().filter_map(|o| o.get("name").and_then(Value::as_str)).collect();
        out.scenes.sort_by_key(|(n, _)| names.iter().position(|o| o == n).unwrap_or(usize::MAX));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shader_file_is_read_safely() {
        let fx = |set: Value| shader_from(&set);
        // A device (Mac and Linux have /dev/zero) is never read.
        #[cfg(unix)]
        assert!(
            fx(json!({ "shader_file": "/dev/zero" })).unwrap_err().contains("isn't a file"),
            "a device is never read"
        );
        assert!(fx(json!({ "shader_file": "/nowhere.wgsl" })).unwrap_err().contains("can't be read"));
        let big = std::env::temp_dir().join(format!("ferrocore-big-{}.wgsl", std::process::id()));
        let _ = std::fs::write(&big, "x".repeat(crate::shaderfx::MAX_LEN + 10));
        let e = fx(json!({ "shader_file": big })).unwrap_err();
        let _ = std::fs::remove_file(&big);
        assert!(e.contains("longer"), "{e}");
        let ok =
            fx(json!({ "shader_text": "fn effect(uv: vec2<f32>) -> vec4<f32> { return pic(uv); }", "param2": 0.25 }));
        assert!(matches!(ok, Ok(Effect::Custom { params: [0.5, 0.25, 0.5, 0.5], .. })), "{ok:?}");
    }

    #[test]
    fn a_slideshow_reads_obs_settings() {
        let dir = std::env::temp_dir().join(format!("ferrocore-obs-slides-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for f in ["b.png", "a.jpg", "notes.txt"] {
            let _ = std::fs::write(dir.join(f), b"x");
        }
        let set = json!({
            "files": [
                { "value": dir.join("..").join("one.png"), "hidden": false },
                { "value": dir, "hidden": false },
                { "value": "/gone.png", "hidden": true },
            ],
            "slide_time": 3000, "transition": "cut", "randomize": true,
        });
        let got = kind_from("slideshow_v2", &set, (1920, 1080), &[]);
        let _ = std::fs::remove_dir_all(&dir);
        let Ok((SourceKind::Slides(s), false)) = got else { panic!("{got:?}") };
        let names: Vec<String> = s
            .files
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        assert_eq!(names, ["one.png", "a.jpg", "b.png"], "a folder's pictures by name, hidden ones out");
        assert_eq!((s.every_ms, s.fade_ms, s.looping, s.random), (3000, 0, true, true));
        assert!(kind_from("slideshow", &json!({ "files": [] }), (1920, 1080), &[]).is_err(), "no pictures");
    }
    use serde_json::json;

    #[test]
    fn a_collection_comes_over() {
        let doc = json!({
            "current_program_scene": "Two",
            "scene_order": [{ "name": "One" }, { "name": "Two" }],
            "sources": [
                { "id": "color_source", "versioned_id": "color_source_v3", "name": "Back",
                  "settings": { "color": 0xFF00_80FFu32 },
                  "filters": [{ "id": "sharpness_filter", "versioned_id": "sharpness_filter_v2", "settings": {} },
                              { "id": "gain_filter", "settings": { "db": 3.0 } },
                              { "id": "gpu_delay", "settings": {} }] },
                { "id": "browser_source", "name": "Web", "settings": {} },
                { "id": "scene", "name": "Two", "settings": { "items": [
                    { "name": "One", "pos": { "x": 10.0, "y": 20.0 }, "scale": { "x": 0.5, "y": 0.5 }, "visible": true } ] } },
                { "id": "scene", "name": "One", "settings": { "items": [
                    { "name": "Back", "rot": 30.0, "bounds_type": 2, "bounds": { "x": 100.0, "y": 50.0 },
                      "crop_left": 4, "blend_type": "screen", "scale_filter": "lanczos", "visible": false },
                    { "name": "Web" } ] } }
            ]
        });
        let got = import(&doc, (640, 360));
        assert_eq!(got.current.as_deref(), Some("Two"));
        assert_eq!(got.sources.len(), 2, "the web page comes over, for the app to render");
        assert_eq!(got.sources[0].kind, SourceKind::Color { width: 640, height: 360, rgba: [0xFF, 0x80, 0, 0xFF] });
        assert_eq!(got.sources[1].kind, SourceKind::Fed { width: 800, height: 600 });
        assert!(got.skipped.iter().any(|s| s.contains("gpu_delay")));
        let names: Vec<&str> = got.scenes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["One", "Two"]);
        let back = &got.scenes[0].1[0];
        assert!(!back.visible);
        assert_eq!(back.blend, Blend::Screen);
        assert_eq!(back.filter, Filter::Lanczos);
        assert_eq!(back.effects.len(), 1);
        assert_eq!((back.placement.rotation, back.placement.bounds, back.placement.crop[0]), (30.0, Bounds::Fit, 4));
        assert_eq!(got.scenes[0].1.len(), 2, "the web page's item stays (its pictures come from the app)");
        let nested = &got.scenes[1].1[0];
        assert_eq!(nested.scene.as_deref(), Some("One"));
        assert_eq!((nested.placement.x, nested.placement.scale_x), (10.0, 0.5));
        assert_eq!(nested.group, None);
    }

    #[test]
    fn a_mask_brings_its_picture() {
        let doc = json!({ "sources": [{ "id": "color_source", "name": "Card", "settings": {},
            "filters": [{ "id": "mask_filter", "versioned_id": "mask_filter_v2", "name": "Round",
                "settings": { "type": "blend_mul_filter.effect", "image_path": "/m.png", "color": 0x00FF_FFFFu32, "opacity": 0.5 } },
                { "id": "mask_filter", "versioned_id": "mask_filter_v2", "name": "Empty", "settings": {} }] }] });
        let got = import(&doc, (64, 64));
        assert_eq!(got.sources.len(), 2, "the card and its mask's picture");
        let pic = got.sources.iter().find(|s| s.name == "Card: Round").map(|s| (s.id, s.kind.clone()));
        let Some((id, SourceKind::Image(path))) = pic else { panic!("no picture: {:?}", got.sources) };
        assert_eq!(path, std::path::PathBuf::from("/m.png"));
        assert!(got.skipped.iter().any(|s| s.contains("no picture")));
        let card = got.sources.iter().find(|s| s.name == "Card").map(|s| s.id);
        assert_ne!(card, Some(id));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn global_sound_comes_over() {
        let doc = json!({
            "DesktopAudioDevice1": { "id": "sck_audio_capture", "name": "Desktop Audio", "settings": { "type": 0 },
                "volume": 0.5, "monitoring_type": 0 },
            "AuxAudioDevice1": { "id": "coreaudio_input_capture", "name": "Mic/Aux", "settings": { "device_id": "x" } },
            "DesktopAudioDevice2": { "id": "sck_audio_capture", "name": "One app", "settings": { "type": 1 } },
            "sources": []
        });
        let got = import(&doc, (64, 64));
        let desk = got.sources.iter().find(|s| s.name == "Desktop Audio");
        assert!(desk.is_some_and(|d| d.global && d.kind == SourceKind::DesktopSound), "{:?}", got.sources);
        assert_eq!(desk.and_then(|d| d.sound.as_ref()).map(|s| s.volume), Some(0.5));
        assert!(got.sources.iter().any(|s| s.name == "Mic/Aux" && s.global));
        assert!(got.skipped.iter().any(|s| s.starts_with("One app")));
    }

    #[test]
    fn groups_come_over_once() {
        let doc = json!({ "sources": [
            { "id": "color_source", "name": "Dot", "settings": { "width": 8, "height": 8 } },
            { "id": "group", "name": "Pair", "settings": { "cx": 120, "cy": 80, "items": [{ "name": "Dot" }] } },
            { "id": "scene", "name": "Main", "settings": { "items": [
                { "name": "Pair", "rot": 350.0 },
                { "name": "Dot", "group_item_backup": true } ] } }
        ]});
        let got = import(&doc, (640, 360));
        assert_eq!(got.groups, ["Pair"]);
        let main = &got.scenes.iter().find(|(n, _)| n == "Main").map(|(_, i)| i.clone()).unwrap_or_default();
        assert_eq!(main.len(), 1, "the backup copy is left out");
        assert_eq!((main[0].scene.as_deref(), main[0].group), (Some("Pair"), Some((120, 80))));
    }
}
