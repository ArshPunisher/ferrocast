//! Crash reports, the app's side: what goes in (and what never does), the signature, the
//! pending/send/delete steps against a fake relay, and the three settings.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use base64::Engine as _;
use ferrocast_secrets::{MemorySecrets, Secrets};
use serde_json::{Value, json};

use crate::crash_report::*;
use crate::{Engine, PlatformApps};

// ── Signature ────────────────────────────────────────────────────────────────────────

#[test]
fn numbers_paths_and_addresses_do_not_split_one_bug_into_many() {
    let a = find_panic("x panicked at /Users/a/proj/crates/engine/src/studio.rs:120:9:\nindex 77 out of range for 5\n");
    let b = find_panic(
        "x panicked at C:\\work\\ferrocast\\crates\\engine\\src\\studio.rs:131:17:\nindex 9 out of range for 12\n",
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let sig = |p: &Panic| signature_of("crash", &file_of(&p.location), &normalise(&p.message));
    assert_eq!(sig(&a), sig(&b));
    assert_eq!(sig(&a).len(), 32);
    assert!(sig(&a).bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    // A different message, file or kind is a different bug.
    let other_msg =
        find_panic("panicked at src/studio.rs:1:1:\ncalled `Option::unwrap()` on a `None` value\n").unwrap();
    assert_ne!(sig(&a), sig(&other_msg));
    let other_file = find_panic("panicked at src/mixer.rs:1:1:\nindex 1 out of range for 2\n").unwrap();
    assert_ne!(sig(&a), sig(&other_file));
    assert_ne!(
        signature_of("crash", "studio.rs", "m"),
        signature_of("unclean_exit", "studio.rs", "m"),
        "the kind is part of it"
    );
    // Addresses and paths inside a message are folded away too.
    assert_eq!(normalise("bad pointer 0x7ff6a1b2c3d4 in /home/x/y.rs"), "bad pointer 0x# in <path>");
    assert_eq!(normalise("a   b\n c"), "a b c");
    assert!(normalise(&"word ".repeat(100)).chars().count() <= 160);
}

#[test]
fn the_older_panic_format_is_read_too() {
    let p = find_panic("thread 'main' panicked at 'boom 5', src/a.rs:10:5\n").unwrap();
    assert_eq!((p.location.as_str(), p.message.as_str()), ("src/a.rs:10:5", "boom 5"));
    assert!(find_panic("nothing here").is_none());
}

#[test]
fn only_the_session_that_ended_badly_is_looked_at() {
    let logs = "old line\nT INFO Ferrocast starting v=1\nT ERROR boom panicked at a.rs:1:1:\nfirst crash\n\
                T INFO Ferrocast starting v=2\nT INFO fine now\n";
    let s = previous_session(logs);
    assert!(s.contains("first crash") && !s.contains("fine now") && !s.contains("old line"), "{s}");
    // Only the current start in the log: everything before it.
    let one = "before\nT INFO Ferrocast starting v=2\nafter\n";
    assert_eq!(previous_session(one), "before\n");
    assert_eq!(previous_session("no markers"), "no markers");
}

// ── Fake relay ───────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Relay {
    status: u16,
    bodies: Vec<Value>,
    auths: Vec<String>,
}

async fn relay(status: u16) -> (String, Arc<Mutex<Relay>>) {
    let st = Arc::new(Mutex::new(Relay { status, ..Default::default() }));
    let app = axum::Router::new()
        .route("/v1/installs", post(|| async { Json(json!({ "install_id": "i1", "install_secret": "s1" })) }))
        .route(
            "/v1/crash-reports",
            post(|State(s): State<Arc<Mutex<Relay>>>, h: HeaderMap, body: axum::body::Bytes| async move {
                let mut r = s.lock().unwrap();
                r.auths.push(h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                r.bodies.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let code = StatusCode::from_u16(r.status).unwrap();
                (code, Json(json!({ "error": { "code": "x", "message": "x", "hint": null } })))
            }),
        )
        .with_state(st.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, st)
}

const SECRET_KEY: &str = "live_fixture_streamkey_0099";
const SECRET_TOKEN: &str = "tok_secret_fixture_alpha";
const DUMP_MARKER: &str = "UNIQUE_DUMP_MARKER_5f3a9c";

/// An engine whose data folder has a log with a panic in the session that ended, a text crash
/// note, and a binary dump, all with fixture secrets in them.
async fn crashed(relay_url: Option<String>, mode: &str) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    plant(dir.path());
    let secrets = Arc::new(MemorySecrets::default());
    secrets.set("twitch-tokens", &format!(r#"{{"access_token":"{SECRET_TOKEN}"}}"#)).unwrap();
    secrets.set(crate::golive::INGEST_KEY_SECRET, SECRET_KEY).unwrap();
    let e = start_in(dir.path(), secrets, relay_url).await;
    set_mode(&e, mode).await;
    e.set_safe_mode_for_test(true, false);
    (e, dir)
}

async fn start_in(dir: &std::path::Path, secrets: Arc<MemorySecrets>, relay_url: Option<String>) -> Engine {
    let apps = PlatformApps { relay_url, ..Default::default() };
    Engine::start_with(dir, secrets, apps).await.unwrap()
}

async fn set_mode(e: &Engine, mode: &str) {
    e.patch_settings(json!({ "crash_reports": mode }).as_object().unwrap()).await.unwrap();
}

fn plant(dir: &std::path::Path) {
    let logs = dir.join("logs");
    let crashes = dir.join("crashes");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::create_dir_all(&crashes).unwrap();
    let log = format!(
        "2026-11-27T10:00:00Z INFO Ferrocast starting version=\"3.8.0\"\n\
         2026-11-27T10:00:01Z WARN connecting rtmp://live.twitch.tv/app/{SECRET_KEY}\n\
         2026-11-27T10:00:02Z ERROR user=secret.email@fixture.example ip=198.51.100.42 token={SECRET_TOKEN}\n\
         2026-11-27T10:05:00Z ERROR ferrocast: Ferrocast stopped unexpectedly info=panicked at /Users/secretHomeUser99/ferrocast/crates/engine/src/studio.rs:120:9:\n\
         index 77 out of range for 5 path=C:\\Users\\WinFixtureUser\\Videos\\out.mkv\n\
         stack backtrace:\n   0: ferrocast::studio::run\n   1: std::rt::lang_start\n\
         2026-11-28T09:00:00Z INFO Ferrocast starting version=\"3.8.0\"\n\
         2026-11-28T09:00:01Z INFO a fine line from this session\n"
    );
    std::fs::write(logs.join("ferrocast.log"), log).unwrap();
    std::fs::write(
        crashes.join("panic-note.txt"),
        "panic note for owner@crash.example at /home/linuxCrashUser/ferrocast token=tok_secret_fixture_alpha",
    )
    .unwrap();
    let mut dmp = b"MDMP\x93\xa7\x00\x00".to_vec();
    dmp.extend_from_slice(format!("{DUMP_MARKER} stream_key={SECRET_KEY} ").as_bytes());
    dmp.extend_from_slice(&[0, 0, 1, 2, 3]);
    std::fs::write(crashes.join("crash-1.dmp"), dmp).unwrap();
}

fn pending_text(v: &Value) -> String {
    v["preview_text"].as_str().unwrap().to_string()
}

// ── What goes in ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_report_has_the_sections_and_no_dump_and_no_secret() {
    let (e, _d) = crashed(None, "ask").await;
    e.crash_report_startup().await;
    let v = e.pending_crash_report().await;
    assert_eq!(v["pending"], true);
    assert_eq!(v["kind"], "crash");
    assert!(v["created_ms"].as_i64().unwrap() > 0);
    let text = pending_text(&v);
    for section in ["=== SENT ===", "=== SUMMARY ===", "=== report.json ===", "=== PANIC AND TRACE", "=== LOGS (last"] {
        assert!(text.contains(section), "missing {section}");
    }
    assert!(text.contains("CRASH NOTE panic-note.txt"), "the text crash note is in");
    assert!(text.contains("stack backtrace") && text.contains("index 77 out of range"), "the trace is in");
    // Never the binary dump, in any form.
    assert!(!text.contains(DUMP_MARKER), "dump bytes leaked");
    assert!(!text.contains("MDMP"), "dump header leaked");
    assert!(!text.contains('\0'));
    // No fixture secret, email, address or user name anywhere.
    for leak in [
        SECRET_KEY,
        SECRET_TOKEN,
        "secret.email@fixture.example",
        "owner@crash.example",
        "198.51.100.42",
        "secretHomeUser99",
        "WinFixtureUser",
        "linuxCrashUser",
    ] {
        assert!(!text.contains(leak), "leaked {leak}");
    }
    // The previous session's panic, not the current session's lines, makes the message.
    assert!(text.contains("message: panicked at "), "{}", &text[..text.len().min(600)]);
    assert!(text.contains("signature: "));
}

#[tokio::test]
async fn the_signature_is_the_same_after_every_crash_of_the_same_bug() {
    let (e, d) = crashed(None, "ask").await;
    e.crash_report_startup().await;
    let first = e.pending_crash_report().await;
    let sig = |t: &str| t.lines().find_map(|l| l.strip_prefix("signature: ")).unwrap().to_string();
    let s1 = sig(&pending_text(&first));
    // The same panic at another line, another folder, another index.
    let log = std::fs::read_to_string(d.path().join("logs/ferrocast.log"))
        .unwrap()
        .replace("studio.rs:120:9", "studio.rs:187:13")
        .replace("/Users/secretHomeUser99", "/Users/someoneElse")
        .replace("index 77 out of range for 5", "index 4 out of range for 2");
    std::fs::write(d.path().join("logs/ferrocast.log"), log).unwrap();
    e.discard_pending_crash_report().await;
    // Another start of the engine on the same folder.
    let e2 = start_in(d.path(), Arc::new(MemorySecrets::default()), None).await;
    set_mode(&e2, "ask").await;
    e2.set_safe_mode_for_test(true, false);
    e2.crash_report_startup().await;
    let s2 = sig(&pending_text(&e2.pending_crash_report().await));
    assert_eq!(s1, s2);
}

#[tokio::test]
async fn a_kill_without_a_panic_is_an_unclean_exit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    std::fs::write(
        dir.path().join("logs/ferrocast.log"),
        "T INFO Ferrocast starting v=1\nT INFO working\nT INFO Ferrocast starting v=1\n",
    )
    .unwrap();
    let e = start_in(dir.path(), Arc::new(MemorySecrets::default()), None).await;
    e.set_safe_mode_for_test(true, false);
    e.crash_report_startup().await;
    let v = e.pending_crash_report().await;
    assert_eq!(v["kind"], "unclean_exit");
    assert!(pending_text(&v).contains("no panic was recorded"));
}

#[tokio::test]
async fn a_clean_start_makes_no_report() {
    let dir = tempfile::tempdir().unwrap();
    let e = start_in(dir.path(), Arc::new(MemorySecrets::default()), None).await;
    e.set_safe_mode_for_test(false, false);
    e.crash_report_startup().await;
    assert_eq!(e.pending_crash_report().await["pending"], false);
}

#[tokio::test]
async fn a_huge_log_still_makes_an_upload_under_the_relay_cap() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    // Lines the compressor can't shrink much.
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut log = String::from("T INFO Ferrocast starting v=1\n");
    for _ in 0..2500 {
        let mut line = String::from("T INFO noise ");
        for _ in 0..40 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            line.push((b'a' + (x % 26) as u8) as char);
            line.push(char::from(b'0' + ((x >> 8) % 10) as u8));
        }
        log.push_str(&line);
        log.push('\n');
    }
    log.push_str("T INFO Ferrocast starting v=1\n");
    std::fs::write(dir.path().join("logs/ferrocast.log"), log).unwrap();
    let e = start_in(dir.path(), Arc::new(MemorySecrets::default()), None).await;
    e.set_safe_mode_for_test(true, false);
    e.crash_report_startup().await;
    let v = e.pending_crash_report().await;
    let body = serde_json::to_vec(&upload_body_for_test(&pending_text(&v), &v)).unwrap();
    assert!(body.len() <= 950_000, "{}", body.len());
}

