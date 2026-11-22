//! Sending a clip to the streamer's own YouTube channel, through the connection already made in
//! Settings (nothing new to sign in to; `youtube.force-ssl` already allows `videos.insert`).
//! A normal video or a Short. The clip stays where it is; the upload is a background job with
//! progress that can be cancelled. YouTube counts it against the app's upload allowance, 100 a day by
//! default (`videos.insert` has its own "Video Uploads" quota bucket, 1 unit a call, apart from the
//! 10,000 units other calls share; the Data API's own page, checked 22 Nov 2026).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrocast_connector::{ConnectorError, UploadSource, VideoUpload};
use ferrocast_core::event::Platform;
use serde::Deserialize;

use super::jobs::{JobKind, JobView, Made};
use super::{probe_file, refuse};
use crate::{Engine, EngineError};

/// Uploads an app may make a day by default.
pub const UPLOADS_PER_DAY: u32 = 100;
/// A Short is square or tall and at most this long (YouTube raised it from 60 s to 3 minutes in
/// October 2024).
pub const SHORTS_MAX_MS: u64 = 180_000;
/// The size of a piece sent to YouTube (the youtube crate's own).
const PIECE: u64 = 8 * 1024 * 1024;
const MAX_TITLE: usize = 100;
const MAX_DESCRIPTION_BYTES: usize = 5000;
const MAX_TAGS: usize = 30;
const MAX_TAG_CHARS: usize = 60;
const MAX_TAGS_TOTAL: usize = 450;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadReq {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// "private" (the default: nothing is public by accident), "unlisted" or "public".
    #[serde(default = "private")]
    pub privacy: String,
    /// A YouTube video category's number.
    #[serde(default)]
    pub category_id: Option<String>,
    /// YouTube asks every upload: is it made for children?
    #[serde(default)]
    pub made_for_kids: bool,
    /// Send it as a Short (square or tall, at most 3 minutes; "#Shorts" is added).
    #[serde(default)]
    pub shorts: bool,
}

fn private() -> String {
    "private".into()
}

fn bad(m: impl Into<String>, hint: &str) -> EngineError {
    refuse(400, "bad_upload", m, hint)
}

fn no_markup(s: &str, what: &str) -> Result<(), EngineError> {
    if s.contains(['<', '>']) {
        return Err(bad(format!("{what} can't have < or > in it."), "YouTube doesn't accept those; take them out."));
    }
    Ok(())
}

/// The request checked and tidied.
pub fn check(req: &UploadReq) -> Result<UploadReq, EngineError> {
    let title = req.title.trim().to_string();
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return Err(bad(format!("The title needs 1 to {MAX_TITLE} characters."), "Shorten the title."));
    }
    if title.chars().any(char::is_control) {
        return Err(bad("The title has a character that can't be in it.", "Use plain text."));
    }
    no_markup(&title, "The title")?;
    // A Short gets "\n\n#Shorts" added after this check: room is kept for it.
    let room = if req.shorts
        && !req.description.to_lowercase().contains("#shorts")
        && !title.to_lowercase().contains("#shorts")
    {
        SHORTS_TAG_BYTES
    } else {
        0
    };
    if req.description.len() + room > MAX_DESCRIPTION_BYTES {
        return Err(bad(format!("The description can be up to {MAX_DESCRIPTION_BYTES} characters."), "Shorten it."));
    }
    if req.description.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(bad("The description has a character that can't be in it.", "Use plain text."));
    }
    no_markup(&req.description, "The description")?;
    if !matches!(req.privacy.as_str(), "private" | "unlisted" | "public") {
        return Err(bad("Privacy is private, unlisted or public.", "Pick one of the three."));
    }
    if let Some(c) = &req.category_id
        && (c.is_empty() || c.len() > 3 || !c.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad("That isn't a YouTube category.", "Pick one from the list."));
    }
    let mut tags = Vec::new();
    for t in &req.tags {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if t.chars().count() > MAX_TAG_CHARS || t.chars().any(char::is_control) || t.contains([',', '<', '>']) {
            return Err(bad(
                format!("A tag can be up to {MAX_TAG_CHARS} characters, without commas or < >."),
                "Fix that tag.",
            ));
        }
        tags.push(t.to_string());
    }
    if tags.len() > MAX_TAGS || tags.iter().map(|t| t.chars().count() + 1).sum::<usize>() > MAX_TAGS_TOTAL {
        return Err(bad("There are too many tags.", "Keep a few of the best."));
    }
    Ok(UploadReq { title, tags, ..req.clone() })
}

/// What "\n\n#Shorts" adds to a description.
const SHORTS_TAG_BYTES: usize = 9;

/// Whether text is shaped like a YouTube video id (what goes into a link).
fn video_id_ok(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The details a Short is sent with: "#Shorts" is in the title or description.
fn with_shorts_tag(mut meta: VideoUpload) -> VideoUpload {
    let has = |s: &str| s.to_lowercase().contains("#shorts");
    if !has(&meta.title) && !has(&meta.description) {
        meta.description = if meta.description.trim().is_empty() {
            "#Shorts".into()
        } else {
            format!("{}\n\n#Shorts", meta.description.trim_end())
        };
    }
    meta
}

/// A file read piece by piece.
struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl FileSource {
    /// Whether `path` is still the file held open.
    fn is_same_as(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            matches!((self.file.metadata(), std::fs::metadata(path)), (Ok(a), Ok(b)) if a.dev() == b.dev() && a.ino() == b.ino())
        }
        #[cfg(not(unix))]
        {
            matches!(
                (self.file.metadata().and_then(|m| m.modified()), std::fs::metadata(path).and_then(|m| m.modified())),
                (Ok(a), Ok(b)) if a == b
            ) && std::fs::metadata(path).is_ok_and(|m| m.len() == self.len)
        }
    }

    fn open(path: &Path) -> Result<Self, EngineError> {
        let file = std::fs::File::open(path).map_err(|e| {
            refuse(
                422,
                "clip_unreadable",
                format!("That clip couldn't be opened: {e}."),
                "Check it isn't open in another app.",
            )
        })?;
        let meta = file.metadata().map_err(|e| EngineError::Internal(e.to_string()))?;
        if !meta.is_file() {
            return Err(super::no_such_clip());
        }
        Ok(Self { file, len: meta.len() })
    }
}

