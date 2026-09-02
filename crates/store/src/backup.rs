//! Database snapshots: taken every day, before every migration, update and import, and on
//! request, kept rotating (R-34).

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

/// How many snapshots to keep besides the daily ones. Older ones are deleted after a new
/// one is written.
pub const KEEP: usize = 10;
/// How many daily snapshots to keep (a week), on top of [`KEEP`], so a week of daily copies
/// never pushes out the copy taken before a migration or an import.
pub const KEEP_DAILY: usize = 7;

/// The label daily snapshots are taken with.
pub const DAILY: &str = "daily";
/// The label of the snapshot taken right before an app update installs.
pub const BEFORE_UPDATE: &str = "before-update";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub id: String,
    pub bytes: u64,
    pub created_at: i64,
    /// Why it was taken: `daily`, `update`, `migration`, `import`, `restore` (the data a
    /// restore replaced), `manual`, or `other`.
    pub reason: &'static str,
    /// The Ferrocast version that took it; `null` for snapshots from before this was kept.
    pub version: Option<String>,
}

/// What a snapshot's label says about why it was taken.
pub fn reason_of(label: &str) -> &'static str {
    match label {
        DAILY => "daily",
        BEFORE_UPDATE => "update",
        "manual" => "manual",
        "replaced" => "restore",
        l if l.ends_with("import") => "import",
        l if l.strip_prefix("before-v").is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())) => {
            "migration"
        }
        _ => "other",
    }
}

/// Kept next to each snapshot as `<id>.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    version: String,
}

/// Writes a consistent copy with `VACUUM INTO` (safe while the app is running) to a temp
/// name, then renames it, so a crash or full disk never leaves a half-written backup that
/// looks valid.
pub fn snapshot(conn: &Connection, dir: &Path, label: &str) -> Result<PathBuf> {
    let backups = dir.join("backups");
    std::fs::create_dir_all(&backups).map_err(|e| StoreError::Backup(e.to_string()))?;
    let safe: String = label.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    let name = format!("{}-{safe}.db", crate::now_ms());
    let tmp = backups.join(format!("{name}.partial"));
    let path = backups.join(&name);
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()]).map_err(|e| StoreError::Backup(e.to_string()))?;
    rename_patiently(&tmp, &path).map_err(|e| StoreError::Backup(e.to_string()))?;
    // Which version took it, for "restore previous" after an update. Losing this file only
    // loses the label, never the backup, so a failure is logged, not returned.
    let meta = Meta { version: env!("CARGO_PKG_VERSION").to_string() };
    let meta_path = path.with_extension("json");
    if let Err(e) = serde_json::to_vec(&meta).map_err(std::io::Error::other).and_then(|b| std::fs::write(&meta_path, b))
    {
        tracing::warn!(error = %e, "couldn't write the backup's version");
    }
    rotate(&backups)?;
    Ok(path)
}

/// When the newest snapshot of any kind was taken (each one is a full copy).
pub fn newest(dir: &Path) -> Result<Option<i64>> {
    Ok(list(dir)?.first().map(|b| b.created_at))
}

/// Lists snapshots, newest first.
pub fn list(dir: &Path) -> Result<Vec<BackupInfo>> {
    let backups = dir.join("backups");
    if !backups.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&backups)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".db") else {
            continue;
        };
        let (stamp, label) = id.split_once('-').unwrap_or((id, ""));
        let created_at = stamp.parse().unwrap_or(0);
        let version = std::fs::read(backups.join(format!("{id}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
            .map(|m| m.version);
        out.push(BackupInfo {
            id: id.to_string(),
            bytes: entry.metadata()?.len(),
            created_at,
            reason: reason_of(label),
            version,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// Marks a snapshot to be restored at the next start (the live database can't be swapped
/// under a running app). Ids are checked against the real list, so no path tricks.
pub fn schedule_restore(dir: &Path, id: &str) -> Result<()> {
    if !list(dir)?.iter().any(|b| b.id == id) {
        return Err(StoreError::NotFound("backup"));
    }
    std::fs::write(dir.join("restore-on-start"), id)?;
    Ok(())
}

/// Called before opening the store: swaps in a scheduled snapshot, keeping the current
/// database as one more backup first.
pub fn apply_scheduled_restore(dir: &Path) -> Result<Option<String>> {
    let marker = dir.join("restore-on-start");
    let Ok(id) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let id = id.trim().to_string();
    if !list(dir)?.iter().any(|b| b.id == id) {
        std::fs::remove_file(&marker)?;
        return Ok(None);
    }
    let db = dir.join("ferrocast.db");
    // Take the chosen backup out first: the safety snapshot below rotates old backups, and
    // the one being restored may be the oldest.
    let tmp = dir.join("ferrocast.db.restoring");
    std::fs::copy(dir.join("backups").join(format!("{id}.db")), &tmp)?;
    // Flushing needs a handle opened for writing: on Windows a read-only one is refused.
    std::fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    if db.exists() {
        // Keep the current data first, through SQLite so the WAL's newest pages are
        // included (a plain file copy would miss everything since the last checkpoint).
        let conn = Connection::open(&db).map_err(|e| StoreError::Backup(e.to_string()))?;
        snapshot(&conn, dir, "replaced")?;
        drop(conn);
    }
    // Rename the flushed copy over the database: a crash mid-way leaves either the old or
    // the new database, never half of one (R-34).
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("ferrocast.db{ext}")));
    }
    rename_patiently(&tmp, &db)?;
    // Only now is the restore done; a crash earlier retries it on the next start.
    std::fs::remove_file(&marker)?;
    Ok(Some(id))
}

/// Renames, retrying for up to about two seconds when the OS says the file is busy: on
/// Windows an antivirus scan or the search indexer often holds a just-written file.
pub(crate) fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut wait = std::time::Duration::from_millis(25);
    for _ in 0..7 {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            other => return other,
        }
    }
    std::fs::rename(from, to)
}

