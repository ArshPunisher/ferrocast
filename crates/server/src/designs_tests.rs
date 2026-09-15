//! `/api/v1/designs` over a real socket: every route, and every way it can be refused,
//! answers with the right status and our error shape (API.md §1, §4 Designs).

use std::io::Write;

use reqwest::StatusCode;
use serde_json::{Value, json};

use super::*;

struct Running {
    base: String,
    token: String,
    client: reqwest::Client,
    engine: Engine,
    _dir: tempfile::TempDir,
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base)
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client.post(self.url(path)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }
    async fn post_empty(&self, path: &str) -> reqwest::Response {
        self.client.post(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn put(&self, path: &str, body: Value) -> reqwest::Response {
        self.client.put(self.url(path)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }
    async fn delete(&self, path: &str) -> reqwest::Response {
        self.client.delete(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }
    async fn upload(&self, bytes: Vec<u8>, field: &str) -> reqwest::Response {
        let form = reqwest::multipart::Form::new()
            .part(field.to_string(), reqwest::multipart::Part::bytes(bytes).file_name("shared.ferrodesign"));
        self.client.post(self.url("/designs/import")).bearer_auth(&self.token).multipart(form).send().await.unwrap()
    }
}

async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
    let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
    // A known catalogue, never whatever web build happens to be next to this repo.
    engine.set_builtin_designs(crate::assets::parse_catalog(CATALOGUE.as_bytes())).await;
    Running {
        base: server.base_url(),
        token: server.sessions.desktop_token().to_string(),
        client: reqwest::Client::new(),
        engine,
        _dir: dir,
    }
}

const ALERT: &str = include_str!("../../core/fixtures/designs/alert-design.json");
const SCENE: &str = include_str!("../../core/fixtures/designs/scene-design.json");

/// A built-in alert design, a built-in scene with its mark, and a built-in widget design.
static CATALOGUE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let mut alert: Value = serde_json::from_str(ALERT).unwrap();
    alert["id"] = json!("builtin.molten");
    alert["built_in"] = json!(true);
    alert["branded"] = json!(true);
    let mut scene: Value = serde_json::from_str(SCENE).unwrap();
    scene["id"] = json!("builtin.brb");
    scene["built_in"] = json!(true);
    scene["branded"] = json!(true);
    scene["content"]["layers"].as_array_mut().unwrap().push(json!({
        "id": "spark", "kind": "layer", "x": 1840, "y": 1010, "w": 56, "h": 48, "config": { "type": "brand", "variant": "mark" }
    }));
    let mut widget: Value = serde_json::from_str(ALERT).unwrap();
    widget["id"] = json!("builtin.label-tag");
    widget["built_in"] = json!(true);
    widget["branded"] = json!(true);
    widget["kind"] = json!("widget");
    widget["content"] = json!({ "widget": "label", "w": 420, "h": 64,
        "config": { "source": "latest_follower", "template": "{value}", "theme": "molten" },
        "style": { "variant": "tag", "accent": "#33d4ff", "brand": "badge" } });
    json!({ "format": "ferrocast-designs", "version": 1, "designs": [alert, scene, widget, { "garbage": true }] })
        .to_string()
});

