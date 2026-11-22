//! Installed Community Store items and persistent one-click undo snapshots (R37).

use ferrocast_core::alert::AlertRule;
use ferrocast_core::design::Design;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

const MAX_UNDO_ENTRIES: i64 = 20;

/// Locally installed Community Store package metadata (never carries telemetry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreInstallRecord {
    pub item_id: String,
    pub kind: String,
    pub format: String,
    pub title: String,
    pub version: String,
    pub creator_id: String,
    pub creator_name: String,
    pub package_sha256: String,
    pub design_ids: Vec<String>,
    pub rule_ids: Vec<String>,
    pub plugin_id: Option<String>,
    pub installed_at: i64,
    pub updated_at: i64,
}

/// Snapshot of plugin zip bytes + active toggle before a store change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSnapshot {
    pub id: String,
    pub zip_b64: String,
    pub on: bool,
}

/// Exact state needed to undo an install, update, or uninstall in one click.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreUndoSnapshot {
    pub prev_install: Option<StoreInstallRecord>,
    pub created_design_ids: Vec<String>,
    pub prev_designs: Vec<Design>,
    pub prev_active_alert: Option<String>,
    pub prev_rules: Vec<AlertRule>,
    pub created_plugin_id: Option<String>,
    pub prev_plugin: Option<PluginSnapshot>,
}

/// Summary returned to clients so they can show a one-click Undo bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreUndoSummary {
    pub token: String,
    pub action: String,
    pub item_id: String,
    pub item_title: String,
    pub created_at: i64,
}

/// Stored undo row including the full rollback snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreUndoEntry {
    pub token: String,
    pub action: String,
    pub item_id: String,
    pub item_title: String,
    pub snapshot: StoreUndoSnapshot,
    pub created_at: i64,
}

impl StoreUndoEntry {
    pub fn summary(&self) -> StoreUndoSummary {
        StoreUndoSummary {
            token: self.token.clone(),
            action: self.action.clone(),
            item_id: self.item_id.clone(),
            item_title: self.item_title.clone(),
            created_at: self.created_at,
        }
    }
}

fn read_install_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreInstallRecord> {
    let design_ids_raw: String = row.get(8)?;
    let rule_ids_raw: String = row.get(9)?;
    Ok(StoreInstallRecord {
        item_id: row.get(0)?,
        kind: row.get(1)?,
        format: row.get(2)?,
        title: row.get(3)?,
        version: row.get(4)?,
        creator_id: row.get(5)?,
        creator_name: row.get(6)?,
        package_sha256: row.get(7)?,
        design_ids: super::from_json(8, design_ids_raw)?,
        rule_ids: super::from_json(9, rule_ids_raw)?,
        plugin_id: row.get(10)?,
        installed_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

pub fn list_installs(store: &Store) -> Result<Vec<StoreInstallRecord>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT item_id, kind, format, title, version, creator_id, creator_name, package_sha256,
                    design_ids, rule_ids, plugin_id, installed_at, updated_at
             FROM store_installs ORDER BY updated_at DESC, item_id",
        )?;
        Ok(st.query_map([], read_install_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get_install(store: &Store, item_id: &str) -> Result<Option<StoreInstallRecord>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT item_id, kind, format, title, version, creator_id, creator_name, package_sha256,
                    design_ids, rule_ids, plugin_id, installed_at, updated_at
             FROM store_installs WHERE item_id = ?1",
            [item_id],
            read_install_row,
        )
        .optional()?)
    })
}

pub fn upsert_install(store: &Store, rec: &StoreInstallRecord) -> Result<StoreInstallRecord> {
    let mut saved = rec.clone();
    let now = now_ms();
    if saved.installed_at == 0 {
        saved.installed_at = now;
    }
    saved.updated_at = now.max(saved.updated_at);
    let design_json = serde_json::to_string(&saved.design_ids)?;
    let rule_json = serde_json::to_string(&saved.rule_ids)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO store_installs
             (item_id, kind, format, title, version, creator_id, creator_name, package_sha256,
              design_ids, rule_ids, plugin_id, installed_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(item_id) DO UPDATE SET
               kind = excluded.kind,
               format = excluded.format,
               title = excluded.title,
               version = excluded.version,
               creator_id = excluded.creator_id,
               creator_name = excluded.creator_name,
               package_sha256 = excluded.package_sha256,
               design_ids = excluded.design_ids,
               rule_ids = excluded.rule_ids,
               plugin_id = excluded.plugin_id,
               updated_at = excluded.updated_at",
            params![
                saved.item_id,
                saved.kind,
                saved.format,
                saved.title,
                saved.version,
                saved.creator_id,
                saved.creator_name,
                saved.package_sha256,
                design_json,
                rule_json,
                saved.plugin_id,
                saved.installed_at,
                saved.updated_at,
            ],
        )?;
        Ok(())
    })?;
    Ok(saved)
}

pub fn delete_install(store: &Store, item_id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM store_installs WHERE item_id = ?1", [item_id])?))?;
    if n == 0 { Err(StoreError::NotFound("installed store item")) } else { Ok(()) }
}

