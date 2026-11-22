//! Font discovery, Google Fonts downloads, and Ferrocast-local caching (`<data>/fonts/`).
//!
//! Fonts extracted from `.overlay` packs or fetched from Google Fonts are stored only inside
//! `<data>/fonts/` so OS-wide font folders are never touched. Missing or offline fonts degrade
//! gracefully with an offline-friendly message instead of aborting pack imports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ferrocast_core::layer::is_font_family;
use ferrocast_store::durable;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_GOOGLE_FONTS_API: &str = "https://fonts.googleapis.com";
const GOOGLE_FONTS_CSS_HOST: &str = "fonts.googleapis.com";
const GOOGLE_FONTS_CDN_HOST: &str = "fonts.gstatic.com";
const MAX_FONT_FILE_BYTES: usize = 10 * 1024 * 1024;
/// A font stylesheet is a few hundred bytes per face; this is plenty.
const MAX_FONT_CSS_BYTES: usize = 256 * 1024;
const MAX_FILES_PER_FAMILY: usize = 2;
const MAX_FAMILIES_PER_PACK: usize = 20;

/// Status of a font referenced by an imported pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontStatus {
    /// Already installed on the operating system.
    Installed,
    /// Bundled inside the imported `.overlay` zip and saved to `<data>/fonts/`.
    Bundled,
    /// Already cached locally inside Ferrocast's own `<data>/fonts/` folder.
    Cached,
    /// Newly downloaded from Google Fonts into `<data>/fonts/`.
    Downloaded,
    /// Not installed or cached yet; preview shows it will be fetched from Google Fonts on import.
    Missing,
    /// Couldn't find this family on Google Fonts (custom/commercial font).
    NotFound,
    /// Couldn't reach Google Fonts (offline/timeout); Studio will use a fallback font.
    OfflineFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontSpec {
    pub family: String,
    pub style: String,
    pub raw_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontPreviewEntry {
    pub family: String,
    pub style: String,
    pub status: FontStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontResultEntry {
    pub family: String,
    pub style: String,
    pub status: FontStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFontEntry {
    pub family: String,
    pub files: Vec<String>,
    pub source: String,
    pub license: String,
}

pub type FontIndex = BTreeMap<String, CachedFontEntry>;

/// One validated font binary bundled directly inside a `.overlay` zip archive.
#[derive(Debug, Clone)]
pub struct BundledFontFile {
    pub archive_path: String,
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub declared_families: Vec<String>,
}

/// Environment lookup hook so tests can simulate which system fonts exist deterministically.
pub type SystemFontsFn = Arc<dyn Fn() -> BTreeSet<String> + Send + Sync>;

pub fn fonts_dir(data: &Path) -> PathBuf {
    data.join("fonts")
}

fn index_path(data: &Path) -> PathBuf {
    fonts_dir(data).join("index.json")
}

pub fn load_index(data: &Path) -> FontIndex {
    durable::load_json_with_backup::<FontIndex>(&index_path(data), |_| true).map(|l| l.value).unwrap_or_default()
}

fn save_index(data: &Path, idx: &FontIndex) -> Result<(), String> {
    let dir = fonts_dir(data);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create Ferrocast's font folder: {e}"))?;
    durable::save_json_pretty(&index_path(data), idx)
        .map_err(|e| format!("Couldn't save Ferrocast's font cache index: {e}"))
}

/// Sniffs font magic bytes and validates TTF/OTF/TTC files with `fontdb`.
pub fn detect_font_format(bytes: &[u8]) -> Option<(&'static str, Vec<String>)> {
    if bytes.len() < 12 {
        return None;
    }
    let header = &bytes[..4];
    let ext = match header {
        [0x00, 0x01, 0x00, 0x00] | b"true" => "ttf",
        b"OTTO" => "otf",
        b"ttcf" => "ttc",
        b"wOFF" => "woff",
        b"wOF2" => "woff2",
        _ => return None,
    };
    if matches!(ext, "ttf" | "otf" | "ttc") {
        let families = ferrocore::fonts::font_families_from_bytes(bytes)?;
        return Some((ext, families));
    }
    Some((ext, Vec::new()))
}

pub fn is_generic_font_name(name: &str) -> bool {
    let low = name.trim().to_ascii_lowercase();
    low.is_empty()
        || matches!(
            low.as_str(),
            "sans-serif"
                | "serif"
                | "monospace"
                | "cursive"
                | "fantasy"
                | "system-ui"
                | "ui-sans-serif"
                | "ui-serif"
                | "ui-monospace"
                | "inherit"
                | "initial"
                | "default"
                | "system"
                | "none"
        )
}

/// Normalizes raw OBS/Streamlabs `font.face` and `custom_font` values into `(family, style)`.
pub fn parse_font_spec(
    face: Option<&str>,
    custom_font: Option<&str>,
    flags: u64,
    explicit_style: Option<&str>,
) -> Option<FontSpec> {
    let mut style = explicit_style
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| style_from_flags(flags));

    let clean_custom = custom_font.and_then(|raw| {
        let fname = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
        let stem = fname
            .strip_suffix(".ttf")
            .or_else(|| fname.strip_suffix(".otf"))
            .or_else(|| fname.strip_suffix(".woff2"))
            .or_else(|| fname.strip_suffix(".woff"))
            .unwrap_or(fname)
            .trim();
        if stem.is_empty() { None } else { Some(stem.to_string()) }
    });

    let chosen =
        face.map(str::trim).filter(|s| !s.is_empty() && !is_generic_font_name(s)).map(str::to_string).or_else(
            || {
                clean_custom.as_deref().map(|stem| {
                    let (fam, inferred_style) = split_family_style_suffix(stem);
                    if style == "Regular" && !inferred_style.is_empty() {
                        style = inferred_style;
                    }
                    fam
                })
            },
        )?;

    let (normalized_family, suffix_style) = split_family_style_suffix(&chosen);
    if style == "Regular" && !suffix_style.is_empty() {
        style = suffix_style;
    }
    let trimmed = normalized_family.trim().to_string();
    if is_generic_font_name(&trimmed) || !is_font_family(&trimmed) {
        return None;
    }
    Some(FontSpec {
        family: trimmed,
        style,
        raw_ref: custom_font.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
    })
}

fn style_from_flags(flags: u64) -> String {
    let bold = (flags & 1) != 0;
    let italic = (flags & 2) != 0;
    match (bold, italic) {
        (true, true) => "Bold Italic".into(),
        (true, false) => "Bold".into(),
        (false, true) => "Italic".into(),
        (false, false) => "Regular".into(),
    }
}

fn split_family_style_suffix(input: &str) -> (String, String) {
    const SUFFIXES: &[(&str, &str)] = &[
        ("extra-bold-italic", "ExtraBold Italic"),
        ("extrabold-italic", "ExtraBold Italic"),
        ("semi-bold-italic", "SemiBold Italic"),
        ("semibold-italic", "SemiBold Italic"),
        ("bold-italic", "Bold Italic"),
        ("extra-bold", "ExtraBold"),
        ("extrabold", "ExtraBold"),
        ("semi-bold", "SemiBold"),
        ("semibold", "SemiBold"),
        ("extra-light", "ExtraLight"),
        ("extralight", "ExtraLight"),
        ("regular", "Regular"),
        ("medium", "Medium"),
        ("italic", "Italic"),
        ("black", "Black"),
        ("light", "Light"),
        ("thin", "Thin"),
        ("bold", "Bold"),
    ];
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    for &(sfx, label) in SUFFIXES {
        for sep in ["-", " "] {
            let pat = format!("{sep}{sfx}");
            if lower.ends_with(&pat) && trimmed.len() > pat.len() {
                let stem = trimmed[..trimmed.len() - pat.len()].trim_end_matches(['-', ' ']);
                if !stem.is_empty() {
                    return (stem.to_string(), label.to_string());
                }
            }
        }
    }
    (trimmed.to_string(), String::new())
}

fn cached_files_for_family(data: &Path, idx: &FontIndex, family: &str) -> Option<Vec<String>> {
    let key = family.to_ascii_lowercase();
    let entry = idx.get(&key)?;
    let dir = fonts_dir(data);
    let existing: Vec<String> = entry
        .files
        .iter()
        .filter(|rel| {
            let p = safe_cache_path(&dir, rel);
            p.is_some_and(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0))
        })
        .cloned()
        .collect();
    if existing.is_empty() { None } else { Some(existing) }
}

fn safe_cache_path(fonts_root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('/') || rel.contains('\\') || rel.contains("..") {
        return None;
    }
    Some(fonts_root.join(rel))
}

fn family_slug(family: &str) -> String {
    let mut s = String::new();
    let mut dash = false;
    for ch in family.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !s.is_empty() {
            s.push('-');
            dash = true;
        }
        if s.len() >= 48 {
            break;
        }
    }
    let s = s.trim_matches('-');
    if s.is_empty() { "font".into() } else { s.into() }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't prepare Ferrocast's font folder: {e}"))?;
    }
    let tmp = path.with_extension(format!("part.{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("Couldn't save font file: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Couldn't finish saving font file: {e}"));
    }
    Ok(())
}

