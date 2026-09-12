//! `.ferrodesign`: one design (with the alert design a theme uses) and its media, as a file
//! to share (API.md §4 Designs).
//!
//! A zip of `manifest.json`, `designs/<n>.json` and `media/<sha256>.<ext>`. Nothing else:
//! no scripts, no web pages, no settings, no tokens, nothing about viewers. Importing checks
//! every entry name, every file's real type and digest, and every design, and always adds
//! new designs (it never replaces one).

use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::path::Path;

use ferrocast_core::design::{Design, DesignContent, DesignError, Origin, OriginKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

use crate::error::StoreError;
use crate::{Store, new_id, now_ms, repo};

pub const FORMAT: &str = "ferrodesign";
pub const VERSION: u32 = 1;
/// Largest `.ferrodesign` the dashboard may upload.
pub const MAX_FILE: u64 = 100 * 1024 * 1024;
const MAX_ENTRIES: usize = 300;
const MAX_JSON: u64 = 2 * 1024 * 1024;
const MAX_DESIGNS: usize = 10;
const MEDIA_EXTS: [&str; 10] = ["png", "jpg", "jpeg", "gif", "webp", "mp3", "ogg", "wav", "webm", "mp4"];
const RISKY_EXTS: [&str; 16] =
    ["js", "mjs", "cjs", "html", "htm", "xhtml", "svg", "svgz", "php", "exe", "bat", "cmd", "sh", "ps1", "wasm", "jar"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub sha256: String,
    pub bytes: u64,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub app_version: String,
    pub created_at: i64,
    pub name: String,
    pub kind: String,
    pub designs: usize,
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
}

/// Why a design file was refused; each maps to its own HTTP status (API.md §4 Designs).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DesignFileError {
    /// Not a zip, or no Ferrocast manifest (415 `bad_design_file`).
    #[error("That file isn't a .ferrodesign.")]
    NotADesignFile,
    /// A script, web page or unexpected file inside (415 `unsafe_design_file`).
    #[error(
        "The design file has a file Ferrocast never imports: {0}. Design files only carry designs, pictures, sounds and videos."
    )]
    Unsafe(String),
    /// Damaged or tampered contents (415 `damaged_design_file`).
    #[error("The design file is damaged: {0}")]
    Damaged(String),
    /// A media file that isn't a picture, sound or video Ferrocast plays (415).
    #[error("{0}")]
    MediaType(String),
    /// Made by a newer Ferrocast (422 `design_too_new`).
    #[error("This design file was made by a newer Ferrocast (format {0}). Update Ferrocast, then import it.")]
    TooNew(u32),
    /// A design inside fails a check; the message names the file and field (422 `invalid_design`).
    #[error("{0}")]
    Invalid(String),
    /// Over a size limit (413 `too_large`).
    #[error("{0}")]
    TooLarge(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    File(#[from] DesignFileError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Store(StoreError::Io(e))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Store(StoreError::Json(e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImportReport {
    pub designs: Vec<Design>,
    pub media_files: usize,
    pub notes: Vec<String>,
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "video/webm" => "webm",
        _ => "mp4",
    }
}

fn zerr(e: impl std::fmt::Display) -> StoreError {
    StoreError::Io(std::io::Error::other(format!("the design file couldn't be written: {e}")))
}

