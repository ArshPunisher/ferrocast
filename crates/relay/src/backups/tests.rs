//! Cloud backups: the pure parts, then every route through the real `router()` with a real
//! `FsBlobStore` (every status each can answer), quotas, pruning, the sweep and the lapse.

use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::blob::{BlobStore, FsBlobStore};
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;

fn row(id: &str, bytes: i64, reason: &str, at: i64) -> Row {
    Row {
        id: id.into(),
        sub_id: "s".into(),
        install_id: "i".into(),
        bytes,
        sha256: "0".repeat(64),
        app_version: "1".into(),
        reason: reason.into(),
        part_size: PART_SIZE,
        state: COMPLETE.into(),
        created_at: at,
        updated_at: at,
    }
}

#[test]
fn parts_are_part_size_and_the_last_one_the_rest() {
    let r = Row { bytes: 2 * PART_SIZE + 5, ..row("a", 0, "manual", 0) };
    assert_eq!(r.parts(), 3);
    assert_eq!((r.part_bytes(0), r.part_bytes(1), r.part_bytes(2)), (PART_SIZE, PART_SIZE, 5));
    let exact = Row { bytes: PART_SIZE, ..r.clone() };
    assert_eq!((exact.parts(), exact.part_bytes(0)), (1, PART_SIZE));
    assert!(id_ok(&new_id()));
    for bad in ["", "bk_", "bk_zz", "../x", &format!("bk_{}", "A".repeat(24))] {
        assert!(!id_ok(bad), "{bad}");
    }
}

#[test]
fn pruning_takes_the_oldest_automatic_ones_first() {
    let mb = 1_000_000;
    // Seven backups: the eighth needs one to go, the oldest automatic one.
    let seven: Vec<Row> =
        (0..7).map(|i| row(&format!("b{i}"), mb, if i == 0 || i == 2 { "manual" } else { "automatic" }, i)).collect();
    assert_eq!(to_prune(&seven, mb), Some(vec!["b1".to_string()]));
    // Room enough: nothing goes.
    assert_eq!(to_prune(&seven[..3], mb), Some(vec![]));
    // Size: 200 MB kept + 100 MB new needs 50 MB gone, automatic ones oldest first.
    let big =
        vec![row("m", 100 * mb, "manual", 0), row("a1", 40 * mb, "automatic", 1), row("a2", 60 * mb, "automatic", 2)];
    assert_eq!(to_prune(&big, 100 * mb), Some(vec!["a1".to_string(), "a2".to_string()]));
    // Only backups made by hand: it can't fit.
    let manual = vec![row("m1", 200 * mb, "manual", 0)];
    assert_eq!(to_prune(&manual, 100 * mb), None);
    let full: Vec<Row> = (0..7).map(|i| row(&format!("m{i}"), 1, "manual", i)).collect();
    assert_eq!(to_prune(&full, 1), None);
}

// ── Through the router ───────────────────────────────────────────────────────────────

struct World {
    base: String,
    db: Arc<Db>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
    state: AppState,
}

async fn world_with(blobs: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let store: Option<Arc<dyn BlobStore>> =
        blobs.then(|| Arc::new(FsBlobStore::new(dir.path().join("blobs")).unwrap()) as Arc<dyn BlobStore>);
    let state = AppState::new(cfg, db).with_blobs(store);
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, http: reqwest::Client::new(), dir, state }
}

async fn world() -> World {
    world_with(true).await
}

