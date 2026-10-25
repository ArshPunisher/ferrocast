//! Bringing in a scene collection from OBS (`obs-studio/basic/scenes/*.json`). The Studio
//! saves in the same format, so a collection loads as it is; only OBS's Browser source is
//! changed into Ferrocast's Web page source on macOS (OBS's needs Chromium inside OBS's own
//! app), keeping its address, size, CSS and frame rate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::StudioError;

/// One of OBS's scene collections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObsCollection {
    /// The file's name without `.json` (what to import it by).
    pub id: String,
    /// The name OBS shows.
    pub name: String,
    pub scenes: usize,
    pub sources: usize,
}

/// What an import brought in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub name: String,
    pub scenes: usize,
    pub sources: usize,
    /// Browser sources changed into Web page sources.
    pub web_pages: usize,
    /// Kinds of source this computer's OBS doesn't have (they stay, empty, as in OBS).
    pub missing: Vec<String>,
}

/// Where OBS keeps its scene collections for this user, if OBS has been run.
pub fn obs_scenes_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?
    };
    let dir = base.join("obs-studio/basic/scenes");
    dir.is_dir().then_some(dir)
}

/// A collection id is a plain file name: letters, digits, spaces, `_`, `-` and `.`, not
/// starting with a dot.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
}

fn counts(doc: &Value) -> (usize, usize) {
    let list = doc.get("sources").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let scenes = list.iter().filter(|s| s.get("id").and_then(Value::as_str) == Some("scene")).count();
    (scenes, list.len() - scenes)
}

/// The collections in `dir`, by name (OBS's backups, `*.json.bak`, aren't listed).
pub fn list(dir: &Path) -> Vec<ObsCollection> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<ObsCollection> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "json" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            if !valid_id(&id) {
                return None;
            }
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            let (scenes, sources) = counts(&doc);
            let name = doc.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ObsCollection { id, name, scenes, sources })
        })
        .collect();
    out.sort_by_key(|c| c.name.to_lowercase());
    out
}

