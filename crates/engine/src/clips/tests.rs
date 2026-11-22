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
    // A phone's portrait clip is stored sideways with a rotation: its shape is the turned one.
    let phone = parse_probe(
        "  Duration: 00:00:03.00, start: 0\n  Stream #0:0: Video: h264, yuv420p, 1920x1080, 30 fps\n    Side data:\n      displaymatrix: rotation of -90.00 degrees\n",
    );
    assert_eq!((phone.width, phone.height), (Some(1080), Some(1920)));
    let upside =
        parse_probe("  Stream #0:0: Video: h264, 1920x1080, 30 fps\n      displaymatrix: rotation of 180.00 degrees\n");
    assert_eq!((upside.width, upside.height), (Some(1920), Some(1080)), "a half turn keeps the shape");
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

#[tokio::test]
async fn a_rename_answers_for_a_clip_past_the_listing_cap() {
    let (e, _d, clips) = engine().await;
    // More than the 2,000 the list holds: the clip renamed may be one it leaves out.
    for i in 0..2005 {
        touch(clips.path(), &format!("f{i:04}.mp4"), b"x");
    }
    assert_eq!(e.clips(None).await.unwrap().len(), 2000);
    let made = e.rename_clip("f0000.mp4", "Renamed").await.unwrap();
    assert_eq!(made.id, "Renamed.mp4");
    assert!(clips.path().join("Renamed.mp4").is_file());
}

#[tokio::test]
async fn captions_shared_by_two_clips_with_one_name_stay_with_the_one_left() {
    let (e, _d, clips) = engine().await;
    let trash = tempfile::tempdir().unwrap();
    e.set_trash_folder_for_test(Some(trash.path().to_path_buf()));
    // a.mp4 and a.mkv are two clips whose captions file is the same a.srt.
    touch(clips.path(), "a.mp4", b"1");
    touch(clips.path(), "a.mkv", b"2");
    touch(clips.path(), "a.srt", b"1\n");
    assert!(!srt_is_exclusive(&clips.path().join("a.mp4")));
    e.rename_clip("a.mkv", "b").await.unwrap();
    assert!(clips.path().join("a.srt").is_file(), "not taken from the clip that keeps the name");
    assert!(!clips.path().join("b.srt").exists());
    // Now a.mp4 is the only clip called a: its captions go with it.
    assert!(srt_is_exclusive(&clips.path().join("a.mp4")));
    e.delete_clip("a.mp4").await.unwrap();
    assert!(trash.path().join("a.srt").is_file());
    // And deleting one of two sharing a name leaves the captions for the other.
    touch(clips.path(), "c.mp4", b"1");
    touch(clips.path(), "c.mov", b"2");
    touch(clips.path(), "c.srt", b"1\n");
    e.delete_clip("c.mov").await.unwrap();
    assert!(clips.path().join("c.srt").is_file() && clips.path().join("c.mp4").is_file());
}

#[test]
fn a_move_never_replaces_and_never_follows_a_link() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "from.mp4", b"new");
    touch(dir.path(), "taken.mp4", b"old");
    let err = place_exclusive(&dir.path().join("from.mp4"), &dir.path().join("taken.mp4")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(dir.path().join("taken.mp4")).unwrap(), b"old");
    assert!(dir.path().join("from.mp4").is_file(), "the source stays when the name is taken");
    place_exclusive(&dir.path().join("from.mp4"), &dir.path().join("free.mp4")).unwrap();
    assert_eq!(std::fs::read(dir.path().join("free.mp4")).unwrap(), b"new");
    assert!(!dir.path().join("from.mp4").exists());
    assert!(reserve(&dir.path().join("free.mp4")).is_err(), "a name that is there can't be taken");
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), dir.path().join("link.mp4")).unwrap();
        assert!(reserve(&dir.path().join("link.mp4")).is_err(), "a link isn't a free name, even a dangling one");
        assert!(!outside.path().join("target").exists());
    }
}

