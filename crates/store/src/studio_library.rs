//! Scene collections and profiles, as OBS keeps them: several named sets of scenes, and several
//! sets of encoder/output settings, with one of each in use. Each lives in its own file under
//! `studio/collections/` or `studio/profiles/`; the list and the ones in use are in
//! `studio/library.json`. The single `scenes.json` and `settings.json` of earlier versions become
//! the first of each ("Untitled"), as OBS names them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::durable;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    pub current: String,
    pub list: Vec<Entry>,
}

fn is_win_reserved_device(id: &str) -> bool {
    let stem = id.split('.').next().unwrap_or(id);
    if stem.eq_ignore_ascii_case("con")
        || stem.eq_ignore_ascii_case("prn")
        || stem.eq_ignore_ascii_case("aux")
        || stem.eq_ignore_ascii_case("nul")
    {
        return true;
    }
    let bytes = stem.as_bytes();
    bytes.len() == 4
        && (stem[..3].eq_ignore_ascii_case("com") || stem[..3].eq_ignore_ascii_case("lpt"))
        && matches!(bytes[3], b'1'..=b'9')
}

impl Shelf {
    fn first() -> Shelf {
        Shelf { current: "untitled".into(), list: vec![Entry { id: "untitled".into(), name: "Untitled".into() }] }
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.list.iter().find(|e| e.id == id)
    }

    /// A name not taken yet ("Show", "Show 2", …), compared without case as OBS does.
    pub fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| self.list.iter().any(|e| e.name.eq_ignore_ascii_case(n));
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} {i}")).find(|c| !taken(c)).unwrap_or_else(|| base.to_string())
    }

    /// A safe file name from `name`, distinct from every id in `list` and from fixed route words
    /// (`current`, `import`, `overlay`).
    pub fn new_id(&self, name: &str) -> String {
        let mut slug = String::new();
        let mut dash = false;
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() {
                slug.push(ch.to_ascii_lowercase());
                dash = false;
            } else if !dash && !slug.is_empty() {
                slug.push('-');
                dash = true;
            }
            if slug.len() >= 40 {
                break;
            }
        }
        let slug = slug.trim_matches('-');
        let base = if slug.is_empty() { "set" } else { slug };
        let taken = |id: &str| {
            matches!(id, "current" | "import" | "overlay")
                || is_win_reserved_device(id)
                || self.list.iter().any(|e| e.id == id)
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base}-{i}")).find(|c| !taken(c)).unwrap_or_else(|| format!("set-{}", self.list.len()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub collections: Shelf,
    pub profiles: Shelf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Collections,
    Profiles,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "collections" => Some(Kind::Collections),
            "profiles" => Some(Kind::Profiles),
            _ => None,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Kind::Collections => "collections",
            Kind::Profiles => "profiles",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Kind::Collections => "scene collection",
            Kind::Profiles => "profile",
        }
    }
}

fn root(data: &Path) -> PathBuf {
    data.join("studio")
}

fn index(data: &Path) -> PathBuf {
    root(data).join("library.json")
}

/// The file holding one scene collection or profile.
pub fn file(data: &Path, kind: Kind, id: &str) -> PathBuf {
    root(data).join(kind.dir()).join(format!("{id}.json"))
}

impl Library {
    pub fn shelf(&self, kind: Kind) -> &Shelf {
        match kind {
            Kind::Collections => &self.collections,
            Kind::Profiles => &self.profiles,
        }
    }

    pub fn shelf_mut(&mut self, kind: Kind) -> &mut Shelf {
        match kind {
            Kind::Collections => &mut self.collections,
            Kind::Profiles => &mut self.profiles,
        }
    }

    /// The file for the collection or profile in use now.
    pub fn current_file(&self, data: &Path, kind: Kind) -> PathBuf {
        file(data, kind, &self.shelf(kind).current)
    }
}

