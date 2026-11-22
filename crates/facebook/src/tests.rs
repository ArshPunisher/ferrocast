use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::*;
use crate::graph::split_stream_url;

#[derive(Default)]
struct Seen {
    /// (method, path, authorization header, body)
    calls: Vec<(String, String, String, Value)>,
    comments_calls: usize,
}

async fn fake_graph(seen: Arc<Mutex<Seen>>) -> String {
    let (s1, s2, s3, s4, s5) = (seen.clone(), seen.clone(), seen.clone(), seen.clone(), seen.clone());
    let app = axum::Router::new()
        .route(
            "/{page}/live_videos",
            post(move |Path(page): Path<String>, h: HeaderMap, Json(body): Json<Value>| {
                let s = s1.clone();
                async move {
                    let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    s.lock().unwrap().calls.push(("POST".into(), format!("/{page}/live_videos"), auth, body.clone()));
                    let scheduled = body.get("status").and_then(Value::as_str) == Some("SCHEDULED_UNPUBLISHED");
                    Json(json!({
                        "id": if scheduled { "lv-sched" } else { "lv-now" },
                        "secure_stream_url": "rtmps://live-api-s.facebook.com:443/rtmp/FB-123-0-KEY",
                        "permalink_url": "/page/videos/1"
                    }))
                }
            })
            .get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                let s = s2.clone();
                async move {
                    s.lock().unwrap().calls.push(("GET".into(), "/page/live_videos".into(), String::new(), json!(q)));
                    Json(json!({ "data": [{ "id": "lv-now", "status": "LIVE" }] }))
                }
            }),
        )
        .route(
            "/{id}/comments",
            get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
                let s = s3.clone();
                async move {
                    let mut g = s.lock().unwrap();
                    g.comments_calls += 1;
                    if g.comments_calls == 1 {
                        assert!(!q.contains_key("after"));
                        Json(json!({
                            "data": [
                                { "id": "c1", "message": "hello from FB", "created_time": "2026-11-22T20:15:04+0000", "from": { "id": "u1", "name": "Asha", "picture": { "data": { "url": "https://x/a.png" } } } },
                                { "id": "c2", "message": "second", "created_time": "2026-11-22T20:15:09+0000", "from": { "id": "u2", "name": "Ravi" } }
                            ],
                            "paging": { "cursors": { "after": "CUR1" } }
                        }))
                    } else {
                        assert_eq!(q.get("after").map(String::as_str), Some("CUR1"));
                        Json(json!({ "data": [ { "id": "c2", "message": "second", "created_time": "2026-11-22T20:15:09+0000", "from": { "id": "u2", "name": "Ravi" } } ] }))
                    }
                }
            })
            .post(move |Path(id): Path<String>, h: HeaderMap, Json(body): Json<Value>| {
                let s = s4.clone();
                async move {
                    let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    s.lock().unwrap().calls.push(("POST".into(), format!("/{id}/comments"), auth, body));
                    Json(json!({ "id": "new-c" }))
                }
            }),
        )
        .route(
            "/denied/live_videos",
            post(|| async {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": { "message": "(#200) Requires publish_video permission", "code": 200 } })))
            }),
        )
        .route(
            "/expired/live_videos",
            post(|| async { (StatusCode::BAD_REQUEST, Json(json!({ "error": { "message": "Error validating access token", "code": 190 } }))) }),
        )
        .route(
            "/{id}",
            post(move |Path(id): Path<String>, Json(body): Json<Value>| {
                let s = s5.clone();
                async move {
                    s.lock().unwrap().calls.push(("POST".into(), format!("/{id}"), String::new(), body));
                    Json(json!({ "success": true }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_stream_address_splits_into_server_and_key() {
    assert_eq!(
        split_stream_url("rtmps://live-api-s.facebook.com:443/rtmp/FB-1-0-abc"),
        Some(("rtmps://live-api-s.facebook.com:443/rtmp/".into(), "FB-1-0-abc".into()))
    );
    for bad in ["", "rtmps://host/", "http://host/x", "nonsense"] {
        assert_eq!(split_stream_url(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn going_live_makes_a_live_video_and_the_token_is_a_header() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_graph(seen.clone()).await;
    let g = Graph::new(&base, "page", "PAGE-TOKEN-SECRET");
    let v = g.create_live_video("My stream", Some("about it"), None).await.unwrap();
    assert_eq!(
        (v.id.as_str(), v.server.as_str(), v.key.as_str()),
        ("lv-now", "rtmps://live-api-s.facebook.com:443/rtmp/", "FB-123-0-KEY")
    );
    let s = seen.lock().unwrap();
    let (_, path, auth, body) = &s.calls[0];
    assert_eq!(path, "/page/live_videos");
    assert_eq!(auth, "Bearer PAGE-TOKEN-SECRET", "the token goes in the header");
    assert_eq!(body["status"], "LIVE_NOW");
    assert_eq!(body["title"], "My stream");
    // Neither the key nor the token shows in Debug output.
    let dbg = format!("{v:?} {g:?}");
    assert!(!dbg.contains("FB-123-0-KEY") && !dbg.contains("PAGE-TOKEN-SECRET"), "{dbg}");
}

#[tokio::test]
async fn a_scheduled_live_video_carries_its_start_time_and_is_not_live() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_graph(seen.clone()).await;
    let g = Graph::new(&base, "page", "t");
    let v = g.create_live_video("Later", None, Some(1_800_000_000)).await.unwrap();
    assert_eq!(v.id, "lv-sched");
    let s = seen.lock().unwrap();
    assert_eq!(s.calls[0].3["status"], "SCHEDULED_UNPUBLISHED");
    assert_eq!(s.calls[0].3["planned_start_time"], 1_800_000_000);
}

#[tokio::test]
async fn a_refusal_and_an_expired_sign_in_are_told_apart_and_never_carry_the_token() {
    let base = fake_graph(Arc::new(Mutex::new(Seen::default()))).await;
    let denied = Graph::new(&base, "denied", "SECRET-T").create_live_video("x", None, None).await.unwrap_err();
    assert!(matches!(&denied, FacebookError::Refused(m) if m.contains("publish_video")), "{denied:?}");
    let expired = Graph::new(&base, "expired", "SECRET-T").create_live_video("x", None, None).await.unwrap_err();
    assert_eq!(expired, FacebookError::SignInNeeded);
    // A request that can't connect says so without its address.
    let down = Graph::new("http://127.0.0.1:1", "p", "SECRET-T").current_live().await.unwrap_err();
    assert!(!down.to_string().contains("SECRET-T") && !down.to_string().contains("127.0.0.1:1"), "{down}");
}

#[tokio::test]
async fn comments_become_chat_messages_once_each() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_graph(seen.clone()).await;
    let g = Arc::new(Graph::new(&base, "page", "t"));
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let task = tokio::spawn(feed::run(g, tx, Some("lv-now".into())));
    let a = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    let b = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    task.abort();
    assert_eq!(a.platform, ferrocast_core::event::Platform::Facebook);
    let ferrocast_core::event::EventKind::ChatMessage { actor, content, .. } = &a.kind else { panic!("{a:?}") };
    assert_eq!(actor.display_name, "Asha");
    assert_eq!(actor.id.as_deref(), Some("u1"));
    assert!(content.text.contains("hello from FB"));
    assert_eq!(a.source_id.as_deref(), Some("c1"));
    assert_eq!(b.source_id.as_deref(), Some("c2"));
    // The second poll returned c2 again: it was not delivered twice.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err());
}

#[test]
fn facebook_times_are_read_with_their_offset() {
    assert_eq!(graph::parse_facebook_time("2026-11-22T20:15:04+0000"), Some(1_795_378_504_000));
    assert_eq!(graph::parse_facebook_time("2026-11-22T20:15:04+0530"), Some(1_795_358_704_000));
    assert_eq!(graph::parse_facebook_time("garbage"), None);
}

#[tokio::test]
async fn the_connector_posts_comments_to_the_live_video_and_schedules_without_going_live() {
    use ferrocast_connector::Connector;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_graph(seen.clone()).await;
    let c = FacebookConnector::new(Arc::new(Graph::new(&base, "page", "t")));
    // Reading the ingest address creates nothing.
    assert!(c.peek_ingest().await.unwrap().starts_with("rtmps://live-api-s.facebook.com"));
    assert!(seen.lock().unwrap().calls.is_empty());
    // Going live makes a video; chat then goes to it.
    let ing = c.ingest().await.unwrap();
    assert_eq!(ing.key, "FB-123-0-KEY");
    assert_eq!(c.live_video().as_deref(), Some("lv-now"));
    c.send_chat("hi everyone", None).await.unwrap();
    {
        let s = seen.lock().unwrap();
        assert!(s.calls.iter().any(|(_, p, _, b)| p == "/lv-now/comments" && b["message"] == "hi everyone"));
    }
    // A schedule makes a scheduled video, never a live one.
    let info = ferrocast_connector::StreamInfo { title: Some("Friday show".into()), ..Default::default() };
    let sched = c.schedule_broadcast(&info, 1_800_000_000_000).await.unwrap();
    assert_eq!(sched.id, "lv-sched");
    let s = seen.lock().unwrap();
    let last = s.calls.iter().rfind(|(_, p, _, _)| p == "/page/live_videos").unwrap();
    assert_eq!(last.3["status"], "SCHEDULED_UNPUBLISHED");
    assert_eq!(last.3["planned_start_time"], 1_800_000_000);
}

/// The sign-in: a state mismatch is refused, and the secret never appears in the address.
#[test]
fn the_relays_hand_over_is_read_and_a_page_without_a_token_is_left_out() {
    let g = UserGrant::from_relay(&json!({
        "user": { "id": "u1", "name": "Dev Person" },
        "pages": [
            { "id": "p1", "name": "My Page", "access_token": "T1", "avatar_url": "https://x/p.png" },
            { "id": "p2", "name": "No token" },
            { "id": "", "access_token": "T3" },
            { "id": "p4", "access_token": "T4", "avatar_url": "javascript:alert(1)" }
        ]
    }))
    .unwrap();
    assert_eq!((g.user_id.as_str(), g.user_name.as_str()), ("u1", "Dev Person"));
    assert_eq!(g.pages.len(), 2);
    assert_eq!(g.pages[0].avatar_url.as_deref(), Some("https://x/p.png"));
    assert_eq!(g.pages[1].id, "p4");
    assert_eq!(g.pages[1].avatar_url, None, "only an https picture is kept");
    for bad in [json!(null), json!({ "pages": [] }), json!({ "user": { "id": "u" }, "pages": "x" })] {
        assert!(UserGrant::from_relay(&bad).is_none(), "{bad}");
    }
}

#[tokio::test]
async fn an_id_that_is_not_a_plain_id_never_reaches_graph() {
    // Nothing listens here: a request that was sent would come back as a Network error, so a
    // Refused answer means nothing was sent.
    let g = Graph::new("http://127.0.0.1:9", "123", "TOKEN");
    let long = "1".repeat(129);
    let bad = [
        "me/permissions",
        "../123",
        "123?method=post",
        "12 3",
        "",
        "123#x",
        "a%2Fb",
        "12/comments",
        "a.b",
        "123\\456",
        "123\n",
        long.as_str(),
    ];
    for id in bad {
        assert!(matches!(g.delete_comment(id).await, Err(FacebookError::Refused(_))), "delete {id:?}");
        assert!(matches!(g.post_comment(id, "hi").await, Err(FacebookError::Refused(_))), "post {id:?}");
        assert!(matches!(g.comments(id, None).await, Err(FacebookError::Refused(_))), "comments {id:?}");
        assert!(matches!(g.update_live(id, Some("t"), None).await, Err(FacebookError::Refused(_))), "update {id:?}");
    }
    // The Page id is held to the same rule.
    let odd_page = Graph::new("http://127.0.0.1:9", "me/accounts", "TOKEN");
    assert!(matches!(odd_page.current_live().await, Err(FacebookError::Refused(_))));
    assert!(matches!(odd_page.create_live_video("t", None, None).await, Err(FacebookError::Refused(_))));
    // A real id goes through to the network.
    for ok in ["123_456", "1234567890123456", "lv-now", "A1_b2-c3"] {
        assert!(matches!(g.delete_comment(ok).await, Err(FacebookError::Network(_))), "{ok}");
    }
}

#[test]
fn a_permalink_is_an_https_address_or_nothing() {
    use crate::graph::public_link;
    // Graph writes a path relative to facebook.com.
    assert_eq!(public_link("/page/videos/1").as_deref(), Some("https://www.facebook.com/page/videos/1"));
    assert_eq!(
        public_link("https://www.facebook.com/p/videos/2").as_deref(),
        Some("https://www.facebook.com/p/videos/2")
    );
    for bad in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        "data:text/html,<script>1</script>",
        "http://www.facebook.com/p/videos/2",
        "//evil.example/x",
        "/\\evil.example/x",
        "relative/no-slash",
        "ftp://x/y",
        "",
        "https://",
        "https://user:pass@evil.example/x",
    ] {
        assert_eq!(public_link(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn a_live_video_comes_back_with_a_usable_permalink() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_graph(seen).await;
    let g = Graph::new(&base, "page", "TOKEN");
    let v = g.create_live_video("t", None, Some(1_800_000_000)).await.unwrap();
    assert_eq!(v.permalink.as_deref(), Some("https://www.facebook.com/page/videos/1"));
}

// ----------------------------------------------------------------------------- the comment feed

use crate::feed::{self, Timing};
use ferrocast_core::event::{ConnectorStatus, EventKind, Roles};

fn quick() -> Timing {
    Timing {
        poll: std::time::Duration::from_millis(20),
        idle: std::time::Duration::from_millis(20),
        recheck: std::time::Duration::from_millis(60),
        rate_cap: std::time::Duration::from_millis(50),
    }
}

async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

#[tokio::test]
async fn a_lapsed_sign_in_says_so_once_and_stops_asking() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let base = serve(axum::Router::new().route(
        "/{page}/live_videos",
        get(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::BAD_REQUEST, Json(json!({ "error": { "code": 190, "message": "expired" } })))
            }
        }),
    ))
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let g = Arc::new(Graph::new(&base, "page", "T"));
    tokio::time::timeout(std::time::Duration::from_secs(3), feed::run_with(g, tx, None, quick()))
        .await
        .expect("the feed ends by itself when the sign-in lapsed");
    let ev = rx.recv().await.expect("a status event");
    assert!(matches!(ev.kind, EventKind::Connector(ConnectorStatus::SignInNeeded)), "{:?}", ev.kind);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "it did not keep asking");
}

#[tokio::test]
async fn a_rate_limit_waits_what_facebook_said_and_a_portal_page_is_not_a_sign_out() {
    // 429 with Retry-After 7 → RateLimited(7 s); an HTML 401 (a captive portal) is not SignInNeeded.
    let base = serve(
        axum::Router::new()
            .route(
                "/{page}/live_videos",
                get(|| async { (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "7")], "slow down") }),
            )
            .route(
                "/{id}/comments",
                get(|| async { (StatusCode::UNAUTHORIZED, "<html>Please log in to the wifi</html>") }),
            ),
    )
    .await;
    let g = Graph::new(&base, "page", "T");
    assert!(
        matches!(g.current_live().await, Err(FacebookError::RateLimited(d)) if d == std::time::Duration::from_secs(7))
    );
    let e = g.comments("lv1", None).await.unwrap_err();
    assert!(!matches!(e, FacebookError::SignInNeeded), "a portal page is not a sign-out: {e:?}");
}

#[tokio::test]
async fn the_feed_waits_out_a_rate_limit_instead_of_the_idle_sleep() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let base = serve(axum::Router::new().route(
        "/{page}/live_videos",
        get(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], "slow")
            }
        }),
    ))
    .await;
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let g = Arc::new(Graph::new(&base, "page", "T"));
    // Idle is long, the rate cap short: if the feed slept the idle time it would ask once.
    let t = Timing { idle: std::time::Duration::from_secs(30), ..quick() };
    let h = tokio::spawn(feed::run_with(g, tx, None, t));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    h.abort();
    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn the_feed_moves_on_when_the_live_video_has_ended() {
    // The video is live for the first look, then the Page is not live, while the old video's
    // comments still answer 200 (a recording keeps its comments).
    let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let comment_polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (l, cp) = (looks.clone(), comment_polls.clone());
    let base = serve(
        axum::Router::new()
            .route(
                "/{page}/live_videos",
                get(move || {
                    let l = l.clone();
                    async move {
                        let n = l.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n == 0 {
                            Json(json!({ "data": [{ "id": "lv1", "status": "LIVE" }] }))
                        } else {
                            Json(json!({ "data": [] }))
                        }
                    }
                }),
            )
            .route(
                "/{id}/comments",
                get(move || {
                    let cp = cp.clone();
                    async move {
                        cp.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Json(json!({ "data": [] }))
                    }
                }),
            ),
    )
    .await;
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let g = Arc::new(Graph::new(&base, "page", "T"));
    let h = tokio::spawn(feed::run_with(g, tx, None, quick()));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let at_600 = comment_polls.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let at_1000 = comment_polls.load(std::sync::atomic::Ordering::SeqCst);
    h.abort();
    assert_eq!(at_600, at_1000, "it stopped reading the ended video's comments");
    assert!(looks.load(std::sync::atomic::Ordering::SeqCst) >= 3, "and keeps looking for the next live video");
}

#[test]
fn the_pages_own_comments_carry_the_broadcaster_role_and_the_seen_set_is_bounded() {
    let mk = |from: &str| Comment {
        id: "c".into(),
        message: "hi".into(),
        from_id: Some(from.into()),
        from_name: "N".into(),
        avatar_url: None,
        created_ms: 1,
    };
    let own = feed::chat_event_for(&mk("page1"), "page1");
    let other = feed::chat_event_for(&mk("viewer"), "page1");
    let roles = |e: &ferrocast_core::event::Event| match &e.kind {
        EventKind::ChatMessage { actor, .. } => actor.roles,
        _ => Roles::empty(),
    };
    assert!(roles(&own).contains(Roles::BROADCASTER));
    assert!(!roles(&other).contains(Roles::BROADCASTER));

    let mut seen = feed::SeenIds::new(3);
    for i in 0..3 {
        assert!(seen.insert(format!("c{i}")));
    }
    assert!(!seen.insert("c1".into()), "a repeat is not new");
    assert!(seen.insert("c3".into()));
    assert_eq!(seen.len(), 3, "never more than the cap");
    assert!(seen.insert("c0".into()), "the oldest was let go, so it counts as new again");
    seen.clear();
    assert_eq!(seen.len(), 0);
}