#[test]
fn the_freedesktop_trash_gets_an_info_file_so_it_can_be_restored() {
    let (home, trash) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let clip = home.path().join("My clip.mp4");
    std::fs::write(&clip, b"1").unwrap();
    trash_freedesktop(&clip, trash.path()).unwrap();
    assert!(!clip.exists() && trash.path().join("files/My clip.mp4").is_file());
    let info = std::fs::read_to_string(trash.path().join("info/My clip.mp4.trashinfo")).unwrap();
    let lines: Vec<&str> = info.lines().collect();
    assert_eq!(lines[0], "[Trash Info]");
    let want = format!("Path={}", percent_path(&std::fs::canonicalize(home.path()).unwrap().join("My clip.mp4")));
    assert_eq!(lines[1], want);
    assert!(lines[1].contains("My%20clip.mp4"));
    assert!(lines[2].starts_with("DeletionDate=20") && lines[2].len() == "DeletionDate=2026-11-22T10:00:00".len());
    // The same name again is a new entry with its own info, never over the first.
    std::fs::write(&clip, b"2").unwrap();
    trash_freedesktop(&clip, trash.path()).unwrap();
    assert_eq!(std::fs::read(trash.path().join("files/My clip.mp4")).unwrap(), b"1");
    assert_eq!(std::fs::read(trash.path().join("files/My clip 2.mp4")).unwrap(), b"2");
    assert!(trash.path().join("info/My clip 2.mp4.trashinfo").is_file());
    assert_eq!(percent_path(std::path::Path::new("/a b/\u{e9}.mp4")), "/a%20b/%C3%A9.mp4");
    assert_eq!(iso_time(0), "1970-01-01T00:00:00");
    assert_eq!(iso_time(1_700_000_000), "2023-11-14T22:13:20");
    assert_eq!(iso_time(951_782_400 + 86_399), "2000-02-29T23:59:59");
}

#[test]
fn old_pictures_are_pruned_to_a_limit() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..120 {
        let p = dir.path().join(format!("{i:03}.jpg"));
        std::fs::write(&p, b"x").unwrap();
        // The older the lower the number.
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    }
    std::fs::write(dir.path().join("note.txt"), b"not a picture").unwrap();
    prune_thumbs(dir.path(), 100);
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.contains(&"note.txt".to_string()) && left.contains(&"119.jpg".to_string()), "newest and others stay");
    assert!(!left.contains(&"000.jpg".to_string()), "the oldest go");
    let pics = left.iter().filter(|n| n.ends_with(".jpg")).count();
    assert!((85..=95).contains(&pics), "{pics}");
    // Under the limit: nothing goes.
    prune_thumbs(dir.path(), 500);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), pics + 1);
}