fn url_encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

fn parse_origin(url: &str) -> Option<Origin> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme_low = scheme.to_ascii_lowercase();
    if scheme_low != "http" && scheme_low != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = &authority[..=end];
        let tail = &authority[end + 1..];
        let port = if let Some(p) = tail.strip_prefix(':') { Some(p.parse::<u16>().ok()?) } else { None };
        (host.to_ascii_lowercase(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        let port = p.parse::<u16>().ok()?;
        (h.to_ascii_lowercase(), Some(port))
    } else {
        (authority.to_ascii_lowercase(), None)
    };
    if host.is_empty() {
        return None;
    }
    Some(Origin { scheme: scheme_low, host, port })
}

fn url_allowed_for_google_fonts(url: &str, configured_base: &str, is_cdn: bool) -> bool {
    let Some(target) = parse_origin(url) else { return false };
    let default_base = configured_base == DEFAULT_GOOGLE_FONTS_API;
    if default_base {
        let expected_host = if is_cdn { GOOGLE_FONTS_CDN_HOST } else { GOOGLE_FONTS_CSS_HOST };
        return target.scheme == "https" && target.host == expected_host && target.port.is_none();
    }
    let Some(allowed_mock) = parse_origin(configured_base) else { return false };
    target == allowed_mock
}

