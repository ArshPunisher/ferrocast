//! Crash reports through the real `router()` with a real `FsBlobStore`: auth, every cap and
//! status, the rate limits, dedupe, eviction, retention, and the admin shapes the Control
//! Center is built against (docs/ADMIN-API.md § Crash reports).

use std::io::Write;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::admin::AdminConfig;
use crate::blob::{BlobStore, FsBlobStore};
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const SIG_A: &str = "0123456789abcdef0123456789abcdef";
const SIG_B: &str = "fedcba9876543210fedcba9876543210";

fn token() -> String {
    ["0123456789abcdef"; 4].concat()
}

struct W {
    base: String,
    st: AppState,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

async fn world(limits: Limits) -> W {
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
    let store: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(dir.path().join("blobs")).unwrap());
    let st = AppState::new(cfg, db)
        .with_blobs(Some(store))
        .with_admin(AdminConfig::new(Some(&token()), None))
        .with_crash_limits(limits);
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    W { base, st, http: reqwest::Client::new(), dir }
}

fn gz(text: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(text.as_bytes()).unwrap();
    e.finish().unwrap()
}

fn envelope(sig: &str, version: &str, bundle: &[u8]) -> Value {
    json!({
        "version": version, "os": "windows 11", "arch": "x86_64", "kind": "crash", "signature": sig,
        "cause_codes": ["gpu_lost", "low_memory"], "message": "panicked at src/lib.rs:1\nsecond line",
        "bundle": base64::engine::general_purpose::STANDARD.encode(bundle),
    })
}

fn good(sig: &str) -> Value {
    envelope(sig, "3.8.1", &gz("=== SUMMARY ===\nhello\n"))
}

impl W {
    /// A new install without `POST /v1/installs` (that has its own per-address limit).
    fn install(&self) -> (String, String) {
        let id = format!("inst{}", crate::random(9));
        let secret = crate::random(24);
        self.st.db.add_install(&id, &crate::hash(&secret)).unwrap();
        (format!("{id}.{secret}"), id)
    }

    async fn send(
        &self,
        auth: Option<&str>,
        ip: Option<&str>,
        body: Vec<u8>,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        let mut r = self.http.post(format!("{}/v1/crash-reports", self.base)).body(body);
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(ip) = ip {
            r = r.header("cf-connecting-ip", ip);
        }
        let res = r.send().await.unwrap();
        let (s, h) = (res.status().as_u16(), res.headers().clone());
        (s, h, res.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, auth: &str, body: &Value) -> (u16, reqwest::header::HeaderMap, Value) {
        self.send(Some(auth), None, serde_json::to_vec(body).unwrap()).await
    }

    async fn admin(&self, method: &str, path: &str) -> (u16, reqwest::header::HeaderMap, axum::body::Bytes) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let res = self
            .http
            .request(m, format!("{}/admin/v1/crash-reports{path}", self.base))
            .bearer_auth(token())
            .send()
            .await
            .unwrap();
        (res.status().as_u16(), res.headers().clone(), res.bytes().await.unwrap())
    }

    async fn get(&self, path: &str) -> Value {
        let (s, _, b) = self.admin("GET", path).await;
        assert_eq!(s, 200, "{path}: {}", String::from_utf8_lossy(&b));
        serde_json::from_slice(&b).unwrap()
    }

    fn blob_exists(&self, id: &str) -> bool {
        self.dir.path().join("blobs/crash-reports").join(id).exists()
    }
}

fn limits() -> Limits {
    // Plenty of room, so a test that isn't about a limit never meets one.
    Limits { per_install_hour: 1000, per_install_day: 1000, per_ip_hour: 1000, ..Limits::default() }
}

// ── Auth, shape, fields ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_upload_needs_a_known_install_and_its_secret() {
    let w = world(limits()).await;
    let body = serde_json::to_vec(&good(SIG_A)).unwrap();
    let (s, h, v) = w.send(None, None, body.clone()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (401, Some("unauthorized")));
    assert!(h.get("retry-after").is_none());
    let (auth, id) = w.install();
    let wrong = format!("{id}.wrong-secret");
    assert_eq!(w.send(Some(&wrong), None, body.clone()).await.0, 401);
    assert_eq!(w.send(Some("nodot"), None, body.clone()).await.0, 401);
    assert_eq!(w.send(Some(&auth), None, body).await.0, 201);
    assert_eq!(w.st.db.crash_totals().unwrap().0, 1, "only the good one was kept");
}