#[cfg(unix)]
#[tokio::test]
async fn a_folder_that_cannot_be_read_is_said_not_shown_empty() {
    use std::os::unix::fs::PermissionsExt;
    let (e, _d, clips) = engine().await;
    touch(clips.path(), "a.mp4", b"1");
    std::fs::set_permissions(clips.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = e.clips(None).await;
    std::fs::set_permissions(clips.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::read_dir(clips.path()).is_ok() && r.is_ok() {
        // Running as a user who can read anything: nothing to check.
        return;
    }
    assert!(matches!(r, Err(EngineError::Refused { status: 422, code: "folder_unreadable", .. })), "{r:?}");
}

#[tokio::test]
async fn a_rename_never_takes_captions_that_belong_to_another_clip() {
    let (e, _d, clips) = engine().await;
    touch(clips.path(), "one.mp4", b"1");
    touch(clips.path(), "two.mkv", b"2");
    touch(clips.path(), "two.srt", b"1\n");
    // two.mp4 is free, but two.srt is two.mkv's: the clip would show them as its own.
    assert!(matches!(
        e.rename_clip("one.mp4", "two").await,
        Err(EngineError::Refused { status: 409, code: "clip_name_taken", .. })
    ));
    assert!(clips.path().join("one.mp4").is_file(), "nothing moved");
}

#[cfg(unix)]
#[test]
fn a_move_that_cannot_remove_the_old_name_takes_the_new_one_back() {
    use std::os::unix::fs::PermissionsExt;
    let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // Same disk (so a link works); the source folder can't be changed.
    let from = src.path().join("a.mp4");
    std::fs::write(&from, b"1").unwrap();
    std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let (to, to2) = (dst.path().join("a.mp4"), dst.path().join("b.mp4"));
    let (r, moved) = (place_exclusive(&from, &to), move_without_replacing(&from, &to2));
    std::fs::set_permissions(src.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if r.is_ok() {
        // Running as a user for whom permissions don't bind (root): nothing to check.
        return;
    }
    assert!(moved.is_err());
    assert!(from.is_file(), "the source is still there");
    assert!(!to.exists() && !to2.exists(), "no half-made second name");
}

#[cfg(unix)]
#[test]
fn a_trash_record_keeps_the_bytes_of_a_folder_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let p = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/v/caf\xe9/clip.mp4"));
    assert_eq!(percent_path(p), "/v/caf%E9/clip.mp4");
}

#[test]
fn the_files_a_recording_is_writing_are_named_from_its_stats() {
    let stats = serde_json::json!({
        "active": true,
        "file": "/Users/me/Videos/Ferrocast/Stream 2026-11-22.mkv",
        "files": ["/Users/me/Videos/Ferrocast/Cam 2026-11-22.mkv", "relative.mp4", 7],
    });
    let names = active_recording_names(&stats);
    assert!(names.contains("Stream 2026-11-22.mkv"));
    assert!(names.contains("Cam 2026-11-22.mkv"));
    assert!(names.contains("relative.mp4"));
    assert!(active_recording_names(&serde_json::Value::Null).is_empty());
    // Not an active recording: no names (a stopped one's file is a clip like any other).
    assert!(active_recording_names(&serde_json::json!({ "active": false, "file": "/x/y.mkv" })).is_empty());
}

#[test]
fn a_clip_that_is_being_recorded_cannot_be_used() {
    let names: std::collections::HashSet<String> = ["Live.mkv".to_string()].into();
    let e = refuse_if_recording(&names, "Live.mkv").unwrap_err();
    assert!(matches!(e, EngineError::Refused { status: 409, code: "clip_in_use", .. }), "{e:?}");
    assert!(refuse_if_recording(&names, "Other.mkv").is_ok());
}

/// A hard link to a file outside the folder is a clip nobody can use: it would stream, upload or
/// trash a file that isn't in the clips folder at all.
#[cfg(unix)]
#[test]
fn a_hard_linked_file_is_not_a_clip() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(outside.path(), "secret.mp4", b"secret");
    std::fs::hard_link(outside.path().join("secret.mp4"), dir.path().join("linked.mp4")).unwrap();
    touch(dir.path(), "plain.mp4", b"plain");
    assert!(resolve(dir.path(), "linked.mp4").is_err(), "a hard link resolved");
    assert!(entry_at(dir.path(), "linked.mp4").is_none(), "a hard link was listed");
    assert!(resolve(dir.path(), "plain.mp4").is_ok());
    assert!(list_in(dir.path(), None).iter().all(|c| c.id != "linked.mp4"));
}

/// A `.srt` that is a link is not captions: not shown as captions, not moved with the clip, not
/// trashed with it (the Trash would copy what it points at).
#[cfg(unix)]
#[tokio::test]
async fn a_linked_srt_beside_a_clip_is_left_alone() {
    let (e, _data, clips) = engine().await;
    let outside = tempfile::tempdir().unwrap();
    let trash = tempfile::tempdir().unwrap();
    e.set_trash_folder_for_test(Some(trash.path().to_path_buf()));
    touch(outside.path(), "id_rsa", b"PRIVATE KEY");
    touch(clips.path(), "Game.mp4", b"video");
    std::os::unix::fs::symlink(outside.path().join("id_rsa"), clips.path().join("Game.srt")).unwrap();
    assert!(!entry_at(clips.path(), "Game.mp4").unwrap().has_captions, "a linked .srt shows as captions");
    e.delete_clip("Game.mp4").await.unwrap();
    // The clip is in the Trash; what the link pointed at was not copied there, and is untouched.
    let trashed: Vec<String> = std::fs::read_dir(trash.path())
        .unwrap()
        .map(|x| x.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert_eq!(std::fs::read(outside.path().join("id_rsa")).unwrap(), b"PRIVATE KEY");
    assert!(std::fs::symlink_metadata(clips.path().join("Game.srt")).is_ok(), "the link itself was taken");
}

#[test]
fn a_name_fits_the_file_system_in_bytes_and_a_device_name_with_a_space_is_still_one() {
    // 120 three-byte characters are 360 bytes: over what a file system takes for a name.
    assert!(new_stem(&"€".repeat(100)).is_err(), "a name of many bytes");
    assert!(new_stem(&"a".repeat(100)).is_ok());
    for dev in ["CON .mp4", "nul  .mkv", "Com1 .mp4"] {
        assert!(plain_name(dev).is_err(), "{dev:?} is a Windows device name");
    }
}

#[test]
fn ffmpeg_out_of_disk_is_said_as_that_not_as_a_bad_clip() {
    let stderr = "Input #0, mov, from 'a.mp4':\nStream mapping:\n[out#0/mp4] Error writing trailer: No space left on device\nError closing file x.part: No space left on device\n";
    let e = crate::clips::jobs::ffmpeg_trouble_from(stderr);
    let EngineError::Refused { message, hint, status, .. } = e else { panic!() };
    assert_eq!(status, 507, "insufficient storage");
    assert!(message.to_lowercase().contains("disk") || message.to_lowercase().contains("space"), "{message}");
    assert!(hint.to_lowercase().contains("free"), "{hint}");
    // Any other failure keeps the line that says what happened (the last, not an early warning).
    let other = crate::clips::jobs::ffmpeg_trouble_from(
        "warning: something odd\nx.mp4: Invalid data found when processing input\n",
    );
    let EngineError::Refused { message, .. } = other else { panic!() };
    assert!(message.contains("Invalid data"), "{message}");
}

#[test]
fn only_the_streams_own_lines_set_the_duration_and_the_rotation() {
    let text = "Input #0, mov,mp4, from 'a.mp4':\n  Metadata:\n    title           : Duration: 99:00:00.00, start: 0\n    comment         : rotation of -90.00 degrees\n  Duration: 00:00:10.00, start: 0.000000, bitrate: 1000 kb/s\n  Stream #0:0: Video: h264, yuv420p, 1920x1080, 30 fps\n";
    let p = parse_probe(text);
    assert_eq!(p.duration_ms, Some(10_000), "a title tag set the length");
    assert_eq!((p.width, p.height), (Some(1920), Some(1080)), "a comment tag turned the picture");
}

/// On a case-insensitive disk (macOS, Windows) "boss.mp4" and "Boss.mp4" are one file: renaming
/// to the other spelling is a rename, not a name clash.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rename_that_only_changes_the_case_works() {
    let (e, _data, clips) = engine().await;
    touch(clips.path(), "boss.mp4", b"video");
    let renamed = e.rename_clip("boss.mp4", "Boss").await.unwrap();
    assert_eq!(renamed.id, "Boss.mp4");
    let names: Vec<String> = std::fs::read_dir(clips.path())
        .unwrap()
        .map(|x| x.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["Boss.mp4".to_string()], "{names:?}");
}

/// A caption file that can't be read is said, not shown as no captions.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_caption_file_is_an_error_not_an_empty_list() {
    use std::os::unix::fs::PermissionsExt;
    let (e, _data, clips) = engine().await;
    touch(clips.path(), "a.mp4", b"video");
    touch(clips.path(), "a.srt", &[0xff, 0xfe, 0x00, 0x80, 0x81]);
    let r = e.clip_captions("a.mp4").await;
    assert!(matches!(r, Err(EngineError::Refused { code: "bad_captions", .. })), "{r:?}");
    let _ = std::fs::set_permissions(clips.path().join("a.srt"), std::fs::Permissions::from_mode(0o644));
}
