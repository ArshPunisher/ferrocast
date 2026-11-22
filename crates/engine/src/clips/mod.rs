//! Clips and highlights (Phase 3 #23): the video files Ferrocast made for the streamer (replay
//! saves, Moments clips, vertical copies, recordings) as a library to browse, trim, rename, delete,
//! caption, put together and send to YouTube.
//!
//! Safety, all of it in this file's [`resolve`]: a clip is named by its file name in the clips
//! folder, never a path; the name can't hold a separator, `..` or a control character; the file
//! must be a plain file (never a link) of a known video kind that sits in that folder itself.
//! ffmpeg only ever reads such files, always with `-protocol_whitelist file`, and writes to a
//! `.part` name that is renamed when done and never over a file that is there. Nothing here
//! touches a live output: the clips are files, and the work runs in the background.

pub mod edit;
pub mod jobs;
pub mod upload;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::{Engine, EngineError};

/// File kinds a clip can be.
const EXTS: [&str; 3] = ["mp4", "mov", "mkv"];
/// A name this long or longer is refused (file systems stop at 255 bytes).
const MAX_NAME: usize = 120;
/// At most this many clips are listed.
const MAX_LIST: usize = 2000;
/// ffmpeg reading a clip's facts or taking its picture mustn't hang.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

/// Where a clip came from, by its name (what Ferrocast calls the files it makes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    /// A recording.
    Recording,
    /// A replay buffer save ("Replay …").
    Replay,
    /// A vertical copy ("… vertical").
    Vertical,
    /// Something made here: a trim, a compilation, a captioned copy.
    Edit,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClipEntry {
    /// The file name: what every other call names it by.
    pub id: String,
    /// The file name without its extension.
    pub name: String,
    pub kind: ClipKind,
    pub ext: String,
    pub size: u64,
    /// When the file was last written (Unix ms).
    pub modified: i64,
    /// A subtitle file ("<name>.srt") is beside it.
    pub has_captions: bool,
}

/// The clips and what can be done with them now.
#[derive(Debug, Clone, Serialize)]
pub struct ClipsOverview {
    pub folder: String,
    /// FFmpeg is here (trims, compilations, pictures and captions need it).
    pub ffmpeg_ready: bool,
    /// YouTube is connected (sending a clip needs it).
    pub youtube_connected: bool,
    pub clips: Vec<ClipEntry>,
}

/// What ffmpeg says about a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub duration_ms: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub has_audio: bool,
    pub has_video: bool,
}

fn refuse(status: u16, code: &'static str, message: impl Into<String>, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

pub fn no_such_clip() -> EngineError {
    refuse(
        404,
        "clip_not_found",
        "That clip isn't in the folder any more.",
        "Reload the list; it may have been moved or deleted.",
    )
}

pub(crate) fn name_taken() -> EngineError {
    refuse(409, "clip_name_taken", "A clip with that name is already there.", "Pick another name.")
}

/// A file name that is one plain name: no separator, no `..`, no control character, not hidden,
/// not a Windows device, no trailing dot or space, no `:` (drive or stream). Used for the id of a
/// clip and for the name given to a new one.
pub fn plain_name(raw: &str) -> Result<&str, EngineError> {
    let bad = |why: &str| {
        refuse(
            400,
            "bad_clip_name",
            format!("That isn't a usable file name: {why}."),
            "Use letters, numbers, spaces, - and _.",
        )
    };
    let n = raw.trim_matches(' ');
    if n.is_empty() || n.chars().count() > MAX_NAME + 8 {
        return Err(bad("it is empty or too long"));
    }
    if n != raw || n.starts_with('.') || n.ends_with('.') {
        return Err(bad("no space or dot at the ends"));
    }
    if n.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) || n.contains("..") || n.chars().any(char::is_control)
    {
        return Err(bad("it has a character a file name can't have"));
    }
    let stem = n.split('.').next().unwrap_or(n).to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem.ends_with(|c: char| matches!(c, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}')));
    if device {
        return Err(bad("it is a name Windows keeps"));
    }
    Ok(n)
}

/// A new clip's name, without its extension.
pub fn new_stem(raw: &str) -> Result<String, EngineError> {
    let n = plain_name(raw)?;
    if n.chars().count() > MAX_NAME {
        return Err(refuse(
            400,
            "bad_clip_name",
            format!("A name can be up to {MAX_NAME} characters."),
            "Use a shorter name.",
        ));
    }
    Ok(n.to_string())
}

fn ext_of(name: &str) -> Option<&'static str> {
    let e = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    EXTS.iter().copied().find(|x| *x == e)
}

