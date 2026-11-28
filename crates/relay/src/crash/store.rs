//! Crash reports' tables (migration 18). Only new tables; nothing existing changes.
//!
//! - `crash_signatures`: one row per problem (a signature the app computes), with how many
//!   reports it got and when. It outlives the bundles and the per-report rows.
//! - `crash_facets`: per signature, the versions, operating systems and installs that hit it
//!   (`facet` is `version`, `os` or `install`), each with a count. `(signature, version)` is
//!   the dedupe key: a repeat only bumps its count and last-seen time.
//! - `crash_reports`: one row per report, with its metadata and the key of its bundle in the
//!   `BlobStore` (`NULL` once the bundle is gone: older than the newest few, evicted for
//!   room, or past the retention).
//! - `crash_rate`: one row per accepted report, only to count the per-install and relay-wide
//!   rate limits. It is separate so trimming the report rows can never loosen a limit.
//! - `crash_key`: the secret that makes `install_hash`, kept in the database so a restore
//!   keeps the hashes stable.
//!
//! The install id is never stored here, only its keyed hash.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS crash_key (id INTEGER PRIMARY KEY CHECK (id = 1), key BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS crash_signatures (signature TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL, count INTEGER NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS crash_signatures_seen ON crash_signatures(last_seen DESC, signature DESC);
CREATE TABLE IF NOT EXISTS crash_facets (signature TEXT NOT NULL REFERENCES crash_signatures(signature) ON DELETE CASCADE, facet TEXT NOT NULL, value TEXT NOT NULL, count INTEGER NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, PRIMARY KEY (signature, facet, value)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS crash_reports (id TEXT PRIMARY KEY, signature TEXT NOT NULL REFERENCES crash_signatures(signature) ON DELETE CASCADE, received_ms INTEGER NOT NULL, version TEXT NOT NULL, os TEXT NOT NULL, arch TEXT NOT NULL, kind TEXT NOT NULL, cause_codes TEXT NOT NULL, message TEXT NOT NULL, install_hash TEXT NOT NULL, size_bytes INTEGER NOT NULL, blob_key TEXT);
CREATE INDEX IF NOT EXISTS crash_reports_sig ON crash_reports(signature, received_ms DESC);
CREATE INDEX IF NOT EXISTS crash_reports_time ON crash_reports(received_ms);
CREATE INDEX IF NOT EXISTS crash_reports_blobs ON crash_reports(received_ms) WHERE blob_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS crash_rate (at INTEGER NOT NULL, install_hash TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS crash_rate_install ON crash_rate(install_hash, at);
CREATE INDEX IF NOT EXISTS crash_rate_at ON crash_rate(at);
";

/// A report as it is stored (and as the detail view shows it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub id: String,
    pub signature: String,
    pub received_ms: i64,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub kind: String,
    pub cause_codes: Vec<String>,
    pub message: String,
    pub install_hash: String,
    pub size_bytes: i64,
    /// `None` once the bundle was removed.
    pub blob_key: Option<String>,
}

const REPORT_COLS: &str =
    "id, signature, received_ms, version, os, arch, kind, cause_codes, message, install_hash, size_bytes, blob_key";

fn report(r: &rusqlite::Row<'_>) -> rusqlite::Result<Report> {
    let codes: String = r.get(7)?;
    Ok(Report {
        id: r.get(0)?,
        signature: r.get(1)?,
        received_ms: r.get(2)?,
        version: r.get(3)?,
        os: r.get(4)?,
        arch: r.get(5)?,
        kind: r.get(6)?,
        cause_codes: serde_json::from_str(&codes).unwrap_or_default(),
        message: r.get(8)?,
        install_hash: r.get(9)?,
        size_bytes: r.get(10)?,
        blob_key: r.get(11)?,
    })
}

/// One signature with its totals, for the list and the detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    pub signature: String,
    pub kind: String,
    pub title: String,
    pub count: i64,
    pub installs: i64,
    pub first_seen: i64,
    pub last_seen: i64,
}

