use std::path::Path;

use super::*;

fn touch(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

async fn engine() -> (Engine, tempfile::TempDir, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();
    let e = Engine::start_with_memory_secrets(data.path()).await.unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    (e, data, clips)
}

#[test]
fn a_name_is_one_plain_name() {
    for bad in [
        "",
        " a",
        "a ",
        ".hidden.mp4",
        "a/b.mp4",
        "..\\a.mp4",
        "a..b.mp4",
        "a:b.mp4",
        "a\u{7}.mp4",
        "CON.mp4",
        "com1",
        "nul.mp4",
        "lpt\u{b9}.mp4",
        "name.",
        "x?.mp4",
        "x|y.mp4",
    ] {
        assert!(plain_name(bad).is_err(), "{bad:?}");
    }
    for ok in ["Replay 2026-11-22 21-00-00.mp4", "my clip", "a.b.mp4", "console", "com10"] {
        assert!(plain_name(ok).is_ok(), "{ok}");
    }
    assert!(new_stem(&"x".repeat(121)).is_err());
}

#[test]
fn only_plain_video_files_in_the_folder_are_clips() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(dir.path(), "Replay 2026-11-22 21-00-00.mp4", b"a");
    touch(dir.path(), "Game vertical.mp4", b"b");
    touch(dir.path(), "Stream.mkv", b"c");
    touch(dir.path(), "notes.txt", b"d");
    touch(dir.path(), ".hidden.mp4", b"e");
    touch(dir.path(), "half.mp4.part", b"f");
    std::fs::create_dir(dir.path().join("folder.mp4")).unwrap();
    touch(outside.path(), "secret.mp4", b"s");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret.mp4"), dir.path().join("link.mp4")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dirlink")).unwrap();
    }
    let list = list_in(dir.path(), None);
    let ids: Vec<&str> = list.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(list.len(), 3, "{ids:?}");
    let kind = |id: &str| list.iter().find(|c| c.id == id).map(|c| c.kind);
    assert_eq!(kind("Replay 2026-11-22 21-00-00.mp4"), Some(ClipKind::Replay));
    assert_eq!(kind("Game vertical.mp4"), Some(ClipKind::Vertical));
    assert_eq!(kind("Stream.mkv"), Some(ClipKind::Recording));
    // A search matches any case, in the name.
    assert_eq!(list_in(dir.path(), Some("GAME")).len(), 1);
    assert!(list_in(dir.path(), Some("zzz")).is_empty());
}

#[test]
fn a_clip_is_found_only_by_a_plain_name_and_never_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(dir.path(), "a.mp4", b"a");
    touch(outside.path(), "secret.mp4", b"s");
    assert!(resolve(dir.path(), "a.mp4").is_ok());
    for bad in ["../a.mp4", "a", "a.txt", "sub/a.mp4", "/etc/passwd", "a.mp4/", "..", ""] {
        assert!(resolve(dir.path(), bad).is_err(), "{bad:?}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret.mp4"), dir.path().join("link.mp4")).unwrap();
        assert!(resolve(dir.path(), "link.mp4").is_err(), "a link is no clip");
        // The folder itself reached through a link is not the folder.
        let via = outside.path().join("via");
        std::os::unix::fs::symlink(dir.path(), &via).unwrap();
        assert!(resolve(&via, "a.mp4").is_ok(), "the folder the streamer chose may itself be a link");
    }
}

#[test]
fn ffmpegs_text_gives_the_facts() {
    let text = "Input #0, mov,mp4, from 'a.mp4':\n  Duration: 00:01:02.50, start: 0.000000, bitrate: 2000 kb/s\n  Stream #0:0(und): Video: h264 (High), yuv420p, 1920x1080 [SAR 1:1 DAR 16:9], 30 fps\n  Stream #0:1(und): Audio: aac (LC), 48000 Hz, stereo\n";
    let p = parse_probe(text);
    assert_eq!(p.duration_ms, Some(62_500));
    assert_eq!((p.width, p.height), (Some(1920), Some(1080)));
    assert!(p.has_audio && p.has_video);
    let silent =
        parse_probe("  Duration: 00:00:03.00, start: 0\n  Stream #0:0: Video: h264, yuv420p, 640x360, 25 fps\n");
    assert!(!silent.has_audio && silent.has_video);
    assert_eq!(parse_probe("nothing here"), Probe::default());
}

