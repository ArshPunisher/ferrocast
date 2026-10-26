//! Bringing in a scene collection from Streamlabs Desktop (`slobs-client/SceneCollections`).
//! Its format is its own, so it is turned into OBS's first, as OBS's own importer does
//! (`frontend/importers/sl.cpp`): sources, scenes with their items, filters, transitions.
//! Then the OBS import (`import::convert`) takes over, so Streamlabs' widgets (Browser
//! sources) become Ferrocast's Web page sources too. Windows source kinds are changed into
//! this computer's (a Streamlabs collection usually comes from a Windows PC); those whose
//! settings can't carry over (a device, a window) come in empty, to be picked again.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::StudioError;
use crate::import::{ObsCollection, valid_id};

/// Where Streamlabs Desktop keeps its scene collections for this user, if it has been run.
pub fn scenes_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        return None;
    };
    let dir = base.join("slobs-client/SceneCollections");
    dir.is_dir().then_some(dir)
}

/// A Streamlabs collection file: its root says so.
fn is_streamlabs(doc: &Value) -> bool {
    doc.get("nodeType").and_then(Value::as_str) == Some("RootNode")
}

/// Collection names, from `manifest.json` (`collections: [{ id, name }]`).
fn names(dir: &Path) -> Map<String, Value> {
    let Some(doc) =
        std::fs::read(dir.join("manifest.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Map::new();
    };
    doc.get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c.get("id")?.as_str()?.to_string(), c.get("name")?.clone())))
        .collect()
}

fn items(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).and_then(|x| x.get("items")).and_then(Value::as_array).cloned().unwrap_or_default()
}

/// The collections in `dir`, by name.
pub fn list(dir: &Path) -> Vec<ObsCollection> {
    let named = names(dir);
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<ObsCollection> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "json" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            if id == "manifest" || !valid_id(&id) {
                return None;
            }
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            if !is_streamlabs(&doc) {
                return None;
            }
            let name = named.get(&id).and_then(Value::as_str).unwrap_or("Streamlabs scenes").to_string();
            Some(ObsCollection { id, name, scenes: items(&doc, "scenes").len(), sources: items(&doc, "sources").len() })
        })
        .collect();
    out.sort_by_key(|c| c.name.to_lowercase());
    out
}

/// Reads collection `id` from `dir` and turns it into OBS's format.
pub fn read(dir: &Path, id: &str) -> Result<Value, StudioError> {
    if !valid_id(id) || id == "manifest" {
        return Err(StudioError::Failed("That isn't one of Streamlabs' scene collections.".into()));
    }
    let bytes = std::fs::read(dir.join(format!("{id}.json")))
        .map_err(|_| StudioError::Failed("That Streamlabs scene collection isn't there any more.".into()))?;
    let doc: Value = serde_json::from_slice(&bytes)
        .map_err(|e| StudioError::Failed(format!("The Streamlabs scene collection didn't read ({e}).")))?;
    if !is_streamlabs(&doc) {
        return Err(StudioError::Failed("That file isn't a Streamlabs scene collection.".into()));
    }
    let name = names(dir).get(id).and_then(Value::as_str).unwrap_or("Streamlabs scenes").to_string();
    Ok(to_obs(&doc, &name))
}

/// A name not taken yet in `taken`: "Name", else "Name(1)", "Name(2)"… (as OBS does).
fn free_name(taken: &[Value], name: &str) -> String {
    let used = |n: &str| taken.iter().any(|s| s.get("name").and_then(Value::as_str) == Some(n));
    let mut out = name.to_string();
    let mut n = 1;
    while used(&out) {
        out = format!("{name}({n})");
        n += 1;
    }
    out
}

/// Streamlabs' filters keep their kind in `type`; OBS's in `id`.
fn filters(v: &Value) -> Value {
    Value::Array(
        items(v, "filters")
            .into_iter()
            .map(|mut f| {
                if let Some(t) = f.get("type").cloned()
                    && let Some(o) = f.as_object_mut()
                {
                    o.insert("id".into(), t);
                }
                f
            })
            .collect(),
    )
}