/// The library, made the first time from the files earlier versions kept (moved, not copied).
pub fn load(data: &Path) -> Library {
    if let Some(loaded) = durable::load_json_with_backup::<Library>(&index(data), |lib| {
        lib.collections.get(&lib.collections.current).is_some() && lib.profiles.get(&lib.profiles.current).is_some()
    }) {
        return loaded.value;
    }
    let lib = Library { collections: Shelf::first(), profiles: Shelf::first() };
    let move_in = |old_name: &str, kind: Kind| {
        let old = root(data).join(old_name);
        let to = file(data, kind, &lib.shelf(kind).current);
        if old.exists() && !to.exists() {
            if let Some(p) = to.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if std::fs::rename(&old, &to).is_err() && std::fs::copy(&old, &to).is_ok() {
                let _ = std::fs::remove_file(&old);
            }
        }
    };
    move_in("scenes.json", Kind::Collections);
    move_in("settings.json", Kind::Profiles);
    save(data, &lib);
    lib
}

/// Writes the library, whole or not at all.
pub fn save(data: &Path, lib: &Library) {
    // Queued: this runs on the async runtime and the main thread (a blocking save can wait seconds
    // on a Windows virus scanner); reads see the queued bytes.
    if let Err(e) = durable::queue_json_pretty(&index(data), lib) {
        tracing::warn!(error = %e, "couldn't save Studio library");
    }
}

/// A name for a collection or profile: trimmed, 1 to 100 characters, no control characters.
pub fn check_name(name: &str) -> Result<String, String> {
    let t = name.trim();
    if t.is_empty() || t.chars().count() > 100 || t.chars().any(char::is_control) {
        return Err("name: 1 to 100 characters.".into());
    }
    Ok(t.to_string())
}

/// Imports an OBS-format collection document under `name` on disk, switches `current` to it,
/// and returns `(new_id, previous_id, saved_name)`.
pub fn import_collection_doc(data: &Path, name: &str, mut doc: Value) -> Result<(String, String, String), String> {
    let clean_name = check_name(name)?;
    let mut lib = load(data);
    let unique_name = lib.collections.free_name(&clean_name);
    let previous_id = lib.collections.current.clone();
    let id = lib.collections.new_id(&unique_name);
    doc["name"] = json!(unique_name);
    let dest = file(data, Kind::Collections, &id);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't prepare scene collections folder: {e}"))?;
    }
    durable::save_json_pretty(&dest, &doc).map_err(|e| format!("Couldn't save the imported scene collection: {e}"))?;
    lib.collections.list.push(Entry { id: id.clone(), name: unique_name.clone() });
    lib.collections.current = id.clone();
    save(data, &lib);
    Ok((id, previous_id, unique_name))
}

/// Switches back to `restore_id` (if still present) and removes an imported collection `remove_id`.
pub fn rollback_imported_collection(data: &Path, remove_id: &str, restore_id: &str) -> Result<String, String> {
    let mut lib = load(data);
    if lib.collections.get(remove_id).is_none() {
        return Err(format!("There's no scene collection with the id \"{remove_id}\"."));
    }
    if lib.collections.list.len() <= 1 {
        return Err("You need at least one scene collection.".into());
    }
    let next_current = if remove_id != restore_id && lib.collections.get(restore_id).is_some() {
        restore_id.to_string()
    } else if lib.collections.current == remove_id {
        lib.collections
            .list
            .iter()
            .find(|e| e.id != remove_id)
            .map(|e| e.id.clone())
            .ok_or_else(|| "You need at least one scene collection.".to_string())?
    } else {
        lib.collections.current.clone()
    };
    lib.collections.current = next_current.clone();
    lib.collections.list.retain(|e| e.id != remove_id);
    save(data, &lib);
    durable::remove_with_backup(&file(data, Kind::Collections, remove_id));
    Ok(next_current)
}

/// File-backed `Shelf` actions for headless mode when no desktop `StudioHost` is running.
pub fn headless_shelf_list(data: &Path, kind: &str) -> Result<Value, String> {
    let kind = Kind::parse(kind).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    Ok(json!(load(data).shelf(kind)))
}

