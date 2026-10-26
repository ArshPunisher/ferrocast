//! Scene collections and profiles, as OBS keeps them: several named sets of scenes, and several
//! named sets of Studio settings (stream, picture, sound, recording, shortcuts), one of each in
//! use. Each lives in its own file under `studio/collections/` or `studio/profiles/`; the list
//! and the ones in use are in `studio/library.json`. The single `scenes.json` and
//! `settings.json` of earlier versions become the first of each ("Untitled"), as OBS names them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The file's name (letters, digits, `-`); stays when renamed.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    pub current: String,
    pub list: Vec<Entry>,
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
        (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_string())
    }

    /// A new id for `name`: its letters and digits, made unique.
    pub fn new_id(&self, name: &str) -> String {
        let mut slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        slug.truncate(40);
        if slug.is_empty() {
            slug = "set".into();
        }
        // "current" and "import" are words in the routes (/library/{kind}/current): never ids.
        let free = |id: &str| !matches!(id, "current" | "import") && self.get(id).is_none();
        if free(&slug) {
            return slug;
        }
        (2..).map(|i| format!("{slug}-{i}")).find(|id| free(id)).unwrap_or(slug)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub collections: Shelf,
    pub profiles: Shelf,
}

/// Which list.
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

    /// What it's called in messages.
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

/// The file of entry `id`.
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

    /// The file of the one in use.
    pub fn current_file(&self, data: &Path, kind: Kind) -> PathBuf {
        file(data, kind, &self.shelf(kind).current)
    }
}

/// The library, made the first time from the files earlier versions kept (moved, not copied).
pub fn load(data: &Path) -> Library {
    if let Some(lib) =
        std::fs::read(index(data)).ok().and_then(|b| serde_json::from_slice::<Library>(&b).ok()).filter(|l| {
            l.collections.get(&l.collections.current).is_some() && l.profiles.get(&l.profiles.current).is_some()
        })
    {
        return lib;
    }
    let lib = Library { collections: Shelf::first(), profiles: Shelf::first() };
    for (old, kind) in [("scenes.json", Kind::Collections), ("settings.json", Kind::Profiles)] {
        let from = root(data).join(old);
        let to = lib.current_file(data, kind);
        if from.exists() && !to.exists() {
            if let Some(dir) = to.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::rename(&from, &to) {
                tracing::warn!(error = %e, file = %from.display(), "an earlier Studio file couldn't move into the library");
            }
        }
    }
    save(data, &lib);
    lib
}

/// Writes the library, whole or not at all.
pub fn save(data: &Path, lib: &Library) {
    let file = index(data);
    let tmp = file.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(lib)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            std::fs::create_dir_all(root(data))?;
            std::fs::write(&tmp, bytes)
        })
        .and_then(|()| std::fs::rename(&tmp, &file));
    if let Err(e) = written {
        tracing::warn!(error = %e, "the Studio's library couldn't be saved");
    }
}

/// Checks a name for a collection or profile.
pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 100 || n.chars().any(char::is_control) {
        return Err("A name of 1 to 100 characters, please.".into());
    }
    Ok(n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earlier_files_become_the_first_of_each() {
        let dir = tempfile::tempdir().unwrap();
        let studio = dir.path().join("studio");
        std::fs::create_dir_all(&studio).unwrap();
        std::fs::write(studio.join("scenes.json"), b"{\"sources\":[]}").unwrap();
        std::fs::write(studio.join("settings.json"), b"{}").unwrap();
        let lib = load(dir.path());
        assert_eq!(lib.collections.get(&lib.collections.current).unwrap().name, "Untitled");
        assert!(!studio.join("scenes.json").exists(), "moved, not copied");
        assert_eq!(std::fs::read(lib.current_file(dir.path(), Kind::Collections)).unwrap(), b"{\"sources\":[]}");
        assert!(lib.current_file(dir.path(), Kind::Profiles).exists());
        // Read back the same next time.
        assert_eq!(load(dir.path()), lib);
    }

    #[test]
    fn ids_and_names_stay_unique() {
        let mut s = Shelf::first();
        assert_eq!(s.new_id("My Show!"), "my-show");
        assert_eq!(s.new_id("Untitled"), "untitled-2");
        assert_eq!(s.new_id("✨✨"), "set");
        assert_eq!(s.new_id("Current"), "current-2", "route words aren't ids");
        assert_eq!(s.new_id("import"), "import-2");
        assert_eq!(s.free_name("untitled"), "untitled 2");
        s.list.push(Entry { id: "show".into(), name: "Show".into() });
        assert_eq!(s.free_name("Show"), "Show 2");
        assert_eq!(s.free_name("Other"), "Other");
        assert!(check_name("  ").is_err());
        assert!(check_name("a\nb").is_err());
        assert_eq!(check_name(" Gaming ").unwrap(), "Gaming");
    }

    #[test]
    fn a_broken_index_starts_again_without_losing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("studio")).unwrap();
        std::fs::write(index(dir.path()), b"{oops").unwrap();
        let lib = load(dir.path());
        assert_eq!(lib.collections.list.len(), 1);
        assert_eq!(lib.profiles.current, "untitled");
    }
}