/// The clip named `id` in `root`: a plain file there, never a link.
pub fn resolve(root: &Path, id: &str) -> Result<PathBuf, EngineError> {
    plain_name(id).map_err(|_| no_such_clip())?;
    if ext_of(id).is_none() {
        return Err(no_such_clip());
    }
    let path = root.join(id);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| no_such_clip())?;
    if !meta.file_type().is_file() {
        return Err(no_such_clip());
    }
    // Its folder is the clips folder itself (a link in the path would make this differ).
    let (here, there) = (path.parent().and_then(|p| p.canonicalize().ok()), root.canonicalize().ok());
    if here.is_none() || here != there {
        return Err(no_such_clip());
    }
    Ok(path)
}

fn kind_of(stem: &str) -> ClipKind {
    let s = stem.to_ascii_lowercase();
    if s.starts_with("replay") {
        ClipKind::Replay
    } else if s.ends_with(" vertical") {
        ClipKind::Vertical
    } else if s.starts_with("compilation") || s.ends_with(" trimmed") || s.ends_with(" captioned") {
        ClipKind::Edit
    } else {
        ClipKind::Recording
    }
}

fn ms(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The clips in `root`, newest first (links, hidden files, half-written ones and other file
/// kinds are left out), the ones whose name has `query` in it (any case) when given.
pub fn list_in(root: &Path, query: Option<&str>) -> Vec<ClipEntry> {
    let q = query.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<ClipEntry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.starts_with('.') || plain_name(&name).is_err() {
                return None;
            }
            let ext = ext_of(&name)?;
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            if !meta.file_type().is_file() {
                return None;
            }
            let stem = name[..name.len() - ext.len() - 1].to_string();
            if q.as_ref().is_some_and(|q| !stem.to_lowercase().contains(q)) {
                return None;
            }
            Some(ClipEntry {
                kind: kind_of(&stem),
                has_captions: srt_beside(&root.join(&name)).is_file(),
                id: name,
                name: stem,
                ext: ext.into(),
                size: meta.len(),
                modified: meta.modified().map_or(0, ms),
            })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out.truncate(MAX_LIST);
    out
}

/// The subtitle file beside a clip.
pub fn srt_beside(clip: &Path) -> PathBuf {
    clip.with_extension("srt")
}

/// `ffmpeg -i` reading only files: its stderr says the duration, the size and the streams.
fn probe_args(path: &Path) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> =
        ["-hide_banner", "-nostdin", "-protocol_whitelist", "file", "-i"].iter().map(Into::into).collect();
    a.push(path.as_os_str().to_owned());
    a
}

/// Reads ffmpeg's text about a file.
pub fn parse_probe(stderr: &str) -> Probe {
    let mut p = Probe::default();
    for line in stderr.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Duration:") {
            let t = rest.trim().split(',').next().unwrap_or("").trim();
            let parts: Vec<&str> = t.split(':').collect();
            if let [h, m, s] = parts.as_slice()
                && let (Ok(h), Ok(m), Ok(s)) = (h.parse::<u64>(), m.parse::<u64>(), s.parse::<f64>())
            {
                p.duration_ms = Some(h * 3_600_000 + m * 60_000 + (s * 1000.0).round() as u64);
            }
        } else if l.contains("Video:") && l.contains("Stream #") && !l.contains("attached pic") {
            p.has_video = true;
            // "…, 1920x1080 [SAR 1:1 DAR 16:9], 30 fps…": the first WxH.
            for tok in l.split([' ', ',']) {
                if let Some((w, h)) = tok.split_once('x')
                    && let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>())
                    && w >= 16
                    && h >= 16
                    && p.width.is_none()
                {
                    p.width = Some(w);
                    p.height = Some(h);
                }
            }
        } else if l.contains("Audio:") && l.contains("Stream #") {
            p.has_audio = true;
        }
    }
    p
}