/// Keeps the newest [`KEEP_DAILY`] daily snapshots and the newest [`KEEP`] others, and
/// removes the rest with their `.json` labels.
fn rotate(backups: &Path) -> Result<()> {
    let mut all: Vec<_> = std::fs::read_dir(backups)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
        .collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    let is_daily = |e: &std::fs::DirEntry| e.file_name().to_string_lossy().ends_with(&format!("-{DAILY}.db"));
    let (daily, other): (Vec<_>, Vec<_>) = all.into_iter().partition(is_daily);
    for old in daily.into_iter().skip(KEEP_DAILY).chain(other.into_iter().skip(KEEP)) {
        let path = old.path();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn snapshots_rotate_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.write(|tx| Ok(tx.execute("INSERT INTO counters(name, value) VALUES ('deaths', 3)", [])?)).unwrap();
        let first = s.snapshot("manual").unwrap();
        assert!(first.exists());
        for i in 0..12 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.snapshot(&format!("n{i}")).unwrap();
        }
        let all = list(dir.path()).unwrap();
        assert_eq!(all.len(), KEEP);
        let target = all.last().unwrap().id.clone();
        s.write(|tx| Ok(tx.execute("UPDATE counters SET value = 99", [])?)).unwrap();
        drop(s);
        schedule_restore(dir.path(), &target).unwrap();
        assert_eq!(apply_scheduled_restore(dir.path()).unwrap(), Some(target));
        assert!(!dir.path().join("restore-on-start").exists());
        let s = Store::open(dir.path()).unwrap();
        let v: i64 = s.read(|c| Ok(c.query_row("SELECT value FROM counters", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v, 3);
        // The safety copy of the replaced database has the newest value, WAL included.
        let replaced = list(dir.path()).unwrap().into_iter().find(|b| b.id.ends_with("-replaced")).unwrap();
        let c = Connection::open(dir.path().join("backups").join(format!("{}.db", replaced.id))).unwrap();
        let kept: i64 = c.query_row("SELECT value FROM counters", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 99);
    }

    #[test]
    fn restore_refuses_unknown_or_path_like_ids() {
        let dir = tempfile::tempdir().unwrap();
        let _s = Store::open(dir.path()).unwrap();
        assert!(schedule_restore(dir.path(), "../../etc/passwd").is_err());
        assert!(schedule_restore(dir.path(), "nope").is_err());
        assert_eq!(apply_scheduled_restore(dir.path()).unwrap(), None);
    }

    #[test]
    fn labels_cannot_escape_the_backup_folder() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let p = s.snapshot("../../evil/../x").unwrap();
        assert_eq!(p.parent().unwrap(), dir.path().join("backups"));
    }

    #[test]
    fn backups_say_why_and_which_version() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for label in [
            DAILY,
            BEFORE_UPDATE,
            "manual",
            "before-import",
            "before-streamelements-import",
            "before-v7",
            "replaced",
            "x",
        ] {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.snapshot(label).unwrap();
        }
        let all = list(dir.path()).unwrap();
        let reasons: Vec<_> = all.iter().rev().map(|b| b.reason).collect();
        assert_eq!(reasons, ["daily", "update", "manual", "import", "import", "migration", "restore", "other"]);
        assert!(all.iter().all(|b| b.version.as_deref() == Some(env!("CARGO_PKG_VERSION"))));
        // A snapshot from before labels were kept still lists, without a version.
        std::fs::remove_file(dir.path().join("backups").join(format!("{}.json", all[0].id))).unwrap();
        assert_eq!(list(dir.path()).unwrap()[0].version, None);
        assert_eq!(reason_of("before-v"), "other");
        assert_eq!(reason_of("before-vx"), "other");
    }

    #[test]
    fn a_week_of_daily_copies_never_pushes_out_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.snapshot("before-v3").unwrap();
        for _ in 0..(KEEP_DAILY + 5) {
            std::thread::sleep(std::time::Duration::from_millis(2));
            s.snapshot(DAILY).unwrap();
        }
        let all = list(dir.path()).unwrap();
        assert_eq!(all.iter().filter(|b| b.reason == "daily").count(), KEEP_DAILY);
        assert!(all.iter().any(|b| b.reason == "migration"));
        // Rotated snapshots take their labels with them.
        let labels = std::fs::read_dir(dir.path().join("backups"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".json"))
            .count();
        assert_eq!(labels, all.len());
        assert_eq!(newest(dir.path()).unwrap(), Some(all[0].created_at));
    }
}