/// Extracts `src: url(...)` URLs from a Google Fonts CSS2 response, preferring `.ttf`/`.otf`.
fn extract_css_font_urls(css: &str) -> Vec<String> {
    let mut ttf_or_otf = Vec::new();
    let mut other = Vec::new();
    let mut rest = css;
    while let Some(idx) = rest.find("url(") {
        let after = &rest[idx + 4..];
        let Some(close) = after.find(')') else { break };
        let candidate = after[..close].trim().trim_matches(['"', '\'']).trim();
        if !candidate.is_empty() {
            let low = candidate.to_ascii_lowercase();
            let list =
                if low.ends_with(".ttf") || low.ends_with(".otf") || low.contains(".ttf?") || low.contains(".otf?") {
                    &mut ttf_or_otf
                } else {
                    &mut other
                };
            if !list.iter().any(|u: &String| u == candidate) {
                list.push(candidate.to_string());
            }
        }
        rest = &after[close + 1..];
    }
    ttf_or_otf.extend(other);
    ttf_or_otf
}

/// The folder a pack's own fonts live in (`<data>/fonts/packs/<pack>/`, inside the folder the
/// text renderer loads, so they work while the pack is in use and go with it on Undo). They are
/// never written into the shared cache or its index.
pub(crate) fn pack_fonts_dir(data: &Path, pack_id: &str) -> PathBuf {
    fonts_dir(data).join("packs").join(pack_id)
}

/// The families a bundled font file stands for: what it declares, or the name its file suggests.
fn bundled_family_names(bf: &BundledFontFile) -> Vec<String> {
    if !bf.declared_families.is_empty() {
        return bf.declared_families.clone();
    }
    parse_font_spec(None, Some(&bf.archive_path), 0, None).map(|s| vec![s.family]).unwrap_or_default()
}

