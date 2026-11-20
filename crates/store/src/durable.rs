//! Atomic, durable JSON persistence with `.bak` fallback and Windows sharing retries.
//!
//! Writes go to a sibling temp file, call `File::sync_all()`, preserve the previous valid
//! primary file as `<path>.bak` (itself written atomically), rename into place with exponential
//! backoff on transient Windows AV/indexer/sharing lock errors, and sync the parent directory on
//! Unix.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Initial retry wait and attempt count for transient file locks (total wait ~1.9 s).
const RETRY_BASE: Duration = Duration::from_millis(15);
const RETRY_COUNT: usize = 7;

/// Result of [`load_json_with_backup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded<T> {
    pub value: T,
    pub from_backup: bool,
}

/// Companion backup path (`<path>.bak`) for any config, collection, or session file.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(".bak");
    PathBuf::from(os)
}

fn temp_path(path: &Path, tag: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(format!(".{pid}.{seq}.{tag}"));
    PathBuf::from(os)
}

/// Returns `true` when an I/O error is likely a transient lock held by Windows Defender, Search
/// Indexer, or another reader racing a file swap.
fn is_transient_lock_err(e: &io::Error) -> bool {
    if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy | io::ErrorKind::WouldBlock) {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32),
        // ERROR_LOCK_VIOLATION (33), ERROR_USER_MAPPED_FILE (1224).
        if matches!(e.raw_os_error(), Some(5 | 32 | 33 | 1224)) {
            return true;
        }
    }
    let _ = e;
    false
}

fn retry_with<T>(mut op: impl FnMut() -> io::Result<T>, mut sleep: impl FnMut(Duration)) -> io::Result<T> {
    let mut wait = RETRY_BASE;
    for _ in 0..RETRY_COUNT {
        match op() {
            Err(e) if is_transient_lock_err(&e) => {
                sleep(wait);
                wait = wait.saturating_mul(2);
            }
            other => return other,
        }
    }
    op()
}

/// Retries a file operation with exponential backoff on transient sharing/permission errors.
pub fn retry_on_sharing<T>(op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with(op, std::thread::sleep)
}

/// Renames `from` over `to`, retrying on transient Windows AV/sharing violations.
pub fn rename_patiently(from: &Path, to: &Path) -> io::Result<()> {
    retry_on_sharing(|| std::fs::rename(from, to))
}

/// Reads `path`, retrying on transient sharing violations.
pub fn read_patiently(path: &Path) -> io::Result<Vec<u8>> {
    retry_on_sharing(|| std::fs::read(path))
}

/// Removes `path` (if present), retrying on transient sharing violations.
pub fn remove_patiently(path: &Path) -> io::Result<()> {
    match retry_on_sharing(|| std::fs::remove_file(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes both `path` and its `.bak` sibling so deleted ephemeral files cannot revive via fallback.
pub fn remove_with_backup(path: &Path) {
    for target in [path, &backup_path(path)] {
        if let Err(e) = remove_patiently(target) {
            tracing::debug!(error = %e, file = %target.display(), "couldn't remove file");
        }
    }
}

fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
            && let Ok(f) = std::fs::File::open(dir)
        {
            let _ = f.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Writes `bytes` to `path` via temp file + `sync_all` + patient atomic rename.
pub fn write_atomic_raw(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = temp_path(path, "tmp");
    let res = (|| -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        rename_patiently(&tmp, path)?;
        sync_parent_dir(path);
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn is_valid_json(bytes: &[u8]) -> bool {
    !bytes.is_empty() && serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok()
}

/// Atomically saves serialized JSON `bytes` to `path`, keeping `<path>.bak` of the previous good
/// JSON file before overwriting.
pub fn write_json_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Only snapshot an existing valid JSON primary to `.bak` so a corrupt primary never clobbers a
    // good backup.
    if let Ok(existing) = read_patiently(path)
        && is_valid_json(&existing)
        && let Err(e) = write_atomic_raw(&backup_path(path), &existing)
    {
        tracing::warn!(error = %e, file = %path.display(), "couldn't update .bak before saving");
    }
    write_atomic_raw(path, bytes)
}

/// Atomically saves pretty-printed JSON with `.bak` preservation.
pub fn save_json_pretty<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_json_bytes(path, &bytes)
}

/// Atomically saves compact JSON with `.bak` preservation.
pub fn save_json_compact<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    write_json_bytes(path, &bytes)
}

fn read_checked_json<T: serde::de::DeserializeOwned>(path: &Path, valid: &impl Fn(&T) -> bool) -> Option<T> {
    let bytes = match read_patiently(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "couldn't read JSON file");
            return None;
        }
    };
    if bytes.is_empty() {
        tracing::warn!(file = %path.display(), "JSON file is empty");
        return None;
    }
    match serde_json::from_slice::<T>(&bytes) {
        Ok(v) if valid(&v) => Some(v),
        Ok(_) => {
            tracing::warn!(file = %path.display(), "JSON file failed validation");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, file = %path.display(), "JSON file is unreadable or truncated");
            None
        }
    }
}

