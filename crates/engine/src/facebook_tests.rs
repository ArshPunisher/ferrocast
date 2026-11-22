//! Facebook as an account: signing in saves the Page's token in the secret store, registers the
//! connector, and signing out forgets all of it. Against a fake Graph API only.

use std::sync::Arc;

use axum::Json;
use axum::routing::get;
use ferrocast_core::event::Platform;
use serde_json::json;

use ferrocast_secrets::Secrets as _;

use crate::{Engine, EngineError, PlatformApps};

/// A fake relay: it registers an install and starts a Facebook sign-in (the real one holds the
/// Meta app secret; the desktop never does). Every sign-in request's Authorization is kept.
async fn fake_relay() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::http::HeaderMap;
    use axum::routing::post;
    let auths: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let a = auths.clone();
    let app = axum::Router::new()
        .route("/v1/installs", post(|| async { Json(json!({ "install_id": "i1", "install_secret": "s1" })) }))
        .route(
            "/v1/facebook/oauth",
            post(move |h: HeaderMap| {
                let a = a.clone();
                async move {
                    a.lock()
                        .unwrap()
                        .push(h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                    Json(json!({ "url": "https://www.facebook.com/v21.0/dialog/oauth?client_id=APP&state=st" }))
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, auths)
}

async fn engine_with(
    graph: String,
    configured: bool,
) -> (Engine, tempfile::TempDir, Arc<ferrocast_secrets::MemorySecrets>) {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let relay_url = if configured { Some(fake_relay().await.0) } else { None };
    let apps =
        PlatformApps { facebook_endpoints: ferrocast_facebook::Endpoints { graph }, relay_url, ..Default::default() };
    let e = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    (e, dir, secrets)
}

async fn quiet_graph() -> String {
    // Nothing live: the comment poll finds no video and waits.
    let app = axum::Router::new().route("/{page}/live_videos", get(|| async { Json(json!({ "data": [] })) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

fn grant(pages: usize) -> ferrocast_facebook::UserGrant {
    ferrocast_facebook::UserGrant {
        user_id: "u1".into(),
        user_name: "Dev".into(),
        pages: (0..pages)
            .map(|i| ferrocast_facebook::Page {
                id: format!("p{i}"),
                name: format!("Page {i}"),
                access_token: "PAGE-TOKEN-SECRET".into(),
                avatar_url: None,
            })
            .collect(),
    }
}

#[tokio::test]
async fn connecting_without_a_relay_says_so_and_changes_nothing() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, false).await;
    let err = e.connect_account(Platform::Facebook).await.unwrap_err();
    assert!(matches!(&err, EngineError::Unavailable(m) if m.contains("relay")), "{err:?}");
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().is_none());
}

#[tokio::test]
async fn facebook_sign_in_goes_through_the_relay_with_the_installs_auth_and_no_secret() {
    let (relay, auths) = fake_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let apps = PlatformApps { relay_url: Some(relay), ..Default::default() };
    // The desktop's app settings have no Meta secret to hold (the type has no place for one).
    assert!(!format!("{apps:?}").to_lowercase().contains("secret"), "{apps:?}");
    let e = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    let flow = e.connect_account(Platform::Facebook).await.unwrap();
    match flow {
        crate::accounts::ConnectFlow::Browser { url } => {
            assert!(url.starts_with("https://www.facebook.com/"), "{url}");
        }
        other => panic!("expected a browser sign-in, got {other:?}"),
    }
    assert_eq!(auths.lock().unwrap().as_slice(), ["Bearer i1.s1"]);
    // Nothing is connected until the relay hands the Pages over.
    assert!(e.connector(Platform::Facebook).is_none());
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().is_none());
}

#[tokio::test]
async fn the_pages_the_relay_hands_over_become_the_facebook_account() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, true).await;
    let payload = json!({
        "user": { "id": "u1", "name": "Dev Person" },
        "pages": [
            { "id": "p1", "name": "My Page", "access_token": "PAGE-TOKEN-SECRET", "avatar_url": "https://x/p.png" },
            { "id": "p2", "name": "Other", "access_token": "OTHER-TOKEN" }
        ]
    });
    e.handle_relay_item("facebook_connected", &payload).await;
    assert!(e.connector(Platform::Facebook).is_some());
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().unwrap().contains("PAGE-TOKEN-SECRET"));
    assert_eq!(e.state().connectors.get("facebook").map(|c| c.state.clone()).as_deref(), Some("connected"));
}

#[tokio::test]
async fn a_relay_answer_with_no_page_or_a_broken_one_connects_nothing() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, true).await;
    for payload in [
        json!({ "user": { "id": "u1", "name": "Dev" }, "pages": [] }),
        json!({ "user": { "id": "u1" } }),
        json!({ "pages": "nope" }),
        json!(null),
    ] {
        e.handle_relay_item("facebook_connected", &payload).await;
        assert!(e.connector(Platform::Facebook).is_none(), "{payload}");
    }
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().is_none());
}

