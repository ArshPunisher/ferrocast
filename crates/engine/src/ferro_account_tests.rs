//! The app's account sign-in against a stand-in relay shaped like ACCOUNTS.md §3 and §5:
//! PKCE start, the browser coming back to the loopback, the code traded for a session, the
//! account read with that session, sessions, sign-out, and a revoked session.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::{Engine, PlatformApps};

#[derive(Default)]
struct Relay {
    start: Option<Value>,
    revoked: bool,
    logouts: Vec<Value>,
    account_reads: usize,
}
type S = Arc<Mutex<Relay>>;

fn bearer(h: &HeaderMap) -> String {
    h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim_start_matches("Bearer ").to_string()
}
fn signed_out() -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": "signed_out", "message": "Signed out.", "hint": null}})))
}

async fn relay() -> (String, S) {
    let s: S = Arc::default();
    let app = Router::new()
        .route(
            "/v1/installs",
            post(|| async { (StatusCode::CREATED, Json(json!({"install_id": "in1", "install_secret": "sec"}))) }),
        )
        .route("/v1/pro/status", get(|| async { Json(json!({"pro": false, "status": "none"})) }))
        .route(
            "/v1/account/app/start",
            post(|State(s): State<S>, h: HeaderMap, Json(b): Json<Value>| async move {
                assert_eq!(bearer(&h), "in1.sec");
                s.lock().unwrap().start = Some(b);
                (
                    StatusCode::CREATED,
                    Json(json!({"flow_id": "af_1", "url": "https://remote.example/login?flow=af_1", "expires_at": 1})),
                )
            }),
        )
        .route(
            "/v1/account/app/token",
            post(|State(s): State<S>, Json(b): Json<Value>| async move {
                let start = s.lock().unwrap().start.clone().unwrap();
                // The challenge must be S256 of the verifier.
                use base64::Engine as _;
                use sha2::Digest as _;
                let want = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(sha2::Sha256::digest(b["code_verifier"].as_str().unwrap().as_bytes()));
                if b["code"] == "used" {
                    return (
                        StatusCode::GONE,
                        Json(json!({"error": {"code": "flow_expired", "message": "gone", "hint": null}})),
                    );
                }
                if b["code"] != "c0de" || start["code_challenge"] != want {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": {"code": "invalid_challenge", "message": "no", "hint": null}})),
                    );
                }
                (
                    StatusCode::CREATED,
                    Json(json!({"session_token": "fca_se_1.x", "session": {"id": "se_1"},
                    "account": {"email": "kabir@example.com"}})),
                )
            }),
        )
        .route(
            "/v1/account",
            get(|State(s): State<S>, h: HeaderMap| async move {
                s.lock().unwrap().account_reads += 1;
                if bearer(&h) != "fca_se_1.x" || s.lock().unwrap().revoked {
                    return signed_out();
                }
                (
                    StatusCode::OK,
                    Json(json!({"account": {"id": "ac_1", "email": "kabir@example.com", "name": "Kabir"},
                    "plan": "free", "pro": {"active": false}, "session": {"id": "se_1", "kind": "app"}, "pcs": 1})),
                )
            }),
        )
        .route(
            "/v1/account/sessions",
            get(|| async { Json(json!({"sessions": [{"id": "se_1", "kind": "app", "current": true}]})) }),
        )
        .route("/v1/account/sessions/{id}", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/v1/account/logout",
            post(|State(s): State<S>, Json(b): Json<Value>| async move {
                s.lock().unwrap().logouts.push(b);
                StatusCode::NO_CONTENT
            }),
        )
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, s)
}

async fn engine(base: String) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(base), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    (e, dir)
}

async fn wait_signed_in(e: &Engine) -> Value {
    for _ in 0..100 {
        let v = e.account_status(false).await;
        if v["state"] != "pending" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("sign-in never finished");
}

#[tokio::test]
async fn signs_in_through_the_loopback_and_out_again() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    assert_eq!(e.account_status(false).await["state"], "signed_out");

    let out = e.account_sign_in(Some("Studio PC".into())).await.unwrap();
    assert_eq!(out["url"], "https://remote.example/login?flow=af_1");
    assert_eq!(e.account_status(false).await["state"], "pending");
    let start = s.lock().unwrap().start.clone().unwrap();
    assert_eq!(start["code_challenge_method"], "S256");
    assert_eq!(start["label"], "Studio PC");
    let redirect = start["redirect_uri"].as_str().unwrap().to_string();
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/auth/callback"));

    // A request with the wrong state doesn't end the sign-in; the right one does.
    let http = reqwest::Client::new();
    let bad = http.get(format!("{redirect}?code=c0de&state=nope")).send().await.unwrap();
    assert_eq!(bad.status(), 400);
    let st = start["state"].as_str().unwrap();
    let ok = http.get(format!("{redirect}?code=c0de&state={st}")).send().await.unwrap();
    assert_eq!(ok.status(), 200);

    let v = wait_signed_in(&e).await;
    assert_eq!(v["state"], "signed_in");
    assert_eq!(v["account"]["email"], "kabir@example.com");
    assert!(v.get("csrf_token").is_none());
    assert_eq!(e.account_sessions().await.unwrap()["sessions"][0]["id"], "se_1");
    e.account_revoke("se_2").await.unwrap();
    assert!(e.account_revoke("../x").await.is_err());
    // Signed in already: a second sign-in is refused, not silently switched.
    assert!(e.account_sign_in(None).await.is_err());

    e.account_sign_out(true).await.unwrap();
    assert_eq!(s.lock().unwrap().logouts, vec![json!({"everywhere": true})]);
    assert_eq!(e.account_status(false).await["state"], "signed_out");
}

