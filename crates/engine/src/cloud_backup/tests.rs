//! Cloud backups: the file format (pure), then the whole path against the real relay
//! (`ferrocast_relay::router` with a file store): encrypt → upload → download → decrypt →
//! import, keys and passphrases that don't fit, the automatic upload, and an upload that
//! carries on after a restart.

use std::sync::Arc;

use ferrocast_relay::billing::store::Synced;
use ferrocast_secrets::Secrets;
use ferrocast_store::repo;
use serde_json::{Value, json};

use super::*;
use crate::{Engine, PlatformApps};

// ── The file format ──────────────────────────────────────────────────────────────────

fn tmp_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn sealed_files_open_with_the_key_and_say_why_when_they_dont() {
    let d = tempfile::tempdir().unwrap();
    let pack = tmp_file(d.path(), "p", b"the whole setup");
    let key = age::x25519::Identity::generate();
    let sealed = d.path().join("s");
    let (bytes, sha) = seal(&pack, &sealed, &key.to_public(), None).unwrap();
    assert_eq!(file_sha(&sealed).unwrap(), (bytes, sha));
    let raw = std::fs::read(&sealed).unwrap();
    assert!(raw.starts_with(b"ferrocast-backup v1\n\n"), "header, then the age file");
    assert!(!raw.windows(5).any(|w| w == b"whole"), "no plaintext in it");
    let out = d.path().join("o");
    let opened = open(&sealed, &out, std::slice::from_ref(&key), None).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"the whole setup");
    assert_eq!(opened.identity.to_public(), key.to_public());
    // Another key, no key, or a passphrase it was never given.
    let other = age::x25519::Identity::generate();
    assert_eq!(open(&sealed, &out, &[other], None).unwrap_err(), OpenErr::WrongKey);
    assert_eq!(open(&sealed, &out, &[], None).unwrap_err(), OpenErr::KeyNeeded);
    assert_eq!(open(&sealed, &out, &[], Some("a long passphrase")).unwrap_err(), OpenErr::NoPassphrase);
    // Not a backup, a changed byte, a cut-off file.
    let junk = tmp_file(d.path(), "j", b"PK\x03\x04 a zip, not a backup");
    assert_eq!(open(&junk, &out, std::slice::from_ref(&key), None).unwrap_err(), OpenErr::Damaged);
    let mut flipped = raw.clone();
    let last = flipped.len() - 3;
    flipped[last] ^= 1;
    let flipped = tmp_file(d.path(), "f", &flipped);
    assert_eq!(open(&flipped, &out, std::slice::from_ref(&key), None).unwrap_err(), OpenErr::Damaged);
    let cut = tmp_file(d.path(), "c", &raw[..raw.len() - 10]);
    assert_eq!(open(&cut, &out, &[key], None).unwrap_err(), OpenErr::Damaged);
}

#[test]
fn a_passphrase_opens_the_key_inside_the_backup() {
    let d = tempfile::tempdir().unwrap();
    let pack = tmp_file(d.path(), "p", b"setup");
    let key = age::x25519::Identity::generate();
    let wrapped = wrap_key(&key, "correct horse battery", true).unwrap();
    let sealed = d.path().join("s");
    seal(&pack, &sealed, &key.to_public(), Some(&wrapped)).unwrap();
    let out = d.path().join("o");
    let opened = open(&sealed, &out, &[], Some("correct horse battery")).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"setup");
    assert_eq!(opened.identity.to_public(), key.to_public(), "the key inside came out");
    assert_eq!(opened.wrapped.as_deref(), Some(wrapped.as_str()));
    assert_eq!(open(&sealed, &out, &[], Some("wrong horse battery")).unwrap_err(), OpenErr::WrongPassphrase);
    // The key still opens it on its own.
    assert!(open(&sealed, &out, &[key], None).is_ok());
}

