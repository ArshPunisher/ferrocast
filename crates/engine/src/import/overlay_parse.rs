//! Pure `.overlay` archive unpacking, validation, and Streamlabs/OWN3D/OBS scene-collection
//! conversion into Ferrocast Studio documents, widget overlay specs, media lists, and font refs.
//!
//! No filesystem writes or network requests happen here; callers get an audited [`ParsedPack`]
//! with safe archive paths, decompressed bytes bounded by size and count caps, and a clear report
//! of mapped widgets, missing assets, and unknown sources.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use ferrocast_core::layer::is_font_family;
use ferrocast_core::overlay::{MAX_WIDGETS, Widget, WidgetKind};
use ferrocast_store::repo::media;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zip::ZipArchive;

/// The kinds that are the same on every computer: transitions, pictures, video files, colours,
/// slideshows, scenes and groups. (Windows and Linux used to give an empty kind for these, so a
/// pack's transition came in with no kind at all.)
#[cfg_attr(target_os = "macos", allow(dead_code))] // the macOS table names these itself
fn portable_kind(kind: &str) -> &'static str {
    match kind {
        "fade_transition" => "fade_transition",
        "cut_transition" => "cut_transition",
        "swipe_transition" => "swipe_transition",
        "slide_transition" => "slide_transition",
        "fade_to_color_transition" => "fade_to_color_transition",
        "wipe_transition" => "wipe_transition",
        "stinger_transition" | "obs_stinger_transition" => "obs_stinger_transition",
        "image_source" => "image_source",
        "ffmpeg_source" => "ffmpeg_source",
        "color_source" | "color_source_v3" => "color_source_v3",
        "slideshow" => "slideshow",
        "scene" => "scene",
        "group" => "group",
        _ => "",
    }
}

#[cfg(test)]
pub(crate) fn portable_kind_for_tests(kind: &str) -> &'static str {
    portable_kind(kind)
}