fn upload_body_for_test(text: &str, v: &Value) -> Value {
    // Rebuilds the body the way the sender does, from the preview.
    let p = Pending {
        created_ms: v["created_ms"].as_i64().unwrap(),
        kind: v["kind"].as_str().unwrap().into(),
        signature: "0".repeat(32),
        version: "3.8.0".into(),
        os: "x".into(),
        arch: "y".into(),
        cause_codes: vec![],
        message: "m".into(),
        text: text.into(),
        auto_attempts: 0,
        refused: None,
    };
    upload_body(&p)
}

// ── The API steps against a fake relay ───────────────────────────────────────────────

fn gunzip(b64: &str) -> String {
    use std::io::Read;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    let mut s = String::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_string(&mut s).unwrap();
    s
}

#[tokio::test]
async fn send_uploads_exactly_the_preview_with_the_install_auth_and_removes_it() {
    let (url, rel) = relay(201).await;
    let (e, d) = crashed(Some(url), "ask").await;
    e.crash_report_startup().await;
    assert!(rel.lock().unwrap().bodies.is_empty(), "ask: nothing is sent without a yes");
    let preview = pending_text(&e.pending_crash_report().await);
    assert_eq!(e.send_pending_crash_report().await.unwrap(), json!({ "sent": true }));
    let (b, auths, sent) = {
        let r = rel.lock().unwrap();
        (r.bodies.first().cloned().unwrap_or(Value::Null), r.auths.clone(), r.bodies.len())
    };
    assert_eq!(sent, 1);
    assert_eq!(auths, ["Bearer i1.s1"]);
    let b = &b;
    assert_eq!(gunzip(b["bundle"].as_str().unwrap()), preview, "what was shown is what was sent");
    let mut keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["arch", "bundle", "cause_codes", "kind", "message", "os", "signature", "version"]);
    assert_eq!(b["kind"], "crash");
    // The envelope fields are the ones the preview header shows.
    for (k, line) in [("signature", "signature: "), ("version", "version: "), ("arch", "arch: ")] {
        assert!(preview.contains(&format!("{line}{}", b[k].as_str().unwrap())), "{k}");
    }
    assert!(preview.contains(&format!("message: {}", b["message"].as_str().unwrap())));
    // It is gone from this PC, and a second send has nothing to send.
    assert_eq!(e.pending_crash_report().await["pending"], false);
    assert!(!d.path().join("crash-report/pending.json").exists());
    assert!(matches!(e.send_pending_crash_report().await, Err(crate::EngineError::Missing { .. })));
}