pub fn headless_shelf_new(data: &Path, kind_str: &str, name: &str, copy: bool) -> Result<Value, String> {
    let kind = Kind::parse(kind_str).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    let name = check_name(name)?;
    let mut lib = load(data);
    if lib.shelf(kind).list.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
        return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
    }
    let id = lib.shelf(kind).new_id(&name);
    let to = file(data, kind, &id);
    if copy {
        let from = lib.current_file(data, kind);
        if let Ok(bytes) = std::fs::read(&from) {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            if let Ok(mut doc) = serde_json::from_slice::<Value>(&bytes) {
                if kind == Kind::Collections {
                    doc["name"] = json!(name);
                }
                durable::save_json_pretty(&to, &doc).map_err(|e| e.to_string())?;
            }
        }
    } else if kind == Kind::Collections {
        if let Some(p) = to.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        durable::save_json_pretty(
            &to,
            &json!({ "name": name, "current_scene": "Main", "current_program_scene": "Main", "sources": [] }),
        )
        .map_err(|e| e.to_string())?;
    }
    lib.shelf_mut(kind).list.push(Entry { id: id.clone(), name });
    lib.shelf_mut(kind).current = id.clone();
    save(data, &lib);
    Ok(json!({ "id": id }))
}

pub fn headless_shelf_switch(data: &Path, kind_str: &str, id: &str) -> Result<Value, String> {
    let kind = Kind::parse(kind_str).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    let mut lib = load(data);
    if lib.shelf(kind).get(id).is_none() {
        return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
    }
    lib.shelf_mut(kind).current = id.to_string();
    save(data, &lib);
    Ok(json!({ "id": id }))
}

pub fn headless_shelf_rename(data: &Path, kind_str: &str, id: &str, name: &str) -> Result<Value, String> {
    let kind = Kind::parse(kind_str).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    let name = check_name(name)?;
    let mut lib = load(data);
    if lib.shelf(kind).get(id).is_none() {
        return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
    }
    if lib.shelf(kind).list.iter().any(|e| e.id != id && e.name.eq_ignore_ascii_case(&name)) {
        return Err(format!("There's already a {} called \"{name}\".", kind.noun()));
    }
    if let Some(entry) = lib.shelf_mut(kind).list.iter_mut().find(|e| e.id == id) {
        entry.name = name;
    }
    save(data, &lib);
    Ok(Value::Null)
}

pub fn headless_shelf_remove(data: &Path, kind_str: &str, id: &str) -> Result<Value, String> {
    let kind = Kind::parse(kind_str).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    let mut lib = load(data);
    if let Some(entry) = lib.shelf(kind).get(id) {
        if lib.shelf(kind).current == id {
            return Err(format!("\"{}\" is in use now.", entry.name));
        }
    } else {
        return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
    }
    if lib.shelf(kind).list.len() <= 1 {
        return Err(format!("You need at least one {}.", kind.noun()));
    }
    lib.shelf_mut(kind).list.retain(|e| e.id != id);
    save(data, &lib);
    durable::remove_with_backup(&file(data, kind, id));
    Ok(Value::Null)
}