/// Converts Streamlabs / cross-platform OBS source and filter kinds to native kinds on this OS.
fn for_this_computer(kind: &str) -> (&'static str, bool) {
    #[cfg(target_os = "macos")]
    return match kind {
        "dshow_input"
        | "v4l2_input"
        | "macos-avcapture"
        | "macos-avcapture-fast"
        | "av_capture_input"
        | "av_capture_input_v2" => ("macos-avcapture", true),
        "game_capture" | "display_capture" | "monitor_capture" | "window_capture" | "xshm_input"
        | "xcomposite_input" | "screen_capture" => ("screen_capture", true),
        "wasapi_input_capture" | "pulse_input_capture" | "coreaudio_input_capture" => ("coreaudio_input_capture", true),
        "wasapi_output_capture" | "pulse_output_capture" | "coreaudio_output_capture" => {
            ("coreaudio_output_capture", true)
        }
        "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" | "text_ft2_source" | "text_ft2_source_v2" => {
            ("text_ft2_source_v2", true)
        }
        "browser_source" | "ferrocast_web" => ("ferrocast_web", true),
        "fade_transition" => ("fade_transition", false),
        "cut_transition" => ("cut_transition", false),
        "swipe_transition" => ("swipe_transition", false),
        "slide_transition" => ("slide_transition", false),
        "fade_to_color_transition" => ("fade_to_color_transition", false),
        "wipe_transition" => ("wipe_transition", false),
        "stinger_transition" | "obs_stinger_transition" => ("obs_stinger_transition", false),
        "image_source" => ("image_source", false),
        "ffmpeg_source" => ("ffmpeg_source", false),
        "color_source" | "color_source_v3" => ("color_source_v3", false),
        "slideshow" => ("slideshow", false),
        "scene" => ("scene", false),
        "group" => ("group", false),
        _ => ("", false),
    };
    #[cfg(target_os = "windows")]
    return match kind {
        "macos-avcapture"
        | "macos-avcapture-fast"
        | "v4l2_input"
        | "av_capture_input"
        | "av_capture_input_v2"
        | "dshow_input" => ("dshow_input", true),
        "screen_capture" | "xshm_input" | "game_capture" => ("game_capture", true),
        "text_ft2_source" | "text_ft2_source_v2" | "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" => {
            ("text_gdiplus", true)
        }
        "browser_source" | "ferrocast_web" => ("browser_source", true),
        _ => (portable_kind(kind), false),
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return match kind {
        "macos-avcapture" | "macos-avcapture-fast" | "dshow_input" | "v4l2_input" => ("v4l2_input", true),
        "screen_capture" | "game_capture" | "xshm_input" => ("xshm_input", true),
        "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" | "text_ft2_source" | "text_ft2_source_v2" => {
            ("text_ft2_source_v2", true)
        }
        "browser_source" | "ferrocast_web" => ("browser_source", true),
        _ => (portable_kind(kind), false),
    };
}

fn known_native_kind(id: &str) -> bool {
    matches!(
        id,
        "scene"
            | "group"
            | "image_source"
            | "ffmpeg_source"
            | "vlc_source"
            | "slideshow"
            | "color_source"
            | "color_source_v3"
            | "text_ft2_source"
            | "text_ft2_source_v2"
            | "text_gdiplus"
            | "text_gdiplus_v2"
            | "text_gdiplus_v3"
            | "browser_source"
            | "ferrocast_web"
            | "macos-avcapture"
            | "macos-avcapture-fast"
            | "dshow_input"
            | "v4l2_input"
            | "screen_capture"
            | "game_capture"
            | "xshm_input"
            | "display_capture"
            | "window_capture"
            | "coreaudio_input_capture"
            | "coreaudio_output_capture"
            | "wasapi_input_capture"
            | "wasapi_output_capture"
            | "pulse_input_capture"
            | "pulse_output_capture"
            | "sck_audio_capture"
    )
}

use super::overlay_safe;
use crate::EngineError;
use crate::fonts::{BundledFontFile, FontSpec, detect_font_format, parse_font_spec};

/// Maximum `.overlay` upload size (150 MB).
pub const MAX_OVERLAY_ZIP: usize = 150 * 1024 * 1024;
/// Maximum HTTP multipart request body for `.overlay` preview uploads.
pub const MAX_OVERLAY_BODY: usize = MAX_OVERLAY_ZIP + 64 * 1024;
/// Maximum zip entry count permitted inside one `.overlay` pack.
pub const MAX_OVERLAY_FILES: usize = 500;
/// Maximum total decompressed byte count across all entries in one `.overlay` pack (300 MB).
pub const MAX_OVERLAY_UNPACKED: u64 = 300 * 1024 * 1024;

const MAX_JSON_ENTRY: usize = 10 * 1024 * 1024;
const MAX_IMAGE_ENTRY: usize = 15 * 1024 * 1024;
const MAX_AUDIO_ENTRY: usize = 10 * 1024 * 1024;
const MAX_FONT_ENTRY: usize = 15 * 1024 * 1024;
const MAX_VIDEO_ENTRY: usize = 60 * 1024 * 1024;
pub(super) const ASSET_PLACEHOLDER_PREFIX: &str = "__FERRO_PACK_ASSET__:";
pub(super) const OVERLAY_PLACEHOLDER_PREFIX: &str = "__FERRO_OVERLAY_URL__:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPackFormat {
    Streamlabs,
    Own3d,
    ObsBundle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayCanvas {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewScene {
    pub name: String,
    pub items: usize,
    pub mapped: usize,
    pub widgets: usize,
    pub unknown: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewWidget {
    pub scene: String,
    pub name: String,
    pub source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapped_to: Option<WidgetKind>,
    pub overlay_widget_name: String,
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownSource {
    pub scene: String,
    pub name: String,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Json,
    Image,
    Audio,
    Video,
    Font,
}

impl EntryKind {
    fn cap(self) -> usize {
        match self {
            EntryKind::Json => MAX_JSON_ENTRY,
            EntryKind::Image => MAX_IMAGE_ENTRY,
            EntryKind::Audio => MAX_AUDIO_ENTRY,
            EntryKind::Font => MAX_FONT_ENTRY,
            EntryKind::Video => MAX_VIDEO_ENTRY,
        }
    }

    pub fn is_media(self) -> bool {
        matches!(self, EntryKind::Image | EntryKind::Audio | EntryKind::Video)
    }
}

#[derive(Debug, Clone)]
pub struct UnpackedAsset {
    /// Forward-slash relative path normalized safely without leading slash or `..`.
    pub rel_path: String,
    pub kind: EntryKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PlannedSceneOverlay {
    pub placeholder_key: String,
    pub scene_name: String,
    pub widgets: Vec<Widget>,
}

#[derive(Debug, Clone)]
pub struct ParsedPack {
    pub suggested_name: String,
    pub format: OverlayPackFormat,
    pub canvas: OverlayCanvas,
    pub scenes: Vec<PreviewScene>,
    pub widgets: Vec<PreviewWidget>,
    pub scene_overlays: Vec<PlannedSceneOverlay>,
    pub unknown_sources: Vec<UnknownSource>,
    pub missing_assets: Vec<String>,
    pub assets: Vec<UnpackedAsset>,
    pub bundled_fonts: Vec<BundledFontFile>,
    pub font_specs: Vec<FontSpec>,
    pub notes: Vec<String>,
    /// OBS collection JSON template containing `ASSET_PLACEHOLDER_PREFIX` and
    /// `OVERLAY_PLACEHOLDER_PREFIX` tokens to resolve once disk paths and overlays exist.
    pub doc_template: Value,
    pub missing_kinds: Vec<String>,
    pub web_page_count: usize,
    pub source_count: usize,
}

fn refused(status: u16, code: &'static str, msg: impl Into<String>, hint: impl Into<String>) -> EngineError {
    EngineError::Refused { status, code, message: msg.into(), hint: hint.into(), retry_after: None }
}

/// How much of an entry may be read: its own cap, or what is left of the whole pack's limit,
/// whichever is less (so the last entry can't take the total past it).
pub(crate) fn entry_read_limit(entry_cap: usize, total_left: u64) -> u64 {
    (entry_cap as u64).min(total_left)
}

/// Compression this strong (unpacked / packed) on a file this big isn't a picture, a video or
/// a font: it is a zip bomb. Text packs 10:1 or so; media is near 1:1.
const MAX_RATIO: u64 = 100;
const RATIO_CHECK_FROM: u64 = 1024 * 1024;

fn classify_ext(path: &str) -> Option<(EntryKind, &'static str)> {
    let fname = path.rsplit('/').next().unwrap_or(path);
    let (_, raw_ext) = fname.rsplit_once('.')?;
    let ext = raw_ext.to_ascii_lowercase();
    match ext.as_str() {
        "json" => Some((EntryKind::Json, "json")),
        "png" => Some((EntryKind::Image, "png")),
        "jpg" | "jpeg" => Some((EntryKind::Image, "jpg")),
        "gif" => Some((EntryKind::Image, "gif")),
        "webp" => Some((EntryKind::Image, "webp")),
        "bmp" => Some((EntryKind::Image, "bmp")),
        "mp3" => Some((EntryKind::Audio, "mp3")),
        "ogg" => Some((EntryKind::Audio, "ogg")),
        "wav" => Some((EntryKind::Audio, "wav")),
        "flac" => Some((EntryKind::Audio, "flac")),
        "aac" => Some((EntryKind::Audio, "aac")),
        "m4a" => Some((EntryKind::Audio, "m4a")),
        "webm" => Some((EntryKind::Video, "webm")),
        "mp4" => Some((EntryKind::Video, "mp4")),
        "mov" => Some((EntryKind::Video, "mov")),
        "ttf" => Some((EntryKind::Font, "ttf")),
        "otf" => Some((EntryKind::Font, "otf")),
        "woff" => Some((EntryKind::Font, "woff")),
        "woff2" => Some((EntryKind::Font, "woff2")),
        _ => None,
    }
}

/// Validates and normalizes one zip entry path, blocking zip-slip traversal and OS control tricks.
pub fn sanitize_archive_path(raw: &str) -> Result<String, EngineError> {
    let bad = |msg: String| refused(415, "unsafe_overlay_file", msg, "Ask whoever made the pack to export it again.");
    if raw.is_empty() || raw.contains('\\') || raw.starts_with('/') || raw.chars().any(char::is_control) {
        return Err(bad(format!("The theme pack contains an unsafe path \"{raw}\".")));
    }
    // `:` is a drive on Windows (`C:`) and an alternate data stream after a name (`bg.png:evil`).
    if raw.contains(':') {
        return Err(bad(format!("The theme pack contains an unsafe path \"{raw}\".")));
    }
    let trimmed = raw.trim_end_matches('/');
    let mut clean_parts = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(bad(format!("The theme pack contains an unsafe path \"{raw}\".")));
        }
        // Windows drops a trailing dot or space (two names become one) and keeps device names
        // (`CON`, `NUL`, `COM1`) for the device, with or without an extension.
        let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
        // `COM1`..`COM9` and `LPT1`..`LPT9`, and Windows also reserves them with superscript digits.
        let last = stem.chars().nth(3);
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.chars().count() == 4
                && last.is_some_and(|c| matches!(c, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}')));
        if seg.ends_with('.') || seg.ends_with(' ') || seg.starts_with(' ') || device {
            return Err(bad(format!("The theme pack contains an unsafe path \"{raw}\".")));
        }
        clean_parts.push(seg);
    }
    if clean_parts.is_empty() {
        return Err(bad("The theme pack contains an empty path entry.".to_string()));
    }
    Ok(clean_parts.join("/"))
}

fn is_os_metadata_entry(clean_path: &str) -> bool {
    if clean_path == "__MACOSX" || clean_path.starts_with("__MACOSX/") {
        return true;
    }
    let fname = clean_path.rsplit('/').next().unwrap_or(clean_path);
    fname == ".DS_Store" || fname.eq_ignore_ascii_case("Thumbs.db") || fname.starts_with("._")
}

fn sniff_media_matches(kind: EntryKind, ext: &str, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if let Some((mk, sniffed)) = media::sniff(data) {
        return match (kind, mk) {
            (EntryKind::Image, media::MediaKind::Image) => {
                !(ext == "png" && sniffed != "png" && sniffed != "image/png")
            }
            (EntryKind::Audio, media::MediaKind::Sound) => true,
            (EntryKind::Video, media::MediaKind::Video) => true,
            _ => false,
        };
    }
    match ext {
        "bmp" => data.len() >= 2 && &data[..2] == b"BM",
        "mov" | "m4a" => data.len() >= 12 && &data[4..8] == b"ftyp",
        "flac" => data.len() >= 4 && &data[..4] == b"fLaC",
        "aac" => {
            (data.len() >= 4 && &data[..4] == b"ADIF")
                || (data.len() >= 2 && data[0] == 0xff && (data[1] & 0xf0) == 0xf0)
        }
        _ => false,
    }
}

struct ExtractedZip {
    json_files: Vec<(String, Value)>,
    assets: Vec<UnpackedAsset>,
    bundled_fonts: Vec<BundledFontFile>,
}

fn extract_zip(zip_bytes: &[u8]) -> Result<ExtractedZip, EngineError> {
    if zip_bytes.is_empty() {
        return Err(refused(415, "bad_overlay_file", "The file is empty.", "Pick a .overlay file."));
    }
    if zip_bytes.len() > MAX_OVERLAY_ZIP {
        return Err(refused(
            413,
            "too_large",
            format!("That theme pack is bigger than {} MB.", MAX_OVERLAY_ZIP / 1024 / 1024),
            "Trim large video files and try again.",
        ));
    }
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes)).map_err(|_| {
        refused(
            415,
            "bad_overlay_file",
            "That doesn't look like a valid .overlay zip archive.",
            "Download or export the .overlay file again.",
        )
    })?;
    if archive.len() > MAX_OVERLAY_FILES {
        return Err(refused(
            413,
            "too_large",
            format!("This theme pack has more than {MAX_OVERLAY_FILES} files inside."),
            "Trim unused assets and try again.",
        ));
    }

    let mut json_files = Vec::new();
    let mut assets = Vec::new();
    let mut bundled_fonts = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut total_unpacked: u64 = 0;

    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|_| {
            refused(
                415,
                "bad_overlay_file",
                "That .overlay archive is damaged.",
                "Download or export the .overlay file again.",
            )
        })?;
        let raw_name = entry
            .name()
            .map_err(|_| {
                refused(
                    415,
                    "unsafe_overlay_file",
                    "The theme pack contains an unreadable non-UTF8 path.",
                    "Ask whoever made the pack to export it cleanly.",
                )
            })?
            .to_string();
        // Only plain files and folders: not a link, a pipe, a device or a socket.
        if entry.unix_mode().is_some_and(|m| !matches!(m & 0o170000, 0 | 0o100000 | 0o040000)) {
            return Err(refused(
                415,
                "unsafe_overlay_file",
                format!("The theme pack contains a symbolic link \"{raw_name}\"."),
                "Ask whoever made the pack to export it cleanly.",
            ));
        }
        if raw_name.ends_with('/') {
            // Still verify parent path has no zip-slip segments.
            let _ = sanitize_archive_path(&raw_name)?;
            continue;
        }
        let clean_path = sanitize_archive_path(&raw_name)?;
        if is_os_metadata_entry(&clean_path) {
            continue;
        }
        // Case-folded beyond ASCII (`É`/`é` are one file on macOS and Windows), and a name can't
        // be both a file and a folder (`a.png` and `a.png/b.png`).
        let folded = clean_path.to_lowercase();
        let clash = !seen_paths.insert(folded.clone())
            || folded.match_indices('/').any(|(i, _)| seen_paths.contains(&folded[..i]))
            || seen_paths.iter().any(|p| p.len() > folded.len() && p.starts_with(&format!("{folded}/")));
        if clash {
            return Err(refused(
                415,
                "unsafe_overlay_file",
                format!("The theme pack contains duplicate entries for \"{clean_path}\"."),
                "Export the theme pack cleanly without duplicate filenames.",
            ));
        }

        let Some((kind, ext)) = classify_ext(&clean_path) else {
            return Err(refused(
                415,
                "unsupported_type",
                format!(
                    "This theme pack contains \"{clean_path}\", which isn't a supported media, font, or JSON file."
                ),
                "Remove scripts, web pages, or unknown file types and export the pack again.",
            ));
        };

        let entry_cap = kind.cap();
        if entry.size() > entry_cap as u64 {
            return Err(refused(
                413,
                "too_large",
                format!("\"{clean_path}\" is bigger than {} MB.", entry_cap / 1024 / 1024),
                "Compress large files inside the pack and try again.",
            ));
        }

        let packed = entry.compressed_size().max(1);
        // Text (JSON) can pack tightly and legitimately; its size is bounded by its own cap.
        let too_squashed =
            |unpacked: u64| kind != EntryKind::Json && unpacked >= RATIO_CHECK_FROM && unpacked / packed > MAX_RATIO;
        let bomb = || {
            refused(
                413,
                "suspicious_compression",
                format!("\"{clean_path}\" is packed far tighter than a picture, video or font can be."),
                "That looks like a zip bomb. Only import theme packs from sources you trust.",
            )
        };
        if too_squashed(entry.size()) {
            return Err(bomb());
        }
        let mut buf = Vec::new();
        let left = MAX_OVERLAY_UNPACKED.saturating_sub(total_unpacked);
        entry.take(entry_read_limit(entry_cap, left) + 1).read_to_end(&mut buf).map_err(|_| {
            refused(
                415,
                "bad_overlay_file",
                format!("Couldn't unpack \"{clean_path}\" from the archive."),
                "The file may be damaged. Download it again.",
            )
        })?;
        if buf.len() > entry_cap {
            return Err(refused(
                413,
                "too_large",
                format!("\"{clean_path}\" unpacks past the {} MB per-file limit.", entry_cap / 1024 / 1024),
                "Compress large files inside the pack and try again.",
            ));
        }
        if too_squashed(buf.len() as u64) {
            return Err(bomb());
        }
        total_unpacked = total_unpacked.saturating_add(buf.len() as u64);
        if total_unpacked > MAX_OVERLAY_UNPACKED {
            return Err(refused(
                413,
                "too_large",
                format!("This theme pack unpacks past the {} MB size limit.", MAX_OVERLAY_UNPACKED / 1024 / 1024),
                "Remove unnecessary large files and try again.",
            ));
        }

        match kind {
            EntryKind::Json => {
                let text = std::str::from_utf8(&buf).map_err(|_| {
                    refused(
                        422,
                        "invalid_overlay",
                        format!("That pack's file \"{clean_path}\" isn't valid UTF-8 JSON."),
                        "Ask whoever made the pack to export it again.",
                    )
                })?;
                let parsed: Value = serde_json::from_str(text).map_err(|_| {
                    refused(
                        422,
                        "invalid_overlay",
                        format!("That pack's scene file \"{clean_path}\" couldn't be read (broken JSON)."),
                        "Ask whoever made the pack to export it again.",
                    )
                })?;
                json_files.push((clean_path, parsed));
            }
            EntryKind::Font => {
                let Some((font_ext, declared_families)) = detect_font_format(&buf) else {
                    return Err(refused(
                        415,
                        "unsupported_type",
                        format!("\"{clean_path}\" doesn't look like a real font file."),
                        "Replace broken font files and export the pack again.",
                    ));
                };
                bundled_fonts.push(BundledFontFile {
                    archive_path: clean_path,
                    bytes: buf,
                    ext: font_ext,
                    declared_families,
                });
            }
            EntryKind::Image | EntryKind::Audio | EntryKind::Video => {
                if !sniff_media_matches(kind, ext, &buf) {
                    return Err(refused(
                        415,
                        "unsupported_type",
                        format!("\"{clean_path}\" doesn't match its media file type."),
                        "Re-save that image, sound, or video file and export again.",
                    ));
                }
                assets.push(UnpackedAsset { rel_path: clean_path, kind, bytes: buf });
            }
        }
    }

    Ok(ExtractedZip { json_files, assets, bundled_fonts })
}