#[test]
fn the_key_file_reads_back_and_pasted_keys_are_found() {
    let key = age::x25519::Identity::generate();
    let text = key_file(&key, 1_792_411_200_000);
    assert!(text.contains("2026-10-19"));
    assert!(text.contains("nobody can open them"));
    assert_eq!(parse_key(&text).unwrap().to_public(), key.to_public());
    let line = key.to_string().expose_secret().to_string();
    assert_eq!(parse_key(&format!("  {}  ", line.to_lowercase())).unwrap().to_public(), key.to_public());
    assert!(parse_key("hello").is_none());
    assert!(parse_key("AGE-SECRET-KEY-1NOTAKEY").is_none());
}

// ── Against the real relay ───────────────────────────────────────────────────────────

struct World {
    relay: String,
    relay_db: Arc<ferrocast_relay::Db>,
    _relay_dir: tempfile::TempDir,
}

async fn relay() -> World {
    let dir = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = ferrocast_relay::Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let blobs = ferrocast_relay::blob::FsBlobStore::new(dir.path().join("blobs")).unwrap();
    let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&dir.path().join("r.db")).unwrap())
        .with_blobs(Some(Arc::new(blobs)));
    let relay_db = state.db.clone();
    let app = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { relay: base, relay_db, _relay_dir: dir }
}

struct Pc {
    e: Engine,
    secrets: Arc<ferrocast_secrets::MemorySecrets>,
    _dir: tempfile::TempDir,
}

impl World {
    /// A PC with Pro on subscription `sub` (the relay's and the remembered status).
    async fn pc(&self, sub: &str) -> Pc {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(self.relay.clone()), ..Default::default() };
        let e = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
        e.cloud().cheap_scrypt.store(true, std::sync::atomic::Ordering::Relaxed);
        let auth = e.relay_auth().await.unwrap();
        let install = auth.split('.').next().unwrap().to_string();
        if self.relay_db.pro_sub(sub).unwrap().is_none() {
            self.relay_db.add_pro_sub(sub, "monthly", "e", "created", &install).unwrap();
            let s = Synced {
                status: "active".into(),
                current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
                cancel_at_end: None,
            };
            self.relay_db.sync_pro_sub(None, sub, &s).unwrap();
        } else {
            self.relay_db.link_pro_install(&install, sub).unwrap();
        }
        let n = now().0;
        let status = json!({
            "pro": true, "status": "active", "plan": "monthly", "subscription_id": sub,
            "current_end": n + 30 * 86_400_000, "access_until": n + 33 * 86_400_000, "checked_at": n,
            "features": { "backups": true }, "quotas": {}
        });
        e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
        // The tests drive uploads themselves.
        e.cloud_backup_auto(false).await.unwrap();
        Pc { e, secrets, _dir: dir }
    }

    fn uploads(&self, sub: &str) -> i64 {
        self.relay_db.quota_used(sub, "backup_uploads", ferrocast_relay::quota::Period::Day).unwrap()
    }
}

async fn counter(e: &Engine) -> i64 {
    e.db(|s| repo::bot::counter(s, "deaths")).await.unwrap()
}

async fn bump(e: &Engine, by: i64) {
    e.db(move |s| repo::bot::bump_counter(s, "deaths", by)).await.unwrap();
}

fn code(e: &EngineError) -> &'static str {
    match e {
        EngineError::Refused { code, .. } => code,
        _ => "other",
    }
}

