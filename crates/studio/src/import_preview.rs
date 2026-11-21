//! Read-only preview of OBS Studio and Streamlabs Desktop scene collections before importing:
//! counts scenes, sources, converted web sources, devices needing re-selection on this computer,
//! and anything that cannot come across (with plain explanations).

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::StudioError;

/// Upper bound on scene-collection JSON files read during preview to avoid excessive allocations.
pub const MAX_COLLECTION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCENE_NAMES: usize = 12;
const MAX_SKIP_ITEMS: usize = 50;

/// One item, filter, device reset, or broken reference explained in plain words before importing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportSkip {
    /// `"source"`, `"filter"`, or `"scene_item"`.
    pub category: &'static str,
    pub name: String,
    pub kind: String,
    pub reason: String,
}

/// Summary produced without changing any Ferrocast Studio collection or starting the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPreview {
    /// `"obs"` or `"streamlabs"`.
    pub source: &'static str,
    pub id: String,
    pub name: String,
    pub scenes: usize,
    pub sources: usize,
    pub web_pages: usize,
    pub scene_names: Vec<String>,
    pub truncated_scenes: usize,
    pub device_resets: Vec<ImportSkip>,
    pub skipped: Vec<ImportSkip>,
    pub skipped_total: usize,
    pub missing: Vec<String>,
}

/// Resolves OBS's scene collections directory (`FERROCAST_OBS_SCENES_DIR` overrides for fixtures).
pub fn obs_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("FERROCAST_OBS_SCENES_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(v));
    }
    crate::import::obs_scenes_dir()
}

/// Resolves Streamlabs Desktop's scene collections directory (`FERROCAST_STREAMLABS_SCENES_DIR` overrides).
pub fn streamlabs_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("FERROCAST_STREAMLABS_SCENES_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(v));
    }
    crate::streamlabs::scenes_dir()
}

/// Input and filter kinds supported by this Ferrocast Studio build (including OBS aliases).
pub fn supported_kinds() -> (BTreeSet<String>, BTreeSet<String>) {
    let k = crate::kinds::kinds();
    let mut inputs: BTreeSet<String> = k.inputs.into_iter().map(|i| i.id).collect();
    inputs.extend(
        [
            "color_source",
            "color_source_v2",
            "text_ft2_source",
            "display_capture",
            "av_capture_input",
            "macos-avcapture",
            "macos-avcapture-fast",
            "coreaudio_output_capture",
            "browser_source",
        ]
        .into_iter()
        .map(str::to_string),
    );
    let mut filters: BTreeSet<String> = k.filters.into_iter().map(|f| f.id).collect();
    filters.extend(
        [
            "chroma_key_filter",
            "color_key_filter",
            "color_filter",
            "luma_key_filter",
            "sharpness_filter",
            "mask_filter",
            "noise_suppress_filter",
        ]
        .into_iter()
        .map(str::to_string),
    );
    (inputs, filters)
}

fn read_json_limited(path: &Path, missing_msg: &str, damaged_msg: &str) -> Result<Value, StudioError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(StudioError::Failed(missing_msg.to_string()));
        }
        Err(_) => return Err(StudioError::Failed(missing_msg.to_string())),
    };
    if !meta.is_file() {
        return Err(StudioError::Failed(missing_msg.to_string()));
    }
    if meta.len() > MAX_COLLECTION_BYTES {
        return Err(StudioError::Failed("The collection file is too large to preview.".into()));
    }
    let bytes = std::fs::read(path).map_err(|_| StudioError::Failed(missing_msg.to_string()))?;
    if bytes.is_empty() {
        return Err(StudioError::Failed(damaged_msg.to_string()));
    }
    serde_json::from_slice(&bytes).map_err(|_| StudioError::Failed(damaged_msg.to_string()))
}

