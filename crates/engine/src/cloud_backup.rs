//! Cloud backups for Ferrocast Pro (docs/plan/PRO.md F5, API.md §9.3).
//!
//! The same `.ferropack` as Export (never tokens or secrets), encrypted on this PC with
//! [age](https://age-encryption.org) before anything leaves it, then uploaded to the relay
//! in parts. The key is an X25519 identity made on first use and kept in the secret store
//! as `cloud-backup-key`; the streamer saves it as `ferrocast-recovery-key.txt` and says so
//! before the first upload, because a lost key means the backups can't be opened by anyone.
//! An optional passphrase travels with each backup as the key wrapped with age's scrypt, so
//! a new PC can restore with the passphrase alone.
//!
//! The file (`.ferrobackup`):
//!
//! ```text
//! ferrocast-backup v1\n
//! passphrase <base64 of an age scrypt file holding the AGE-SECRET-KEY line>\n   (optional)
//! \n
//! <an age file encrypted to the X25519 recipient, holding the .ferropack>
//! ```
//!
//! After the daily local backup, while Pro is active and the key is saved, the pack's
//! content hash is compared with the last upload's and a new backup goes up when it
//! changed; "Back up now" always uploads. Uploads survive a restart: what was sent is kept
//! in `cloud-backup/state.json` next to the ciphertext and carries on from the next part.
//! Restore downloads, checks the hash, decrypts and runs the normal import, which takes a
//! local backup first. None of this ever runs without Pro or gets in the free app's way.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use age::secrecy::{ExposeSecret, SecretString};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::pro::{relay_error, unreachable};
use crate::{Engine, EngineError, now};

/// The secret store key of the X25519 identity (`AGE-SECRET-KEY-1…`).
pub(crate) const KEY_SECRET: &str = "cloud-backup-key";
/// The file the dashboard offers to save.
pub const KEY_FILE_NAME: &str = "ferrocast-recovery-key.txt";
const MAGIC: &str = "ferrocast-backup v1";
const MAX_HEADER: u64 = 8 * 1024;
/// The relay's limits (`crates/relay/src/backups`): everything kept, and backups kept.
pub const MAX_TOTAL: u64 = 250_000_000;
pub const MAX_BACKUPS: u64 = 7;
pub const UPLOADS_PER_DAY: u64 = 6;
/// Passphrases shorter than this are refused (they protect a key that opens everything).
pub const MIN_PASSPHRASE: usize = 12;
/// How often the background task looks: resume an interrupted upload, catch a missed day.
const TICK: Duration = Duration::from_secs(3600);
/// An automatic upload at most this often (the daily local backup's rhythm).
const AUTO_EVERY_MS: i64 = 23 * 3_600_000;
/// One part can take a while on a slow upload (8 MB at 1 Mbit/s is about a minute).
const PART_TIMEOUT: Duration = Duration::from_secs(600);
/// Downloads have no total limit (250 MB on a slow line takes a while); they fail when the
/// connection can't be made or nothing arrives for this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether `s` says the streamer saved exactly this key.
fn confirmed(s: &Saved, id: Option<&age::x25519::Identity>) -> bool {
    id.is_some_and(|id| s.saved_key.as_deref() == Some(id.to_public().to_string().as_str()))
}

/// What's saved in `cloud-backup/state.json` (a file, not the settings table, so it's never
/// in a pack and a restore can't bring another PC's upload state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Saved {
    /// The public key (`age1…`) of the recovery key the streamer confirmed they saved. Uploads
    /// need it to match the key in the secret store, so nothing is ever encrypted to a key
    /// the streamer doesn't have.
    pub saved_key: Option<String>,
    /// The key wrapped with the passphrase (base64 age scrypt file), when one is set.
    pub wrapped_key: Option<String>,
    /// Automatic uploads after the daily backup.
    pub auto: bool,
    /// The content hash of the last pack that went up, and when.
    pub last_hash: Option<String>,
    pub last_upload_at: Option<i64>,
    /// The last upload or restore, for the dashboard.
    pub last: Option<Outcome>,
    /// An upload that hasn't completed yet.
    pub upload: Option<Pending>,
    /// An upload completed with the current key (a restore then keeps it).
    pub key_used: bool,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            saved_key: None,
            wrapped_key: None,
            auto: true,
            last_hash: None,
            last_upload_at: None,
            last: None,
            upload: None,
            key_used: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Outcome {
    pub at: i64,
    pub ok: bool,
    /// `upload` or `restore`.
    pub what: String,
    pub message: Option<String>,
}

/// An upload the relay knows about: its id and which parts it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub id: String,
    pub part_size: u64,
    pub bytes: u64,
    pub sha256: String,
    pub reason: String,
    pub content_hash: String,
    pub parts_done: Vec<u64>,
}

impl Pending {
    fn parts(&self) -> u64 {
        self.bytes.div_ceil(self.part_size.max(1))
    }
}