#[tokio::test]
async fn an_older_relay_or_a_limit_keeps_the_report_here() {
    for (status, reason) in
        [(404u16, "relay_too_old"), (429, "rate_limited"), (500, "relay_unavailable"), (503, "relay_unavailable")]
    {
        let (url, _rel) = relay(status).await;
        let (e, d) = crashed(Some(url), "ask").await;
        e.crash_report_startup().await;
        assert_eq!(
            e.send_pending_crash_report().await.unwrap(),
            json!({ "sent": false, "reason": reason }),
            "{status}"
        );
        assert_eq!(e.pending_crash_report().await["pending"], true, "{status}: kept");
        assert!(d.path().join("crash-report/pending.json").exists());
    }
    // No relay in this build, and a relay nobody answers from.
    let (e, _d) = crashed(None, "ask").await;
    e.crash_report_startup().await;
    assert_eq!(e.send_pending_crash_report().await.unwrap()["reason"], "relay_unavailable");
    let (e, _d) = crashed(Some("http://127.0.0.1:1".into()), "ask").await;
    e.crash_report_startup().await;
    assert_eq!(e.send_pending_crash_report().await.unwrap()["reason"], "relay_unavailable");
}

#[tokio::test]
async fn a_relay_that_refuses_for_good_is_reported_as_such() {
    for (status, reason) in [(413u16, "too_large"), (400, "rejected"), (401, "rejected")] {
        let (url, _rel) = relay(status).await;
        let (e, _d) = crashed(Some(url), "ask").await;
        e.crash_report_startup().await;
        assert_eq!(e.send_pending_crash_report().await.unwrap()["reason"], reason, "{status}");
        assert_eq!(e.pending_crash_report().await["pending"], true);
    }
}

