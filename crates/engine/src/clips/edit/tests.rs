use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::clips::jobs::{JobState, JobView};

fn arg_strs(a: &[OsString]) -> Vec<String> {
    a.iter().map(|x| x.to_string_lossy().into_owned()).collect()
}

fn piece(name: &str, ms: u64, audio: bool) -> Piece {
    Piece { path: PathBuf::from(name), duration_ms: ms, has_audio: audio }
}

#[test]
fn every_ffmpeg_call_reads_only_files_and_never_overwrites() {
    let part = Path::new("/c/out.mp4.part");
    let trim = arg_strs(&trim_args(Path::new("/c/a.mp4"), 1500, 3000, false, part));
    let (comp, _) =
        compile_args(&[piece("/c/a.mp4", 4000, true), piece("/c/b.mp4", 4000, false)], 1280, 720, 0.5, true, part);
    let comp = arg_strs(&comp);
    let burn = arg_strs(&burn_args(Path::new("/c/a.mp4"), "subs.srt", part));
    for (what, a) in [("trim", &trim), ("compile", &comp), ("burn", &burn)] {
        assert_eq!(a[0], "-n", "{what}: never over a file");
        let inputs = a.iter().filter(|x| *x == "-i").count();
        let guards = a.windows(2).filter(|w| w[0] == "-protocol_whitelist" && w[1] == "file").count();
        assert_eq!(inputs, guards, "{what}: every input is guarded");
        assert_eq!(a.last().map(String::as_str), Some("/c/out.mp4.part"), "{what}: written as a part");
    }
    assert!(trim.windows(2).any(|w| w[0] == "-ss" && w[1] == "1.500"));
    assert!(trim.windows(2).any(|w| w[0] == "-t" && w[1] == "3.000"));
    assert!(trim.contains(&"copy".to_string()), "a quick trim copies the streams");
    let precise = arg_strs(&trim_args(Path::new("/c/a.mp4"), 0, 1000, true, part));
    assert!(precise.contains(&"libx264".to_string()) && !precise.contains(&"copy".to_string()));
}

#[test]
fn a_compilation_joins_with_a_cut_or_a_crossfade_and_one_loudness() {
    let two = [piece("a", 4000, true), piece("b", 6000, false)];
    let (cut, total) = compile_graph(&two, 1280, 720, 0.0, false);
    assert_eq!(total, 10_000);
    assert!(cut.contains("concat=n=2:v=1:a=1") && !cut.contains("xfade") && !cut.contains("loudnorm"));
    // The clip with no sound gets silence, so the joins line up.
    assert!(cut.contains("anullsrc") && cut.contains("[1:v]scale=1280:720"));
    // A sound is padded and cut to its picture's length, so a short or long one can't drift the fade.
    assert!(cut.contains("apad,atrim=0:4,"), "{cut}");
    let (fade, total) = compile_graph(&two, 1280, 720, 0.5, true);
    assert_eq!(total, 9500);
    assert!(fade.contains("xfade=transition=fade:duration=0.5:offset=3.500") && fade.contains("acrossfade=d=0.5"));
    assert!(fade.contains("loudnorm=I=-16"));
    let three = [piece("a", 4000, true), piece("b", 4000, true), piece("c", 4000, true)];
    let (g, total) = compile_graph(&three, 1280, 720, 1.0, false);
    assert_eq!(total, 10_000);
    // The second offset is after the first fade: 4 - 1 + 4 - 1.
    assert!(g.contains("offset=3.000") && g.contains("offset=6.000"), "{g}");
    let (_, v, a) = {
        let mut it = g.split('\u{0}');
        (it.next(), it.next().unwrap(), it.next().unwrap())
    };
    assert_eq!((v, a), ("[vx2]", "[ax2]"));
}