/// Streamlabs' one Screen Capture (game, display or window, picked in `capture_source_list`)
/// as the OBS kind it stands for.
fn screen_capture(settings: &mut Map<String, Value>) -> &'static str {
    let list = settings.get("capture_source_list").and_then(Value::as_str).unwrap_or("").to_string();
    let kind = list.split(':').next().unwrap_or("");
    let out = match kind {
        "game" => "game_capture",
        "window" => "window_capture",
        _ => "monitor_capture",
    };
    for k in ["auto_capture_rules_path", "auto_placeholder_image", "auto_placeholder_message", "capture_source_list"] {
        settings.remove(k);
    }
    out
}

/// A source kind from another computer as this one's: `(kind, keep its settings)`. A device,
/// a window or a display is the other computer's, so it comes in empty.
pub fn for_this_computer(kind: &str) -> (&str, bool) {
    if cfg!(target_os = "macos") {
        match kind {
            "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" => ("text_ft2_source_v2", true),
            "game_capture" | "monitor_capture" | "window_capture" | "xshm_input" | "xcomposite_input" => {
                ("screen_capture", false)
            }
            "wasapi_input_capture" | "pulse_input_capture" | "alsa_input_capture" => ("coreaudio_input_capture", false),
            "wasapi_output_capture"
            | "wasapi_process_output_capture"
            | "pulse_output_capture"
            | "jack_output_capture" => ("sck_audio_capture", false),
            "dshow_input" | "v4l2_input" => ("macos-avcapture", false),
            k => (k, true),
        }
    } else if cfg!(windows) {
        match kind {
            "text_ft2_source" | "text_ft2_source_v2" => ("text_gdiplus", true),
            "coreaudio_input_capture" | "pulse_input_capture" | "alsa_input_capture" => ("wasapi_input_capture", false),
            "coreaudio_output_capture" | "sck_audio_capture" | "pulse_output_capture" | "jack_output_capture" => {
                ("wasapi_output_capture", false)
            }
            "av_capture_input" | "av_capture_input_v2" | "macos-avcapture" | "v4l2_input" => ("dshow_input", false),
            "screen_capture" | "xshm_input" => ("monitor_capture", false),
            k => (k, true),
        }
    } else {
        (kind, true)
    }
}

