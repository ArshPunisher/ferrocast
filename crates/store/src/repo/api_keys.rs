//! Local API keys and redacted usage audit storage.

use std::fmt;

use ferrocast_core::api_keys::{ApiKeyAuditEntry, ApiKeyScope, ApiKeySummary, MAX_ACTIVE_KEYS};
use rusqlite::{OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::{Store, new_id, now_ms};

/// Maximum retained audit entries (oldest trimmed automatically).
pub const MAX_AUDIT_ROWS: i64 = 500;
/// Maximum retained total rows in `api_keys` (oldest revoked keys trimmed when exceeded).
const MAX_TOTAL_ROWS: i64 = 128;

/// Stored API key record including its SHA-256 hex digest (used for constant-time auth checks).
#[derive(Clone)]
pub struct ApiKeyRecord {
    pub summary: ApiKeySummary,
    pub key_hash: String,
}

impl fmt::Debug for ApiKeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyRecord").field("summary", &self.summary).field("key_hash", &"[redacted]").finish()
    }
}

/// Inserts a newly hashed API key and returns its stored summary.
pub fn insert(
    store: &Store,
    id: &str,
    name: &str,
    prefix: &str,
    key_hash: &str,
    scopes: &[ApiKeyScope],
    expires_at: Option<i64>,
) -> Result<ApiKeySummary> {
    let created_at = now_ms();
    let scopes_json = serde_json::to_string(scopes)?;
    store.write(|tx| {
        let active: i64 = tx.query_row("SELECT COUNT(*) FROM api_keys WHERE revoked_at IS NULL", [], |r| r.get(0))?;
        if active >= i64::try_from(MAX_ACTIVE_KEYS).unwrap_or(32) {
            return Err(StoreError::Conflict(format!(
                "You can have at most {MAX_ACTIVE_KEYS} active API keys. Revoke an unused key first."
            )));
        }
        tx.execute(
            "INSERT INTO api_keys(id, name, prefix, key_hash, scopes_json, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, name, prefix, key_hash, scopes_json, created_at, expires_at],
        )?;
        // Prune oldest revoked entries if historical table grows large.
        tx.execute(
            "DELETE FROM api_keys
             WHERE revoked_at IS NOT NULL
               AND id NOT IN (SELECT id FROM api_keys ORDER BY created_at DESC, rowid DESC LIMIT ?1)",
            [MAX_TOTAL_ROWS],
        )?;
        Ok(ApiKeySummary {
            id: id.to_string(),
            name: name.to_string(),
            prefix: prefix.to_string(),
            scopes: scopes.to_vec(),
            created_at,
            expires_at,
            last_used_at: None,
            revoked_at: None,
        })
    })
}

/// Lists all local API keys (newest first), excluding hashes.
pub fn list(store: &Store) -> Result<Vec<ApiKeySummary>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, name, prefix, scopes_json, created_at, expires_at, last_used, revoked_at
             FROM api_keys ORDER BY created_at DESC, rowid DESC",
        )?;
        Ok(st
            .query_map([], |r| {
                Ok(ApiKeySummary {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    prefix: r.get(2)?,
                    scopes: super::from_json(3, r.get(3)?)?,
                    created_at: r.get(4)?,
                    expires_at: r.get(5)?,
                    last_used_at: r.get(6)?,
                    revoked_at: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Looks up an API key record by its SHA-256 hex digest.
pub fn find_by_hash(store: &Store, key_hash: &str) -> Result<Option<ApiKeyRecord>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT id, name, prefix, key_hash, scopes_json, created_at, expires_at, last_used, revoked_at
             FROM api_keys WHERE key_hash = ?1",
            [key_hash],
            |r| {
                Ok(ApiKeyRecord {
                    summary: ApiKeySummary {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        prefix: r.get(2)?,
                        scopes: super::from_json(4, r.get(4)?)?,
                        created_at: r.get(5)?,
                        expires_at: r.get(6)?,
                        last_used_at: r.get(7)?,
                        revoked_at: r.get(8)?,
                    },
                    key_hash: r.get(3)?,
                })
            },
        )
        .optional()?)
    })
}

