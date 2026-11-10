//! `/api/v1/automations` over a real socket: save and read back, the Test button, and every
//! refusal with its status and our error shape.

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

async fn error(res: reqwest::Response) -> (StatusCode, String, String) {
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    assert!(v["error"]["message"].is_string(), "{status}: not our error shape: {v}");
    (status, v["error"]["code"].as_str().unwrap_or("").into(), v["error"]["message"].as_str().unwrap_or("").into())
}

#[tokio::test]
async fn rules_are_saved_read_back_and_tested() {
    let s = start().await;
    let get = || s.client.get(s.url("/automations")).bearer_auth(&s.token).send();
    assert_eq!(get().await.unwrap().json::<Value>().await.unwrap(), json!([]));
    let rule = ferrocast_engine::automation::example();
    let res = s.client.put(s.url("/automations")).bearer_auth(&s.token).json(&json!([rule])).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let back = get().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(back[0]["id"], "clip");
    assert_eq!(back[0]["when"]["type"], "chat");
    assert_eq!(back[0]["then"][1]["text"], "Saved, {name}!");
    // Its Test button: accepted (its steps run in the background; here there's no Studio).
    let res = s.client.post(s.url("/automations/clip/run")).bearer_auth(&s.token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let res = s.client.post(s.url("/automations/nope/run")).bearer_auth(&s.token).send().await.unwrap();
    assert_eq!(error(res).await.0, StatusCode::NOT_FOUND);
    // No token: refused.
    let res = s.client.get(s.url("/automations")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bad_lists_are_refused_and_change_nothing() {
    let s = start().await;
    let put = |body: Value| s.client.put(s.url("/automations")).bearer_auth(&s.token).json(&body).send();
    put(json!([ferrocast_engine::automation::example()])).await.unwrap();
    let mut no_steps = ferrocast_engine::automation::example();
    no_steps["then"] = json!([]);
    let (status, code, message) = error(put(json!([no_steps])).await.unwrap()).await;
    assert_eq!((status, code.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "automation_invalid"));
    assert!(message.contains("nothing to do"), "{message}");
    // Not a rule at all: a plain 400 or 422 with our shape, never a 500.
    for body in [json!({ "id": "x" }), json!([{ "id": "x", "name": "X", "when": { "type": "teleport" }, "then": [] }])]
    {
        let (status, ..) = error(put(body).await.unwrap()).await;
        assert!(status.is_client_error(), "{status}");
    }
    let back =
        s.client.get(s.url("/automations")).bearer_auth(&s.token).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(back.as_array().map(Vec::len), Some(1), "the saved list stays");
}