fn signature_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Signature> {
    Ok(Signature {
        signature: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        count: r.get(3)?,
        first_seen: r.get(4)?,
        last_seen: r.get(5)?,
        installs: r.get(6)?,
    })
}

const SIGNATURE_COLS: &str = "s.signature, s.kind, s.title, s.count, s.first_seen, s.last_seen,
    (SELECT COUNT(*) FROM crash_facets f WHERE f.signature = s.signature AND f.facet = 'install')";

/// What the list asks for.
#[derive(Debug, Default)]
pub(crate) struct ListFilter {
    pub kind: Option<String>,
    pub version: Option<String>,
    pub q: Option<String>,
    /// `(last_seen, signature)` of the last row of the page before.
    pub before: Option<(i64, String)>,
    pub limit: i64,
}

/// What a new report adds.
pub(crate) struct NewReport<'a> {
    pub id: &'a str,
    pub signature: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub version: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
    pub cause_codes: &'a [String],
    pub message: &'a str,
    pub install_hash: &'a str,
    pub size_bytes: i64,
    pub blob_key: &'a str,
    pub now: i64,
}

fn bump_facet(tx: &Transaction<'_>, sig: &str, facet: &str, value: &str, now: i64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO crash_facets (signature, facet, value, count, first_seen, last_seen) VALUES (?1, ?2, ?3, 1, ?4, ?4)
         ON CONFLICT (signature, facet, value) DO UPDATE SET count = count + 1, last_seen = excluded.last_seen",
        params![sig, facet, value, now],
    )?;
    Ok(())
}

impl Db {
    /// The secret behind `install_hash`: made once, at the first report.
    pub(crate) fn crash_key(&self) -> rusqlite::Result<Vec<u8>> {
        let c = self.c();
        let mut fresh = [0u8; 32];
        // No OS randomness means nothing secure is possible; refuse loudly.
        #[allow(clippy::expect_used)]
        getrandom::fill(&mut fresh).expect("the OS random number generator is unavailable");
        c.execute("INSERT OR IGNORE INTO crash_key (id, key) VALUES (1, ?1)", [fresh.as_slice()])?;
        c.query_row("SELECT key FROM crash_key WHERE id = 1", [], |r| r.get(0))
    }