pub fn save_undo(store: &Store, entry: &StoreUndoEntry) -> Result<StoreUndoSummary> {
    let mut saved = entry.clone();
    let now = now_ms();
    if saved.created_at == 0 {
        saved.created_at = now;
    }
    let snapshot_json = serde_json::to_string(&saved.snapshot)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO store_undo(token, action, item_id, item_title, snapshot_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(token) DO UPDATE SET
               action = excluded.action,
               item_id = excluded.item_id,
               item_title = excluded.item_title,
               snapshot_json = excluded.snapshot_json,
               created_at = excluded.created_at",
            params![saved.token, saved.action, saved.item_id, saved.item_title, snapshot_json, saved.created_at],
        )?;
        tx.execute(
            "DELETE FROM store_undo WHERE token NOT IN (
               SELECT token FROM store_undo ORDER BY created_at DESC, rowid DESC LIMIT ?1
             )",
            [MAX_UNDO_ENTRIES],
        )?;
        Ok(())
    })?;
    Ok(saved.summary())
}

pub fn latest_undo(store: &Store) -> Result<Option<StoreUndoSummary>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT token, action, item_id, item_title, created_at FROM store_undo
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            [],
            |r| {
                Ok(StoreUndoSummary {
                    token: r.get(0)?,
                    action: r.get(1)?,
                    item_id: r.get(2)?,
                    item_title: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )
        .optional()?)
    })
}

/// The undo entry (by token, or the newest), left in place: it is dropped by `drop_undo` only
/// after the undo has been applied, so a failed undo can be tried again.
pub fn peek_undo(store: &Store, token: Option<&str>) -> Result<StoreUndoEntry> {
    store.write(|tx| {
        let row: Option<(String, String, String, String, String, i64)> = match token {
            Some(tok) => tx
                .query_row(
                    "SELECT token, action, item_id, item_title, snapshot_json, created_at
                     FROM store_undo WHERE token = ?1",
                    [tok],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?,
            None => tx
                .query_row(
                    "SELECT token, action, item_id, item_title, snapshot_json, created_at
                     FROM store_undo ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?,
        };
        let Some((tok, action, item_id, item_title, raw_snap, created_at)) = row else {
            return Err(StoreError::NotFound("undo action"));
        };
        let snapshot: StoreUndoSnapshot =
            serde_json::from_str(&raw_snap).map_err(|e| StoreError::Conflict(format!("corrupt undo snapshot: {e}")))?;
        Ok(StoreUndoEntry { token: tok, action, item_id, item_title, snapshot, created_at })
    })
}

/// Removes an undo entry (after it was applied).
pub fn drop_undo(store: &Store, token: &str) -> Result<()> {
    store.write(|tx| {
        tx.execute("DELETE FROM store_undo WHERE token = ?1", [token])?;
        Ok(())
    })
}

/// Peeks and drops in one step (the entry is gone even if what it undoes fails: prefer
/// `peek_undo` then `drop_undo`).
pub fn take_undo(store: &Store, token: Option<&str>) -> Result<StoreUndoEntry> {
    let e = peek_undo(store, token)?;
    drop_undo(store, &e.token)?;
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_install() -> StoreInstallRecord {
        StoreInstallRecord {
            item_id: "aurora-pulse".into(),
            kind: "theme".into(),
            format: "ferrodesign".into(),
            title: "Aurora Pulse".into(),
            version: "1.0.0".into(),
            creator_id: "mira-atelier".into(),
            creator_name: "Mira Atelier".into(),
            package_sha256: "a".repeat(64),
            design_ids: vec!["d1".into(), "d2".into()],
            rule_ids: vec![],
            plugin_id: None,
            installed_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn peeking_an_undo_leaves_it_until_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let undo = StoreUndoEntry {
            token: "u1".into(),
            action: "install".into(),
            item_id: "aurora-pulse".into(),
            item_title: "Aurora Pulse".into(),
            snapshot: StoreUndoSnapshot {
                prev_install: None,
                created_design_ids: vec![],
                prev_designs: vec![],
                prev_active_alert: None,
                prev_rules: vec![],
                created_plugin_id: None,
                prev_plugin: None,
            },
            created_at: 1,
        };
        save_undo(&store, &undo).unwrap();
        let first = peek_undo(&store, Some("u1")).unwrap();
        assert_eq!(first.token, "u1");
        // A failed undo can be tried again: the entry is still there.
        assert!(peek_undo(&store, Some("u1")).is_ok());
        drop_undo(&store, "u1").unwrap();
        assert!(peek_undo(&store, Some("u1")).is_err());
    }

    #[test]
    fn install_and_undo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(list_installs(&store).unwrap().is_empty());
        assert!(latest_undo(&store).unwrap().is_none());

        let saved = upsert_install(&store, &sample_install()).unwrap();
        assert_eq!(list_installs(&store).unwrap().len(), 1);
        assert_eq!(get_install(&store, "aurora-pulse").unwrap().unwrap().version, "1.0.0");

        let undo = StoreUndoEntry {
            token: "u1".into(),
            action: "install".into(),
            item_id: "aurora-pulse".into(),
            item_title: "Aurora Pulse".into(),
            snapshot: StoreUndoSnapshot {
                prev_install: Some(saved),
                created_design_ids: vec!["d1".into()],
                prev_designs: vec![],
                prev_active_alert: None,
                prev_rules: vec![],
                created_plugin_id: None,
                prev_plugin: None,
            },
            created_at: 2000,
        };
        save_undo(&store, &undo).unwrap();
        assert_eq!(latest_undo(&store).unwrap().unwrap().token, "u1");
        let popped = take_undo(&store, Some("u1")).unwrap();
        assert_eq!(popped.item_id, "aurora-pulse");
        assert!(latest_undo(&store).unwrap().is_none());

        delete_install(&store, "aurora-pulse").unwrap();
        assert!(list_installs(&store).unwrap().is_empty());
    }
}