fn reason_for_source(kind: &str) -> String {
    match kind {
        "game_capture"
        | "window_capture"
        | "monitor_capture"
        | "wasapi_process_output_capture"
        | "wasapi_input_capture"
        | "wasapi_output_capture"
        | "dshow_input"
        | "text_gdiplus"
        | "text_gdiplus_v2"
        | "text_gdiplus_v3" => {
            format!("Uses Windows source \"{kind}\", which isn't available on this computer.")
        }
        "xcomposite_input"
        | "xshm_input"
        | "pipewire-screen-capture-source"
        | "v4l2_input"
        | "pulse_input_capture"
        | "pulse_output_capture"
        | "alsa_input_capture"
        | "jack_output_capture" => {
            format!("Uses Linux source \"{kind}\", which isn't available on this computer.")
        }
        "av_capture_input"
        | "av_capture_input_v2"
        | "screen_capture"
        | "sck_audio_capture"
        | "coreaudio_input_capture"
        | "coreaudio_output_capture" => {
            format!("Uses macOS source \"{kind}\", which isn't available on this computer.")
        }
        "ndi_source" | "obs_ndi_source" => {
            "Uses NDI. Install or enable NDI on this computer before using this source.".into()
        }
        "spout_capture" => "Spout capture only works on Windows and cannot run here.".into(),
        "browser_source" => "Embedded browser source isn't available in this build.".into(),
        k if k.starts_with("streamlabs_") => {
            format!("Streamlabs native feature \"{k}\" doesn't have a built-in Ferrocast source.")
        }
        "" => "This source is missing its source type in the file.".into(),
        k => format!("Uses plugin source \"{k}\", which isn't installed in Ferrocast."),
    }
}

fn reason_for_filter(kind: &str) -> String {
    match kind {
        k if k.starts_with("vst") => {
            format!("VST plugin filter \"{k}\" isn't migrated automatically; re-add it in Studio if installed.")
        }
        "" => "This filter is missing its filter type in the file.".into(),
        k => format!("Uses plugin filter \"{k}\", which isn't available in Ferrocast."),
    }
}

fn reason_for_device_reset(original_kind: &str) -> String {
    match original_kind {
        "dshow_input" | "v4l2_input" | "av_capture_input" | "av_capture_input_v2" => {
            "Comes from another system and arrives empty so you can pick a camera on this computer.".into()
        }
        "wasapi_input_capture" | "pulse_input_capture" | "alsa_input_capture" | "coreaudio_input_capture" => {
            "Comes from another system and arrives empty so you can pick your microphone here.".into()
        }
        "wasapi_output_capture"
        | "wasapi_process_output_capture"
        | "pulse_output_capture"
        | "jack_output_capture"
        | "sck_audio_capture"
        | "coreaudio_output_capture" => {
            "Comes from another system and arrives empty so you can pick desktop sound on this computer.".into()
        }
        _ => "Comes from another system and arrives empty so you can pick a display or window here.".into(),
    }
}

fn push_limited(list: &mut Vec<ImportSkip>, total: Option<&mut usize>, item: ImportSkip) {
    if let Some(count) = total {
        *count += 1;
    }
    if list.len() < MAX_SKIP_ITEMS {
        list.push(item);
    }
}

fn is_valid_obs_doc(doc: &Value) -> bool {
    let Some(obj) = doc.as_object() else {
        return false;
    };
    obj.get("sources").is_none_or(Value::is_array)
}

