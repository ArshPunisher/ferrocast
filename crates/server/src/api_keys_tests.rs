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
        Case { method: "GET", path: "/overlays", body: None, expected: [F, F, A, F, F, F, A] },
        Case { method: "GET", path: "/obs/scenes", body: None, expected: [A, F, F, A, F, F, A] },
        Case { method: "GET", path: "/golive", body: None, expected: [A, F, F, F, A, F, A] },
        Case { method: "GET", path: "/settings", body: None, expected: [F, F, F, F, F, A, A] },
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
    // A query key is refused off the live socket, and is never logged either way.
    let q_res = rig.client.get(format!("{}/api/v1/status?key={token}", rig.base)).send().await.unwrap();
    assert_eq!(q_res.status(), StatusCode::UNAUTHORIZED);

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

    // Settings-only key must still be denied on /api/v1/live.
    let mut bad_req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    bad_req.headers_mut().insert("authorization", format!("Bearer {settings_tok}").parse().unwrap());
    assert!(tokio_tungstenite::connect_async(bad_req).await.is_err());

    // Key combining Settings with Chat connects to /live and receives `links` frames.
    let (_, chat_settings_tok) = rig.create_key("Chat + Settings", &[ApiKeyScope::Chat, ApiKeyScope::Settings]).await;
    let mut cs_req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    cs_req.headers_mut().insert("authorization", format!("Bearer {chat_settings_tok}").parse().unwrap());
    let (mut cs_ws, _) = tokio_tungstenite::connect_async(cs_req).await.unwrap();
    assert_eq!(
        rig.call(
            HttpMethod::PUT,
            "/links",
            &chat_settings_tok,
            Some(json!({ "discord": "https://discord.gg/keyframe" }))
        )
        .await,
        StatusCode::OK
    );
    let link_frame = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let Some(Ok(WsMessage::Text(txt))) = cs_ws.next().await else {
                panic!("expected links frame for Chat+Settings key");
            };
            let v: Value = serde_json::from_str(&txt).unwrap();
            if v["type"] == "links" {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(link_frame["data"]["resolved"]["discord"], "discord.gg/keyframe");

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

/// Custom alert HTML and fields are code-like content: PC only, for an alerts key as for a phone.
#[tokio::test]
async fn an_alerts_key_cannot_add_or_change_custom_alert_code() {
    let rig = Rig::start().await;
    let (_, tok) = rig.create_key("alerts", &[ApiKeyScope::Alerts]).await;
    let rules: Value =
        rig.client.get(rig.url("/alert-rules")).bearer_auth(&tok).send().await.unwrap().json().await.unwrap();
    let mut rule = rules[0].clone();
    let id = rule["id"].as_str().unwrap().to_string();
    // Tuning a rule is fine.
    rule["variations"][0]["volume"] = json!(55);
    assert_eq!(
        rig.call(reqwest::Method::PUT, &format!("/alert-rules/{id}"), &tok, Some(rule.clone())).await,
        StatusCode::OK
    );
    for (k, v) in [("custom_enabled", json!(true)), ("html", json!("<p>x</p>"))] {
        let mut bad = rule.clone();
        bad["variations"][0][k] = v;
        assert_eq!(
            rig.call(reqwest::Method::PUT, &format!("/alert-rules/{id}"), &tok, Some(bad)).await,
            StatusCode::FORBIDDEN,
            "{k}"
        );
    }
    let mut create = rule.clone();
    create["id"] = json!("");
    create["variations"][0]["html"] = json!("<p>x</p>");
    assert_eq!(rig.call(reqwest::Method::POST, "/alert-rules", &tok, Some(create)).await, StatusCode::FORBIDDEN);
}

/// Revoking a key closes the sockets it has open at once, and an expired one is closed when it
/// expires: a key that is off must not keep receiving the stream.
#[tokio::test]
async fn revoking_a_key_closes_its_open_live_sockets_at_once() {
    let rig = Rig::start().await;
    let ws_base = rig.base.replacen("http://", "ws://", 1);
    let (id, tok) = rig.create_key("Chat", &[ApiKeyScope::Chat]).await;
    let mut req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {tok}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // hello and status first.
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    }
    let res =
        rig.client.delete(rig.url(&format!("/api-keys/{id}"))).bearer_auth(&rig.session_token).send().await.unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    // The socket closes (a close frame or the end of the stream) within a moment, not never.
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the revoked key's socket is still open");
    rig.engine.shutdown();
}