/// The pack's fonts that may be installed: files this program can load (a `.woff` or `.woff2`
/// can't be, and only its first bytes were looked at), for a family not already on this
/// computer or in the cache. A pack can't take over a font that is already there.
fn bundled_to_install<'a>(
    data: &Path,
    idx: &FontIndex,
    installed: &BTreeSet<String>,
    bundled: &'a [BundledFontFile],
) -> Vec<&'a BundledFontFile> {
    bundled
        .iter()
        .filter(|b| matches!(b.ext, "ttf" | "otf" | "ttc"))
        .filter(|b| {
            !bundled_family_names(b)
                .iter()
                .any(|f| installed_matches(installed, f) || cached_files_for_family(data, idx, f).is_some())
        })
        .collect()
}

fn bundled_matches_spec(spec: &FontSpec, bundled: &[&BundledFontFile]) -> bool {
    let fam_low = spec.family.to_ascii_lowercase();
    let raw_low = spec.raw_ref.as_deref().map(|r| r.rsplit(['/', '\\']).next().unwrap_or(r).to_ascii_lowercase());
    bundled.iter().any(|b| {
        b.declared_families.iter().any(|f| f.eq_ignore_ascii_case(&fam_low))
            || raw_low
                .as_ref()
                .is_some_and(|r| b.archive_path.rsplit('/').next().is_some_and(|fname| fname.eq_ignore_ascii_case(r)))
    })
}

fn installed_matches(installed: &BTreeSet<String>, family: &str) -> bool {
    installed.iter().any(|f| f.eq_ignore_ascii_case(family))
}

/// Computes preview status for every font used in the pack without writing files or making network calls.
pub fn preview_fonts(
    data: &Path,
    specs: &[FontSpec],
    bundled: &[BundledFontFile],
    installed: &BTreeSet<String>,
) -> Vec<FontPreviewEntry> {
    let idx = load_index(data);
    let local_families = ferrocore::fonts::dir_font_families(&fonts_dir(data));
    let kept = bundled_to_install(data, &idx, installed, bundled);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for spec in specs {
        let key = spec.family.to_ascii_lowercase();
        if !seen.insert(key) {
            continue;
        }
        if bundled_matches_spec(spec, &kept) {
            out.push(FontPreviewEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Bundled,
                cached_file: None,
            });
            continue;
        }
        if installed_matches(installed, &spec.family) {
            out.push(FontPreviewEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Installed,
                cached_file: None,
            });
            continue;
        }
        if let Some(files) = cached_files_for_family(data, &idx, &spec.family) {
            out.push(FontPreviewEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Cached,
                cached_file: files.into_iter().next(),
            });
            continue;
        }
        if local_families.iter().any(|f| f.eq_ignore_ascii_case(&spec.family)) {
            out.push(FontPreviewEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Cached,
                cached_file: None,
            });
            continue;
        }
        out.push(FontPreviewEntry {
            family: spec.family.clone(),
            style: spec.style.clone(),
            status: FontStatus::Missing,
            cached_file: None,
        });
    }
    out
}

