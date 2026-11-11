//! `/api/v1/plugins` over a real socket: install, list, off, remove, and every refusal with
//! its status and our error shape.

use std::io::Write;

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::*;

const FX: &str = "fn effect(uv: vec2<f32>) -> vec4<f32> { let c = pic(uv); return vec4<f32>(1.0 - c.rgb, c.a); }";

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
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.client.put(self.url(path)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }
    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client.delete(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn upload(&self, bytes: Vec<u8>) -> reqwest::Response {
        let form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(bytes).file_name("plugin.zip"));
        self.client.post(self.url("/plugins")).bearer_auth(&self.token).multipart(form).send().await.unwrap()
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

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, data) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    buf.into_inner()
}

fn plugin(id: &str, effect: &str) -> Vec<u8> {
    let m = json!({ "id": id, "name": "Invert", "version": "1.0.0", "author": "Ana", "kind": "effect", "effect": "effect.wgsl" });
    zip_of(&[("plugin.json", m.to_string().as_bytes()), ("effect.wgsl", effect.as_bytes())])
}

async fn code(res: reqwest::Response) -> (StatusCode, String) {
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    assert!(v["error"]["message"].is_string(), "{status}: not our error shape: {v}");
    assert!(v["error"]["hint"].is_string() || v["error"]["hint"].is_null());
    (status, v["error"]["code"].as_str().unwrap_or("").to_string())
}

#[tokio::test]
async fn a_plugin_installs_lists_turns_off_and_goes() {
    let s = start().await;
    assert_eq!(s.get("/plugins").await.json::<Value>().await.unwrap(), json!([]));
    let res = s.upload(plugin("com.ana.invert", FX)).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let p: Value = res.json().await.unwrap();
    assert_eq!(
        (p["id"].as_str(), p["on"].as_bool(), p["problem"].is_null()),
        (Some("com.ana.invert"), Some(true), true)
    );
    let list: Value = s.get("/plugins").await.json().await.unwrap();
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    let off: Value = s.put("/plugins/com.ana.invert", json!({ "on": false })).await.json().await.unwrap();
    assert_eq!(off["on"], json!(false));
    assert_eq!(s.delete("/plugins/com.ana.invert").await.status(), StatusCode::NO_CONTENT);
    assert_eq!(s.get("/plugins").await.json::<Value>().await.unwrap(), json!([]));
}

#[tokio::test]
async fn every_refusal_has_its_status_and_says_why() {
    let s = start().await;
    // Not a form at all.
    let res = s.client.post(s.url("/plugins")).bearer_auth(&s.token).json(&json!({})).send().await.unwrap();
    assert_eq!(code(res).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Empty, too big, not a zip, a broken effect.
    assert_eq!(code(s.upload(Vec::new()).await).await.0, StatusCode::BAD_REQUEST);
    let big = vec![0u8; ferrocast_engine::plugins::MAX_ZIP as usize + 1];
    assert_eq!(code(s.upload(big).await).await, (StatusCode::PAYLOAD_TOO_LARGE, "plugin_too_big".into()));
    assert_eq!(
        code(s.upload(b"hello".to_vec()).await).await,
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "plugin_not_a_plugin".into())
    );
    let broken = plugin("com.ana.broken", "fn effect(uv: vec2<f32>) -> vec4<f32> { return nope; }");
    assert_eq!(code(s.upload(broken).await).await, (StatusCode::UNPROCESSABLE_ENTITY, "plugin_effect_broken".into()));
    // A program inside is refused by its type.
    let m = json!({ "id": "com.ana.x", "name": "X", "version": "1", "kind": "effect", "effect": "effect.wgsl" });
    let evil = zip_of(&[("plugin.json", m.to_string().as_bytes()), ("effect.wgsl", FX.as_bytes()), ("run.exe", b"MZ")]);
    assert_eq!(code(s.upload(evil).await).await, (StatusCode::UNSUPPORTED_MEDIA_TYPE, "plugin_file_type".into()));
    // Unknown plugins, and a body that isn't { on }.
    assert_eq!(code(s.put("/plugins/nope.nope", json!({ "on": true })).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(code(s.delete("/plugins/nope.nope").await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(s.put("/plugins/nope.nope", json!({ "on": "yes" })).await.status(), StatusCode::BAD_REQUEST);
    // Nothing got in.
    assert_eq!(s.get("/plugins").await.json::<Value>().await.unwrap(), json!([]));
}

/// A panel's page on its own (not the frame's srcdoc, which the dashboard's policy kept from
/// running scripts): its inline code may run, nothing else loads, only the dashboard frames it.
#[tokio::test]
async fn a_panel_is_a_page_of_its_own() {
    let s = start().await;
    let m = json!({ "id": "com.ana.panel", "name": "Buttons", "version": "1.0.0", "kind": "panel", "panel": "panel.html", "actions": ["scene"] });
    let html = b"<button onclick=\"parent.postMessage({type:'act',step:{type:'scene',name:'A'}},'*')\">A</button><script>1</script>";
    let z = zip_of(&[("plugin.json", m.to_string().as_bytes()), ("panel.html", html)]);
    assert_eq!(s.upload(z).await.status(), StatusCode::CREATED);
    let page = s.client.get(format!("{}/plugin-panel/com.ana.panel", s.base)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers().get("content-security-policy").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    for want in ["script-src 'unsafe-inline'", "default-src 'none'", "frame-ancestors 'self'", "sandbox allow-scripts"]
    {
        assert!(csp.contains(want), "{want} in {csp}");
    }
    assert!(page.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    assert!(page.text().await.unwrap().contains("<button onclick="));
    // Off, or not a panel: no page.
    s.put("/plugins/com.ana.panel", json!({ "on": false })).await;
    let off = s.client.get(format!("{}/plugin-panel/com.ana.panel", s.base)).send().await.unwrap();
    assert!(off.status().is_client_error());
    let none = s.client.get(format!("{}/plugin-panel/nope.nope", s.base)).send().await.unwrap();
    assert_eq!(none.status(), StatusCode::NOT_FOUND);
}

/// The catalog before the owner's key is in the app: closed, said plainly (503 with a hint), and
/// nothing can be installed from it.
#[tokio::test]
async fn the_catalog_is_closed_until_its_key_is_in() {
    let s = start().await;
    let r = s.client.get(format!("{}/api/v1/plugins/catalog", s.base)).bearer_auth(&s.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "catalog_not_open");
    assert!(body["error"]["hint"].as_str().is_some_and(|h| !h.is_empty()));
    let r = s
        .client
        .post(format!("{}/api/v1/plugins/catalog/install", s.base))
        .bearer_auth(&s.token)
        .json(&json!({ "id": "com.ana.invert" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(s.get("/plugins").await.json::<Value>().await.unwrap().as_array().is_some_and(|a| a.is_empty()));
}
