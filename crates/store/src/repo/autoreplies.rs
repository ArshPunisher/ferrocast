//! Auto-replies (PRO.md M5), in the order they're checked. Validation is the core's
//! (`AutoReply::validate`); the engine calls it before saving and maps its errors to the
//! right status. This module only keeps the list and its order.

use ferrocast_core::autoreply::AutoReply;
use rusqlite::params;

use crate::Store;
use crate::error::{Result, StoreError};

/// Every rule, first-checked first.
pub fn list(store: &Store) -> Result<Vec<AutoReply>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT rule FROM autoreplies ORDER BY position, rowid")?;
        Ok(st.query_map([], |r| super::from_json(0, r.get(0)?))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn count(store: &Store) -> Result<usize> {
    let n: i64 = store.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM autoreplies", [], |r| r.get(0))?))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// Saves a rule: an existing id keeps its place, a new one goes last.
pub fn save(store: &Store, rule: &AutoReply) -> Result<()> {
    if rule.id.trim().is_empty() {
        return Err(StoreError::Conflict("an auto-reply needs an id".into()));
    }
    let json = serde_json::to_string(rule)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO autoreplies(id, position, rule)
             VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM autoreplies), ?2)
             ON CONFLICT(id) DO UPDATE SET rule = excluded.rule",
            params![rule.id, json],
        )?;
        Ok(())
    })
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM autoreplies WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("auto-reply")) } else { Ok(()) }
}

/// Puts the rules in this order. `ids` must name every rule exactly once.
pub fn reorder(store: &Store, ids: &[String]) -> Result<()> {
    store.write(|tx| {
        let have: Vec<String> = {
            let mut st = tx.prepare("SELECT id FROM autoreplies")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut want = ids.to_vec();
        want.sort();
        want.dedup();
        let mut sorted = have.clone();
        sorted.sort();
        if want.len() != ids.len() || want != sorted {
            return Err(StoreError::Conflict(
                "ids: list every auto-reply exactly once (the list changed; reload it).".into(),
            ));
        }
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE autoreplies SET position = ?1 WHERE id = ?2",
                params![i64::try_from(i).unwrap_or(0), id],
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::autoreply::MatchKind;

    fn rule(id: &str) -> AutoReply {
        let mut r = AutoReply::new(id, MatchKind::Contains, id, "reply");
        r.id = id.into();
        r
    }

    #[test]
    fn keeps_order_updates_in_place_and_reorders() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for id in ["a", "b", "c"] {
            save(&s, &rule(id)).unwrap();
        }
        let mut b = rule("b");
        b.reply = "changed".into();
        save(&s, &b).unwrap();
        let ids = |s: &Store| list(s).unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(&s), ["a", "b", "c"]);
        assert_eq!(list(&s).unwrap()[1].reply, "changed");
        assert_eq!(count(&s).unwrap(), 3);

        reorder(&s, &["c".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(ids(&s), ["c", "a", "b"]);
        save(&s, &rule("d")).unwrap();
        assert_eq!(ids(&s), ["c", "a", "b", "d"], "new ones go last");

        assert!(matches!(reorder(&s, &["c".into(), "a".into()]), Err(StoreError::Conflict(_))), "missing ids");
        assert!(matches!(reorder(&s, &["c".into(), "c".into(), "a".into(), "b".into()]), Err(StoreError::Conflict(_))));
        assert!(matches!(
            reorder(&s, &["c".into(), "a".into(), "b".into(), "zz".into()]),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(ids(&s), ["c", "a", "b", "d"], "a refused reorder changes nothing");

        delete(&s, "a").unwrap();
        assert!(matches!(delete(&s, "a"), Err(StoreError::NotFound(_))));
        assert_eq!(ids(&s), ["c", "b", "d"]);
        assert!(matches!(save(&s, &rule("")), Err(StoreError::Conflict(_))));
    }
}