async fn code(res: reqwest::Response) -> (StatusCode, String, String) {
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    let message = v["error"]["message"].as_str().unwrap_or("").to_string();
    assert!(v["error"]["message"].is_string(), "{status}: not our error shape: {v}");
    (status, v["error"]["code"].as_str().unwrap_or("").to_string(), message)
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

fn manifest(version: u32) -> Vec<u8> {
    serde_json::to_vec(&json!({ "format": "ferrodesign", "version": version, "app_version": "x", "created_at": 0,
        "name": "Shared", "kind": "alert", "designs": 1, "files": {} }))
    .unwrap()
}

#[tokio::test]
async fn the_catalogue_reads_only_good_built_ins() {
    assert_eq!(crate::assets::parse_catalog(b"not json"), vec![]);
    assert_eq!(crate::assets::parse_catalog(br#"{"format":"other","version":1,"designs":[]}"#), vec![]);
    assert_eq!(crate::assets::parse_catalog(br#"{"format":"ferrocast-designs","version":9,"designs":[]}"#), vec![]);
    assert_eq!(crate::assets::parse_catalog(CATALOGUE.as_bytes()).len(), 3, "the garbage entry is skipped");
}

#[tokio::test]
async fn listing_reading_and_auth() {
    let r = start().await;
    let res = r.client.get(r.url("/designs")).send().await.unwrap();
    assert_eq!(code(res).await.0, StatusCode::UNAUTHORIZED);
    let all: Value = r.get("/designs").await.json().await.unwrap();
    assert_eq!(all.as_array().unwrap().len(), 3);
    assert_eq!(all[0]["id"], "builtin.molten");
    assert_eq!(all[0]["branded"], true);
    let scenes: Value = r.get("/designs?kind=scene").await.json().await.unwrap();
    assert_eq!(scenes.as_array().unwrap().len(), 1);
    let (s, c, m) = code(r.get("/designs?kind=sticker").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    assert!(m.starts_with("kind:"), "{m}");
    let one: Value = r.get("/designs/builtin.brb").await.json().await.unwrap();
    assert_eq!(one["kind"], "scene");
    assert_eq!(code(r.get("/designs/nope").await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(code(r.get("/designs/builtin.nope").await).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn creating_and_editing_refuse_every_kind_of_bad_input() {
    let r = start().await;
    let mut body: Value = serde_json::from_str(ALERT).unwrap();
    body["built_in"] = json!(true);
    body["branded"] = json!(true);
    let res = r.post("/designs", body.clone()).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let made: Value = res.json().await.unwrap();
    assert_eq!((made["built_in"].clone(), made["branded"].clone()), (json!(false), json!(false)));
    let id = made["id"].as_str().unwrap().to_string();

    // 400: not JSON, a field of the wrong type, a field that fails a check (named).
    let res = r
        .client
        .post(r.url("/designs"))
        .bearer_auth(&r.token)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(code(res).await.0, StatusCode::BAD_REQUEST);
    let mut wrong = body.clone();
    wrong["content"]["events"]["tip"]["variations"][0]["look"]["frame"] = json!("iframe");
    assert_eq!(code(r.post("/designs", wrong).await).await.0, StatusCode::BAD_REQUEST);
    let mut bad = body.clone();
    bad["content"]["events"]["tip"]["variations"][0]["look"]["colors"]["highlight"] = json!("red;}");
    let (s, c, m) = code(r.post("/designs", bad).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    assert!(m.contains("look.colors.highlight"), "{m}");
    // 415: not JSON at all. 413: over the JSON limit.
    let res = r.client.post(r.url("/designs")).bearer_auth(&r.token).body("x").send().await.unwrap();
    assert_eq!(code(res).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let mut huge = body.clone();
    huge["description"] = json!("x".repeat(2 * 1024 * 1024));
    assert_eq!(code(r.post("/designs", huge).await).await.0, StatusCode::PAYLOAD_TOO_LARGE);

    // Edits.
    let mut edit = made.clone();
    edit["name"] = json!("Renamed");
    let saved: Value = r.put(&format!("/designs/{id}"), edit.clone()).await.json().await.unwrap();
    assert_eq!(saved["name"], "Renamed");
    let (s, c, _) = code(r.put("/designs/builtin.molten", edit.clone()).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "built_in"));
    assert_eq!(code(r.put("/designs/missing", edit.clone()).await).await.0, StatusCode::NOT_FOUND);
    let mut scene: Value = serde_json::from_str(SCENE).unwrap();
    scene["id"] = json!(id);
    assert_eq!(code(r.put(&format!("/designs/{id}"), scene).await).await.0, StatusCode::CONFLICT, "kind can't change");
    let mut forged = edit;
    forged["branded"] = json!(true);
    assert_eq!(code(r.put(&format!("/designs/{id}"), forged).await).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn duplicate_use_active_and_delete() {
    let r = start().await;
    let res = r.post_empty("/designs/builtin.molten/duplicate").await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let copy: Value = res.json().await.unwrap();
    assert_eq!(copy["name"], "Molten test copy");
    assert_eq!(copy["branded"], false);
    let named: Value =
        r.post("/designs/builtin.brb/duplicate", json!({ "name": "My BRB" })).await.json().await.unwrap();
    assert_eq!(named["name"], "My BRB");
    assert!(!named.to_string().contains("\"brand\""), "the mark layer is gone from the copy");
    assert_eq!(code(r.post_empty("/designs/nope/duplicate").await).await.0, StatusCode::NOT_FOUND);
    let res = r
        .client
        .post(r.url("/designs/builtin.molten/duplicate"))
        .bearer_auth(&r.token)
        .header("content-type", "application/json")
        .body("{bad")
        .send()
        .await
        .unwrap();
    assert_eq!(code(res).await.0, StatusCode::BAD_REQUEST);

    // The alert design in use.
    let none: Value = r.get("/designs/active-alert").await.json().await.unwrap();
    assert_eq!(none["design_id"], Value::Null);
    let used: Value = r.post_empty("/designs/builtin.molten/use").await.json().await.unwrap();
    assert_eq!(used["alert_design"], "builtin.molten");
    let (s, c, m) = code(r.put("/designs/active-alert", json!({ "design_id": "builtin.brb" })).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    assert!(m.contains("pick an alert design"), "{m}");
    assert_eq!(
        code(r.put("/designs/active-alert", json!({ "design_id": "gone" })).await).await.0,
        StatusCode::NOT_FOUND
    );
    let cid = copy["id"].as_str().unwrap();
    let set: Value = r.put("/designs/active-alert", json!({ "design_id": cid })).await.json().await.unwrap();
    assert_eq!(set["design_id"], cid);
    let (s, c, _) = code(r.delete(&format!("/designs/{cid}")).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "in_use"));
    r.put("/designs/active-alert", json!({ "design_id": null })).await;
    assert_eq!(r.delete(&format!("/designs/{cid}")).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(code(r.delete(&format!("/designs/{cid}")).await).await.0, StatusCode::NOT_FOUND);
    let (s, c, _) = code(r.delete("/designs/builtin.molten").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "built_in"));

    // Scenes become overlays; the mark of a built-in can't be edited away.
    let used: Value = r.post("/designs/builtin.brb/use", json!({})).await.json().await.unwrap();
    let overlay = used["overlays"][0].clone();
    assert_eq!(overlay["design"]["branded"], true);
    let mut widgets = overlay["widgets"].as_array().unwrap().clone();
    widgets.retain(|w| w["config"]["type"] != "brand");
    let edit = json!({ "name": overlay["name"], "width": 1920, "height": 1080, "widgets": widgets });
    let (s, c, _) = code(r.put(&format!("/overlays/{}", overlay["id"].as_str().unwrap()), edit).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "branding"));
    let (s, _, m) = code(r.post("/designs/builtin.label-tag/use", json!({})).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(m.starts_with("overlay_id:"), "{m}");
    assert_eq!(
        code(r.post("/designs/builtin.label-tag/use", json!({ "overlay_id": "nope" })).await).await.0,
        StatusCode::NOT_FOUND
    );
    let added: Value =
        r.post("/designs/builtin.label-tag/use", json!({ "overlay_id": overlay["id"] })).await.json().await.unwrap();
    let label = added["overlays"][0]["widgets"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(label["config"]["style"]["variant"], "tag");
    assert_eq!(code(r.post("/designs/builtin.brb/use", json!({ "scenes": 3 })).await).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_alerts_take_a_design_and_an_amount() {
    let r = start().await;
    let res = r.post("/test", json!({ "kind": "tip", "design_id": "builtin.molten", "amount": 1500 })).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    assert_eq!(
        code(r.post("/test", json!({ "kind": "tip", "design_id": "gone" })).await).await.0,
        StatusCode::NOT_FOUND
    );
    let (s, _, m) = code(r.post("/test", json!({ "kind": "tip", "amount": 0 })).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(m.starts_with("amount:"), "{m}");
    assert_eq!(code(r.post("/test", json!({ "kind": "follow", "amount": 5 })).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        code(r.post("/test", json!({ "kind": "tip", "design_id": "builtin.brb" })).await).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn design_files_export_and_import_with_every_refusal() {
    let r = start().await;
    let (s, c, _) = code(r.post_empty("/designs/builtin.molten/export").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "built_in"));
    assert_eq!(code(r.post_empty("/designs/nope/export").await).await.0, StatusCode::NOT_FOUND);
    let copy: Value = r.post_empty("/designs/builtin.molten/duplicate").await.json().await.unwrap();
    let res = r.post_empty(&format!("/designs/{}/export", copy["id"].as_str().unwrap())).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/zip");
    let disposition = res.headers()["content-disposition"].to_str().unwrap().to_string();
    assert!(disposition.contains("Molten-test-copy.ferrodesign"), "{disposition}");
    let file = res.bytes().await.unwrap().to_vec();

    let res = r.upload(file.clone(), "file").await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let report: Value = res.json().await.unwrap();
    assert_eq!(report["designs"][0]["origin"]["type"], "import");
    assert_ne!(report["designs"][0]["id"], copy["id"]);

    let design = serde_json::to_vec(&serde_json::from_str::<Value>(ALERT).unwrap()).unwrap();
    let cases: Vec<(Vec<u8>, StatusCode, &str)> = vec![
        (b"not a zip".to_vec(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_design_file"),
        (zip_of(&[("designs/1.json", &design)]), StatusCode::UNSUPPORTED_MEDIA_TYPE, "bad_design_file"),
        (
            zip_of(&[("manifest.json", &manifest(1)), ("designs/1.json", &design), ("run.js", b"alert(1)")]),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsafe_design_file",
        ),
        (
            zip_of(&[
                ("manifest.json", &manifest(1)),
                ("designs/1.json", &design),
                (&format!("media/{}.png", "0".repeat(64)), b"\x89PNG\r\n\x1a\n"),
            ]),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "damaged_design_file",
        ),
        (
            zip_of(&[("manifest.json", &manifest(3)), ("designs/1.json", &design)]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "design_too_new",
        ),
        (
            zip_of(&[("manifest.json", &manifest(1)), ("designs/1.json", b"{\"schema\":1}")]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_design",
        ),
    ];
    for (bytes, status, want) in cases {
        let (s, c, m) = code(r.upload(bytes, "file").await).await;
        assert_eq!((s, c.as_str()), (status, want), "{m}");
    }
    let (s, _, m) = code(r.upload(file.clone(), "other").await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(m.contains("\"file\""), "{m}");
    assert_eq!(code(r.upload(Vec::new(), "file").await).await.0, StatusCode::BAD_REQUEST);
    let res = r
        .client
        .post(r.url("/designs/import"))
        .bearer_auth(&r.token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (s, c, _) = code(res).await;
    assert_eq!((s, c.as_str()), (StatusCode::UNSUPPORTED_MEDIA_TYPE, "wrong_content_type"));
    let big = vec![0u8; ferrocast_store::designfile::MAX_FILE as usize + 1024];
    let (s, c, m) = code(r.upload(big, "file").await).await;
    assert_eq!((s, c.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
    assert!(m.contains("100 MB"), "{m}");
    assert_eq!(r.engine.designs(None).await.unwrap().len(), 5, "3 built-ins, the copy and one import");
}

#[tokio::test]
async fn media_a_design_uses_cant_be_deleted() {
    let r = start().await;
    let form = reqwest::multipart::Form::new()
        .part("file", reqwest::multipart::Part::bytes(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec()).file_name("a.png"));
    let up: Value = r
        .client
        .post(r.url("/media"))
        .bearer_auth(&r.token)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mid = up["id"].as_str().unwrap();
    let mut d: Value = serde_json::from_str(ALERT).unwrap();
    d["content"]["events"]["tip"]["variations"][0]["look"]["media"] = json!({ "kind": "image", "ref": mid });
    let made: Value = r.post("/designs", d).await.json().await.unwrap();
    let (s, c, m) = code(r.delete(&format!("/media/{mid}")).await).await;
    assert_eq!((s, c.as_str()), (StatusCode::CONFLICT, "in_use"));
    assert!(m.contains(made["name"].as_str().unwrap()), "{m}");
}

/// The real catalogue from the web build (when it's next to this repo): every built-in the
/// web side ships must parse and pass the engine's own checks, so the two never drift.
#[tokio::test]
async fn the_web_catalogue_passes_the_engines_checks() {
    let path = std::env::var("FERROCAST_WEB_DIST")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../ferrocast-web/dist"))
        .join("overlays/designs/catalog.json");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("no web build at {}; skipped", path.display());
        return;
    };
    let raw: Value = serde_json::from_slice(&bytes).unwrap();
    let count = raw["designs"].as_array().map(Vec::len).unwrap_or(0);
    let parsed = crate::assets::parse_catalog(&bytes);
    assert_eq!(parsed.len(), count, "every built-in parses in Rust");
    for d in &parsed {
        d.validate().unwrap_or_else(|e| panic!("{}: {e}", d.id));
        assert!(d.id.starts_with("builtin.") && d.built_in && d.branded, "{}", d.id);
    }
    let r = start().await;
    r.engine.set_builtin_designs(parsed).await;
    assert_eq!(r.engine.builtin_designs().len(), count);
}