#[tokio::test]
async fn a_revoked_session_reads_as_signed_out_with_a_reason() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.inner.secrets.set(super::SESSION, "fca_se_1.x").unwrap();
    assert_eq!(e.account_status(false).await["state"], "signed_in");
    s.lock().unwrap().revoked = true;
    let v = e.account_status(true).await;
    assert_eq!(v["state"], "signed_out");
    assert!(v["error"]["message"].as_str().unwrap().contains("signed out"));
    // And the token is gone, so the next read doesn't ask the relay again.
    assert!(e.inner.secrets.get(super::SESSION).unwrap().is_none());
}

#[tokio::test]
async fn cancel_stops_waiting() {
    let (base, _s) = relay().await;
    let (e, _d) = engine(base).await;
    e.account_sign_in(None).await.unwrap();
    assert_eq!(e.account_status(false).await["state"], "pending");
    e.account_sign_in_cancel();
    assert_eq!(e.account_status(false).await["state"], "signed_out");
}

#[test]
fn pkce_challenge_is_s256_of_the_verifier() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let (v, c) = super::pkce();
    assert!((43..=128).contains(&v.len()));
    assert_eq!(c, base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(v.as_bytes())));
}

/// The session token as the fake relay hands it out, built in pieces.
fn token() -> String {
    ["fca", "_se_1", ".x"].concat()
}

fn redirect_and_state(s: &S) -> (String, String) {
    let start = s.lock().unwrap().start.clone().unwrap();
    (start["redirect_uri"].as_str().unwrap().to_string(), start["state"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn the_token_stays_in_the_keychain_only() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.account_sign_in(None).await.unwrap();
    let (redirect, st) = redirect_and_state(&s);
    let page = reqwest::get(format!("{redirect}?code=c0de&state={st}")).await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    assert!(page.text().await.unwrap().contains("signed in"));
    let v = wait_signed_in(&e).await;
    assert_eq!(e.inner.secrets.get(super::SESSION).unwrap().as_deref(), Some(token().as_str()));
    assert!(!v.to_string().contains("fca_"), "never in the answer: {v}");
    let cached: Value =
        e.db(|st| ferrocast_store::repo::settings::get::<Value>(st, super::CACHE_KEY)).await.unwrap().unwrap();
    assert!(!cached.to_string().contains("fca_"), "never in settings");
    // Reused for 15 minutes unless asked to refresh.
    // Let the background reads (the sign-in's own, the Pro refresh's) finish first.
    let mut reads = s.lock().unwrap().account_reads;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let now = s.lock().unwrap().account_reads;
        if now == reads {
            break;
        }
        reads = now;
    }
    e.account_status(false).await;
    assert_eq!(s.lock().unwrap().account_reads, reads);
    e.account_status(true).await;
    assert_eq!(s.lock().unwrap().account_reads, reads + 1);
    assert_eq!(e.account_email().await.as_deref(), Some("kabir@example.com"));
}

#[tokio::test]
async fn a_wrong_code_ends_the_sign_in_with_a_reason() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.account_sign_in(None).await.unwrap();
    let (redirect, st) = redirect_and_state(&s);
    let page = reqwest::get(format!("{redirect}?code=wrong&state={st}")).await.unwrap();
    assert_eq!(page.status(), 400, "the browser hears it didn't work");
    let v = wait_signed_in(&e).await;
    assert_eq!(v["state"], "signed_out");
    assert!(v["error"]["message"].as_str().unwrap().contains("didn't work"), "{v}");
    assert!(e.inner.secrets.get(super::SESSION).unwrap().is_none());
    // A used or expired code reads the same.
    e.account_sign_in(None).await.unwrap();
    let (redirect, st) = redirect_and_state(&s);
    assert_eq!(reqwest::get(format!("{redirect}?code=used&state={st}")).await.unwrap().status(), 400);
    let v = wait_signed_in(&e).await;
    assert!(v["error"]["message"].as_str().unwrap().contains("didn't work"), "{v}");
}