#[tokio::test]
async fn a_good_upload_is_kept_as_an_opaque_blob_under_a_key_the_relay_made() {
    let w = world(limits()).await;
    let (auth, id) = w.install();
    let bundle = gz("=== SUMMARY ===\nhello\n");
    let (s, h, v) = w.post(&auth, &envelope(SIG_A, "3.8.1", &bundle)).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(h["cache-control"], "no-store");
    let rid = v["id"].as_str().unwrap();
    assert!(report_id_ok(rid), "{rid}");
    assert_eq!(v["signature"], SIG_A);
    // The file is exactly the bytes sent, under the relay's own key.
    let stored = std::fs::read(w.dir.path().join("blobs/crash-reports").join(rid)).unwrap();
    assert_eq!(stored, bundle);
    let r = w.st.db.crash_report(rid).unwrap().unwrap();
    assert_eq!(r.blob_key.as_deref(), Some(format!("crash-reports/{rid}").as_str()));
    // The install id is nowhere in the crash tables, only its keyed hash.
    assert_ne!(r.install_hash, id);
    assert_eq!(r.install_hash.len(), 64);
    let dump = {
        let c = w.st.db.c();
        let mut all = String::new();
        for t in ["crash_reports", "crash_facets", "crash_rate", "crash_signatures"] {
            let mut st = c.prepare(&format!("SELECT * FROM {t}")).unwrap();
            let n = st.column_count();
            let mut rows = st.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                for i in 0..n {
                    all.push_str(&format!("{:?}|", row.get_ref(i).unwrap()));
                }
            }
        }
        all
    };
    assert!(!dump.contains(&id), "the install id must not be stored in the crash tables");
    // The same install hashes the same way every time; another install differently.
    let (_, h2) = (0, w.st.crash.install_hash(&w.st, &id).unwrap());
    assert_eq!(h2, r.install_hash);
    let (_, other) = w.install();
    assert_ne!(w.st.crash.install_hash(&w.st, &other).unwrap(), h2);
}

#[tokio::test]
async fn bad_fields_are_400_with_the_error_shape() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let ok = good(SIG_A);
    let with = |k: &str, v: Value| {
        let mut e = ok.clone();
        e[k] = v;
        e
    };
    let cases = vec![
        with("version", json!("")),
        with("version", json!("v".repeat(33))),
        with("version", json!("3.8.1\n")),
        with("version", json!(7)),
        with("os", json!("x".repeat(65))),
        with("os", json!("win\u{1b}[31m")),
        with("arch", json!("a".repeat(17))),
        with("arch", json!("x86 64")),
        with("kind", json!("exploded")),
        with("signature", json!("../../etc/passwd")),
        with("signature", json!("ABCDEF0123456789")),
        with("signature", json!("abc")),
        with("signature", json!("a".repeat(129))),
        with("cause_codes", json!(vec!["a"; 9])),
        with("cause_codes", json!(["x".repeat(65)])),
        with("cause_codes", json!(["has space"])),
        with("cause_codes", json!("gpu_lost")),
        with("message", json!("m".repeat(2049))),
        with("bundle", json!("!!!not base64!!!")),
        with("bundle", json!(base64::engine::general_purpose::STANDARD.encode(b"plain text, not gzip at all"))),
        with("bundle", json!(null)),
    ];
    for (i, c) in cases.iter().enumerate() {
        let (s, _, v) = w.post(&auth, c).await;
        assert_eq!(s, 400, "case {i}: {c}");
        assert!(v["error"]["code"].is_string() && v["error"]["message"].is_string(), "case {i}: {v}");
    }
    for raw in [b"not json".to_vec(), b"[1,2]".to_vec(), Vec::new()] {
        let (s, _, v) = w.send(Some(&auth), None, raw).await;
        assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_json")));
    }
    assert_eq!(w.st.db.crash_totals().unwrap(), (0, 0, 0), "nothing was kept");
}