struct AssetLookup<'a> {
    assets: &'a [UnpackedAsset],
    exact_lower: BTreeMap<String, usize>,
    basename_counts: BTreeMap<String, Option<usize>>,
}

impl<'a> AssetLookup<'a> {
    fn new(assets: &'a [UnpackedAsset]) -> Self {
        let mut exact_lower = BTreeMap::new();
        let mut basename_counts: BTreeMap<String, Option<usize>> = BTreeMap::new();
        for (idx, a) in assets.iter().enumerate() {
            exact_lower.insert(a.rel_path.to_ascii_lowercase(), idx);
            let base = a.rel_path.rsplit('/').next().unwrap_or(&a.rel_path).to_ascii_lowercase();
            basename_counts.entry(base).and_modify(|slot| *slot = None).or_insert(Some(idx));
        }
        Self { assets, exact_lower, basename_counts }
    }

    fn resolve(&self, raw_ref: &str) -> Option<usize> {
        let norm =
            raw_ref.trim().replace('\\', "/").trim_start_matches("./").trim_start_matches('/').to_ascii_lowercase();
        if norm.is_empty() {
            return None;
        }
        if let Some(&idx) = self.exact_lower.get(&norm) {
            return Some(idx);
        }
        // Strip leading top folder segment when JSON lives inside a subfolder or root omits it.
        if let Some((_, rest)) = norm.split_once('/')
            && let Some(&idx) = self.exact_lower.get(rest)
        {
            return Some(idx);
        }
        // Suffix match on multi-segment references (`/Users/.../pack/assets/bg.png`).
        let mut matches = self
            .assets
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                let p = a.rel_path.to_ascii_lowercase();
                norm.ends_with(&format!("/{p}")) || p.ends_with(&format!("/{norm}"))
            })
            .map(|(i, _)| i);
        if let Some(first) = matches.next()
            && matches.next().is_none()
        {
            return Some(first);
        }
        let base = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        self.basename_counts.get(&base).and_then(|opt| *opt)
    }
}

fn clean_pack_name(filename: &str, json_name: Option<&str>) -> String {
    if let Some(n) = json_name.map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= 80) {
        return n.to_string();
    }
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .strip_suffix(".overlay")
        .or_else(|| filename.rsplit(['/', '\\']).next().unwrap_or(filename).strip_suffix(".OVERLAY"))
        .or_else(|| filename.rsplit(['/', '\\']).next().unwrap_or(filename).strip_suffix(".zip"))
        .unwrap_or(filename)
        .trim();
    if stem.is_empty() { "Imported pack".into() } else { stem.chars().take(80).collect() }
}

fn is_root_node(v: &Value) -> bool {
    v.get("nodeType").and_then(Value::as_str) == Some("RootNode")
        || v.pointer("/data/scenes/data/items").is_some_and(Value::is_array)
}

fn is_obs_collection_json(v: &Value) -> bool {
    v.get("sources").is_some_and(Value::is_array)
}

fn is_own3d_manifest_json(v: &Value) -> bool {
    v.get("scenes").is_some_and(Value::is_array) && !is_obs_collection_json(v) && !is_root_node(v)
}

fn detect_manifest_hints(json_files: &[(String, Value)]) -> (Option<String>, bool, Vec<FontSpec>) {
    let mut name: Option<String> = None;
    let mut own3d_brand = false;
    let mut manifest_fonts = Vec::new();
    for (path, v) in json_files {
        let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let raw_str = v.to_string().to_ascii_lowercase();
        if (base == "manifest.json"
            || base == "pack.json"
            || raw_str.contains("\"own3d")
            || raw_str.contains("own3d.pro")
            || raw_str.contains("own3d.tv"))
            && raw_str.contains("own3d")
        {
            own3d_brand = true;
        }
        if name.is_none()
            && let Some(n) =
                v.get("name").or_else(|| v.get("title")).or_else(|| v.pointer("/data/name")).and_then(Value::as_str)
            && !n.trim().is_empty()
        {
            name = Some(n.trim().to_string());
        }
        if let Some(arr) = v.get("fonts").and_then(Value::as_array) {
            for f in arr {
                let s = f.as_str().or_else(|| f.get("family").and_then(Value::as_str));
                if let Some(spec) = parse_font_spec(s, s, 0, f.get("style").and_then(Value::as_str)) {
                    manifest_fonts.push(spec);
                }
            }
        }
    }
    (name, own3d_brand, manifest_fonts)
}

fn select_primary_json(json_files: &[(String, Value)]) -> Option<&(String, Value)> {
    // 1. Standard Streamlabs RootNode `config.json` at root or one level down.
    if let Some(hit) = json_files.iter().find(|(p, v)| {
        p.eq_ignore_ascii_case("config.json")
            && (is_root_node(v) || is_obs_collection_json(v) || is_own3d_manifest_json(v))
    }) {
        return Some(hit);
    }
    if let Some(hit) = json_files.iter().find(|(_, v)| is_root_node(v)) {
        return Some(hit);
    }
    if let Some(hit) = json_files.iter().find(|(p, v)| {
        let base = p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
        matches!(base.as_str(), "collection.json" | "scenes.json" | "manifest.json")
            && (is_obs_collection_json(v) || is_own3d_manifest_json(v))
    }) {
        return Some(hit);
    }
    json_files.iter().find(|(_, v)| is_obs_collection_json(v) || is_own3d_manifest_json(v))
}

fn sl_color_to_obs(color_val: Option<&Value>) -> Option<u64> {
    let v = color_val?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some((n as u32) as u64);
    }
    // { r, g, b, a } or "#rrggbb"
    if let Some(hex) = v.as_str() {
        let s = hex.trim().trim_start_matches('#');
        if s.len() == 6
            && let Ok(rgb) = u32::from_str_radix(s, 16)
        {
            let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
            return Some(u64::from(0xff00_0000 | (b << 16) | (g << 8) | r));
        }
        if s.len() == 8
            && let Ok(rrggbbaa) = u32::from_str_radix(s, 16)
        {
            let (r, g, b, a) =
                ((rrggbbaa >> 24) & 0xff, (rrggbbaa >> 16) & 0xff, (rrggbbaa >> 8) & 0xff, rrggbbaa & 0xff);
            return Some(u64::from((a << 24) | (b << 16) | (g << 8) | r));
        }
    }
    if let Some(obj) = v.as_object() {
        let r = obj.get("r").and_then(Value::as_u64).unwrap_or(255) & 0xff;
        let g = obj.get("g").and_then(Value::as_u64).unwrap_or(255) & 0xff;
        let b = obj.get("b").and_then(Value::as_u64).unwrap_or(255) & 0xff;
        let a = obj.get("a").and_then(Value::as_u64).unwrap_or(255) & 0xff;
        return Some((a << 24) | (b << 16) | (g << 8) | r);
    }
    None
}

fn map_widget_type(raw: Option<&Value>, raw_name: &str) -> Option<(WidgetKind, &'static str)> {
    if let Some(n) = raw.and_then(Value::as_u64) {
        return match n {
            0 => Some((WidgetKind::Alerts, "Alerts")),
            1 | 2 | 3 | 4 | 15 | 16 | 17 | 18 | 22 => Some((WidgetKind::Goal, "Goal bar")),
            6 => Some((WidgetKind::Chat, "Chat box")),
            7 => Some((WidgetKind::Events, "Event list")),
            9 => Some((WidgetKind::Viewers, "Viewer count")),
            12 => Some((WidgetKind::Giveaway, "Spin wheel / Giveaway")),
            19 => Some((WidgetKind::Poll, "Poll")),
            _ => None,
        };
    }
    let key = raw.and_then(Value::as_str).unwrap_or(raw_name).trim().to_ascii_lowercase().replace(['-', '_', ' '], "");
    if key.is_empty() {
        return None;
    }
    match key.as_str() {
        "alertbox" | "alerts" | "alert" => Some((WidgetKind::Alerts, "Alerts")),
        "chatbox" | "chat" => Some((WidgetKind::Chat, "Chat box")),
        "eventlist" | "events" | "recentevents" => Some((WidgetKind::Events, "Event list")),
        "goal" | "donationgoal" | "followergoal" | "subscribergoal" | "subgoal" | "bitgoal" | "charitygoal"
        | "superchatgoal" | "starsgoal" | "supportergoal" => Some((WidgetKind::Goal, "Goal bar")),
        "viewercount" | "viewers" => Some((WidgetKind::Viewers, "Viewer count")),
        "poll" => Some((WidgetKind::Poll, "Poll")),
        "spinwheel" | "wheel" | "giveaway" => Some((WidgetKind::Giveaway, "Spin wheel / Giveaway")),
        "streamlabel" | "streamlabels" | "label" | "labels" => Some((WidgetKind::Label, "Stream label")),
        _ => None,
    }
}