#[tokio::test]
async fn a_backup_goes_up_encrypted_and_comes_back_on_this_pc() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    bump(&pc.e, 7).await;
    // The first upload waits for the recovery key to be saved.
    assert_eq!(code(&pc.e.cloud_backup_now().await.unwrap_err()), "key_not_saved");
    assert_eq!(code(&pc.e.cloud_backup_key_saved().await.unwrap_err()), "no_key");
    let file = pc.e.cloud_backup_key().await.unwrap();
    assert!(file.contains("AGE-SECRET-KEY-1"));
    assert_eq!(pc.e.cloud_backup_key().await.unwrap().lines().last(), file.lines().last(), "the same key each time");
    assert!(pc.secrets.get(KEY_SECRET).unwrap().is_some(), "kept in the secret store");
    let v = pc.e.cloud_backup_key_saved().await.unwrap();
    assert_eq!(v["key"], json!({ "exists": true, "saved": true, "passphrase": false }));
    let v = pc.e.cloud_backup_now().await.unwrap();
    let list = v["backups"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    assert_eq!((list[0]["reason"].as_str(), list[0]["this_install"].as_bool()), (Some("manual"), Some(true)));
    assert_eq!(v["busy"], Value::Null);
    assert_eq!(v["last"]["ok"], true);
    assert!(pc.e.cloud_state().await.upload.is_none());
    let id = list[0]["id"].as_str().unwrap().to_string();
    // What the relay holds is ciphertext: the counter's name never appears in it.
    let blob = w._relay_dir.path().join("blobs").join("backups").join("sub_1").join(&id);
    let raw = std::fs::read(&blob).unwrap();
    assert!(raw.starts_with(b"ferrocast-backup v1\n"));
    assert!(!raw.windows(6).any(|x| x == b"deaths"), "the relay never sees the plaintext");
    // Change the data, restore: it's back (and a local backup was taken first).
    bump(&pc.e, 5).await;
    assert_eq!(counter(&pc.e).await, 12);
    let report = pc.e.cloud_backup_restore(&id, None, None).await.unwrap();
    assert!(report["backup"].as_str().is_some_and(|b| !b.is_empty()), "{report}");
    assert_eq!(counter(&pc.e).await, 7);
    assert!(!pc.e.cloud_dir().join("restore.ferrobackup").exists(), "temporary files are gone");
    // Delete.
    pc.e.cloud_backup_delete(&id).await.unwrap();
    assert_eq!(pc.e.cloud_backups().await.unwrap()["backups"], json!([]));
    assert_eq!(code(&pc.e.cloud_backup_delete(&id).await.unwrap_err()), "not_found");
}

#[tokio::test]
async fn a_new_pc_needs_the_key_or_passphrase_and_a_wrong_one_fails_cleanly() {
    let w = relay().await;
    let old = w.pc("sub_1").await;
    bump(&old.e, 3).await;
    let key_text = old.e.cloud_backup_key().await.unwrap();
    old.e.cloud_backup_key_saved().await.unwrap();
    let v = old.e.cloud_backup_passphrase(Some("short".into())).await;
    assert!(matches!(v, Err(EngineError::Invalid(m)) if m.contains("12")));
    let v = old.e.cloud_backup_passphrase(Some("my long stream passphrase".into())).await.unwrap();
    assert_eq!(v["key"]["passphrase"], true);
    let v = old.e.cloud_backup_now().await.unwrap();
    let id = v["backups"][0]["id"].as_str().unwrap().to_string();

    // A fresh install on the same Pro sees it as another PC's.
    let new = w.pc("sub_1").await;
    let v = new.e.cloud_backups().await.unwrap();
    assert_eq!(v["key"]["exists"], false);
    assert_eq!(v["backups"][0]["this_install"], false);
    assert_eq!(code(&new.e.cloud_backup_restore(&id, None, None).await.unwrap_err()), "key_needed");
    let stranger = age::x25519::Identity::generate().to_string().expose_secret().to_string();
    assert_eq!(code(&new.e.cloud_backup_restore(&id, Some(stranger), None).await.unwrap_err()), "wrong_key");
    assert_eq!(
        code(&new.e.cloud_backup_restore(&id, None, Some("not my passphrase".into())).await.unwrap_err()),
        "wrong_passphrase"
    );
    assert!(matches!(new.e.cloud_backup_restore(&id, Some("hello".into()), None).await, Err(EngineError::Invalid(_))));
    assert_eq!(counter(&new.e).await, 0, "nothing was imported by a failed try");
    let v = new.e.cloud_backups().await.unwrap();
    assert_eq!(v["last"]["ok"], false);
    // The passphrase alone opens it, and this PC keeps that key from now on.
    new.e.cloud_backup_restore(&id, None, Some("my long stream passphrase".into())).await.unwrap();
    assert_eq!(counter(&new.e).await, 3);
    let v = new.e.cloud_backups().await.unwrap();
    assert_eq!(v["key"], json!({ "exists": true, "saved": true, "passphrase": true }));
    assert_eq!(new.e.cloud_backup_key().await.unwrap().lines().last(), key_text.lines().last());
    // A third PC with the key file's text.
    let third = w.pc("sub_1").await;
    third.e.cloud_backup_restore(&id, Some(key_text), None).await.unwrap();
    assert_eq!(counter(&third.e).await, 3);
}