/// Copies validated bundled fonts into `<data>/fonts/` and downloads any missing families solely
/// from Google Fonts into `<data>/fonts/` with local caching and offline-friendly fallbacks.
pub async fn resolve_and_install_fonts(
    data: &Path,
    pack_id: &str,
    specs: &[FontSpec],
    bundled: &[BundledFontFile],
    installed: &BTreeSet<String>,
    google_fonts_base: &str,
    fetch_missing: bool,
) -> (Vec<FontResultEntry>, bool) {
    let dir = fonts_dir(data);
    let mut idx = load_index(data);
    let mut index_dirty = false;

    // 1. Save the pack's own fonts in the pack's folder (not the shared cache or its index):
    // only loadable files, only for families not already here, and a failed write is not claimed.
    let pack_dir = pack_fonts_dir(data, pack_id);
    let mut bundled_saved_by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut written: Vec<&BundledFontFile> = Vec::new();
    for bf in bundled_to_install(data, &idx, installed, bundled) {
        let mut h = Sha256::new();
        h.update(&bf.bytes);
        let digest: String = h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect();
        let stem = bf.archive_path.rsplit('/').next().and_then(|n| n.split('.').next()).unwrap_or("pack-font");
        let file_name = format!("{}-{}.{}", family_slug(stem), digest, bf.ext);
        let target = pack_dir.join(&file_name);
        if let Err(e) = write_atomic(&target, &bf.bytes) {
            tracing::warn!(error = %e, font = %bf.archive_path, "couldn't save a pack's font");
            continue;
        }
        for fam in bundled_family_names(bf) {
            bundled_saved_by_key.entry(fam.to_ascii_lowercase()).or_default().push(file_name.clone());
        }
        written.push(bf);
    }
    let bundled = &written;

    let local_families = ferrocore::fonts::dir_font_families(&dir);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Ferrocast/1.0")
        .build()
        .ok();

    let base = google_fonts_base.trim_end_matches('/');
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut any_offline = false;

    for spec in specs.iter().take(MAX_FAMILIES_PER_PACK) {
        let key = spec.family.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(files) = bundled_saved_by_key.get(&key) {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Bundled,
                license: Some("Bundled with pack".into()),
                file: files.first().cloned(),
                message: Some("Saved from theme pack to Ferrocast's font folder.".into()),
            });
            continue;
        }
        if bundled_matches_spec(spec, bundled) {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Bundled,
                license: Some("Bundled with pack".into()),
                file: None,
                message: Some("Saved from theme pack to Ferrocast's font folder.".into()),
            });
            continue;
        }
        if installed_matches(installed, &spec.family) {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Installed,
                license: None,
                file: None,
                message: Some("Already installed on this computer.".into()),
            });
            continue;
        }
        if let Some(files) = cached_files_for_family(data, &idx, &spec.family) {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Cached,
                license: idx.get(&key).map(|e| e.license.clone()),
                file: files.first().cloned(),
                message: Some("Using Ferrocast's local font cache.".into()),
            });
            continue;
        }
        if local_families.iter().any(|f| f.eq_ignore_ascii_case(&spec.family)) {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Cached,
                license: None,
                file: None,
                message: Some("Using Ferrocast's local font folder.".into()),
            });
            continue;
        }
        if !fetch_missing {
            out.push(FontResultEntry {
                family: spec.family.clone(),
                style: spec.style.clone(),
                status: FontStatus::Missing,
                license: None,
                file: None,
                message: Some(format!("Skipped Google Fonts download for \"{}\"; using a fallback font.", spec.family)),
            });
            continue;
        }

        let Some(ref http) = client else {
            any_offline = true;
            out.push(offline_entry(spec));
            continue;
        };

        match fetch_one_google_font(http, base, &dir, &spec.family).await {
            Ok(saved_files) => {
                let first = saved_files.first().cloned();
                idx.insert(
                    key,
                    CachedFontEntry {
                        family: spec.family.clone(),
                        files: saved_files,
                        source: "google_fonts".into(),
                        license: "Open license (Google Fonts)".into(),
                    },
                );
                index_dirty = true;
                out.push(FontResultEntry {
                    family: spec.family.clone(),
                    style: spec.style.clone(),
                    status: FontStatus::Downloaded,
                    license: Some("Open license (Google Fonts)".into()),
                    file: first,
                    message: Some("Downloaded from Google Fonts into Ferrocast's font folder.".into()),
                });
            }
            Err(FetchError::NotFound) => {
                out.push(FontResultEntry {
                    family: spec.family.clone(),
                    style: spec.style.clone(),
                    status: FontStatus::NotFound,
                    license: None,
                    file: None,
                    message: Some(format!(
                        "\"{}\" isn't available on Google Fonts. Studio will use a fallback system font.",
                        spec.family
                    )),
                });
            }
            Err(FetchError::Offline(_reason)) => {
                any_offline = true;
                out.push(offline_entry(spec));
            }
        }
    }

    if index_dirty && let Err(e) = save_index(data, &idx) {
        tracing::warn!(error = %e, "couldn't persist font cache index");
    }
    (out, any_offline)
}