/// Writes `design` (plus the alert design its theme uses, when given) and every library file
/// they use to `out`. Built-ins are refused by the caller; this writes the designs unbranded.
pub fn export(
    store: &Store,
    design: &Design,
    companion: Option<&Design>,
    out: &Path,
    app_version: &str,
) -> Result<Manifest> {
    let mut designs: Vec<Design> = vec![design.clone()];
    if let Some(c) = companion {
        designs.push(c.clone());
    }
    // Library ids become the file's digest, which is a valid media id on the way back in.
    let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut paths: BTreeMap<String, (String, std::path::PathBuf)> = BTreeMap::new();
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    for d in &designs {
        for id in d.media_ids() {
            if remap.contains_key(&id) {
                continue;
            }
            let Some((media, path)) = repo::media::get(store, &id)? else { continue };
            let Some(checked) = repo::media::check_file(&path)? else { continue };
            let name = format!("media/{}.{}", checked.sha256, ext_for(checked.mime));
            files.insert(
                name.clone(),
                FileEntry { sha256: checked.sha256.clone(), bytes: checked.bytes, mime: checked.mime.into() },
            );
            paths.insert(name, (media.name, path));
            remap.insert(id, checked.sha256);
        }
    }
    let ids: BTreeMap<String, String> =
        designs.iter().enumerate().map(|(i, d)| (d.id.clone(), format!("d{}", i + 1))).collect();
    for d in &mut designs {
        d.remap_media(&remap);
        d.id = ids.get(&d.id).cloned().unwrap_or_else(|| d.id.clone());
        d.built_in = false;
        d.branded = false;
        d.pro = false;
        if let DesignContent::Theme(t) = &mut d.content {
            t.alert_design = t.alert_design.as_ref().and_then(|a| ids.get(a).cloned());
        }
    }
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        app_version: app_version.into(),
        created_at: now_ms(),
        name: design.name.clone(),
        kind: design.kind().key().into(),
        designs: designs.len(),
        files,
    };
    let tmp = out.with_extension("ferrodesign.partial");
    let file = std::fs::File::create(&tmp)?;
    let mut zip = zip::ZipWriter::new(file);
    let json = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("manifest.json", json).map_err(zerr)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    for (i, d) in designs.iter().enumerate() {
        zip.start_file(format!("designs/{}.json", i + 1), json).map_err(zerr)?;
        zip.write_all(serde_json::to_string_pretty(d)?.as_bytes())?;
    }
    for (name, (_, path)) in &paths {
        zip.start_file(name.as_str(), stored).map_err(zerr)?;
        zip.write_all(&std::fs::read(path)?)?;
    }
    zip.finish().map_err(zerr)?.sync_all()?;
    crate::backup::rename_patiently(&tmp, out)?;
    Ok(manifest)
}

/// What an entry name may be. Anything else stops the import.
enum Entry {
    Manifest,
    Design(String),
    Media { sha256: String },
    Skip,
}

fn classify(name: &str) -> std::result::Result<Entry, DesignFileError> {
    if name.ends_with('/') {
        return Ok(Entry::Skip);
    }
    if name == "manifest.json" {
        return Ok(Entry::Manifest);
    }
    if matches!(name, "preview.png" | "preview.webp" | "preview.jpg") {
        return Ok(Entry::Skip);
    }
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let shown: String = name.chars().take(80).collect();
    if RISKY_EXTS.contains(&ext) {
        return Err(DesignFileError::Unsafe(format!("\"{shown}\"")));
    }
    if let Some(n) = name.strip_prefix("designs/").and_then(|r| r.strip_suffix(".json"))
        && !n.is_empty()
        && n.len() <= 8
        && n.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Ok(Entry::Design(name.to_string()));
    }
    if let Some(file) = name.strip_prefix("media/")
        && let Some((stem, e)) = file.split_once('.')
        && stem.len() == 64
        && stem.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && MEDIA_EXTS.contains(&e)
    {
        return Ok(Entry::Media { sha256: stem.to_string() });
    }
    Err(DesignFileError::Unsafe(format!("\"{shown}\"")))
}

fn damaged(e: impl std::fmt::Display) -> Error {
    DesignFileError::Damaged(e.to_string()).into()
}

fn read_json<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut f = zip.by_name(name).map_err(damaged)?;
    if f.size() > MAX_JSON {
        return Err(DesignFileError::TooLarge(format!("\"{name}\" in the design file is over 2 MB.")).into());
    }
    let mut buf = Vec::new();
    f.by_ref().take(MAX_JSON + 1).read_to_end(&mut buf).map_err(damaged)?;
    if buf.len() as u64 > MAX_JSON {
        return Err(DesignFileError::TooLarge(format!("\"{name}\" in the design file is over 2 MB.")).into());
    }
    Ok(buf)
}