/// What's happening right now, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Progress {
    /// `packing`, `encrypting`, `uploading`, `downloading`, `decrypting`, `restoring`.
    pub phase: &'static str,
    pub done: u64,
    pub total: u64,
}

/// The engine's cloud backup state (in `Inner`).
#[derive(Debug, Default)]
pub(crate) struct Cloud {
    /// One upload or restore at a time.
    busy: tokio::sync::Mutex<()>,
    /// One read-change-write of the state file at a time.
    file: tokio::sync::Mutex<()>,
    /// One read-or-make (or replace) of the key at a time, so two first uses can't make two.
    key: tokio::sync::Mutex<()>,
    progress: std::sync::Mutex<Option<Progress>>,
    /// Tests: stop an upload after this many parts (a crash halfway).
    #[cfg(test)]
    pub stop_after_parts: std::sync::Mutex<Option<u64>>,
    /// Tests: a cheap scrypt work factor.
    #[cfg(test)]
    pub cheap_scrypt: std::sync::atomic::AtomicBool,
    /// Tests: a short download idle timeout.
    #[cfg(test)]
    pub idle_timeout: std::sync::Mutex<Option<Duration>>,
}

fn refused(status: u16, code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

fn io_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Store(ferrocast_store::StoreError::Io(std::io::Error::other(e.to_string())))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn mb(n: u64) -> String {
    format!("{:.1} MB", n as f64 / 1e6)
}

// ── The key and the file format (pure, blocking) ─────────────────────────────────────

/// The recovery key file's text.
pub(crate) fn key_file(identity: &age::x25519::Identity, created: i64) -> String {
    let date = chrono_date(created);
    format!(
        "# Ferrocast recovery key for cloud backups (made {date}).\n\
         # Keep it somewhere safe and private: a password manager or a USB stick.\n\
         # Your cloud backups can only be opened with this key (or your passphrase, if you set one).\n\
         # If you lose it, nobody can open them, Ferrocast included.\n\
         # public key: {}\n{}\n",
        identity.to_public(),
        identity.to_string().expose_secret()
    )
}

/// `2026-10-19` (UTC).
fn chrono_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

/// The identity in a pasted key or a recovery key file (the `AGE-SECRET-KEY-1…` line).
pub(crate) fn parse_key(text: &str) -> Option<age::x25519::Identity> {
    text.lines()
        .map(str::trim)
        .find(|l| l.to_ascii_uppercase().starts_with("AGE-SECRET-KEY-1"))
        .and_then(|l| age::x25519::Identity::from_str(&l.to_ascii_uppercase()).ok())
}

/// The key wrapped with a passphrase: an age scrypt file, base64.
pub(crate) fn wrap_key(identity: &age::x25519::Identity, passphrase: &str, cheap: bool) -> Result<String, String> {
    let mut r = age::scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
    if cheap {
        r.set_work_factor(4);
    }
    let enc = age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut w = enc.wrap_output(&mut out).map_err(|e| e.to_string())?;
    w.write_all(identity.to_string().expose_secret().as_bytes()).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

/// Why a backup couldn't be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenErr {
    /// Not a Ferrocast backup, or damaged.
    Damaged,
    /// No key here and none given.
    KeyNeeded,
    /// The key(s) tried don't open it.
    WrongKey,
    /// The passphrase doesn't open the key in it.
    WrongPassphrase,
    /// A passphrase was given but this backup was saved without one.
    NoPassphrase,
    Io(String),
}

impl OpenErr {
    fn api(self) -> EngineError {
        match self {
            OpenErr::Damaged => refused(
                422,
                "damaged_backup",
                "This cloud backup can't be read: it isn't a Ferrocast backup or it's damaged.",
                "Pick another backup.",
            ),
            OpenErr::KeyNeeded => refused(
                422,
                "key_needed",
                "This backup needs its recovery key or passphrase.",
                "Paste the key from ferrocast-recovery-key.txt, or type your passphrase.",
            ),
            OpenErr::WrongKey => refused(
                422,
                "wrong_key",
                "That recovery key doesn't open this backup.",
                "Use the key file from the PC that made it, or its passphrase.",
            ),
            OpenErr::WrongPassphrase => {
                refused(422, "wrong_passphrase", "That passphrase doesn't open this backup.", "Check it and try again.")
            }
            OpenErr::NoPassphrase => refused(
                422,
                "no_passphrase",
                "This backup was saved without a passphrase.",
                "Use the recovery key file from the PC that made it.",
            ),
            OpenErr::Io(e) => io_err(e),
        }
    }
}

/// Reads the header: the wrapped key, if there is one. Leaves `r` at the age file.
fn read_header<R: BufRead>(r: &mut R) -> Result<Option<String>, OpenErr> {
    let mut wrapped = None;
    let mut first = true;
    let mut limited = r.take(MAX_HEADER);
    loop {
        let mut line = String::new();
        let n = limited.read_line(&mut line).map_err(|_| OpenErr::Damaged)?;
        if n == 0 || !line.ends_with('\n') {
            return Err(OpenErr::Damaged);
        }
        let line = line.trim_end_matches('\n');
        if first {
            if line != MAGIC {
                return Err(OpenErr::Damaged);
            }
            first = false;
            continue;
        }
        if line.is_empty() {
            return Ok(wrapped);
        }
        if let Some(w) = line.strip_prefix("passphrase ") {
            wrapped = Some(w.to_string());
        }
        // Unknown lines are for later versions: skipped.
    }
}

/// Encrypts `pack` into `out` for `recipient` (with the wrapped key in the header when
/// there is one). Returns `(bytes, sha256)` of what was written.
pub(crate) fn seal(
    pack: &Path,
    out: &Path,
    recipient: &age::x25519::Recipient,
    wrapped: Option<&str>,
) -> std::io::Result<(u64, String)> {
    struct Hashing<W: Write> {
        inner: W,
        h: Sha256,
        n: u64,
    }
    impl<W: Write> Write for Hashing<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = self.inner.write(buf)?;
            self.h.update(&buf[..n]);
            self.n += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    let file = std::fs::File::create(out)?;
    let mut w = Hashing { inner: std::io::BufWriter::new(file), h: Sha256::new(), n: 0 };
    w.write_all(format!("{MAGIC}\n").as_bytes())?;
    if let Some(k) = wrapped {
        w.write_all(format!("passphrase {k}\n").as_bytes())?;
    }
    w.write_all(b"\n")?;
    let enc = age::Encryptor::with_recipients(std::iter::once(recipient as &dyn age::Recipient))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut sealed = enc.wrap_output(&mut w)?;
    std::io::copy(&mut BufReader::new(std::fs::File::open(pack)?), &mut sealed)?;
    sealed.finish()?;
    w.flush()?;
    let Hashing { inner, h, n } = w;
    inner.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok((n, hex(&h.finalize())))
}

/// What opened a backup.
pub(crate) struct Opened {
    /// The identity that worked.
    pub identity: age::x25519::Identity,
    /// The backup's wrapped key, if it had one.
    pub wrapped: Option<String>,
}

impl std::fmt::Debug for Opened {
    // Never print the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened").field("passphrase", &self.wrapped.is_some()).finish_non_exhaustive()
    }
}

