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
        let v = e.account_status().await;
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
    assert_eq!(e.account_status().await["state"], "signed_out");

    let out = e.account_sign_in(Some("Studio PC".into())).await.unwrap();
    assert_eq!(out["url"], "https://remote.example/login?flow=af_1");
    assert_eq!(e.account_status().await["state"], "pending");
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
    assert_eq!(e.account_status().await["state"], "signed_out");
}

#[tokio::test]
async fn a_revoked_session_reads_as_signed_out_with_a_reason() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.inner.secrets.set(super::SESSION, "fca_se_1.x").unwrap();
    assert_eq!(e.account_status().await["state"], "signed_in");
    s.lock().unwrap().revoked = true;
    let v = e.account_status().await;
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
    assert_eq!(e.account_status().await["state"], "pending");
    e.account_sign_in_cancel();
    assert_eq!(e.account_status().await["state"], "signed_out");
}

#[test]
fn pkce_challenge_is_s256_of_the_verifier() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let (v, c) = super::pkce();
    assert!((43..=128).contains(&v.len()));
    assert_eq!(c, base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(v.as_bytes())));
}