/// Previews one parsed OBS collection JSON object without mutating `doc`.
pub fn preview_obs_doc(
    id: &str,
    doc: &Value,
    web_kind: Option<&str>,
    known_inputs: Option<&BTreeSet<String>>,
    known_filters: Option<&BTreeSet<String>>,
) -> Result<ImportPreview, StudioError> {
    if !is_valid_obs_doc(doc) {
        return Err(StudioError::Failed(format!("\"{id}\" isn't an OBS scene collection.")));
    }
    let name =
        doc.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id).to_string();
    let list = doc.get("sources").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);

    let mut scenes = 0usize;
    let mut sources = 0usize;
    let mut web_pages = 0usize;
    let mut scene_names = Vec::new();
    let mut skipped = Vec::new();
    let mut skipped_total = 0usize;
    let mut missing = BTreeSet::new();

    for entry in list {
        let kind = entry.get("id").and_then(Value::as_str).unwrap_or_default();
        let entry_name = entry.get("name").and_then(Value::as_str).unwrap_or("Source").to_string();

        if kind == "scene" {
            scenes += 1;
            if scene_names.len() < MAX_SCENE_NAMES {
                scene_names.push(entry_name);
            }
            continue;
        }
        sources += 1;

        let effective_kind = if kind == "browser_source" {
            if let Some(wk) = web_kind {
                web_pages += 1;
                wk
            } else {
                kind
            }
        } else {
            kind
        };

        if let Some(known) = known_inputs
            && kind != "group"
            && !known.contains(effective_kind)
            && !known.contains(kind)
        {
            missing.insert(kind.to_string());
            push_limited(
                &mut skipped,
                Some(&mut skipped_total),
                ImportSkip {
                    category: "source",
                    name: entry_name.clone(),
                    kind: kind.to_string(),
                    reason: reason_for_source(kind),
                },
            );
        }

        if let Some(k_filters) = known_filters
            && let Some(filters) = entry.get("filters").and_then(Value::as_array)
        {
            for flt in filters {
                let f_kind = flt.get("id").and_then(Value::as_str).unwrap_or_default();
                let f_name = flt.get("name").and_then(Value::as_str).unwrap_or("Filter");
                if !k_filters.contains(f_kind) {
                    push_limited(
                        &mut skipped,
                        Some(&mut skipped_total),
                        ImportSkip {
                            category: "filter",
                            name: format!("{entry_name} / {f_name}"),
                            kind: f_kind.to_string(),
                            reason: reason_for_filter(f_kind),
                        },
                    );
                }
            }
        }
    }

    Ok(ImportPreview {
        source: "obs",
        id: id.to_string(),
        name,
        scenes,
        sources,
        web_pages,
        truncated_scenes: scenes.saturating_sub(scene_names.len()),
        scene_names,
        device_resets: Vec::new(),
        skipped,
        skipped_total,
        missing: missing.into_iter().collect(),
    })
}

/// Previews OBS scene collection `id` from `dir` without modifying anything.
pub fn preview_obs(
    dir: &Path,
    id: &str,
    web_kind: Option<&str>,
    known_inputs: Option<&BTreeSet<String>>,
    known_filters: Option<&BTreeSet<String>>,
) -> Result<ImportPreview, StudioError> {
    if !crate::import::valid_id(id) {
        return Err(StudioError::Failed("That isn't one of OBS's scene collections.".into()));
    }
    let path = dir.join(format!("{id}.json"));
    let doc = read_json_limited(
        &path,
        &format!("OBS's scene collection \"{id}\" isn't there any more."),
        &format!("OBS's scene collection \"{id}\" couldn't be read because the JSON is damaged."),
    )?;
    preview_obs_doc(id, &doc, web_kind, known_inputs, known_filters)
}

fn streamlabs_collection_name(dir: &Path, id: &str) -> String {
    let manifest = dir.join("manifest.json");
    let Ok(bytes) = std::fs::read(manifest) else {
        return "Streamlabs scenes".into();
    };
    let Ok(val) = serde_json::from_slice::<Value>(&bytes) else {
        return "Streamlabs scenes".into();
    };
    val.get("collections")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
                .and_then(|entry| entry.get("name").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Streamlabs scenes")
        .to_string()
}

fn streamlabs_array<'a>(v: &'a Value, key: &str) -> Option<&'a [Value]> {
    v.get(key).and_then(|x| x.get("items")).and_then(Value::as_array).map(Vec::as_slice)
}