/// Reads and checks a design file, then adds its designs (and media) as the streamer's own.
pub fn import(store: &Store, path: &Path) -> Result<ImportReport> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_FILE {
        return Err(too_big_file().into());
    }
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| DesignFileError::NotADesignFile)?;
    if zip.len() > MAX_ENTRIES {
        return Err(DesignFileError::TooLarge(format!("The design file has more than {MAX_ENTRIES} files.")).into());
    }
    let mut design_names = Vec::new();
    let mut media_names: Vec<(String, String)> = Vec::new();
    let mut has_manifest = false;
    let mut unpacked: u64 = 0;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(damaged)?;
        let name = f.name().map_err(damaged)?.to_string();
        unpacked = unpacked.saturating_add(f.size());
        match classify(&name)? {
            Entry::Manifest => has_manifest = true,
            Entry::Design(n) => design_names.push(n),
            Entry::Media { sha256 } => media_names.push((name, sha256)),
            Entry::Skip => {}
        }
    }
    if !has_manifest {
        return Err(DesignFileError::NotADesignFile.into());
    }
    if unpacked > 4 * MAX_FILE {
        return Err(too_big_file().into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_json(&mut zip, "manifest.json")?).map_err(|_| DesignFileError::NotADesignFile)?;
    if manifest.format != FORMAT {
        return Err(DesignFileError::NotADesignFile.into());
    }
    if manifest.version > VERSION {
        return Err(DesignFileError::TooNew(manifest.version).into());
    }
    if design_names.is_empty() {
        return Err(DesignFileError::Invalid("The design file has no designs in it.".into()).into());
    }
    if design_names.len() > MAX_DESIGNS {
        return Err(DesignFileError::TooLarge(format!("A design file can hold up to {MAX_DESIGNS} designs.")).into());
    }
    design_names.sort();
    let mut designs: Vec<Design> = Vec::new();
    for name in &design_names {
        let bytes = read_json(&mut zip, name)?;
        let d: Design = serde_json::from_slice(&bytes).map_err(|e| {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(s) = v.get("schema").and_then(serde_json::Value::as_u64)
                && s > u64::from(ferrocast_core::design::SCHEMA)
            {
                return Error::from(DesignFileError::TooNew(u32::try_from(s).unwrap_or(u32::MAX)));
            }
            DesignFileError::Invalid(format!("{name} can't be read: {e}.")).into()
        })?;
        let mut check = d.clone();
        check.built_in = false;
        check.branded = false;
        check.validate().map_err(|e| match e {
            DesignError::TooNew(v) => Error::from(DesignFileError::TooNew(v)),
            DesignError::Invalid(m) => DesignFileError::Invalid(format!("{name}: {m}")).into(),
        })?;
        if designs.iter().any(|d| d.id == check.id) {
            return Err(DesignFileError::Invalid(format!(
                "{name}: two designs in the file share the id \"{}\". Export it again from Ferrocast.",
                check.id.chars().take(40).collect::<String>()
            ))
            .into());
        }
        designs.push(check);
    }
    // Every library file a design uses must be in the file, under its digest.
    let present: std::collections::HashSet<&str> = media_names.iter().map(|(_, s)| s.as_str()).collect();
    for (d, name) in designs.iter().zip(&design_names) {
        if let Some(missing) = d.media_ids().into_iter().find(|id| !present.contains(id.as_str())) {
            let shown: String = missing.chars().take(16).collect();
            return Err(DesignFileError::Invalid(format!(
                "{name} uses a file that isn't in the design file ({shown}…). Export it again from Ferrocast."
            ))
            .into());
        }
    }
    // Unpack and check every media file before anything is saved.
    let staging = store.dir().join("tmp").join(format!("design-{}", new_id()));
    std::fs::create_dir_all(&staging)?;
    let result = stage_and_save(store, &mut zip, &staging, &manifest, designs, &media_names);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn too_big_file() -> DesignFileError {
    DesignFileError::TooLarge(format!("Design files can be up to {} MB.", MAX_FILE / 1024 / 1024))
}

fn stage_and_save<R: Read + Seek>(
    store: &Store,
    zip: &mut zip::ZipArchive<R>,
    staging: &Path,
    manifest: &Manifest,
    designs: Vec<Design>,
    media_names: &[(String, String)],
) -> Result<ImportReport> {
    let mut staged: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (name, sha) in media_names {
        let mut f = zip.by_name(name).map_err(damaged)?;
        let limit = repo::media::MediaKind::Video.max_bytes();
        if f.size() > limit {
            return Err(DesignFileError::TooLarge(format!("\"{name}\" is over 50 MB, the most for one file.")).into());
        }
        let out_path = staging.join(sha);
        let mut out = std::fs::File::create(&out_path)?;
        let n = std::io::copy(&mut f.by_ref().take(limit + 1), &mut out).map_err(damaged)?;
        if n > limit {
            return Err(DesignFileError::TooLarge(format!("\"{name}\" is over 50 MB, the most for one file.")).into());
        }
        drop(out);
        let data = std::fs::read(&out_path)?;
        let digest: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        if &digest != sha {
            return Err(DesignFileError::Damaged(format!("\"{name}\" doesn't match its checksum.")).into());
        }
        if let Some(entry) = manifest.files.get(name)
            && entry.sha256 != *sha
        {
            return Err(DesignFileError::Damaged(format!("the manifest's checksum for \"{name}\" is wrong.")).into());
        }
        match repo::media::check_file(&out_path)? {
            Some(_) => staged.push((sha.clone(), out_path)),
            None => {
                return Err(DesignFileError::MediaType(match repo::media::sniff(&data) {
                    Some((kind, _)) => format!("\"{name}\" is too big. {}", kind.limit_label()),
                    None => format!(
                        "\"{name}\" isn't a picture, sound or video Ferrocast can play (PNG, JPEG, GIF, WebP, MP3, OGG, WAV, WebM or MP4)."
                    ),
                })
                .into());
            }
        }
    }
    // Save the media (reusing a file the library already has), then every design at once.
    let used: std::collections::HashSet<String> = designs.iter().flat_map(Design::media_ids).collect();
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut media_files = 0;
    let mut notes = Vec::new();
    for (sha, path) in staged {
        if !used.contains(&sha) {
            continue;
        }
        if let Some(existing) = repo::media::find_by_sha256(store, &sha)? {
            remap.insert(sha, existing);
            continue;
        }
        let data = std::fs::read(&path)?;
        let name = format!("{} file", manifest.name.chars().take(60).collect::<String>());
        let m = repo::media::save(store, &name, &data).map_err(|e| match e {
            repo::media::SaveError::Store(s) => Error::Store(s),
            repo::media::SaveError::Upload(u) => DesignFileError::MediaType(u.to_string()).into(),
        })?;
        remap.insert(sha, m.id);
        media_files += 1;
    }
    let now = now_ms();
    let from = Some(manifest.name.chars().take(120).collect::<String>());
    let new_ids: BTreeMap<String, String> = designs.iter().map(|d| (d.id.clone(), new_id())).collect();
    let mut out = Vec::new();
    for d in designs {
        let id = new_ids.get(&d.id).cloned().unwrap_or_else(new_id);
        let mut mine = d.into_user(id, Origin { kind: OriginKind::Import, from: from.clone() }, now);
        mine.remap_media(&remap);
        if let DesignContent::Theme(t) = &mut mine.content
            && let Some(a) = &t.alert_design
        {
            match new_ids.get(a) {
                Some(new) => t.alert_design = Some(new.clone()),
                None if a.starts_with("builtin.") => {}
                None => {
                    notes.push(format!("{}: its alert design wasn't in the file, so it uses your own.", mine.name));
                    t.alert_design = None;
                }
            }
        }
        out.push(mine);
    }
    for d in &out {
        repo::designs::insert(store, d)?;
    }
    if manifest.files.is_empty() && media_files == 0 {
        notes.push("No pictures or sounds came with it: it uses the art built into Ferrocast.".into());
    }
    Ok(ImportReport { designs: out, media_files, notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_id;

    fn fixture() -> Design {
        serde_json::from_str(include_str!("../../core/fixtures/designs/alert-design.json")).unwrap()
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRxxxxxxxx";

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    fn with_picture(s: &Store) -> Design {
        let m = repo::media::save(s, "coin.png", PNG).unwrap();
        let mut d = fixture();
        d.id = new_id();
        if let DesignContent::Alert(a) = &mut d.content {
            let l = &mut a.events.values_mut().next().unwrap().variations[0].look;
            l.media = ferrocast_core::design::MediaSlot {
                kind: ferrocast_core::design::MediaKind::Image,
                media_ref: Some(m.id.clone()),
            };
        }
        repo::designs::insert(s, &d).unwrap()
    }

    #[test]
    fn round_trip_adds_a_new_unbranded_design_with_its_media() {
        let (dir, s) = store();
        let d = with_picture(&s);
        let out = dir.path().join("x.ferrodesign");
        let m = export(&s, &d, None, &out, "0.1.0").unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.designs, 1);
        // Into another install.
        let (_d2, s2) = store();
        let report = import(&s2, &out).unwrap();
        assert_eq!(report.designs.len(), 1);
        assert_eq!(report.media_files, 1);
        let got = &report.designs[0];
        assert_ne!(got.id, d.id);
        assert!(!got.built_in && !got.branded);
        assert_eq!(got.origin.kind, OriginKind::Import);
        assert_eq!(got.name, d.name);
        let media = got.media_ids();
        let (saved, path) = repo::media::get(&s2, &media[0]).unwrap().unwrap();
        assert_eq!(saved.mime, "image/png");
        assert_eq!(std::fs::read(path).unwrap(), PNG);
        // Same content as before apart from ids and media refs.
        let mut a = d.clone();
        let mut b = got.clone();
        a.remap_media(&BTreeMap::new());
        b.remap_media(&BTreeMap::from([(media[0].clone(), d.media_ids()[0].clone())]));
        assert_eq!(a.content, b.content);
        // Importing twice adds a second copy and reuses the file.
        let again = import(&s2, &out).unwrap();
        assert_eq!(again.media_files, 0, "the library already has that picture");
        assert_eq!(repo::designs::list(&s2).unwrap().len(), 2);
    }

    #[test]
    fn a_theme_brings_its_alert_design_along() {
        let (dir, s) = store();
        let alert = with_picture(&s);
        let mut theme: Design =
            serde_json::from_str(include_str!("../../core/fixtures/designs/scene-design.json")).unwrap();
        let DesignContent::Scene(scene) = theme.content.clone() else { unreachable!() };
        theme.id = new_id();
        theme.content = DesignContent::Theme(ferrocast_core::design::ThemeContent {
            scenes: vec![scene],
            alert_design: Some(alert.id.clone()),
        });
        repo::designs::insert(&s, &theme).unwrap();
        let out = dir.path().join("t.ferrodesign");
        export(&s, &theme, Some(&alert), &out, "0.1.0").unwrap();
        let (_d2, s2) = store();
        let r = import(&s2, &out).unwrap();
        assert_eq!(r.designs.len(), 2);
        let t = r.designs.iter().find(|d| d.kind() == ferrocast_core::design::DesignKind::Theme).unwrap();
        let a = r.designs.iter().find(|d| d.kind() == ferrocast_core::design::DesignKind::Alert).unwrap();
        let DesignContent::Theme(tc) = &t.content else { unreachable!() };
        assert_eq!(tc.alert_design.as_deref(), Some(a.id.as_str()), "the link points at the imported copy");
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(f.reopen().unwrap());
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        f
    }

    fn manifest(version: u32) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "format": FORMAT, "version": version, "app_version": "x",
            "created_at": 0, "name": "Shared", "kind": "alert", "designs": 1, "files": {} }))
        .unwrap()
    }

    fn file_err(s: &Store, path: &Path) -> DesignFileError {
        match import(s, path) {
            Err(Error::File(e)) => e,
            other => panic!("expected a design file error, got {other:?}"),
        }
    }

    #[test]
    fn hostile_files_are_refused_with_the_right_reason_and_change_nothing() {
        let (_d, s) = store();
        let design = serde_json::to_vec(&fixture()).unwrap();
        let m = manifest(1);
        let not_zip = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(not_zip.path(), b"definitely not a zip").unwrap();
        assert_eq!(file_err(&s, not_zip.path()), DesignFileError::NotADesignFile);
        let no_manifest = zip_of(&[("designs/1.json", &design)]);
        assert_eq!(file_err(&s, no_manifest.path()), DesignFileError::NotADesignFile);
        let pack_manifest = serde_json::to_vec(&serde_json::json!({ "format": "ferropack", "version": 1,
            "app_version": "x", "created_at": 0, "name": "x", "kind": "alert", "designs": 1 }))
        .unwrap();
        let wrong = zip_of(&[("manifest.json", &pack_manifest), ("designs/1.json", &design)]);
        assert_eq!(file_err(&s, wrong.path()), DesignFileError::NotADesignFile);
        for evil in ["designs/run.js", "index.html", "media/x.svg", "../../escape.json", "notes.txt", "media/ABC.png"] {
            let f = zip_of(&[("manifest.json", &m), ("designs/1.json", &design), (evil, b"<script>")]);
            assert!(matches!(file_err(&s, f.path()), DesignFileError::Unsafe(_)), "{evil}");
        }
        let newer = zip_of(&[("manifest.json", &manifest(2)), ("designs/1.json", &design)]);
        assert_eq!(file_err(&s, newer.path()), DesignFileError::TooNew(2));
        let mut v: serde_json::Value = serde_json::from_slice(&design).unwrap();
        v["schema"] = serde_json::json!(5);
        let newer_design = zip_of(&[("manifest.json", &m), ("designs/1.json", &serde_json::to_vec(&v).unwrap())]);
        assert_eq!(file_err(&s, newer_design.path()), DesignFileError::TooNew(5));
        let mut bad = fixture();
        bad.name = String::new();
        let invalid = zip_of(&[("manifest.json", &m), ("designs/1.json", &serde_json::to_vec(&bad).unwrap())]);
        assert!(
            matches!(file_err(&s, invalid.path()), DesignFileError::Invalid(msg) if msg.starts_with("designs/1.json: name:"))
        );
        let junk = zip_of(&[("manifest.json", &m), ("designs/1.json", b"{not json")]);
        assert!(matches!(file_err(&s, junk.path()), DesignFileError::Invalid(msg) if msg.contains("can't be read")));
        let twice = zip_of(&[("manifest.json", &m), ("designs/1.json", &design), ("designs/2.json", &design)]);
        assert!(matches!(file_err(&s, twice.path()), DesignFileError::Invalid(msg) if msg.contains("share the id")));
        let empty = zip_of(&[("manifest.json", &m)]);
        assert!(matches!(file_err(&s, empty.path()), DesignFileError::Invalid(_)));
        // A file whose name claims a digest it doesn't have.
        let sha = "0".repeat(64);
        let lie = zip_of(&[("manifest.json", &m), ("designs/1.json", &design), (&format!("media/{sha}.png"), PNG)]);
        assert!(matches!(file_err(&s, lie.path()), DesignFileError::Damaged(_)));
        // A page dressed as a picture, under its true digest.
        let page = b"<html><script>alert(1)</script>";
        let real: String = Sha256::digest(page).iter().map(|b| format!("{b:02x}")).collect();
        let dressed =
            zip_of(&[("manifest.json", &m), ("designs/1.json", &design), (&format!("media/{real}.png"), page)]);
        assert!(matches!(file_err(&s, dressed.path()), DesignFileError::MediaType(_)));
        // A design pointing at a file the zip doesn't have.
        let mut needs = fixture();
        if let DesignContent::Alert(a) = &mut needs.content {
            a.events.values_mut().next().unwrap().variations[0].look.sound.media_ref = Some("f".repeat(64));
        }
        let missing = zip_of(&[("manifest.json", &m), ("designs/1.json", &serde_json::to_vec(&needs).unwrap())]);
        assert!(
            matches!(file_err(&s, missing.path()), DesignFileError::Invalid(msg) if msg.contains("isn't in the design file"))
        );
        // A forged "branded built-in" comes in as the streamer's own, unbranded.
        let mut forged = fixture();
        forged.built_in = true;
        forged.branded = true;
        forged.pro = true;
        let f = zip_of(&[("manifest.json", &m), ("designs/1.json", &serde_json::to_vec(&forged).unwrap())]);
        let r = import(&s, f.path()).unwrap();
        assert!(!r.designs[0].branded && !r.designs[0].built_in && !r.designs[0].pro);
        assert_eq!(repo::designs::list(&s).unwrap().len(), 1, "only the good file added anything");
        assert!(!s.dir().join("tmp").read_dir().unwrap().any(|e| e.unwrap().path().is_dir()), "staging is cleaned up");
    }

    #[test]
    fn oversized_files_get_a_size_error() {
        let (_d, s) = store();
        let big = tempfile::NamedTempFile::new().unwrap();
        big.as_file().set_len(MAX_FILE + 1).unwrap();
        assert!(matches!(file_err(&s, big.path()), DesignFileError::TooLarge(m) if m.contains("100 MB")));
        let huge_json = vec![b' '; (MAX_JSON + 10) as usize];
        let f = zip_of(&[("manifest.json", &manifest(1)), ("designs/1.json", &huge_json)]);
        assert!(matches!(file_err(&s, f.path()), DesignFileError::TooLarge(_)));
    }
}