#[tokio::test]
async fn a_message_with_control_characters_is_cleaned_not_stored_raw() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let mut e = good(SIG_A);
    e["message"] = json!("boom\u{1b}[2J\u{0}\u{7}!\nline two");
    let (s, _, v) = w.post(&auth, &e).await;
    assert_eq!(s, 201);
    let r = w.st.db.crash_report(v["id"].as_str().unwrap()).unwrap().unwrap();
    assert!(!r.message.chars().any(|c| c.is_control() && c != '\n' && c != '\t'), "{:?}", r.message);
    assert_eq!(w.st.db.crash_signature(SIG_A).unwrap().unwrap().title, "boom [2J  !");
}

#[tokio::test]
async fn a_body_over_the_cap_is_413_in_the_error_shape_and_nothing_is_kept() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    // No Content-Length trick: a real 1.5 MB body.
    let huge = vec![b'x'; 1_500_000];
    let (s, _, v) = w.send(Some(&auth), None, huge).await;
    assert_eq!((s, v["error"]["code"].as_str()), (413, Some("too_large")), "{v}");
    // A bundle that fits only just: under the cap it is accepted.
    let mut noisy = Vec::new();
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for _ in 0..500_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        noisy.push((x & 0xff) as u8);
    }
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
    e.write_all(&noisy).unwrap();
    let big = e.finish().unwrap();
    assert!(big.len() > 500_000 && big.len() < 700_000);
    assert_eq!(w.post(&auth, &envelope(SIG_A, "3.8.1", &big)).await.0, 201);
    assert_eq!(w.st.db.crash_totals().unwrap().0, 1);
    // An oversize bundle inside an otherwise small envelope is refused too.
    let over = vec![0x1f; 800_000];
    let mut b = vec![0x1f, 0x8b, 0x08];
    b.extend(over);
    let (s, _, _) = w.post(&auth, &envelope(SIG_B, "3.8.1", &b)).await;
    assert_eq!(s, 413);
}

// ── Rate limits ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_sender_that_stalls_mid_body_is_cut_off_with_408() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let w = world(Limits { body_timeout: std::time::Duration::from_millis(300), ..limits() }).await;
    let (auth, _) = w.install();
    let addr = w.base.trim_start_matches("http://").to_string();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let head = format!(
        "POST /v1/crash-reports HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {auth}\r\nContent-Type: application/json\r\nContent-Length: 5000\r\n\r\n{{\"a\":"
    );
    s.write_all(head.as_bytes()).await.unwrap();
    // …and never sends the rest.
    // A read can end mid-reply: collect until the error code shows (or the time runs out).
    let mut reply = String::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut buf = vec![0u8; 4096];
        while !reply.contains("request_timeout") {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => reply.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    })
    .await;
    assert!(reply.starts_with("HTTP/1.1 408"), "{reply}");
    assert!(reply.contains("request_timeout"), "{reply}");
}

#[tokio::test]
async fn an_install_gets_five_an_hour_then_429_with_retry_after() {
    let w = world(Limits { per_ip_hour: 1000, ..Limits::default() }).await;
    let (auth, _) = w.install();
    for i in 0..5 {
        assert_eq!(w.post(&auth, &good(SIG_A)).await.0, 201, "report {i}");
    }
    let (s, h, v) = w.post(&auth, &good(SIG_A)).await;
    assert_eq!((s, v["error"]["code"].as_str()), (429, Some("rate_limited")));
    let wait: i64 = h["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=3600).contains(&wait), "{wait}");
    // Another install is unaffected.
    let (other, _) = w.install();
    assert_eq!(w.post(&other, &good(SIG_A)).await.0, 201);
    assert_eq!(w.st.db.crash_totals().unwrap().0, 6);
}

#[tokio::test]
async fn uploads_sent_at_the_same_moment_can_not_get_past_the_install_limit() {
    let w = Arc::new(world(Limits { per_ip_hour: 1000, ..Limits::default() }).await);
    let (auth, _) = w.install();
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let (w, auth) = (w.clone(), auth.clone());
        tasks.push(tokio::spawn(async move { w.post(&auth, &good(SIG_A)).await.0 }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == 201 {
            ok += 1;
        }
    }
    assert_eq!(ok, 5, "exactly the hourly allowance got in");
    assert_eq!(w.st.db.crash_totals().unwrap().0, 5);
}

#[tokio::test]
async fn a_blob_that_can_not_be_deleted_does_not_hang_the_storage_cap() {
    let bundle = gz("same size every time!");
    let size = bundle.len() as i64;
    let w = world(Limits { storage_cap: size + 1, ..limits() }).await;
    let (a1, _) = w.install();
    let (_, _, v) = w.post(&a1, &envelope(SIG_A, "3.8.1", &bundle)).await;
    let first = v["id"].as_str().unwrap().to_string();
    // The file's folder can't be written to: its delete fails (Unix only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = w.dir.path().join("blobs/crash-reports");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let (a2, _) = w.install();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            w.post(&a2, &envelope(SIG_B, "3.8.1", &gz("another one, same size"))),
        )
        .await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Never a hang, and never past the cap: it is refused (503), the old file stays.
        let (s, ..) = res.expect("the upload must not hang");
        assert_eq!(s, 503);
        assert!(w.blob_exists(&first));
        assert_eq!(w.st.db.crash_totals().unwrap().0, 1, "only the first report was kept");
    }
    #[cfg(not(unix))]
    let _ = first;
}

