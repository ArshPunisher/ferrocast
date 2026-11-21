//! `/api/v1/studio/remote-control` over a real socket: off by default, the password shown once,
//! a new one on request, and the refusals in our error shape.

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::*;

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    _engine: Engine,
    _dir: tempfile::TempDir,
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base)
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        _engine: engine,
        _dir: dir,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn it_is_off_by_default_and_shows_the_password_once() {
    let s = start().await;
    let get = || s.client.get(s.url("/studio/remote-control")).bearer_auth(&s.token).send();
    let v: Value = get().await.unwrap().json().await.unwrap();
    assert_eq!(
        (v["enabled"].clone(), v["lan"].clone(), v["has_password"].clone()),
        (json!(false), json!(false), json!(false))
    );
    assert_eq!(v["port"], 4455);
    assert!(v.get("password").is_none());

    let port = free_port();
    let res = s
        .client
        .post(s.url("/studio/remote-control"))
        .bearer_auth(&s.token)
        .json(&json!({ "enabled": true, "port": port }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let on: Value = res.json().await.unwrap();
    assert_eq!(
        (on["enabled"].clone(), on["listening"].clone(), on["port"].clone()),
        (json!(true), json!(true), json!(port))
    );
    let pw = on["password"].as_str().unwrap().to_string();
    assert_eq!(pw.len(), 24);

    // Read back: it's on, and the password isn't there to read.
    let back: Value = get().await.unwrap().json().await.unwrap();
    assert_eq!((back["enabled"].clone(), back["has_password"].clone()), (json!(true), json!(true)));
    assert!(back.get("password").is_none());

    // A new one.
    let res = s.client.post(s.url("/studio/remote-control/password")).bearer_auth(&s.token).send().await.unwrap();
    let fresh: Value = res.json().await.unwrap();
    assert_ne!(fresh["password"].as_str().unwrap(), pw);

    // A port under 1024 is refused in our error shape; the dashboard's session is needed.
    let res = s
        .client
        .post(s.url("/studio/remote-control"))
        .bearer_auth(&s.token)
        .json(&json!({ "port": 80 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let e: Value = res.json().await.unwrap();
    assert!(e["error"]["message"].is_string());
    let res = s.client.get(s.url("/studio/remote-control")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
