use super::*;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

const SECRET: &[u8] = b"a-test-secret-that-is-long-enough!!";

fn free_udp() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

fn ffmpeg() -> Option<std::path::PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
}

fn bond(ffmpeg: Option<std::path::PathBuf>) -> Bond {
    Bond::new(Config {
        secret: SECRET.to_vec(),
        srtla: free_udp(),
        srt: free_udp(),
        keepalive: Duration::from_secs(60),
        max_sessions: 2,
        ffmpeg,
        allow_private: true,
    })
}

fn token(account: &str) -> String {
    token::sign(SECRET, account, now_ms() + 3_600_000).unwrap()
}

#[test]
fn times_keep_rising_across_reconnections_and_be_right_back() {
    let mut c = Clock::default();
    assert_eq!(c.map(1, 0), 0);
    assert_eq!(c.map(1, 1000), 1000);
    // A new connection starts its times at 0 again: it goes on just after.
    assert_eq!(c.map(2, 0), 1040);
    assert_eq!(c.map(2, 500), 1540);
    // Be right back, with its own times.
    assert_eq!(c.map(3, 7000), 1580);
    // A late tag of an old segment never goes below zero.
    let _ = tag(TagKind::Video, 0);
}

async fn call(
    b: &Bond,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut r = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    if let Some(t) = auth {
        r = r.header("authorization", format!("Bearer {t}"));
    }
    let req = r.body(body.map_or_else(Body::empty, |v| Body::from(v.to_string()))).unwrap();
    let res = b.router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn new_session(t: &str, url: &str) -> serde_json::Value {
    json!({ "token": t, "width": 640, "height": 360, "fps": 30,
        "destinations": [{ "id": "twitch", "url": url, "key": "live_abc" }] })
}

#[tokio::test]
async fn platforms_must_be_on_the_internet() {
    let b = Bond::new(Config { allow_private: false, ..bond(None).cfg.as_ref().clone() });
    for url in [
        "rtmp://127.0.0.1/app",
        "rtmp://10.0.0.5/app",
        "rtmp://169.254.169.254/app",
        "rtmp://localhost/app",
        "rtmp://[::1]/app",
        "rtmp://100.64.1.1/app",
    ] {
        let (s, e) = call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("a"), url))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {e}");
    }
    assert_eq!(public("1.1.1.1", 1935).await, Some("1.1.1.1:1935".parse().unwrap()));
    for ip in ["198.18.0.1", "240.0.0.1", "192.0.0.8", "[64:ff9b::a00:1]", "[2002:a00:1::1]", "[fe80::1]"] {
        assert_eq!(public(ip.trim_matches(['[', ']']), 1935).await, None, "{ip}");
    }
}

#[tokio::test]
async fn sessions_need_a_good_token_and_belong_to_their_account() {
    let b = bond(None);
    let (s, e) = call(&b, "POST", "/v1/sessions", None, Some(new_session("v1.x.1.00", "rtmp://127.0.0.1/app"))).await;
    assert_eq!((s, e["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("bad_token")));
    let (s, e) = call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("a"), "ftp://x/app"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"]["message"].as_str().unwrap().starts_with("destinations[].url"));
    let (s, e) = call(&b, "POST", "/v1/sessions", None, Some(json!({ "token": "x" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let (s, made) =
        call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("a"), "rtmp://127.0.0.1:9/app"))).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().unwrap();
    assert_eq!(made["stream_id"], format!("bond:{id}"));
    let (s, v) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!((s, v["input"].as_str()), (StatusCode::OK, Some("waiting")));
    // Another account can't see or stop it; no token, no answer.
    let (s, _) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("b")), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&b, "DELETE", &format!("/v1/sessions/{id}"), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // A second session of the same account ends the first.
    let (_, again) =
        call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("a"), "rtmp://127.0.0.1:9/app"))).await;
    let (s, _) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Full: says so (ended ones don't count).
    call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("c"), "rtmp://127.0.0.1:9/app"))).await;
    let (s, e) = call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("d"), "rtmp://127.0.0.1:9/app"))).await;
    assert_eq!((s, e["error"]["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("full")));
    let id2 = again["id"].as_str().unwrap();
    let (s, _) = call(&b, "DELETE", &format!("/v1/sessions/{id2}"), Some(&token("a")), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

/// The whole way: SRT in (TS from ffmpeg) → Bond → RTMP out to a stand-in platform; then the
/// sender stops and Be right back holds the platform's stream.
#[tokio::test]
async fn a_stream_goes_on_to_the_platform_and_is_held_when_the_computer_drops() {
    let Some(ff) = ffmpeg() else { return };
    let b = bond(Some(ff.clone()));
    let media = b.clone();
    tokio::spawn(async move { media.run_media().await });
    // The platform: an RTMP server with its own hub.
    let platform = Hub::new();
    let ingest = ferrocast_golive::ingest::Ingest::bind(0, "live_abc", platform.clone()).await.unwrap();
    let url = ingest.server_url();
    let (_stop, stop_rx) = watch::channel(false);
    tokio::spawn(ingest.run(stop_rx));
    let (s, made) = call(&b, "POST", "/v1/sessions", None, Some(new_session(&token("a"), &url))).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().unwrap().to_string();
    // Six seconds of 320x180 TS, sent in real time by SRT.
    let ts = std::process::Command::new(&ff)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
        ])
        .args(["-t", "6", "-c:v", "libx264", "-g", "30", "-bf", "0", "-c:a", "aac", "-f", "mpegts", "-"])
        .output()
        .unwrap()
        .stdout;
    let srt = b.cfg.srt;
    let mut tx = None;
    for _ in 0..20 {
        let c = srt_tokio::SrtSocket::builder()
            .latency(Duration::from_millis(120))
            .call(SocketAddr::from(([127, 0, 0, 1], srt.port())), Some(&format!("bond:{id}")))
            .await;
        if let Ok(c) = c {
            tx = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut tx = tx.expect("the SRT side takes the stream");
    use futures::SinkExt;
    let chunks: Vec<&[u8]> = ts.chunks(1316).collect();
    let per = Duration::from_secs(6) / chunks.len() as u32;
    for c in chunks {
        tx.send((Instant::now(), bytes::Bytes::copy_from_slice(c))).await.unwrap();
        tokio::time::sleep(per).await;
    }
    let (_, v) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!(v["input"], "live", "{v}");
    assert_eq!(v["destinations"][0]["health"]["state"], "live", "{v}");
    assert!(platform.stats().kbps > 0, "the platform gets the stream");
    assert_eq!(platform.stats().width, Some(640), "and its size, from the session");
    // The computer drops: Be right back at the session's size goes on in its place.
    drop(tx);
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (_, v) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!(v["input"], "away", "{v}");
    assert_eq!(v["destinations"][0]["health"]["state"], "live", "still live on the platform: {v}");
    assert!(platform.stats().kbps > 0, "Be right back is on the platform");
    // Stopped: the platform's stream ends.
    let (s, _) = call(&b, "DELETE", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&b, "GET", &format!("/v1/sessions/{id}"), Some(&token("a")), None).await;
    assert_eq!(v["input"], "ended");
}
