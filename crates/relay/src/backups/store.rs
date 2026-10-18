//! Cloud backups' tables (migration 5). One row per backup, owned by the subscription
//! (`sub_id`), so the PCs that share Pro share the list. A row is `uploading` until its
//! parts are joined and the hash checked, then `complete`. Parts are listed in their own
//! table while they arrive. Only sizes, hashes and dates are stored here: the files are
//! ciphertext in the `BlobStore`, and the relay never has a key to them.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cloud_backups (id TEXT PRIMARY KEY, sub_id TEXT NOT NULL, install_id TEXT NOT NULL, bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, app_version TEXT NOT NULL, reason TEXT NOT NULL, part_size INTEGER NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS cloud_backups_sub ON cloud_backups(sub_id, state, created_at);
CREATE TABLE IF NOT EXISTS cloud_backup_parts (backup_id TEXT NOT NULL REFERENCES cloud_backups(id) ON DELETE CASCADE, n INTEGER NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY (backup_id, n)) WITHOUT ROWID;
";

pub(crate) const UPLOADING: &str = "uploading";
pub(crate) const COMPLETE: &str = "complete";

/// One backup row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub id: String,
    pub sub_id: String,
    pub install_id: String,
    pub bytes: i64,
    pub sha256: String,
    pub app_version: String,
    pub reason: String,
    pub part_size: i64,
    pub state: String,
    /// When the upload started; once complete, when it completed.
    pub created_at: i64,
    /// The last part or change.
    pub updated_at: i64,
}

impl Row {
    /// How many parts the upload is cut into.
    pub fn parts(&self) -> i64 {
        if self.part_size <= 0 { 0 } else { (self.bytes + self.part_size - 1) / self.part_size }
    }

    /// The size part `n` must have: `part_size`, the last one the rest.
    pub fn part_bytes(&self, n: i64) -> i64 {
        let last = self.parts() - 1;
        if n < last { self.part_size } else { self.bytes - last * self.part_size }
    }
}

const COLS: &str =
    "id, sub_id, install_id, bytes, sha256, app_version, reason, part_size, state, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        sub_id: r.get(1)?,
        install_id: r.get(2)?,
        bytes: r.get(3)?,
        sha256: r.get(4)?,
        app_version: r.get(5)?,
        reason: r.get(6)?,
        part_size: r.get(7)?,
        state: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

impl Db {
    pub(crate) fn add_backup(&self, b: &Row) -> rusqlite::Result<()> {
        self.c().execute(
            &format!("INSERT INTO cloud_backups ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![
                b.id,
                b.sub_id,
                b.install_id,
                b.bytes,
                b.sha256,
                b.app_version,
                b.reason,
                b.part_size,
                b.state,
                b.created_at,
                b.updated_at
            ],
        )?;
        Ok(())
    }

    /// One of this subscription's backups (complete or not).
    pub(crate) fn backup(&self, sub: &str, id: &str) -> rusqlite::Result<Option<Row>> {
        self.c()
            .query_row(&format!("SELECT {COLS} FROM cloud_backups WHERE sub_id = ?1 AND id = ?2"), [sub, id], row)
            .optional()
    }

    /// The subscription's backups in one state, oldest first.
    pub(crate) fn backups(&self, sub: &str, state: &str) -> rusqlite::Result<Vec<Row>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM cloud_backups WHERE sub_id = ?1 AND state = ?2 ORDER BY created_at, id"
        ))?;
        let rows = st.query_map([sub, state], row)?;
        rows.collect()
    }

    /// Whether this subscription has any backup kept (list and download stay open after a
    /// lapse while it does, R-04).
    pub(crate) fn has_backups(&self, sub: &str) -> rusqlite::Result<bool> {
        self.c().query_row("SELECT EXISTS(SELECT 1 FROM cloud_backups WHERE sub_id = ?1)", [sub], |r| r.get(0))
    }

    /// Records that part `n` arrived whole.
    pub(crate) fn add_backup_part(&self, id: &str, n: i64, bytes: i64, now: i64) -> rusqlite::Result<()> {
        let c = self.c();
        c.execute(
            "INSERT INTO cloud_backup_parts (backup_id, n, bytes) VALUES (?1, ?2, ?3)
             ON CONFLICT (backup_id, n) DO UPDATE SET bytes = excluded.bytes",
            params![id, n, bytes],
        )?;
        c.execute("UPDATE cloud_backups SET updated_at = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    /// The parts that arrived: `(n, bytes)`, in order.
    pub(crate) fn backup_parts(&self, id: &str) -> rusqlite::Result<Vec<(i64, i64)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT n, bytes FROM cloud_backup_parts WHERE backup_id = ?1 ORDER BY n")?;
        let rows = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The upload is whole and checked: complete as of `now`, its parts forgotten.
    pub(crate) fn complete_backup(&self, id: &str, now: i64) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM cloud_backup_parts WHERE backup_id = ?1", [id])?;
        tx.execute(
            "UPDATE cloud_backups SET state = ?2, created_at = ?3, updated_at = ?3 WHERE id = ?1",
            params![id, COMPLETE, now],
        )?;
        tx.commit()
    }

    /// Deletes a backup row (and its parts' rows). The caller removes the files.
    pub(crate) fn delete_backup(&self, id: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM cloud_backups WHERE id = ?1", [id])? > 0)
    }

    /// Uploads that stopped before `before` (no part since): the sweep removes them.
    pub(crate) fn stale_backup_uploads(&self, before: i64) -> rusqlite::Result<Vec<Row>> {
        let c = self.c();
        let mut st =
            c.prepare(&format!("SELECT {COLS} FROM cloud_backups WHERE state = ?1 AND updated_at < ?2 ORDER BY id"))?;
        let rows = st.query_map(params![UPLOADING, before], row)?;
        rows.collect()
    }

    /// Every subscription with a backup row.
    pub(crate) fn backup_subs(&self) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT DISTINCT sub_id FROM cloud_backups ORDER BY sub_id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Every row of a subscription, any state (the lapse sweep removes them all).
    pub(crate) fn all_backups(&self, sub: &str) -> rusqlite::Result<Vec<Row>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM cloud_backups WHERE sub_id = ?1 ORDER BY id"))?;
        let rows = st.query_map([sub], row)?;
        rows.collect()
    }

    /// Tests: pretend an upload's last part came `ago_ms` ago.
    #[cfg(test)]
    pub(crate) fn backdate_backup(&self, id: &str, ago_ms: i64) -> rusqlite::Result<()> {
        let at = crate::now_ms() - ago_ms;
        self.c().execute("UPDATE cloud_backups SET updated_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }
}