#[tokio::test]
async fn the_wrong_state_or_path_never_ends_the_wait() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let wait = tokio::spawn(super::wait_for_code(l, "the-right-state", std::time::Duration::from_secs(5)));
    let http = reqwest::Client::new();
    assert_eq!(http.get(format!("{base}/favicon.ico")).send().await.unwrap().status(), 404);
    let r = http.get(format!("{base}/auth/callback?code=c&state=the-wrong-state")).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = http.get(format!("{base}/auth/callback?error=access_denied&state=x")).send().await.unwrap();
    assert_eq!(r.status(), 400, "a cancel needs the right state too");
    assert!(!wait.is_finished());
    // The right one: the code comes back, with the browser waiting for the answer.
    let browser = tokio::spawn({
        let http = http.clone();
        async move { http.get(format!("{base}/auth/callback?code=c0de&state=the-right-state")).send().await }
    });
    let (code, mut socket) = wait.await.unwrap().unwrap();
    assert_eq!(code, "c0de");
    super::respond(&mut socket, 200, "ok").await.unwrap();
    assert_eq!(browser.await.unwrap().unwrap().status(), 200);
}

#[tokio::test]
async fn a_cancel_at_google_is_a_cancel() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let wait = tokio::spawn(super::wait_for_code(l, "st4te-st4te", std::time::Duration::from_secs(5)));
    let r = reqwest::get(format!("{base}/auth/callback?error=access_denied&state=st4te-st4te")).await.unwrap();
    assert_eq!(r.status(), 200);
    let err = wait.await.unwrap().unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { code: "cancelled", .. }), "{err:?}");
}

#[tokio::test]
async fn waiting_too_long_expires() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = super::wait_for_code(l, "st4te-st4te", std::time::Duration::from_millis(50)).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 410, code: "flow_expired", .. }), "{err:?}");
}

#[tokio::test]
async fn cancel_closes_the_listener() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.account_sign_in(None).await.unwrap();
    let (redirect, st) = redirect_and_state(&s);
    e.account_sign_in_cancel();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = reqwest::get(format!("{redirect}?code=c0de&state={st}")).await;
    assert!(r.is_err(), "nobody listens after Cancel");
    assert!(e.inner.secrets.get(super::SESSION).unwrap().is_none());
    assert_eq!(e.account_status(false).await["state"], "signed_out");
}

#[tokio::test]
async fn a_second_sign_in_replaces_the_first() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.account_sign_in(None).await.unwrap();
    let (first, st1) = redirect_and_state(&s);
    e.account_sign_in(None).await.unwrap();
    let (second, st2) = redirect_and_state(&s);
    assert_ne!(first, second);
    assert_ne!(st1, st2);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(reqwest::get(format!("{first}?code=c0de&state={st1}")).await.is_err(), "the first stopped");
    assert_eq!(reqwest::get(format!("{second}?code=c0de&state={st2}")).await.unwrap().status(), 200);
    assert_eq!(wait_signed_in(&e).await["state"], "signed_in");
    let err = e.account_sign_in(None).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 409, code: "already_signed_in", .. }), "{err:?}");
}

#[tokio::test]
async fn an_offline_relay_is_a_clear_503_and_memory_is_stale() {
    let (base, _s) = relay().await;
    let (e, _d) = engine(base).await;
    e.inner.secrets.set(super::SESSION, &token()).unwrap();
    assert_eq!(e.account_status(true).await["state"], "signed_in");
    // Point the engine at a port nobody listens on.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(dead_url), ..Default::default() };
    let off =
        Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    let err = off.account_sign_in(None).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 503, code: "relay_unreachable", .. }), "{err:?}");
    // Signed in with an answer in memory: stale, never an error.
    let cached: Value =
        e.db(|st| ferrocast_store::repo::settings::get::<Value>(st, super::CACHE_KEY)).await.unwrap().unwrap();
    off.db(move |st| ferrocast_store::repo::settings::set(st, super::CACHE_KEY, &cached)).await.unwrap();
    off.inner.secrets.set(super::SESSION, &token()).unwrap();
    let v = off.account_status(true).await;
    assert_eq!((v["state"].as_str(), v["stale"].as_bool()), (Some("signed_in"), Some(true)), "{v}");
    assert_eq!(v["account"]["email"], "kabir@example.com");
    let err = off.account_sessions().await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 503, .. }), "{err:?}");
    // Signing out everywhere needs the relay; this PC forgets the session anyway.
    let err = off.account_sign_out(true).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 503, .. }), "{err:?}");
    assert!(off.inner.secrets.get(super::SESSION).unwrap().is_none());
}

#[tokio::test]
async fn the_web_dashboard_opens_the_tunnel_of_a_signed_in_pc() {
    let (base, _s) = relay().await;
    let (e, _d) = engine(base).await;
    // Neither Pro nor an account: the relay's ask is ignored.
    e.handle_relay_item("remote_wanted", &json!({ "reason": "web_dashboard" })).await;
    assert!(!e.remote_wanted().await);
    e.inner.secrets.set(super::SESSION, &token()).unwrap();
    e.handle_relay_item("remote_wanted", &json!({ "reason": "web_dashboard" })).await;
    assert!(e.remote_wanted().await, "a free account may open it");
    e.account_sign_out(false).await.unwrap();
    assert!(!e.remote_wanted().await, "signed out without Pro: closed again");
}