fn sha(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Bytes that aren't all the same, so a swapped part changes the hash.
fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8 ^ (i / 251) as u8).collect()
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        self.set_sub(sub, "active", Some(now_ms() + 30 * DAY));
        auth
    }

    /// A second PC on the same subscription.
    fn second(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.link_pro_install(id, sub).unwrap();
        auth
    }

    fn set_sub(&self, sub: &str, status: &str, current_end: Option<i64>) {
        let s = Synced { status: status.into(), current_end, cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
    }

    async fn send(&self, method: Method, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, auth: &str, body: Value) -> (u16, Value) {
        self.send(Method::POST, path, Some(auth), Some(body)).await
    }

    async fn get(&self, path: &str, auth: &str) -> (u16, Value) {
        self.send(Method::GET, path, Some(auth), None).await
    }

    async fn part(&self, auth: &str, id: &str, n: i64, bytes: Vec<u8>) -> (u16, Value) {
        let res = self
            .http
            .put(format!("{}/v1/pro/backups/{id}/parts/{n}", self.base))
            .bearer_auth(auth)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn start(&self, auth: &str, bytes: &[u8], reason: &str) -> (u16, Value) {
        let body =
            json!({ "bytes": bytes.len(), "sha256": sha(bytes), "app_version": "0.1.0-beta.9", "reason": reason });
        self.post("/v1/pro/backups", auth, body).await
    }

    /// A whole upload: start, every part, complete. Returns the complete answer.
    async fn upload(&self, auth: &str, bytes: &[u8], reason: &str) -> (u16, Value) {
        let (s, v) = self.start(auth, bytes, reason).await;
        assert_eq!(s, 201, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        let part = v["part_size"].as_u64().unwrap() as usize;
        for (n, chunk) in bytes.chunks(part).enumerate() {
            let (s, v) = self.part(auth, &id, n as i64, chunk.to_vec()).await;
            assert_eq!(s, 200, "part {n}: {v}");
        }
        self.post(&format!("/v1/pro/backups/{id}/complete"), auth, json!({})).await
    }

    fn files(&self) -> Vec<String> {
        let root = self.dir.path().join("blobs");
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path().strip_prefix(&root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }
}

#[tokio::test]
async fn every_route_needs_a_known_install_with_pro() {
    let w = world().await;
    let free = w.install("free");
    let id = new_id();
    let routes: Vec<(Method, String)> = vec![
        (Method::GET, "/v1/pro/backups".into()),
        (Method::POST, "/v1/pro/backups".into()),
        (Method::GET, format!("/v1/pro/backups/{id}")),
        (Method::DELETE, format!("/v1/pro/backups/{id}")),
        (Method::PUT, format!("/v1/pro/backups/{id}/parts/0")),
        (Method::POST, format!("/v1/pro/backups/{id}/complete")),
    ];
    for (m, path) in routes {
        let body = (m == Method::POST).then(|| json!({ "nonsense": true }));
        let (s, v) = w.send(m.clone(), &path, None, body.clone()).await;
        assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")), "{m} {path}");
        let (s, _) = w.send(m.clone(), &path, Some("nobody.secret"), body.clone()).await;
        assert_eq!(s, 401, "{m} {path} unknown install");
        let (s, v) = w.send(m.clone(), &path, Some(&free), body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{m} {path}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Pro"));
    }
}

#[tokio::test]
async fn an_upload_in_parts_round_trips_and_is_shared_by_the_subscription() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let bytes = data(2 * PART_SIZE as usize + 1234);
    let (s, v) = w.start(&auth, &bytes, "manual").await;
    assert_eq!(s, 201, "{v}");
    assert_eq!((v["part_size"].as_i64(), v["parts"].as_i64()), (Some(PART_SIZE), Some(3)));
    let id = v["id"].as_str().unwrap().to_string();
    // Parts can arrive in any order, and again.
    let chunks: Vec<Vec<u8>> = bytes.chunks(PART_SIZE as usize).map(<[u8]>::to_vec).collect();
    for n in [2, 0, 1, 1] {
        let (s, v) = w.part(&auth, &id, n, chunks[n as usize].clone()).await;
        assert_eq!((s, v["part"].as_i64()), (200, Some(n)), "{v}");
    }
    // Not in the list until it's complete.
    assert_eq!(w.get("/v1/pro/backups", &auth).await.1, json!([]));
    let (s, v) = w.post(&format!("/v1/pro/backups/{id}/complete"), &auth, json!({})).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["backup"]["bytes"].as_u64(), Some(bytes.len() as u64));
    assert_eq!(v["backup"]["this_install"], true);
    assert_eq!(v["pruned"], json!([]));
    // Completing again (a lost answer) is fine.
    assert_eq!(w.post(&format!("/v1/pro/backups/{id}/complete"), &auth, json!({})).await.0, 200);
    assert_eq!(w.files(), [format!("backups/sub_1/{id}")], "the parts are gone, one file is left");
    let (s, list) = w.get("/v1/pro/backups", &auth).await;
    assert_eq!(s, 200);
    let one = &list[0];
    assert_eq!(one["id"], id.as_str());
    assert_eq!(one["sha256"], sha(&bytes).as_str());
    assert_eq!((one["reason"].as_str(), one["app_version"].as_str()), (Some("manual"), Some("0.1.0-beta.9")));
    assert_eq!(one["from_install"], "i1");
    // Download streams the same bytes back.
    let res = w.http.get(format!("{}/v1/pro/backups/{id}", w.base)).bearer_auth(&auth).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/octet-stream");
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["x-content-sha256"], sha(&bytes).as_str());
    assert!(res.headers()["content-disposition"].to_str().unwrap().contains(".ferrobackup"));
    assert_eq!(res.bytes().await.unwrap().to_vec(), bytes);
    // Another PC on the subscription sees it as someone else's; another subscription doesn't.
    let other_pc = w.second("i2", "sub_1");
    let (_, l) = w.get("/v1/pro/backups", &other_pc).await;
    assert_eq!((l[0]["this_install"].as_bool(), l[0]["from_install"].as_str()), (Some(false), Some("i1")));
    let stranger = w.pro("i3", "sub_2");
    assert_eq!(w.get("/v1/pro/backups", &stranger).await.1, json!([]));
    assert_eq!(w.get(&format!("/v1/pro/backups/{id}"), &stranger).await.0, 404);
    assert_eq!(w.send(Method::DELETE, &format!("/v1/pro/backups/{id}"), Some(&stranger), None).await.0, 404);
    // Delete.
    assert_eq!(w.send(Method::DELETE, &format!("/v1/pro/backups/{id}"), Some(&other_pc), None).await.0, 204);
    assert_eq!(w.get("/v1/pro/backups", &auth).await.1, json!([]));
    assert!(w.files().is_empty());
    assert_eq!(w.send(Method::DELETE, &format!("/v1/pro/backups/{id}"), Some(&auth), None).await.0, 404);
    assert_eq!(w.get("/v1/pro/backups/nope", &auth).await.0, 404);
}

#[tokio::test]
async fn a_hash_mismatch_deletes_the_upload_and_missing_parts_keep_it() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let bytes = data(PART_SIZE as usize + 10);
    let (_, v) = w.start(&auth, &bytes, "automatic").await;
    let id = v["id"].as_str().unwrap().to_string();
    let complete = format!("/v1/pro/backups/{id}/complete");
    assert_eq!(w.part(&auth, &id, 0, bytes[..PART_SIZE as usize].to_vec()).await.0, 200);
    let (s, v) = w.post(&complete, &auth, json!({})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("parts_missing")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("Part 1"));
    // The last part with other bytes of the same length.
    let mut wrong = bytes[PART_SIZE as usize..].to_vec();
    wrong[0] ^= 0xff;
    assert_eq!(w.part(&auth, &id, 1, wrong).await.0, 200);
    let (s, v) = w.post(&complete, &auth, json!({})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("hash_mismatch")), "{v}");
    assert!(w.files().is_empty(), "the upload and its parts are gone: {:?}", w.files());
    assert_eq!(w.post(&complete, &auth, json!({})).await.0, 404);
    assert_eq!(w.part(&auth, &id, 0, vec![1]).await.0, 404);
}