#[test]
fn subtitles_round_trip_and_junk_is_dropped() {
    let cues = vec![
        Cue { start_ms: 0, end_ms: 1500, text: "Hello there".into() },
        Cue { start_ms: 1500, end_ms: 3_661_250, text: "Two\nlines".into() },
    ];
    let text = render_srt(&cues);
    assert!(text.starts_with("1\n00:00:00,000 --> 00:00:01,500\nHello there\n"));
    assert!(text.contains("00:00:01,500 --> 01:01:01,250"));
    assert_eq!(parse_srt(&text), cues);
    // Windows line ends, no number line, a dot for the comma, a broken block, a block with no words.
    let mixed = "00:00:01.000 --> 00:00:02.000\r\nNo number\r\n\r\nnot a time\r\nxx\r\n\r\n3\r\n00:00:03,000 --> 00:00:04,000\r\n\r\n";
    let got = parse_srt(mixed);
    assert_eq!(got, vec![Cue { start_ms: 1000, end_ms: 2000, text: "No number".into() }]);
    assert!(parse_srt("").is_empty());
}

#[test]
fn edited_captions_are_checked() {
    let ok = |s, e, t: &str| Cue { start_ms: s, end_ms: e, text: t.into() };
    assert!(check_cues(&[ok(0, 1000, "a"), ok(500, 2000, "b")]).is_ok(), "overlap is allowed");
    for bad in [
        vec![ok(1000, 1000, "a")],
        vec![ok(2000, 1000, "a")],
        vec![ok(0, 1000, "  ")],
        vec![ok(0, 1000, &"x".repeat(301))],
        vec![ok(0, 1000, "a\u{7}b")],
        vec![ok(0, 1000, "a\n\nb")],
        vec![ok(5000, 6000, "a"), ok(1000, 2000, "b")],
        vec![ok(0, 25 * 3_600_000, "a")],
    ] {
        assert!(
            matches!(check_cues(&bad), Err(EngineError::Refused { status: 400, code: "bad_captions", .. })),
            "{bad:?}"
        );
    }
    let many: Vec<Cue> = (0..2001).map(|i| ok(i * 10, i * 10 + 5, "a")).collect();
    assert!(check_cues(&many).is_err());
}

async fn engine() -> (Engine, tempfile::TempDir, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();
    let e = Engine::start_with_memory_secrets(data.path()).await.unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    (e, data, clips)
}

