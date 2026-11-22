//! Integration and matrix tests for local API keys, scopes, expiry, hashing, logging, and rate limits.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body as AxumBody;
use axum::http::{Method as HttpMethod, Request as HttpRequest};
use ferrocast_core::api_keys::{ApiKeyCreate, ApiKeyScope, KEY_PREFIX};
use ferrocast_engine::Engine;
use ferrocast_engine::api_keys::sha256_hex;
use ferrocast_store::repo;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

use super::*;

struct Rig {
    base: String,
    session_token: String,
    client: reqwest::Client,
    engine: Engine,
    dir: tempfile::TempDir,
}

impl Rig {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        Self {
            base: server.base_url(),
            session_token: server.sessions.desktop_token().to_string(),
            client: reqwest::Client::new(),
            engine,
            dir,
        }
    }

    fn url(&self, rel: &str) -> String {
        format!("{}/api/v1{rel}", self.base)
    }

    async fn create_key(&self, name: &str, scopes: &[ApiKeyScope]) -> (String, String) {
        let res = self
            .client
            .post(self.url("/api-keys"))
            .bearer_auth(&self.session_token)
            .json(&json!({ "name": name, "scopes": scopes, "expires_at": null }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: Value = res.json().await.unwrap();
        let id = body["key"]["id"].as_str().unwrap().to_string();
        let token = body["token"].as_str().unwrap().to_string();
        (id, token)
    }

    async fn call(&self, method: reqwest::Method, rel: &str, token: &str, body: Option<Value>) -> StatusCode {
        let mut req = self.client.request(method, self.url(rel)).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        req.send().await.unwrap().status()
    }
}

/// Captures every tracing event's formatted fields to guarantee plain API keys and hashes never log.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<String>>>);

struct LogVisitor(String);

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(&mut self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::TRACE)
    }
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut v = LogVisitor(format!("span:{}", span.metadata().name()));
        span.record(&mut v);
        self.0.lock().unwrap().push(v.0);
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut v = LogVisitor("record".into());
        values.record(&mut v);
        self.0.lock().unwrap().push(v.0);
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut v = LogVisitor(event.metadata().target().to_string());
        event.record(&mut v);
        self.0.lock().unwrap().push(v.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn scope_enforcement_matrix_across_all_route_groups() {
    let rig = Rig::start().await;

    // One key per individual scope + one key holding ALL 6 scopes.
    let mut keys: Vec<(Option<ApiKeyScope>, &str, String)> = Vec::new();
    for scope in ApiKeyScope::ALL {
        let (_, tok) = rig.create_key(&format!("key_{}", scope.as_str()), &[scope]).await;
        keys.push((Some(scope), scope.as_str(), tok));
    }
    let (_, all_tok) = rig.create_key("key_all", &ApiKeyScope::ALL).await;
    keys.push((None, "all", all_tok));

    #[derive(Clone, Copy)]
    enum Access {
        Forbidden,
        Allowed,
    }

    // Representative routes across every route group + NEVER-allowed secrets/accounts/diagnostics/plugins/keys.
    struct Case {
        method: &'static str,
        path: &'static str,
        body: Option<Value>,
        expected: [Access; 7], // [read_only, chat, alerts, studio_control, go_live, settings, all]
    }
    use Access::{Allowed as A, Forbidden as F};

    let matrix = [
        // Core read: open to every key
        Case { method: "GET", path: "/status", body: None, expected: [A, A, A, A, A, A, A] },
        Case { method: "GET", path: "/openapi.json", body: None, expected: [A, A, A, A, A, A, A] },
        Case { method: "GET", path: "/asyncapi.json", body: None, expected: [A, A, A, A, A, A, A] },
        // Read-only group (reads across public resources)
        Case { method: "GET", path: "/themes", body: None, expected: [A, F, F, F, F, A, A] },
        Case { method: "GET", path: "/events", body: None, expected: [A, F, A, F, F, F, A] },
        Case { method: "GET", path: "/chat", body: None, expected: [A, A, F, F, F, F, A] },
        Case { method: "GET", path: "/overlays", body: None, expected: [A, F, A, F, F, F, A] },
        Case { method: "GET", path: "/obs/scenes", body: None, expected: [A, F, F, A, F, F, A] },
        Case { method: "GET", path: "/golive", body: None, expected: [A, F, F, F, A, F, A] },
        Case { method: "GET", path: "/settings", body: None, expected: [A, F, F, F, F, A, A] },
        // Chat writes
        Case {
            method: "POST",
            path: "/chat/send",
            body: Some(json!({ "platforms": "twitch", "text": "hello" })),
            expected: [F, A, F, F, F, F, A],
        },
        Case {
            method: "POST",
            path: "/mod/panic",
            body: Some(json!({ "on": false })),
            expected: [F, A, F, F, F, F, A],
        },
        Case {
            method: "POST",
            path: "/counters",
            body: Some(json!({ "name": "deaths", "value": 0 })),
            expected: [F, A, F, F, F, F, A],
        },
        // Alerts writes
        Case {
            method: "POST",
            path: "/test",
            body: Some(json!({ "kind": "follow", "platform": "twitch" })),
            expected: [F, F, A, F, F, F, A],
        },
        Case { method: "POST", path: "/queue/pause", body: None, expected: [F, F, A, F, F, F, A] },
        // StudioControl writes
        Case {
            method: "POST",
            path: "/obs/scene",
            body: Some(json!({ "scene": "Main" })),
            expected: [F, F, F, A, F, F, A],
        },
        Case {
            method: "POST",
            path: "/moments/markers",
            body: Some(json!({ "label": "clip" })),
            expected: [F, F, F, A, F, F, A],
        },
        // GoLive writes
        Case {
            method: "PUT",
            path: "/golive/protection",
            body: Some(json!({ "enabled": false })),
            expected: [F, F, F, F, A, F, A],
        },
        Case { method: "POST", path: "/show/start", body: Some(json!({})), expected: [F, F, F, F, A, F, A] },
        // Settings writes (safe settings patch allowed; changing lan_access ALWAYS blocked)
        Case {
            method: "PUT",
            path: "/settings",
            body: Some(json!({ "sound": true })),
            expected: [F, F, F, F, F, A, A],
        },
        Case {
            method: "PUT",
            path: "/settings",
            body: Some(json!({ "lan_access": true })),
            expected: [F, F, F, F, F, F, F],
        },
        // NEVER allowed for ANY scope, even with ALL 6 scopes granted!
        Case { method: "GET", path: "/api-keys", body: None, expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/api-keys",
            body: Some(json!({ "name": "x", "scopes": ["read_only"] })),
            expected: [F, F, F, F, F, F, F],
        },
        Case { method: "DELETE", path: "/api-keys/some_id", body: None, expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/session/pair",
            body: Some(json!({ "code": "123456" })),
            expected: [F, F, F, F, F, F, F],
        },
        Case { method: "POST", path: "/session/pair-code", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/accounts", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/accounts/twitch/connect", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/pro", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/pro/account", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/pro/remote", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/tips", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "PUT", path: "/tips", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "DELETE", path: "/tips", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/discord", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "PUT", path: "/discord", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "PUT", path: "/obs", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/obs/detect", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/obs/detect", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/golive/ingest", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/golive/destinations", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "DELETE", path: "/golive/destinations/d1", body: None, expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/golive/test",
            body: Some(json!({ "url": "rtmp://x" })),
            expected: [F, F, F, F, F, F, F],
        },
        Case { method: "GET", path: "/golive/networks", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/golive/new-key", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/studio/remote-control", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/studio/guests", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/diagnostics/preview", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/diagnostics/report", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/diagnostics/report.zip",
            body: Some(json!({})),
            expected: [F, F, F, F, F, F, F],
        },
        Case { method: "GET", path: "/plugins", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/plugins/catalog", body: None, expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/plugins/catalog/install",
            body: Some(json!({})),
            expected: [F, F, F, F, F, F, F],
        },
        Case { method: "POST", path: "/plugins/p1/act", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/export", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/import/apply", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/designs/import", body: Some(json!({})), expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/designs/d1/export", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/backups/b1/restore", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/updates/install", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "GET", path: "/studio/engine/log", body: None, expected: [F, F, F, F, F, F, F] },
        Case { method: "POST", path: "/studio/engine/restart", body: None, expected: [F, F, F, F, F, F, F] },
        Case {
            method: "POST",
            path: "/studio/engine/pick-file",
            body: Some(json!({})),
            expected: [F, F, F, F, F, F, F],
        },
    ];

    for case in &matrix {
        let m = reqwest::Method::from_bytes(case.method.as_bytes()).unwrap();
        for (idx, (_, label, token)) in keys.iter().enumerate() {
            let status = rig.call(m.clone(), case.path, token, case.body.clone()).await;
            match case.expected[idx] {
                Access::Forbidden => assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "scope={label} MUST be 403 for {} {} (got {status})",
                    case.method,
                    case.path
                ),
                Access::Allowed => assert_ne!(
                    status,
                    StatusCode::FORBIDDEN,
                    "scope={label} MUST NOT be 403 for {} {} (got {status})",
                    case.method,
                    case.path
                ),
            }
        }
    }

    rig.engine.shutdown();
}