/// Previews Streamlabs Desktop scene collection `id` from `dir` without modifying anything.
pub fn preview_streamlabs(
    dir: &Path,
    id: &str,
    web_kind: Option<&str>,
    known_inputs: Option<&BTreeSet<String>>,
    known_filters: Option<&BTreeSet<String>>,
) -> Result<ImportPreview, StudioError> {
    if !crate::import::valid_id(id) || id == "manifest" {
        return Err(StudioError::Failed("That isn't one of Streamlabs' scene collections.".into()));
    }
    let path = dir.join(format!("{id}.json"));
    let doc = read_json_limited(
        &path,
        "That Streamlabs scene collection isn't there any more.",
        "Streamlabs' scene collection couldn't be read because the JSON is damaged.",
    )?;
    let (Some(raw_sources), Some(raw_scenes)) =
        (doc.is_object().then(|| streamlabs_array(&doc, "sources")).flatten(), streamlabs_array(&doc, "scenes"))
    else {
        return Err(StudioError::Failed("That file isn't a Streamlabs scene collection.".into()));
    };

    let name = streamlabs_collection_name(dir, id);
    let mut device_resets = Vec::new();
    let mut source_ids = HashSet::with_capacity(raw_sources.len());

    for src in raw_sources {
        if let Some(sid) = src.get("id").and_then(Value::as_str) {
            source_ids.insert(sid);
        }
        let kind = src.get("type").and_then(Value::as_str).unwrap_or_default();
        let src_name = src.get("name").and_then(Value::as_str).unwrap_or("Source").to_string();
        let (_, keep_settings) = crate::streamlabs::for_this_computer(kind);
        if !keep_settings {
            push_limited(
                &mut device_resets,
                None,
                ImportSkip {
                    category: "source",
                    name: src_name,
                    kind: kind.to_string(),
                    reason: reason_for_device_reset(kind),
                },
            );
        }
    }

    let scene_ids: HashSet<&str> = raw_scenes.iter().filter_map(|s| s.get("id").and_then(Value::as_str)).collect();
    let mut extra_skips = Vec::new();
    let mut extra_skips_total = 0usize;

    for sc in raw_scenes {
        let sc_name = sc.get("name").and_then(Value::as_str).unwrap_or("Scene");
        if let Some(items) = streamlabs_array(sc, "sceneItems") {
            for item in items {
                let target_id = item.get("sourceId").and_then(Value::as_str).unwrap_or_default();
                if !source_ids.contains(target_id) && !scene_ids.contains(target_id) {
                    push_limited(
                        &mut extra_skips,
                        Some(&mut extra_skips_total),
                        ImportSkip {
                            category: "scene_item",
                            name: sc_name.to_string(),
                            kind: target_id.to_string(),
                            reason: "Scene item points to a missing source reference in the Streamlabs file.".into(),
                        },
                    );
                }
            }
        }
    }

    let obs_doc = crate::streamlabs::to_obs(&doc, &name);
    let mut preview = preview_obs_doc(id, &obs_doc, web_kind, known_inputs, known_filters)?;
    preview.source = "streamlabs";
    preview.device_resets = device_resets;
    preview.skipped_total += extra_skips_total;
    for skip in extra_skips {
        if preview.skipped.len() >= MAX_SKIP_ITEMS {
            break;
        }
        preview.skipped.push(skip);
    }
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fail<T>(res: Result<T, impl std::fmt::Display>) -> String {
        match res {
            Ok(_) => panic!("expected Err"),
            Err(e) => e.to_string(),
        }
    }

    fn ok<T>(res: Result<T, impl std::fmt::Display>) -> T {
        res.unwrap_or_else(|e| panic!("unexpected error: {e}"))
    }

    fn make_huge_obs(scenes: usize, sources_per_scene: usize, bad_count: usize) -> Value {
        let mut list = Vec::with_capacity(scenes + scenes * sources_per_scene + bad_count);
        for s in 0..scenes {
            list.push(json!({
                "id": "scene",
                "name": format!("Scene {s:03}"),
                "settings": { "items": [] }
            }));
            for i in 0..sources_per_scene {
                let kind = if i == 0 { "browser_source" } else { "image_source" };
                list.push(json!({
                    "id": kind,
                    "name": format!("Src {s:03}-{i:02}"),
                    "settings": { "url": "https://example.com" }
                }));
            }
        }
        for u in 0..bad_count {
            list.push(json!({
                "id": "obs_thirdparty_plugin",
                "name": format!("Custom Plugin {u}"),
                "settings": {}
            }));
        }
        json!({ "name": "Mega Show", "sources": list })
    }

    fn make_huge_streamlabs(scenes: usize, sources_per_scene: usize) -> Value {
        let mut sc_items = Vec::with_capacity(scenes);
        let mut src_items = Vec::with_capacity(scenes * sources_per_scene);
        for s in 0..scenes {
            let mut item_refs = Vec::with_capacity(sources_per_scene);
            for i in 0..sources_per_scene {
                let sid = format!("src-{s}-{i}");
                let ty = match i % 3 {
                    0 => "browser_source",
                    1 => "image_source",
                    _ => "dshow_input",
                };
                src_items.push(json!({
                    "id": sid,
                    "name": format!("SL {s}-{i}"),
                    "type": ty,
                    "settings": {},
                    "filters": { "items": [] }
                }));
                item_refs.push(json!({ "sourceId": sid, "visible": true }));
            }
            sc_items.push(json!({
                "id": format!("sc-{s}"),
                "name": format!("SL Scene {s:03}"),
                "sceneItems": { "items": item_refs }
            }));
        }
        json!({
            "scenes": { "items": sc_items },
            "sources": { "items": src_items },
            "transitions": { "items": [], "defaultTransitionId": "" }
        })
    }

    #[test]
    fn obs_fixture_folder_empty_and_blank_collection() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        assert!(crate::import::list(dir.path()).is_empty());

        let (inputs, filters) = supported_kinds();
        let err = fail(preview_obs(dir.path(), "missing", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(err.contains("isn't there"), "unexpected msg: {err}");

        std::fs::write(dir.path().join("blank.json"), json!({ "name": "Blank", "sources": [] }).to_string())
            .unwrap_or_else(|e| panic!("{e}"));
        let p = ok(preview_obs(dir.path(), "blank", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert_eq!(p.name, "Blank");
        assert_eq!(p.scenes, 0);
        assert_eq!(p.sources, 0);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn obs_fixture_folder_broken_json_and_bad_structures() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("broken.json"), "{not json").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("array.json"), "[1, 2, 3]").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("wrong_sources.json"), json!({ "name": "Bad", "sources": 42 }).to_string())
            .unwrap_or_else(|e| panic!("{e}"));

        // Listing skips corrupt or structurally invalid files without panicking.
        assert!(crate::import::list(dir.path()).is_empty());

        std::fs::write(dir.path().join("good.json"), json!({ "name": "Good", "sources": [] }).to_string())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(crate::import::list(dir.path()).len(), 1);

        let (inputs, filters) = supported_kinds();
        let e1 = fail(preview_obs(dir.path(), "broken", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(e1.contains("JSON is damaged"), "{e1}");
        let e2 = fail(preview_obs(dir.path(), "array", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(e2.contains("isn't an OBS"), "{e2}");
        let e3 =
            fail(preview_obs(dir.path(), "wrong_sources", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(e3.contains("isn't an OBS"), "{e3}");
        let bad_id = fail(preview_obs(dir.path(), "../secret", None, None, None));
        assert!(bad_id.contains("isn't one of OBS's"), "{bad_id}");
    }

    #[test]
    fn obs_fixture_folder_unknown_sources_filters_and_conversions() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let doc = json!({
            "name": "Stream Setup",
            "sources": [
                { "id": "scene", "name": "Gameplay", "settings": {} },
                { "id": "scene", "name": "BRB", "settings": {} },
                { "id": "browser_source", "name": "Alerts", "settings": { "url": "https://a.test" } },
                {
                    "id": "image_source",
                    "name": "Frame",
                    "settings": {},
                    "filters": [
                        { "id": "crop_filter", "name": "Trim" },
                        { "id": "vst_filter", "name": "Legacy VST" }
                    ]
                },
                { "id": "spout_capture", "name": "Spout Feed", "settings": {} },
                { "id": "custom_plugin_v1", "name": "Fancy Wheel", "settings": {} }
            ]
        });
        std::fs::write(dir.path().join("setup.json"), doc.to_string()).unwrap_or_else(|e| panic!("{e}"));

        let (inputs, filters) = supported_kinds();
        let p = ok(preview_obs(dir.path(), "setup", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert_eq!(p.source, "obs");
        assert_eq!(p.name, "Stream Setup");
        assert_eq!((p.scenes, p.sources, p.web_pages), (2, 4, 1));
        assert_eq!(p.scene_names, vec!["Gameplay", "BRB"]);
        assert_eq!(p.truncated_scenes, 0);
        assert_eq!(p.missing, vec!["custom_plugin_v1", "spout_capture"]);
        assert_eq!(p.skipped_total, 3);
        assert!(p.skipped.iter().any(|s| s.category == "filter" && s.kind == "vst_filter" && !s.reason.is_empty()));
        assert!(p.skipped.iter().any(|s| s.category == "source" && s.kind == "spout_capture" && !s.reason.is_empty()));
    }

    #[test]
    fn obs_fixture_folder_huge_collection_is_bounded() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let doc = make_huge_obs(150, 15, 75);
        std::fs::write(dir.path().join("huge.json"), doc.to_string()).unwrap_or_else(|e| panic!("{e}"));

        let (inputs, filters) = supported_kinds();
        let p = ok(preview_obs(dir.path(), "huge", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert_eq!(p.scenes, 150);
        assert_eq!(p.sources, 150 * 15 + 75);
        assert_eq!(p.web_pages, 150);
        assert_eq!(p.scene_names.len(), MAX_SCENE_NAMES);
        assert_eq!(p.truncated_scenes, 150 - MAX_SCENE_NAMES);
        assert_eq!(p.skipped_total, 75);
        assert_eq!(p.skipped.len(), MAX_SKIP_ITEMS);
    }

    #[test]
    fn streamlabs_fixture_folder_empty_broken_unknown_devices_and_huge() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (inputs, filters) = supported_kinds();

        // Empty folder
        assert!(crate::streamlabs::list(dir.path()).is_empty());
        let e = fail(preview_streamlabs(dir.path(), "sl-1", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(e.contains("isn't there"), "{e}");

        // Corrupt manifest still lets valid collection file preview; corrupt collection returns error.
        std::fs::write(dir.path().join("manifest.json"), "{broken manifest").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("bad.json"), "{broken json").unwrap_or_else(|e| panic!("{e}"));
        let e_broken =
            fail(preview_streamlabs(dir.path(), "bad", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert!(e_broken.contains("JSON is damaged"), "{e_broken}");

        // Rich Streamlabs collection: widgets, unknown source/filter, missing scene item, cross-OS devices
        let rich = json!({
            "scenes": {
                "items": [
                    {
                        "id": "sc-main",
                        "name": "Main Live",
                        "sceneItems": {
                            "items": [
                                { "sourceId": "s-alert", "visible": true },
                                { "sourceId": "s-missing", "visible": true },
                                { "sourceId": "s-cam", "visible": true }
                            ]
                        }
                    }
                ]
            },
            "sources": {
                "items": [
                    {
                        "id": "s-alert",
                        "name": "Alert Box",
                        "type": "browser_source",
                        "settings": { "url": "https://streamlabs.com/alert-box/v3/abc" }
                    },
                    {
                        "id": "s-cam",
                        "name": "StreamCam",
                        "type": "dshow_input",
                        "settings": {}
                    },
                    {
                        "id": "s-mic",
                        "name": "Shure Mic",
                        "type": "wasapi_input_capture",
                        "settings": {},
                        "filters": {
                            "items": [{ "type": "custom_fx_filter", "name": "MagicFX", "settings": {} }]
                        }
                    },
                    {
                        "id": "s-wheel",
                        "name": "Spin Wheel",
                        "type": "streamlabs_spin_wheel",
                        "settings": {}
                    }
                ]
            },
            "transitions": { "items": [], "defaultTransitionId": "" }
        });
        std::fs::write(dir.path().join("sl-rich.json"), rich.to_string()).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            dir.path().join("manifest.json"),
            json!({ "collections": [{ "id": "sl-rich", "name": "Neon Pack" }] }).to_string(),
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let p =
            ok(preview_streamlabs(dir.path(), "sl-rich", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert_eq!(p.source, "streamlabs");
        assert_eq!(p.name, "Neon Pack");
        assert_eq!(p.scenes, 1);
        assert_eq!(p.sources, 4);
        assert_eq!(p.web_pages, 1);
        assert!(p.missing.contains(&"streamlabs_spin_wheel".to_string()));
        assert!(p.skipped.iter().any(|s| s.category == "scene_item" && !s.reason.is_empty()));
        assert!(p.skipped.iter().any(|s| s.category == "filter" && s.kind == "custom_fx_filter"));
        if cfg!(target_os = "macos") {
            assert_eq!(p.device_resets.len(), 2, "{:?}", p.device_resets);
        }

        // Huge Streamlabs collection
        let huge = make_huge_streamlabs(80, 20);
        std::fs::write(dir.path().join("sl-huge.json"), huge.to_string()).unwrap_or_else(|e| panic!("{e}"));
        let p_huge =
            ok(preview_streamlabs(dir.path(), "sl-huge", Some(crate::WEB_PAGE_KIND), Some(&inputs), Some(&filters)));
        assert_eq!(p_huge.scenes, 80);
        assert_eq!(p_huge.sources, 1600);
        assert_eq!(p_huge.scene_names.len(), MAX_SCENE_NAMES);
        assert_eq!(p_huge.truncated_scenes, 80 - MAX_SCENE_NAMES);
    }
}