fn offline_entry(spec: &FontSpec) -> FontResultEntry {
    FontResultEntry {
        family: spec.family.clone(),
        style: spec.style.clone(),
        status: FontStatus::OfflineFallback,
        license: None,
        file: None,
        message: Some(format!(
            "Couldn't reach Google Fonts (you may be offline). Studio will use a fallback system font for \"{}\" until you're back online.",
            spec.family
        )),
    }
}

enum FetchError {
    NotFound,
    Offline(String),
}

/// Reads a response body as it arrives and gives up the moment it passes `cap` bytes (a
/// `Content-Length` can be missing or wrong, so it can't be trusted to bound anything).
/// `Ok(None)`: too big. `Err`: the connection failed.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Option<Vec<u8>>, String> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Ok(None);
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len() + chunk.len() > cap {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Some(buf))
}

async fn fetch_one_google_font(
    http: &reqwest::Client,
    base: &str,
    fonts_dir: &Path,
    family: &str,
) -> Result<Vec<String>, FetchError> {
    let encoded = url_encode_query_value(family);
    let css_url = format!("{base}/css2?family={encoded}:wght@400;700&display=swap");
    if !url_allowed_for_google_fonts(&css_url, base, false) {
        return Err(FetchError::Offline("Blocked untrusted font catalog host.".into()));
    }
    let mut resp = http.get(&css_url).send().await.map_err(|e| FetchError::Offline(e.to_string()))?;
    // A family with no bold (Bebas Neue, Anton, Bangers) answers 400 to `:wght@400;700`: ask
    // once more for whatever weights it has.
    if resp.status() == reqwest::StatusCode::BAD_REQUEST {
        let plain_url = format!("{base}/css2?family={encoded}&display=swap");
        if !url_allowed_for_google_fonts(&plain_url, base, false) {
            return Err(FetchError::NotFound);
        }
        resp = http.get(&plain_url).send().await.map_err(|e| FetchError::Offline(e.to_string()))?;
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND || resp.status() == reqwest::StatusCode::BAD_REQUEST {
        return Err(FetchError::NotFound);
    }
    if !resp.status().is_success() {
        return Err(FetchError::Offline(format!("Google Fonts replied HTTP {}", resp.status())));
    }
    let css_bytes = match read_capped(resp, MAX_FONT_CSS_BYTES).await {
        Ok(Some(b)) => b,
        // A stylesheet this big isn't one: treat the family as not found.
        Ok(None) => return Err(FetchError::NotFound),
        Err(e) => return Err(FetchError::Offline(e)),
    };
    let css = String::from_utf8_lossy(&css_bytes).into_owned();
    let urls = extract_css_font_urls(&css);
    if urls.is_empty() {
        return Err(FetchError::NotFound);
    }

    let mut saved = Vec::new();
    for asset_url in urls.into_iter().take(MAX_FILES_PER_FAMILY) {
        if !url_allowed_for_google_fonts(&asset_url, base, true) {
            tracing::warn!(url = %asset_url, "blocked non-Google-Fonts binary URL in font stylesheet");
            continue;
        }
        let Ok(file_resp) = http.get(&asset_url).send().await else {
            if saved.is_empty() {
                return Err(FetchError::Offline("Failed to download font file.".into()));
            }
            continue;
        };
        if !file_resp.status().is_success() {
            continue;
        }
        let Ok(Some(bytes)) = read_capped(file_resp, MAX_FONT_FILE_BYTES).await else { continue };
        let Some((ext, _families)) = detect_font_format(&bytes) else {
            tracing::warn!(url = %asset_url, "rejected downloaded font with invalid header");
            continue;
        };
        let mut h = Sha256::new();
        h.update(&bytes);
        let short_hash: String = h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect();
        let filename = format!("{}-{short_hash}.{ext}", family_slug(family));
        let dest = fonts_dir.join(&filename);
        if write_atomic(&dest, &bytes).is_ok() {
            saved.push(filename);
        }
    }

    if saved.is_empty() { Err(FetchError::NotFound) } else { Ok(saved) }
}