#[tokio::test]
async fn revoked_expired_and_bad_keys_return_401() {
    let rig = Rig::start().await;
    let (id, token) = rig.create_key("Temp Deck", &[ApiKeyScope::ReadOnly]).await;

    assert_eq!(rig.call(reqwest::Method::GET, "/status", &token, None).await, StatusCode::OK);

    // Revoke cleanly.
    let del =
        rig.client.delete(rig.url(&format!("/api-keys/{id}"))).bearer_auth(&rig.session_token).send().await.unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    assert_eq!(rig.call(reqwest::Method::GET, "/status", &token, None).await, StatusCode::UNAUTHORIZED);

    // Expired key inserted in store.
    let expired_tok = format!("{KEY_PREFIX}{}", "c".repeat(64));
    repo::api_keys::insert(
        &rig.engine.store(),
        "exp_1",
        "Expired Key",
        "fc_key_cccccccc...",
        &sha256_hex(&expired_tok),
        &[ApiKeyScope::ReadOnly],
        Some(1),
    )
    .unwrap();
    assert_eq!(rig.call(reqwest::Method::GET, "/status", &expired_tok, None).await, StatusCode::UNAUTHORIZED);

    // Malformed & nonexistent keys return 401.
    assert_eq!(rig.call(reqwest::Method::GET, "/status", "fc_key_bad", None).await, StatusCode::UNAUTHORIZED);
    let ghost = format!("{KEY_PREFIX}{}", "0".repeat(64));
    assert_eq!(rig.call(reqwest::Method::GET, "/status", &ghost, None).await, StatusCode::UNAUTHORIZED);

    rig.engine.shutdown();
}