/// Loads `path` as `T`, falling back to `<path>.bak` when `path` is missing, unreadable,
/// truncated, or fails `valid`.
pub fn load_json_with_backup<T: serde::de::DeserializeOwned>(
    path: &Path,
    valid: impl Fn(&T) -> bool,
) -> Option<Loaded<T>> {
    if let Some(value) = read_checked_json(path, &valid) {
        return Some(Loaded { value, from_backup: false });
    }
    let bak = backup_path(path);
    if let Some(value) = read_checked_json(&bak, &valid) {
        tracing::warn!(file = %path.display(), backup = %bak.display(), "loaded JSON fallback from .bak");
        return Some(Loaded { value, from_backup: true });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Sample {
        version: u32,
        name: String,
    }

    #[test]
    fn truncated_main_file_loads_bak() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("settings.json");

        let v1 = Sample { version: 1, name: "good-v1".into() };
        let v2 = Sample { version: 2, name: "good-v2".into() };

        save_json_pretty(&path, &v1).unwrap_or_else(|e| panic!("{e}"));
        assert!(!backup_path(&path).exists(), "no prior file existed on first save");

        save_json_pretty(&path, &v2).unwrap_or_else(|e| panic!("{e}"));
        assert!(backup_path(&path).exists(), "second save preserves last good file in .bak");

        // Truncate the main file mid-JSON to simulate a torn write / corruption.
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap_or_else(|e| panic!("{e}"));

        let loaded: Loaded<Sample> = load_json_with_backup(&path, |s: &Sample| s.version > 0)
            .unwrap_or_else(|| panic!("expected .bak fallback to load"));
        assert_eq!(loaded, Loaded { value: v1.clone(), from_backup: true });

        // Saving a repaired v3 while main is corrupt must not overwrite .bak until primary recovers.
        let corrupt = std::fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(!is_valid_json(&corrupt));
        let v3 = Sample { version: 3, name: "repaired-v3".into() };
        save_json_pretty(&path, &v3).unwrap_or_else(|e| panic!("{e}"));
        let bak_raw = std::fs::read(backup_path(&path)).unwrap_or_else(|e| panic!("{e}"));
        let bak_val: Sample = serde_json::from_slice(&bak_raw).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(bak_val, v1, "corrupt primary never overwrote valid .bak");

        let reloaded: Loaded<Sample> =
            load_json_with_backup(&path, |_| true).unwrap_or_else(|| panic!("expected repaired primary to load"));
        assert_eq!(reloaded, Loaded { value: v3, from_backup: false });
    }

    #[test]
    fn empty_or_invalid_primary_falls_back_to_bak() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("collection.json");

        let v1 = Sample { version: 10, name: "valid".into() };
        let v2 = Sample { version: 20, name: "next".into() };
        save_json_pretty(&path, &v1).unwrap_or_else(|e| panic!("{e}"));
        save_json_pretty(&path, &v2).unwrap_or_else(|e| panic!("{e}"));

        // Empty main file falls back to .bak (v1).
        std::fs::write(&path, b"").unwrap_or_else(|e| panic!("{e}"));
        let loaded: Loaded<Sample> =
            load_json_with_backup(&path, |_| true).unwrap_or_else(|| panic!("expected .bak on empty file"));
        assert_eq!(loaded, Loaded { value: v1, from_backup: true });

        // Validation predicate failure also falls back to .bak after a valid save.
        save_json_pretty(&path, &v2).unwrap_or_else(|e| panic!("{e}"));
        let zero = Sample { version: 0, name: "invalid".into() };
        save_json_pretty(&path, &zero).unwrap_or_else(|e| panic!("{e}"));
        let validated: Loaded<Sample> = load_json_with_backup(&path, |s: &Sample| s.version > 0)
            .unwrap_or_else(|| panic!("expected .bak on validation failure"));
        assert_eq!(validated.value.version, 20);
        assert!(validated.from_backup);
    }

    #[test]
    fn retries_on_transient_sharing_violations_and_removes_backup_on_delete() {
        let mut attempts = 0usize;
        let mut sleeps = Vec::new();
        let out = retry_with(
            || {
                attempts += 1;
                if attempts <= 3 { Err(io::Error::from(io::ErrorKind::PermissionDenied)) } else { Ok("renamed") }
            },
            |d| sleeps.push(d),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out, "renamed");
        assert_eq!(attempts, 4);
        assert_eq!(sleeps.len(), 3);

        // Hard errors (e.g. NotFound) must not retry.
        let mut hard_attempts = 0usize;
        let err = retry_with(
            || -> io::Result<()> {
                hard_attempts += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            |_| panic!("unexpected sleep on non-transient error"),
        );
        assert!(err.is_err());
        assert_eq!(hard_attempts, 1);

        // Exhausting transient retries returns the error cleanly.
        let mut exhausted = 0usize;
        let mut exhausted_sleeps = 0usize;
        let err = retry_with(
            || -> io::Result<()> {
                exhausted += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
            |_| exhausted_sleeps += 1,
        );
        assert!(err.is_err());
        assert_eq!(exhausted, RETRY_COUNT + 1);
        assert_eq!(exhausted_sleeps, RETRY_COUNT);

        // Removing a file deletes both primary and .bak.
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let session = dir.path().join("session.json");
        save_json_pretty(&session, &serde_json::json!({"live": true})).unwrap_or_else(|e| panic!("{e}"));
        save_json_pretty(&session, &serde_json::json!({"live": false})).unwrap_or_else(|e| panic!("{e}"));
        assert!(session.exists() && backup_path(&session).exists());
        remove_with_backup(&session);
        assert!(!session.exists() && !backup_path(&session).exists());
    }
}