#[tokio::test]
async fn parts_are_checked_for_type_number_and_size() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let bytes = data(PART_SIZE as usize + 100);
    let (_, v) = w.start(&auth, &bytes, "manual").await;
    let id = v["id"].as_str().unwrap().to_string();
    let url = format!("{}/v1/pro/backups/{id}/parts/0", w.base);
    // Only octet-stream.
    for ct in [None, Some("application/json"), Some("text/plain")] {
        let mut r = w.http.put(&url).bearer_auth(&auth).body(vec![1u8; 10]);
        if let Some(ct) = ct {
            r = r.header("content-type", ct);
        }
        let res = r.send().await.unwrap();
        assert_eq!(res.status(), 415, "{ct:?}");
        let v: Value = res.json().await.unwrap();
        assert_eq!(v["error"]["code"], "unsupported_type");
    }
    // Over part_size: 413 with the limit.
    let (s, v) = w.part(&auth, &id, 0, vec![0u8; PART_SIZE as usize + 1]).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_large")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("8388608 bytes"), "{v}");
    // The wrong size for that part, or a number past the end.
    let (s, v) = w.part(&auth, &id, 0, vec![0u8; 50]).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_part")), "{v}");
    let (s, v) = w.part(&auth, &id, 2, vec![0u8; 50]).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_part")), "{v}");
    assert_eq!(w.part(&auth, &id, -1, vec![0u8; 50]).await.0, 422);
    assert_eq!(w.part(&auth, &id, 1, vec![0u8; 100]).await.0, 200, "the last part is the rest");
    assert_eq!(w.files(), [format!("backups/sub_1/{id}.part1")], "refused parts left nothing");
}