#[tokio::test]
async fn plain_key_is_never_stored_returned_or_logged() {
    use tracing::instrument::WithSubscriber as _;

    let rig = Rig::start().await;
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());

    let (id, token) = rig.create_key("Audit Check", &[ApiKeyScope::ReadOnly]).await;
    assert_eq!(rig.call(reqwest::Method::GET, "/status", &token, None).await, StatusCode::OK);
    // Forbidden route also logs cleanly without token.
    assert_eq!(rig.call(reqwest::Method::GET, "/accounts", &token, None).await, StatusCode::FORBIDDEN);
    // Query-param auth also must never log the query key.
    let q_res = rig.client.get(format!("{}/api/v1/status?key={token}", rig.base)).send().await.unwrap();
    assert_eq!(q_res.status(), StatusCode::OK);

    // Also run directly via `oneshot` under explicit `WithSubscriber` so parallel test threads cannot race TLS.
    let direct_app = router(AppState {
        engine: rig.engine.clone(),
        sessions: Arc::new(Sessions::new()),
        guard: auth::Guard { port: 47820, lan: false, dev_origins: vec![] },
        api_limiter: rate_limit::ApiKeyRateLimiter::default(),
    });
    let direct_req = HttpRequest::builder()
        .method(HttpMethod::GET)
        .uri("/api/v1/status?key=fc_key_unused")
        .header("host", "127.0.0.1:47820")
        .header("authorization", format!("Bearer {token}"))
        .body(AxumBody::empty())
        .unwrap();
    let direct_res = direct_app.oneshot(direct_req).with_subscriber(capture.clone()).await.unwrap();
    assert_eq!(direct_res.status(), StatusCode::OK);

    // Inspect GET /api-keys response JSON.
    let list_res = rig.client.get(rig.url("/api-keys")).bearer_auth(&rig.session_token).send().await.unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let view: Value = list_res.json().await.unwrap();
    let view_text = serde_json::to_string(&view).unwrap();
    assert!(!view_text.contains(&token), "GET /api-keys leaked plain token!");
    assert!(!view_text.contains(&sha256_hex(&token)), "GET /api-keys leaked key hash!");
    assert_eq!(view["keys"][0]["id"], id);
    let audit = view["audit"].as_array().unwrap();
    assert!(audit.len() >= 3);
    assert!(audit.iter().any(|e| e["method"] == "GET" && e["path"] == "/status" && e["status"] == 200));
    assert!(audit.iter().any(|e| e["method"] == "GET" && e["path"] == "/accounts" && e["status"] == 403));

    // Inspect tracing events captured across requests.
    let logs = capture.0.lock().unwrap().join("\n");
    assert!(logs.contains("api_key_use"), "expected api_key_use log entry");
    assert!(!logs.contains(&token), "tracing logs leaked plain API key!");
    assert!(!logs.contains(&sha256_hex(&token)), "tracing logs leaked SHA-256 hash!");
    assert!(!logs.contains("?key="), "tracing logs leaked query key parameter!");

    // Inspect raw SQLite database bytes on disk.
    let db_bytes = std::fs::read(rig.dir.path().join("ferrocast.db")).unwrap();
    assert!(
        !db_bytes.windows(token.len()).any(|w| w == token.as_bytes()),
        "SQLite file on disk contained plain API key!"
    );

    rig.engine.shutdown();
}