#[tokio::test]
async fn automatic_uploads_skip_a_day_without_changes() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    pc.e.cloud_backup_key().await.unwrap();
    // Without the key saved, or with automatic uploads off, nothing goes.
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 0);
    pc.e.cloud_backup_key_saved().await.unwrap();
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 0, "automatic uploads are off");
    pc.e.cloud_backup_auto(true).await.unwrap();
    bump(&pc.e, 1).await;
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 1);
    let v = pc.e.cloud_backups().await.unwrap();
    assert_eq!(v["backups"][0]["reason"], "automatic");
    // The next day, nothing changed: no upload.
    pc.e.cloud_update(|s| s.last_upload_at = Some(now().0 - AUTO_EVERY_MS - 1)).await.unwrap();
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 1, "the same data isn't uploaded again");
    // The day after, a change: up it goes. Within the day it waits.
    bump(&pc.e, 1).await;
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 1, "once a day");
    pc.e.cloud_update(|s| s.last_upload_at = Some(now().0 - AUTO_EVERY_MS - 1)).await.unwrap();
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 2);
    // Without Pro (the remembered status says no), nothing runs.
    pc.e.db(|s| repo::settings::set(s, "pro_status", &json!({ "pro": false, "status": "none" }))).await.unwrap();
    pc.e.cloud_update(|s| s.last_upload_at = None).await.unwrap();
    bump(&pc.e, 1).await;
    pc.e.cloud_backup_round().await;
    assert_eq!(w.uploads("sub_1"), 2);
}

