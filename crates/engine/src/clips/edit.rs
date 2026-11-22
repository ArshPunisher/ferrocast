//! Making new clips from clips: a trim, a compilation of several (with an optional crossfade and
//! one loudness), subtitles (made by Whisper, read, edited and written back, or burned in). Each
//! is a background job (see [`super::jobs`]); each output is written as ".part" and takes its name
//! only when whole, and never replaces a clip that is there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::jobs::{JobKind, JobView, Made, free_stem, part_path, run_ffmpeg};
use super::{Probe, new_stem, probe_file, refuse, srt_beside};
use crate::{Engine, EngineError};

/// A compilation holds at least two clips and at most this many.
pub const MAX_COMPILE: usize = 20;
/// A trim is at least this long.
const MIN_TRIM_MS: u64 = 200;
/// The longest crossfade, seconds.
pub const MAX_FADE_S: f32 = 3.0;
/// Subtitles: at most this many, each at most this long.
const MAX_CUES: usize = 2000;
const MAX_CUE_CHARS: usize = 300;
/// A subtitle file read is at most this big.
const MAX_SRT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrimReq {
    pub start_ms: u64,
    pub end_ms: u64,
    /// The new clip's name; missing: "<name> trimmed".
    #[serde(default)]
    pub name: Option<String>,
    /// Cut exactly (the picture is made again, slower) instead of at the nearest keyframe.
    #[serde(default)]
    pub precise: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileReq {
    /// The clips, in order: 2 to 20 (a clip may come twice).
    pub clips: Vec<String>,
    /// A crossfade between clips, seconds, 0 to 3 (0: a cut).
    #[serde(default)]
    pub crossfade_s: f32,
    /// One loudness for the whole (EBU R128, -16 LUFS).
    #[serde(default)]
    pub normalize: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

fn secs(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The arguments of a trim: from `start_ms` for `len_ms`, the streams copied or made again.
pub fn trim_args(input: &Path, start_ms: u64, len_ms: u64, precise: bool, part: &Path) -> Vec<OsString> {
    let mut a = os(&["-n", "-protocol_whitelist", "file", "-ss"]);
    a.push(secs(start_ms).into());
    a.push("-i".into());
    a.push(input.as_os_str().to_owned());
    a.push("-t".into());
    a.push(secs(len_ms).into());
    if precise {
        a.extend(os(&[
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a",
            "192k",
        ]));
    } else {
        a.extend(os(&["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"]));
    }
    a.extend(os(&["-movflags", "+faststart", "-f", "mp4"]));
    a.push(part.as_os_str().to_owned());
    a
}

/// One compilation's facts about each clip.
#[derive(Debug, Clone)]
pub struct Piece {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub has_audio: bool,
}

/// The filter graph and total length (ms) of a compilation at `w`×`h`.
pub fn compile_graph(pieces: &[Piece], w: u32, h: u32, fade_s: f32, normalize: bool) -> (String, u64) {
    let n = pieces.len();
    let mut g = String::new();
    for (i, p) in pieces.iter().enumerate() {
        g += &format!(
            "[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p,setpts=PTS-STARTPTS[v{i}];"
        );
        if p.has_audio {
            // Padded and cut to the picture's length, so each sound ends where its picture does and
            // the crossfade of the two lines up (a clip whose sound is short or long would drift).
            g += &format!(
                "[{i}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,apad,atrim=0:{},asetpts=PTS-STARTPTS[a{i}];",
                p.duration_ms as f64 / 1000.0
            );
        } else {
            g += &format!(
                "anullsrc=r=48000:cl=stereo,atrim=0:{},aformat=sample_fmts=fltp:channel_layouts=stereo,asetpts=PTS-STARTPTS[a{i}];",
                p.duration_ms as f64 / 1000.0
            );
        }
    }
    let fade_ms = (fade_s.clamp(0.0, MAX_FADE_S) * 1000.0).round() as u64;
    let total;
    let (mut v_last, mut a_last) = ("[v0]".to_string(), "[a0]".to_string());
    if fade_ms == 0 {
        for i in 0..n {
            g += &format!("[v{i}][a{i}]");
        }
        g += &format!("concat=n={n}:v=1:a=1[v][a]");
        total = pieces.iter().map(|p| p.duration_ms).sum();
        (v_last, a_last) = ("[v]".into(), "[a]".into());
    } else {
        let fade = fade_ms as f64 / 1000.0;
        let mut offset = pieces[0].duration_ms as f64 / 1000.0 - fade;
        for (i, piece) in pieces.iter().enumerate().skip(1) {
            let (v, a) = (format!("[vx{i}]"), format!("[ax{i}]"));
            g += &format!("{v_last}[v{i}]xfade=transition=fade:duration={fade}:offset={offset:.3}{v};");
            g += &format!("{a_last}[a{i}]acrossfade=d={fade}{a};");
            offset += piece.duration_ms as f64 / 1000.0 - fade;
            (v_last, a_last) = (v, a);
        }
        total = pieces.iter().map(|p| p.duration_ms).sum::<u64>() - fade_ms * (n as u64 - 1);
    }
    if normalize {
        g += &format!(";{a_last}loudnorm=I=-16:TP=-1.5:LRA=11[an]");
        a_last = "[an]".into();
    }
    // The graph's pieces end with a ";" only between statements.
    let g = g.replace(";;", ";").trim_end_matches(';').to_string();
    (format!("{g}\u{0}{v_last}\u{0}{a_last}"), total)
}

/// The arguments of a compilation: every input read as a plain file, never replacing.
pub fn compile_args(
    pieces: &[Piece],
    w: u32,
    h: u32,
    fade_s: f32,
    normalize: bool,
    part: &Path,
) -> (Vec<OsString>, u64) {
    let (packed, total) = compile_graph(pieces, w, h, fade_s, normalize);
    let mut it = packed.split('\u{0}');
    let (graph, v, a) = (it.next().unwrap_or(""), it.next().unwrap_or("[v]"), it.next().unwrap_or("[a]"));
    let mut args = os(&["-n"]);
    for p in pieces {
        args.extend(os(&["-protocol_whitelist", "file", "-i"]));
        args.push(p.path.as_os_str().to_owned());
    }
    args.extend(os(&["-filter_complex"]));
    args.push(graph.into());
    args.extend(os(&["-map", v, "-map", a]));
    args.extend(os(&[
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ]));
    args.push(part.as_os_str().to_owned());
    (args, total)
}

/// SubRip time: `HH:MM:SS,mmm`.
fn srt_time(ms: u64) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

fn parse_srt_time(s: &str) -> Option<u64> {
    let (hms, milli) = s.trim().split_once([',', '.'])?;
    let mut it = hms.split(':');
    let (h, m, sec) =
        (it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u64>().ok()?);
    if it.next().is_some() || m > 59 || sec > 59 {
        return None;
    }
    let milli: u64 = format!("{milli:0<3}").get(..3)?.parse().ok()?;
    Some(h * 3_600_000 + m * 60_000 + sec * 1000 + milli)
}

/// The cues of a SubRip text (a broken block is skipped).
pub fn parse_srt(text: &str) -> Vec<Cue> {
    let text = text.replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().map(str::trim_end).filter(|l| !l.is_empty());
        let Some(first) = lines.next() else { continue };
        // The number line is optional in what people paste.
        let times = if first.contains("-->") { Some(first) } else { lines.next() };
        let Some((a, b)) = times.and_then(|t| t.split_once("-->")) else { continue };
        let (Some(start_ms), Some(end_ms)) =
            (parse_srt_time(a), parse_srt_time(b.split_whitespace().next().unwrap_or("")))
        else {
            continue;
        };
        let body = lines.collect::<Vec<_>>().join("\n");
        if body.trim().is_empty() || out.len() >= MAX_CUES {
            continue;
        }
        out.push(Cue { start_ms, end_ms: end_ms.max(start_ms + 1), text: body });
    }
    out
}

/// A SubRip text of cues (already checked).
pub fn render_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, srt_time(c.start_ms), srt_time(c.end_ms), c.text.trim()));
    }
    out
}

