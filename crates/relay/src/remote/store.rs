//! The dashboard-from-anywhere tables (migration 7). Device tokens and pairing codes are
//! kept only as peppered hashes (HMAC-SHA256, `RemoteConfig::pepper`); the audit log keeps
//! the action's name (method and route template), its status, the device and the time,
//! never a body, a query or an id from the path, and only for 30 days.

use rusqlite::{OptionalExtension, params};

use crate::db::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS remote_devices (id TEXT PRIMARY KEY, install_id TEXT NOT NULL, sub_id TEXT NOT NULL, role TEXT NOT NULL, name TEXT NOT NULL, token_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS remote_devices_install ON remote_devices(install_id);
CREATE TABLE IF NOT EXISTS remote_codes (code_hash TEXT PRIMARY KEY, install_id TEXT NOT NULL, sub_id TEXT NOT NULL, role TEXT NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS remote_codes_install ON remote_codes(install_id);
CREATE TABLE IF NOT EXISTS remote_audit (id INTEGER PRIMARY KEY, sub_id TEXT NOT NULL, install_id TEXT NOT NULL, device_id TEXT NOT NULL, device_name TEXT NOT NULL, action TEXT NOT NULL, status INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS remote_audit_install_at ON remote_audit(install_id, at);
CREATE INDEX IF NOT EXISTS remote_audit_at ON remote_audit(at);
";

/// Migration 11 (M10): where a change from a phone was answered, `pc` (the streaming PC,
/// through its tunnel) or `cloud` (the relay itself while the PC is off). Older relays write
/// rows without it and get `pc`.
pub(crate) const SCHEMA_SOURCE: &str = "
ALTER TABLE remote_audit ADD COLUMN source TEXT NOT NULL DEFAULT 'pc';
";

/// One paired phone or browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Device {
    pub id: String,
    pub install_id: String,
    pub sub_id: String,
    pub role: String,
    pub name: String,
    pub token_hash: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub expires_at: i64,
}

/// One line of the audit log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AuditRow {
    pub device_id: String,
    pub device_name: String,
    pub action: String,
    pub status: i64,
    pub at: i64,
    /// `pc` or `cloud` (answered by the relay while the PC was off).
    pub source: String,
}

/// A pairing code that was taken (and is now gone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Code {
    pub install_id: String,
    pub sub_id: String,
    pub role: String,
    pub name: String,
}

const DEVICE_COLS: &str = "id, install_id, sub_id, role, name, token_hash, created_at, last_used_at, expires_at";

fn device(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get(0)?,
        install_id: r.get(1)?,
        sub_id: r.get(2)?,
        role: r.get(3)?,
        name: r.get(4)?,
        token_hash: r.get(5)?,
        created_at: r.get(6)?,
        last_used_at: r.get(7)?,
        expires_at: r.get(8)?,
    })
}

impl Db {
    /// (devices, unused codes) of an install, any role: whether the tunnel is wanted.
    pub(crate) fn remote_totals(&self, install: &str, now: i64) -> rusqlite::Result<(i64, i64)> {
        let c = self.c();
        let devices: i64 = c.query_row(
            "SELECT COUNT(*) FROM remote_devices WHERE install_id = ?1 AND expires_at > ?2",
            params![install, now],
            |r| r.get(0),
        )?;
        let codes: i64 = c.query_row(
            "SELECT COUNT(*) FROM remote_codes WHERE install_id = ?1 AND expires_at > ?2",
            params![install, now],
            |r| r.get(0),
        )?;
        Ok((devices, codes))
    }

