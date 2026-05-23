//! The relay's own small SQLite database: installs, who owns which Kick channel, the
//! per-install delivery queue, seen webhook ids, and sign-ins in progress.
//! It never stores platform tokens beyond the moment they're handed to the desktop.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::now_ms;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS installs (id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS kick_owners (broadcaster_id TEXT PRIMARY KEY, install_id TEXT NOT NULL REFERENCES installs(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS queue (id INTEGER PRIMARY KEY AUTOINCREMENT, install_id TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS queue_install ON queue(install_id, id);
CREATE TABLE IF NOT EXISTS seen (message_id TEXT PRIMARY KEY, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pending_oauth (state TEXT PRIMARY KEY, install_id TEXT NOT NULL, verifier TEXT NOT NULL, created_at INTEGER NOT NULL);
";

/// Items waiting for one install, at most. A desktop away for days gets the newest ones.
pub const MAX_QUEUE: i64 = 5_000;
/// How long undelivered items are kept.
pub const QUEUE_TTL_MS: i64 = 3 * 86_400_000;

#[derive(Debug)]
pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub kind: String,
    pub payload: String,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn add_install(&self, id: &str, secret_hash: &str) -> rusqlite::Result<()> {
        let now = now_ms();
        self.c().execute(
            "INSERT INTO installs(id, secret_hash, created_at, last_seen) VALUES (?1, ?2, ?3, ?3)",
            params![id, secret_hash, now],
        )?;
        Ok(())
    }

    pub fn install_hash(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.c().query_row("SELECT secret_hash FROM installs WHERE id = ?1", [id], |r| r.get(0)).optional()
    }

    pub fn touch_install(&self, id: &str) -> rusqlite::Result<()> {
        self.c().execute("UPDATE installs SET last_seen = ?2 WHERE id = ?1", params![id, now_ms()])?;
        Ok(())
    }

    pub fn set_kick_owner(&self, broadcaster: &str, install: &str) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO kick_owners(broadcaster_id, install_id) VALUES (?1, ?2)
             ON CONFLICT(broadcaster_id) DO UPDATE SET install_id = excluded.install_id",
            params![broadcaster, install],
        )?;
        Ok(())
    }

    pub fn kick_owner(&self, broadcaster: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row("SELECT install_id FROM kick_owners WHERE broadcaster_id = ?1", [broadcaster], |r| r.get(0))
            .optional()
    }

    /// Queues an item, trimming the oldest beyond the cap. Returns its id.
    pub fn enqueue(&self, install: &str, kind: &str, payload: &str) -> rusqlite::Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO queue(install_id, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![install, kind, payload, now_ms()],
        )?;
        let id = c.last_insert_rowid();
        c.execute(
            "DELETE FROM queue WHERE install_id = ?1 AND id <= (SELECT id FROM queue WHERE install_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
            params![install, MAX_QUEUE],
        )?;
        Ok(id)
    }

    pub fn pending(&self, install: &str, after: i64, limit: i64) -> rusqlite::Result<Vec<Item>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT id, kind, payload FROM queue WHERE install_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3")?;
        let rows = st.query_map(params![install, after, limit], |r| {
            Ok(Item { id: r.get(0)?, kind: r.get(1)?, payload: r.get(2)? })
        })?;
        rows.collect()
    }

    /// Deletes a delivered item. Only the owning install can ack it.
    pub fn ack(&self, install: &str, id: i64) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM queue WHERE install_id = ?1 AND id = ?2", params![install, id])? > 0)
    }

    /// Whether a webhook id was already handled (replay protection).
    pub fn seen(&self, message_id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .c()
            .query_row("SELECT 1 FROM seen WHERE message_id = ?1", [message_id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Records a webhook id as handled; `true` if it wasn't already.
    pub fn first_time(&self, message_id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .c()
            .execute("INSERT OR IGNORE INTO seen(message_id, at) VALUES (?1, ?2)", params![message_id, now_ms()])?
            > 0)
    }

    pub fn put_oauth(&self, state: &str, install: &str, verifier: &str) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO pending_oauth(state, install_id, verifier, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![state, install, verifier, now_ms()],
        )?;
        Ok(())
    }

    /// Takes a sign-in in progress (one use) if it's younger than ten minutes.
    pub fn take_oauth(&self, state: &str) -> rusqlite::Result<Option<(String, String)>> {
        let c = self.c();
        let row: Option<(String, String, i64)> = c
            .query_row("SELECT install_id, verifier, created_at FROM pending_oauth WHERE state = ?1", [state], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        c.execute("DELETE FROM pending_oauth WHERE state = ?1", [state])?;
        Ok(row.filter(|(_, _, at)| now_ms() - at < 600_000).map(|(i, v, _)| (i, v)))
    }

    /// Housekeeping: old queue items, seen ids and abandoned sign-ins.
    pub fn prune(&self) -> rusqlite::Result<()> {
        let now = now_ms();
        let c = self.c();
        c.execute("DELETE FROM queue WHERE created_at < ?1", [now - QUEUE_TTL_MS])?;
        c.execute("DELETE FROM seen WHERE at < ?1", [now - 86_400_000])?;
        c.execute("DELETE FROM pending_oauth WHERE created_at < ?1", [now - 600_000])?;
        Ok(())
    }
}