/// Revokes a key by id (idempotent if already revoked; returns `NotFound` if unknown id).
pub fn revoke(store: &Store, id: &str) -> Result<()> {
    let at = now_ms();
    let changed = store.write(|tx| {
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1)", [id], |r| r.get(0))?;
        if !exists {
            return Err(StoreError::NotFound("API key"));
        }
        Ok(tx.execute("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2", params![at, id])?)
    })?;
    let _ = changed;
    Ok(())
}

/// Records an API key usage event in the audit log and updates `last_used` without ever storing
/// the plain key or hash in the audit table.
pub fn record_usage(store: &Store, key_id: &str, key_name: &str, method: &str, path: &str, status: u16) -> Result<()> {
    let at = now_ms();
    let id = new_id();
    store.write(|tx| {
        tx.execute("UPDATE api_keys SET last_used = ?1 WHERE id = ?2", params![at, key_id])?;
        tx.execute(
            "INSERT INTO api_key_audit(id, key_id, key_name, method, path, status, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, key_id, key_name, method, path, i64::from(status), at],
        )?;
        tx.execute(
            "DELETE FROM api_key_audit
             WHERE id NOT IN (SELECT id FROM api_key_audit ORDER BY at DESC, rowid DESC LIMIT ?1)",
            [MAX_AUDIT_ROWS],
        )?;
        Ok(())
    })
}

/// Returns the most recent redacted API key usage log entries, newest first.
pub fn list_audit(store: &Store, limit: usize) -> Result<Vec<ApiKeyAuditEntry>> {
    let cap = i64::try_from(limit.min(usize::try_from(MAX_AUDIT_ROWS).unwrap_or(500))).unwrap_or(100);
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, key_id, key_name, method, path, status, at
             FROM api_key_audit ORDER BY at DESC, rowid DESC LIMIT ?1",
        )?;
        Ok(st
            .query_map([cap], |r| {
                let raw_status: i64 = r.get(5)?;
                Ok(ApiKeyAuditEntry {
                    id: r.get(0)?,
                    key_id: r.get(1)?,
                    key_name: r.get(2)?,
                    method: r.get(3)?,
                    path: r.get(4)?,
                    status: u16::try_from(raw_status).unwrap_or(0),
                    at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_lookup_revoke_and_audit_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let summary = insert(
            &store,
            "k1",
            "Stream Deck",
            "fc_key_deadbeef...",
            "hash_abc",
            &[ApiKeyScope::ReadOnly, ApiKeyScope::StudioControl],
            Some(9_999_999_999_999),
        )
        .unwrap();
        assert_eq!(summary.name, "Stream Deck");
        assert!(summary.revoked_at.is_none());

        let found = find_by_hash(&store, "hash_abc").unwrap().unwrap();
        assert_eq!(found.summary.id, "k1");
        assert_eq!(found.summary.scopes, vec![ApiKeyScope::ReadOnly, ApiKeyScope::StudioControl]);
        assert!(format!("{found:?}").contains("[redacted]"));
        assert!(!format!("{found:?}").contains("hash_abc"));

        record_usage(&store, "k1", "Stream Deck", "GET", "/status", 200).unwrap();
        let keys = list(&store).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].last_used_at.is_some());

        let audit = list_audit(&store, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].path, "/status");
        assert_eq!(audit[0].status, 200);

        revoke(&store, "k1").unwrap();
        let after = find_by_hash(&store, "hash_abc").unwrap().unwrap();
        assert!(after.summary.revoked_at.is_some());

        assert!(matches!(revoke(&store, "missing"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn enforces_max_active_keys_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for i in 0..MAX_ACTIVE_KEYS {
            insert(
                &store,
                &format!("k{i}"),
                &format!("Key {i}"),
                "fc_key_00000000...",
                &format!("hash_{i}"),
                &[ApiKeyScope::ReadOnly],
                None,
            )
            .unwrap();
        }
        let err =
            insert(&store, "k_over", "Overflow", "fc_key_00000000...", "hash_over", &[ApiKeyScope::ReadOnly], None)
                .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));

        // Revoking one frees up a slot.
        revoke(&store, "k0").unwrap();
        insert(&store, "k_new", "Replacement", "fc_key_00000000...", "hash_new", &[ApiKeyScope::ReadOnly], None)
            .unwrap();
    }
}
