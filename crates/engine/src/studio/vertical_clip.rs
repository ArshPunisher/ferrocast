//! A vertical copy of each saved clip (plan item 6, 29 Oct): 1080×1920 for Shorts, TikTok and
//! Reels, the clip whole in the middle over a blurred, zoomed copy of itself (no part of the
//! picture is cut off). Made by ffmpeg in the background, next to the clip, as "<name>
//! vertical.mp4"; nothing is sent anywhere.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::Engine;

/// Longer than any clip takes; a stuck ffmpeg is stopped and its half-written file removed.
const LIMIT: Duration = Duration::from_secs(300);

/// Where the vertical copy of `clip` goes: beside it, " vertical" added to its name.
pub fn vertical_path(clip: &Path) -> Option<PathBuf> {
    let stem = clip.file_stem()?.to_str()?;
    Some(clip.with_file_name(format!("{stem} vertical.mp4")))
}

/// ffmpeg's arguments: blurred background filling 1080×1920, the clip scaled to the width in
/// the middle, the sound copied as it is. Paths go as arguments, never through a shell.
pub fn vertical_args(clip: &Path, out: &Path) -> Vec<OsString> {
    let filter = "[0:v]split=2[bg][fg];\
        [bg]scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,boxblur=20:2[back];\
        [fg]scale=1080:-2[front];\
        [back][front]overlay=(W-w)/2:(H-h)/2,format=yuv420p[v]";
    let mut a: Vec<OsString> =
        ["-hide_banner", "-loglevel", "error", "-nostdin", "-n", "-i"].iter().map(OsString::from).collect();
    a.push(clip.as_os_str().to_owned());
    for s in [
        "-filter_complex",
        filter,
        "-map",
        "[v]",
        "-map",
        "0:a?",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        // Usually made while still live: two threads, so the stream keeps the computer.
        "-threads",
        "2",
        "-crf",
        "20",
        "-c:a",
        "aac",
        "-b:a",
        "160k",
        "-movflags",
        "+faststart",
        // Written under another name and renamed when whole: the format can't come from it.
        "-f",
        "mp4",
    ] {
        a.push(s.into());
    }
    a.push(out.as_os_str().to_owned());
    a
}

impl Engine {
    /// Makes the vertical copy of a clip just saved, when the streamer asked for them and
    /// ffmpeg is here. In the background; a failure is logged and leaves no file behind.
    pub(crate) fn vertical_copy(&self, clip: Option<&str>) {
        if !self.scenes_config().vertical_clips {
            return;
        }
        let Some(clip) = clip.map(PathBuf::from).filter(|p| p.is_file()) else { return };
        let Some(ffmpeg) = self.ffmpeg_path() else {
            tracing::info!("no vertical clip: ffmpeg isn't here yet");
            return;
        };
        let Some(out) = vertical_path(&clip).filter(|o| !o.exists()) else { return };
        // Half-written, it has another name, so a folder never shows a broken clip.
        let part = out.with_extension("mp4.part");
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(&part).await;
            let run = Command::new(&ffmpeg).args(vertical_args(&clip, &part)).kill_on_drop(true).output();
            let ok = match tokio::time::timeout(LIMIT, run).await {
                Ok(Ok(o)) if o.status.success() => true,
                Ok(Ok(o)) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    tracing::warn!(error = %err.trim(), "the vertical clip couldn't be made");
                    false
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "ffmpeg didn't start for the vertical clip");
                    false
                }
                Err(_) => {
                    tracing::warn!("the vertical clip took too long and was stopped");
                    false
                }
            };
            if ok && tokio::fs::rename(&part, &out).await.is_ok() {
                tracing::info!(file = %out.display(), "vertical clip made");
            } else {
                let _ = tokio::fs::remove_file(&part).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copy_sits_beside_the_clip() {
        let p = vertical_path(Path::new("/clips/Replay 2026-10-29 21-00-00.mp4")).unwrap();
        assert_eq!(p, Path::new("/clips/Replay 2026-10-29 21-00-00 vertical.mp4"));
        assert_eq!(vertical_path(Path::new("/clips/a.mkv")).unwrap(), Path::new("/clips/a vertical.mp4"));
        assert!(vertical_path(Path::new("/")).is_none());
    }

    #[test]
    fn paths_are_plain_arguments() {
        // A name that looks like an option or holds shell characters stays one argument.
        let clip = Path::new("/c/-y; rm -rf ~ $(x).mp4");
        let out = Path::new("/c/-y; rm -rf ~ $(x) vertical.mp4");
        let a = vertical_args(clip, out);
        assert_eq!(a.iter().filter(|x| x.as_os_str() == clip.as_os_str()).count(), 1);
        assert_eq!(a.last().unwrap(), out.as_os_str());
        // Never overwrites (-n), and the input comes right after -i.
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(a[i + 1], clip.as_os_str());
        assert!(a.iter().any(|x| x == "-n"));
        let f = a.iter().position(|x| x == "-filter_complex").unwrap();
        assert!(a[f + 1].to_str().unwrap().contains("1080:1920"));
    }
}