/// A Streamlabs stream label's kind as a Ferrocast label source and its text. The count kinds are
/// for this stream, this month or all time as the name says; the rest are the latest of each.
/// A kind nothing here knows is the latest follower (a label with no source isn't valid).
fn map_streamlabel_template(label_type: &str) -> (&'static str, String) {
    let key = label_type.trim().to_ascii_lowercase();
    let has = |w: &str| key.contains(w);
    let span = if has("total") || has("all_time") || has("alltime") {
        "alltime"
    } else if has("month") {
        "monthly"
    } else {
        "session"
    };
    let counted = has("count") || has("total") || has("session") || has("month");
    let (source, text): (String, &str) = if has("follower") {
        if counted {
            (format!("{span}_follows"), "Followers: {value}")
        } else {
            ("latest_follower".into(), "Latest follower: {value}")
        }
    } else if has("sub") {
        if counted { (format!("{span}_subs"), "Subs: {value}") } else { ("latest_sub".into(), "Latest sub: {value}") }
    } else if has("donat") || has("tip") {
        if counted { (format!("{span}_tips"), "Tips: {value}") } else { ("latest_tip".into(), "Latest tip: {value}") }
    } else if has("cheer") || has("bit") {
        if counted {
            (format!("{span}_bits"), "Bits: {value}")
        } else {
            ("latest_bits".into(), "Latest cheer: {value}")
        }
    } else if has("raid") {
        if counted {
            (format!("{span}_raids"), "Raids: {value}")
        } else {
            ("latest_raid".into(), "Latest raid: {value}")
        }
    } else {
        ("latest_follower".into(), "{value}")
    };
    // A source the engine doesn't have (a combination it never offers) falls back to the latest.
    let source = ferrocast_core::overlay::LabelSource::parse(&source)
        .map(ferrocast_core::overlay::LabelSource::as_str)
        .unwrap_or("latest_follower");
    (source, text.to_string())
}

/// A label's text as the label engine reads it: the old Streamlabs words for the value are `{value}`
/// here, and anything that doesn't parse or is too long is the plain value.
fn label_template(raw: &str) -> String {
    let mut t = raw.to_string();
    for old in
        ["recent_follow", "recent_sub", "recent_tip", "recent_bits", "recent_raid", "session_followers", "session_subs"]
    {
        t = t.replace(&format!("{{{old}}}"), "{value}");
    }
    if t.chars().count() > 240 || ferrocast_core::template::Template::parse(&t).is_err() {
        return "{value}".into();
    }
    t
}

#[cfg(test)]
pub(crate) fn widget_default_size_for_tests(kind: WidgetKind) -> (i32, i32) {
    widget_default_size(kind)
}

#[cfg(test)]
pub(crate) fn widget_default_config_for_tests(kind: WidgetKind) -> Value {
    widget_default_config(kind, None, None)
}

fn widget_default_size(kind: WidgetKind) -> (i32, i32) {
    match kind {
        WidgetKind::Alerts => (800, 420),
        WidgetKind::Chat => (480, 560),
        WidgetKind::Events => (420, 340),
        WidgetKind::Goal => (600, 96),
        WidgetKind::Label => (480, 72),
        WidgetKind::Viewers => (260, 72),
        WidgetKind::Poll => (480, 340),
        WidgetKind::Giveaway => (520, 420),
        WidgetKind::EndCredits => (800, 700),
        WidgetKind::EmoteWall => (1920, 1080),
        WidgetKind::ChatHighlight => (620, 220),
        WidgetKind::Layer => (400, 200),
    }
}

fn clamp_widget_box(
    x_px: f64,
    y_px: f64,
    scale_x: f64,
    scale_y: f64,
    canvas: OverlayCanvas,
    kind: WidgetKind,
) -> (i32, i32, i32, i32) {
    let (dw, dh) = widget_default_size(kind);
    let cw = canvas.width as i32;
    let ch = canvas.height as i32;
    let w = ((dw as f64) * scale_x.abs().clamp(0.2, 2.5)).round() as i32;
    let h = ((dh as f64) * scale_y.abs().clamp(0.2, 2.5)).round() as i32;
    let w = w.clamp(80, cw);
    let h = h.clamp(40, ch);
    let x = (x_px.round() as i32).clamp(0, cw.saturating_sub(w));
    let y = (y_px.round() as i32).clamp(0, ch.saturating_sub(h));
    (x, y, w, h)
}

fn widget_default_config(kind: WidgetKind, label_text: Option<&str>, font_family_name: Option<&str>) -> Value {
    let mut base = match kind {
        WidgetKind::Alerts => json!({ "theme": "molten", "rule_ids": null }),
        WidgetKind::Chat => json!({
            "platforms": [],
            "max_messages": 25,
            "fade_after_s": null,
            "show_badges": true,
            "show_platform": true,
            "hide_commands": true,
            "hide_bots": true,
            "font_size": 20,
            "theme": "molten"
        }),
        WidgetKind::Events => json!({
            "kinds": ["follow", "subscription", "gift_batch", "paid_message", "tip", "raid"],
            "max_items": 6,
            "theme": "molten"
        }),
        WidgetKind::Goal => json!({ "goal_id": "", "theme": "molten" }),
        // A label needs a source (a config without one fails validation and the whole overlay
        // was dropped): the latest follower unless the pack's kind says otherwise.
        WidgetKind::Label => json!({
            "source": "latest_follower",
            "template": label_text.map_or_else(|| "Latest follower: {value}".to_string(), label_template),
            "theme": "molten"
        }),
        WidgetKind::Viewers => json!({ "theme": "molten" }),
        WidgetKind::Poll => json!({ "theme": "molten" }),
        WidgetKind::Giveaway => json!({ "theme": "molten" }),
        // No pack from Streamlabs or OWN3D maps to these (they are Ferrocast's own widgets), so
        // these are only here for a complete match: the defaults pass `validate_widget_config`.
        WidgetKind::EndCredits => json!({ "theme": "molten", "title": "Thanks for watching", "speed": 60 }),
        WidgetKind::EmoteWall => json!({ "theme": "molten", "max_emotes": 60, "size": 56, "lifetime_s": 6 }),
        WidgetKind::ChatHighlight => {
            json!({ "theme": "molten", "duration_s": 30, "font_size": 24, "show_badges": true, "show_platform": true })
        }
        WidgetKind::Layer => json!({ "type": "text", "text": label_text.unwrap_or("") }),
    };
    if kind != WidgetKind::Layer
        && let Some(f) = font_family_name.filter(|s| is_font_family(s))
        && let Some(obj) = base.as_object_mut()
    {
        obj.insert("style".into(), json!({ "font": f }));
    }
    base
}

fn redact_url_note(url: &str) -> String {
    // Only where it is (scheme and host): a path, query or fragment can hold a token.
    match reqwest::Url::parse(url.trim()) {
        Ok(u) if u.host_str().is_some() => format!("{}://{}/…", u.scheme(), u.host_str().unwrap_or_default()),
        _ => "an address".into(),
    }
}

struct BuildCtx<'a> {
    lookup: AssetLookup<'a>,
    sources_by_name: Map<String, Value>,
    used_source_names: BTreeSet<String>,
    missing_assets: Vec<String>,
    missing_assets_seen: BTreeSet<String>,
    unknown_sources: Vec<UnknownSource>,
    font_specs: Vec<FontSpec>,
    widgets: Vec<PreviewWidget>,
    scene_overlays: Vec<PlannedSceneOverlay>,
    preview_scenes: Vec<PreviewScene>,
    notes: Vec<String>,
    web_page_count: usize,
    overlay_counter: usize,
}

impl<'a> BuildCtx<'a> {
    fn unique_source_name(&mut self, desired: &str) -> String {
        let base = if desired.trim().is_empty() { "Source" } else { desired.trim() };
        if self.used_source_names.insert(base.to_ascii_lowercase()) {
            return base.to_string();
        }
        for i in 2.. {
            let cand = format!("{base} ({i})");
            if self.used_source_names.insert(cand.to_ascii_lowercase()) {
                return cand;
            }
        }
        base.to_string()
    }

    fn note_missing_asset(&mut self, name: &str) {
        let trimmed = name.trim();
        if !trimmed.is_empty() && self.missing_assets_seen.insert(trimmed.to_ascii_lowercase()) {
            self.missing_assets.push(trimmed.to_string());
        }
    }

    fn resolve_asset_placeholder(&mut self, raw_ref: Option<&str>, fallback_name: &str) -> String {
        if let Some(raw) = raw_ref {
            if let Some(idx) = self.lookup.resolve(raw) {
                return format!("{ASSET_PLACEHOLDER_PREFIX}{idx}");
            }
            self.note_missing_asset(raw);
            return fallback_name.to_string();
        }
        String::new()
    }

    fn push_text_font(&mut self, settings: &Value) -> Option<String> {
        let face = settings.pointer("/font/face").and_then(Value::as_str);
        let style = settings.pointer("/font/style").and_then(Value::as_str);
        let flags = settings.pointer("/font/flags").and_then(Value::as_u64).unwrap_or(0);
        let custom = settings.get("custom_font").and_then(Value::as_str);
        if let Some(spec) = parse_font_spec(face, custom, flags, style) {
            let fam = spec.family.clone();
            self.font_specs.push(spec);
            return Some(fam);
        }
        None
    }

    fn register_source(
        &mut self,
        name: String,
        kind: &str,
        settings: Value,
        filters: Vec<Value>,
        extra: Option<(&str, Value)>,
    ) {
        if kind == "ferrocast_web" || kind == "browser_source" {
            self.web_page_count += 1;
        }
        let mut obj = Map::new();
        obj.insert("name".into(), json!(name));
        obj.insert("id".into(), json!(kind));
        obj.insert("versioned_id".into(), json!(kind));
        obj.insert("settings".into(), settings);
        if !filters.is_empty() {
            obj.insert("filters".into(), Value::Array(filters));
        }
        if let Some((k, v)) = extra {
            obj.insert(k.into(), v);
        }
        self.sources_by_name.insert(name, Value::Object(obj));
    }
}

