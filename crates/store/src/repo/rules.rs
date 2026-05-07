//! Alert rules (ordered) and the persisted alert queue.

use ferrocast_core::alert::{AlertRule, default_rules};
use ferrocast_core::queue::AlertQueue;
use rusqlite::{OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

/// Rules in the order the streamer arranged them. A new install gets the defaults, so
/// alerts work the moment an account is connected.
pub fn list(store: &Store) -> Result<Vec<AlertRule>> {
    let rules: Vec<AlertRule> = store.read(|c| {
        let mut st = c.prepare("SELECT rule FROM alert_rules ORDER BY position, id")?;
        Ok(st.query_map([], |r| super::from_json(0, r.get(0)?))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    if !rules.is_empty() || settings_seeded(store)? {
        return Ok(rules);
    }
    let defaults = default_rules();
    for r in &defaults {
        save(store, r)?;
    }
    super::settings::set(store, "alert_rules_seeded", &true)?;
    Ok(defaults)
}

fn settings_seeded(store: &Store) -> Result<bool> {
    Ok(super::settings::get::<bool>(store, "alert_rules_seeded")?.unwrap_or(false))
}

/// Validates then inserts or replaces a rule, keeping its position (new rules go last).
pub fn save(store: &Store, rule: &AlertRule) -> Result<()> {
    rule.validate().map_err(|e| StoreError::Conflict(e.to_string()))?;
    let json = serde_json::to_string(rule)?;
    store.write(|tx| {
        let pos: i64 = tx.query_row(
            "SELECT COALESCE((SELECT position FROM alert_rules WHERE id = ?1), (SELECT COALESCE(MAX(position), -1) + 1 FROM alert_rules))",
            [&rule.id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO alert_rules(id, position, rule, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET rule = excluded.rule, updated_at = excluded.updated_at",
            params![rule.id, pos, json, now_ms()],
        )?;
        Ok(())
    })
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM alert_rules WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("alert rule")) } else { Ok(()) }
}

pub fn load_queue(store: &Store) -> Result<Option<AlertQueue>> {
    let raw: Option<String> = store
        .read(|c| Ok(c.query_row("SELECT state FROM alert_queue WHERE id = 1", [], |r| r.get(0)).optional()?))?;
    match raw {
        None => Ok(None),
        Some(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| StoreError::Corrupt { what: "alert queue", detail: e.to_string() }),
    }
}

pub fn save_queue(store: &Store, q: &AlertQueue) -> Result<()> {
    let json = serde_json::to_string(q)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO alert_queue(id, state) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET state = excluded.state",
            [json],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::alert::{render, select};
    use ferrocast_core::event::Millis;
    use ferrocast_core::sample::any_sample;

    #[test]
    fn defaults_are_seeded_once_and_deleting_all_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let rules = list(&s).unwrap();
        assert_eq!(rules.len(), default_rules().len());
        for r in &rules {
            delete(&s, &r.id).unwrap();
        }
        assert!(list(&s).unwrap().is_empty(), "the streamer's choice to have no rules is kept");
    }

    #[test]
    fn order_is_kept_on_update_and_new_rules_go_last() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut rules = list(&s).unwrap();
        let first_id = rules[0].id.clone();
        rules[0].name = "Renamed".into();
        save(&s, &rules[0]).unwrap();
        let mut extra = rules[1].clone();
        extra.id = "zz-new".into();
        save(&s, &extra).unwrap();
        let after = list(&s).unwrap();
        assert_eq!(after[0].id, first_id);
        assert_eq!(after[0].name, "Renamed");
        assert_eq!(after.last().unwrap().id, "zz-new");
    }

    #[test]
    fn invalid_rules_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut r = list(&s).unwrap()[0].clone();
        r.kinds.clear();
        assert!(matches!(save(&s, &r), Err(StoreError::Conflict(_))));
        assert!(matches!(delete(&s, "missing"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn queue_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(load_queue(&s).unwrap().is_none());
        let rules = list(&s).unwrap();
        let e = any_sample("tip", Millis(0)).unwrap();
        let (r, v) = select(&rules, &e).unwrap();
        let mut q = AlertQueue::default();
        q.pause();
        q.enqueue(render("a1", r, v, &e), 0, Millis(0));
        save_queue(&s, &q).unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(load_queue(&s).unwrap(), Some(q));
    }
}