#[tokio::test]
async fn a_clip_is_renamed_with_its_captions_and_never_over_another() {
    let (e, _d, clips) = engine().await;
    touch(clips.path(), "one.mp4", b"1");
    touch(clips.path(), "one.srt", b"1\n");
    touch(clips.path(), "two.mp4", b"2");
    let made = e.rename_clip("one.mp4", "Best play").await.unwrap();
    assert_eq!(made.id, "Best play.mp4");
    assert!(clips.path().join("Best play.srt").is_file() && !clips.path().join("one.srt").exists());
    assert!(matches!(
        e.rename_clip("Best play.mp4", "two").await,
        Err(EngineError::Refused { status: 409, code: "clip_name_taken", .. })
    ));
    assert!(clips.path().join("Best play.mp4").is_file(), "the failed rename left it");
    for bad in ["../x", "a/b", "", ".x"] {
        assert!(matches!(e.rename_clip("two.mp4", bad).await, Err(EngineError::Refused { status: 400, .. })), "{bad}");
    }
    assert!(matches!(
        e.rename_clip("nope.mp4", "x").await,
        Err(EngineError::Refused { status: 404, code: "clip_not_found", .. })
    ));
    // The same name is no change.
    assert_eq!(e.rename_clip("two.mp4", "two").await.unwrap().id, "two.mp4");
}

#[tokio::test]
async fn deleting_moves_to_the_trash_and_keeps_names_apart() {
    let (e, _d, clips) = engine().await;
    let trash = tempfile::tempdir().unwrap();
    e.set_trash_folder_for_test(Some(trash.path().to_path_buf()));
    touch(clips.path(), "a.mp4", b"1");
    touch(clips.path(), "a.srt", b"1");
    e.delete_clip("a.mp4").await.unwrap();
    assert!(!clips.path().join("a.mp4").exists() && !clips.path().join("a.srt").exists());
    assert!(trash.path().join("a.mp4").is_file() && trash.path().join("a.srt").is_file());
    // Another with the same name goes next to it, never over it.
    touch(clips.path(), "a.mp4", b"2");
    e.delete_clip("a.mp4").await.unwrap();
    assert_eq!(std::fs::read(trash.path().join("a.mp4")).unwrap(), b"1");
    assert_eq!(std::fs::read(trash.path().join("a 2.mp4")).unwrap(), b"2");
    assert!(matches!(e.delete_clip("a.mp4").await, Err(EngineError::Refused { status: 404, .. })));
    assert!(matches!(e.delete_clip("../x.mp4").await, Err(EngineError::Refused { status: 404, .. })));
}

#[tokio::test]
async fn a_failed_trash_says_so_in_plain_words() {
    let (e, _d, clips) = engine().await;
    // The "Trash" is a file, so nothing can go into it.
    let trash = tempfile::NamedTempFile::new().unwrap();
    e.set_trash_folder_for_test(Some(trash.path().to_path_buf()));
    touch(clips.path(), "a.mp4", b"1");
    match e.delete_clip("a.mp4").await {
        Err(EngineError::Refused { status: 422, code: "trash_unavailable", hint, .. }) => assert!(!hint.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(clips.path().join("a.mp4").is_file(), "it stays where it was");
}

#[tokio::test]
async fn the_folder_is_where_recordings_go_or_a_clips_folder_in_the_data() {
    let data = tempfile::tempdir().unwrap();
    let e = Engine::start_with_memory_secrets(data.path()).await.unwrap();
    assert_eq!(e.clips_folder().await.unwrap(), data.path().join("Clips"));
    assert!(e.clips(None).await.unwrap().is_empty(), "no folder yet: no clips, no error");
}