#[tokio::test]
async fn delete_means_dont_send_and_it_stays_deleted() {
    let (url, rel) = relay(201).await;
    let (e, d) = crashed(Some(url), "ask").await;
    e.crash_report_startup().await;
    assert!(e.discard_pending_crash_report().await);
    assert_eq!(e.pending_crash_report().await["pending"], false);
    // Looking again never builds it again.
    assert_eq!(e.pending_crash_report().await["pending"], false);
    assert!(!d.path().join("crash-report/pending.json").exists());
    assert!(!e.discard_pending_crash_report().await, "nothing left to delete");
    assert!(rel.lock().unwrap().bodies.is_empty());
}

// ── The three settings ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn never_builds_nothing_sends_nothing_and_drops_an_old_report() {
    let (url, rel) = relay(201).await;
    let url2 = url.clone();
    let (e, d) = crashed(Some(url), "ask").await;
    e.crash_report_startup().await;
    assert_eq!(e.pending_crash_report().await["pending"], true);
    // The person changes their mind, and the next start finds a crash again.
    set_mode(&e, "never").await;
    let e2 = start_in(d.path(), Arc::new(MemorySecrets::default()), Some(url2)).await;
    e2.set_safe_mode_for_test(true, false);
    e2.crash_report_startup().await;
    assert_eq!(e2.pending_crash_report().await["pending"], false, "dropped");
    assert!(!d.path().join("crash-report").join("pending.json").exists());
    assert!(rel.lock().unwrap().bodies.is_empty());
}

