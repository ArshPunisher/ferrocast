//! Cloud sync and "Use cloud overlays" (docs/CLOUD.md §1.1) against a stand-in relay that
//! keeps versions like `/v1/cloud/docs`: off by default, needs the account, pushes this PC's
//! documents, pulls the web's, the cloud copy wins a conflict (this PC's kept as a copy), and
//! the overlay URLs switch to the cloud's.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::{Engine, PlatformApps};

#[derive(Default)]
struct Cloud {
    on: bool,
    version: i64,
    /// (kind, id) → (version, body or deleted)
    docs: BTreeMap<(String, String), (i64, Option<Value>)>,
}
type S = Arc<Mutex<Cloud>>;

impl Cloud {
    fn write(&mut self, kind: &str, id: &str, body: Option<Value>) -> i64 {
        self.version += 1;
        self.docs.insert((kind.into(), id.into()), (self.version, body));
        self.version
    }
}

async fn relay() -> (String, S) {
    let s: S = Arc::default();
    let app = Router::new()
        .route("/v1/installs", post(|| async { Json(json!({"install_id": "in1", "install_secret": "sec"})) }))
        .route("/v1/pro/status", get(|| async { Json(json!({"pro": false, "status": "none"})) }))
        .route(
            "/v1/cloud/enable",
            post(|State(s): State<S>| async move {
                s.lock().unwrap().on = true;
                Json(json!({"enabled": true}))
            }),
        )
        .route(
            "/v1/cloud/docs",
            get(|State(s): State<S>, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                let since: i64 = q.get("since").and_then(|v| v.parse().ok()).unwrap_or(0);
                let c = s.lock().unwrap();
                let docs: Vec<Value> = c
                    .docs
                    .iter()
                    .filter(|(_, (v, _))| *v > since)
                    .map(|((k, id), (v, b))| json!({"kind": k, "id": id, "version": v, "deleted": b.is_none(), "body": b}))
                    .collect();
                Json(json!({"version": c.version, "more": false, "docs": docs}))
            })
            .put(|State(s): State<S>, Json(b): Json<Value>| async move {
                let mut c = s.lock().unwrap();
                let mut applied = Vec::new();
                let mut conflicts = Vec::new();
                for d in b["docs"].as_array().unwrap() {
                    let (kind, id) = (d["kind"].as_str().unwrap(), d["id"].as_str().unwrap());
                    let at = c.docs.get(&(kind.into(), id.into())).map_or(0, |(v, body)| if body.is_some() { *v } else { 0 });
                    let base = d["base_version"].as_i64().unwrap();
                    if at != base {
                        let (v, body) = c.docs.get(&(kind.into(), id.into())).cloned().unwrap_or((0, None));
                        conflicts.push(json!({"kind": kind, "id": id, "server": {"version": v, "deleted": body.is_none(), "body": body}}));
                        continue;
                    }
                    let body = (!d["body"].is_null()).then(|| {
                        let mut b = d["body"].clone();
                        if kind == "overlay" {
                            b["token"] = json!("");
                        }
                        b
                    });
                    let v = c.write(kind, id, body);
                    applied.push(json!({"kind": kind, "id": id, "version": v}));
                }
                Json(json!({"version": c.version, "applied": applied, "conflicts": conflicts}))
            }),
        )
        .route(
            "/v1/cloud/overlays",
            get(|State(s): State<S>| async move {
                let c = s.lock().unwrap();
                let list: Vec<Value> = c
                    .docs
                    .iter()
                    .filter(|((k, _), (_, b))| k == "overlay" && b.is_some())
                    .map(|((_, id), _)| json!({"id": id, "url": format!("https://overlays.example/o/{id}-cloud/")}))
                    .collect();
                Json(json!(list))
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

#[tokio::test]
async fn off_by_default_and_needs_the_account() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    let v = e.cloud_status().await;
    assert_eq!((v["sync"].as_bool(), v["use_cloud_overlays"].as_bool()), (Some(false), Some(false)));
    let err = e.cloud_set(Some(true), None).await.unwrap_err();
    assert!(matches!(err, crate::EngineError::Refused { status: 409, code: "signed_out", .. }), "{err:?}");
    assert!(!s.lock().unwrap().on, "nothing reached the cloud");
    // Sync never runs while off, signed in or not.
    e.inner.secrets.set(crate::ferro_account::SESSION, "fca_se_1.x").unwrap();
    e.cloud_sync_now().await.unwrap();
    assert!(s.lock().unwrap().docs.is_empty());
}

#[tokio::test]
async fn pushes_pulls_and_the_cloud_wins_a_conflict() {
    let (base, s) = relay().await;
    let (e, _d) = engine(base).await;
    e.inner.secrets.set(crate::ferro_account::SESSION, "fca_se_1.x").unwrap();
    let o = e.create_overlay("Main", ferrocast_core::overlay::Preset::Starter).await.unwrap();

    // On: cloud mode turned on for the account, this PC's documents go up.
    let v = e.cloud_set(Some(true), None).await.unwrap();
    assert_eq!(v["sync"], true);
    assert!(v["error"].is_null(), "{v}");
    {
        let c = s.lock().unwrap();
        assert!(c.on);
        let up = &c.docs[&("overlay".to_string(), o.id.clone())];
        assert_eq!(up.1.as_ref().unwrap()["name"], "Main");
        assert_eq!(up.1.as_ref().unwrap()["token"], "", "this PC's token never leaves it");
        assert!(c.docs.keys().any(|(k, _)| k == "alert_rule"));
        assert!(c.docs.contains_key(&("settings".to_string(), "main".to_string())));
    }

    // The web renames a rule: the next sync brings it here.
    let rule_id = {
        let mut c = s.lock().unwrap();
        let ((_, id), (_, body)) =
            c.docs.iter().find(|((k, _), _)| k == "alert_rule").map(|(k, v)| (k.clone(), v.clone())).unwrap();
        let mut b = body.unwrap();
        b["name"] = json!("From the web");
        c.write("alert_rule", &id, Some(b));
        id
    };
    e.cloud_sync_now().await.unwrap();
    let rules = e.state().rules.clone();
    assert_eq!(rules.iter().find(|r| r.id == rule_id).unwrap().name, "From the web");

    // Both change the overlay: the web's wins, this PC's is kept as a copy.
    {
        let mut c = s.lock().unwrap();
        let mut b = c.docs[&("overlay".to_string(), o.id.clone())].1.clone().unwrap();
        b["name"] = json!("Web name");
        c.write("overlay", &o.id, Some(b));
    }
    let mut mine = e.overlay(&o.id).await.unwrap();
    mine.name = "PC name".into();
    e.update_overlay(mine).await.unwrap();
    let v = e.cloud_sync_now().await.unwrap();
    let names: Vec<String> = e.overlays().await.unwrap().into_iter().map(|o| o.name).collect();
    assert!(names.contains(&"Web name".to_string()), "{names:?}");
    assert!(names.contains(&"PC name (this PC)".to_string()), "{names:?}");
    assert!(v["notes"][0].as_str().unwrap().contains("PC name (this PC)"), "{v}");
    // The copy goes up as a new overlay; nothing is left to push after.
    let before = s.lock().unwrap().version;
    e.cloud_sync_now().await.unwrap();
    e.cloud_sync_now().await.unwrap();
    let after = s.lock().unwrap().version;
    assert!(after - before <= 1, "a quiet sync pushes nothing ({before} → {after})");

    // A delete on the web deletes it here.
    s.lock().unwrap().write("overlay", &o.id, None);
    e.cloud_sync_now().await.unwrap();
    assert!(e.overlay(&o.id).await.is_err());
}

#[tokio::test]
async fn use_cloud_overlays_shows_the_cloud_urls() {
    let (base, _s) = relay().await;
    let (e, _d) = engine(base).await;
    e.inner.secrets.set(crate::ferro_account::SESSION, "fca_se_1.x").unwrap();
    let o = e.create_overlay("Main", ferrocast_core::overlay::Preset::Starter).await.unwrap();
    assert!(o.url.contains("/o/"), "{}", o.url);
    e.cloud_set(Some(true), Some(true)).await.unwrap();
    let list = e.overlays().await.unwrap();
    let main = list.iter().find(|x| x.id == o.id).unwrap();
    assert_eq!(main.url, format!("https://overlays.example/o/{}-cloud/", o.id));
    // Off again: this PC's own URL.
    e.cloud_set(None, Some(false)).await.unwrap();
    let main = e.overlays().await.unwrap().into_iter().find(|x| x.id == o.id).unwrap();
    assert_eq!(main.url, o.url);
}