/// Cues a person edited, checked: how many, how long, ordered times, no stray control characters.
pub fn check_cues(cues: &[Cue]) -> Result<(), EngineError> {
    let bad = |m: String| refuse(400, "bad_captions", m, "Fix that caption and save again.");
    if cues.len() > MAX_CUES {
        return Err(bad(format!("There can be up to {MAX_CUES} captions.")));
    }
    for (i, c) in cues.iter().enumerate() {
        let n = i + 1;
        if c.end_ms <= c.start_ms {
            return Err(bad(format!("Caption {n} ends before it starts.")));
        }
        if c.end_ms > 24 * 3_600_000 {
            return Err(bad(format!("Caption {n} is past 24 hours.")));
        }
        let t = c.text.trim();
        if t.is_empty() || t.chars().count() > MAX_CUE_CHARS {
            return Err(bad(format!("Caption {n} needs 1 to {MAX_CUE_CHARS} characters.")));
        }
        if t.chars().any(|ch| ch.is_control() && ch != '\n') || t.contains("\n\n") {
            return Err(bad(format!("Caption {n} has a character that can't be in it.")));
        }
        if i > 0 && c.start_ms < cues[i - 1].start_ms {
            return Err(bad(format!("Caption {n} starts before the one above it.")));
        }
    }
    Ok(())
}

