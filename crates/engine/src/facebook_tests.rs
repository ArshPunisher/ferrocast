//! Facebook as an account: signing in saves the Page's token in the secret store, registers the
//! connector, and signing out forgets all of it. Against a fake Graph API only.

use std::sync::Arc;

use axum::Json;
use axum::routing::get;
use ferrocast_core::event::Platform;
use serde_json::json;

use ferrocast_secrets::Secrets as _;

use crate::{Engine, EngineError, PlatformApps};

async fn engine_with(
    graph: String,
    configured: bool,
) -> (Engine, tempfile::TempDir, Arc<ferrocast_secrets::MemorySecrets>) {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let apps = PlatformApps {
        facebook: configured
            .then(|| ferrocast_facebook::AppCredentials { app_id: "APP".into(), app_secret: "APP-SECRET".into() }),
        facebook_endpoints: ferrocast_facebook::Endpoints {
            authorize: "https://www.facebook.com/dialog/oauth".into(),
            graph,
        },
        ..Default::default()
    };
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
async fn connecting_without_the_owners_app_says_so_and_changes_nothing() {
    let (e, _d, secrets) = engine_with(quiet_graph().await, false).await;
    let err = e.connect_account(Platform::Facebook).await.unwrap_err();
    assert!(matches!(&err, EngineError::Unavailable(m) if m.contains("no Facebook app")), "{err:?}");
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
        facebook: Some(ferrocast_facebook::AppCredentials { app_id: "APP".into(), app_secret: "S".into() }),
        facebook_endpoints: ferrocast_facebook::Endpoints { authorize: String::new(), graph: graph.clone() },
        ..Default::default()
    };
    let first = Engine::start_with(dir.path(), secrets.clone(), apps()).await.unwrap();
    first.finish_facebook(grant(1)).await.unwrap();
    first.shutdown();
    let again = Engine::start_with(dir.path(), secrets, apps()).await.unwrap();
    again.resume_accounts().await;
    assert!(again.connector(Platform::Facebook).is_some());
}
