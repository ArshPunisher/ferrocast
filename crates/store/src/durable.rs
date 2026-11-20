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
    if matches!(e.kind(), io::ErrorKind::ResourceBusy | io::ErrorKind::WouldBlock) {
        return true;
    }
    // "Permission denied" is only a passing lock on Windows (a scanner or the indexer has the file
    // open). Elsewhere it is a folder or file the user can't write, which no wait mends: retrying
    // it stalled every save by ~1.9 s, on the Studio's main thread, in a read-only data folder.
    #[cfg(windows)]
    {
        if e.kind() == io::ErrorKind::PermissionDenied {
            return true;
        }
        // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32),
        // ERROR_LOCK_VIOLATION (33), ERROR_USER_MAPPED_FILE (1224).
        if matches!(e.raw_os_error(), Some(5 | 32 | 33 | 1224)) {
            return true;
        }
    }
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
    queue::wait_idle(from);
    retry_on_sharing(|| std::fs::rename(from, to))
}

/// Reads `path`, retrying on transient sharing violations.
pub fn read_patiently(path: &Path) -> io::Result<Vec<u8>> {
    // A save queued but not yet on disk is what the file means: read that, not the older bytes.
    if let Some(bytes) = queue::pending_bytes(path) {
        return Ok(bytes);
    }
    retry_on_sharing(|| std::fs::read(path))
}

