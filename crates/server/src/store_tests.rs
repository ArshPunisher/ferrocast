//! `/api/v1/store/*` HTTP integration tests over a real socket (R37).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode as HttpStatus;
use axum::response::Response;
use axum::routing::get;
use ferrocast_engine::plugin_catalog::Source;
use ferrocast_engine::store_catalog::set_source;
use reqwest::StatusCode;
use serde_json::{Value, json};

use super::*;

const TEST_KEY: &str = include_str!("../../engine/testdata/store_catalog/test-key.pub");
const CATALOG_V1: &[u8] = include_bytes!("../../engine/testdata/store_catalog/catalog.json");
const CATALOG_V1_SIG: &str = include_str!("../../engine/testdata/store_catalog/catalog.json.sig");
const PKG_THEME_V1: &[u8] =
    include_bytes!("../../engine/testdata/store_catalog/packages/aurora-pulse-1.0.0.ferrodesign");
const PKG_ALERT: &[u8] = include_bytes!("../../engine/testdata/store_catalog/packages/arcade-ticker-1.0.0.ferroalert");
const PKG_WIDGET: &[u8] = include_bytes!("../../engine/testdata/store_catalog/packages/foundry-goal-1.0.0.ferrodesign");
const PKG_OVERLAY: &[u8] =
    include_bytes!("../../engine/testdata/store_catalog/packages/intermission-slate-1.0.0.ferrodesign");
const PKG_PLUGIN: &[u8] = include_bytes!("../../engine/testdata/store_catalog/packages/soft-dimmer-1.0.0.zip");
const ASSET_AURORA_PNG: &[u8] = include_bytes!("../../engine/testdata/store_catalog/assets/aurora-preview.png");
const ASSET_ARCADE_PNG: &[u8] = include_bytes!("../../engine/testdata/store_catalog/assets/arcade-preview.png");

static SERVER_STORE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
struct FixtureState {
    catalog: Vec<u8>,
    sig: String,
    files: HashMap<String, Vec<u8>>,
}

impl Default for FixtureState {
    fn default() -> Self {
        let mut files = HashMap::new();
        files.insert("packages/aurora-pulse-1.0.0.ferrodesign".into(), PKG_THEME_V1.to_vec());
        files.insert("packages/arcade-ticker-1.0.0.ferroalert".into(), PKG_ALERT.to_vec());
        files.insert("packages/foundry-goal-1.0.0.ferrodesign".into(), PKG_WIDGET.to_vec());
        files.insert("packages/intermission-slate-1.0.0.ferrodesign".into(), PKG_OVERLAY.to_vec());
        files.insert("packages/soft-dimmer-1.0.0.zip".into(), PKG_PLUGIN.to_vec());
        files.insert("assets/aurora-preview.png".into(), ASSET_AURORA_PNG.to_vec());
        files.insert("assets/arcade-preview.png".into(), ASSET_ARCADE_PNG.to_vec());
        Self { catalog: CATALOG_V1.to_vec(), sig: CATALOG_V1_SIG.into(), files }
    }
}

async fn start() -> (String, String, reqwest::Client, Engine, tempfile::TempDir, Arc<Mutex<FixtureState>>) {
    let fstate = Arc::new(Mutex::new(FixtureState::default()));
    let s1 = fstate.clone();
    let s2 = fstate.clone();
    let s3 = fstate.clone();
    let app = Router::new()
        .route(
            "/catalog.json",
            get(move || async move { Response::new(Body::from(s1.lock().unwrap().catalog.clone())) }),
        )
        .route(
            "/catalog.json.sig",
            get(move || async move { Response::new(Body::from(s2.lock().unwrap().sig.clone())) }),
        )
        .route(
            "/{*path}",
            get(move |axum::extract::Path(path): axum::extract::Path<String>| async move {
                match s3.lock().unwrap().files.get(&path).cloned() {
                    Some(b) => Response::new(Body::from(b)),
                    None => Response::builder().status(HttpStatus::NOT_FOUND).body(Body::empty()).unwrap(),
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: TEST_KEY.into(), local: true });

    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    (server.base_url(), server.sessions.desktop_token().to_string(), reqwest::Client::new(), engine, dir, fstate)
}

#[tokio::test]
async fn store_http_install_undo_uninstall_and_security_errors() {
    let _guard = SERVER_STORE_LOCK.lock().await;
    let (base, token, client, _engine, _dir, fstate) = start().await;
    let url = |p: &str| format!("{base}/api/v1{p}");

    // Catalog returns signed items.
    let res = client.get(url("/store/catalog?fresh=true")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cat: Value = res.json().await.unwrap();
    assert_eq!(cat["items"].as_array().map(Vec::len), Some(5));

    // Verified asset serves cleanly; tampered asset refuses with 502.
    let ares = client.get(url("/store/assets/aurora-pulse/preview.png")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(ares.status(), StatusCode::OK);
    assert_eq!(ares.headers().get("content-type").and_then(|v| v.to_str().ok()), Some("image/png"));

    // Resolve ferrocast:// link -> 200 requires_confirmation without installing.
    let lres = client
        .post(url("/store/resolve-link"))
        .bearer_auth(&token)
        .json(&json!({ "uri": "ferrocast://store/install/aurora-pulse" }))
        .send()
        .await
        .unwrap();
    assert_eq!(lres.status(), StatusCode::OK);
    let link: Value = lres.json().await.unwrap();
    assert_eq!(link["requires_confirmation"], json!(true));
    let inst_empty: Value =
        client.get(url("/store/installed")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(inst_empty["installed"].as_array().map(Vec::len), Some(0));

    // Malicious ferrocast:// URI -> 400.
    let bad_lres = client
        .post(url("/store/resolve-link"))
        .bearer_auth(&token)
        .json(&json!({ "uri": "ferrocast://store/install/../../etc/passwd" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_lres.status(), StatusCode::BAD_REQUEST);

    // Install, Undo, Reinstall, Uninstall via HTTP.
    let ires = client
        .post(url("/store/install"))
        .bearer_auth(&token)
        .json(&json!({ "id": "aurora-pulse" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ires.status(), StatusCode::CREATED);
    let iv: Value = ires.json().await.unwrap();
    let undo_tok = iv["undo"]["token"].as_str().unwrap().to_string();

    let ures =
        client.post(url("/store/undo")).bearer_auth(&token).json(&json!({ "token": undo_tok })).send().await.unwrap();
    assert_eq!(ures.status(), StatusCode::OK);
    let inst_after_undo: Value =
        client.get(url("/store/installed")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(inst_after_undo["installed"].as_array().map(Vec::len), Some(0));

    // Reinstall, tamper catalog signature -> installed stays accessible & uninstall works offline!
    let ires2 = client
        .post(url("/store/install"))
        .bearer_auth(&token)
        .json(&json!({ "id": "foundry-goal-widget" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ires2.status(), StatusCode::CREATED);

    fstate.lock().unwrap().catalog[15] ^= 1;
    let broken_cat = client.get(url("/store/catalog?fresh=true")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(broken_cat.status(), StatusCode::BAD_GATEWAY);
    let still_installed: Value =
        client.get(url("/store/installed")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(still_installed["installed"].as_array().map(Vec::len), Some(1));

    let unres = client
        .post(url("/store/uninstall"))
        .bearer_auth(&token)
        .json(&json!({ "id": "foundry-goal-widget" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unres.status(), StatusCode::OK);
}
