//! Secrets (platform tokens, the OBS password, webhook URLs) never go in the database,
//! settings, exports or logs (R-45). Where they do go depends on the OS:
//!
//! - **macOS**: a private file, `secrets.json` in Ferrocast's data folder ([`FileSecrets`]).
//!   The folder is `0700` and the file `0600`, so only this user can read it. Not the login
//!   Keychain: our builds are ad-hoc signed (no paid Apple Developer ID), so every update
//!   has a new code identity and macOS would ask for the login password ("Ferrocast wants to
//!   access key 'Ferrocast' in your keychain") after every update. That looks like phishing
//!   and trains people to type their password into prompts. OBS itself keeps its WebSocket
//!   password in a plain config file, so a user-only file is the same bar as the tool it
//!   talks to. Old Keychain items are never read (reading them is what shows the prompt):
//!   after the switch, accounts are connected again once.
//! - **Windows**: Credential Manager. **Linux**: Secret Service. Neither prompts.
//! - Tests, and machines with no keychain: [`MemorySecrets`] (sign in again after a restart
//!   rather than writing tokens to an unprotected place).
//!
//! Callers use the [`Secrets`] trait so the store can be swapped.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The keychain service name every Ferrocast secret is filed under (Windows and Linux).
pub const SERVICE: &str = "Ferrocast";

/// The secrets file's name inside the data folder (macOS).
pub const FILE_NAME: &str = "secrets.json";

/// Bigger than any real set of tokens; a huge file is a damaged one.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("this computer's keychain isn't available: {0}")]
    Unavailable(String),
    #[error("the keychain refused: {0}")]
    Keychain(String),
    /// The secrets file couldn't be read or written (permissions, a full disk).
    #[error("Ferrocast's private settings file couldn't be used ({path}): {why}")]
    File { path: String, why: String },
    /// The secrets file is there but isn't what Ferrocast writes. It is left alone.
    #[error(
        "Ferrocast's private settings file is damaged ({path}). Nothing was changed. Quit Ferrocast, move that file somewhere else, then reconnect your accounts."
    )]
    Corrupt { path: String },
}

pub trait Secrets: Send + Sync + fmt::Debug {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    /// Removes a secret; removing one that isn't there is fine.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// The OS keychain (Windows Credential Manager, Linux Secret Service). Not used on macOS:
/// see the crate docs.
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Default)]
pub struct Keychain;

#[cfg(not(target_os = "macos"))]
impl Keychain {
    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, key).map_err(|e| match e {
            keyring::Error::NoDefaultStore => SecretError::Unavailable(e.to_string()),
            other => SecretError::Keychain(other.to_string()),
        })
    }

    /// Whether the keychain works here (headless Linux servers often have none).
    pub fn available() -> bool {
        keyring::Entry::store_status().is_ok()
    }
}

#[cfg(not(target_os = "macos"))]
impl Secrets for Keychain {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        Self::entry(key)?.set_password(value).map_err(|e| SecretError::Keychain(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Keychain(e.to_string())),
        }
    }
}

/// Secrets in one private JSON file (`{ "key": "value", … }`), used on macOS.
///
/// - The folder is created `0700` (and tightened to it), the file is `0600`.
/// - Loaded on first use, then kept in memory behind a mutex.
/// - Every change is written atomically: a temp file in the same folder, fsync, rename,
///   then fsync of the folder, so a crash leaves either the old file or the new one.
/// - A damaged file is reported ([`SecretError::Corrupt`]) and never overwritten: a
///   streamer's tokens are not silently wiped.
/// - Values never appear in `Debug`, errors or logs.
pub struct FileSecrets {
    dir: PathBuf,
    path: PathBuf,
    /// `None` until the file has been read.
    cache: Mutex<Option<BTreeMap<String, String>>>,
}

impl fmt::Debug for FileSecrets {
    // Never print the values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileSecrets({})", self.path.display())
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FileSecrets {
    /// Secrets in `dir/secrets.json`. Nothing is read or created until first use.
    pub fn new(dir: &Path) -> Self {
        FileSecrets { dir: dir.to_path_buf(), path: dir.join(FILE_NAME), cache: Mutex::new(None) }
    }

    /// Where the file lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file_err(&self, e: &std::io::Error) -> SecretError {
        SecretError::File { path: self.path.display().to_string(), why: e.to_string() }
    }

    fn corrupt(&self) -> SecretError {
        SecretError::Corrupt { path: self.path.display().to_string() }
    }

    fn read(&self) -> Result<BTreeMap<String, String>, SecretError> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(self.file_err(&e)),
        };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return Err(self.corrupt());
        }
        let bytes = std::fs::read(&self.path).map_err(|e| self.file_err(&e))?;
        let map: BTreeMap<String, String> = serde_json::from_slice(&bytes).map_err(|_| self.corrupt())?;
        // A file someone loosened by hand is tightened again (best effort).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(map)
    }

    fn ensure_dir(&self) -> Result<(), SecretError> {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&self.dir).map_err(|e| self.file_err(&e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&self.dir).map_err(|e| self.file_err(&e))?.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| self.file_err(&e))?;
            }
        }
        Ok(())
    }

    /// Writes the whole map atomically (temp file, fsync, rename, fsync the folder).
    fn write(&self, map: &BTreeMap<String, String>) -> Result<(), SecretError> {
        self.ensure_dir()?;
        let body = serde_json::to_vec_pretty(map).map_err(|_| self.corrupt())?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!(".{FILE_NAME}.{}.{n}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
                std::fs::File::open(&self.dir)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.file_err(&e));
        }
        Ok(())
    }

    /// Runs `f` on the loaded map, holding the lock (so concurrent writers never lose an
    /// update). Nothing is cached when the file can't be read, so a fixed file is picked
    /// up on the next call.
    fn with<T>(
        &self,
        f: impl FnOnce(&Self, &mut BTreeMap<String, String>) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let mut guard = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(self.read()?);
        }
        match guard.as_mut() {
            Some(map) => f(self, map),
            None => Err(self.corrupt()),
        }
    }
}