impl UploadSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = self.file.read(&mut buf[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    }
}

/// How a platform's trouble reads to the streamer.
fn describe(e: ConnectorError) -> EngineError {
    match e {
        ConnectorError::RateLimited { retry_after, .. } => refuse(
            429,
            "youtube_quota",
            "YouTube's daily limit for uploads is used up.",
            &format!(
                "It resets after midnight Pacific time (in about {} hours). Try again then.",
                (retry_after.as_secs() / 3600).max(1)
            ),
        ),
        ConnectorError::SignInNeeded(_) => refuse(
            401,
            "youtube_sign_in",
            "YouTube needs you to sign in again.",
            "Open Settings, Accounts, and connect YouTube.",
        ),
        ConnectorError::NotSupported { .. } | ConnectorError::NotConnected(_) => refuse(
            422,
            "youtube_not_connected",
            "YouTube isn't connected.",
            "Open Settings, Accounts, and connect YouTube.",
        ),
        ConnectorError::Network { message, .. } => refuse(
            502,
            "youtube_unreachable",
            format!("Couldn't reach YouTube: {message}."),
            "Check your connection and try again; the file is untouched.",
        ),
        ConnectorError::Refused { message, .. } => {
            refuse(422, "youtube_refused", format!("YouTube said no: {message}."), "Check the details and try again.")
        }
        other => EngineError::Connector(other),
    }
}

impl Engine {
    /// Sends a clip to YouTube → the job (its `link` is the video's address when done).
    pub async fn upload_clip(&self, id: &str, req: UploadReq) -> Result<JobView, EngineError> {
        let req = check(&req)?;
        let path = self.clip_path(id).await?;
        // The file picked now is the file sent: it is held open from here, so a clip renamed or
        // replaced by another of the same name while the job waits is not sent under these details.
        let mut source = {
            let p = path.clone();
            tokio::task::spawn_blocking(move || FileSource::open(&p))
                .await
                .map_err(|e| EngineError::Internal(e.to_string()))??
        };
        let yt = self
            .connector(Platform::Youtube)
            .ok_or_else(|| describe(ConnectorError::NotConnected(Platform::Youtube)))?;
        if req.shorts {
            let ffmpeg = self.clips_ffmpeg()?;
            let p = probe_file(&ffmpeg, &path).await?;
            let tall_enough = p.width.zip(p.height).is_some_and(|(w, h)| h >= w);
            if !tall_enough {
                return Err(bad("A Short is square or tall.", "Make a vertical copy first, then send that."));
            }
            if p.duration_ms.is_none_or(|d| d > SHORTS_MAX_MS) {
                return Err(bad("A Short is at most 3 minutes.", "Trim the clip first."));
            }
            // What was checked is what will be sent: the path still names the file held open.
            if !source.is_same_as(&path) {
                return Err(refuse(409, "clip_changed", "That clip changed while it was being checked.", "Try again."));
            }
        }
        let shorts = req.shorts;
        let mut meta = VideoUpload {
            title: req.title,
            description: req.description,
            tags: req.tags,
            privacy: req.privacy,
            category_id: req.category_id,
            made_for_kids: req.made_for_kids,
        };
        if shorts {
            meta = with_shorts_tag(meta);
        }
        let label =
            format!("Upload {}", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(self.start_clip_job(JobKind::Upload, label, move |ctx| async move {
            let c2 = ctx.clone();
            // Whether the last piece is on its way (or the only one): from then on a stop can't tell
            // whether YouTube has the video.
            let last_piece = Arc::new(AtomicBool::new(false));
            let lp = last_piece.clone();
            let progress = move |sent: u64, total: u64| {
                if total.saturating_sub(sent) <= PIECE {
                    lp.store(true, Ordering::SeqCst);
                }
                c2.progress(if total == 0 { 0.0 } else { sent as f32 / total as f32 });
            };
            let sent = tokio::select! {
                r = yt.upload_video(&meta, &mut source, &progress) => r.map_err(describe)?,
                // Stopped while a piece was on its way: if it was the last, YouTube may have the
                // video already, and nothing here can know.
                () = ctx.cancel.wait() => {
                    return Err(if last_piece.load(Ordering::SeqCst) {
                        super::jobs::cancelled_with(
                            "Stopped near the end.",
                            "YouTube may already have the video. Check YouTube Studio before sending it again.",
                        )
                    } else {
                        super::jobs::cancelled()
                    });
                }
            };
            if !video_id_ok(&sent) {
                return Err(EngineError::Internal("YouTube sent an id that isn't a video id".into()));
            }
            let link = if shorts {
                format!("https://www.youtube.com/shorts/{sent}")
            } else {
                format!("https://www.youtube.com/watch?v={sent}")
            };
            ctx.link(&link);
            Ok(Made { output: None })
        }))
    }
}

#[cfg(test)]
mod tests;
