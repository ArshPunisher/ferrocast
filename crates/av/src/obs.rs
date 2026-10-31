//! Opening an OBS scene collection in our engine (ENGINE.md E7): the JSON OBS (and Ferrocast's
//! Studio) saves becomes the engine's sources and scenes. Every source kind, filter and setting
//! the engine has is carried over; anything else is listed by name in `skipped`, so the app can
//! say what still needs OBS's engine.

use serde_json::Value;

use crate::afx::AudioEffect;
use crate::engine::{SceneItem, SourceKind};
use crate::mix::{Monitoring, SourceSettings};
use crate::source::text::{TextAlign, TextStyle};
use crate::{Blend, Bounds, Effect, Filter, Placement};

/// A source as the engine takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSource {
    pub id: u64,
    pub name: String,
    pub kind: SourceKind,
    /// Its sound settings, for kinds with sound.
    pub sound: Option<SourceSettings>,
    pub sound_effects: Vec<AudioEffect>,
}

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

/// A source's sound filter, from OBS's id and settings.
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
        _ => return None,
    })
}

/// A source kind the engine plays, from OBS's id and settings; Err with what it is otherwise.
fn kind_from(id: &str, s: &Value, canvas: (u32, u32)) -> Result<(SourceKind, bool), String> {
    let text_of = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(match id {
        "color_source" | "color_source_v2" | "color_source_v3" => (
            SourceKind::Color {
                width: s.get("width").and_then(Value::as_u64).map_or(canvas.0, |v| v as u32),
                height: s.get("height").and_then(Value::as_u64).map_or(canvas.1, |v| v as u32),
                rgba: color(s.get("color").and_then(Value::as_u64).unwrap_or(0xFFFF_FFFF)),
            },
            false,
        ),
        "image_source" => (SourceKind::Image(text_of("file").into()), false),
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
        #[cfg(target_os = "macos")]
        "ffmpeg_source" => {
            if s.get("is_local_file").and_then(Value::as_bool) == Some(false) {
                return Err("Media from a network address".into());
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
            // The first display (OBS keeps a display's UUID; ours are listed by number).
            let display = crate::screen::shareable().ok().and_then(|(d, _)| d.first().map(|d| d.id)).unwrap_or(0);
            (SourceKind::Screen { display, sound: false }, false)
        }
        #[cfg(target_os = "macos")]
        "av_capture_input" | "av_capture_input_v2" | "macos-avcapture" | "macos-avcapture-fast" => {
            (SourceKind::Camera(text_of("device")), false)
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
        other => return Err(other.to_string()),
    })
}

fn placement_of(it: &Value) -> Placement {
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
    for s in &sources {
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
            if let Some(e) = effect_from(kind, &fs) {
                effects.push(e);
            } else if let Some(e) = audio_effect_from(kind, &fs) {
                sound_effects.push(e);
            } else {
                out.skipped.push(format!("{name}: the filter {kind}"));
            }
        }
        match kind_from(id, &settings, canvas) {
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
                out.sources.push(ImportedSource { id: next_id, name: name.clone(), kind, sound, sound_effects });
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
                              { "id": "scroll_filter", "settings": {} }] },
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
        assert_eq!(got.sources.len(), 1);
        assert_eq!(got.sources[0].kind, SourceKind::Color { width: 640, height: 360, rgba: [0xFF, 0x80, 0, 0xFF] });
        assert!(got.skipped.iter().any(|s| s.contains("browser_source")));
        assert!(got.skipped.iter().any(|s| s.contains("scroll_filter")));
        let names: Vec<&str> = got.scenes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["One", "Two"]);
        let back = &got.scenes[0].1[0];
        assert!(!back.visible);
        assert_eq!(back.blend, Blend::Screen);
        assert_eq!(back.filter, Filter::Lanczos);
        assert_eq!(back.effects.len(), 1);
        assert_eq!((back.placement.rotation, back.placement.bounds, back.placement.crop[0]), (30.0, Bounds::Fit, 4));
        assert_eq!(got.scenes[0].1.len(), 1, "the browser source's item is left out");
        let nested = &got.scenes[1].1[0];
        assert_eq!(nested.scene.as_deref(), Some("One"));
        assert_eq!((nested.placement.x, nested.placement.scale_x), (10.0, 0.5));
        assert_eq!(nested.group, None);
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