#[tokio::test]
async fn rate_limiting_returns_429_and_retry_after_and_audits() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
    let created = engine
        .create_api_key(ApiKeyCreate {
            name: "Burst Test".into(),
            scopes: vec![ApiKeyScope::ReadOnly],
            expires_at: None,
        })
        .await
        .unwrap();

    let state = AppState {
        engine: engine.clone(),
        sessions: Arc::new(Sessions::new()),
        guard: auth::Guard { port: 47820, lan: false, dev_origins: vec![] },
        api_limiter: rate_limit::ApiKeyRateLimiter::new(2, Duration::from_secs(60)),
    };
    let app = router(state);

    let send = |app: Router, tok: &str| {
        let req = HttpRequest::builder()
            .method(HttpMethod::GET)
            .uri("/api/v1/status")
            .header("host", "127.0.0.1:47820")
            .header("authorization", format!("Bearer {tok}"))
            .body(AxumBody::empty())
            .unwrap();
        app.oneshot(req)
    };

    let r1 = send(app.clone(), &created.token).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r1.headers().get("x-ratelimit-limit").unwrap(), "2");
    assert_eq!(r1.headers().get("x-ratelimit-remaining").unwrap(), "1");

    let r2 = send(app.clone(), &created.token).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(r2.headers().get("x-ratelimit-remaining").unwrap(), "0");

    let r3 = send(app, &created.token).await.unwrap();
    assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(r3.headers().get("retry-after").is_some());
    assert_eq!(r3.headers().get("x-ratelimit-remaining").unwrap(), "0");

    let audit = engine.list_api_key_audit(10).await.unwrap();
    assert_eq!(audit[0].status, 429);

    engine.shutdown();
}

#[tokio::test]
async fn live_websocket_filters_frames_by_api_key_scope() {
    let rig = Rig::start().await;
    let ws_base = rig.base.replacen("http://", "ws://", 1);

    let (_, settings_tok) = rig.create_key("Settings Only", &[ApiKeyScope::Settings]).await;
    let (_, chat_tok) = rig.create_key("Chat Only", &[ApiKeyScope::Chat]).await;

    // Settings-only key must be denied on /api/v1/live.
    let mut bad_req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    bad_req.headers_mut().insert("authorization", format!("Bearer {settings_tok}").parse().unwrap());
    assert!(tokio_tungstenite::connect_async(bad_req).await.is_err());

    // Chat key connects with Bearer header; receives hello + status, skips queue.
    let mut ok_req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    ok_req.headers_mut().insert("authorization", format!("Bearer {chat_tok}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(ok_req).await.unwrap();

    let mut seen_types = Vec::new();
    for _ in 0..2 {
        let Some(Ok(WsMessage::Text(txt))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap()
        else {
            panic!("expected hello/status text frame");
        };
        let v: Value = serde_json::from_str(&txt).unwrap();
        let ty = v["type"].as_str().unwrap().to_string();
        assert_ne!(ty, "queue", "chat key must not receive queue frame");
        seen_types.push(ty);
    }
    assert_eq!(seen_types, vec!["hello", "status"]);

    rig.engine.shutdown();
}
