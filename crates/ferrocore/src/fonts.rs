//! Ferrocast's own font directory helper (`<data>/fonts/`) and OS/directory font scanning
//! backed by `cosmic-text`'s `fontdb`. Fonts loaded from `<data>/fonts/` never touch system-wide
//! OS font folders.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use cosmic_text::fontdb;

static EXTRA_FONTS_DIR: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<PathBuf>> {
    EXTRA_FONTS_DIR.get_or_init(|| RwLock::new(None))
}

/// Registers `<data>/fonts/` so every `TextRenderer` loads Ferrocast-local downloaded and bundled fonts.
pub fn set_extra_fonts_dir(dir: PathBuf) {
    *slot().write().unwrap_or_else(|p| p.into_inner()) = Some(dir);
}

/// Returns the configured Ferrocast-local font directory, if any.
pub fn extra_fonts_dir() -> Option<PathBuf> {
    slot().read().unwrap_or_else(|p| p.into_inner()).clone()
}

fn collect_families(db: &fontdb::Database) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for face in db.faces() {
        for (family, _) in &face.families {
            let trimmed = family.trim();
            if !trimmed.is_empty() {
                out.insert(trimmed.to_string());
            }
        }
    }
    out
}

/// Font family names currently installed on this computer.
pub fn installed_system_font_families() -> BTreeSet<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    collect_families(&db)
}

/// Font family names present in `dir` (non-existent directories return an empty set).
pub fn dir_font_families(dir: &Path) -> BTreeSet<String> {
    if !dir.is_dir() {
        return BTreeSet::new();
    }
    let mut db = fontdb::Database::new();
    db.load_fonts_dir(dir);
    collect_families(&db)
}

/// Checks whether raw TTF/OTF/TTC bytes parse as at least one valid font face and returns its
/// declared family names.
pub fn font_families_from_bytes(bytes: &[u8]) -> Option<Vec<String>> {
    let mut db = fontdb::Database::new();
    db.load_font_data(bytes.to_vec());
    let families: Vec<String> = collect_families(&db).into_iter().collect();
    if families.is_empty() { None } else { Some(families) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_font_families_handles_missing_and_invalid_bytes() {
        let missing = PathBuf::from("/nonexistent-ferrocast-fonts-test-dir");
        assert!(dir_font_families(&missing).is_empty());
        assert!(font_families_from_bytes(b"not a valid font file").is_none());
    }
}
