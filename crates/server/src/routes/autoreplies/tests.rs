//! Auto-reply routes over a real socket: CRUD, order, Try it, and every error status
//! (400, 401, 404, 409, 413, 415, 422).

use std::sync::Arc;

use ferrocast_engine::Engine;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{ServeOptions, serve};

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    _engine: Engine,
    _dir: tempfile::TempDir,
}

impl Running {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> reqwest::Response {
        self.client
            .request(method, format!("{}/api/v1{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.send(reqwest::Method::POST, path, body).await
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.send(reqwest::Method::PUT, path, body).await
    }
    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client.delete(format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token).send().await.unwrap()
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        _engine: engine,
        _dir: dir,
    }
}

async fn body(res: reqwest::Response) -> (StatusCode, Value) {
    let s = res.status();
    (s, res.json().await.unwrap_or(Value::Null))
}

fn discord() -> Value {
    json!({ "name": "Discord", "match": { "kind": "word", "text": "discord" }, "reply": "Join: discord.gg/x" })
}

#[tokio::test]
async fn needs_the_session() {
    let r = start().await;
    for path in ["/autoreplies", "/autoreplies/starters"] {
        let res = r.client.get(format!("{}/api/v1{path}", r.base)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn create_list_update_reorder_delete() {
    let r = start().await;
    let (s, a) = body(r.post("/autoreplies", discord()).await).await;
    assert_eq!(s, StatusCode::CREATED);
    // Defaults filled in.
    assert_eq!(a["enabled"], true);
    assert_eq!(a["cooldown_s"], 30);
    assert_eq!(a["user_cooldown_s"], 120);
    assert_eq!(a["min_role"], "everyone");
    assert_eq!(a["only_when_live"], true);
    assert_eq!(a["match"]["case_sensitive"], false);
    assert_eq!(a["platforms"], json!([]));
    let a_id = a["id"].as_str().unwrap().to_string();
    assert!(!a_id.is_empty());

    let (s, b) = body(
        r.post(
            "/autoreplies",
            json!({ "name": "Game", "match": { "kind": "contains", "text": "what game" }, "reply": "{game}",
                    "platforms": ["kick"], "min_role": "subscriber", "only_when_live": false }),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(b["min_role"], "subs", "aliases are stored as the command roles");
    let b_id = b["id"].as_str().unwrap().to_string();

    let (s, list) = body(r.get("/autoreplies").await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["id"], a_id.as_str());

    let mut changed = discord();
    changed["enabled"] = json!(false);
    let (s, v) = body(r.put(&format!("/autoreplies/{a_id}"), changed).await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["id"], a_id.as_str());

    let (s, v) = body(r.put("/autoreplies/order", json!({ "ids": [b_id, a_id] })).await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["id"], b_id.as_str());
    let (s, v) = body(r.put("/autoreplies/order", json!({ "ids": [b_id] })).await).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("exactly once"));

    assert_eq!(r.delete(&format!("/autoreplies/{a_id}")).await.status(), StatusCode::NO_CONTENT);
    let (s, v) = body(r.delete(&format!("/autoreplies/{a_id}")).await).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "autoreply_not_found");
    let (s, v) = body(r.put("/autoreplies/nope", discord()).await).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::NOT_FOUND, Some("autoreply_not_found")));

    let (s, v) = body(r.get("/autoreplies/starters").await).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn bad_input_gets_the_right_status_and_a_clear_message() {
    let r = start().await;
    // 422: a pattern that doesn't compile, with where the problem is.
    let (s, v) = body(
        r.post(
            "/autoreplies",
            json!({ "name": "x", "match": { "kind": "regex", "text": "what (game" }, "reply": "y" }),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "bad_pattern");
    let m = v["error"]["message"].as_str().unwrap();
    assert!(m.starts_with("match.text: the pattern has a problem at character 6"), "{m}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("Contains"));
    // 422: a pattern that matches everything.
    let (s, v) = body(
        r.post("/autoreplies", json!({ "name": "x", "match": { "kind": "regex", "text": ".*" }, "reply": "y" })).await,
    )
    .await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("pattern_too_broad")));
    // 413: reply over 500 characters.
    let mut long = discord();
    long["reply"] = json!("x".repeat(501));
    let (s, v) = body(r.post("/autoreplies", long).await).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(v["error"]["code"], "reply_too_long");
    assert!(v["error"]["message"].as_str().unwrap().contains("up to 500"));
    // 400: a field, named.
    let mut empty = discord();
    empty["name"] = json!("  ");
    let (s, v) = body(r.post("/autoreplies", empty).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().starts_with("name:"));
    let mut tpl = discord();
    tpl["reply"] = json!("{oops");
    let (s, v) = body(r.post("/autoreplies", tpl).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().starts_with("reply:"));
    let (s, _) = body(
        r.post("/autoreplies", json!({ "name": "x", "match": { "kind": "fuzzy", "text": "a" }, "reply": "y" })).await,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown match kind");
    let mut cd = discord();
    cd["cooldown_s"] = json!(100_000);
    let (s, v) = body(r.post("/autoreplies", cd).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().starts_with("cooldown_s:"));
    // 415: not JSON.
    let res = r
        .client
        .post(format!("{}/api/v1/autoreplies", r.base))
        .bearer_auth(&r.token)
        .body(discord().to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // 409: an id that's taken.
    let (_, a) = body(r.post("/autoreplies", discord()).await).await;
    let mut dup = discord();
    dup["id"] = a["id"].clone();
    assert_eq!(r.post("/autoreplies", dup).await.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_hundred_rule_limit_is_a_clear_422() {
    let r = start().await;
    for i in 0..100 {
        let res = r
            .post(
                "/autoreplies",
                json!({ "name": format!("r{i}"), "match": { "text": format!("w{i}") }, "reply": "x" }),
            )
            .await;
        assert_eq!(res.status(), StatusCode::CREATED, "rule {i}");
    }
    let (s, v) = body(r.post("/autoreplies", discord()).await).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "too_many_autoreplies");
    assert!(v["error"]["message"].as_str().unwrap().contains("100"));
}

#[tokio::test]
async fn try_it_with_the_saved_list_or_a_draft() {
    let r = start().await;
    r.post("/autoreplies", discord()).await;
    let (s, v) = body(r.post("/autoreplies/try", json!({ "text": "any DISCORD?" })).await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["matched"], true);
    assert_eq!(v["rule_name"], "Discord");
    assert_eq!(v["reply"], "Join: discord.gg/x");
    let (_, v) = body(r.post("/autoreplies/try", json!({ "text": "discordant" })).await).await;
    assert_eq!(v["matched"], false);
    assert_eq!(v["reply"], Value::Null);
    let draft =
        json!({ "name": "", "match": { "kind": "starts_with", "text": "hi" }, "reply": "Hey {user} on {platform}" });
    let (s, v) =
        body(r.post("/autoreplies/try", json!({ "text": "hi chat", "rule": draft, "platform": "kick" })).await).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["reply"], "Hey viewer on Kick");
    let bad = json!({ "name": "x", "match": { "kind": "regex", "text": "[a" }, "reply": "y" });
    let (s, v) = body(r.post("/autoreplies/try", json!({ "text": "a", "rule": bad })).await).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("bad_pattern")));
    let (s, _) = body(r.post("/autoreplies/try", json!({ "text": "a", "platform": "tiktok" })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