#[tokio::test]
async fn a_key_that_expires_has_its_open_live_socket_closed() {
    let rig = Rig::start().await;
    let ws_base = rig.base.replacen("http://", "ws://", 1);
    let soon = ferrocast_engine::now().0 + 1500;
    let res = rig
        .client
        .post(rig.url("/api-keys"))
        .bearer_auth(&rig.session_token)
        .json(&json!({ "name": "Soon", "scopes": [ApiKeyScope::Chat], "expires_at": soon }))
        .send()
        .await
        .unwrap();
    let body: Value = res.json().await.unwrap();
    let tok = body["token"].as_str().unwrap().to_string();
    let mut req = format!("{ws_base}/api/v1/live").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {tok}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the expired key's socket is still open");
    rig.engine.shutdown();
}

pub(super) mod exposure_helpers {
    //! Keys are loopback-only unless the owner switched on "API keys over LAN"; key management is
    //! the PC's own; a web page can't use a key from the browser.
    use super::*;
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    pub struct Lan {
        pub app: Router,
        pub sessions: Arc<Sessions>,
        pub engine: Engine,
        pub token: String,
        pub _dir: tempfile::TempDir,
    }

    pub async fn lan() -> Lan {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let created = engine
            .create_api_key(ApiKeyCreate { name: "Lan".into(), scopes: vec![ApiKeyScope::ReadOnly], expires_at: None })
            .await
            .unwrap();
        let sessions = Arc::new(Sessions::new());
        let app = router(AppState {
            engine: engine.clone(),
            sessions: sessions.clone(),
            guard: auth::Guard { port: 47820, lan: true, dev_origins: vec![] },
            api_limiter: rate_limit::ApiKeyRateLimiter::default(),
        });
        Lan { app, sessions, engine, token: created.token, _dir: dir }
    }

    pub fn req(
        method: &str,
        uri: &str,
        peer: &str,
        bearer: &str,
        origin: Option<&str>,
        body: Option<Value>,
    ) -> HttpRequest<AxumBody> {
        let mut b = HttpRequest::builder().method(method).uri(uri).header("host", "192.168.1.9:47820");
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        let mut r = match body {
            Some(v) => b.header("content-type", "application/json").body(AxumBody::from(v.to_string())).unwrap(),
            None => b.body(AxumBody::empty()).unwrap(),
        };
        let addr: SocketAddr = peer.parse().unwrap();
        r.extensions_mut().insert(ConnectInfo(addr));
        r
    }

    #[tokio::test]
    async fn a_key_works_from_this_pc_and_not_from_the_network_unless_switched_on() {
        let l = lan().await;
        let ok = |s: StatusCode| s == StatusCode::OK;
        let r =
            l.app.clone().oneshot(req("GET", "/api/v1/status", "127.0.0.1:50000", &l.token, None, None)).await.unwrap();
        assert!(ok(r.status()), "loopback: {}", r.status());
        let r = l
            .app
            .clone()
            .oneshot(req("GET", "/api/v1/status", "192.168.1.20:50000", &l.token, None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "LAN peer, switch off");
        // The owner switches it on (a PUT /settings with the PC's own token).
        let on = req(
            "PUT",
            "/api/v1/settings",
            "127.0.0.1:50000",
            l.sessions.desktop_token(),
            None,
            Some(json!({ "api_keys_over_lan": true })),
        );
        let r = l.app.clone().oneshot(on).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "switching it on");
        let r = l
            .app
            .clone()
            .oneshot(req("GET", "/api/v1/status", "192.168.1.20:50000", &l.token, None, None))
            .await
            .unwrap();
        assert!(ok(r.status()), "LAN peer, switch on: {}", r.status());
        l.engine.shutdown();
    }

