//! A vertical copy of each saved clip (plan item 6, 29 Oct): 1080×1920 for Shorts, TikTok and
//! Reels, the clip whole in the middle over a blurred, zoomed copy of itself (no part of the
//! picture is cut off). Made by ffmpeg in the background, next to the clip, as "<name>
//! vertical.mp4"; nothing is sent anywhere.
//!
//! Captioned clips (30 Oct): Whisper writes what was said into "<name>.srt" beside the clip,
//! and the vertical copy gets the words burned in, above where Shorts and TikTok put their
//! buttons (most of them are watched muted).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::Engine;
use crate::studio_host::{Said, Transcribe};

/// ffmpeg turning a clip's sound into what Whisper reads: 16 kHz mono floats.
const AUDIO_LIMIT: Duration = Duration::from_secs(120);
/// At most this much sound is transcribed (a clip is seconds, not an hour).
const MAX_AUDIO_SECONDS: usize = 15 * 60;

/// The subtitles' look on a 1080×1920 copy (libass units, the picture 288 tall): white, bold,
/// outlined, at the bottom but above the apps' buttons.
const SUB_STYLE: &str = "FontName=Helvetica,Fontsize=12,Bold=1,PrimaryColour=&H00FFFFFF,OutlineColour=&H00000000,BorderStyle=1,Outline=2,Shadow=0,Alignment=2,MarginV=55,MarginL=20,MarginR=20";

/// The subtitle file of `clip`: beside it, ".srt".
pub fn srt_path(clip: &Path) -> PathBuf {
    clip.with_extension("srt")
}

/// SubRip time: `HH:MM:SS,mmm`.
fn srt_time(ms: u64) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// A caption at most this long (characters): about two short lines on a phone.
const CAPTION_CHARS: usize = 32;

/// Long stretches cut into captions of whole words, at most `max` characters each, the time
/// shared out by length (Whisper times sentences, not words).
pub fn short_captions(said: &[Said], max: usize) -> Vec<Said> {
    let mut out = Vec::new();
    for s in said {
        let words: Vec<&str> = s.text.split_whitespace().collect();
        let mut pieces: Vec<String> = Vec::new();
        for w in words {
            match pieces.last_mut() {
                Some(p) if p.chars().count() + 1 + w.chars().count() <= max => {
                    p.push(' ');
                    p.push_str(w);
                }
                _ => pieces.push(w.to_string()),
            }
        }
        let total: usize = pieces.iter().map(|p| p.chars().count()).sum::<usize>().max(1);
        let span = s.end_ms.saturating_sub(s.start_ms);
        let mut at = s.start_ms;
        for (i, p) in pieces.iter().enumerate() {
            let end =
                if i + 1 == pieces.len() { s.end_ms } else { at + span * p.chars().count() as u64 / total as u64 };
            out.push(Said { start_ms: at, end_ms: end, text: p.clone() });
            at = end;
        }
    }
    out
}

/// What was said as a SubRip file; empty stretches are left out, overlaps are trimmed.
pub fn srt(said: &[Said]) -> String {
    let said = short_captions(said, CAPTION_CHARS);
    let said = said.as_slice();
    let mut out = String::new();
    let mut n = 0;
    for (i, s) in said.iter().enumerate() {
        let text = s.text.trim();
        if text.is_empty() {
            continue;
        }
        let end = said.get(i + 1).map_or(s.end_ms, |next| s.end_ms.min(next.start_ms.max(s.start_ms + 1)));
        n += 1;
        out.push_str(&format!("{n}\n{} --> {}\n{text}\n\n", srt_time(s.start_ms), srt_time(end.max(s.start_ms + 1))));
    }
    out
}

/// ffmpeg's arguments to read a clip's sound as 16 kHz mono floats on stdout.
fn audio_args(clip: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> =
        ["-hide_banner", "-loglevel", "error", "-nostdin", "-i"].iter().map(OsString::from).collect();
    a.push(clip.as_os_str().to_owned());
    for s in ["-vn", "-ac", "1", "-ar", "16000", "-t", "900", "-f", "f32le", "-"] {
        a.push(s.into());
    }
    a
}

/// Longer than any clip takes; a stuck ffmpeg is stopped and its half-written file removed.
const LIMIT: Duration = Duration::from_secs(300);

/// Where the vertical copy of `clip` goes: beside it, " vertical" added to its name.
pub fn vertical_path(clip: &Path) -> Option<PathBuf> {
    let stem = clip.file_stem()?.to_str()?;
    Some(clip.with_file_name(format!("{stem} vertical.mp4")))
}

