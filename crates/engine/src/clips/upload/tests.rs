use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::clips::jobs::{JobState, JobView};

fn req() -> UploadReq {
    UploadReq {
        title: "Best play".into(),
        description: "from the stream".into(),
        tags: vec!["fps".into()],
        privacy: "private".into(),
        category_id: Some("20".into()),
        made_for_kids: false,
        shorts: false,
    }
}

#[test]
fn the_details_are_checked_and_privacy_defaults_to_private() {
    assert!(check(&req()).is_ok());
    let parsed: UploadReq = serde_json::from_value(json!({ "title": "x" })).unwrap();
    assert_eq!(parsed.privacy, "private", "nothing is public by accident");
    assert!(serde_json::from_value::<UploadReq>(json!({ "title": "x", "oops": 1 })).is_err());
    let with = |f: &dyn Fn(&mut UploadReq)| {
        let mut r = req();
        f(&mut r);
        r
    };
    for r in [
        with(&|r| r.title = "  ".into()),
        with(&|r| r.title = "x".repeat(101)),
        with(&|r| r.title = "a<b".into()),
        with(&|r| r.title = "a\u{7}".into()),
        with(&|r| r.description = "x".repeat(5001)),
        with(&|r| r.description = "<script>".into()),
        with(&|r| r.privacy = "everyone".into()),
        with(&|r| r.category_id = Some("20; drop".into())),
        with(&|r| r.tags = vec!["a,b".into()]),
        with(&|r| r.tags = vec!["x".repeat(61)]),
        with(&|r| r.tags = (0..31).map(|i| format!("t{i}")).collect()),
    ] {
        assert!(matches!(check(&r), Err(EngineError::Refused { status: 400, code: "bad_upload", .. })), "{r:?}");
    }
    // Tidied: tags are trimmed, empty ones dropped.
    let mut r = req();
    r.tags = vec![" a ".into(), "".into()];
    r.title = "  Hi ".into();
    let c = check(&r).unwrap();
    assert_eq!((c.title.as_str(), c.tags.as_slice()), ("Hi", ["a".to_string()].as_slice()));
}

#[test]
fn a_short_is_tagged_once() {
    let m = |t: &str, d: &str| VideoUpload {
        title: t.into(),
        description: d.into(),
        tags: vec![],
        privacy: "private".into(),
        category_id: None,
        made_for_kids: false,
    };
    assert_eq!(with_shorts_tag(m("a", "")).description, "#Shorts");
    assert_eq!(with_shorts_tag(m("a", "text\n")).description, "text\n\n#Shorts");
    assert_eq!(with_shorts_tag(m("a #shorts", "text")).description, "text");
    assert_eq!(with_shorts_tag(m("a", "x #Shorts")).description, "x #Shorts");
}

#[derive(Default)]
struct Seen {
    opened: Vec<serde_json::Value>,
    ranges: Vec<String>,
    body_bytes: usize,
    open_status: u16,
    /// How long the piece's answer is held back (a stop test).
    hold_ms: u64,
}

/// A fake YouTube: the token and upload endpoints, nothing else. The owner's real account is never
/// reached.
async fn mock(seen: Arc<Mutex<Seen>>) -> (tempfile::TempDir, Engine) {
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    let base = Arc::new(Mutex::new(String::new()));
    let (s1, s2, b1) = (seen.clone(), seen.clone(), base.clone());
    let app = axum::Router::new()
        .route(
            "/api/upload/videos",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let (s, b) = (s1.clone(), b1.clone());
                async move {
                    let mut s = s.lock().unwrap();
                    s.opened.push(body);
                    let mut h = HeaderMap::new();
                    if s.open_status == 403 {
                        return (StatusCode::FORBIDDEN, h, json!({ "error": { "message": "The caller does not have permission", "errors": [{ "reason": "forbidden" }] } }).to_string());
                    }
                    if s.open_status == 403 + 1 {
                        return (StatusCode::FORBIDDEN, h, json!({ "error": { "message": "quota", "errors": [{ "reason": "quotaExceeded" }] } }).to_string());
                    }
                    h.insert("location", HeaderValue::from_str(&format!("{}/api/session/x", b.lock().unwrap())).unwrap());
                    (StatusCode::OK, h, String::new())
                }
            }),
        )
        .route(
            "/api/session/x",
            axum::routing::put(move |h: HeaderMap, body: axum::body::Bytes| {
                let s = s2.clone();
                async move {
                    let hold = s.lock().unwrap().hold_ms;
                    if hold > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
                    }
                    let mut s = s.lock().unwrap();
                    s.ranges.push(h.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                    s.body_bytes += body.len();
                    (StatusCode::OK, HeaderMap::new(), "{\"id\":\"vid42\"}".to_string())
                }
            }),
        );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", rest.local_addr().unwrap());
    *base.lock().unwrap() = url.clone();
    tokio::spawn(async move { axum::serve(rest, app).await });
    let dir = tempfile::tempdir().unwrap();
    let apps = crate::PlatformApps {
        youtube: Some(ferrocast_youtube::AppCredentials { client_id: "c".into(), client_secret: "s".into() }),
        youtube_endpoints: ferrocast_youtube::Endpoints {
            authorize: format!("{url}/auth"),
            token: format!("{url}/token"),
            api: format!("{url}/api"),
            grpc: "http://127.0.0.1:9".into(),
        },
        ..Default::default()
    };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    let tokens = ferrocast_youtube::Tokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: crate::now().0 + 3_600_000,
    };
    let api = e.youtube_api(tokens).unwrap();
    e.register_connector(Arc::new(ferrocast_youtube::YoutubeConnector { api, live: Default::default() }));
    (dir, e)
}