/// Removes `path` (if present), retrying on transient sharing violations.
pub fn remove_patiently(path: &Path) -> io::Result<()> {
    queue::wait_idle(path);
    match retry_on_sharing(|| std::fs::remove_file(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes both `path` and its `.bak` sibling so deleted ephemeral files cannot revive via fallback.
pub fn remove_with_backup(path: &Path) {
    queue::wait_idle(path);
    for target in [path, &backup_path(path)] {
        if let Err(e) = remove_patiently(target) {
            tracing::warn!(error = %e, file = %target.display(), "couldn't remove file");
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
        let _ = remove_patiently(&tmp);
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
    if let Ok(existing) = retry_on_sharing(|| std::fs::read(path))
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

pub use queue::{SaveError, flush_saves, take_save_errors};

/// Queues `bytes` to be saved to `path` (atomic, with `.bak`, same as [`write_json_bytes`]) on a
/// background thread, and returns at once. The UI/async caller never waits out the Windows
/// lock retries (up to ~1.9 s per file operation, ~5.7 s per save).
///
/// Saves of one path land in order and a newer one replaces an older one still waiting, so the
/// last call wins. A failure is logged and kept for [`take_save_errors`] (never swallowed);
/// [`read_patiently`] and [`load_json_with_backup`] see queued bytes at once.
pub fn queue_json_bytes(path: &Path, bytes: Vec<u8>) {
    queue::submit(path, bytes, write_json_bytes);
}

/// [`queue_json_bytes`] for a serializable value; serializing errors are returned now.
pub fn queue_json_pretty<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    queue_json_bytes(path, serde_json::to_vec_pretty(value).map_err(io::Error::other)?);
    Ok(())
}

mod queue {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

    /// A save that failed after every retry.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SaveError {
        pub file: PathBuf,
        pub message: String,
    }

    type WriteFn = fn(&Path, &[u8]) -> io::Result<()>;

    struct Entry {
        generation: u64,
        bytes: Vec<u8>,
        write: WriteFn,
    }

    #[derive(Default)]
    struct State {
        pending: HashMap<PathBuf, Entry>,
        order: VecDeque<PathBuf>,
        errors: HashMap<PathBuf, String>,
        next_generation: u64,
        worker_started: bool,
    }

    struct Shared {
        state: Mutex<State>,
        /// Signalled when work arrives (worker) and when a save finishes (waiters).
        changed: Condvar,
    }

    fn shared() -> &'static Shared {
        static S: OnceLock<Shared> = OnceLock::new();
        S.get_or_init(|| Shared { state: Mutex::new(State::default()), changed: Condvar::new() })
    }

    fn lock(s: &Shared) -> MutexGuard<'_, State> {
        s.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn submit(path: &Path, bytes: Vec<u8>, write: WriteFn) {
        let s = shared();
        let mut st = lock(s);
        st.next_generation += 1;
        let generation = st.next_generation;
        st.pending.insert(path.to_path_buf(), Entry { generation, bytes, write });
        st.order.push_back(path.to_path_buf());
        if !st.worker_started {
            st.worker_started = true;
            let spawned = std::thread::Builder::new().name("ferrocast-save".into()).spawn(move || worker(s));
            if spawned.is_err() {
                // No thread: save here rather than lose it.
                st.worker_started = false;
                drop(st);
                run_one(s);
                return;
            }
        }
        drop(st);
        s.changed.notify_all();
    }

    fn worker(s: &'static Shared) {
        loop {
            {
                let mut st = lock(s);
                while st.order.is_empty() {
                    st = s.changed.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            run_one(s);
        }
    }

    /// Takes the oldest queued path and saves its newest bytes, outside the lock.
    fn run_one(s: &Shared) {
        let (path, generation, bytes, write) = {
            let mut st = lock(s);
            let Some(path) = st.order.pop_front() else { return };
            let Some(e) = st.pending.get(&path) else { return };
            (path, e.generation, e.bytes.clone(), e.write)
        };
        let result = write(&path, &bytes);
        let mut st = lock(s);
        match result {
            Ok(()) => {
                st.errors.remove(&path);
            }
            Err(e) => {
                // No file contents or user data: just the path and the OS error.
                tracing::error!(error = %e, file = %path.display(), "a queued save failed after retrying");
                st.errors
                    .insert(path.clone(), format!("Couldn't save: {e}. The file may be in use by another program."));
            }
        }
        // Only forget the bytes if nothing newer arrived while writing.
        if st.pending.get(&path).is_some_and(|e| e.generation == generation) {
            st.pending.remove(&path);
        }
        drop(st);
        s.changed.notify_all();
    }

    pub(super) fn pending_bytes(path: &Path) -> Option<Vec<u8>> {
        lock(shared()).pending.get(path).map(|e| e.bytes.clone())
    }

    /// Blocks until `path` has no save queued or running (a no-op when it never had one).
    pub(super) fn wait_idle(path: &Path) {
        let s = shared();
        let mut st = lock(s);
        while st.pending.contains_key(path) {
            st = s.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits up to `limit` for every queued save to finish; `true` when all did.
    pub fn flush_saves(limit: Duration) -> bool {
        let s = shared();
        let deadline = std::time::Instant::now() + limit;
        let mut st = lock(s);
        while !st.pending.is_empty() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else { return false };
            st = s.changed.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// Failures since the last call (cleared when a later save of that file succeeds).
    pub fn take_save_errors() -> Vec<SaveError> {
        let mut st = lock(shared());
        st.errors.drain().map(|(file, message)| SaveError { file, message }).collect()
    }
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
                if attempts <= 3 { Err(io::Error::from(io::ErrorKind::ResourceBusy)) } else { Ok("renamed") }
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
                Err(io::Error::from(io::ErrorKind::ResourceBusy))
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

    /// A folder the user can't write to fails at once (it used to wait out every retry first).
    #[cfg(not(windows))]
    #[test]
    fn permission_denied_is_not_retried_off_windows() {
        let mut attempts = 0usize;
        let err = retry_with(
            || -> io::Result<()> {
                attempts += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
            |_| panic!("waited on a permission error no wait will mend"),
        );
        assert!(err.is_err());
        assert_eq!(attempts, 1);
    }

    /// On Windows the same error is a scanner's or the indexer's lock and passes: retried.
    #[cfg(windows)]
    #[test]
    fn permission_denied_is_retried_on_windows() {
        let mut attempts = 0usize;
        let out = retry_with(
            || {
                attempts += 1;
                if attempts < 3 { Err(io::Error::from(io::ErrorKind::PermissionDenied)) } else { Ok(()) }
            },
            |_| {},
        );
        assert!(out.is_ok());
        assert_eq!(attempts, 3);
        for code in [5, 32, 33, 1224] {
            assert!(is_transient_lock_err(&io::Error::from_raw_os_error(code)), "Win32 error {code} must retry");
        }
    }

    /// Real Windows file locking: renaming over a file held open without `FILE_SHARE_DELETE`
    /// fails with `ERROR_SHARING_VIOLATION` until the reader closes it, and `rename_patiently`
    /// retries across that window and replaces the existing file atomically.
    #[cfg(windows)]
    #[test]
    fn rename_patiently_replaces_existing_file_and_retries_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let target = dir.path().join("settings.json");
        let staged = dir.path().join("settings.json.tmp");
        std::fs::write(&target, b"{\"v\":1}").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&staged, b"{\"v\":2}").unwrap_or_else(|e| panic!("{e}"));

        // Hold target open with share_mode(0) (no delete/write/read sharing) like an AV scanner.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&target).unwrap_or_else(|e| panic!("{e}"));
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(lock);
        });

        rename_patiently(&staged, &target).unwrap_or_else(|e| panic!("expected retry to succeed: {e}"));
        let _ = holder.join();
        assert_eq!(std::fs::read(&target).unwrap_or_else(|e| panic!("{e}")), b"{\"v\":2}");
        assert!(!staged.exists());
    }

    fn slow_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::thread::sleep(Duration::from_millis(300));
        write_json_bytes(path, bytes)
    }

    fn locked_write(_: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::ResourceBusy))
    }

    /// The caller of a queued save is not held up by a slow (locked, retrying) write, and the
    /// queued bytes are what a read sees meanwhile.
    #[test]
    fn queued_save_returns_promptly_while_the_write_retries_off_thread() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("scenes.json");
        let started = std::time::Instant::now();
        queue::submit(&path, br#"{"v":1}"#.to_vec(), slow_write);
        assert!(started.elapsed() < Duration::from_millis(150), "the caller waited on the write");
        assert_eq!(read_patiently(&path).unwrap_or_else(|e| panic!("{e}")), br#"{"v":1}"#);
        assert!(!path.exists(), "the slow write hasn't landed yet");
        assert!(flush_saves(Duration::from_secs(10)));
        assert_eq!(std::fs::read(&path).unwrap_or_else(|e| panic!("{e}")), br#"{"v":1}"#);
    }

    /// Two quick saves of one file end with the newer content, and the older good file is the .bak.
    #[test]
    fn rapid_saves_of_one_file_end_with_the_newest() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("scenes.json");
        write_json_bytes(&path, br#"{"v":0}"#).unwrap_or_else(|e| panic!("{e}"));
        queue::submit(&path, br#"{"v":1}"#.to_vec(), slow_write);
        for v in 2..=20 {
            queue_json_bytes(&path, format!(r#"{{"v":{v}}}"#).into_bytes());
        }
        assert!(flush_saves(Duration::from_secs(20)));
        assert_eq!(std::fs::read(&path).unwrap_or_else(|e| panic!("{e}")), br#"{"v":20}"#);
        assert!(is_valid_json(&std::fs::read(backup_path(&path)).unwrap_or_else(|e| panic!("{e}"))));
    }

    /// A save that never succeeds is reported, not swallowed, and the message carries no content;
    /// a later good save clears it.
    #[test]
    fn a_failed_queued_save_is_surfaced() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("secret-name.json");
        queue::submit(&path, b"{\"password\":\"hunter2\"}".to_vec(), locked_write);
        assert!(flush_saves(Duration::from_secs(10)));
        let errs = take_save_errors();
        let mine: Vec<_> = errs.iter().filter(|e| e.file == path).collect();
        assert_eq!(mine.len(), 1);
        assert!(mine[0].message.starts_with("Couldn't save:"));
        assert!(!mine[0].message.contains("hunter2"));
        assert!(!path.exists(), "nothing half-written landed");
    }
}