#[tokio::test]
async fn an_install_gets_twenty_a_day() {
    let w = world(Limits { per_install_hour: 1000, per_ip_hour: 1000, ..Limits::default() }).await;
    let (auth, _) = w.install();
    for i in 0..20 {
        assert_eq!(w.post(&auth, &good(SIG_A)).await.0, 201, "report {i}");
    }
    let (s, h, _) = w.post(&auth, &good(SIG_A)).await;
    assert_eq!(s, 429);
    assert!(h["retry-after"].to_str().unwrap().parse::<i64>().unwrap() > 0);
}

#[tokio::test]
async fn an_address_gets_thirty_an_hour_however_many_installs_it_mints() {
    let w = world(Limits { per_ip_hour: 3, ..Limits::default() }).await;
    for i in 0..3 {
        let (auth, _) = w.install();
        let (s, ..) = w.send(Some(&auth), Some("203.0.113.9"), serde_json::to_vec(&good(SIG_A)).unwrap()).await;
        assert_eq!(s, 201, "install {i}");
    }
    let (auth, _) = w.install();
    let (s, h, v) = w.send(Some(&auth), Some("203.0.113.9"), serde_json::to_vec(&good(SIG_A)).unwrap()).await;
    assert_eq!((s, v["error"]["code"].as_str()), (429, Some("rate_limited")));
    assert!(h.get("retry-after").is_some());
    // Another address is fine, and so is the default limit's size.
    let (s, ..) = w.send(Some(&auth), Some("203.0.113.10"), serde_json::to_vec(&good(SIG_A)).unwrap()).await;
    assert_eq!(s, 201);
    assert_eq!(Limits::default().per_ip_hour, 30);
}

#[tokio::test]
async fn the_whole_relay_takes_only_so_many_a_day() {
    let w = world(Limits { global_day: 3, ..limits() }).await;
    for _ in 0..3 {
        let (auth, _) = w.install();
        assert_eq!(w.post(&auth, &good(SIG_A)).await.0, 201);
    }
    let (auth, _) = w.install();
    let (s, h, _) = w.post(&auth, &good(SIG_A)).await;
    assert_eq!(s, 429);
    assert!(h.get("retry-after").is_some());
}

#[tokio::test]
async fn a_flood_of_one_signature_can_not_loosen_the_limits_by_trimming_its_rows() {
    // Rows are capped per signature; the counters live apart, so they keep counting.
    let w = world(Limits { max_rows_per_signature: 3, global_day: 4, ..limits() }).await;
    for _ in 0..4 {
        let (auth, _) = w.install();
        assert_eq!(w.post(&auth, &good(SIG_A)).await.0, 201);
    }
    let (auth, _) = w.install();
    assert_eq!(w.post(&auth, &good(SIG_A)).await.0, 429);
    let sig = w.st.db.crash_signature(SIG_A).unwrap().unwrap();
    assert_eq!(sig.count, 4, "the count keeps growing");
    assert_eq!(w.st.db.crash_reports_of(SIG_A, 50).unwrap().len(), 3, "but only 3 rows are kept");
}

// ── Dedupe, storage, retention ───────────────────────────────────────────────────────