async fn finished(e: &Engine, id: &str) -> JobView {
    for _ in 0..300 {
        let j = e.clip_job(id).unwrap();
        if !matches!(j.state, JobState::Queued | JobState::Running) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job didn't finish");
}

#[tokio::test]
async fn a_clip_is_sent_to_the_mock_youtube_and_its_link_comes_back() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    std::fs::write(clips.path().join("a.mp4"), vec![9u8; 5000]).unwrap();
    let job = e.upload_clip("a.mp4", UploadReq { privacy: "unlisted".into(), ..req() }).await.unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.link.as_deref(), Some("https://www.youtube.com/watch?v=vid42"));
    let s = seen.lock().unwrap();
    assert_eq!(s.body_bytes, 5000);
    assert_eq!(s.opened[0]["snippet"]["title"], "Best play");
    assert_eq!(s.opened[0]["status"]["privacyStatus"], "unlisted");
    assert!(clips.path().join("a.mp4").is_file(), "the clip stays where it is");
}

#[tokio::test]
async fn trouble_with_youtube_reads_in_plain_words_with_a_next_step() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    std::fs::write(clips.path().join("a.mp4"), vec![9u8; 100]).unwrap();
    for (status, code_part) in [(403u16, "said no"), (404u16, "daily limit")] {
        seen.lock().unwrap().open_status = status;
        let job = e.upload_clip("a.mp4", req()).await.unwrap();
        let done = finished(&e, &job.id).await;
        assert_eq!(done.state, JobState::Failed);
        let err = done.error.unwrap();
        assert!(err.message.contains(code_part), "{}", err.message);
        assert!(!err.hint.is_empty());
    }
    // Not connected: refused before any job.
    e.unregister_connector(Platform::Youtube);
    assert!(matches!(
        e.upload_clip("a.mp4", req()).await,
        Err(EngineError::Refused { code: "youtube_not_connected", status: 422, .. })
    ));
}

#[tokio::test]
async fn bad_details_and_unknown_clips_start_no_job() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    std::fs::write(clips.path().join("a.mp4"), b"x").unwrap();
    assert!(matches!(
        e.upload_clip("a.mp4", UploadReq { title: "".into(), ..req() }).await,
        Err(EngineError::Refused { status: 400, .. })
    ));
    assert!(matches!(e.upload_clip("../a.mp4", req()).await, Err(EngineError::Refused { status: 404, .. })));
    assert!(matches!(e.upload_clip("zzz.mp4", req()).await, Err(EngineError::Refused { status: 404, .. })));
    assert!(e.clip_jobs().is_empty());
    assert!(seen.lock().unwrap().opened.is_empty(), "nothing reached the mock");
}

#[tokio::test]
async fn a_short_must_be_square_or_tall_and_short_enough() {
    let Some(ffmpeg) = std::env::var_os("FERROCAST_FFMPEG")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(crate::golive::find_ffmpeg)
    else {
        eprintln!("SKIPPED: no ffmpeg on this machine");
        return;
    };
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    let make = |name: &str, size: &str| {
        let ok = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={size}:rate=10:duration=2"))
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(clips.path().join(name))
            .status()
            .unwrap()
            .success();
        assert!(ok);
    };
    make("wide.mp4", "320x180");
    make("tall.mp4", "180x320");
    let short = UploadReq { shorts: true, ..req() };
    assert!(matches!(
        e.upload_clip("wide.mp4", short.clone()).await,
        Err(EngineError::Refused { code: "bad_upload", .. })
    ));
    let job = e.upload_clip("tall.mp4", short).await.unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.link.as_deref(), Some("https://www.youtube.com/shorts/vid42"));
    assert!(seen.lock().unwrap().opened[0]["snippet"]["description"].as_str().unwrap().contains("#Shorts"));
}

#[tokio::test]
async fn the_file_picked_is_the_file_sent_even_if_another_takes_its_name_meanwhile() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    std::fs::write(clips.path().join("a.mp4"), vec![1u8; 1234]).unwrap();
    // Another job holds the line, so the upload waits its turn.
    let blocker = e.start_clip_job(crate::clips::jobs::JobKind::Trim, "block".into(), |ctx| async move {
        ctx.cancel.wait().await;
        Err(crate::clips::jobs::cancelled())
    });
    let job = e.upload_clip("a.mp4", req()).await.unwrap();
    // A different clip is saved under the same name while it waits.
    std::fs::remove_file(clips.path().join("a.mp4")).unwrap();
    std::fs::write(clips.path().join("a.mp4"), vec![2u8; 999]).unwrap();
    assert_eq!(e.clip_job(&job.id).unwrap().state, JobState::Queued);
    e.cancel_clip_job(&blocker.id).unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(seen.lock().unwrap().body_bytes, 1234, "the original went, not the replacement");
}

#[tokio::test]
async fn stopping_an_upload_near_its_end_says_youtube_may_have_it() {
    let seen = Arc::new(Mutex::new(Seen { hold_ms: 5000, ..Seen::default() }));
    let (_d, e) = mock(seen.clone()).await;
    let clips = tempfile::tempdir().unwrap();
    e.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    std::fs::write(clips.path().join("a.mp4"), vec![9u8; 5000]).unwrap();
    let job = e.upload_clip("a.mp4", req()).await.unwrap();
    // The only piece (a small file) is on its way and the answer is held back.
    for _ in 0..100 {
        if !seen.lock().unwrap().opened.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    e.cancel_clip_job(&job.id).unwrap();
    let done = finished(&e, &job.id).await;
    assert_eq!(done.state, JobState::Cancelled);
    let note = done.error.expect("a note");
    assert_eq!(note.message, "Stopped near the end.");
    assert!(note.hint.contains("YouTube Studio"));
    assert!(done.link.is_none());
}