    /// Adds a code unless the install already has `max` devices plus unused codes of `role`:
    /// counted and inserted in one statement, so two requests at once can't both get in.
    /// `false` when it's full.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn remote_add_code_capped(
        &self,
        code_hash: &str,
        install: &str,
        sub: &str,
        role: &str,
        name: &str,
        now: i64,
        expires_at: i64,
        max: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.c().execute(
            "INSERT INTO remote_codes(code_hash, install_id, sub_id, role, name, created_at, expires_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
             WHERE (SELECT COUNT(*) FROM remote_devices WHERE install_id = ?2 AND role = ?4 AND expires_at > ?6)
                 + (SELECT COUNT(*) FROM remote_codes WHERE install_id = ?2 AND role = ?4 AND expires_at > ?6) < ?8",
            params![code_hash, install, sub, role, name, now, expires_at, max],
        )?;
        Ok(n > 0)
    }

    /// Takes a code once: it's deleted whether or not it was still valid.
    pub(crate) fn remote_take_code(&self, code_hash: &str, now: i64) -> rusqlite::Result<Option<Code>> {
        let c = self.c();
        let row = c
            .query_row(
                "DELETE FROM remote_codes WHERE code_hash = ?1 RETURNING install_id, sub_id, role, name, expires_at",
                [code_hash],
                |r| {
                    Ok((
                        Code { install_id: r.get(0)?, sub_id: r.get(1)?, role: r.get(2)?, name: r.get(3)? },
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(code, exp)| (exp > now).then_some(code)))
    }

    pub(crate) fn remote_add_device(&self, d: &Device) -> rusqlite::Result<()> {
        self.c().execute(
            &format!("INSERT INTO remote_devices({DEVICE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
            params![
                d.id,
                d.install_id,
                d.sub_id,
                d.role,
                d.name,
                d.token_hash,
                d.created_at,
                d.last_used_at,
                d.expires_at
            ],
        )?;
        Ok(())
    }

    /// Moves a device between a free account's key and Pro on the same PC.
    pub(crate) fn remote_set_sub(&self, id: &str, sub: &str) -> rusqlite::Result<()> {
        self.c().execute("UPDATE remote_devices SET sub_id = ?2 WHERE id = ?1", params![id, sub])?;
        Ok(())
    }

    pub(crate) fn remote_device(&self, id: &str) -> rusqlite::Result<Option<Device>> {
        self.c().query_row(&format!("SELECT {DEVICE_COLS} FROM remote_devices WHERE id = ?1"), [id], device).optional()
    }

    /// An install's devices (expired ones left out), newest first.
    pub(crate) fn remote_devices(&self, install: &str, now: i64) -> rusqlite::Result<Vec<Device>> {
        let c = self.c();
        let mut q = c.prepare(&format!(
            "SELECT {DEVICE_COLS} FROM remote_devices WHERE install_id = ?1 AND expires_at > ?2 ORDER BY created_at DESC, id"
        ))?;
        q.query_map(params![install, now], device)?.collect()
    }

    /// Slides the 30-day expiry forward.
    pub(crate) fn remote_touch(&self, id: &str, now: i64, expires_at: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE remote_devices SET last_used_at = ?2, expires_at = ?3 WHERE id = ?1",
            params![id, now, expires_at],
        )?;
        Ok(())
    }

    /// `true` when the device was there (and belonged to `install`, if given).
    pub(crate) fn remote_delete_device(&self, id: &str, install: Option<&str>) -> rusqlite::Result<bool> {
        let n = match install {
            Some(i) => {
                self.c().execute("DELETE FROM remote_devices WHERE id = ?1 AND install_id = ?2", params![id, i])?
            }
            None => self.c().execute("DELETE FROM remote_devices WHERE id = ?1", [id])?,
        };
        Ok(n > 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn remote_audit(
        &self,
        sub: &str,
        install: &str,
        device_id: &str,
        device_name: &str,
        action: &str,
        status: u16,
        at: i64,
    ) -> rusqlite::Result<()> {
        self.remote_audit_from(sub, install, device_id, device_name, action, status, at, "pc")
    }

    /// An audit line with where it was answered (`pc` or `cloud`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn remote_audit_from(
        &self,
        sub: &str,
        install: &str,
        device_id: &str,
        device_name: &str,
        action: &str,
        status: u16,
        at: i64,
        source: &str,
    ) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO remote_audit(sub_id, install_id, device_id, device_name, action, status, at, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![sub, install, device_id, device_name, action, i64::from(status), at, source],
        )?;
        Ok(())
    }

    /// The newest `limit` audit lines of an install.
    pub(crate) fn remote_audit_list(&self, install: &str, limit: i64) -> rusqlite::Result<Vec<AuditRow>> {
        let c = self.c();
        let mut q = c.prepare(
            "SELECT device_id, device_name, action, status, at, source FROM remote_audit WHERE install_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
        )?;
        q.query_map(params![install, limit], |r| {
            Ok(AuditRow {
                device_id: r.get(0)?,
                device_name: r.get(1)?,
                action: r.get(2)?,
                status: r.get(3)?,
                at: r.get(4)?,
                source: r.get(5)?,
            })
        })?
        .collect()
    }

    /// Expired devices and codes, and audit lines older than `audit_before`. Returns the
    /// installs whose devices went (their tunnels may close).
    pub(crate) fn remote_prune(&self, now: i64, audit_before: i64) -> rusqlite::Result<Vec<String>> {
        let c = self.c();
        let mut q = c.prepare("SELECT DISTINCT install_id FROM remote_devices WHERE expires_at <= ?1")?;
        let installs: Vec<String> = q.query_map([now], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        drop(q);
        c.execute("DELETE FROM remote_devices WHERE expires_at <= ?1", [now])?;
        c.execute("DELETE FROM remote_codes WHERE expires_at <= ?1", [now])?;
        c.execute("DELETE FROM remote_audit WHERE at < ?1", [audit_before])?;
        Ok(installs)
    }
}