fn convert_sl_filters(raw_filters: Option<&Value>) -> Vec<Value> {
    let Some(arr) = raw_filters.and_then(Value::as_array) else { return Vec::new() };
    arr.iter()
        .filter_map(|f| {
            let raw_id = f.get("type").or_else(|| f.get("id")).and_then(Value::as_str)?;
            let name = f.get("name").and_then(Value::as_str).unwrap_or(raw_id);
            let (mapped_id, _) = for_this_computer(raw_id);
            let obs_id = if mapped_id.is_empty() { raw_id } else { mapped_id };
            Some(json!({
                "name": name,
                "id": obs_id,
                "versioned_id": obs_id,
                "enabled": f.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                "settings": f.get("settings").cloned().unwrap_or_else(|| json!({})),
            }))
        })
        .collect()
}

fn scale_coord(raw: Option<f64>, axis: f64, default: f64) -> f64 {
    match raw {
        Some(v) if v.is_finite() => v * axis,
        _ => default,
    }
}

fn scale_factor(raw: Option<f64>, axis: f64) -> f64 {
    match raw {
        Some(v) if v.is_finite() && v.abs() > 0.0 => (v * axis).clamp(-1000.0, 1000.0),
        _ => 1.0,
    }
}

fn parse_streamlabs_root(
    root: &Value,
    extracted: ExtractedZip,
    filename: &str,
    hint_name: Option<String>,
    own3d_brand: bool,
    manifest_fonts: Vec<FontSpec>,
) -> Result<ParsedPack, EngineError> {
    let base_w = root.pointer("/data/baseWidth").and_then(Value::as_u64).unwrap_or(1920).clamp(320, 7680) as u32;
    let base_h = root.pointer("/data/baseHeight").and_then(Value::as_u64).unwrap_or(1080).clamp(180, 4320) as u32;
    let canvas = OverlayCanvas { width: base_w, height: base_h };
    let pack_name = clean_pack_name(filename, hint_name.as_deref());

    let raw_scenes = root.pointer("/data/scenes/data/items").and_then(Value::as_array).ok_or_else(|| {
        refused(
            422,
            "invalid_overlay",
            "This .overlay file is missing its scenes list.",
            "Ask whoever made the pack to export it again.",
        )
    })?;
    if raw_scenes.is_empty() {
        return Err(refused(
            422,
            "overlay_no_scenes",
            "This theme pack doesn't contain any scenes.",
            "Pick another .overlay pack.",
        ));
    }

    // Map sceneId -> OBS scene name upfront so nested SceneSourceNode can resolve targets cleanly.
    let mut scene_id_to_name: BTreeMap<String, String> = BTreeMap::new();
    let mut used_scene_names = BTreeSet::new();
    let mut resolved_scene_names = Vec::with_capacity(raw_scenes.len());
    for (idx, sc) in raw_scenes.iter().enumerate() {
        let base_name =
            sc.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Scene");
        let mut chosen = base_name.to_string();
        if !used_scene_names.insert(chosen.to_ascii_lowercase()) {
            chosen = format!("{base_name} ({})", idx + 1);
            used_scene_names.insert(chosen.to_ascii_lowercase());
        }
        if let Some(sid) = sc.get("sceneId").or_else(|| sc.get("id")).and_then(Value::as_str) {
            scene_id_to_name.insert(sid.to_string(), chosen.clone());
        }
        resolved_scene_names.push(chosen);
    }

    let extracted_assets = extracted.assets;
    let extracted_bundled_fonts = extracted.bundled_fonts;
    let mut ctx = BuildCtx {
        lookup: AssetLookup::new(&extracted_assets),
        sources_by_name: Map::new(),
        used_source_names: BTreeSet::new(),
        missing_assets: Vec::new(),
        missing_assets_seen: BTreeSet::new(),
        unknown_sources: Vec::new(),
        font_specs: manifest_fonts,
        widgets: Vec::new(),
        scene_overlays: Vec::new(),
        preview_scenes: Vec::new(),
        notes: Vec::new(),
        web_page_count: 0,
        overlay_counter: 0,
    };
    // Reserve scene names so no regular source can collide with a scene name.
    for n in &resolved_scene_names {
        ctx.used_source_names.insert(n.to_ascii_lowercase());
    }

    let mut obs_scene_sources = Vec::new();
    let mut scene_order = Vec::new();
    let web_kind = for_this_computer("browser_source").0;

    for (scene_idx, (sc, scene_name)) in raw_scenes.iter().zip(resolved_scene_names.iter()).enumerate() {
        scene_order.push(json!({ "name": scene_name }));
        let raw_items = sc.pointer("/slots/data/items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);

        // Streamlabs lists slots top-first; OBS stores scene items bottom-first.
        let mut obs_items: Vec<Value> = Vec::new();
        let mut scene_widgets: Vec<Widget> = Vec::new();
        let mut mapped_count = 0usize;
        let mut unknown_count = 0usize;
        let mut widget_count = 0usize;
        let mut next_item_id: i64 = 1;

        // Collect folder child ids so we know which items live in a folder vs root (keep draw order cleanly).
        for slot in raw_items.iter().rev() {
            if slot.get("sceneNodeType").and_then(Value::as_str) == Some("folder") {
                continue;
            }
            let item_name_raw = slot.get("name").and_then(Value::as_str).unwrap_or("Layer");
            let content = slot.get("content");
            let node_type = content.and_then(|c| c.get("nodeType")).and_then(Value::as_str).unwrap_or("");
            let data = content.and_then(|c| c.get("data")).unwrap_or(&Value::Null);

            let pos_x = scale_coord(slot.get("x").and_then(Value::as_f64), f64::from(base_w), 0.0);
            let pos_y = scale_coord(slot.get("y").and_then(Value::as_f64), f64::from(base_h), 0.0);
            let sc_x = scale_factor(slot.get("scaleX").and_then(Value::as_f64), f64::from(base_w));
            let sc_y = scale_factor(slot.get("scaleY").and_then(Value::as_f64), f64::from(base_h));
            let rotation = slot.get("rotation").and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0);
            let visible = slot.get("visible").and_then(Value::as_bool).unwrap_or(true);
            let locked = slot.get("locked").and_then(Value::as_bool).unwrap_or(false);
            let filters = convert_sl_filters(slot.get("filters"));

            let mut make_obs_item = |source_name: String, bounds_opt: Option<(f64, f64, &str)>| {
                let id = next_item_id;
                next_item_id += 1;
                let mut obj = Map::new();
                obj.insert("id".into(), json!(id));
                obj.insert("name".into(), json!(source_name));
                obj.insert("visible".into(), json!(visible));
                obj.insert("locked".into(), json!(locked));
                obj.insert("pos".into(), json!({ "x": pos_x, "y": pos_y }));
                obj.insert("scale".into(), json!({ "x": sc_x, "y": sc_y }));
                obj.insert("rot".into(), json!(rotation));
                obj.insert("align".into(), json!(5));
                if let Some((bw, bh, bt)) = bounds_opt {
                    obj.insert("bounds_type".into(), json!(if bt == "fit" { 2 } else { 1 }));
                    obj.insert("bounds".into(), json!({ "x": bw.max(1.0), "y": bh.max(1.0) }));
                }
                if let Some(crop) = slot.get("crop").and_then(Value::as_object) {
                    obj.insert("crop_left".into(), json!(crop.get("left").and_then(Value::as_i64).unwrap_or(0).max(0)));
                    obj.insert("crop_top".into(), json!(crop.get("top").and_then(Value::as_i64).unwrap_or(0).max(0)));
                    obj.insert(
                        "crop_right".into(),
                        json!(crop.get("right").and_then(Value::as_i64).unwrap_or(0).max(0)),
                    );
                    obj.insert(
                        "crop_bottom".into(),
                        json!(crop.get("bottom").and_then(Value::as_i64).unwrap_or(0).max(0)),
                    );
                }
                obs_items.push(Value::Object(obj));
            };

            match node_type {
                "ImageNode" => {
                    let sname = ctx.unique_source_name(item_name_raw);
                    let file_ref =
                        ctx.resolve_asset_placeholder(data.get("filename").and_then(Value::as_str), item_name_raw);
                    let mut settings = data.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
                    settings.insert("file".into(), json!(file_ref));
                    ctx.register_source(sname.clone(), "image_source", Value::Object(settings), filters, None);
                    make_obs_item(sname, None);
                    mapped_count += 1;
                }
                "VideoNode" => {
                    let sname = ctx.unique_source_name(item_name_raw);
                    let file_ref =
                        ctx.resolve_asset_placeholder(data.get("filename").and_then(Value::as_str), item_name_raw);
                    let mut settings = data.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
                    settings.insert("local_file".into(), json!(file_ref));
                    settings.insert("is_local_file".into(), json!(true));
                    ctx.register_source(sname.clone(), "ffmpeg_source", Value::Object(settings), filters, None);
                    make_obs_item(sname, None);
                    mapped_count += 1;
                }
                "TextNode" => {
                    let sname = ctx.unique_source_name(item_name_raw);
                    let mut settings = data.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
                    if let Some(c) = sl_color_to_obs(settings.get("color")) {
                        settings.entry("color1").or_insert_with(|| json!(c));
                    }
                    if let Some(gc) = sl_color_to_obs(settings.get("gradient_color")) {
                        settings.entry("color2").or_insert_with(|| json!(gc));
                    }
                    let sv = Value::Object(settings);
                    let parsed_fam = ctx.push_text_font(&sv);
                    let mut final_settings = sv;
                    if let Some(fam) = parsed_fam
                        && final_settings.get("font").is_none()
                    {
                        final_settings["font"] = json!({ "face": fam, "size": 48 });
                    }
                    let (mapped_fam, _) = for_this_computer("text_gdiplus");
                    let obs_kind = if mapped_fam.is_empty() { "text_ft2_source_v2" } else { mapped_fam };
                    ctx.register_source(sname.clone(), obs_kind, final_settings, filters, None);
                    make_obs_item(sname, None);
                    mapped_count += 1;
                }
                "WebcamNode" => {
                    let sname = ctx.unique_source_name(item_name_raw);
                    let (cam_kind, _) = for_this_computer("dshow_input");
                    let w = data.get("width").and_then(Value::as_f64).unwrap_or(0.25) * f64::from(base_w);
                    let h = data.get("height").and_then(Value::as_f64).unwrap_or(0.25) * f64::from(base_h);
                    ctx.register_source(sname.clone(), cam_kind, json!({}), filters, None);
                    make_obs_item(sname, Some((w, h, "fit")));
                    mapped_count += 1;
                }
                "GameCaptureNode" => {
                    let sname = ctx.unique_source_name(item_name_raw);
                    let (cap_kind, _) = for_this_computer("game_capture");
                    if let Some(ph) = data.get("placeholderFile").and_then(Value::as_str) {
                        let _ = ctx.lookup.resolve(ph);
                    }
                    ctx.register_source(sname.clone(), cap_kind, json!({}), filters, None);
                    make_obs_item(sname, None);
                    mapped_count += 1;
                }
                "IconLibraryNode" => {
                    let folder = data
                        .get("folder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim_end_matches('/')
                        .to_ascii_lowercase();
                    let first_in_folder = extracted_assets.iter().position(|a| {
                        a.kind == EntryKind::Image
                            && (folder.is_empty() || a.rel_path.to_ascii_lowercase().starts_with(&format!("{folder}/")))
                    });
                    if let Some(asset_idx) = first_in_folder {
                        let sname = ctx.unique_source_name(item_name_raw);
                        ctx.register_source(
                            sname.clone(),
                            "image_source",
                            json!({ "file": format!("{ASSET_PLACEHOLDER_PREFIX}{asset_idx}") }),
                            filters,
                            None,
                        );
                        make_obs_item(sname, None);
                        mapped_count += 1;
                    } else {
                        unknown_count += 1;
                        ctx.unknown_sources.push(UnknownSource {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            kind: "IconLibrary".into(),
                            reason: "No image was found in that icon library folder inside the pack.".into(),
                        });
                    }
                }
                "SceneSourceNode" => {
                    let target_id = data.get("sceneId").and_then(Value::as_str).unwrap_or("");
                    if let Some(target_scene) = scene_id_to_name.get(target_id) {
                        make_obs_item(target_scene.clone(), None);
                        mapped_count += 1;
                    } else {
                        unknown_count += 1;
                        ctx.unknown_sources.push(UnknownSource {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            kind: "Scene reference".into(),
                            reason: "Referenced scene wasn't found in this pack.".into(),
                        });
                    }
                }
                "WidgetNode" => {
                    let raw_type = data.get("type");
                    let raw_label = raw_type
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| raw_type.and_then(Value::as_u64).map(|n| format!("Widget #{n}")))
                        .unwrap_or_else(|| "Widget".into());
                    if let Some((wk, label)) = map_widget_type(raw_type, item_name_raw) {
                        if scene_widgets.len() < MAX_WIDGETS {
                            ctx.overlay_counter += 1;
                            let wid = format!("w-{}", ctx.overlay_counter);
                            let box_rect = clamp_widget_box(pos_x, pos_y, sc_x, sc_y, canvas, wk);
                            let font_hint = data.pointer("/settings/font/face").and_then(Value::as_str);
                            if let Some(spec) = parse_font_spec(font_hint, None, 0, None) {
                                ctx.font_specs.push(spec);
                            }
                            scene_widgets.push(Widget::new(
                                &wid,
                                wk,
                                box_rect,
                                widget_default_config(wk, None, font_hint),
                            ));
                        }
                        widget_count += 1;
                        mapped_count += 1;
                        ctx.widgets.push(PreviewWidget {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            source_kind: raw_label,
                            mapped_to: Some(wk),
                            overlay_widget_name: label.to_string(),
                            supported: true,
                        });
                    } else {
                        unknown_count += 1;
                        ctx.widgets.push(PreviewWidget {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            source_kind: raw_label.clone(),
                            mapped_to: None,
                            overlay_widget_name: "Not supported".to_string(),
                            supported: false,
                        });
                        ctx.unknown_sources.push(UnknownSource {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            kind: raw_label,
                            reason: "This third-party widget isn't available natively in Ferrocast yet.".into(),
                        });
                    }
                }
                "StreamlabelNode" => {
                    let stat = data.get("labelType").and_then(Value::as_str).unwrap_or("follower");
                    let text_settings = data.pointer("/textSource/data/settings").unwrap_or(&Value::Null);
                    let font_hint = ctx.push_text_font(text_settings);
                    let (label_source, template) = map_streamlabel_template(stat);
                    if scene_widgets.len() < MAX_WIDGETS {
                        ctx.overlay_counter += 1;
                        let wid = format!("lbl-{}", ctx.overlay_counter);
                        let box_rect = clamp_widget_box(pos_x, pos_y, sc_x, sc_y, canvas, WidgetKind::Label);
                        scene_widgets.push(Widget::new(&wid, WidgetKind::Label, box_rect, {
                            let mut cfg =
                                widget_default_config(WidgetKind::Label, Some(&template), font_hint.as_deref());
                            cfg["source"] = json!(label_source);
                            cfg
                        }));
                    }
                    widget_count += 1;
                    mapped_count += 1;
                    ctx.widgets.push(PreviewWidget {
                        scene: scene_name.clone(),
                        name: item_name_raw.to_string(),
                        source_kind: format!("Streamlabel ({stat})"),
                        mapped_to: Some(WidgetKind::Label),
                        overlay_widget_name: "Label widget".to_string(),
                        supported: true,
                    });
                }
                "SmartBrowserNode" => {
                    let settings = data.get("settings").unwrap_or(&Value::Null);
                    let raw_url = settings.get("url").and_then(Value::as_str).unwrap_or("").trim();
                    // Only a page on the public internet: a stranger's pack may not point the
                    // Studio at this computer, the local network or a file.
                    if overlay_safe::public_page_url(raw_url).is_some() {
                        let sname = ctx.unique_source_name(item_name_raw);
                        ctx.register_source(
                            sname.clone(),
                            web_kind,
                            json!({
                                "url": raw_url,
                                "width": settings.get("width").and_then(Value::as_u64).unwrap_or(1920),
                                "height": settings.get("height").and_then(Value::as_u64).unwrap_or(1080),
                            }),
                            filters,
                            None,
                        );
                        make_obs_item(sname, None);
                        mapped_count += 1;
                        ctx.notes.push(format!(
                            "Web source \"{}\" in \"{}\" keeps its URL ({}).",
                            item_name_raw,
                            scene_name,
                            redact_url_note(raw_url)
                        ));
                    } else {
                        unknown_count += 1;
                        ctx.unknown_sources.push(UnknownSource {
                            scene: scene_name.clone(),
                            name: item_name_raw.to_string(),
                            kind: "SmartBrowserNode".into(),
                            reason: "Only web pages on the internet are imported (not local files, this computer or the local network); rebuild it in Ferrocast Overlays."
                                .into(),
                        });
                    }
                }
                other => {
                    unknown_count += 1;
                    ctx.unknown_sources.push(UnknownSource {
                        scene: scene_name.clone(),
                        name: item_name_raw.to_string(),
                        kind: if other.is_empty() { "unknown".into() } else { other.into() },
                        reason: format!("Source type \"{}\" isn't supported yet and was left out.", other),
                    });
                }
            }
        }

        // Add one Ferrocast Overlay browser source covering mapped widgets/labels on this scene.
        if !scene_widgets.is_empty() {
            let ph_key = format!("{OVERLAY_PLACEHOLDER_PREFIX}{scene_idx}");
            let sname = ctx.unique_source_name(&format!("{scene_name} Overlay"));
            ctx.register_source(
                sname.clone(),
                web_kind,
                json!({
                    "url": ph_key,
                    "width": base_w,
                    "height": base_h,
                }),
                Vec::new(),
                None,
            );
            let id = next_item_id;
            obs_items.push(json!({
                "id": id,
                "name": sname,
                "visible": true,
                "locked": false,
                "pos": { "x": 0.0, "y": 0.0 },
                "scale": { "x": 1.0, "y": 1.0 },
                "rot": 0.0,
                "align": 5,
            }));
            ctx.scene_overlays.push(PlannedSceneOverlay {
                placeholder_key: ph_key,
                scene_name: scene_name.clone(),
                widgets: scene_widgets,
            });
        }

        obs_scene_sources.push(json!({
            "name": scene_name,
            "id": "scene",
            "versioned_id": "scene",
            "settings": {
                "id_counter": obs_items.len() as i64,
                "items": obs_items,
            }
        }));

        ctx.preview_scenes.push(PreviewScene {
            name: scene_name.clone(),
            items: mapped_count + unknown_count,
            mapped: mapped_count,
            widgets: widget_count,
            unknown: unknown_count,
        });
    }

    let mut transitions = Vec::new();
    let mut current_transition = "Fade".to_string();
    if let Some(tr) = root.pointer("/data/transition/data").and_then(Value::as_object) {
        let t_type = tr.get("type").and_then(Value::as_str).unwrap_or("fade_transition");
        let dur = tr.get("duration").and_then(Value::as_i64).unwrap_or(300).clamp(50, 10_000);
        let (mapped_tr, _) = for_this_computer(t_type);
        let obs_tr = if mapped_tr.is_empty() { "fade_transition" } else { mapped_tr };
        let mut settings = tr.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(raw_stinger) =
            tr.get("path").and_then(Value::as_str).or_else(|| settings.get("path").and_then(Value::as_str))
        {
            let resolved = ctx.resolve_asset_placeholder(Some(raw_stinger), raw_stinger);
            settings.insert("path".into(), json!(resolved));
        }
        current_transition = "Theme Transition".into();
        transitions.push(json!({
            "name": current_transition,
            "id": obs_tr,
            "settings": Value::Object(settings),
            "duration": dur,
        }));
    } else {
        transitions.push(json!({ "name": "Fade", "id": "fade_transition", "settings": {}, "duration": 300 }));
    }

    let first_scene = resolved_scene_names[0].clone();
    let mut sources_list: Vec<Value> = obs_scene_sources;
    sources_list.extend(ctx.sources_by_name.into_values());

    let mut doc = json!({
        "name": pack_name,
        "current_scene": first_scene,
        "current_program_scene": first_scene,
        "current_transition": current_transition,
        "transition_duration": transitions.first().and_then(|t| t.get("duration")).and_then(Value::as_i64).unwrap_or(300),
        "scene_order": scene_order,
        "transitions": transitions,
        "sources": sources_list,
    });

    // The pack's JSON is a stranger's: rebuild it from what is allowed (see overlay_safe).
    let safe = overlay_safe::make_safe(&mut doc, ctx.lookup.assets.len(), &|r| ctx.lookup.resolve(r));
    ctx.notes.extend(safe.notes);
    for m in safe.missing_assets {
        if ctx.missing_assets_seen.insert(m.to_ascii_lowercase()) {
            ctx.missing_assets.push(m);
        }
    }
    ctx.web_page_count = count_web_pages(&doc);

    let format = if own3d_brand { OverlayPackFormat::Own3d } else { OverlayPackFormat::Streamlabs };
    let source_count = doc["sources"].as_array().map(Vec::len).unwrap_or(0);
    let BuildCtx {
        lookup: _,
        preview_scenes,
        widgets,
        scene_overlays,
        unknown_sources,
        missing_assets,
        font_specs,
        notes,
        web_page_count,
        ..
    } = ctx;

    Ok(ParsedPack {
        suggested_name: pack_name,
        format,
        canvas,
        scenes: preview_scenes,
        widgets,
        scene_overlays,
        unknown_sources,
        missing_assets,
        assets: extracted_assets,
        bundled_fonts: extracted_bundled_fonts,
        font_specs,
        notes,
        doc_template: doc,
        missing_kinds: Vec::new(),
        web_page_count,
        source_count,
    })
}