    /// `(reports in the last hour, oldest of them)` and the same for the last day, for one
    /// install (or everyone with `None`).
    pub(crate) fn crash_rate(&self, install_hash: Option<&str>, since: i64) -> rusqlite::Result<(i64, Option<i64>)> {
        self.c().query_row(
            "SELECT COUNT(*), MIN(at) FROM crash_rate WHERE at > ?1 AND (?2 IS NULL OR install_hash = ?2)",
            params![since, install_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// Bytes of the bundles kept.
    pub(crate) fn crash_bytes(&self) -> rusqlite::Result<i64> {
        self.c().query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM crash_reports WHERE blob_key IS NOT NULL",
            [],
            |r| r.get(0),
        )
    }

    /// The oldest kept bundles first: `(id, blob_key, size)`.
    pub(crate) fn crash_oldest_bundles(&self, limit: i64) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, blob_key, size_bytes FROM crash_reports WHERE blob_key IS NOT NULL ORDER BY received_ms, id LIMIT ?1",
        )?;
        let rows = st.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// The bundles of one signature past the newest `keep`: `(id, blob_key)`.
    pub(crate) fn crash_extra_bundles(&self, signature: &str, keep: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, blob_key FROM crash_reports WHERE signature = ?1 AND blob_key IS NOT NULL
             ORDER BY received_ms DESC, id DESC LIMIT -1 OFFSET ?2",
        )?;
        let rows = st.query_map(params![signature, keep], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The bundles received before `before`: `(id, blob_key)`.
    pub(crate) fn crash_bundles_before(&self, before: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, blob_key FROM crash_reports WHERE blob_key IS NOT NULL AND received_ms < ?1 ORDER BY received_ms, id",
        )?;
        let rows = st.query_map([before], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The bundle of this report is gone: forget its key.
    pub(crate) fn crash_clear_bundle(&self, id: &str) -> rusqlite::Result<()> {
        self.c().execute("UPDATE crash_reports SET blob_key = NULL WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Adds one report and counts it under its signature, in one transaction.
    pub(crate) fn crash_add(&self, n: &NewReport<'_>) -> rusqlite::Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO crash_signatures (signature, kind, title, count, first_seen, last_seen) VALUES (?1, ?2, ?3, 1, ?4, ?4)
             ON CONFLICT (signature) DO UPDATE SET count = count + 1, last_seen = MAX(last_seen, excluded.last_seen)",
            params![n.signature, n.kind, n.title, n.now],
        )?;
        bump_facet(&tx, n.signature, "version", n.version, n.now)?;
        bump_facet(&tx, n.signature, "os", n.os, n.now)?;
        bump_facet(&tx, n.signature, "install", n.install_hash, n.now)?;
        tx.execute(
            &format!(
                "INSERT INTO crash_reports ({REPORT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            params![
                n.id,
                n.signature,
                n.now,
                n.version,
                n.os,
                n.arch,
                n.kind,
                serde_json::to_string(n.cause_codes).unwrap_or_else(|_| "[]".into()),
                n.message,
                n.install_hash,
                n.size_bytes,
                n.blob_key
            ],
        )?;
        tx.execute("INSERT INTO crash_rate (at, install_hash) VALUES (?1, ?2)", params![n.now, n.install_hash])?;
        // Old rate rows are only needed for a day.
        tx.execute("DELETE FROM crash_rate WHERE at < ?1", [n.now - 86_400_000])?;
        tx.commit()
    }

    /// Keeps only the newest `keep` report rows of a signature (their bundles must be gone
    /// first: only the newest few ever have one). The signature's counts stay.
    pub(crate) fn crash_trim_rows(&self, signature: &str, keep: i64) -> rusqlite::Result<usize> {
        self.c().execute(
            "DELETE FROM crash_reports WHERE signature = ?1 AND blob_key IS NULL AND id IN (
                SELECT id FROM crash_reports WHERE signature = ?1 ORDER BY received_ms DESC, id DESC LIMIT -1 OFFSET ?2)",
            params![signature, keep],
        )
    }

    pub(crate) fn crash_signature_count(&self) -> rusqlite::Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM crash_signatures", [], |r| r.get(0))
    }

    /// The signature that was seen least recently, other than `except`.
    pub(crate) fn crash_stalest_signature(&self, except: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT signature FROM crash_signatures WHERE signature <> ?1 ORDER BY last_seen, signature LIMIT 1",
                [except],
                |r| r.get(0),
            )
            .optional()
    }

    /// The bundles of one signature: `(id, blob_key)`.
    pub(crate) fn crash_signature_bundles(&self, signature: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT id, blob_key FROM crash_reports WHERE signature = ?1 AND blob_key IS NOT NULL")?;
        let rows = st.query_map([signature], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Deletes a signature with its facets and reports (cascade). `false` if there was none.
    pub(crate) fn crash_delete_signature(&self, signature: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM crash_signatures WHERE signature = ?1", [signature])? > 0)
    }

    /// Report rows older than `before` go, and signatures not seen since then.
    pub(crate) fn crash_prune_metadata(&self, before: i64) -> rusqlite::Result<()> {
        let c = self.c();
        c.execute("DELETE FROM crash_reports WHERE received_ms < ?1 AND blob_key IS NULL", [before])?;
        // Never one that still has a bundle: its key is the only way to delete the file.
        c.execute(
            "DELETE FROM crash_signatures WHERE last_seen < ?1 AND NOT EXISTS (
                SELECT 1 FROM crash_reports r WHERE r.signature = crash_signatures.signature AND r.blob_key IS NOT NULL)",
            [before],
        )?;
        c.execute("DELETE FROM crash_rate WHERE at < ?1", [before.max(0)])?;
        Ok(())
    }

    /// Signatures last seen before `before`: their bundles are removed first.
    pub(crate) fn crash_signatures_before(&self, before: i64) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT signature FROM crash_signatures WHERE last_seen < ?1")?;
        let rows = st.query_map([before], |r| r.get(0))?;
        rows.collect()
    }

    pub(crate) fn crash_signature(&self, signature: &str) -> rusqlite::Result<Option<Signature>> {
        self.c()
            .query_row(
                &format!("SELECT {SIGNATURE_COLS} FROM crash_signatures s WHERE s.signature = ?1"),
                [signature],
                signature_row,
            )
            .optional()
    }

    /// One page of the list, newest last-seen first (one more than `limit` when there is a next page).
    pub(crate) fn crash_list(&self, f: &ListFilter) -> rusqlite::Result<Vec<Signature>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {SIGNATURE_COLS} FROM crash_signatures s
             WHERE (?1 IS NULL OR s.kind = ?1)
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM crash_facets f WHERE f.signature = s.signature AND f.facet = 'version' AND f.value = ?2))
               AND (?3 IS NULL OR instr(lower(s.title), lower(?3)) > 0 OR instr(s.signature, lower(?3)) > 0)
               AND (?4 IS NULL OR s.last_seen < ?4 OR (s.last_seen = ?4 AND s.signature < ?5))
             ORDER BY s.last_seen DESC, s.signature DESC LIMIT ?6"
        ))?;
        let (ms, sig) = match &f.before {
            Some((m, s)) => (Some(*m), Some(s.as_str())),
            None => (None, None),
        };
        let rows = st.query_map(params![f.kind, f.version, f.q, ms, sig, f.limit + 1], signature_row)?;
        rows.collect()
    }

    /// A signature's values for one facet (`version` or `os`), most recent first.
    pub(crate) fn crash_facet_values(&self, signature: &str, facet: &str) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT value FROM crash_facets WHERE signature = ?1 AND facet = ?2 ORDER BY last_seen DESC, value LIMIT 50",
        )?;
        let rows = st.query_map(params![signature, facet], |r| r.get(0))?;
        rows.collect()
    }

    pub(crate) fn crash_latest_report_id(&self, signature: &str) -> rusqlite::Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT id FROM crash_reports WHERE signature = ?1 ORDER BY received_ms DESC, id DESC LIMIT 1",
                [signature],
                |r| r.get(0),
            )
            .optional()
    }

    /// A signature's newest reports.
    pub(crate) fn crash_reports_of(&self, signature: &str, limit: i64) -> rusqlite::Result<Vec<Report>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {REPORT_COLS} FROM crash_reports WHERE signature = ?1 ORDER BY received_ms DESC, id DESC LIMIT ?2"
        ))?;
        let rows = st.query_map(params![signature, limit], report)?;
        rows.collect()
    }

    pub(crate) fn crash_report(&self, id: &str) -> rusqlite::Result<Option<Report>> {
        self.c().query_row(&format!("SELECT {REPORT_COLS} FROM crash_reports WHERE id = ?1"), [id], report).optional()
    }

    /// `(reports, signatures, bytes)` across everything.
    pub(crate) fn crash_totals(&self) -> rusqlite::Result<(i64, i64, i64)> {
        let c = self.c();
        let (reports, signatures): (i64, i64) =
            c.query_row("SELECT COALESCE(SUM(count), 0), COUNT(*) FROM crash_signatures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let bytes: i64 = c.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM crash_reports WHERE blob_key IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok((reports, signatures, bytes))
    }
}