#[tokio::test]
async fn repeats_count_under_one_signature_and_only_the_newest_three_bundles_stay() {
    let w = world(limits()).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        let (auth, _) = w.install();
        let ver = if i < 4 { "3.8.1" } else { "3.8.2" };
        let (s, _, v) = w.post(&auth, &envelope(SIG_A, ver, &gz(&format!("report {i}")))).await;
        assert_eq!(s, 201);
        ids.push(v["id"].as_str().unwrap().to_string());
        // Distinct receive times, so "newest" is clear.
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let sig = w.st.db.crash_signature(SIG_A).unwrap().unwrap();
    assert_eq!((sig.count, sig.installs), (5, 5));
    assert_eq!(w.st.db.crash_facet_values(SIG_A, "version").unwrap(), vec!["3.8.2", "3.8.1"]);
    // The two oldest lost their files but keep their rows.
    assert!(!w.blob_exists(&ids[0]) && !w.blob_exists(&ids[1]));
    assert!(w.blob_exists(&ids[2]) && w.blob_exists(&ids[3]) && w.blob_exists(&ids[4]));
    assert_eq!(w.st.db.crash_reports_of(SIG_A, 50).unwrap().len(), 5);
    let (s, _, b) = w.admin("GET", &format!("/reports/{}/text", ids[0])).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("bundle_removed")));
    let (s, _, b) = w.admin("GET", &format!("/reports/{}/text", ids[4])).await;
    assert_eq!((s, b.as_ref()), (200, b"report 4".as_slice()));
}

#[tokio::test]
async fn at_the_storage_cap_the_oldest_bundles_go_first() {
    let bundle = gz("same size every time!");
    let size = bundle.len() as i64;
    let w = world(Limits { storage_cap: size * 5 / 2, ..limits() }).await;
    let mut ids = Vec::new();
    for i in 0..4 {
        let (auth, _) = w.install();
        // Different signatures, so the per-signature rule is not what removes them.
        let sig = format!("{:032x}", i + 1);
        let (s, _, v) = w.post(&auth, &envelope(&sig, "3.8.1", &bundle)).await;
        assert_eq!(s, 201);
        ids.push(v["id"].as_str().unwrap().to_string());
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    assert!(w.st.db.crash_bytes().unwrap() <= size * 5 / 2);
    assert!(!w.blob_exists(&ids[0]) && !w.blob_exists(&ids[1]), "the oldest went");
    assert!(w.blob_exists(&ids[2]) && w.blob_exists(&ids[3]));
    // Their metadata is still listed.
    assert_eq!(w.get("").await["totals"]["signatures"], 4);
}

#[tokio::test]
async fn bundles_go_after_90_days_and_everything_after_a_year() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let (_, _, v) = w.post(&auth, &good(SIG_A)).await;
    let id = v["id"].as_str().unwrap().to_string();
    let now = now_ms();
    sweep_at(&w.st, now + 80 * DAY).await;
    assert!(w.blob_exists(&id), "kept at 80 days");
    sweep_at(&w.st, now + 91 * DAY).await;
    assert!(!w.blob_exists(&id), "bundle gone at 91 days");
    assert!(w.st.db.crash_report(&id).unwrap().is_some(), "metadata stays");
    assert_eq!(w.get(&format!("/{SIG_A}")).await["reports"].as_array().unwrap().len(), 1);
    sweep_at(&w.st, now + 366 * DAY).await;
    assert!(w.st.db.crash_report(&id).unwrap().is_none());
    assert!(w.st.db.crash_signature(SIG_A).unwrap().is_none());
    assert_eq!(w.st.db.crash_totals().unwrap(), (0, 0, 0));
}

#[cfg(unix)]
#[tokio::test]
async fn a_bundle_that_can_not_be_deleted_keeps_its_signature_for_the_next_try() {
    use std::os::unix::fs::PermissionsExt;
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let (_, _, v) = w.post(&auth, &good(SIG_A)).await;
    let id = v["id"].as_str().unwrap().to_string();
    let dir = w.dir.path().join("blobs/crash-reports");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Retention can't delete the file: the signature and its key stay.
    sweep_at(&w.st, now_ms() + 400 * DAY).await;
    assert!(w.st.db.crash_signature(SIG_A).unwrap().is_some());
    assert!(w.st.db.crash_report(&id).unwrap().unwrap().blob_key.is_some());
    // And the admin delete says so instead of pretending.
    let (s, _, b) = w.admin("DELETE", &format!("/{SIG_A}")).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("storage")));
    assert!(w.st.db.crash_signature(SIG_A).unwrap().is_some());
    // The disk recovers: the next sweep finishes the job.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    sweep_at(&w.st, now_ms() + 400 * DAY).await;
    assert!(w.st.db.crash_signature(SIG_A).unwrap().is_none());
    assert!(!w.blob_exists(&id));
}