fn parse_obs_or_own3d_pack(
    primary: &Value,
    extracted: ExtractedZip,
    filename: &str,
    hint_name: Option<String>,
    own3d_brand: bool,
    mut font_specs: Vec<FontSpec>,
) -> Result<ParsedPack, EngineError> {
    // Support OWN3D simplified schema `{ name, resolution, scenes: [{ name, items/layers/sources }] }`
    // by converting it into OBS JSON cleanly before asset/widget rewriting.
    let mut doc = if is_obs_collection_json(primary) { primary.clone() } else { convert_own3d_scenes_to_obs(primary)? };

    let pack_name = clean_pack_name(filename, hint_name.as_deref().or_else(|| doc.get("name").and_then(Value::as_str)));
    let web_kind = for_this_computer("browser_source").0;
    let mut missing_kinds = Vec::new();
    let mut missing_kinds_seen = BTreeSet::new();
    if let Some(sources) = doc.get_mut("sources").and_then(Value::as_array_mut) {
        for src in sources.iter_mut() {
            let old_id = src.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let mapped = for_this_computer(&old_id).0;
            let eff_id = if mapped.is_empty() { old_id.as_str() } else { mapped };
            if !mapped.is_empty() {
                src["id"] = json!(mapped);
                src["versioned_id"] = json!(mapped);
            }
            if eff_id != "scene"
                && eff_id != "group"
                && !eff_id.is_empty()
                && !known_native_kind(eff_id)
                && missing_kinds_seen.insert(eff_id.to_string())
            {
                missing_kinds.push(eff_id.to_string());
            }
        }
    }

    let lookup = AssetLookup::new(&extracted.assets);
    let mut missing_assets = Vec::new();
    let mut missing_seen = BTreeSet::new();
    let mut widgets_out = Vec::new();
    let mut unknown_sources = Vec::new();
    let mut scene_overlays = Vec::new();
    let mut notes = Vec::new();
    let mut overlay_counter = 0usize;

    // Rewrite asset file paths and extract text fonts across sources & transitions.
    if let Some(sources) = doc.get_mut("sources").and_then(Value::as_array_mut) {
        for src in sources.iter_mut() {
            let kind = src.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let sname = src.get("name").and_then(Value::as_str).unwrap_or("Source").to_string();
            if kind.starts_with("text_")
                && let Some(settings) = src.get("settings")
                && let Some(spec) = parse_font_spec(
                    settings.pointer("/font/face").and_then(Value::as_str),
                    settings.get("custom_font").and_then(Value::as_str),
                    settings.pointer("/font/flags").and_then(Value::as_u64).unwrap_or(0),
                    settings.pointer("/font/style").and_then(Value::as_str),
                )
            {
                font_specs.push(spec);
            }
            if let Some(settings) = src.get_mut("settings").and_then(Value::as_object_mut) {
                for key in ["file", "local_file", "path", "image_file", "luma_path", "mask_path"] {
                    if let Some(raw) = settings.get(key).and_then(Value::as_str).map(str::to_string)
                        && !raw.is_empty()
                    {
                        if let Some(idx) = lookup.resolve(&raw) {
                            settings.insert(key.into(), json!(format!("{ASSET_PLACEHOLDER_PREFIX}{idx}")));
                        } else if !raw.starts_with("http://")
                            && !raw.starts_with("https://")
                            && missing_seen.insert(raw.to_ascii_lowercase())
                        {
                            missing_assets.push(raw);
                        }
                    }
                }
                if let Some(url) = settings.get("url").and_then(Value::as_str).map(str::to_string)
                    && (url.contains(".html") && !url.starts_with("http"))
                {
                    unknown_sources.push(UnknownSource {
                        scene: pack_name.clone(),
                        name: sname,
                        kind,
                        reason: "Local HTML overlays are blocked for security; recreate as a Ferrocast overlay.".into(),
                    });
                    settings.insert("url".into(), json!(""));
                }
            }
        }
    }

    if let Some(transitions) = doc.get_mut("transitions").and_then(Value::as_array_mut) {
        for tr in transitions {
            if let Some(settings) = tr.get_mut("settings").and_then(Value::as_object_mut)
                && let Some(raw) = settings.get("path").and_then(Value::as_str).map(str::to_string)
                && !raw.is_empty()
            {
                if let Some(idx) = lookup.resolve(&raw) {
                    settings.insert("path".into(), json!(format!("{ASSET_PLACEHOLDER_PREFIX}{idx}")));
                } else if missing_seen.insert(raw.to_ascii_lowercase()) {
                    missing_assets.push(raw);
                }
            }
        }
    }

    // Check optional OWN3D `widgets` metadata in primary JSON to synthesize Ferrocast Overlays.
    if let Some(raw_widgets) = primary.get("widgets").and_then(Value::as_array) {
        let mut widgets_by_scene: BTreeMap<String, Vec<Widget>> = BTreeMap::new();
        let default_scene = doc.get("current_scene").and_then(Value::as_str).unwrap_or("Main").to_string();
        for w in raw_widgets {
            let scene_name = w
                .get("scene")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&default_scene)
                .to_string();
            let wname = w.get("name").and_then(Value::as_str).unwrap_or("Widget").to_string();
            let wtype = w.get("type").or_else(|| w.get("kind"));
            if let Some((wk, label)) = map_widget_type(wtype, &wname) {
                let list = widgets_by_scene.entry(scene_name.clone()).or_default();
                if list.len() < MAX_WIDGETS {
                    overlay_counter += 1;
                    let wid = format!("own3d-{overlay_counter}");
                    let rect = clamp_widget_box(
                        w.get("x").and_then(Value::as_f64).unwrap_or(40.0),
                        w.get("y").and_then(Value::as_f64).unwrap_or(40.0),
                        1.0,
                        1.0,
                        OverlayCanvas { width: 1920, height: 1080 },
                        wk,
                    );
                    let font_hint = w.get("font").and_then(Value::as_str);
                    if let Some(spec) = parse_font_spec(font_hint, None, 0, None) {
                        font_specs.push(spec);
                    }
                    list.push(Widget::new(
                        &wid,
                        wk,
                        rect,
                        widget_default_config(wk, w.get("template").and_then(Value::as_str), font_hint),
                    ));
                }
                widgets_out.push(PreviewWidget {
                    scene: scene_name,
                    name: wname,
                    source_kind: wtype.and_then(Value::as_str).unwrap_or("widget").into(),
                    mapped_to: Some(wk),
                    overlay_widget_name: label.into(),
                    supported: true,
                });
            } else {
                let k = wtype.and_then(Value::as_str).unwrap_or("unknown").to_string();
                unknown_sources.push(UnknownSource {
                    scene: scene_name.clone(),
                    name: wname.clone(),
                    kind: k.clone(),
                    reason: format!("Unsupported OWN3D widget \"{k}\"; rebuild in Overlays."),
                });
                widgets_out.push(PreviewWidget {
                    scene: scene_name,
                    name: wname,
                    source_kind: k,
                    mapped_to: None,
                    overlay_widget_name: "Not supported".into(),
                    supported: false,
                });
            }
        }
        for (idx, (sc_name, list)) in widgets_by_scene.into_iter().enumerate() {
            let ph_key = format!("{OVERLAY_PLACEHOLDER_PREFIX}own3d-{idx}");
            // A name no source of the pack has (it could have a source called "Main Overlay").
            let taken: BTreeSet<String> = doc
                .get("sources")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter().filter_map(|s| s.get("name").and_then(Value::as_str)).map(str::to_lowercase).collect()
                })
                .unwrap_or_default();
            let mut src_name = format!("{sc_name} Overlay");
            for n in 2.. {
                if !taken.contains(&src_name.to_lowercase()) {
                    break;
                }
                src_name = format!("{sc_name} Overlay ({n})");
            }
            if let Some(sources) = doc.get_mut("sources").and_then(Value::as_array_mut) {
                let mut added_to_scene = false;
                for s in sources.iter_mut() {
                    if s.get("id").and_then(Value::as_str) == Some("scene")
                        && s.get("name").and_then(Value::as_str) == Some(sc_name.as_str())
                        && let Some(items) = s.pointer_mut("/settings/items").and_then(Value::as_array_mut)
                    {
                        let next_id =
                            items.iter().filter_map(|i| i.get("id").and_then(Value::as_i64)).max().unwrap_or(0) + 1;
                        items.push(json!({
                            "id": next_id,
                            "name": src_name,
                            "visible": true,
                            "locked": false,
                            "pos": { "x": 0.0, "y": 0.0 },
                            "scale": { "x": 1.0, "y": 1.0 },
                            "rot": 0.0,
                            "align": 5
                        }));
                        added_to_scene = true;
                        break;
                    }
                }
                if added_to_scene {
                    sources.push(json!({
                        "name": src_name,
                        "id": web_kind,
                        "versioned_id": web_kind,
                        "settings": { "url": ph_key, "width": 1920, "height": 1080 }
                    }));
                    scene_overlays.push(PlannedSceneOverlay {
                        placeholder_key: ph_key,
                        scene_name: sc_name,
                        widgets: list,
                    });
                }
            }
        }
    }

    // The pack's JSON is a stranger's: rebuild it from what is allowed (see overlay_safe).
    let safe = overlay_safe::make_safe(&mut doc, extracted.assets.len(), &|r| lookup.resolve(r));
    for m in safe.missing_assets {
        if missing_seen.insert(m.to_ascii_lowercase()) {
            missing_assets.push(m);
        }
    }
    notes.extend(safe.notes);
    let web_pages = count_web_pages(&doc);
    let source_count = doc
        .get("sources")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|s| !matches!(s.get("id").and_then(Value::as_str), Some("scene" | "group"))).count())
        .unwrap_or(0);

    let scenes: Vec<PreviewScene> = doc
        .get("sources")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|s| s.get("id").and_then(Value::as_str) == Some("scene"))
                .map(|s| {
                    let sname = s.get("name").and_then(Value::as_str).unwrap_or("Scene").to_string();
                    let items = s.pointer("/settings/items").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                    let w_cnt = widgets_out.iter().filter(|w| w.scene == sname && w.supported).count();
                    let u_cnt = unknown_sources.iter().filter(|u| u.scene == sname).count();
                    PreviewScene { name: sname, items, mapped: items, widgets: w_cnt, unknown: u_cnt }
                })
                .collect()
        })
        .unwrap_or_default();
    if scenes.is_empty() {
        return Err(refused(
            422,
            "overlay_no_scenes",
            "This theme pack doesn't contain any scenes.",
            "Pick another .overlay pack.",
        ));
    }

    for miss in &missing_kinds {
        notes.push(format!("Unrecognized source kind \"{miss}\" was left out."));
    }

    let format = if own3d_brand || is_own3d_manifest_json(primary) {
        OverlayPackFormat::Own3d
    } else {
        OverlayPackFormat::ObsBundle
    };

    Ok(ParsedPack {
        suggested_name: pack_name,
        format,
        canvas: OverlayCanvas { width: 1920, height: 1080 },
        scenes,
        widgets: widgets_out,
        scene_overlays,
        unknown_sources,
        missing_assets,
        assets: extracted.assets,
        bundled_fonts: extracted.bundled_fonts,
        font_specs,
        notes,
        doc_template: doc,
        missing_kinds,
        web_page_count: web_pages,
        source_count,
    })
}