#[tokio::test]
async fn always_sends_once_on_its_own_and_not_again() {
    let (url, rel) = relay(201).await;
    let dir = tempfile::tempdir().unwrap();
    plant(dir.path());
    let secrets = Arc::new(MemorySecrets::default());
    let e = start_in(dir.path(), secrets.clone(), Some(url.clone())).await;
    set_mode(&e, "always").await;
    e.set_safe_mode_for_test(true, false);
    e.crash_report_startup().await;
    assert_eq!(rel.lock().unwrap().bodies.len(), 1, "sent without a prompt");
    assert_eq!(e.pending_crash_report().await["pending"], false);
    // The same run asking again does nothing (once per run).
    e.crash_report_startup().await;
    assert_eq!(rel.lock().unwrap().bodies.len(), 1);
    // The next start, with a clean exit: nothing to send.
    let e2 = start_in(dir.path(), secrets, Some(url)).await;
    e2.set_safe_mode_for_test(false, false);
    e2.crash_report_startup().await;
    assert_eq!(rel.lock().unwrap().bodies.len(), 1);
}

#[tokio::test]
async fn always_tries_again_next_start_after_a_limit_but_never_more_than_three_times() {
    let (url, rel) = relay(429).await;
    let dir = tempfile::tempdir().unwrap();
    plant(dir.path());
    let secrets = Arc::new(MemorySecrets::default());
    for start in 0..5 {
        let e = start_in(dir.path(), secrets.clone(), Some(url.clone())).await;
        set_mode(&e, "always").await;
        // The crash is only found the first time; later starts are clean.
        e.set_safe_mode_for_test(start == 0, false);
        e.crash_report_startup().await;
    }
    assert_eq!(rel.lock().unwrap().bodies.len(), 3, "three tries, then it waits for a yes");
    let e = start_in(dir.path(), secrets, Some(url)).await;
    assert_eq!(e.pending_crash_report().await["pending"], true, "still here for a manual send");
}