#[tokio::test]
async fn a_signed_in_page_is_saved_registered_and_forgotten_on_sign_out() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, true).await;
    e.finish_facebook(grant(2)).await.unwrap();
    // The first Page is the account; its token is in the secret store, and nowhere else.
    let saved = secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().unwrap();
    assert!(saved.contains("PAGE-TOKEN-SECRET") && saved.contains("\"p0\""), "{saved}");
    assert!(e.connector(Platform::Facebook).is_some(), "the connector is registered");
    let view = e.status();
    let fb = &view["connectors"]["facebook"];
    assert_eq!(fb["state"], "connected", "{view}");
    assert_eq!(fb["account"]["name"], "Page 0");
    // The account row says who, without any token.
    let accounts = e
        .db(ferrocast_store::repo::settings::accounts)
        .await
        .unwrap()
        .iter()
        .map(|a| format!("{} {} {:?}", a.platform, a.display_name, a.scopes))
        .collect::<String>();
    assert!(accounts.contains("facebook") && !accounts.contains("PAGE-TOKEN-SECRET"), "{accounts}");

    e.disconnect_account(Platform::Facebook).await.unwrap();
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().is_none(), "the token is forgotten");
    assert!(e.connector(Platform::Facebook).is_none());
}

#[tokio::test]
async fn a_facebook_login_with_no_page_says_what_to_do() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, true).await;
    let err = e.finish_facebook(grant(0)).await.unwrap_err();
    assert!(matches!(&err, EngineError::Unavailable(m) if m.contains("Page")), "{err:?}");
    assert!(secrets.get(crate::accounts::FACEBOOK_PAGE).unwrap().is_none());
    assert!(e.connector(Platform::Facebook).is_none());
}

#[tokio::test]
async fn a_saved_page_comes_back_on_the_next_start() {
    let graph = quiet_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let apps = || PlatformApps {
        facebook_endpoints: ferrocast_facebook::Endpoints { graph: graph.clone() },
        ..Default::default()
    };
    let first = Engine::start_with(dir.path(), secrets.clone(), apps()).await.unwrap();
    first.finish_facebook(grant(1)).await.unwrap();
    first.shutdown();
    let again = Engine::start_with(dir.path(), secrets, apps()).await.unwrap();
    again.resume_accounts().await;
    assert!(again.connector(Platform::Facebook).is_some());
}

/// A Graph that writes down every call it gets (method and path), whatever it is.
async fn recording_graph() -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |m: axum::http::Method, u: axum::http::Uri| {
        let s = s.clone();
        async move {
            s.lock().unwrap().push((m.to_string(), u.path().to_string()));
            if u.path().ends_with("/live_videos") && m == axum::http::Method::GET {
                return Json(json!({ "data": [] }));
            }
            Json(json!({ "success": true }))
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, seen)
}

#[tokio::test]
async fn a_facebook_delete_only_goes_to_a_comment_ferrocast_has_seen() {
    let (graph, seen) = recording_graph().await;
    let (e, _d, _s) = engine_with(graph, true).await;
    e.finish_facebook(grant(1)).await.unwrap();
    let deletes = || seen.lock().unwrap().iter().filter(|(m, _)| m == "DELETE").count();

    // A Page post, a live video, a permission: not comments Ferrocast saw, so nothing is sent.
    for id in ["999_111", "me/permissions", "../123", "1?method=post"] {
        let err = e.delete_message(Platform::Facebook, id).await.unwrap_err();
        assert!(matches!(err, EngineError::Missing { code: "facebook_comment_unknown", .. }), "{id}: {err:?}");
    }
    assert_eq!(deletes(), 0);

    // A comment that came in on the live video is deleted.
    let ev = ferrocast_facebook::feed::chat_event(&ferrocast_facebook::Comment {
        id: "1234_5678".into(),
        message: "hello".into(),
        from_id: Some("u1".into()),
        from_name: "Asha".into(),
        avatar_url: None,
        created_ms: 1_000,
    });
    e.db(move |s| ferrocast_store::repo::events::insert(s, &ev)).await.unwrap();
    e.delete_message(Platform::Facebook, "1234_5678").await.unwrap();
    assert_eq!(deletes(), 1);
    assert!(seen.lock().unwrap().iter().any(|(m, p)| m == "DELETE" && p == "/1234_5678"));
}

#[tokio::test]
async fn signing_in_to_facebook_tells_open_dashboards_at_once() {
    let (e, _d, _s) = engine_with(quiet_graph().await, true).await;
    let mut live = e.subscribe_live();
    e.finish_facebook(grant(1)).await.unwrap();
    let mut told = false;
    while let Ok(f) = live.try_recv() {
        if f.text.contains("\"type\":\"status\"") && f.text.contains("\"facebook\"") && f.text.contains("connected") {
            told = true;
        }
    }
    assert!(told, "a status frame with Facebook connected, so a waiting Connect dialog closes");
}

#[tokio::test]
async fn a_page_token_that_lapsed_shows_sign_in_needed() {
    let app = axum::Router::new().route(
        "/{page}/live_videos",
        get(|| async {
            (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "error": { "code": 190, "message": "expired" } })))
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    let (e, _d, _s) = engine_with(base, true).await;
    e.finish_facebook(grant(1)).await.unwrap();
    let mut state = String::new();
    for _ in 0..40 {
        state = e.state().connectors.get("facebook").map(|c| c.state.clone()).unwrap_or_default();
        if state == "sign_in_needed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(state, "sign_in_needed");
}