fn convert_own3d_scenes_to_obs(manifest: &Value) -> Result<Value, EngineError> {
    let raw_scenes = manifest.get("scenes").and_then(Value::as_array).ok_or_else(|| {
        refused(
            422,
            "invalid_overlay",
            "This .overlay pack has no recognizable scenes list.",
            "Pick a Streamlabs or OWN3D .overlay pack.",
        )
    })?;
    if raw_scenes.is_empty() {
        return Err(refused(
            422,
            "overlay_no_scenes",
            "This theme pack doesn't contain any scenes.",
            "Pick another .overlay pack.",
        ));
    }

    let mut obs_sources = Vec::new();
    let mut scene_order = Vec::new();
    let mut source_defs: Map<String, Value> = Map::new();

    for sc in raw_scenes {
        let sname = sc.get("name").and_then(Value::as_str).unwrap_or("Scene").trim().to_string();
        scene_order.push(json!({ "name": sname }));
        let raw_items = sc
            .get("sources")
            .or_else(|| sc.get("layers"))
            .or_else(|| sc.get("items"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut items = Vec::new();
        for (idx, item) in raw_items.iter().enumerate() {
            let iname = item.get("name").and_then(Value::as_str).unwrap_or("Layer").to_string();
            let raw_kind = item.get("type").or_else(|| item.get("kind")).and_then(Value::as_str).unwrap_or("image");
            let raw_lower = raw_kind.to_ascii_lowercase();
            let (obs_id, settings): (&str, Value) = match raw_lower.as_str() {
                "image" | "image_source" => (
                    "image_source",
                    json!({ "file": item.get("file").or_else(|| item.get("src")).and_then(Value::as_str).unwrap_or("") }),
                ),
                "video" | "media" | "ffmpeg_source" => (
                    "ffmpeg_source",
                    json!({
                        "local_file": item.get("file").or_else(|| item.get("src")).and_then(Value::as_str).unwrap_or(""),
                        "is_local_file": true,
                        "looping": item.get("loop").and_then(Value::as_bool).unwrap_or(true),
                    }),
                ),
                "text" | "text_ft2_source_v2" | "text_gdiplus" => {
                    let font_face = item.get("font").and_then(Value::as_str).unwrap_or("Sans");
                    (
                        "text_ft2_source_v2",
                        json!({
                            "text": item.get("text").and_then(Value::as_str).unwrap_or(""),
                            "font": { "face": font_face, "size": item.get("size").and_then(Value::as_u64).unwrap_or(42) }
                        }),
                    )
                }
                "camera" | "webcam" => ("dshow_input", json!({})),
                "browser" | "browser_source" | "web" => {
                    ("browser_source", json!({ "url": item.get("url").and_then(Value::as_str).unwrap_or("") }))
                }
                _ => (raw_kind, item.get("settings").cloned().unwrap_or_else(|| json!({}))),
            };
            source_defs.entry(iname.clone()).or_insert_with(|| {
                json!({
                    "name": iname,
                    "id": obs_id,
                    "versioned_id": obs_id,
                    "settings": settings,
                })
            });
            items.push(json!({
                "id": (idx + 1) as i64,
                "name": iname,
                "visible": item.get("visible").and_then(Value::as_bool).unwrap_or(true),
                "locked": false,
                "pos": {
                    "x": item.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    "y": item.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                },
                "scale": { "x": 1.0, "y": 1.0 },
                "rot": 0.0,
                "align": 5
            }));
        }
        obs_sources.push(json!({
            "name": sname,
            "id": "scene",
            "versioned_id": "scene",
            "settings": { "id_counter": items.len() as i64, "items": items }
        }));
    }

    let first_scene = scene_order.first().and_then(|s| s["name"].as_str()).unwrap_or("Scene").to_string();
    obs_sources.extend(source_defs.into_values());
    Ok(json!({
        "name": manifest.get("name").and_then(Value::as_str).unwrap_or("OWN3D Pack"),
        "current_scene": first_scene,
        "current_program_scene": first_scene,
        "scene_order": scene_order,
        "transitions": [{ "name": "Fade", "id": "fade_transition", "settings": {}, "duration": 300 }],
        "sources": obs_sources
    }))
}

/// Safely unpacks, audits, parses, and maps an uploaded `.overlay` zip archive into memory.
pub fn parse_overlay_zip(zip_bytes: &[u8], filename: &str) -> Result<ParsedPack, EngineError> {
    let extracted = extract_zip(zip_bytes)?;
    let (hint_name, own3d_brand, manifest_fonts) = detect_manifest_hints(&extracted.json_files);
    let primary = select_primary_json(&extracted.json_files)
        .ok_or_else(|| {
            refused(
                422,
                "invalid_overlay",
                "This .overlay pack doesn't contain a Streamlabs or OWN3D scene collection JSON.",
                "Export the pack again from Streamlabs or OWN3D.",
            )
        })?
        .1
        .clone();

    if is_root_node(&primary) {
        parse_streamlabs_root(&primary, extracted, filename, hint_name, own3d_brand, manifest_fonts)
    } else {
        parse_obs_or_own3d_pack(&primary, extracted, filename, hint_name, own3d_brand, manifest_fonts)
    }
}

/// How many browser sources a collection holds.
fn count_web_pages(doc: &Value) -> usize {
    doc.get("sources")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|s| matches!(s.get("id").and_then(Value::as_str), Some("browser_source" | "ferrocast_web")))
                .count()
        })
        .unwrap_or(0)
}

