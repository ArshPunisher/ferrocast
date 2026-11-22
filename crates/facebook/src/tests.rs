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
#[tokio::test]
async fn the_sign_in_flow_checks_state_and_trades_the_code_for_pages() {
    let token_calls = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let tc = token_calls.clone();
    let app = axum::Router::new()
        .route(
            "/oauth/access_token",
            post(move |axum::extract::RawQuery(q): axum::extract::RawQuery, body: String| {
                let tc = tc.clone();
                async move {
                    tc.lock().unwrap().push((q.unwrap_or_default(), body.clone()));
                    Json(json!({ "access_token": if body.contains("fb_exchange_token") { "LONG" } else { "SHORT" } }))
                }
            }),
        )
        .route("/me", get(|| async { Json(json!({ "id": "u9", "name": "Dev Person" })) }))
        .route(
            "/me/accounts",
            get(|| async { Json(json!({ "data": [ { "id": "p1", "name": "My Page", "access_token": "PAGE-T", "picture": { "data": { "url": "https://x/p.png" } } } ] })) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let ep = Endpoints {
        authorize: "https://www.facebook.com/dialog/oauth".into(),
        graph: format!("http://127.0.0.1:{port}"),
    };
    let creds = oauth::AppCredentials { app_id: "APP".into(), app_secret: "APP-SECRET".into() };

    // 1. A callback with the wrong state is refused.
    let pending = oauth::begin(&ep, &creds).await.unwrap();
    assert!(pending.url.contains("client_id=APP") && pending.url.contains("pages_manage_posts"));
    assert!(!pending.url.contains("APP-SECRET"), "the secret is never in the address");
    let redirect = url::Url::parse(&pending.url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    let http = http_client();
    let t = tokio::spawn({
        let (ep, creds, http) = (ep.clone(), creds.clone(), http.clone());
        async move { oauth::finish(&http, &ep, &creds, pending, std::time::Duration::from_secs(5)).await }
    });
    let _ = reqwest::get(format!("{redirect}?state=wrong&code=abc")).await;
    assert!(matches!(t.await.unwrap(), Err(FacebookError::SignIn(m)) if m.contains("state")));

    // 2. The right state trades the code, then the long-lived token, then lists the Pages.
    let pending = oauth::begin(&ep, &creds).await.unwrap();
    let url = url::Url::parse(&pending.url).unwrap();
    let state = url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()).unwrap();
    let redirect = url.query_pairs().find(|(k, _)| k == "redirect_uri").map(|(_, v)| v.into_owned()).unwrap();
    let t = tokio::spawn({
        let (ep, creds, http) = (ep.clone(), creds.clone(), http.clone());
        async move { oauth::finish(&http, &ep, &creds, pending, std::time::Duration::from_secs(5)).await }
    });
    let _ = reqwest::get(format!("{redirect}?state={state}&code=thecode")).await;
    let grant = t.await.unwrap().unwrap();
    assert_eq!(grant.user_name, "Dev Person");
    assert_eq!(grant.pages.len(), 1);
    assert_eq!(grant.pages[0].access_token, "PAGE-T");
    let calls = token_calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.contains("code=thecode") && calls[1].1.contains("fb_exchange_token=SHORT"), "{calls:?}");
    // The secret travelled in the body, never in the query.
    assert!(
        calls.iter().all(|(q, b)| !q.contains("APP-SECRET") && b.contains("client_secret=APP-SECRET")),
        "{calls:?}"
    );
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