#[tokio::test]
async fn signatures_can_not_be_minted_without_bound() {
    let w = world(Limits { max_signatures: 2, ..limits() }).await;
    let mut ids = Vec::new();
    for i in 1..=3 {
        let (auth, _) = w.install();
        let (_, _, v) = w.post(&auth, &good(&format!("{i:032x}"))).await;
        ids.push(v["id"].as_str().unwrap().to_string());
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    assert_eq!(w.st.db.crash_signature_count().unwrap(), 2);
    assert!(w.st.db.crash_signature(&format!("{:032x}", 1)).unwrap().is_none(), "the stalest went");
    assert!(!w.blob_exists(&ids[0]), "with its bundle");
}

#[tokio::test]
async fn without_a_file_store_an_upload_is_503_after_auth() {
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
    let st = AppState::new(cfg, db);
    let secret = "s3cret";
    st.db.add_install("inst1", &crate::hash(secret)).unwrap();
    let app = router(st).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/crash-reports"))
        .bearer_auth(format!("inst1.{secret}"))
        .json(&good(SIG_A))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "storage_unavailable");
}

// ── Admin ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_admin_routes_need_the_admin_token() {
    let w = world(limits()).await;
    for (m, p) in [("GET", ""), ("GET", "/abcdef0123456789"), ("DELETE", "/abcdef0123456789")] {
        let res = w
            .http
            .request(
                reqwest::Method::from_bytes(m.as_bytes()).unwrap(),
                format!("{}/admin/v1/crash-reports{p}", w.base),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 401, "{m} {p}");
        let res = w
            .http
            .request(
                reqwest::Method::from_bytes(m.as_bytes()).unwrap(),
                format!("{}/admin/v1/crash-reports{p}", w.base),
            )
            .bearer_auth("x".repeat(64))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 401, "{m} {p} wrong token");
    }
    // Through the public hostname (a proxy header) the admin API is refused outright.
    let res = w
        .http
        .get(format!("{}/admin/v1/crash-reports", w.base))
        .bearer_auth(token())
        .header("cf-connecting-ip", "203.0.113.1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 403);
}