#[tokio::test]
async fn bad_starts_get_400_415_422_or_413() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let ok = json!({ "bytes": 10, "sha256": "a".repeat(64), "app_version": "0.1.0", "reason": "manual" });
    let res =
        w.http.post(format!("{}/v1/pro/backups", w.base)).bearer_auth(&auth).body(ok.to_string()).send().await.unwrap();
    assert_eq!(res.status(), 415);
    let res = w
        .http
        .post(format!("{}/v1/pro/backups", w.base))
        .bearer_auth(&auth)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let (s, v) = w.post("/v1/pro/backups", &auth, json!({ "bytes": 10 })).await;
    assert_eq!(s, 400);
    assert!(v["error"]["message"].as_str().unwrap().contains("sha256"), "{v}");
    for (patch, field) in [
        (json!({ "bytes": 0 }), "bytes"),
        (json!({ "sha256": "xyz" }), "sha256"),
        (json!({ "app_version": "" }), "app_version"),
        (json!({ "app_version": "1.0; rm -rf" }), "app_version"),
        (json!({ "reason": "weekly" }), "reason"),
    ] {
        let mut b = ok.clone();
        for (k, v) in patch.as_object().unwrap() {
            b[k] = v.clone();
        }
        let (s, v) = w.post("/v1/pro/backups", &auth, b).await;
        assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid_backup")), "{field}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(field), "{v}");
    }
    let mut huge = ok.clone();
    huge["bytes"] = json!(MAX_TOTAL + 1);
    let (s, v) = w.post("/v1/pro/backups", &auth, huge).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("backup_quota")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("250 MB"));
    assert_eq!(w.db.quota_used("sub_1", UPLOADS.name, UPLOADS.period).unwrap(), 0, "refusals count nothing");
}

#[tokio::test]
async fn one_upload_at_a_time_per_subscription() {
    let w = world().await;
    let a = w.pro("i1", "sub_1");
    let b = w.second("i2", "sub_1");
    let (_, first) = w.start(&a, b"one", "automatic").await;
    let first_id = first["id"].as_str().unwrap().to_string();
    assert_eq!(w.part(&a, &first_id, 0, b"one".to_vec()).await.0, 200);
    // The other PC waits while the first is moving.
    let (s, v) = w.start(&b, b"two", "manual").await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("upload_in_progress")), "{v}");
    // The same PC starting again (it restarted) replaces its own upload.
    let (s, again) = w.start(&a, b"one!", "automatic").await;
    assert_eq!(s, 201);
    assert_eq!(w.post(&format!("/v1/pro/backups/{first_id}/complete"), &a, json!({})).await.0, 404);
    assert!(w.files().is_empty(), "the replaced upload's part went");
    // A stalled upload from another PC stops blocking after an hour.
    w.db.backdate_backup(again["id"].as_str().unwrap(), BUSY_MS + 1).unwrap();
    assert_eq!(w.start(&b, b"two", "manual").await.0, 201);
}