/// Reads collection `id` from `dir`.
pub fn read(dir: &Path, id: &str) -> Result<Value, StudioError> {
    if !valid_id(id) {
        return Err(StudioError::Failed("That isn't one of OBS's scene collections.".into()));
    }
    let path = dir.join(format!("{id}.json"));
    let bytes = std::fs::read(&path)
        .map_err(|_| StudioError::Failed(format!("OBS's scene collection \"{id}\" isn't there any more.")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| StudioError::Failed(format!("OBS's scene collection \"{id}\" didn't read ({e}).")))
}

/// A file path as a `file://` address (spaces and the few characters that would end or
/// change one are escaped).
fn file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for c in path.replace('\\', "/").chars() {
        match c {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    if !path.starts_with('/') && path.chars().nth(1) == Some(':') {
        // C:/… on Windows: file:///C:/…
        out.insert(7, '/');
    }
    out
}

/// Changes the collection for the Studio: Browser sources into `web_kind` when given, and
/// notes kinds not in `known` (when given). Returns what's in it.
pub fn convert(doc: &mut Value, web_kind: Option<&str>, known: Option<&BTreeSet<String>>) -> ImportReport {
    let name = doc.get("name").and_then(Value::as_str).unwrap_or("OBS").to_string();
    let mut web_pages = 0;
    let mut missing = BTreeSet::new();
    if let Some(list) = doc.get_mut("sources").and_then(Value::as_array_mut) {
        for s in list.iter_mut() {
            let id = s.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            if id == "browser_source"
                && let Some(kind) = web_kind
            {
                web_pages += 1;
                s["id"] = json!(kind);
                s["versioned_id"] = json!(kind);
                let set = s.get("settings").cloned().unwrap_or_else(|| json!({}));
                let local = set.get("is_local_file").and_then(Value::as_bool).unwrap_or(false);
                let file = set.get("local_file").and_then(Value::as_str).unwrap_or_default();
                let url = if local && !file.is_empty() {
                    file_url(file)
                } else {
                    set.get("url").and_then(Value::as_str).unwrap_or_default().to_string()
                };
                let mut out = json!({ "url": url });
                for k in ["width", "height", "css"] {
                    if let Some(v) = set.get(k) {
                        out[k] = v.clone();
                    }
                }
                // OBS uses the canvas's rate unless a custom one is set.
                if set.get("fps_custom").and_then(Value::as_bool).unwrap_or(false)
                    && let Some(fps) = set.get("fps")
                {
                    out["fps"] = fps.clone();
                }
                s["settings"] = out;
                continue;
            }
            if let Some(k) = known
                && !id.is_empty()
                && id != "scene"
                && id != "group"
                && !k.contains(&id)
            {
                missing.insert(id);
            }
        }
    }
    let (scenes, sources) = counts(doc);
    ImportReport { name, scenes, sources, web_pages, missing: missing.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        json!({
            "name": "My show",
            "sources": [
                { "id": "scene", "name": "Gameplay", "settings": {} },
                { "id": "scene", "name": "Chatting", "settings": {} },
                { "id": "browser_source", "versioned_id": "browser_source", "name": "Alerts",
                  "settings": { "url": "http://127.0.0.1:47820/o/abc", "width": 1280, "height": 720,
                                "css": "body{}", "fps": 60, "fps_custom": true, "reroute_audio": true } },
                { "id": "browser_source", "versioned_id": "browser_source", "name": "Local",
                  "settings": { "is_local_file": true, "local_file": "/Users/me/My Overlay/index.html" } },
                { "id": "macos-avcapture", "name": "Camera", "settings": {} },
                { "id": "some_plugin_source", "name": "Plugin", "settings": {} }
            ]
        })
    }

    #[test]
    fn browser_sources_become_web_pages() {
        let mut d = doc();
        let known: BTreeSet<String> = ["macos-avcapture".to_string()].into();
        let r = convert(&mut d, Some("ferrocast_web"), Some(&known));
        assert_eq!(r.name, "My show");
        assert_eq!((r.scenes, r.sources, r.web_pages), (2, 4, 2));
        assert_eq!(r.missing, vec!["some_plugin_source".to_string()]);
        let alerts = &d["sources"][2];
        assert_eq!(alerts["id"], "ferrocast_web");
        assert_eq!(alerts["versioned_id"], "ferrocast_web");
        assert_eq!(
            alerts["settings"],
            json!({ "url": "http://127.0.0.1:47820/o/abc", "width": 1280, "height": 720, "css": "body{}", "fps": 60 })
        );
        assert_eq!(d["sources"][3]["settings"]["url"], "file:///Users/me/My%20Overlay/index.html");
    }

    #[test]
    fn without_a_web_kind_nothing_changes() {
        let mut d = doc();
        let before = d.clone();
        let r = convert(&mut d, None, None);
        assert_eq!(d, before);
        assert_eq!(r.web_pages, 0);
        assert!(r.missing.is_empty());
    }

    #[test]
    fn ids_are_plain_file_names() {
        assert!(valid_id("Untitled"));
        assert!(valid_id("My Show 2"));
        assert!(!valid_id("../secrets"));
        assert!(!valid_id(".hidden"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
    }

    #[test]
    fn windows_paths_become_file_urls() {
        assert_eq!(file_url("C:\\Overlays\\a b.html"), "file:///C:/Overlays/a%20b.html");
    }

    #[test]
    fn lists_collections_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.json"), doc().to_string()).unwrap();
        std::fs::write(dir.path().join("a.json"), json!({ "name": "Another", "sources": [] }).to_string()).unwrap();
        std::fs::write(dir.path().join("b.json.bak"), "{}").unwrap();
        std::fs::write(dir.path().join("broken.json"), "{").unwrap();
        let l = list(dir.path());
        assert_eq!(l.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Another", "My show"]);
        assert_eq!((l[1].scenes, l[1].sources), (2, 4));
        assert!(read(dir.path(), "../b").is_err());
        assert_eq!(read(dir.path(), "b").unwrap()["name"], "My show");
    }
}