/// ffmpeg's arguments: blurred background filling 1080×1920, the clip scaled to the width in
/// the middle, the sound copied as it is. Paths go as arguments, never through a shell.
/// `subs`: a subtitle file's name in ffmpeg's working folder, burned in (a plain name: paths
/// with colons or quotes break ffmpeg's filter syntax).
pub fn vertical_args(clip: &Path, out: &Path, subs: Option<&str>) -> Vec<OsString> {
    let burn = subs.map(|f| format!(",subtitles={f}:force_style='{SUB_STYLE}'")).unwrap_or_default();
    let filter = format!(
        "[0:v]split=2[bg][fg];\
        [bg]scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,boxblur=20:2[back];\
        [fg]scale=1080:-2[front];\
        [back][front]overlay=(W-w)/2:(H-h)/2{burn},format=yuv420p[v]"
    );
    let filter = filter.as_str();
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
    /// After a clip is saved: its subtitles (when asked for, with a speech model here) and its
    /// vertical copy (when asked for), in the background. A failure is logged and leaves no
    /// file behind; subtitles failing still leaves the vertical copy, without words.
    pub(crate) fn vertical_copy(&self, clip: Option<&str>) {
        let cfg = self.scenes_config();
        if !cfg.vertical_clips && !cfg.clip_captions {
            return;
        }
        let Some(clip) = clip.map(PathBuf::from).filter(|p| p.is_file()) else { return };
        let Some(ffmpeg) = self.ffmpeg_path() else {
            tracing::info!("no vertical clip or subtitles: ffmpeg isn't here yet");
            return;
        };
        let engine = self.clone();
        tokio::spawn(async move {
            let subs = if cfg.clip_captions { engine.clip_subtitles(&ffmpeg, &clip).await } else { None };
            if cfg.vertical_clips {
                make_vertical(&ffmpeg, &clip, subs.as_deref()).await;
            }
        });
    }

    /// Writes "<clip>.srt" (never over one already there) → its path, when anything was said.
    async fn clip_subtitles(&self, ffmpeg: &Path, clip: &Path) -> Option<PathBuf> {
        let out = srt_path(clip);
        if out.exists() {
            return Some(out);
        }
        // The better model when it's here, else the fast one.
        let Some(model) = self.speech_model_path("small").or_else(|| self.speech_model_path("base")) else {
            tracing::info!("no clip subtitles: no speech model is downloaded");
            return None;
        };
        let run = Command::new(ffmpeg).args(audio_args(clip)).kill_on_drop(true).output();
        let sound = match tokio::time::timeout(AUDIO_LIMIT, run).await {
            Ok(Ok(o)) if o.status.success() => o.stdout,
            other => {
                tracing::warn!(?other, "the clip's sound couldn't be read for subtitles");
                return None;
            }
        };
        let sound: Vec<f32> =
            sound.as_chunks::<4>().0.iter().take(16_000 * MAX_AUDIO_SECONDS).map(|b| f32::from_le_bytes(*b)).collect();
        let how = Transcribe { model, language: "auto".into(), translate: false };
        let said = match self.studio_host() {
            Ok(h) => h.transcribe(sound, how).await,
            Err(e) => Err(e.to_string()),
        };
        let text = match said {
            Ok(s) => srt(&s),
            Err(err) => {
                tracing::warn!(%err, "the clip couldn't be transcribed");
                return None;
            }
        };
        if text.is_empty() {
            tracing::info!("no clip subtitles: nothing was said");
            return None;
        }
        let part = out.with_extension("srt.part");
        if tokio::fs::write(&part, text).await.is_ok() && tokio::fs::rename(&part, &out).await.is_ok() {
            tracing::info!(file = %out.display(), "clip subtitles written");
            Some(out)
        } else {
            let _ = tokio::fs::remove_file(&part).await;
            None
        }
    }
}

