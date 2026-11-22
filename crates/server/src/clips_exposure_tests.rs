//! Clips are this PC's own: a device on the network holding a paired session can't reach them,
//! and a clip served with the session in its address is never kept by a cache.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use reqwest::StatusCode;
use tower::ServiceExt;

use super::*;

async fn rig() -> (Router, Arc<Sessions>, ferrocast_engine::Engine, tempfile::TempDir, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let clips = tempfile::tempdir().unwrap();
    let engine = ferrocast_engine::Engine::start_with_memory_secrets(data.path()).await.unwrap();
    engine.set_clips_folder_for_test(Some(clips.path().to_path_buf()));
    let sessions = Arc::new(Sessions::new());
    let app = router(AppState {
        engine: engine.clone(),
        sessions: sessions.clone(),
        guard: auth::Guard { port: 47820, lan: true, dev_origins: vec![] },
    });
    (app, sessions, engine, data, clips)
}

fn call(
    app: &Router,
    method: &str,
    uri: &str,
    peer: &str,
    token: &str,
) -> impl std::future::Future<Output = axum::http::Response<Body>> {
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "192.168.1.9:47820")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    let app = app.clone();
    async move { app.oneshot(req).await.unwrap() }
}

#[tokio::test]
async fn a_device_on_the_network_cannot_reach_the_clips() {
    let (app, sessions, engine, _d, clips) = rig().await;
    std::fs::write(clips.path().join("Game.mp4"), b"video").unwrap();
    let code = sessions.new_code();
    let paired = sessions.pair(&code).unwrap_or_else(|_| panic!("pairing"));
    let routes =
        [("GET", "/api/v1/clips"), ("DELETE", "/api/v1/clips/Game.mp4"), ("GET", "/api/v1/clips/Game.mp4/file")];
    for (m, p) in routes {
        let r = call(&app, m, p, "192.168.1.20:50000", &paired).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{m} {p} from the network");
    }
    assert!(clips.path().join("Game.mp4").exists(), "the clip was taken by the network call");
    // From this PC the same calls get through.
    for (m, p) in routes {
        let r = call(&app, m, p, "127.0.0.1:50000", &paired).await;
        assert_ne!(r.status(), StatusCode::FORBIDDEN, "{m} {p} from this PC");
    }
    engine.shutdown();
}

#[tokio::test]
async fn a_clip_file_is_never_cached() {
    let (app, sessions, engine, _d, clips) = rig().await;
    std::fs::write(clips.path().join("Game.mp4"), b"video bytes").unwrap();
    let r = call(&app, "GET", "/api/v1/clips/Game.mp4/file", "127.0.0.1:50000", sessions.desktop_token()).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("cache-control").and_then(|v| v.to_str().ok()), Some("no-store"));
    engine.shutdown();
}
