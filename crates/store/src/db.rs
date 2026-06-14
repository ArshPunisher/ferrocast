//! Opening the database, migrating it and handing out connections.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::backup;
use crate::error::{Result, StoreError};

/// Migrations in order. The index + 1 is the schema version (`PRAGMA user_version`).
/// Never edit a released migration; add a new one.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_moderation_flags.sql"),
    include_str!("../migrations/0003_held_and_timeouts.sql"),
    include_str!("../migrations/0004_tts_pending_details.sql"),
];

/// Schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const READERS: usize = 3;

/// The database: one writer, a few readers.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

impl Store {
    /// Opens (or creates) `dir/ferrocast.db`, backing it up and migrating it if needed.
    pub fn open(dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("ferrocast.db");
        let existed = path.exists();
        let mut writer = connect(&path, true)?;
        let version: i64 = writer.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::TooNew { found: version, supported: SCHEMA_VERSION });
        }
        if existed && version < SCHEMA_VERSION && version > 0 {
            backup::snapshot(&writer, dir, &format!("before-v{SCHEMA_VERSION}"))?;
        }
        migrate(&mut writer, version)?;
        let readers = (0..READERS).map(|_| connect(&path, false).map(Mutex::new)).collect::<Result<Vec<_>>>()?;
        Ok(Store { dir: dir.to_path_buf(), writer: Mutex::new(writer), readers, next_reader: AtomicUsize::new(0) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs `f` on the writer connection inside a transaction.
    pub fn write<T>(&self, f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Runs `f` on one of the reader connections.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let i = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len().max(1);
        match self.readers.get(i) {
            Some(r) => f(&r.lock().unwrap_or_else(|p| p.into_inner())),
            None => f(&self.writer.lock().unwrap_or_else(|p| p.into_inner())),
        }
    }

    /// Copies the database to `backups/` now (used before app updates too).
    pub fn snapshot(&self, label: &str) -> Result<PathBuf> {
        let conn = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        backup::snapshot(&conn, &self.dir, label)
    }
}

/// `check` runs SQLite's integrity check; only the first connection needs it, and on a
/// large event log it takes noticeable time.
fn connect(path: &Path, check: bool) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // A damaged file must stop the app with a clear error, never start it empty (R-34).
    if check {
        let ok: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if ok != "ok" {
            return Err(StoreError::Corrupt { what: "database file", detail: ok });
        }
    }
    Ok(conn)
}

fn migrate(conn: &mut Connection, from: i64) -> Result<()> {
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version <= from {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::info!(version, "database migrated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let v: i64 = s.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        drop(s);
        Store::open(dir.path()).unwrap();
        assert!(!dir.path().join("backups").exists(), "no backup when nothing migrated");
    }

    #[test]
    fn refuses_a_newer_database() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        let c = Connection::open(dir.path().join("ferrocast.db")).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        assert!(matches!(Store::open(dir.path()), Err(StoreError::TooNew { found: 99, .. })));
    }

    #[test]
    fn refuses_a_damaged_file_instead_of_starting_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ferrocast.db"), b"this is not a database at all, just junk bytes").unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert!(std::fs::read(dir.path().join("ferrocast.db")).unwrap().starts_with(b"this is not"), "left untouched");
    }

    #[test]
    fn failed_write_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let r: Result<()> = s.write(|tx| {
            tx.execute("INSERT INTO counters(name, value) VALUES ('a', 1)", [])?;
            Err(StoreError::Conflict("boom".into()))
        });
        assert!(r.is_err());
        let n: i64 = s.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM counters", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0);
    }
}