/// Decrypts `file` into `out` with the first of `keys` that opens it, or else the
/// passphrase on the backup's wrapped key.
pub(crate) fn open(
    file: &Path,
    out: &Path,
    keys: &[age::x25519::Identity],
    passphrase: Option<&str>,
) -> Result<Opened, OpenErr> {
    let mut r = BufReader::new(std::fs::File::open(file).map_err(|e| OpenErr::Io(e.to_string()))?);
    let wrapped = read_header(&mut r)?;
    // This PC's key and pasted keys first; the passphrase only when none of them fits.
    let identity = match which_key(file, keys) {
        Some(id) => id,
        None => {
            let Some(p) = passphrase else {
                return Err(if keys.is_empty() { OpenErr::KeyNeeded } else { OpenErr::WrongKey });
            };
            let Some(w) = &wrapped else {
                return Err(if keys.is_empty() { OpenErr::NoPassphrase } else { OpenErr::WrongKey });
            };
            let raw = base64::engine::general_purpose::STANDARD.decode(w.trim()).map_err(|_| OpenErr::Damaged)?;
            let id = age::scrypt::Identity::new(SecretString::from(p.to_string()));
            let dec = age::Decryptor::new(&raw[..]).map_err(|_| OpenErr::Damaged)?;
            let mut text = String::new();
            dec.decrypt(std::iter::once(&id as &dyn age::Identity))
                .map_err(|_| OpenErr::WrongPassphrase)?
                .read_to_string(&mut text)
                .map_err(|_| OpenErr::WrongPassphrase)?;
            let inner = parse_key(&text).ok_or(OpenErr::Damaged)?;
            which_key(file, std::slice::from_ref(&inner)).ok_or(OpenErr::Damaged)?
        }
    };
    let dec = age::Decryptor::new(r).map_err(|_| OpenErr::Damaged)?;
    let mut reader = match dec.decrypt(std::iter::once(&identity as &dyn age::Identity)) {
        Ok(r) => r,
        Err(age::DecryptError::NoMatchingKeys) => return Err(OpenErr::WrongKey),
        Err(_) => return Err(OpenErr::Damaged),
    };
    let mut outf = std::fs::File::create(out).map_err(|e| OpenErr::Io(e.to_string()))?;
    // A failed check of the payload (cut or changed bytes) is a damaged backup.
    std::io::copy(&mut reader, &mut outf).map_err(|_| OpenErr::Damaged)?;
    outf.sync_all().map_err(|e| OpenErr::Io(e.to_string()))?;
    Ok(Opened { identity, wrapped })
}