pub fn headless_shelf_export(data: &Path, kind_str: &str, id: &str) -> Result<Value, String> {
    let kind = Kind::parse(kind_str).ok_or_else(|| "kind: collections or profiles.".to_string())?;
    let lib = load(data);
    if lib.shelf(kind).get(id).is_none() {
        return Err(format!("There's no {} with the id \"{id}\".", kind.noun()));
    }
    let path = file(data, kind, id);
    let doc = durable::read_patiently(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle() {
        assert!(crate::durable::flush_saves(std::time::Duration::from_secs(10)));
    }

    #[test]
    fn earlier_files_become_the_first_of_each() {
        let dir = tempfile::tempdir().unwrap();
        let studio = dir.path().join("studio");
        std::fs::create_dir_all(&studio).unwrap();
        std::fs::write(studio.join("scenes.json"), b"{\"sources\":[]}").unwrap();
        std::fs::write(studio.join("settings.json"), b"{}").unwrap();
        let lib = load(dir.path());
        settle();
        assert_eq!(lib.collections.current, "untitled");
        assert_eq!(lib.collections.get(&lib.collections.current).unwrap().name, "Untitled");
        assert!(!studio.join("scenes.json").exists(), "moved, not copied");
        assert!(file(dir.path(), Kind::Collections, "untitled").exists());
        assert!(file(dir.path(), Kind::Profiles, "untitled").exists());
    }

    #[test]
    fn ids_and_names_stay_unique() {
        let mut s = Shelf::first();
        assert_eq!(s.new_id("My Show!"), "my-show");
        assert_eq!(s.new_id("Untitled"), "untitled-2");
        assert_eq!(s.new_id("✨✨"), "set");
        assert_eq!(s.new_id("Current"), "current-2", "route words aren't ids");
        assert_eq!(s.new_id("import"), "import-2");
        assert_eq!(s.new_id("overlay"), "overlay-2");
        assert_eq!(s.new_id("CON"), "con-2", "Windows DOS device names aren't ids");
        assert_eq!(s.new_id("nul"), "nul-2");
        assert_eq!(s.new_id("COM1"), "com1-2");
        assert_eq!(s.new_id("lpt9"), "lpt9-2");
        s.list.push(Entry { id: "gaming".into(), name: "Gaming".into() });
        assert_eq!(s.free_name("Gaming"), "Gaming 2");
        assert_eq!(s.free_name("gaming"), "gaming 2");
    }

    #[test]
    fn a_broken_index_starts_again_without_losing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("studio")).unwrap();
        std::fs::write(dir.path().join("studio/library.json"), b"not json").unwrap();
        let lib = load(dir.path());
        settle();
        assert_eq!(lib.collections.list.len(), 1);
        assert_eq!(lib.profiles.current, "untitled");
    }

    #[test]
    fn truncated_library_falls_back_to_bak() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let mut lib = load(dir.path());
        settle();
        lib.collections.list.push(Entry { id: "gaming".into(), name: "Gaming".into() });
        lib.collections.current = "gaming".into();
        save(dir.path(), &lib);
        settle();

        let bytes = std::fs::read(index(dir.path())).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(index(dir.path()), &bytes[..bytes.len() / 3]).unwrap_or_else(|e| panic!("{e}"));

        let recovered = load(dir.path());
        settle();
        assert_eq!(recovered.collections.list.len(), 1, "last good library before truncation was .bak");
        assert!(crate::durable::backup_path(&index(dir.path())).exists());

        // Second save -> third save preserves second save in .bak and survives truncation.
        let mut lib2 = recovered;
        lib2.collections.list.push(Entry { id: "gaming".into(), name: "Gaming".into() });
        lib2.collections.current = "gaming".into();
        save(dir.path(), &lib2);
        settle();
        let mut lib3 = lib2.clone();
        lib3.profiles.list.push(Entry { id: "podcast".into(), name: "Podcast".into() });
        save(dir.path(), &lib3);
        settle();

        let full = std::fs::read(index(dir.path())).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(index(dir.path()), &full[..full.len() / 2]).unwrap_or_else(|e| panic!("{e}"));
        let loaded = load(dir.path());
        settle();
        assert_eq!(loaded.collections.current, "gaming");
    }

    #[test]
    fn headless_import_and_rollback_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let initial = load(dir.path());
        assert_eq!(initial.collections.current, "untitled");
        let (id, prev, saved_name) = import_collection_doc(dir.path(), "Neon Pack", json!({ "sources": [] })).unwrap();
        assert_eq!(prev, "untitled");
        assert_eq!(saved_name, "Neon Pack");
        assert!(file(dir.path(), Kind::Collections, &id).exists());
        assert_eq!(load(dir.path()).collections.current, id);

        let restored = rollback_imported_collection(dir.path(), &id, &prev).unwrap();
        assert_eq!(restored, "untitled");
        assert!(!file(dir.path(), Kind::Collections, &id).exists());
        assert_eq!(load(dir.path()).collections.current, "untitled");
    }
}