#[tokio::test]
async fn the_list_and_detail_have_exactly_the_contract_shapes() {
    let w = world(limits()).await;
    let (a1, _) = w.install();
    let (a2, _) = w.install();
    w.post(&a1, &envelope(SIG_A, "3.8.1", &gz("one"))).await;
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    let mut second = envelope(SIG_A, "3.8.2", &gz("two"));
    second["os"] = json!("macos 15");
    let (_, _, last) = w.post(&a2, &second).await;
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    let mut other = envelope(SIG_B, "3.8.2", &gz("three"));
    other["kind"] = json!("hang");
    other["message"] = json!("stuck for 30 s");
    w.post(&a1, &other).await;

    let list = w.get("").await;
    assert_eq!(list["next"], Value::Null);
    assert_eq!(list["totals"], json!({ "reports": 3, "signatures": 2, "bytes": w.st.db.crash_bytes().unwrap() }));
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Newest last-seen first.
    assert_eq!(items[0]["signature"], SIG_B);
    let a = &items[1];
    let mut keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "count",
            "first_seen_ms",
            "installs",
            "kind",
            "last_seen_ms",
            "latest_report_id",
            "os",
            "signature",
            "title",
            "versions"
        ]
    );
    assert_eq!((a["count"].as_i64(), a["installs"].as_i64()), (Some(2), Some(2)));
    assert_eq!(a["kind"], "crash");
    assert_eq!(a["title"], "panicked at src/lib.rs:1");
    assert_eq!(a["versions"], json!(["3.8.2", "3.8.1"]));
    assert_eq!(a["os"], json!(["macos 15", "windows 11"]));
    assert_eq!(a["latest_report_id"], last["id"]);
    assert!(a["first_seen_ms"].as_i64().unwrap() <= a["last_seen_ms"].as_i64().unwrap());

    let d = w.get(&format!("/{SIG_A}")).await;
    let mut keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["count", "first_seen_ms", "installs", "kind", "last_seen_ms", "reports", "signature", "title"]);
    let reports = d["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["id"], last["id"], "newest first");
    let mut rk: Vec<&str> = reports[0].as_object().unwrap().keys().map(String::as_str).collect();
    rk.sort_unstable();
    assert_eq!(
        rk,
        ["arch", "cause_codes", "id", "install_hash", "kind", "message", "os", "received_ms", "size_bytes", "version"]
    );
    assert_eq!(reports[0]["cause_codes"], json!(["gpu_lost", "low_memory"]));
    assert_eq!(reports[0]["size_bytes"], gz("two").len());
    assert_eq!(reports[0]["message"], "panicked at src/lib.rs:1\nsecond line");
    let hash = reports[0]["install_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    assert_ne!(reports[0]["install_hash"], reports[1]["install_hash"]);
}

#[tokio::test]
async fn the_list_filters_and_pages_with_a_cursor() {
    let w = world(limits()).await;
    for i in 1..=5 {
        let (auth, _) = w.install();
        let mut e = envelope(&format!("{i:032x}"), if i % 2 == 0 { "3.8.1" } else { "3.8.2" }, &gz("x"));
        if i == 5 {
            e["kind"] = json!("error");
            e["message"] = json!("Needle in a haystack");
        }
        w.post(&auth, &e).await;
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let sigs = |v: &Value| -> Vec<String> {
        v["items"].as_array().unwrap().iter().map(|i| i["signature"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(sigs(&w.get("?kind=error").await), [format!("{:032x}", 5)]);
    assert_eq!(sigs(&w.get("?version=3.8.1").await).len(), 2);
    assert_eq!(sigs(&w.get("?q=needle").await), [format!("{:032x}", 5)]);
    assert_eq!(sigs(&w.get(&format!("?q={:032x}", 3)).await), [format!("{:032x}", 3)]);
    let p1 = w.get("?limit=2").await;
    assert_eq!(sigs(&p1), [format!("{:032x}", 5), format!("{:032x}", 4)]);
    let cur = p1["next"].as_str().unwrap().to_string();
    let p2 = w.get(&format!("?limit=2&before={cur}")).await;
    assert_eq!(sigs(&p2), [format!("{:032x}", 3), format!("{:032x}", 2)]);
    let p3 = w.get(&format!("?limit=2&before={}", p2["next"].as_str().unwrap())).await;
    assert_eq!((sigs(&p3), p3["next"].clone()), ([format!("{:032x}", 1)].to_vec(), Value::Null));
    for bad in ["?kind=nope", "?limit=0", "?limit=201", "?before=garbage", "?before=1:../x"] {
        let (s, _, b) = w.admin("GET", bad).await;
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["error"]["code"].as_str()), (400, Some("bad_query")), "{bad}");
    }
}

#[tokio::test]
async fn the_text_view_is_plain_text_with_safe_headers_and_a_download_option() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let text = "=== SUMMARY ===\n<script>alert(1)</script>\n";
    let (_, _, v) = w.post(&auth, &envelope(SIG_A, "3.8.1", &gz(text))).await;
    let id = v["id"].as_str().unwrap();
    let (s, h, b) = w.admin("GET", &format!("/reports/{id}/text")).await;
    assert_eq!(s, 200);
    assert_eq!(h["content-type"], "text/plain; charset=utf-8");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(h.get("content-disposition").is_none());
    assert_eq!(b.as_ref(), text.as_bytes());
    let (_, h, _) = w.admin("GET", &format!("/reports/{id}/text?download=1")).await;
    assert_eq!(h["content-disposition"].to_str().unwrap(), format!("attachment; filename=\"{id}.txt\""));
    // The raw bundle is an octet-stream attachment.
    let (s, h, b) = w.admin("GET", &format!("/reports/{id}/bundle")).await;
    assert_eq!((s, h["content-type"].to_str().unwrap()), (200, "application/octet-stream"));
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment;"));
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(b.as_ref(), gz(text).as_slice());
    // Unknown and malformed ids are 404 not_found, never a path.
    for p in [
        "/reports/cr_00000000000000000000000000000000/text".to_string(),
        "/reports/..%2f..%2fetc/text".to_string(),
        "/reports/nope/text".to_string(),
        "/abcdef0123456789".to_string(),
        "/not-a-signature".to_string(),
    ] {
        let (s, _, b) = w.admin("GET", &p).await;
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")), "{p}");
    }
}

#[tokio::test]
async fn the_text_view_stops_at_2_mb_even_for_a_decompression_bomb() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    // 100 MB of zeros gzips to about 100 KB: small enough to upload.
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![0u8; 1024 * 1024];
    for _ in 0..100 {
        e.write_all(&zeros).unwrap();
    }
    let bomb = e.finish().unwrap();
    assert!(bomb.len() < 200_000, "{}", bomb.len());
    let (s, _, v) = w.post(&auth, &envelope(SIG_A, "3.8.1", &bomb)).await;
    assert_eq!(s, 201, "an upload is never unpacked: {v}");
    let id = v["id"].as_str().unwrap();
    let (s, _, b) = w.admin("GET", &format!("/reports/{id}/text")).await;
    assert_eq!(s, 200);
    assert!(b.len() < TEXT_CAP + 200, "{}", b.len());
    assert!(b.len() >= TEXT_CAP, "{}", b.len());
    assert!(String::from_utf8_lossy(&b).ends_with("stops at 2 MB ...]\n"));
}

#[tokio::test]
async fn a_damaged_bundle_is_422_or_partial_never_a_500() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    // The magic is right, the rest is junk.
    let mut junk = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 3];
    junk.extend((0..200u32).map(|i| (i * 7 % 251) as u8));
    let (s, _, v) = w.post(&auth, &envelope(SIG_A, "3.8.1", &junk)).await;
    assert_eq!(s, 201);
    let id = v["id"].as_str().unwrap();
    let (s, _, b) = w.admin("GET", &format!("/reports/{id}/text")).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("bundle_unreadable")));
    // One cut short keeps what could be read.
    let whole = gz(&"line of text\n".repeat(2000));
    let cut = whole[..whole.len() / 2].to_vec();
    let (_, _, v) = w.post(&auth, &envelope(SIG_B, "3.8.1", &cut)).await;
    let (s, _, b) = w.admin("GET", &format!("/reports/{}/text", v["id"].as_str().unwrap())).await;
    assert_eq!(s, 200);
    assert!(String::from_utf8_lossy(&b).starts_with("line of text"));
}