impl Engine {
    /// The folder the clips are in: where recordings go.
    pub async fn clips_folder(&self) -> Result<PathBuf, EngineError> {
        if let Some(p) = self.studio().clips.folder_override.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(p);
        }
        if let Ok(host) = self.studio_host() {
            let s = host.settings().await;
            let chosen = s.record_folder.filter(|f| !f.trim().is_empty()).map(PathBuf::from);
            let default = (!s.record_folder_default.is_empty()).then(|| PathBuf::from(&s.record_folder_default));
            if let Some(p) = chosen.or(default) {
                return Ok(p);
            }
        }
        Ok(self.data_dir().join("Clips"))
    }

    /// For tests: look for clips here.
    pub fn set_clips_folder_for_test(&self, folder: Option<PathBuf>) {
        *self.studio().clips.folder_override.lock().unwrap_or_else(|p| p.into_inner()) = folder;
    }

    /// For tests: "the Trash" is this folder.
    pub fn set_trash_folder_for_test(&self, folder: Option<PathBuf>) {
        *self.studio().clips.trash_override.lock().unwrap_or_else(|p| p.into_inner()) = folder;
    }

    /// The clips, newest first.
    pub async fn clips(&self, query: Option<&str>) -> Result<Vec<ClipEntry>, EngineError> {
        let root = self.clips_folder().await?;
        let q = query.map(String::from);
        tokio::task::spawn_blocking(move || list_in(&root, q.as_deref()))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// The library and what can be done with it now.
    pub async fn clips_overview(&self, query: Option<&str>) -> Result<ClipsOverview, EngineError> {
        Ok(ClipsOverview {
            folder: self.clips_folder().await?.display().to_string(),
            ffmpeg_ready: self.ffmpeg_path().is_some(),
            youtube_connected: self.connector(ferrocast_core::event::Platform::Youtube).is_some(),
            clips: self.clips(query).await?,
        })
    }

    /// The file of a clip, for playing it in the dashboard (the name is checked like any other)
    /// → its path and the media type to send it as.
    pub async fn clip_file(&self, id: &str) -> Result<(PathBuf, &'static str), EngineError> {
        let path = self.clip_path(id).await?;
        let mime = match ext_of(id) {
            Some("mov") => "video/quicktime",
            Some("mkv") => "video/x-matroska",
            _ => "video/mp4",
        };
        Ok((path, mime))
    }

    /// A clip's picture as a data URL (what the dashboard shows in a tile).
    pub async fn clip_thumbnail_image(&self, id: &str) -> Result<String, EngineError> {
        use base64::Engine as _;
        let jpg = self.clip_thumbnail(id).await?;
        Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpg)))
    }

    pub(crate) async fn clip_path(&self, id: &str) -> Result<PathBuf, EngineError> {
        let root = self.clips_folder().await?;
        let id = id.to_string();
        tokio::task::spawn_blocking(move || resolve(&root, &id))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?
    }

    pub(crate) fn clips_ffmpeg(&self) -> Result<PathBuf, EngineError> {
        self.ffmpeg_path().ok_or_else(|| {
            refuse(
                422,
                "ffmpeg_missing",
                "Ferrocast needs FFmpeg for this.",
                "Get FFmpeg in Settings (it's free), then try again.",
            )
        })
    }

    /// What ffmpeg says about a clip: how long, how big, whether it has sound.
    pub async fn clip_probe(&self, id: &str) -> Result<Probe, EngineError> {
        let path = self.clip_path(id).await?;
        let ffmpeg = self.clips_ffmpeg()?;
        probe_file(&ffmpeg, &path).await
    }

    /// A small picture from a clip: a JPEG, kept for next time.
    pub async fn clip_thumbnail(&self, id: &str) -> Result<Vec<u8>, EngineError> {
        let path = self.clip_path(id).await?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| no_such_clip())?;
        let key = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(id.as_bytes());
            h.update(meta.len().to_le_bytes());
            h.update(meta.modified().map_or(0, ms).to_le_bytes());
            h.finalize().iter().take(10).map(|b| format!("{b:02x}")).collect::<String>()
        };
        let dir = self.data_dir().join("clip-thumbs");
        let file = dir.join(format!("{key}.jpg"));
        if let Ok(bytes) = tokio::fs::read(&file).await {
            return Ok(bytes);
        }
        let ffmpeg = self.clips_ffmpeg()?;
        let probe = probe_file(&ffmpeg, &path).await.unwrap_or_default();
        // A second in (the first frame is often black), unless the clip is shorter.
        let at = if probe.duration_ms.unwrap_or(0) > 2500 { "1" } else { "0" };
        tokio::fs::create_dir_all(&dir).await.map_err(|e| EngineError::Internal(e.to_string()))?;
        let part = dir.join(format!("{key}.part.jpg"));
        let mut cmd = crate::clips::jobs::hidden(&ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-protocol_whitelist",
            "file",
            "-ss",
            at,
            "-i",
        ])
        .arg(&path)
        .args(["-frames:v", "1", "-vf", "scale=320:-2", "-q:v", "5"])
        .arg(&part)
        .kill_on_drop(true);
        let ok = matches!(tokio::time::timeout(PROBE_LIMIT, cmd.output()).await, Ok(Ok(o)) if o.status.success());
        if !ok || tokio::fs::rename(&part, &file).await.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(refuse(
                422,
                "no_thumbnail",
                "Couldn't take a picture from that clip.",
                "The file may be damaged or still being written.",
            ));
        }
        tokio::fs::read(&file).await.map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// Gives a clip another name (and its subtitle file the same), never over another file.
    pub async fn rename_clip(&self, id: &str, new_name: &str) -> Result<ClipEntry, EngineError> {
        let stem = new_stem(new_name)?;
        let root = self.clips_folder().await?;
        let path = self.clip_path(id).await?;
        let ext = ext_of(id).unwrap_or("mp4");
        let new_id = format!("{stem}.{ext}");
        let to = root.join(&new_id);
        let from_srt = srt_beside(&path);
        let to_srt = srt_beside(&to);
        tokio::task::spawn_blocking(move || -> Result<(), EngineError> {
            if new_id_equal(&path, &to) {
                return Ok(());
            }
            move_without_replacing(&path, &to)?;
            if from_srt.is_file() && !to_srt.exists() {
                let _ = move_without_replacing(&from_srt, &to_srt);
            }
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        self.clips(None).await?.into_iter().find(|c| c.id == format!("{stem}.{ext}")).ok_or_else(no_such_clip)
    }

    /// Moves a clip (and its subtitle file) to the Trash: never gone for good from here.
    pub async fn delete_clip(&self, id: &str) -> Result<(), EngineError> {
        let path = self.clip_path(id).await?;
        let srt = srt_beside(&path);
        let over = self.studio().clips.trash_override.lock().unwrap_or_else(|p| p.into_inner()).clone();
        tokio::task::spawn_blocking(move || -> Result<(), EngineError> {
            trash_file(&path, over.as_deref())?;
            if srt.is_file() {
                let _ = trash_file(&srt, over.as_deref());
            }
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))?
    }
}

fn new_id_equal(a: &Path, b: &Path) -> bool {
    a == b
}

/// Renames without replacing: the new name appears only if nothing is there (a hard link then
/// the old name removed, which fails when the name is taken).
pub(crate) fn move_without_replacing(from: &Path, to: &Path) -> Result<(), EngineError> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from).map_err(|e| EngineError::Internal(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(name_taken()),
        // A file system with no links (some network or exFAT shares): check, then rename.
        Err(_) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(name_taken());
            }
            std::fs::rename(from, to).map_err(|e| EngineError::Internal(e.to_string()))
        }
    }
}