    #[tokio::test]
    async fn key_management_is_this_pcs_own() {
        let l = lan().await;
        let desk = l.sessions.desktop_token().to_string();
        let r =
            l.app.clone().oneshot(req("GET", "/api/v1/api-keys", "127.0.0.1:50000", &desk, None, None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        // A paired phone on the network holds a session token too: not for keys.
        let r = l
            .app
            .clone()
            .oneshot(req("GET", "/api/v1/api-keys", "192.168.1.20:50000", &desk, None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = l
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/api-keys",
                "192.168.1.20:50000",
                &desk,
                None,
                Some(json!({ "name": "x", "scopes": ["read_only"], "expires_at": null })),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        l.engine.shutdown();
    }

    #[tokio::test]
    async fn a_web_page_cannot_use_a_key_from_the_browser() {
        let l = lan().await;
        for origin in ["http://evil.example", "http://192.168.1.50:8080", "null"] {
            let r = l
                .app
                .clone()
                .oneshot(req("GET", "/api/v1/status", "127.0.0.1:50000", &l.token, Some(origin), None))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{origin}");
        }
        l.engine.shutdown();
    }

    #[tokio::test]
    async fn a_key_cannot_switch_on_keys_over_lan() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
        let created = engine
            .create_api_key(ApiKeyCreate { name: "S".into(), scopes: vec![ApiKeyScope::Settings], expires_at: None })
            .await
            .unwrap();
        let app = router(AppState {
            engine: engine.clone(),
            sessions: Arc::new(Sessions::new()),
            guard: auth::Guard { port: 47820, lan: false, dev_origins: vec![] },
            api_limiter: rate_limit::ApiKeyRateLimiter::default(),
        });
        let r = app
            .oneshot(req(
                "PUT",
                "/api/v1/settings",
                "127.0.0.1:50000",
                &created.token,
                None,
                Some(json!({ "api_keys_over_lan": true })),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        engine.shutdown();
    }
}

mod hardening {
    use super::exposure_helpers::*;
    use super::*;

    #[tokio::test]
    async fn a_key_in_the_query_is_accepted_only_on_the_live_socket() {
        let l = lan().await;
        let uri = format!("/api/v1/status?key={}", l.token);
        let r = l.app.clone().oneshot(req("GET", &uri, "127.0.0.1:50000", "", None, None)).await.unwrap();
        assert_ne!(r.status(), StatusCode::OK, "a ?key= on a plain route");
        l.engine.shutdown();
    }

    #[tokio::test]
    async fn a_guesser_is_cut_off_per_address_before_every_try_costs_a_lookup() {
        let l = lan().await;
        let bad = format!("{KEY_PREFIX}{}", "0".repeat(64));
        let mut last = StatusCode::OK;
        for _ in 0..(rate_limit::BAD_KEY_LIMIT + 5) {
            let r =
                l.app.clone().oneshot(req("GET", "/api/v1/status", "127.0.0.2:50000", &bad, None, None)).await.unwrap();
            last = r.status();
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
        // Another address is not affected.
        let r =
            l.app.clone().oneshot(req("GET", "/api/v1/status", "127.0.0.1:50000", &l.token, None, None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        l.engine.shutdown();
    }

    #[test]
    fn a_flood_of_429s_writes_one_audit_row_per_key_per_window() {
        let id = format!("flood-{}", ferrocast_engine::now().0);
        assert!(rate_limit::audit_429_now(&id));
        for _ in 0..50 {
            assert!(!rate_limit::audit_429_now(&id));
        }
    }

    #[test]
    fn the_audit_path_is_a_route_template_never_what_was_typed() {
        use crate::routes::remote_scope::audit_path;
        assert_eq!(audit_path("/viewers/twitch/someviewer/note"), "/viewers/{platform}/{user_id}/note");
        assert_eq!(audit_path("/chat"), "/chat");
        // Not a route: nothing of it is kept (a probe can't write words into the log).
        assert_eq!(audit_path("/secret-word/another"), "/(unknown)");
        assert_eq!(audit_path("/chat/xyz-not-a-route"), "/(unknown)");
    }

    #[test]
    fn an_unknown_live_frame_is_not_delivered_to_any_key() {
        use crate::routes::remote_scope::api_key_live_frame_allowed;
        let all = ApiKeyScope::ALL;
        assert!(!api_key_live_frame_allowed(&all, r#"{"v":1,"type":"snapshot","data":{}}"#));
        assert!(!api_key_live_frame_allowed(&all, r#"{"v":1,"type":"a_frame_nobody_listed","data":{}}"#));
        assert!(api_key_live_frame_allowed(&all, r#"{"v":1,"type":"hello","data":{}}"#));
    }
}