#[tokio::test]
async fn delete_removes_the_signature_its_rows_and_its_files() {
    let w = world(limits()).await;
    let (auth, _) = w.install();
    let (_, _, v) = w.post(&auth, &good(SIG_A)).await;
    let id = v["id"].as_str().unwrap().to_string();
    w.post(&auth, &good(SIG_B)).await;
    let (s, h, b) = w.admin("DELETE", &format!("/{SIG_A}")).await;
    assert_eq!((s, b.len()), (204, 0));
    assert_eq!(h["cache-control"], "no-store");
    assert!(!w.blob_exists(&id));
    assert!(w.st.db.crash_report(&id).unwrap().is_none());
    assert_eq!(w.get("").await["totals"]["signatures"], 1);
    let (s, _, b) = w.admin("DELETE", &format!("/{SIG_A}")).await;
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
    let (s, ..) = w.admin("DELETE", "/..%2f..").await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn the_table_names_and_the_migration_are_additive() {
    let w = world(limits()).await;
    let c = w.st.db.c();
    for t in ["crash_key", "crash_signatures", "crash_facets", "crash_reports", "crash_rate"] {
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [t], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "{t}");
    }
    assert!(crate::migrate::MIGRATIONS.iter().any(|m| m.version == 18 && m.name.contains("crash")));
}

#[test]
fn a_bomb_is_cut_at_the_cap_in_the_pure_function() {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(&vec![b'a'; 5_000_000]).unwrap();
    let data = e.finish().unwrap();
    let (out, cut) = gunzip_capped(&data, 1000).unwrap();
    assert_eq!((out.len(), cut), (1000, true));
    let (out, cut) = gunzip_capped(&gz("short"), 1000).unwrap();
    assert_eq!((out.as_slice(), cut), (b"short".as_slice(), false));
    assert!(gunzip_capped(b"definitely not gzip", 10).is_err());
}