#[tokio::test]
async fn the_oldest_automatic_backup_goes_to_make_room() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let mut ids = Vec::new();
    let mut add = |id: &Value| ids.push(id["backup"]["id"].as_str().unwrap().to_string());
    for i in 0..UPLOADS.limit {
        let reason = if i == 0 { "manual" } else { "automatic" };
        let (s, v) = w.upload(&auth, format!("backup {i}").as_bytes(), reason).await;
        assert_eq!(s, 201, "{v}");
        add(&v);
        // Distinct times, so oldest is well defined.
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    // The day's six uploads are used: 429 with the reset time.
    let res = w
        .http
        .post(format!("{}/v1/pro/backups", w.base))
        .bearer_auth(&auth)
        .json(&json!({ "bytes": 3, "sha256": sha(b"new"), "app_version": "1", "reason": "manual" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers()["x-quota-reset"].to_str().unwrap().parse::<i64>().unwrap() > now_ms());
    assert!(res.headers().contains_key("retry-after"));
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "backup_upload_quota");
    // The next day: the seventh still fits.
    w.db.c().execute("DELETE FROM quota_usage", []).unwrap();
    let (s, v) = w.upload(&auth, b"backup 6", "automatic").await;
    assert_eq!((s, v["pruned"].clone()), (201, json!([])), "{v}");
    add(&v);
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    // A new day: the eighth backup pushes out the oldest automatic one (not the manual one).
    w.db.c().execute("DELETE FROM quota_usage", []).unwrap();
    let (s, v) = w.upload(&auth, b"backup 7", "automatic").await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["pruned"], json!([ids[1]]));
    let (_, list) = w.get("/v1/pro/backups", &auth).await;
    let listed: Vec<&str> = list.as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert_eq!(listed.len(), MAX_BACKUPS);
    assert!(listed.contains(&ids[0].as_str()) && !listed.contains(&ids[1].as_str()), "{listed:?}");
    assert_eq!(listed[0], v["backup"]["id"].as_str().unwrap(), "newest first");
    assert_eq!(w.files().len(), MAX_BACKUPS);
    // Seven backups made by hand: an eighth can't fit.
    let w2 = world().await;
    let auth2 = w2.pro("i9", "sub_9");
    for i in 0..MAX_BACKUPS {
        if i == 6 {
            w2.db.c().execute("DELETE FROM quota_usage", []).unwrap();
        }
        assert_eq!(w2.upload(&auth2, format!("m{i}").as_bytes(), "manual").await.0, 201);
    }
    let (s, v) = w2.start(&auth2, b"m7", "manual").await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("backup_quota")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("Delete"));
}

#[tokio::test]
async fn status_lists_the_feature_and_its_quota() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let (s, v) = w.get("/v1/pro/status", &auth).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["features"]["backups"], true);
    assert_eq!(v["quotas"]["backup_uploads"]["limit"], 6);
    assert_eq!(v["quotas"]["backup_uploads"]["period"], "day");
    // Without a file store the feature is off and writes say so.
    let bare = world_with(false).await;
    let auth = bare.pro("i1", "sub_1");
    assert_eq!(bare.get("/v1/pro/status", &auth).await.1["features"]["backups"], false);
    let (s, v) = bare.start(&auth, b"x", "manual").await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("storage_unavailable")), "{v}");
}

#[tokio::test]
async fn after_pro_ends_backups_stay_readable_for_30_days_then_go() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let (_, v) = w.upload(&auth, b"keep me", "manual").await;
    let id = v["backup"]["id"].as_str().unwrap().to_string();
    // A second subscription whose upload stalled two days ago.
    let other = w.pro("i2", "sub_2");
    let (_, v) = w.start(&other, b"half", "automatic").await;
    let stalled = v["id"].as_str().unwrap().to_string();
    assert_eq!(w.part(&other, &stalled, 0, b"half".to_vec()).await.0, 200);
    w.db.backdate_backup(&stalled, 2 * DAY).unwrap();
    // Pro lapses (every renewal failed): no new uploads, but list, download and delete stay open.
    w.set_sub("sub_1", "halted", None);
    assert_eq!(w.start(&auth, b"new", "manual").await.0, 402);
    assert_eq!(w.get("/v1/pro/backups", &auth).await.0, 200);
    let res = w.http.get(format!("{}/v1/pro/backups/{id}", w.base)).bearer_auth(&auth).send().await.unwrap();
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"keep me");
    sweep(&w.state).await;
    assert_eq!(w.get("/v1/pro/backups", &auth).await.1.as_array().map(Vec::len), Some(1), "kept for now");
    assert!(w.db.backup("sub_2", &stalled).unwrap().is_none(), "the stalled upload went after a day");
    // Ended 31 days ago: everything goes, and then the routes answer 402.
    w.set_sub("sub_1", "cancelled", Some(now_ms() - 31 * DAY));
    sweep(&w.state).await;
    assert!(w.files().is_empty(), "{:?}", w.files());
    assert_eq!(w.get("/v1/pro/backups", &auth).await.0, 402);
    assert_eq!(w.get(&format!("/v1/pro/backups/{id}"), &auth).await.0, 402);
}