/// Replaces asset and overlay placeholders inside `doc_template` after assets live on disk and
/// Ferrocast overlays have been created.
///
/// A placeholder is filled only where a file or an overlay's address belongs (a setting that
/// names a file, a slideshow's files, a browser source's `url`). A pack can write the same words
/// anywhere (a text source, a name); there they stay words, so the overlay's secret address and
/// this computer's folders never end up in something a viewer can read.
pub fn finalize_doc(doc_template: &Value, asset_paths: &[String], overlay_urls: &BTreeMap<String, String>) -> Value {
    /// Settings that hold a file of the pack.
    const FILE_KEYS: [&str; 9] =
        ["file", "local_file", "path", "image_file", "luma_path", "mask_path", "image_path", "clut_path", "value"];

    fn walk(
        v: &Value,
        key: Option<&str>,
        in_files: bool,
        asset_paths: &[String],
        overlay_urls: &BTreeMap<String, String>,
    ) -> Value {
        match v {
            Value::String(s) => {
                // `value` is a file only inside a slideshow's `files`.
                let file_here = key.is_some_and(|k| FILE_KEYS.contains(&k) && (k != "value" || in_files));
                if file_here
                    && let Some(rest) = s.strip_prefix(ASSET_PLACEHOLDER_PREFIX)
                    && let Ok(idx) = rest.parse::<usize>()
                    && let Some(path) = asset_paths.get(idx)
                {
                    return Value::String(path.clone());
                }
                if key == Some("url")
                    && s.starts_with(OVERLAY_PLACEHOLDER_PREFIX)
                    && let Some(url) = overlay_urls.get(s)
                {
                    return Value::String(url.clone());
                }
                Value::String(s.clone())
            }
            Value::Array(arr) => Value::Array(
                arr.iter().map(|i| walk(i, key, in_files || key == Some("files"), asset_paths, overlay_urls)).collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, val)| (k.clone(), walk(val, Some(k), in_files, asset_paths, overlay_urls)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut out = walk(doc_template, None, false, asset_paths, overlay_urls);
    drop_unmade_overlay_sources(&mut out);
    out
}

/// A browser source whose overlay wasn't made (the option was off, or making it failed) would
/// keep its placeholder as an address. It is left out, with its scene items.
fn drop_unmade_overlay_sources(doc: &mut Value) {
    let Some(list) = doc.get_mut("sources").and_then(Value::as_array_mut) else { return };
    let unmade: BTreeSet<String> = list
        .iter()
        .filter(|s| {
            s.pointer("/settings/url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with(OVERLAY_PLACEHOLDER_PREFIX))
        })
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    if unmade.is_empty() {
        return;
    }
    list.retain(|s| !s.get("name").and_then(Value::as_str).is_some_and(|n| unmade.contains(n)));
    for s in list.iter_mut() {
        if let Some(items) = s.pointer_mut("/settings/items").and_then(Value::as_array_mut) {
            items.retain(|i| !i.get("name").and_then(Value::as_str).is_some_and(|n| unmade.contains(n)));
        }
    }
}