#[tokio::test]
async fn captions_are_read_edited_saved_and_exported() {
    let (e, _d, clips) = engine().await;
    std::fs::write(clips.path().join("a.mp4"), b"x").unwrap();
    assert!(e.clip_captions("a.mp4").await.unwrap().is_empty(), "none yet");
    let cues = vec![Cue { start_ms: 0, end_ms: 2000, text: "Hi".into() }];
    e.set_clip_captions("a.mp4", cues.clone()).await.unwrap();
    assert_eq!(e.clip_captions("a.mp4").await.unwrap(), cues);
    assert!(e.clip_captions_srt("a.mp4").await.unwrap().contains("00:00:00,000 --> 00:00:02,000"));
    assert!(!clips.path().join("a.srt.part").exists());
    assert!(e.clips(None).await.unwrap()[0].has_captions);
    assert!(matches!(
        e.set_clip_captions("a.mp4", vec![Cue { start_ms: 5, end_ms: 1, text: "x".into() }]).await,
        Err(EngineError::Refused { status: 400, .. })
    ));
    assert_eq!(e.clip_captions("a.mp4").await.unwrap(), cues, "a refused save leaves the file");
    assert!(matches!(e.set_clip_captions("../a.mp4", cues).await, Err(EngineError::Refused { status: 404, .. })));
    // Captions are never made over ones that are there.
    assert!(matches!(
        e.make_clip_captions("a.mp4").await,
        Err(EngineError::Conflict(_)) | Err(EngineError::Refused { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_caption_file_that_is_a_link_is_never_followed() {
    let (e, _d, clips) = engine().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(clips.path().join("a.mp4"), b"x").unwrap();
    std::fs::write(outside.path().join("secret.srt"), "1\n00:00:00,000 --> 00:00:01,000\nsecret\n\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.srt"), clips.path().join("a.srt")).unwrap();
    assert!(matches!(e.clip_captions("a.mp4").await, Err(EngineError::Refused { code: "bad_captions", .. })));
    assert!(matches!(
        e.set_clip_captions("a.mp4", vec![Cue { start_ms: 0, end_ms: 1000, text: "x".into() }]).await,
        Err(EngineError::Refused { code: "bad_captions", .. })
    ));
    assert!(
        std::fs::read_to_string(outside.path().join("secret.srt")).unwrap().contains("secret"),
        "not written through the link"
    );
}

async fn finished(e: &Engine, id: &str) -> JobView {
    for _ in 0..600 {
        let j = e.clip_job(id).unwrap();
        if !matches!(j.state, JobState::Queued | JobState::Running) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job didn't finish");
}

fn ffmpeg_here() -> Option<PathBuf> {
    std::env::var_os("FERROCAST_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()).or_else(crate::golive::find_ffmpeg)
}

fn make_clip(ffmpeg: &Path, out: &Path, secs: u32, audio: bool, size: &str) {
    let mut c = std::process::Command::new(ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=size={size}:rate=30:duration={secs}"));
    if audio {
        c.args(["-f", "lavfi", "-i"]).arg(format!("sine=frequency=440:duration={secs}"));
    }
    c.args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
    if audio {
        c.args(["-c:a", "aac", "-shortest"]);
    }
    assert!(c.arg(out).status().unwrap().success());
}

#[tokio::test]
async fn two_real_clips_are_compiled_with_a_crossfade_and_loudness() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    make_clip(&ffmpeg, &clips.path().join("one.mp4"), 3, true, "640x360");
    make_clip(&ffmpeg, &clips.path().join("two.mp4"), 3, false, "320x240");
    let job = e
        .compile_clips(CompileReq {
            clips: vec!["one.mp4".into(), "two.mp4".into()],
            crossfade_s: 0.5,
            normalize: true,
            name: Some("Best of".into()),
        })
        .await
        .unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.output.as_deref(), Some("Best of.mp4"));
    assert!((done.progress - 1.0).abs() < f32::EPSILON);
    let probe = e.clip_probe("Best of.mp4").await.unwrap();
    // 3 + 3 - 0.5 of crossfade, to within a frame or two.
    let d = probe.duration_ms.unwrap();
    assert!((5300..=5800).contains(&d), "length {d}");
    assert!(probe.has_audio && probe.has_video);
    assert_eq!((probe.width, probe.height), (Some(640), Some(360)), "the first clip's size");
    assert!(!clips.path().join("Best of.mp4.part").exists());
    // The same name again gets its own file, never over the first.
    let again = e
        .compile_clips(CompileReq {
            clips: vec!["two.mp4".into(), "one.mp4".into()],
            crossfade_s: 0.0,
            normalize: false,
            name: Some("Best of".into()),
        })
        .await
        .unwrap();
    let second = finished(&e, &again.id).await;
    assert_eq!(second.state, JobState::Done, "{:?}", second.error);
    assert_eq!(second.output.as_deref(), Some("Best of 2.mp4"));
    let d2 = e.clip_probe("Best of 2.mp4").await.unwrap().duration_ms.unwrap();
    assert!((5800..=6300).contains(&d2), "a cut adds the lengths: {d2}");
    // A thumbnail comes from it, and is kept.
    let jpg = e.clip_thumbnail("Best of.mp4").await.unwrap();
    assert_eq!(&jpg[..2], &[0xff, 0xd8]);
    assert!(std::fs::read_dir(e.data_dir().join("clip-thumbs")).unwrap().count() >= 1);
    // The captions of a clip with nothing said are an error in words, not a blank file.
    assert!(!clips.path().join("Best of.srt").exists());
}

#[tokio::test]
async fn a_trim_cuts_a_real_clip_and_bad_ranges_are_refused() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    make_clip(&ffmpeg, &clips.path().join("long.mp4"), 6, true, "320x240");
    let req = |s, en| TrimReq { start_ms: s, end_ms: en, name: None, precise: true };
    for (s, en) in [(7000, 9000), (3000, 3100), (4000, 2000)] {
        assert!(
            matches!(
                e.trim_clip("long.mp4", req(s, en)).await,
                Err(EngineError::Refused { status: 400, code: "bad_trim", .. })
            ),
            "{s}-{en}"
        );
    }
    let job = e.trim_clip("long.mp4", req(1000, 3000)).await.unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.output.as_deref(), Some("long trimmed.mp4"));
    let d = e.clip_probe("long trimmed.mp4").await.unwrap().duration_ms.unwrap();
    assert!((1800..=2300).contains(&d), "length {d}");
    // An end past the clip is the end of it.
    let tail = e
        .trim_clip("long.mp4", TrimReq { start_ms: 4000, end_ms: 99_000, name: Some("tail".into()), precise: false })
        .await
        .unwrap();
    assert_eq!(finished(&e, &tail.id).await.state, JobState::Done);
    assert!(e.clip_probe("tail.mp4").await.is_ok());
}

#[tokio::test]
async fn a_compilation_is_checked_before_it_starts() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    make_clip(&ffmpeg, &clips.path().join("a.mp4"), 2, true, "320x240");
    std::fs::write(clips.path().join("junk.mp4"), b"not a video").unwrap();
    let c = |clips: &[&str], fade| CompileReq {
        clips: clips.iter().map(|s| s.to_string()).collect(),
        crossfade_s: fade,
        normalize: false,
        name: None,
    };
    assert!(
        matches!(e.compile_clips(c(&["a.mp4"], 0.0)).await, Err(EngineError::Refused { code: "bad_compile", .. })),
        "one clip"
    );
    assert!(
        matches!(
            e.compile_clips(c(&["a.mp4", "a.mp4"], 9.0)).await,
            Err(EngineError::Refused { code: "bad_compile", .. })
        ),
        "fade too long"
    );
    assert!(matches!(
        e.compile_clips(c(&["a.mp4", "a.mp4"], f32::NAN)).await,
        Err(EngineError::Refused { code: "bad_compile", .. })
    ));
    assert!(
        matches!(
            e.compile_clips(c(&["a.mp4", "a.mp4"], 1.5)).await,
            Err(EngineError::Refused { code: "bad_compile", .. })
        ),
        "fade longer than half a clip"
    );
    assert!(matches!(
        e.compile_clips(c(&["a.mp4", "nope.mp4"], 0.0)).await,
        Err(EngineError::Refused { status: 404, .. })
    ));
    assert!(matches!(
        e.compile_clips(c(&["a.mp4", "../a.mp4"], 0.0)).await,
        Err(EngineError::Refused { status: 404, .. })
    ));
    assert!(matches!(
        e.compile_clips(c(&["a.mp4", "junk.mp4"], 0.0)).await,
        Err(EngineError::Refused { code: "not_a_clip", .. })
    ));
    let many: Vec<&str> = std::iter::repeat_n("a.mp4", 21).collect();
    assert!(matches!(e.compile_clips(c(&many, 0.0)).await, Err(EngineError::Refused { code: "bad_compile", .. })));
    assert!(e.clip_jobs().is_empty(), "none of those started a job");
}

#[tokio::test]
async fn cancelling_stops_ffmpeg_and_removes_the_half_made_file() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    let root = clips.path().to_path_buf();
    let slow = ffmpeg.clone();
    let job = e.start_clip_job(JobKind::Compile, "slow".into(), move |ctx| async move {
        let out = root.join("slow.mp4");
        let part = crate::clips::jobs::part_path(&out);
        let args: Vec<OsString> = [
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1920x1080:rate=30",
            "-t",
            "3600",
            "-c:v",
            "libx264",
            "-preset",
            "veryslow",
            "-f",
            "mp4",
        ]
        .iter()
        .map(OsString::from)
        .chain([part.clone().into_os_string()])
        .collect();
        crate::clips::jobs::run_ffmpeg(&ctx, &slow, args, 3_600_000, &part, &out, None).await?;
        Ok(Made { output: Some("slow.mp4".into()) })
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(e.clip_job(&job.id).unwrap().state, JobState::Running);
    e.cancel_clip_job(&job.id).unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Cancelled);
    assert!(done.error.is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let left: Vec<_> = std::fs::read_dir(clips.path()).unwrap().flatten().map(|f| f.file_name()).collect();
    assert!(left.is_empty(), "nothing left behind: {left:?}");
    // A finished job can be dismissed; a running one can't.
    e.dismiss_clip_job(&job.id).unwrap();
    assert!(e.clip_job(&job.id).is_err());
}

#[tokio::test]
async fn jobs_run_one_at_a_time_and_a_waiting_one_can_be_cancelled() {
    let (e, _d, _c) = engine().await;
    let first = e.start_clip_job(JobKind::Trim, "first".into(), |ctx| async move {
        ctx.cancel.wait().await;
        Err(crate::clips::jobs::cancelled())
    });
    let second = e.start_clip_job(JobKind::Trim, "second".into(), |_ctx| async move { Ok(Made { output: None }) });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(e.clip_job(&first.id).unwrap().state, JobState::Running);
    assert_eq!(e.clip_job(&second.id).unwrap().state, JobState::Queued, "one at a time");
    assert!(matches!(e.dismiss_clip_job(&second.id), Err(EngineError::Conflict(_))), "can't dismiss a waiting job");
    e.cancel_clip_job(&second.id).unwrap();
    assert_eq!(finished(&e, &second.id).await.state, JobState::Cancelled);
    e.cancel_clip_job(&first.id).unwrap();
    assert_eq!(finished(&e, &first.id).await.state, JobState::Cancelled);
    // A job that fails says why in words.
    let bad = e.start_clip_job(JobKind::Trim, "bad".into(), |_| async move {
        Err::<Made, _>(EngineError::Invalid("nope".into()))
    });
    let f = finished(&e, &bad.id).await;
    assert_eq!(f.state, JobState::Failed);
    assert_eq!(f.error.unwrap().message, "nope");
    assert!(matches!(e.cancel_clip_job("job-999"), Err(EngineError::Missing { code: "job_not_found", .. })));
}

#[tokio::test]
async fn burning_captions_in_works_or_says_this_ffmpeg_cant() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    make_clip(&ffmpeg, &clips.path().join("a.mp4"), 2, true, "640x360");
    // None yet: told to make them first.
    assert!(matches!(e.burn_clip_captions("a.mp4", None).await, Err(EngineError::Refused { code: "no_captions", .. })));
    e.set_clip_captions("a.mp4", vec![Cue { start_ms: 0, end_ms: 1500, text: "It's a: test 'quote'".into() }])
        .await
        .unwrap();
    if !ffmpeg_can_draw_captions(&ffmpeg).await {
        eprintln!("this ffmpeg has no `subtitles` filter (no libass): checking the refusal only");
        match e.burn_clip_captions("a.mp4", None).await {
            Err(EngineError::Refused { code: "ffmpeg_no_captions", hint, .. }) => assert!(hint.contains(".srt")),
            other => panic!("{other:?}"),
        }
        return;
    }
    let job = e.burn_clip_captions("a.mp4", Some("Captioned copy")).await.unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.output.as_deref(), Some("Captioned copy.mp4"));
    assert!(e.clip_probe("Captioned copy.mp4").await.unwrap().has_video);
    let work: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|f| f.file_name().to_string_lossy().starts_with(&format!("ferrocast-burn-{}-", std::process::id())))
        .collect();
    assert!(work.is_empty(), "the work folder is removed");
}