impl Secrets for FileSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        self.with(|_, m| Ok(m.get(key).cloned()))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.with(|s, m| {
            if m.get(key).map(String::as_str) == Some(value) {
                return Ok(());
            }
            let mut next = m.clone();
            next.insert(key.to_string(), value.to_string());
            s.write(&next)?;
            *m = next;
            Ok(())
        })
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.with(|s, m| {
            if !m.contains_key(key) {
                return Ok(());
            }
            let mut next = m.clone();
            next.remove(key);
            s.write(&next)?;
            *m = next;
            Ok(())
        })
    }
}

/// Secrets kept in memory for one run (tests, or a machine without a keychain, where the
/// streamer signs in again after a restart rather than us writing tokens to disk).
#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
}

impl fmt::Debug for MemorySecrets {
    // Never print the values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "MemorySecrets({n} entries)")
    }
}

impl Secrets for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.map.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
        Ok(())
    }
}

/// The right store for this OS: the private file in `data_dir` on macOS; the keychain on
/// Windows and Linux when it works, otherwise memory (with a warning in the log).
pub fn best_available(data_dir: &Path) -> Box<dyn Secrets> {
    #[cfg(target_os = "macos")]
    {
        Box::new(FileSecrets::new(data_dir))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = data_dir;
        if Keychain::available() {
            Box::new(Keychain)
        } else {
            tracing_warn("no OS keychain; secrets will be kept in memory for this run only");
            Box::new(MemorySecrets::default())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn tracing_warn(msg: &str) {
    eprintln!("ferrocast: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip_and_never_prints_values() {
        let s = MemorySecrets::default();
        assert_eq!(s.get("twitch").unwrap(), None);
        s.set("twitch", "super-secret-token").unwrap();
        assert_eq!(s.get("twitch").unwrap().as_deref(), Some("super-secret-token"));
        assert!(!format!("{s:?}").contains("super-secret"));
        s.delete("twitch").unwrap();
        s.delete("twitch").unwrap();
        assert_eq!(s.get("twitch").unwrap(), None);
    }

    #[test]
    fn file_store_round_trips_across_restarts_and_deletes() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("Ferrocast");
        let s = FileSecrets::new(&dir);
        assert_eq!(s.get("twitch").unwrap(), None);
        assert!(!dir.exists(), "reading creates nothing");
        s.set("twitch", "super-secret-token").unwrap();
        s.set("obs-password", "pw").unwrap();
        assert_eq!(s.get("twitch").unwrap().as_deref(), Some("super-secret-token"));
        assert!(!format!("{s:?}").contains("super-secret"));

        // A fresh instance (the next start) reads the same file.
        let again = FileSecrets::new(&dir);
        assert_eq!(again.get("twitch").unwrap().as_deref(), Some("super-secret-token"));
        again.delete("twitch").unwrap();
        again.delete("twitch").unwrap();
        assert_eq!(again.get("twitch").unwrap(), None);
        assert_eq!(FileSecrets::new(&dir).get("obs-password").unwrap().as_deref(), Some("pw"));
        assert_eq!(FileSecrets::new(&dir).get("twitch").unwrap(), None);
        // No temp files left behind.
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from(FILE_NAME)]);
    }

    #[cfg(unix)]
    #[test]
    fn file_and_folder_are_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("Ferrocast");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = FileSecrets::new(&dir);
        s.set("k", "v").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "the folder is tightened");
        assert_eq!(mode(s.path()), 0o600);
        // Loosened by hand: tightened again on the next read.
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(FileSecrets::new(&dir).get("k").unwrap().as_deref(), Some("v"));
        assert_eq!(mode(s.path()), 0o600);
    }

    #[test]
    fn a_damaged_file_is_an_error_and_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join(FILE_NAME);
        for junk in [&b"{not json"[..], b"[]", br#"{"k": 1}"#, b""] {
            std::fs::write(&path, junk).unwrap();
            let s = FileSecrets::new(d.path());
            for r in [s.get("k").map(|_| ()), s.set("k", "secret-value"), s.delete("k")] {
                let e = r.unwrap_err();
                assert!(matches!(e, SecretError::Corrupt { .. }), "{junk:?}: {e}");
                assert!(e.to_string().contains("damaged") && !e.to_string().contains("secret-value"));
            }
            assert_eq!(std::fs::read(&path).unwrap(), junk, "left exactly as it was");
        }
        // Once fixed, it works without a restart.
        std::fs::write(&path, br#"{"k":"v"}"#).unwrap();
        let s = FileSecrets::new(d.path());
        std::fs::write(&path, b"{oops").unwrap();
        assert!(s.get("k").is_err());
        std::fs::write(&path, br#"{"k":"v"}"#).unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn concurrent_sets_all_land() {
        let d = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(FileSecrets::new(d.path()));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        s.set(&format!("k{i}-{j}"), &format!("v{i}-{j}")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let fresh = FileSecrets::new(d.path());
        for i in 0..16 {
            for j in 0..5 {
                assert_eq!(fresh.get(&format!("k{i}-{j}")).unwrap(), Some(format!("v{i}-{j}")));
            }
        }
    }
}
