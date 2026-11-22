//! Clips routes over a real socket: auth, the happy paths, and each error status with our error
//! shape. The YouTube side is never reached (no connector is registered here); the upload
//! against a mock endpoint is tested in the engine.

use std::sync::Arc;

use ferrocast_engine::Engine;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{ServeOptions, serve};

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    engine: Engine,
    clips: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

impl Running {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut r = self.client.request(method, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send().await.unwrap()
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.call(reqwest::Method::PUT, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> reqwest::Response {
        self.call(reqwest::Method::DELETE, path, None).await
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    engine.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        engine,
        clips,
        _dir: dir,
    }
}

async fn body(res: reqwest::Response) -> (StatusCode, Value) {
    let s = res.status();
    (s, res.json().await.unwrap_or(Value::Null))
}

async fn error(res: reqwest::Response) -> (StatusCode, String, String) {
    let (s, v) = body(res).await;
    (
        s,
        v["error"]["code"].as_str().unwrap_or("<no error>").to_string(),
        v["error"]["hint"].as_str().unwrap_or("").to_string(),
    )
}

#[tokio::test]
async fn every_clips_route_needs_the_session() {
    let r = start().await;
    for (m, p) in [
        ("GET", "/clips"),
        ("GET", "/clips/jobs"),
        ("GET", "/clips/a.mp4"),
        ("GET", "/clips/a.mp4/thumbnail"),
        ("GET", "/clips/a.mp4/captions"),
        ("GET", "/clips/a.mp4/captions.srt"),
        ("PUT", "/clips/a.mp4"),
        ("DELETE", "/clips/a.mp4"),
        ("POST", "/clips/compile"),
        ("POST", "/clips/a.mp4/trim"),
        ("POST", "/clips/a.mp4/upload"),
        ("POST", "/clips/a.mp4/captions"),
        ("POST", "/clips/a.mp4/captions/burn"),
        ("POST", "/clips/jobs/job-1/cancel"),
    ] {
        let res = r.client.request(m.parse().unwrap(), format!("{}/api/v1{p}", r.base)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{m} {p}");
    }
}

#[tokio::test]
async fn the_library_lists_searches_renames_and_trashes() {
    let r = start().await;
    let trash = tempfile::tempdir().unwrap();
    r.engine.set_trash_folder_for_test(Some(trash.path().to_path_buf()));
    std::fs::write(r.clips.path().join("Replay one.mp4"), b"1").unwrap();
    std::fs::write(r.clips.path().join("Other.mkv"), b"2").unwrap();
    std::fs::write(r.clips.path().join("notes.txt"), b"3").unwrap();
    let (s, v) = body(r.get("/clips").await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["clips"].as_array().unwrap().len(), 2);
    assert_eq!(v["youtube_connected"], false);
    assert!(v["folder"].as_str().unwrap().len() > 3);
    let (_, found) = body(r.get("/clips?q=replay").await).await;
    assert_eq!(found["clips"][0]["id"], "Replay one.mp4");
    assert_eq!(found["clips"][0]["kind"], "replay");
    // Rename: 200, then the clash and bad names.
    let (s, v) = body(r.put("/clips/Other.mkv", json!({ "name": "Better" })).await).await;
    assert_eq!((s, v["id"].as_str()), (StatusCode::OK, Some("Better.mkv")));
    std::fs::write(r.clips.path().join("Third.mkv"), b"3").unwrap();
    let (s, c, h) = error(r.put("/clips/Better.mkv", json!({ "name": "Third" })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "clip_name_taken"));
    assert!(!h.is_empty());
    let (s, c, _) = error(r.put("/clips/Better.mkv", json!({ "name": "../x" })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "bad_clip_name"));
    let (s, _, _) = error(r.put("/clips/Better.mkv", json!({ "name": "x", "extra": 1 })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown fields are refused");
    // Delete: 204 into the Trash, then 404.
    assert_eq!(r.delete("/clips/Better.mkv").await.status(), StatusCode::NO_CONTENT);
    assert!(trash.path().join("Better.mkv").is_file());
    let (s, c, _) = error(r.delete("/clips/Better.mkv").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "clip_not_found"));
}

#[tokio::test]
async fn a_path_in_the_name_never_reaches_a_file() {
    let r = start().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.mp4"), b"s").unwrap();
    // The encoded slash and dots stay one segment, and the name is refused as no such clip.
    for id in ["..%2Fsecret.mp4", "%2e%2e%2fsecret.mp4", "a%2Fb.mp4", "..", "secret"] {
        let (s, c, _) = error(r.delete(&format!("/clips/{id}")).await).await;
        assert!(s == StatusCode::NOT_FOUND || s == StatusCode::BAD_REQUEST, "{id}: {s}");
        let _ = c;
    }
    assert!(outside.path().join("secret.mp4").is_file());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret.mp4"), r.clips.path().join("link.mp4")).unwrap();
        let (s, c, _) = error(r.delete("/clips/link.mp4").await).await;
        assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "clip_not_found"));
        let (_, v) = body(r.get("/clips").await).await;
        assert!(v["clips"].as_array().unwrap().is_empty(), "a link isn't listed");
        assert!(outside.path().join("secret.mp4").is_file());
    }
}

#[tokio::test]
async fn captions_are_saved_read_and_downloaded() {
    let r = start().await;
    std::fs::write(r.clips.path().join("a b.mp4"), b"x").unwrap();
    let (s, v) = body(r.get("/clips/a%20b.mp4/captions").await).await;
    assert_eq!((s, v["cues"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));
    let cues = json!({ "cues": [{ "start_ms": 0, "end_ms": 1500, "text": "Hello" }] });
    let (s, v) = body(r.put("/clips/a%20b.mp4/captions", cues.clone()).await).await;
    assert_eq!((s, &v), (StatusCode::OK, &cues));
    let res = r.get("/clips/a%20b.mp4/captions.srt").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("application/x-subrip"));
    assert_eq!(res.headers()["content-disposition"], "attachment; filename=\"a b.srt\"");
    assert!(res.text().await.unwrap().contains("00:00:00,000 --> 00:00:01,500\nHello"));
    let bad = json!({ "cues": [{ "start_ms": 5, "end_ms": 1, "text": "x" }] });
    let (s, c, _) = error(r.put("/clips/a%20b.mp4/captions", bad).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "bad_captions"));
}