#[tokio::test]
async fn always_does_not_retry_a_report_the_relay_refused_for_good() {
    let (url, rel) = relay(413).await;
    let dir = tempfile::tempdir().unwrap();
    plant(dir.path());
    let secrets = Arc::new(MemorySecrets::default());
    for start in 0..3 {
        let e = start_in(dir.path(), secrets.clone(), Some(url.clone())).await;
        set_mode(&e, "always").await;
        e.set_safe_mode_for_test(start == 0, false);
        e.crash_report_startup().await;
    }
    assert_eq!(rel.lock().unwrap().bodies.len(), 1);
}

#[tokio::test]
async fn ask_is_the_default_and_the_setting_only_takes_the_three_values() {
    let dir = tempfile::tempdir().unwrap();
    let e = start_in(dir.path(), Arc::new(MemorySecrets::default()), None).await;
    assert_eq!(e.settings().crash_reports, "ask");
    for good in ["always", "never", "ask"] {
        set_mode(&e, good).await;
        assert_eq!(e.settings().crash_reports, good);
    }
    let bad = e.patch_settings(json!({ "crash_reports": "sometimes" }).as_object().unwrap()).await.unwrap_err();
    assert!(bad.to_string().contains("crash_reports: use one of ask, always, never."), "{bad}");
    let bad = e.patch_settings(json!({ "crash_reports": 3 }).as_object().unwrap()).await.unwrap_err();
    assert!(bad.to_string().contains("crash_reports: must be text."), "{bad}");
    assert_eq!(e.settings().crash_reports, "ask", "a refused value changes nothing");
}

#[test]
fn an_old_settings_file_without_the_field_reads_as_ask() {
    let old = json!({ "keep_chat_days": 30, "theme": "dark", "lan_access": false });
    let s: crate::state::EngineSettings = serde_json::from_value(old).unwrap();
    assert_eq!(s.crash_reports, "ask");
    assert_eq!(s.theme, "dark");
}

// ── Against the real relay ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_real_relay_takes_what_the_app_sends_and_shows_it_to_the_admin() {
    let rdir = tempfile::tempdir().unwrap();
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rl.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let token = ["0123456789abcdef"; 4].concat();
    let store: Arc<dyn ferrocast_relay::blob::BlobStore> =
        Arc::new(ferrocast_relay::blob::FsBlobStore::new(rdir.path().join("blobs")).unwrap());
    let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
        .with_blobs(Some(store))
        .with_admin(ferrocast_relay::admin::AdminConfig::new(Some(&token), None));
    let app = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(rl, app).await });

    let (e, _d) = crashed(Some(base.clone()), "ask").await;
    e.crash_report_startup().await;
    let preview = pending_text(&e.pending_crash_report().await);
    assert_eq!(e.send_pending_crash_report().await.unwrap(), json!({ "sent": true }));

    let http = reqwest::Client::new();
    let list: Value = http
        .get(format!("{base}/admin/v1/crash-reports"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let item = &list["items"][0];
    assert_eq!(item["kind"], "crash");
    assert_eq!(item["count"], 1);
    assert!(item["title"].as_str().unwrap().starts_with("panicked at "), "{item}");
    let id = item["latest_report_id"].as_str().unwrap();
    let text = http
        .get(format!("{base}/admin/v1/crash-reports/reports/{id}/text"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(text, preview, "the admin reads exactly what the person was shown");
}