/// The arguments burning `subs` (a plain name in the working folder) into a clip.
pub fn burn_args(input: &Path, subs: &str, part: &Path) -> Vec<OsString> {
    let mut a = os(&["-n", "-protocol_whitelist", "file", "-i"]);
    a.push(input.as_os_str().to_owned());
    let style = "FontName=Helvetica,Fontsize=18,Bold=1,PrimaryColour=&H00FFFFFF,OutlineColour=&H00000000,BorderStyle=1,Outline=2,Shadow=0,Alignment=2,MarginV=24";
    a.push("-vf".into());
    a.push(format!("subtitles={subs}:force_style='{style}'").into());
    a.extend(os(&[
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "copy",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ]));
    a.push(part.as_os_str().to_owned());
    a
}

impl Engine {
    /// A new clip made from one: the part from `start_ms` to `end_ms` → the job.
    pub async fn trim_clip(&self, id: &str, req: TrimReq) -> Result<JobView, EngineError> {
        let root = self.clips_folder().await?;
        let path = self.clip_path(id).await?;
        let ffmpeg = self.clips_ffmpeg()?;
        let probe = probe_file(&ffmpeg, &path).await?;
        let total = probe.duration_ms.ok_or_else(|| {
            refuse(422, "not_a_clip", "That clip's length isn't known.", "It may still be being written.")
        })?;
        if req.start_ms >= total {
            return Err(refuse(
                400,
                "bad_trim",
                "The start is past the end of the clip.",
                "Pick a start inside the clip.",
            ));
        }
        let end = req.end_ms.min(total);
        if end <= req.start_ms || end - req.start_ms < MIN_TRIM_MS {
            return Err(refuse(
                400,
                "bad_trim",
                "The part to keep is too short.",
                "Keep at least a fifth of a second.",
            ));
        }
        let base = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "clip".into());
        let wanted = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => new_stem(n)?,
            None => new_stem(&format!("{base} trimmed")).unwrap_or_else(|_| "Trimmed".into()),
        };
        let precise = req.precise;
        let (start, len) = (req.start_ms, end - req.start_ms);
        Ok(self.start_clip_job(JobKind::Trim, format!("Trim {base}"), move |ctx| async move {
            let stem = tokio::task::spawn_blocking({
                let (r, w) = (root.clone(), wanted.clone());
                move || free_stem(&r, &w, "mp4")
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
            let out = root.join(format!("{stem}.mp4"));
            let part = part_path(&out);
            run_ffmpeg(&ctx, &ffmpeg, trim_args(&path, start, len, precise, &part), len, &part, &out, None).await?;
            Ok(Made { output: Some(format!("{stem}.mp4")) })
        }))
    }

    /// Several clips as one file → the job.
    pub async fn compile_clips(&self, req: CompileReq) -> Result<JobView, EngineError> {
        let bad = |m: &str, h: &str| refuse(400, "bad_compile", m.to_string(), h);
        if req.clips.len() < 2 || req.clips.len() > MAX_COMPILE {
            return Err(bad(
                &format!("A compilation needs 2 to {MAX_COMPILE} clips."),
                "Pick the clips to put together.",
            ));
        }
        if !(0.0..=MAX_FADE_S).contains(&req.crossfade_s) || !req.crossfade_s.is_finite() {
            return Err(bad(&format!("A crossfade is 0 to {MAX_FADE_S} seconds."), "Pick a shorter fade."));
        }
        let root = self.clips_folder().await?;
        // Every clip is looked up first (a missing one is a 404 whether or not FFmpeg is here).
        let mut paths = Vec::new();
        for id in &req.clips {
            paths.push(self.clip_path(id).await?);
        }
        let ffmpeg = self.clips_ffmpeg()?;
        let mut pieces = Vec::new();
        let mut size = None;
        for (id, path) in req.clips.iter().zip(paths) {
            let p: Probe = probe_file(&ffmpeg, &path).await?;
            let d = p.duration_ms.filter(|d| *d > 0 && p.has_video).ok_or_else(|| {
                refuse(
                    422,
                    "not_a_clip",
                    format!("\"{id}\" isn't a video with a length."),
                    "Leave it out, or wait until it is saved.",
                )
            })?;
            let fade_ms = (req.crossfade_s * 1000.0) as u64;
            if fade_ms > 0 && d < fade_ms * 2 {
                return Err(refuse(
                    400,
                    "bad_compile",
                    format!("\"{id}\" is shorter than twice the crossfade."),
                    "Use a shorter fade or leave that clip out.",
                ));
            }
            if size.is_none() {
                size = p.width.zip(p.height);
            }
            pieces.push(Piece { path, duration_ms: d, has_audio: p.has_audio });
        }
        // The first clip's size (even numbers, none under 16): the others are fitted inside it.
        let (w, h) = size.map(|(w, h)| (w.clamp(16, 7680) & !1, h.clamp(16, 4320) & !1)).unwrap_or((1920, 1080));
        let wanted = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => new_stem(n)?,
            None => "Compilation".to_string(),
        };
        let (fade, norm) = (req.crossfade_s, req.normalize);
        let count = pieces.len();
        Ok(self.start_clip_job(JobKind::Compile, format!("Compile {count} clips"), move |ctx| async move {
            let stem = tokio::task::spawn_blocking({
                let (r, n) = (root.clone(), wanted.clone());
                move || free_stem(&r, &n, "mp4")
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
            let out = root.join(format!("{stem}.mp4"));
            let part = part_path(&out);
            let (args, total) = compile_args(&pieces, w, h, fade, norm, &part);
            run_ffmpeg(&ctx, &ffmpeg, args, total, &part, &out, None).await?;
            Ok(Made { output: Some(format!("{stem}.mp4")) })
        }))
    }

    /// The subtitles beside a clip (none: an empty list).
    pub async fn clip_captions(&self, id: &str) -> Result<Vec<Cue>, EngineError> {
        let path = self.clip_path(id).await?;
        let srt = srt_beside(&path);
        tokio::task::spawn_blocking(move || -> Result<Vec<Cue>, EngineError> {
            let Ok(meta) = std::fs::symlink_metadata(&srt) else { return Ok(Vec::new()) };
            if !meta.file_type().is_file() || meta.len() > MAX_SRT_BYTES {
                return Err(refuse(
                    422,
                    "bad_captions",
                    "That clip's caption file can't be read.",
                    "Delete it and make the captions again.",
                ));
            }
            Ok(parse_srt(&std::fs::read_to_string(&srt).unwrap_or_default()))
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))?
    }

    /// The subtitles as the text of an .srt file (a download).
    pub async fn clip_captions_srt(&self, id: &str) -> Result<String, EngineError> {
        Ok(render_srt(&self.clip_captions(id).await?))
    }

    /// Writes the subtitles a person edited beside the clip.
    pub async fn set_clip_captions(&self, id: &str, cues: Vec<Cue>) -> Result<Vec<Cue>, EngineError> {
        check_cues(&cues)?;
        let path = self.clip_path(id).await?;
        let srt = srt_beside(&path);
        let text = render_srt(&cues);
        tokio::task::spawn_blocking(move || -> Result<(), EngineError> {
            // Not through a link someone put there.
            if std::fs::symlink_metadata(&srt).is_ok_and(|m| !m.file_type().is_file()) {
                return Err(refuse(
                    422,
                    "bad_captions",
                    "That caption file can't be written.",
                    "Delete it and try again.",
                ));
            }
            write_new_then_rename(&srt, text.as_bytes()).map_err(|e| EngineError::Internal(e.to_string()))
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        Ok(cues)
    }

    /// Whisper writes what was said beside the clip → the job.
    pub async fn make_clip_captions(&self, id: &str) -> Result<JobView, EngineError> {
        let path = self.clip_path(id).await?;
        let ffmpeg = self.clips_ffmpeg()?;
        if srt_beside(&path).exists() {
            return Err(EngineError::Conflict("That clip has captions already: edit them instead.".into()));
        }
        let base = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(self.start_clip_job(JobKind::Captions, format!("Captions for {base}"), move |ctx| async move {
            ctx.progress(0.05);
            let made = tokio::select! {
                m = ctx.engine.clip_subtitles(&ffmpeg, &path) => m,
                () = ctx.cancel.wait() => return Err(super::jobs::cancelled()),
            };
            if made.is_none() {
                return Err(refuse(
                    422,
                    "no_captions",
                    "No captions could be made: nothing was said, or no speech model is downloaded.",
                    "Download a speech model in Settings, Studio, then try again.",
                ));
            }
            Ok(Made { output: None })
        }))
    }

    /// The subtitles burned into a new copy → the job.
    pub async fn burn_clip_captions(&self, id: &str, name: Option<&str>) -> Result<JobView, EngineError> {
        let root = self.clips_folder().await?;
        let path = self.clip_path(id).await?;
        let ffmpeg = self.clips_ffmpeg()?;
        let srt = srt_beside(&path);
        if self.clip_captions(id).await?.is_empty() {
            return Err(refuse(
                422,
                "no_captions",
                "That clip has no captions yet.",
                "Make them first, then burn them in.",
            ));
        }
        if !ffmpeg_can_draw_captions(&ffmpeg).await {
            return Err(refuse(
                422,
                "ffmpeg_no_captions",
                "This FFmpeg can't draw captions into the picture.",
                "Export the captions as a .srt file instead, or get Ferrocast's FFmpeg in Settings.",
            ));
        }
        let probe = probe_file(&ffmpeg, &path).await?;
        let base = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let wanted = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => new_stem(n)?,
            None => new_stem(&format!("{base} captioned")).unwrap_or_else(|_| "Captioned".into()),
        };
        let total = probe.duration_ms.unwrap_or(0);
        Ok(self.start_clip_job(JobKind::Burn, format!("Burn captions into {base}"), move |ctx| async move {
            let stem = tokio::task::spawn_blocking({
                let (r, n) = (root.clone(), wanted.clone());
                move || free_stem(&r, &n, "mp4")
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
            let out = root.join(format!("{stem}.mp4"));
            let part = part_path(&out);
            // A folder of its own, made new, holding the captions as a plain name (a path with a
            // colon or quote breaks ffmpeg's filter text).
            let work = std::env::temp_dir().join(format!("ferrocast-burn-{}-{:x}", std::process::id(), now_nanos()));
            tokio::fs::create_dir(&work).await.map_err(|e| EngineError::Internal(e.to_string()))?;
            let copied = tokio::fs::copy(&srt, work.join("subs.srt")).await;
            let res = match copied {
                Ok(_) => {
                    run_ffmpeg(&ctx, &ffmpeg, burn_args(&path, "subs.srt", &part), total, &part, &out, Some(&work))
                        .await
                }
                Err(e) => Err(EngineError::Internal(e.to_string())),
            };
            let _ = tokio::fs::remove_dir_all(&work).await;
            res?;
            Ok(Made { output: Some(format!("{stem}.mp4")) })
        }))
    }
}

/// Writes `bytes` to a file of its own beside `target` (made new, so it can't be a link someone
/// left, and two saves never share one) and renames it over `target`.
fn write_new_then_rename(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for _ in 0..100 {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let part = target.with_file_name(format!("{name}.{}-{n}.part", std::process::id()));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&part) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            other => other?,
        };
        let written = f.write_all(bytes).and_then(|()| f.sync_all());
        drop(f);
        let done = written.and_then(|()| std::fs::rename(&part, target));
        if done.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        return done;
    }
    Err(std::io::Error::other("couldn't make a file to write the captions in"))
}

fn now_nanos() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos())
}

/// Whether this ffmpeg has the `subtitles` filter (it needs libass).
pub(crate) async fn ffmpeg_can_draw_captions(ffmpeg: &Path) -> bool {
    let mut cmd = super::jobs::hidden(ffmpeg);
    cmd.args(["-hide_banner", "-filters"]).kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output()).await {
        Ok(Ok(o)) => {
            String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some("subtitles"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