#[test]
fn two_saves_at_once_never_share_a_file_and_leave_none_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.srt");
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let t = target.clone();
            std::thread::spawn(move || write_new_then_rename(&t, format!("write {i}\n").repeat(500).as_bytes()))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let text = std::fs::read_to_string(&target).unwrap();
    // Whole: one writer's text, never a mix.
    let first = text.lines().next().unwrap();
    assert!(text.lines().all(|l| l == first) && text.lines().count() == 500, "{first}");
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.srt"], "no staging file is left");
}

#[tokio::test]
async fn a_clip_with_short_sound_still_crossfades_to_the_right_length() {
    let Some(ffmpeg) = ffmpeg_here() else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let (e, _d, clips) = engine().await;
    // 4 s of picture with only 1 s of sound, then 3 s with sound.
    let ok = std::process::Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=4",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .arg(clips.path().join("short-sound.mp4"))
        .status()
        .unwrap()
        .success();
    assert!(ok);
    make_clip(&ffmpeg, &clips.path().join("b.mp4"), 3, true, "320x240");
    let job = e
        .compile_clips(CompileReq {
            clips: vec!["short-sound.mp4".into(), "b.mp4".into()],
            crossfade_s: 1.0,
            normalize: false,
            name: Some("Mixed".into()),
        })
        .await
        .unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let d = e.clip_probe("Mixed.mp4").await.unwrap().duration_ms.unwrap();
    // 4 + 3 - 1 s of fade.
    assert!((5700..=6400).contains(&d), "length {d}");
}

