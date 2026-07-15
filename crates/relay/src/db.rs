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
CREATE TABLE IF NOT EXISTS tip_pages (install_id TEXT PRIMARY KEY, handle TEXT NOT NULL UNIQUE, config TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS released_handles (handle TEXT PRIMARY KEY, install_id TEXT NOT NULL, released_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS handle_changes (install_id TEXT NOT NULL, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pending_oauth (state TEXT PRIMARY KEY, install_id TEXT NOT NULL, verifier TEXT NOT NULL, created_at INTEGER NOT NULL);
";

/// How a tip page save went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    Yes,
    Taken,
    TooManyChanges,
}

/// Holds a released handle for its install. An install holds only its latest released
/// name, so renaming can't be used to squat dozens of names.
fn release(tx: &rusqlite::Transaction<'_>, install: &str, handle: &str, now: i64) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM released_handles WHERE install_id = ?1", [install])?;
    tx.execute(
        "INSERT INTO released_handles(handle, install_id, released_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(handle) DO UPDATE SET install_id = excluded.install_id, released_at = excluded.released_at",
        params![handle, install, now],
    )?;
    Ok(())
}

/// Items waiting for one install, at most. A desktop away for days gets the newest ones.
pub const MAX_QUEUE: i64 = 5_000;
/// How long undelivered items are kept. Tips are money, so they wait longer.
pub const QUEUE_TTL_MS: i64 = 3 * 86_400_000;
pub const TIP_TTL_MS: i64 = 30 * 86_400_000;
/// An install that never connected after signing up is removed after this long.
pub const UNUSED_INSTALL_MS: i64 = 7 * 86_400_000;

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

    /// Tests: pretend an install signed up `ago_ms` ago and never connected since.
    #[cfg(test)]
    pub(crate) fn backdate_install(&self, id: &str, ago_ms: i64) -> rusqlite::Result<()> {
        let at = now_ms() - ago_ms;
        self.c().execute("UPDATE installs SET created_at = ?2, last_seen = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    pub fn touch_install(&self, id: &str) -> rusqlite::Result<()> {
        // Always after created_at, so "never connected" (equal stamps) can't match a used install.
        self.c()
            .execute("UPDATE installs SET last_seen = MAX(?2, created_at + 1) WHERE id = ?1", params![id, now_ms()])?;
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

    /// Queues an item, trimming the oldest beyond the cap. Non-tip items go first: a chat
    /// flood while the desktop is away must never push out a tip. Returns its id.
    pub fn enqueue(&self, install: &str, kind: &str, payload: &str) -> rusqlite::Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO queue(install_id, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![install, kind, payload, now_ms()],
        )?;
        let id = c.last_insert_rowid();
        let count: i64 = c.query_row("SELECT COUNT(*) FROM queue WHERE install_id = ?1", [install], |r| r.get(0))?;
        let over = count - MAX_QUEUE;
        if over > 0 {
            let gone = c.execute(
                "DELETE FROM queue WHERE id IN (SELECT id FROM queue WHERE install_id = ?1 AND kind != 'tip' ORDER BY id LIMIT ?2)",
                params![install, over],
            )?;
            let left = over - i64::try_from(gone).unwrap_or(over);
            if left > 0 {
                c.execute(
                    "DELETE FROM queue WHERE id IN (SELECT id FROM queue WHERE install_id = ?1 ORDER BY id LIMIT ?2)",
                    params![install, left],
                )?;
            }
        }
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

    /// Saves a tip page. Refuses a handle another install owns, or released less than
    /// `hold_ms` ago, and more than `max_changes` new handles a day. All in one transaction,
    /// so parallel saves can't get past the limit.
    pub fn set_tip_config(
        &self,
        install: &str,
        handle: &str,
        json: &str,
        hold_ms: i64,
        max_changes: i64,
    ) -> rusqlite::Result<Saved> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let owner: Option<String> =
            tx.query_row("SELECT install_id FROM tip_pages WHERE handle = ?1", [handle], |r| r.get(0)).optional()?;
        if owner.as_deref().is_some_and(|o| o != install) {
            return Ok(Saved::Taken);
        }
        let held: Option<(String, i64)> = tx
            .query_row("SELECT install_id, released_at FROM released_handles WHERE handle = ?1", [handle], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if held.as_ref().is_some_and(|(o, at)| o != install && now - at < hold_ms) {
            return Ok(Saved::Taken);
        }
        let previous: Option<String> =
            tx.query_row("SELECT handle FROM tip_pages WHERE install_id = ?1", [install], |r| r.get(0)).optional()?;
        // Putting your own page back up under the name you took down isn't a new name.
        let reclaiming = previous.is_none() && held.as_ref().is_some_and(|(o, _)| o == install);
        if previous.as_deref() != Some(handle) && !reclaiming {
            let changes: i64 = tx.query_row(
                "SELECT COUNT(*) FROM handle_changes WHERE install_id = ?1 AND at > ?2",
                params![install, now - 86_400_000],
                |r| r.get(0),
            )?;
            if changes >= max_changes {
                return Ok(Saved::TooManyChanges);
            }
            if let Some(p) = &previous {
                release(&tx, install, p, now)?;
            }
            tx.execute("INSERT INTO handle_changes(install_id, at) VALUES (?1, ?2)", params![install, now])?;
            tx.execute("DELETE FROM released_handles WHERE handle = ?1", [handle])?;
        } else if reclaiming {
            tx.execute("DELETE FROM released_handles WHERE handle = ?1", [handle])?;
        }
        tx.execute(
            "INSERT INTO tip_pages(install_id, handle, config) VALUES (?1, ?2, ?3)
             ON CONFLICT(install_id) DO UPDATE SET handle = excluded.handle, config = excluded.config",
            params![install, handle, json],
        )?;
        tx.commit()?;
        Ok(Saved::Yes)
    }

    /// Takes a tip page down; its handle stays held for this install.
    pub fn delete_tip_config(&self, install: &str) -> rusqlite::Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let handle: Option<String> =
            tx.query_row("SELECT handle FROM tip_pages WHERE install_id = ?1", [install], |r| r.get(0)).optional()?;
        let Some(handle) = handle else { return Ok(false) };
        release(&tx, install, &handle, now_ms())?;
        tx.execute("DELETE FROM tip_pages WHERE install_id = ?1", [install])?;
        tx.commit()?;
        Ok(true)
    }

    /// Forgets a webhook id (its delivery failed, so the provider's retry must get through).
    pub fn unsee(&self, message_id: &str) -> rusqlite::Result<()> {
        self.c().execute("DELETE FROM seen WHERE message_id = ?1", [message_id])?;
        Ok(())
    }

    pub fn tip_config(&self, install: &str) -> rusqlite::Result<Option<String>> {
        self.c().query_row("SELECT config FROM tip_pages WHERE install_id = ?1", [install], |r| r.get(0)).optional()
    }

    pub fn tip_config_by_handle(&self, handle: &str) -> rusqlite::Result<Option<String>> {
        self.c().query_row("SELECT config FROM tip_pages WHERE handle = ?1", [handle], |r| r.get(0)).optional()
    }

    /// Housekeeping: old queue items, seen ids and abandoned sign-ins.
    pub fn prune(&self) -> rusqlite::Result<()> {
        let now = now_ms();
        let c = self.c();
        c.execute("DELETE FROM queue WHERE kind != 'tip' AND created_at < ?1", [now - QUEUE_TTL_MS])?;
        c.execute("DELETE FROM queue WHERE created_at < ?1", [now - TIP_TTL_MS])?;
        // Tip ids are kept as long as tips are (Razorpay notices over 7 days old are refused).
        c.execute("DELETE FROM seen WHERE message_id NOT LIKE 'tip:%' AND at < ?1", [now - 86_400_000])?;
        c.execute("DELETE FROM seen WHERE at < ?1", [now - TIP_TTL_MS])?;
        c.execute("DELETE FROM handle_changes WHERE at < ?1", [now - 86_400_000])?;
        c.execute("DELETE FROM released_handles WHERE released_at < ?1", [now - crate::tips::HANDLE_HOLD_MS])?;
        c.execute("DELETE FROM pending_oauth WHERE created_at < ?1", [now - 600_000])?;
        // Installs registered but never used again (a scripted sign-up flood, or an app that
        // was installed and removed) go after a week; anything with a tip page or a Kick
        // channel stays.
        c.execute(
            "DELETE FROM installs WHERE last_seen = created_at AND created_at < ?1
               AND id NOT IN (SELECT install_id FROM tip_pages)
               AND id NOT IN (SELECT install_id FROM kick_owners)",
            [now - UNUSED_INSTALL_MS],
        )?;
        Ok(())
    }
}