/// Streamlabs' collection as OBS's, for this computer.
pub fn to_obs(root: &Value, name: &str) -> Value {
    let mut sources: Vec<Value> = Vec::new();
    // Streamlabs' id → the name it has here (items point at sources by id).
    let mut by_id: Vec<(String, String)> = Vec::new();
    for s in items(root, "sources") {
        let mut settings = s.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut kind = s.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        if kind == "screen_capture" {
            kind = screen_capture(&mut settings).to_string();
        }
        let (here, keep) = for_this_computer(&kind);
        let out_name = free_name(&sources, s.get("name").and_then(Value::as_str).unwrap_or("Source"));
        let sync = s
            .get("syncOffset")
            .map(|o| {
                o.get("sec").and_then(Value::as_f64).unwrap_or(0.0) * 1e9
                    + o.get("nsec").and_then(Value::as_f64).unwrap_or(0.0)
            })
            .unwrap_or(0.0) as i64;
        if let Some(id) = s.get("id").and_then(Value::as_str) {
            by_id.push((id.to_string(), out_name.clone()));
        }
        sources.push(json!({
            "id": here,
            "versioned_id": here,
            "name": out_name,
            "settings": if keep { Value::Object(settings) } else { json!({}) },
            "filters": filters(&s),
            "sync": sync,
            "volume": s.get("volume").and_then(Value::as_f64).unwrap_or(1.0),
            "muted": s.get("muted").and_then(Value::as_bool).unwrap_or(false),
            "monitoring_type": s.get("monitoringType").and_then(Value::as_i64).unwrap_or(0),
        }));
    }
    let scenes = items(root, "scenes");
    // Scenes are sources too, and items may point at a scene (a nested scene).
    let mut scene_names = Vec::new();
    for sc in &scenes {
        let n = free_name(&sources, sc.get("name").and_then(Value::as_str).unwrap_or("Scene"));
        if let Some(id) = sc.get("id").and_then(Value::as_str) {
            by_id.push((id.to_string(), n.clone()));
        }
        // Held as a placeholder so the next scene's name sees it taken.
        sources.push(json!({ "name": n }));
        scene_names.push(n);
    }
    sources.truncate(sources.len() - scenes.len());
    let name_of = |id: &str| by_id.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone());
    for (sc, n) in scenes.iter().zip(&scene_names) {
        let list: Vec<Value> = items(sc, "sceneItems")
            .iter()
            .filter_map(|it| {
                let source = name_of(it.get("sourceId")?.as_str()?)?;
                let crop = it.get("crop").cloned().unwrap_or(json!({}));
                let num = |v: &Value, k: &str, d: f64| v.get(k).and_then(Value::as_f64).unwrap_or(d);
                Some(json!({
                    "name": source,
                    "pos": { "x": num(it, "x", 0.0), "y": num(it, "y", 0.0) },
                    "scale": { "x": num(it, "scaleX", 1.0), "y": num(it, "scaleY", 1.0) },
                    "rot": num(it, "rotation", 0.0),
                    "visible": it.get("visible").and_then(Value::as_bool).unwrap_or(true),
                    "crop_top": num(&crop, "top", 0.0) as i64,
                    "crop_bottom": num(&crop, "bottom", 0.0) as i64,
                    "crop_left": num(&crop, "left", 0.0) as i64,
                    "crop_right": num(&crop, "right", 0.0) as i64,
                }))
            })
            .enumerate()
            .map(|(i, mut v)| {
                v["id"] = json!(i + 1);
                v
            })
            .collect();
        let count = list.len();
        sources.push(json!({
            "id": "scene",
            "versioned_id": "scene",
            "name": n,
            "settings": { "items": list, "id_counter": count },
            "filters": filters(sc),
            "volume": 1.0,
            "private_settings": {},
        }));
    }
    let default_t = root.get("transitions").and_then(|t| t.get("defaultTransitionId")).and_then(Value::as_str);
    let transitions: Vec<Value> = root
        .get("transitions")
        .and_then(|t| t.get("transitions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| {
            json!({
                "id": t.get("type").cloned().unwrap_or(json!("fade_transition")),
                "name": t.get("name").cloned().unwrap_or(json!("Fade")),
                "settings": t.get("settings").cloned().unwrap_or(json!({})),
                "duration": t.get("duration").and_then(Value::as_i64).unwrap_or(300),
            })
        })
        .collect();
    let current_t = root
        .get("transitions")
        .and_then(|t| t.get("transitions"))
        .and_then(Value::as_array)
        .and_then(|l| l.iter().find(|t| t.get("id").and_then(Value::as_str) == default_t))
        .and_then(|t| t.get("name").cloned())
        .unwrap_or(json!(""));
    let first = scene_names.first().cloned().unwrap_or_default();
    json!({
        "name": name,
        "sources": sources,
        "transitions": transitions,
        "current_scene": first,
        "current_program_scene": first,
        "current_transition": current_t,
        "scene_order": scene_names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "nodeType": "RootNode",
            "sources": { "items": [
                { "id": "s1", "name": "Webcam", "type": "dshow_input", "settings": { "video_device_id": "Logi" },
                  "volume": 1, "muted": false, "syncOffset": { "sec": 0, "nsec": 200000000 },
                  "filters": { "items": [ { "name": "Key", "type": "chroma_key_filter_v2", "settings": {} } ] } },
                { "id": "s2", "name": "Alert Box", "type": "browser_source",
                  "settings": { "url": "https://streamlabs.com/alert-box/v3/x", "width": 800, "height": 600 } },
                { "id": "s3", "name": "Game", "type": "screen_capture",
                  "settings": { "capture_source_list": "game:1", "auto_placeholder_image": "x" } },
                { "id": "s4", "name": "Title", "type": "text_gdiplus", "settings": { "text": "hi" } },
                { "id": "s5", "name": "Live", "type": "image_source", "settings": { "file": "/a.png" } }
            ] },
            "scenes": { "items": [
                { "id": "c1", "name": "Live", "sceneItems": { "items": [
                    { "sourceId": "s3", "x": 0, "y": 0, "scaleX": 1, "scaleY": 1, "rotation": 0, "visible": true,
                      "crop": { "top": 0, "bottom": 0, "left": 10, "right": 0 } },
                    { "sourceId": "s1", "x": 1500, "y": 800, "scaleX": 0.5, "scaleY": 0.5, "visible": false },
                    { "sourceId": "c2" },
                    { "sourceId": "gone" }
                ] } },
                { "id": "c2", "name": "BRB", "sceneItems": { "items": [ { "sourceId": "s2" } ] } }
            ] },
            "transitions": { "defaultTransitionId": "t1", "transitions": [
                { "id": "t1", "name": "Swipe", "type": "swipe_transition", "duration": 500, "settings": {} }
            ] }
        })
    }

    #[test]
    fn a_streamlabs_collection_becomes_obs_s() {
        let doc = to_obs(&sample(), "Main");
        let src = doc["sources"].as_array().unwrap();
        let by = |n: &str| src.iter().find(|s| s["name"] == n).unwrap().clone();
        // Kinds and settings.
        assert_eq!(by("Alert Box")["id"], "browser_source");
        assert_eq!(by("Alert Box")["settings"]["url"], "https://streamlabs.com/alert-box/v3/x");
        assert_eq!(by("Webcam")["filters"][0]["id"], "chroma_key_filter_v2");
        assert_eq!(by("Webcam")["sync"], 200_000_000);
        // A scene and a source share a name ("Live"): the scene gets "Live(1)".
        assert_eq!(doc["current_scene"], "Live(1)");
        let live = by("Live(1)");
        let it = live["settings"]["items"].as_array().unwrap();
        // The missing source is left out; the nested scene is in by its new name.
        assert_eq!(it.len(), 3);
        assert_eq!(it[0]["name"], "Game");
        assert_eq!(it[0]["crop_left"], 10);
        assert_eq!(it[1]["visible"], false);
        assert_eq!(it[1]["scale"]["x"], 0.5);
        assert_eq!(it[2]["name"], "BRB");
        assert_eq!(doc["current_transition"], "Swipe");
        assert_eq!(doc["transitions"][0]["duration"], 500);
        assert_eq!(doc["scene_order"].as_array().unwrap().len(), 2);
        if cfg!(target_os = "macos") {
            // Windows devices and captures come in as this Mac's kinds, empty.
            assert_eq!(by("Webcam")["id"], "macos-avcapture");
            assert_eq!(by("Webcam")["settings"], json!({}));
            assert_eq!(by("Game")["id"], "screen_capture");
            assert_eq!(by("Title")["id"], "text_ft2_source_v2");
            assert_eq!(by("Title")["settings"]["text"], "hi");
        }
    }

    #[test]
    fn lists_collections_by_their_streamlabs_names() {
        let dir = std::env::temp_dir().join(format!("fc-sl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc-1.json"), sample().to_string()).unwrap();
        std::fs::write(dir.join("other.json"), "{\"name\":\"not streamlabs\"}").unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            json!({ "collections": [ { "id": "abc-1", "name": "Main" } ] }).to_string(),
        )
        .unwrap();
        let l = list(&dir);
        assert_eq!(l, [ObsCollection { id: "abc-1".into(), name: "Main".into(), scenes: 2, sources: 5 }]);
        let doc = read(&dir, "abc-1").unwrap();
        assert_eq!(doc["name"], "Main");
        assert!(read(&dir, "other").is_err());
        assert!(read(&dir, "../x").is_err());
        assert!(read(&dir, "manifest").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