/// ffmpeg's text about a file, read.
pub(crate) async fn probe_file(ffmpeg: &Path, path: &Path) -> Result<Probe, EngineError> {
    let mut cmd = jobs::hidden(ffmpeg);
    cmd.args(probe_args(path)).kill_on_drop(true);
    match tokio::time::timeout(PROBE_LIMIT, cmd.output()).await {
        // ffmpeg exits 1 ("at least one output file must be specified"); the facts are on stderr.
        Ok(Ok(o)) => {
            let p = parse_probe(&String::from_utf8_lossy(&o.stderr));
            if p.has_video || p.has_audio {
                Ok(p)
            } else {
                Err(refuse(
                    422,
                    "not_a_clip",
                    "That file isn't a video ffmpeg can read.",
                    "It may be damaged or still being written.",
                ))
            }
        }
        Ok(Err(e)) => {
            Err(refuse(422, "ffmpeg_failed", format!("FFmpeg didn't start: {e}."), "Check FFmpeg in Settings."))
        }
        Err(_) => Err(refuse(422, "ffmpeg_slow", "Reading that clip took too long.", "Try again in a moment.")),
    }
}

/// Puts a file in the Trash. `over`: a folder to use as the Trash (tests).
pub(crate) fn trash_file(path: &Path, over: Option<&Path>) -> Result<(), EngineError> {
    let fail = |e: String| {
        refuse(
            422,
            "trash_unavailable",
            format!("Couldn't move that to the Trash: {e}"),
            "Check the Trash is available, or delete it in the Finder or Explorer.",
        )
    };
    if let Some(dir) = over {
        return move_into(path, dir).map_err(|e| fail(e.to_string()));
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| fail("no home folder".into()))?;
        move_into(path, &home.join(".Trash")).map_err(|e| fail(e.to_string()))
    }
    #[cfg(target_os = "windows")]
    {
        // The Recycle Bin, through the shell's own call.
        let script = "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($args[0], 'OnlyErrorDialogs', 'SendToRecycleBin')";
        let ok = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok { Ok(()) } else { Err(fail("the Recycle Bin didn't take it".into())) }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok_or_else(|| fail("no home folder".into()))?;
        move_into(path, &base.join("Trash/files")).map_err(|e| fail(e.to_string()))
    }
}

/// Moves `path` into `dir` under a name nothing there has ("name 2.mp4"…).
fn move_into(path: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "clip".into());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.clone(), String::new()),
    };
    for n in 1..1000 {
        let candidate = if n == 1 { name.clone() } else { format!("{stem} {n}{ext}") };
        let to = dir.join(&candidate);
        match std::fs::hard_link(path, &to) {
            Ok(()) => return std::fs::remove_file(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // No links across disks or on some shares: a rename, which keeps a name nothing has.
            Err(_) => {
                if std::fs::symlink_metadata(&to).is_ok() {
                    continue;
                }
                return std::fs::rename(path, &to).or_else(|_| {
                    std::fs::copy(path, &to)?;
                    std::fs::remove_file(path)
                });
            }
        }
    }
    Err(std::io::Error::other("too many files with that name in the Trash"))
}

/// The state the clips keep.
#[derive(Default, Debug)]
pub(crate) struct ClipsState {
    pub folder_override: std::sync::Mutex<Option<PathBuf>>,
    pub trash_override: std::sync::Mutex<Option<PathBuf>>,
    pub jobs: jobs::Jobs,
}

#[cfg(test)]
mod tests;
