use std::sync::Arc;

use axum::Json;
use axum::routing::post;
use serde_json::json;

use super::*;
use crate::PlatformApps;

#[test]
fn the_services_words_become_health_lines() {
    let live = json!({ "state": "live", "kbps": 6000 });
    assert_eq!(health_of(&live, "live", 1).state, PushState::Live);
    assert_eq!(health_of(&live, "live", 1).kbps, 6000);
    match health_of(&live, "away", 1).state {
        PushState::Reconnecting { reason, .. } => assert!(reason.contains("Be right back"), "{reason}"),
        other => panic!("{other:?}"),
    }
    let r = json!({ "state": "reconnecting", "attempt": 3, "retry_in_ms": 4000, "reason": "Twitch refused the key." });
    assert_eq!(
        health_of(&r, "live", 1).state,
        PushState::Reconnecting { attempt: 3, retry_in_ms: 4000, reason: "Twitch refused the key.".into() }
    );
    assert_eq!(refusal(&json!({ "error": { "message": "Full." } }), 503), "Full.");
}

const SECRET: &[u8] = b"an-engine-test-bond-secret-32-chars!";

/// The whole way from this side: Go live with Bond on hands the platform to the service (a
/// real one, in this test), which sends it on; stopping the stream ends its session.
#[tokio::test]
async fn with_bond_on_the_platform_is_sent_from_the_service() {
    use ferrocast_testkit::rtmp::{Got, platform};
    let Some(ff) = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
    else {
        return;
    };
    // The service, on this computer.
    let free = || std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (srtla, srt) = (free(), free());
    let bond = ferrocast_bond::Bond::new(ferrocast_bond::Config {
        secret: SECRET.to_vec(),
        srtla,
        srt,
        keepalive: Duration::from_secs(60),
        max_sessions: 4,
        ffmpeg: None,
        allow_private: true,
        public_url: "http://127.0.0.1:8795".into(),
        rtc_ip: Some(std::net::IpAddr::from([127, 0, 0, 1])),
        rtc_ports: (41000, 41099),
        vapid: None,
    });
    let media = bond.clone();
    tokio::spawn(async move { media.run_media().await });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", l.local_addr().unwrap());
    let router = bond.router();
    tokio::spawn(async move { axum::serve(l, router).await });
    // The relay: an install, and a Bond token for it.
    let api2 = api.clone();
    let relay = axum::Router::new()
        .route(
            "/v1/installs",
            post(|| async {
                (axum::http::StatusCode::CREATED, Json(json!({"install_id": "in1", "install_secret": "sec"})))
            }),
        )
        .route(
            "/v1/pro/bond/token",
            post(move || {
                let api = api2.clone();
                async move {
                    let exp = crate::now().0 + 3_600_000;
                    Json(json!({ "token": ferrocast_bond::token::sign(SECRET, "acct", exp), "expires_ms": exp,
                        "api": api, "srtla_host": "127.0.0.1", "srtla_port": srtla.port() }))
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, relay).await });
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay_url), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    // The platform, and Go live pointed at it with Bond on.
    let (port, mut got) = platform("key-p").await;
    let d =
        e.add_golive_destination("Mine", &format!("rtmp://127.0.0.1:{port}/app"), "key-p", Canvas::Main).await.unwrap();
    e.set_golive_bond(true).await.unwrap();
    // A real stream into Go live's hub, as OBS or the Studio would send it.
    let ts = std::process::Command::new(&ff)
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30", "-f", "lavfi", "-i", "sine"])
        .args(["-t", "8", "-c:v", "libx264", "-g", "30", "-bf", "0", "-c:a", "aac", "-f", "mpegts", "-"])
        .output()
        .unwrap()
        .stdout;
    let tags = ferrocast_golive::demux::TsDemuxer::new().push(&ts);
    let hub = e.golive().hub.clone();
    hub.start();
    e.reconcile().await;
    assert!(e.golive_view().await.bond_sending, "Bond took it");
    let start = tokio::time::Instant::now();
    let feed = tokio::spawn(async move {
        for t in tags {
            tokio::time::sleep_until(start + Duration::from_millis(u64::from(t.ms))).await;
            match t.kind {
                ferrocast_golive::demux::TagKind::Video => hub.video(t.ms, t.body),
                ferrocast_golive::demux::TagKind::Audio => hub.audio(t.ms, t.body),
            }
        }
    });
    // The platform is published to (by the service) and gets pictures.
    let mut published = false;
    let mut frames = 0;
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < until && frames < 30 {
        match tokio::time::timeout(Duration::from_secs(2), got.recv()).await {
            Ok(Some(Got::Publish(k))) => published = k == "key-p",
            Ok(Some(Got::Video(..))) => frames += 1,
            _ => {}
        }
    }
    assert!(published && frames >= 30, "published {published}, {frames} frames");
    // Its health line says live, read from the service.
    let mut live = false;
    for _ in 0..30 {
        let v = e.golive_view().await;
        live = v
            .destinations
            .iter()
            .find(|x| x.id == d.id)
            .and_then(|x| x.health.clone())
            .is_some_and(|h| h.state == PushState::Live);
        if live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(live, "the health line comes from the service");
    feed.abort();
    // The stream stops: the session ends there.
    e.golive().hub.end();
    e.reconcile().await;
    let mut ended = false;
    for _ in 0..50 {
        if let Ok(Some(Got::Closed)) = tokio::time::timeout(Duration::from_millis(200), got.recv()).await {
            ended = true;
            break;
        }
    }
    assert!(ended, "the platform's stream ended");
    assert!(!e.golive_view().await.bond_sending);
}

/// "Stream from your phone": a Bond session for the ticked platforms that waits for the phone,
/// its link (secret in the fragment), its state read back, and ended.
#[tokio::test]
async fn a_phone_link_is_made_for_the_ticked_platforms() {
    let free = || std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (srtla, srt) = (free(), free());
    let bond = ferrocast_bond::Bond::new(ferrocast_bond::Config {
        secret: SECRET.to_vec(),
        srtla,
        srt,
        keepalive: Duration::from_secs(60),
        max_sessions: 4,
        ffmpeg: None,
        allow_private: true,
        public_url: "https://bond.example".into(),
        rtc_ip: Some(std::net::IpAddr::from([127, 0, 0, 1])),
        rtc_ports: (41100, 41199),
        vapid: None,
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", l.local_addr().unwrap());
    let router = bond.router();
    tokio::spawn(async move { axum::serve(l, router).await });
    let api2 = api.clone();
    let relay = axum::Router::new()
        .route(
            "/v1/installs",
            post(|| async {
                (axum::http::StatusCode::CREATED, Json(json!({"install_id": "in1", "install_secret": "sec"})))
            }),
        )
        .route(
            "/v1/pro/bond/token",
            post(move || {
                let api = api2.clone();
                async move {
                    let exp = crate::now().0 + 3_600_000;
                    Json(json!({ "token": ferrocast_bond::token::sign(SECRET, "acct", exp), "expires_ms": exp,
                        "api": api, "srtla_host": "127.0.0.1", "srtla_port": srtla.port() }))
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, relay).await });
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay_url), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    // Nothing ticked that can go (the built-in platforms aren't connected): says so.
    let order: Vec<(String, bool)> = e.golive_config().destinations.iter().map(|d| (d.id.clone(), false)).collect();
    e.set_golive_destinations(order).await.unwrap();
    assert!(e.golive_irl_start().await.unwrap_err().to_string().contains("Tick at least one platform"));
    e.add_golive_destination("Mine", "rtmp://127.0.0.1:9/app", "key-p", Canvas::Main).await.unwrap();
    let v = e.golive_irl_start().await.unwrap();
    let link = v["phone_url"].as_str().unwrap();
    assert!(link.starts_with("https://bond.example/irl#"), "{link}");
    let (sid, secret) = link.split_once('#').unwrap().1.split_once('.').unwrap();
    assert_eq!((sid.len(), secret.len()), (32, 32));
    let now = e.golive_irl().await;
    assert_eq!(now["input"], "waiting");
    assert_eq!(now["destinations"][0]["health"]["state"], "waiting");
    e.golive_irl_end().await;
    assert_eq!(e.golive_irl().await["phone_url"], serde_json::Value::Null);
}
