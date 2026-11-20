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
    if let Some(queued) = queue::pending_read(path) {
        return queued;
    }
    retry_on_sharing(|| std::fs::read(path))
}

/// Removes `path` (if present), retrying on transient sharing violations.
pub fn remove_patiently(path: &Path) -> io::Result<()> {
    queue::wait_idle(path);
    remove_patiently_now(path)
}

fn remove_patiently_now(path: &Path) -> io::Result<()> {
    match retry_on_sharing(|| std::fs::remove_file(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes both `path` and its `.bak` sibling so deleted ephemeral files cannot revive via fallback.
pub fn remove_with_backup(path: &Path) {
    queue::wait_idle(path);
    remove_with_backup_now(path);
}

fn remove_with_backup_now(path: &Path) {
    for target in [path, &backup_path(path)] {
        if let Err(e) = remove_patiently_now(target) {
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

pub use queue::{SaveError, SaveEvent, SaveFailure, flush_saves, set_save_observer, take_save_errors};

/// Queues `bytes` to be saved to `path` (atomic, with `.bak`, same as [`write_json_bytes`]) on a
/// background thread, and returns at once. The UI/async caller never waits out the Windows
/// lock retries (up to ~1.9 s per file operation, ~5.7 s per save).
///
/// Saves of one path land in order and a newer one replaces an older one still waiting, so the
/// last call wins. A failure is logged, kept for [`take_save_errors`] and sent to the
/// [`set_save_observer`] (never swallowed); [`read_patiently`] and [`load_json_with_backup`]
/// see queued bytes at once.
pub fn queue_json_bytes(path: &Path, bytes: Vec<u8>) {
    queue::submit(path, queue::Op::Write { bytes, write: write_json_bytes });
}

/// [`queue_json_bytes`] for a serializable value; serializing errors are returned now.
pub fn queue_json_pretty<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    queue_json_bytes(path, serde_json::to_vec_pretty(value).map_err(io::Error::other)?);
    Ok(())
}

/// [`remove_with_backup`] on the background writer, in order with the saves of the same file.
/// Reads see the file as gone at once.
pub fn queue_remove_with_backup(path: &Path) {
    queue::submit(path, queue::Op::Remove);
}

mod queue {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

    /// A save that failed after every retry.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SaveError {
        pub file: PathBuf,
        pub message: String,
    }

    /// Why a queued save failed, without free text: an OS error's own words can carry a path
    /// (the user's folder name), and this goes to dashboards.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SaveFailure {
        pub kind: io::ErrorKind,
        pub raw_os_error: Option<i32>,
    }

    impl SaveFailure {
        fn of(e: &io::Error) -> Self {
            Self { kind: e.kind(), raw_os_error: e.raw_os_error() }
        }

        /// Safe to show: the error kind and the OS error number, nothing else.
        pub fn describe(&self) -> String {
            match self.raw_os_error {
                Some(n) => format!("{} (os error {n})", self.kind),
                None => self.kind.to_string(),
            }
        }
    }

    /// A queued file went from saving to failing or back (`None`). Sent once per change (a new
    /// kind of failure counts as a change), not once per attempt.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SaveEvent {
        pub file: PathBuf,
        pub error: Option<SaveFailure>,
    }

    type WriteFn = fn(&Path, &[u8]) -> io::Result<()>;
    type Observer = Box<dyn Fn(&SaveEvent) + Send + Sync>;

    #[derive(Clone)]
    pub(super) enum Op {
        Write { bytes: Vec<u8>, write: WriteFn },
        Remove,
    }

    struct Entry {
        generation: u64,
        op: Op,
    }

    #[derive(Default)]
    struct State {
        pending: HashMap<PathBuf, Entry>,
        order: VecDeque<PathBuf>,
        errors: HashMap<PathBuf, String>,
        failing: HashMap<PathBuf, SaveFailure>,
        next_generation: u64,
        worker_started: bool,
    }

    struct Shared {
        state: Mutex<State>,
        /// Signalled when work arrives (worker) and when a save finishes (waiters).
        changed: Condvar,
        observer: Mutex<Option<std::sync::Arc<Observer>>>,
    }

    fn shared() -> &'static Shared {
        static S: OnceLock<Shared> = OnceLock::new();
        S.get_or_init(|| Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            observer: Mutex::new(None),
        })
    }

    fn lock(s: &Shared) -> MutexGuard<'_, State> {
        s.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where failures and recoveries go (the engine turns them into a dashboard notice). Runs on
    /// the writer thread: keep it quick.
    pub fn set_save_observer(f: impl Fn(&SaveEvent) + Send + Sync + 'static) {
        *shared().observer.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(Box::new(f)));
    }

    pub(super) fn submit(path: &Path, op: Op) {
        let s = shared();
        let mut st = lock(s);
        st.next_generation += 1;
        let generation = st.next_generation;
        st.pending.insert(path.to_path_buf(), Entry { generation, op });
        st.order.push_back(path.to_path_buf());
        if !st.worker_started {
            st.worker_started = true;
            if std::thread::Builder::new().name("ferrocast-save".into()).spawn(move || worker(s)).is_err() {
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

    /// Takes the oldest queued path and does its newest operation, outside the lock. A panic in
    /// the write is caught and becomes a failed save, so the worker never dies.
    fn run_one(s: &Shared) {
        let (path, generation, op) = {
            let mut st = lock(s);
            let Some(path) = st.order.pop_front() else { return };
            let Some(e) = st.pending.get(&path) else { return };
            (path, e.generation, e.op.clone())
        };
        let result = catch_unwind(AssertUnwindSafe(|| match &op {
            Op::Write { bytes, write } => write(&path, bytes),
            Op::Remove => {
                remove_with_backup_now(&path);
                Ok(())
            }
        }))
        .unwrap_or_else(|_| Err(io::Error::other("the save stopped unexpectedly")));
        let mut event = None;
        let mut st = lock(s);
        match result {
            Ok(()) => {
                st.errors.remove(&path);
                if st.failing.remove(&path).is_some() {
                    event = Some(SaveEvent { file: path.clone(), error: None });
                }
            }
            Err(e) => {
                // No file contents or user data: just the path and the OS error.
                tracing::error!(error = %e, file = %path.display(), "a queued save failed after retrying");
                st.errors
                    .insert(path.clone(), format!("Couldn't save: {e}. The file may be in use by another program."));
                let failure = SaveFailure::of(&e);
                if st.failing.insert(path.clone(), failure) != Some(failure) {
                    event = Some(SaveEvent { file: path.clone(), error: Some(failure) });
                }
            }
        }
        drop(st);
        // Told before the operation counts as finished, so whoever flushes sees the notice too.
        if let Some(ev) = event {
            let observer = s.observer.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(f) = observer {
                // A panicking observer must not kill the writer either.
                let _ = catch_unwind(AssertUnwindSafe(|| f(&ev)));
            }
        }
        // Only forget the operation if nothing newer arrived while it ran.
        let mut st = lock(s);
        if st.pending.get(&path).is_some_and(|e| e.generation == generation) {
            st.pending.remove(&path);
        }
        drop(st);
        s.changed.notify_all();
    }

    /// What a read of `path` should see while an operation on it is queued.
    pub(super) fn pending_read(path: &Path) -> Option<io::Result<Vec<u8>>> {
        match &lock(shared()).pending.get(path)?.op {
            Op::Write { bytes, .. } => Some(Ok(bytes.clone())),
            Op::Remove => Some(Err(io::Error::from(io::ErrorKind::NotFound))),
        }
    }

    /// `true` when a removal of `path` is queued (its `.bak` must not bring it back).
    pub(super) fn removal_pending(path: &Path) -> bool {
        matches!(lock(shared()).pending.get(path), Some(Entry { op: Op::Remove, .. }))
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
    // Queued for removal: gone, and its .bak (still on disk for now) must not bring it back.
    if queue::removal_pending(path) {
        return None;
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

    /// The two tests that drain `take_save_errors` (a shared list) must not overlap.
    static ERRORS_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        submit_with(&path, br#"{"v":1}"#.to_vec(), slow_write);
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
        submit_with(&path, br#"{"v":1}"#.to_vec(), slow_write);
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
        let _guard = ERRORS_TEST.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("secret-name.json");
        submit_with(&path, b"{\"password\":\"hunter2\"}".to_vec(), locked_write);
        assert!(flush_saves(Duration::from_secs(10)));
        let errs = take_save_errors();
        let mine: Vec<_> = errs.iter().filter(|e| e.file == path).collect();
        assert_eq!(mine.len(), 1);
        assert!(mine[0].message.starts_with("Couldn't save:"));
        assert!(!mine[0].message.contains("hunter2"));
        assert!(!path.exists(), "nothing half-written landed");
    }

    fn submit_with(path: &Path, bytes: Vec<u8>, write: fn(&Path, &[u8]) -> io::Result<()>) {
        queue::submit(path, queue::Op::Write { bytes, write });
    }

    fn panicking_write(_: &Path, _: &[u8]) -> io::Result<()> {
        panic!("injected write panic");
    }

    /// A write that panics is a failed save, not a dead writer: later saves still land.
    #[test]
    fn a_panicking_write_is_a_failed_save_and_the_worker_lives() {
        let _guard = ERRORS_TEST.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let bad = dir.path().join("bad.json");
        let good = dir.path().join("good.json");
        submit_with(&bad, b"{}".to_vec(), panicking_write);
        assert!(flush_saves(Duration::from_secs(10)));
        assert!(take_save_errors().iter().any(|e| e.file == bad), "the panic must be recorded as a SaveError");
        queue_json_bytes(&good, br#"{"ok":true}"#.to_vec());
        assert!(flush_saves(Duration::from_secs(10)), "the worker died after the panic");
        assert_eq!(std::fs::read(&good).unwrap_or_else(|e| panic!("{e}")), br#"{"ok":true}"#);
    }

    /// A queued removal reads as gone at once (its .bak too), lands after an earlier queued save,
    /// and a save queued after it brings the file back.
    #[test]
    fn a_queued_removal_is_in_order_and_read_as_gone() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("session.json");
        write_json_bytes(&path, br#"{"v":1}"#).unwrap_or_else(|e| panic!("{e}"));
        write_json_bytes(&path, br#"{"v":2}"#).unwrap_or_else(|e| panic!("{e}"));
        assert!(backup_path(&path).exists());
        submit_with(&path, br#"{"v":3}"#.to_vec(), slow_write);
        queue_remove_with_backup(&path);
        assert!(load_json_with_backup::<serde_json::Value>(&path, |_| true).is_none());
        assert!(flush_saves(Duration::from_secs(10)));
        assert!(!path.exists() && !backup_path(&path).exists());
        queue_json_bytes(&path, br#"{"v":4}"#.to_vec());
        assert!(flush_saves(Duration::from_secs(10)));
        assert_eq!(std::fs::read(&path).unwrap_or_else(|e| panic!("{e}")), br#"{"v":4}"#);
    }

    /// The observer hears one failure per file (not one per attempt) and one recovery.
    #[test]
    fn the_observer_hears_each_change_once() {
        use std::sync::{Arc, Mutex};
        let heard: Arc<Mutex<Vec<SaveEvent>>> = Arc::default();
        let sink = heard.clone();
        set_save_observer(move |e| sink.lock().unwrap_or_else(|p| p.into_inner()).push(e.clone()));
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("observed.json");
        for _ in 0..3 {
            submit_with(&path, b"{}".to_vec(), locked_write);
            assert!(flush_saves(Duration::from_secs(10)));
        }
        queue_json_bytes(&path, b"{}".to_vec());
        assert!(flush_saves(Duration::from_secs(10)));
        let mine: Vec<SaveEvent> =
            heard.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|e| e.file == path).cloned().collect();
        assert_eq!(mine.len(), 2, "{mine:?}");
        assert!(mine[0].error.is_some() && mine[1].error.is_none());
    }

    static STORM_WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn counting_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        STORM_WRITES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(40));
        write_json_bytes(path, bytes)
    }

    /// An undo storm (a key held down) saves the collection on every step: the writer coalesces
    /// them, so a hundred quick saves are a handful of disk writes, and the last one is what is
    /// on disk (agy GAPS #22).
    #[test]
    fn a_hundred_rapid_saves_are_a_few_writes_and_the_last_one_wins() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("storm.json");
        for v in 0..100 {
            submit_with(&path, format!(r#"{{"v":{v}}}"#).into_bytes(), counting_write);
        }
        assert!(flush_saves(Duration::from_secs(20)));
        let writes = STORM_WRITES.load(std::sync::atomic::Ordering::SeqCst);
        assert!(writes <= 5, "{writes} disk writes for 100 saves");
        assert_eq!(std::fs::read(&path).unwrap_or_else(|e| panic!("{e}")), br#"{"v":99}"#);
    }
}