#[tokio::test]
async fn an_upload_cut_off_halfway_carries_on_after_a_restart() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    // A media file big enough for two parts (8 MiB each).
    let mut big = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    big.extend((0..9_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    let store = pc.e.store();
    tokio::task::spawn_blocking(move || {
        repo::media::save(&store, "big.png", &big).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
    .unwrap()
    .unwrap();
    pc.e.cloud_backup_key().await.unwrap();
    pc.e.cloud_backup_key_saved().await.unwrap();
    *pc.e.cloud().stop_after_parts.lock().unwrap() = Some(1);
    assert!(pc.e.cloud_backup_now().await.is_err(), "stopped after the first part");
    let saved = pc.e.cloud_state().await;
    let pending = saved.upload.clone().expect("the upload is remembered");
    assert_eq!((pending.parts(), pending.parts_done.clone()), (2, vec![0]));
    assert!(pc.e.cloud_dir().join("upload.ferrobackup").exists());
    assert_eq!(saved.last.as_ref().map(|l| l.ok), Some(false));
    // "Restart": nothing in memory, only what's on disk. The background round resumes it.
    *pc.e.cloud().stop_after_parts.lock().unwrap() = None;
    pc.e.cloud_backup_round().await;
    assert!(pc.e.cloud_state().await.upload.is_none(), "finished");
    assert_eq!(w.uploads("sub_1"), 1, "the same upload carried on; no new one was started");
    let v = pc.e.cloud_backups().await.unwrap();
    assert_eq!(v["backups"].as_array().unwrap().len(), 1);
    assert!(v["backups"][0]["bytes"].as_u64().unwrap() > 8 * 1024 * 1024);
    assert!(!pc.e.cloud_dir().join("upload.ferrobackup").exists());
    // And it opens.
    let id = v["backups"][0]["id"].as_str().unwrap().to_string();
    pc.e.cloud_backup_restore(&id, None, None).await.unwrap();
    // A damaged leftover (its hash doesn't match) is dropped, and a fresh upload starts.
    pc.e.cloud_update(|s| {
        s.upload = Some(Pending {
            id: "bk_000000000000000000000000".into(),
            part_size: 8,
            bytes: 3,
            sha256: "0".repeat(64),
            reason: "manual".into(),
            content_hash: "x".into(),
            parts_done: vec![],
        })
    })
    .await
    .unwrap();
    pc.e.cloud_backup_now().await.unwrap();
    assert_eq!(w.uploads("sub_1"), 2);
}

#[tokio::test]
async fn without_pro_on_the_relay_its_402_comes_through() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    w.relay_db
        .sync_pro_sub(None, "sub_1", &Synced { status: "halted".into(), current_end: None, cancel_at_end: None })
        .unwrap();
    let e = pc.e.cloud_backups().await.unwrap_err();
    assert!(matches!(&e, EngineError::Refused { status: 402, code: "pro_required", .. }), "{e:?}");
    pc.e.cloud_backup_key().await.unwrap();
    pc.e.cloud_backup_key_saved().await.unwrap();
    let e = pc.e.cloud_backup_now().await.unwrap_err();
    assert_eq!(code(&e), "pro_required");
    // Offline: a clear 502.
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some("http://127.0.0.1:1".into()), ..Default::default() };
    let off =
        Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    assert_eq!(code(&off.cloud_backups().await.unwrap_err()), "relay_unreachable");
}

// ── Review fixes ─────────────────────────────────────────────────────────────────────

#[test]
fn a_working_key_wins_over_a_wrong_passphrase() {
    let d = tempfile::tempdir().unwrap();
    let pack = tmp_file(d.path(), "p", b"setup");
    let key = age::x25519::Identity::generate();
    let wrapped = wrap_key(&key, "correct horse battery", true).unwrap();
    let sealed = d.path().join("s");
    seal(&pack, &sealed, &key.to_public(), Some(&wrapped)).unwrap();
    let out = d.path().join("o");
    // The key is tried first, so a wrong (or unneeded) passphrase doesn't get in its way.
    let opened = open(&sealed, &out, std::slice::from_ref(&key), Some("not the passphrase")).unwrap();
    assert_eq!(opened.identity.to_public(), key.to_public());
    assert_eq!(std::fs::read(&out).unwrap(), b"setup");
    // A wrong key and the right passphrase: the passphrase opens it.
    let other = age::x25519::Identity::generate();
    assert!(open(&sealed, &out, std::slice::from_ref(&other), Some("correct horse battery")).is_ok());
    // A wrong key, no passphrase on the backup: a wrong key, not "no passphrase".
    let plain = d.path().join("plain");
    seal(&pack, &plain, &key.to_public(), None).unwrap();
    assert_eq!(open(&plain, &out, &[other], Some("anything at all")).unwrap_err(), OpenErr::WrongKey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_uses_make_one_key_and_saved_means_that_key() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let e = pc.e.clone();
        tasks.push(tokio::spawn(async move {
            if i % 2 == 0 {
                e.cloud_backup_key().await.unwrap().lines().last().unwrap().to_string()
            } else {
                e.cloud_backup_passphrase(Some("a long enough phrase".into())).await.unwrap();
                e.cloud_backup_key().await.unwrap().lines().last().unwrap().to_string()
            }
        }));
    }
    let mut lines = Vec::new();
    for t in tasks {
        lines.push(t.await.unwrap());
    }
    lines.dedup();
    assert_eq!(lines.len(), 1, "every caller got the same key: {lines:?}");
    assert_eq!(pc.secrets.get(KEY_SECRET).unwrap().as_deref(), Some(lines[0].as_str()), "and it's the stored one");
    pc.e.cloud_backup_key_saved().await.unwrap();
    // The stored key changes after it was confirmed: it counts as not saved, nothing uploads.
    let other = age::x25519::Identity::generate();
    pc.secrets.set(KEY_SECRET, other.to_string().expose_secret()).unwrap();
    let v = pc.e.cloud_backups().await.unwrap();
    assert_eq!(v["key"]["saved"], false);
    assert_eq!(code(&pc.e.cloud_backup_now().await.unwrap_err()), "key_not_saved");
    assert_eq!(w.uploads("sub_1"), 0);
}

#[tokio::test]
async fn back_up_now_packs_todays_data_instead_of_resuming() {
    let w = relay().await;
    let pc = w.pc("sub_1").await;
    let mut big = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    big.extend((0..9_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    let store = pc.e.store();
    tokio::task::spawn_blocking(move || {
        repo::media::save(&store, "big.png", &big).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
    .unwrap()
    .unwrap();
    pc.e.cloud_backup_key().await.unwrap();
    pc.e.cloud_backup_key_saved().await.unwrap();
    // Yesterday's automatic upload stopped halfway.
    pc.e.cloud_backup_auto(true).await.unwrap();
    *pc.e.cloud().stop_after_parts.lock().unwrap() = Some(1);
    pc.e.cloud_backup_round().await;
    assert!(pc.e.cloud_state().await.upload.is_some());
    *pc.e.cloud().stop_after_parts.lock().unwrap() = None;
    // Today's change, then Back up now: a fresh pack with it, uploaded as a manual backup.
    bump(&pc.e, 9).await;
    let v = pc.e.cloud_backup_now().await.unwrap();
    assert_eq!(w.uploads("sub_1"), 2, "a new upload, not the old one resumed");
    let list = v["backups"].as_array().unwrap();
    assert_eq!(list.len(), 1, "the relay replaced the unfinished one: {v}");
    assert_eq!(list[0]["reason"], "manual");
    bump(&pc.e, 1).await;
    pc.e.cloud_backup_restore(list[0]["id"].as_str().unwrap(), None, None).await.unwrap();
    assert_eq!(counter(&pc.e).await, 9, "today's data is in it");
}

/// A relay whose download sends `chunks` pieces, `gap` apart (a slow line), or stalls.
async fn slow_relay(chunks: usize, gap: std::time::Duration) -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route("/v1/installs", post(|| async { axum::Json(json!({ "install_id": "i1", "install_secret": "s1" })) }))
        .route(
            "/v1/pro/backups/{id}",
            get(move || async move {
                let s = futures_util::stream::unfold(0usize, move |i| async move {
                    if i == chunks {
                        return None;
                    }
                    tokio::time::sleep(gap).await;
                    Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![7u8; 1000])), i + 1))
                });
                axum::body::Body::from_stream(s)
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

#[tokio::test]
async fn a_slow_but_steady_download_finishes_and_a_stalled_one_gives_up() {
    let id = "bk_000000000000000000000000";
    for (gap_ms, ok) in [(150, true), (900, false)] {
        let relay = slow_relay(8, std::time::Duration::from_millis(gap_ms)).await;
        let dir = tempfile::tempdir().unwrap();
        let apps = PlatformApps { relay_url: Some(relay), ..Default::default() };
        let e =
            Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
        *e.cloud().idle_timeout.lock().unwrap() = Some(std::time::Duration::from_millis(400));
        let to = dir.path().join("dl");
        let started = std::time::Instant::now();
        let r = e.download_backup(id, &to).await;
        if ok {
            r.unwrap();
            assert!(started.elapsed() > std::time::Duration::from_millis(800), "longer than the idle timeout in all");
            assert_eq!(std::fs::metadata(&to).unwrap().len(), 8000);
        } else {
            assert_eq!(code(&r.unwrap_err()), "relay_unreachable", "nothing arrived for too long");
        }
    }
}