/// Makes "<clip> vertical.mp4" (never over one already there), with `subs` burned in when
/// given. ffmpeg runs in a folder of its own holding the subtitles as "subs.srt".
async fn make_vertical(ffmpeg: &Path, clip: &Path, subs: Option<&Path>) {
    let Some(out) = vertical_path(clip).filter(|o| !o.exists()) else { return };
    // Half-written, it has another name, so a folder never shows a broken clip.
    let part = out.with_extension("mp4.part");
    let _ = tokio::fs::remove_file(&part).await;
    // A folder per copy: clips saved close together never share (or remove) each other's.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("ferrocast-vertical-{}-{n}", std::process::id()));
    let burn = match subs {
        Some(s) => {
            tokio::fs::create_dir_all(&work).await.is_ok() && tokio::fs::copy(s, work.join("subs.srt")).await.is_ok()
        }
        None => false,
    };
    let args = vertical_args(clip, &part, burn.then_some("subs.srt"));
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args).kill_on_drop(true);
    if burn {
        cmd.current_dir(&work);
    }
    let ok = match tokio::time::timeout(LIMIT, cmd.output()).await {
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
    if subs.is_some() {
        let _ = tokio::fs::remove_dir_all(&work).await;
    }
    if ok && tokio::fs::rename(&part, &out).await.is_ok() {
        tracing::info!(file = %out.display(), captioned = burn, "vertical clip made");
    } else {
        let _ = tokio::fs::remove_file(&part).await;
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
        let a = vertical_args(clip, out, None);
        assert_eq!(a.iter().filter(|x| x.as_os_str() == clip.as_os_str()).count(), 1);
        assert_eq!(a.last().unwrap(), out.as_os_str());
        // Never overwrites (-n), and the input comes right after -i.
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(a[i + 1], clip.as_os_str());
        assert!(a.iter().any(|x| x == "-n"));
        let f = a.iter().position(|x| x == "-filter_complex").unwrap();
        assert!(a[f + 1].to_str().unwrap().contains("1080:1920"));
    }

    #[test]
    fn long_sentences_become_short_captions() {
        let s = Said {
            start_ms: 0,
            end_ms: 9000,
            text: "Welcome back to the stream everyone today we are playing ranked".into(),
        };
        let got = short_captions(&[s], 32);
        assert!(got.len() >= 2);
        assert!(got.iter().all(|c| c.text.chars().count() <= 32), "{got:?}");
        // Back to back, from the start to the end, words in order.
        assert_eq!(got[0].start_ms, 0);
        assert_eq!(got.last().unwrap().end_ms, 9000);
        assert!(got.windows(2).all(|w| w[0].end_ms == w[1].start_ms));
        let joined = got.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, "Welcome back to the stream everyone today we are playing ranked");
        // A word longer than a caption stays whole.
        let long = Said { start_ms: 0, end_ms: 100, text: "supercalifragilisticexpialidocious!!".into() };
        assert_eq!(short_captions(&[long], 32).len(), 1);
    }

    #[test]
    fn subtitles_are_subrip() {
        let said = vec![
            Said { start_ms: 0, end_ms: 2500, text: " Welcome back ".into() },
            Said { start_ms: 2400, end_ms: 4000, text: "".into() },
            Said { start_ms: 3_725_001, end_ms: 3_726_000, text: "clip that!".into() },
        ];
        assert_eq!(
            srt(&said),
            "1\n00:00:00,000 --> 00:00:02,500\nWelcome back\n\n2\n01:02:05,001 --> 01:02:06,000\nclip that!\n\n"
        );
        assert_eq!(srt(&[]), "");
        // Overlapping stretches: the first ends where the next starts.
        let overlap = vec![
            Said { start_ms: 0, end_ms: 3000, text: "one".into() },
            Said { start_ms: 2000, end_ms: 4000, text: "two".into() },
        ];
        assert!(srt(&overlap).contains("00:00:00,000 --> 00:00:02,000\none"));
        assert_eq!(srt_path(Path::new("/c/Replay 1.mp4")), Path::new("/c/Replay 1.srt"));
    }

    #[test]
    fn subtitles_burn_in_by_a_plain_name() {
        let a = vertical_args(Path::new("/c/a.mp4"), Path::new("/c/a v.mp4"), Some("subs.srt"));
        let f = a.iter().position(|x| x == "-filter_complex").unwrap();
        let filter = a[f + 1].to_str().unwrap();
        assert!(filter.contains(",subtitles=subs.srt:force_style='"), "{filter}");
        assert!(filter.ends_with(",format=yuv420p[v]"));
        // No subtitles: the filter as before.
        let b = vertical_args(Path::new("/c/a.mp4"), Path::new("/c/a v.mp4"), None);
        let g = b.iter().position(|x| x == "-filter_complex").unwrap();
        assert!(!b[g + 1].to_str().unwrap().contains("subtitles"));
    }

    #[test]
    fn the_sound_is_read_as_16k_floats() {
        let a = audio_args(Path::new("/c/-x.mp4"));
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(a[i + 1], "/c/-x.mp4");
        assert!(a.windows(2).any(|w| w[0] == "-ar" && w[1] == "16000"));
        assert_eq!(a.last().unwrap(), "-");
    }
}