/// The key among `tries` that opens `file` (header only, cheap).
fn which_key(file: &Path, tries: &[age::x25519::Identity]) -> Option<age::x25519::Identity> {
    for id in tries {
        let mut r = BufReader::new(std::fs::File::open(file).ok()?);
        read_header(&mut r).ok()?;
        let dec = age::Decryptor::new(r).ok()?;
        if dec.decrypt(std::iter::once(id as &dyn age::Identity)).is_ok() {
            return Some(id.clone());
        }
    }
    None
}

/// The SHA-256 of a file (hex) and its size.
fn file_sha(path: &Path) -> std::io::Result<(u64, String)> {
    let mut f = BufReader::new(std::fs::File::open(path)?);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((n, hex(&h.finalize())))
}

// ── The engine side ──────────────────────────────────────────────────────────────────

/// Why an upload went (`reason` on the relay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    Automatic,
    Manual,
}

impl Reason {
    fn key(self) -> &'static str {
        match self {
            Reason::Automatic => "automatic",
            Reason::Manual => "manual",
        }
    }
}

impl Engine {
    fn cloud(&self) -> &Cloud {
        &self.inner.cloud_backup
    }

    fn cloud_dir(&self) -> PathBuf {
        self.store().dir().join("cloud-backup")
    }

    fn set_progress(&self, p: Option<Progress>) {
        *self.cloud().progress.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }

    pub(crate) async fn cloud_state(&self) -> Saved {
        let path = self.cloud_dir().join("state.json");
        match tokio::fs::read(&path).await {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "the cloud backup state can't be read; starting over");
                Saved::default()
            }),
            Err(_) => Saved::default(),
        }
    }

    /// Changes the state file (temp file + rename, one change at a time).
    pub(crate) async fn cloud_update<T>(&self, f: impl FnOnce(&mut Saved) -> T) -> Result<T, EngineError> {
        let _one = self.cloud().file.lock().await;
        let mut s = self.cloud_state().await;
        let out = f(&mut s);
        let dir = self.cloud_dir();
        tokio::fs::create_dir_all(&dir).await.map_err(io_err)?;
        let tmp = dir.join("state.json.partial");
        let bytes = serde_json::to_vec_pretty(&s).map_err(io_err)?;
        tokio::fs::write(&tmp, bytes).await.map_err(io_err)?;
        tokio::fs::rename(&tmp, dir.join("state.json")).await.map_err(io_err)?;
        Ok(out)
    }

    fn cloud_identity(&self) -> Result<Option<age::x25519::Identity>, EngineError> {
        let text = self.inner.secrets.get(KEY_SECRET).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        Ok(text.as_deref().and_then(parse_key))
    }

    /// The key, made and saved in the secret store the first time. Serialized, so concurrent
    /// first uses (two downloads, a download and a passphrase) all get the same key.
    async fn cloud_identity_or_new(&self) -> Result<age::x25519::Identity, EngineError> {
        let _one = self.cloud().key.lock().await;
        if let Some(id) = self.cloud_identity()? {
            return Ok(id);
        }
        let id = age::x25519::Identity::generate();
        self.inner
            .secrets
            .set(KEY_SECRET, id.to_string().expose_secret())
            .map_err(|e| EngineError::Unavailable(format!("Ferrocast couldn't save the recovery key: {e}")))?;
        Ok(id)
    }

    fn cheap_scrypt(&self) -> bool {
        #[cfg(test)]
        {
            self.cloud().cheap_scrypt.load(std::sync::atomic::Ordering::Relaxed)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// The recovery key file's text (`GET /api/v1/pro/backups/key`), making the key first.
    pub async fn cloud_backup_key(&self) -> Result<String, EngineError> {
        let id = self.cloud_identity_or_new().await?;
        Ok(key_file(&id, now().0))
    }

    /// The streamer saved the recovery key: uploads may start.
    pub async fn cloud_backup_key_saved(&self) -> Result<Value, EngineError> {
        let public = {
            let _one = self.cloud().key.lock().await;
            match self.cloud_identity()? {
                Some(id) => id.to_public().to_string(),
                None => {
                    return Err(refused(
                        409,
                        "no_key",
                        "There's no recovery key yet.",
                        "Download or copy the key first.",
                    ));
                }
            }
        };
        // Bound to the key in the secret store now: the one every download handed out.
        self.cloud_update(move |s| s.saved_key = Some(public)).await?;
        self.cloud_backups_local().await
    }

    /// Sets (or with `None` removes) the passphrase that can also open new backups.
    pub async fn cloud_backup_passphrase(&self, passphrase: Option<String>) -> Result<Value, EngineError> {
        let wrapped = match passphrase {
            None => None,
            Some(p) => {
                if p.chars().count() < MIN_PASSPHRASE {
                    return Err(EngineError::Invalid(format!(
                        "passphrase: at least {MIN_PASSPHRASE} characters (a few words work well)."
                    )));
                }
                let id = self.cloud_identity_or_new().await?;
                let cheap = self.cheap_scrypt();
                let w = tokio::task::spawn_blocking(move || wrap_key(&id, &p, cheap))
                    .await
                    .map_err(|e| EngineError::Internal(e.to_string()))?
                    .map_err(io_err)?;
                Some(w)
            }
        };
        self.cloud_update(|s| s.wrapped_key = wrapped).await?;
        self.cloud_backups_local().await
    }

    /// Automatic uploads on or off.
    pub async fn cloud_backup_auto(&self, on: bool) -> Result<Value, EngineError> {
        self.cloud_update(|s| s.auto = on).await?;
        self.cloud_backups_local().await
    }

    /// This PC's part of the view (no relay call).
    async fn cloud_backups_local(&self) -> Result<Value, EngineError> {
        let s = self.cloud_state().await;
        let id = self.cloud_identity()?;
        let key = id.is_some();
        let saved = confirmed(&s, id.as_ref());
        let progress = self.cloud().progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(json!({
            "key": { "exists": key, "saved": saved, "passphrase": key && s.wrapped_key.is_some() },
            "auto": s.auto,
            "busy": progress,
            "last": s.last.as_ref().map(|o| json!({ "at": o.at, "ok": o.ok, "what": o.what, "message": o.message })),
            "last_upload_at": s.last_upload_at,
            "uploading": s.upload.as_ref().map(|u| json!({ "parts_done": u.parts_done.len(), "parts": u.parts() })),
            "limits": { "bytes": MAX_TOTAL, "backups": MAX_BACKUPS, "uploads_per_day": UPLOADS_PER_DAY },
        }))
    }

    /// `GET /api/v1/pro/backups`: this PC's key and upload state plus the relay's list.
    pub async fn cloud_backups(&self) -> Result<Value, EngineError> {
        let mut v = self.cloud_backups_local().await?;
        let list = self.pro_call(reqwest::Method::GET, "/v1/pro/backups", None).await?;
        v["backups"] = list;
        Ok(v)
    }

    /// `DELETE /api/v1/pro/backups/{id}`.
    pub async fn cloud_backup_delete(&self, id: &str) -> Result<(), EngineError> {
        if !id_ok(id) {
            return Err(EngineError::Missing {
                code: "not_found",
                message: "There's no cloud backup with that id.",
                hint: "Reload the list.",
            });
        }
        self.pro_raw(reqwest::Method::DELETE, &format!("/v1/pro/backups/{id}"), None).await?;
        Ok(())
    }

    /// "Back up now": packs, encrypts and uploads one backup, waiting for it.
    pub async fn cloud_backup_now(&self) -> Result<Value, EngineError> {
        let Ok(_busy) = self.cloud().busy.try_lock() else {
            return Err(refused(409, "backup_running", "A cloud backup is already running.", "Wait for it to finish."));
        };
        let s = self.cloud_state().await;
        if !confirmed(&s, self.cloud_identity()?.as_ref()) {
            return Err(refused(
                409,
                "key_not_saved",
                "Save your recovery key first.",
                "Without it nobody can open your cloud backups, so Ferrocast asks before the first one.",
            ));
        }
        let r = self.cloud_upload(Reason::Manual, true).await;
        self.note_outcome("upload", &r).await;
        r?;
        self.cloud_backups().await
    }

    async fn note_outcome<T>(&self, what: &str, r: &Result<T, EngineError>) {
        let message = r.as_ref().err().map(ToString::to_string);
        let ok = r.is_ok();
        let what = what.to_string();
        let _ = self.cloud_update(|s| s.last = Some(Outcome { at: now().0, ok, what, message })).await;
    }

    /// One automatic round: resume an interrupted upload, or upload when the day's due and
    /// the data changed. Quiet: failures are logged and tried again next round.
    pub(crate) async fn cloud_backup_round(&self) {
        if !self.pro_has(|f| f.backups).await {
            return;
        }
        let s = self.cloud_state().await;
        if !confirmed(&s, self.cloud_identity().ok().flatten().as_ref()) {
            return;
        }
        let resume = s.upload.is_some();
        let due = s.last_upload_at.is_none_or(|t| now().0 - t >= AUTO_EVERY_MS);
        if !resume && !(s.auto && due) {
            return;
        }
        let Ok(_busy) = self.cloud().busy.try_lock() else { return };
        let r = self.cloud_upload(Reason::Automatic, false).await;
        match &r {
            Ok(true) => tracing::info!("cloud backup uploaded"),
            Ok(false) => tracing::debug!("cloud backup: nothing changed since the last one"),
            Err(e) => tracing::warn!(error = %e, "cloud backup didn't go up; trying again later"),
        }
        if !matches!(r, Ok(false)) {
            self.note_outcome("upload", &r).await;
        }
    }

    /// After the daily local backup (`backups::run`).
    pub(crate) fn cloud_backup_after_daily(&self) {
        let engine = self.clone();
        tokio::spawn(async move { engine.cloud_backup_round().await });
    }

    /// Part uploads: each part fails fast enough to be retried.
    fn relay_upload(&self) -> reqwest::Client {
        reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).timeout(PART_TIMEOUT).build().unwrap_or_default()
    }

    /// Downloads: no total limit, only a connect timeout and an idle one per read, so a slow
    /// but steady line finishes and a stalled one gives up.
    fn relay_download(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(self.idle_timeout())
            .build()
            .unwrap_or_default()
    }

    fn idle_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(d) = *self.cloud().idle_timeout.lock().unwrap_or_else(|e| e.into_inner()) {
            return d;
        }
        IDLE_TIMEOUT
    }

    /// Uploads one backup (or finishes the interrupted one). `force` uploads even when the
    /// data didn't change. Returns false when nothing needed to go.
    async fn cloud_upload(&self, reason: Reason, force: bool) -> Result<bool, EngineError> {
        let dir = self.cloud_dir();
        tokio::fs::create_dir_all(&dir).await.map_err(io_err)?;
        let file = dir.join("upload.ferrobackup");
        let saved = self.cloud_state().await;
        let result = async {
            // An interrupted upload carries on if its file is still the one the relay expects.
            // "Back up now" always packs today's data instead (a new POST replaces this PC's
            // unfinished upload on the relay).
            let mut pending = match saved.upload.clone() {
                Some(_) if force => {
                    self.cloud_update(|s| s.upload = None).await?;
                    let _ = tokio::fs::remove_file(&file).await;
                    None
                }
                Some(p) => {
                    let f = file.clone();
                    let same = tokio::task::spawn_blocking(move || file_sha(&f)).await.ok().and_then(Result::ok);
                    if same.as_ref().is_some_and(|(n, sha)| *n == p.bytes && *sha == p.sha256) {
                        Some(p)
                    } else {
                        self.cloud_update(|s| s.upload = None).await?;
                        None
                    }
                }
                None => None,
            };
            if pending.is_none() {
                self.set_progress(Some(Progress { phase: "packing", done: 0, total: 0 }));
                let hash = self.db(ferrocast_store::pack::content_hash).await?;
                if !force && saved.last_hash.as_deref() == Some(hash.as_str()) {
                    // Nothing changed: this day counts as done.
                    self.cloud_update(|s| s.last_upload_at = Some(now().0)).await?;
                    return Ok(false);
                }
                let pack = dir.join("pack.ferropack");
                let p = pack.clone();
                self.db(move |s| ferrocast_store::pack::export(s, &p, env!("CARGO_PKG_VERSION"))).await?;
                self.set_progress(Some(Progress { phase: "encrypting", done: 0, total: 0 }));
                // Only the key the streamer confirmed saving, never one made or swapped since.
                let id = self.cloud_identity()?.filter(|id| confirmed(&saved, Some(id))).ok_or_else(|| {
                    refused(
                        409,
                        "key_not_saved",
                        "Save your recovery key first.",
                        "The recovery key changed since you saved it. Download it again and confirm.",
                    )
                })?;
                let wrapped = saved.wrapped_key.clone();
                let (pk, out) = (pack.clone(), file.clone());
                let sealed =
                    tokio::task::spawn_blocking(move || seal(&pk, &out, &id.to_public(), wrapped.as_deref())).await;
                let _ = tokio::fs::remove_file(&pack).await;
                let (bytes, sha) = sealed.map_err(|e| EngineError::Internal(e.to_string()))?.map_err(io_err)?;
                if bytes > MAX_TOTAL {
                    let _ = tokio::fs::remove_file(&file).await;
                    return Err(refused(
                        413,
                        "backup_quota",
                        format!("Your data is {} once packed; cloud backups hold up to {}.", mb(bytes), mb(MAX_TOTAL)),
                        "Remove big videos from the media library, or use Export to keep a copy yourself.",
                    ));
                }
                let body = json!({ "bytes": bytes, "sha256": sha, "app_version": env!("CARGO_PKG_VERSION"), "reason": reason.key() });
                let v = self.pro_call(reqwest::Method::POST, "/v1/pro/backups", Some(body)).await?;
                let (Some(id), Some(part_size)) = (v["id"].as_str(), v["part_size"].as_u64()) else {
                    return Err(unreachable("it sent an answer Ferrocast can't read"));
                };
                let p = Pending {
                    id: id.to_string(),
                    part_size: part_size.max(1),
                    bytes,
                    sha256: sha,
                    reason: reason.key().into(),
                    content_hash: hash,
                    parts_done: Vec::new(),
                };
                let keep = p.clone();
                self.cloud_update(move |s| s.upload = Some(keep)).await?;
                pending = Some(p);
            }
            let Some(p) = pending else { return Ok(false) };
            self.send_parts(&p, &file).await?;
            self.complete_upload(&p).await?;
            let _ = tokio::fs::remove_file(&file).await;
            Ok(true)
        }
        .await;
        self.set_progress(None);
        result
    }

    async fn send_parts(&self, p: &Pending, file: &Path) -> Result<(), EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let http = self.relay_upload();
        let total = p.parts();
        let mut done: Vec<u64> = p.parts_done.clone();
        #[cfg(test)]
        let stop = *self.cloud().stop_after_parts.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        let mut sent_now = 0u64;
        for n in 0..total {
            if done.contains(&n) {
                continue;
            }
            #[cfg(test)]
            if stop.is_some_and(|s| sent_now >= s) {
                return Err(EngineError::Unavailable("stopped for the test".into()));
            }
            self.set_progress(Some(Progress { phase: "uploading", done: done.len() as u64, total }));
            let (f, at, len) = (file.to_path_buf(), n * p.part_size, p.part_size.min(p.bytes - n * p.part_size));
            let chunk = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                use std::io::Seek;
                let mut fh = std::fs::File::open(f)?;
                fh.seek(std::io::SeekFrom::Start(at))?;
                let mut buf = vec![0u8; usize::try_from(len).unwrap_or(0)];
                fh.read_exact(&mut buf)?;
                Ok(buf)
            })
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?
            .map_err(io_err)?;
            let res = http
                .put(format!("{url}/v1/pro/backups/{}/parts/{n}", p.id))
                .bearer_auth(&auth)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(chunk)
                .send()
                .await
                .map_err(|e| unreachable(e.without_url()))?;
            if !res.status().is_success() {
                let status = res.status().as_u16();
                let err = relay_error(res).await;
                if matches!(status, 404 | 409) {
                    // The relay forgot this upload (swept, replaced, or done): start over next time.
                    self.cloud_update(|s| s.upload = None).await?;
                }
                return Err(err);
            }
            done.push(n);
            let keep = done.clone();
            self.cloud_update(move |s| {
                if let Some(u) = s.upload.as_mut() {
                    u.parts_done = keep;
                }
            })
            .await?;
            #[cfg(test)]
            {
                sent_now += 1;
            }
        }
        Ok(())
    }

    async fn complete_upload(&self, p: &Pending) -> Result<(), EngineError> {
        let res =
            self.pro_call(reqwest::Method::POST, &format!("/v1/pro/backups/{}/complete", p.id), Some(json!({}))).await;
        match res {
            Ok(_) => {
                let hash = p.content_hash.clone();
                self.cloud_update(move |s| {
                    s.upload = None;
                    s.last_hash = Some(hash);
                    s.last_upload_at = Some(now().0);
                    s.key_used = true;
                })
                .await?;
                Ok(())
            }
            Err(e) => {
                if let EngineError::Refused { code, .. } = &e {
                    match *code {
                        // Some parts didn't arrive: send them again next time.
                        "parts_missing" => {
                            self.cloud_update(|s| {
                                if let Some(u) = s.upload.as_mut() {
                                    u.parts_done.clear();
                                }
                            })
                            .await?;
                        }
                        // The relay deleted it (or it's gone): start over next time.
                        "hash_mismatch" | "not_found" => {
                            self.cloud_update(|s| s.upload = None).await?;
                        }
                        _ => {}
                    }
                }
                Err(e)
            }
        }
    }

    /// `POST /api/v1/pro/backups/{id}/restore`: downloads a backup, opens it with this PC's
    /// key (or the `key` / `passphrase` given) and imports it (a local backup is taken
    /// first). On a PC without a key of its own yet, the key that worked is kept.
    pub async fn cloud_backup_restore(
        &self,
        id: &str,
        key: Option<String>,
        passphrase: Option<String>,
    ) -> Result<Value, EngineError> {
        if !id_ok(id) {
            return Err(EngineError::Missing {
                code: "not_found",
                message: "There's no cloud backup with that id.",
                hint: "Reload the list.",
            });
        }
        let given = match key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            None => None,
            Some(k) => Some(parse_key(k).ok_or_else(|| {
                EngineError::Invalid("key: paste the line that starts with AGE-SECRET-KEY-1.".into())
            })?),
        };
        let passphrase = passphrase.filter(|p| !p.is_empty());
        let Ok(_busy) = self.cloud().busy.try_lock() else {
            return Err(refused(409, "backup_running", "A cloud backup is already running.", "Wait for it to finish."));
        };
        let r = self.restore_inner(id, given, passphrase).await;
        self.set_progress(None);
        self.note_outcome("restore", &r).await;
        r
    }

    async fn restore_inner(
        &self,
        id: &str,
        given: Option<age::x25519::Identity>,
        passphrase: Option<String>,
    ) -> Result<Value, EngineError> {
        let dir = self.cloud_dir();
        tokio::fs::create_dir_all(&dir).await.map_err(io_err)?;
        let file = dir.join("restore.ferrobackup");
        let pack = dir.join("restore.ferropack");
        let cleanup = || async {
            let _ = tokio::fs::remove_file(&file).await;
            let _ = tokio::fs::remove_file(&pack).await;
        };
        let r = async {
            self.download_backup(id, &file).await?;
            self.set_progress(Some(Progress { phase: "decrypting", done: 0, total: 0 }));
            let mine = self.cloud_identity()?;
            let keys: Vec<age::x25519::Identity> = mine.iter().cloned().chain(given.clone()).collect();
            let (f, p) = (file.clone(), pack.clone());
            let opened = tokio::task::spawn_blocking(move || open(&f, &p, &keys, passphrase.as_deref()))
                .await
                .map_err(|e| EngineError::Internal(e.to_string()))?
                .map_err(OpenErr::api)?;
            self.set_progress(Some(Progress { phase: "restoring", done: 0, total: 0 }));
            let p = pack.clone();
            let report = self.db(move |s| ferrocast_store::pack::import(s, &p)).await.map_err(pack_error)?;
            self.reload().await?;
            // A PC without its own key in use takes the one that worked (so its backups and
            // the other PC's open with one key from now on).
            let s = self.cloud_state().await;
            let adopt = match &mine {
                None => true,
                Some(m) => !s.key_used && m.to_public().to_string() != opened.identity.to_public().to_string(),
            };
            if adopt {
                let _one = self.cloud().key.lock().await;
                let public = opened.identity.to_public().to_string();
                self.inner
                    .secrets
                    .set(KEY_SECRET, opened.identity.to_string().expose_secret())
                    .map_err(|e| EngineError::Unavailable(format!("Ferrocast couldn't save the recovery key: {e}")))?;
                let wrapped = opened.wrapped.clone();
                self.cloud_update(move |s| {
                    s.saved_key = Some(public);
                    s.wrapped_key = wrapped;
                    s.upload = None;
                })
                .await?;
            }
            Ok(json!(report))
        }
        .await;
        cleanup().await;
        r
    }

    async fn download_backup(&self, id: &str, to: &Path) -> Result<(), EngineError> {
        use tokio::io::AsyncWriteExt;
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let mut res = self
            .relay_download()
            .get(format!("{url}/v1/pro/backups/{id}"))
            .bearer_auth(auth)
            .send()
            .await
            .map_err(|e| unreachable(e.without_url()))?;
        if !res.status().is_success() {
            return Err(relay_error(res).await);
        }
        let want = res.headers().get("x-content-sha256").and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase);
        let total = res.content_length().unwrap_or(0);
        let mut out = tokio::fs::File::create(to).await.map_err(io_err)?;
        let mut h = Sha256::new();
        let mut got = 0u64;
        while let Some(chunk) = res.chunk().await.map_err(|e| unreachable(e.without_url()))? {
            got += chunk.len() as u64;
            if got > MAX_TOTAL {
                return Err(OpenErr::Damaged.api());
            }
            h.update(&chunk);
            out.write_all(&chunk).await.map_err(io_err)?;
            self.set_progress(Some(Progress { phase: "downloading", done: got, total }));
        }
        out.sync_all().await.map_err(io_err)?;
        if want.is_some_and(|w| w != hex(&h.finalize())) {
            return Err(refused(
                502,
                "download_damaged",
                "The backup didn't arrive whole.",
                "Try again; nothing on this PC changed.",
            ));
        }
        Ok(())
    }
}

fn id_ok(id: &str) -> bool {
    id.len() == 27 && id.starts_with("bk_") && id[3..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn pack_error(e: EngineError) -> EngineError {
    use ferrocast_store::PackError as P;
    match e {
        EngineError::Store(ferrocast_store::StoreError::Pack(p)) => {
            let message = p.to_string();
            match p {
                P::NotAPack | P::Unsafe(_) | P::Corrupt(_) => {
                    refused(415, "damaged_pack", message, "This backup's data can't be imported. Try another one.")
                }
                P::TooNew(_) => {
                    refused(422, "pack_too_new", message, "Update Ferrocast, then restore the backup again.")
                }
                P::TooLarge(_) => refused(413, "too_large", message, ""),
            }
        }
        other => other,
    }
}

/// The background task: a round an hour (resume, catch a missed day). The daily backup
/// also starts one as soon as it's taken.
pub(crate) async fn run(engine: Engine) {
    // Let the app settle before the first look.
    tokio::time::sleep(Duration::from_secs(120)).await;
    loop {
        engine.cloud_backup_round().await;
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests;