#[tokio::test]
async fn jobs_start_with_202_and_bad_requests_start_none() {
    let r = start().await;
    std::fs::write(r.clips.path().join("a.mp4"), b"x").unwrap();
    // Bad compile requests: 400 in words, no job.
    for b in [
        json!({ "clips": ["a.mp4"] }),
        json!({ "clips": ["a.mp4", "a.mp4"], "crossfade_s": 9 }),
        json!({ "clips": [] }),
    ] {
        let (s, c, h) = error(r.post("/clips/compile", b).await).await;
        assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "bad_compile"));
        assert!(!h.is_empty());
    }
    let (s, _, _) = error(r.post("/clips/compile", json!({ "clips": ["a.mp4", "a.mp4"], "bogus": 1 })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, c, _) = error(r.post("/clips/compile", json!({ "clips": ["a.mp4", "nope.mp4"] })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "clip_not_found"));
    // Upload: details checked first, then "YouTube isn't connected" (422) with a next step.
    let (s, c, _) = error(r.post("/clips/a.mp4/upload", json!({ "title": "", "privacy": "private" })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "bad_upload"));
    let (s, c, h) = error(r.post("/clips/a.mp4/upload", json!({ "title": "Hi" })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "youtube_not_connected"));
    assert!(h.contains("Accounts"));
    // Trim: the range is checked (or FFmpeg's absence said), never a 500.
    let (s, c, _) = error(r.post("/clips/a.mp4/trim", json!({ "start_ms": 0, "end_ms": 1000 })).await).await;
    assert!(s.is_client_error(), "{s} {c}");
    let (_, v) = body(r.get("/clips/jobs").await).await;
    assert!(v["jobs"].as_array().unwrap().is_empty(), "none of those started a job");
    // Unknown job: 404 in our shape.
    let (s, c, _) = error(r.get("/clips/jobs/job-9").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "job_not_found"));
    let (s, c, _) = error(r.post("/clips/jobs/job-9/cancel", json!({})).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "job_not_found"));
    let (s, c, _) = error(r.delete("/clips/jobs/job-9").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::NOT_FOUND, "job_not_found"));
}