/// Every ffmpeg run on a clip reads only files, as the one container its extension names (a
/// file's own bytes can't pick a playlist or reference demuxer that reads other files).
#[test]
fn trim_compile_burn_and_probe_read_files_only_as_the_named_container() {
    let part = Path::new("o.part");
    let check = |args: &[OsString], demux: &str, label: &str| {
        let s = arg_strs(args);
        let i = s.iter().position(|x| x == "-i").unwrap_or_else(|| panic!("{label}: no -i"));
        let before = &s[..i];
        let has = |a: &str, b: &str| before.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(has("-protocol_whitelist", "file"), "{label}: {before:?}");
        assert!(has("-f", demux), "{label}: no -f {demux}: {before:?}");
    };
    for (name, demux) in [("a.mp4", "mov"), ("a.mov", "mov"), ("a.mkv", "matroska")] {
        let p = Path::new(name);
        check(&trim_args(p, 0, 1000, false, part), demux, "trim");
        check(&burn_args(p, "subs.srt", part), demux, "burn");
        let (c, _) = compile_args(&[piece(name, 5000, true), piece(name, 5000, true)], 1280, 720, 0.5, false, part);
        // Every input, not just the first.
        let s = arg_strs(&c);
        for (n, at) in
            s.iter().enumerate().filter(|(_, x)| *x == "-i").map(|(n, _)| ((), n)).enumerate().map(|(k, (_, n))| (k, n))
        {
            let before = &s[..at];
            assert!(before.windows(2).any(|w| w[0] == "-f" && w[1] == demux), "compile input {n}: no -f {demux}");
        }
        check(&c, demux, "compile");
        check(&crate::clips::probe_args_for_test(p), demux, "probe");
    }
}

/// Burned in, a caption's `{\...}` would be read by the subtitle renderer as drawing and font
/// commands: the braces are made harmless in the copy the renderer sees (the .srt on disk keeps
/// the person's own text).
#[test]
fn ass_override_tags_cannot_reach_the_renderer() {
    let cues = vec![Cue { start_ms: 0, end_ms: 1000, text: "{\\p1}m 0 0 l 99999 99999{\\fs9999}hi {x}".into() }];
    let burn = render_srt_for_burn(&cues);
    assert!(!burn.contains('{') && !burn.contains('}'), "{burn:?}");
    assert!(burn.contains("hi"));
    // The file the person edits is untouched.
    assert!(render_srt(&cues).contains("{\\p1}"));
}

#[test]
fn the_burn_works_in_a_private_folder_made_new() {
    let a = private_work_dir().unwrap();
    let b = private_work_dir().unwrap();
    assert_ne!(a.path(), b.path());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(a.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:o}");
    }
    let p = a.path().to_path_buf();
    drop(a);
    assert!(!p.exists(), "the folder outlived its owner");
}
