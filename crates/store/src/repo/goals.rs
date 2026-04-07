//! Goals and the per-event contributions that make refunds exact (R-32).

use ferrocast_core::event::Platform;
use ferrocast_core::goal::Goal;
use rusqlite::{OptionalExtension, params};

use crate::Store;
use crate::error::{Result, StoreError};

fn pkey(p: Platform) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn list(store: &Store) -> Result<Vec<Goal>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT goal FROM goals ORDER BY rowid")?;
        Ok(st
            .query_map([], |r| super::from_json(0, r.get(0)?))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get(store: &Store, id: &str) -> Result<Goal> {
    store
        .read(|c| {
            Ok(
                c.query_row("SELECT goal FROM goals WHERE id = ?1", [id], |r| {
                    super::from_json(0, r.get(0)?)
                })
                .optional()?,
            )
        })?
        .ok_or(StoreError::NotFound("goal"))
}

pub fn save(store: &Store, goal: &Goal) -> Result<()> {
    if goal.title.trim().is_empty() || goal.title.chars().count() > 80 {
        return Err(StoreError::Conflict(
            "a goal needs a title of up to 80 characters".into(),
        ));
    }
    if goal.target <= 0 {
        return Err(StoreError::Conflict(
            "the target must be more than zero".into(),
        ));
    }
    if goal.kind == ferrocast_core::goal::GoalKind::Tips && goal.currency.is_none() {
        return Err(StoreError::Conflict("a tips goal needs a currency".into()));
    }
    let json = serde_json::to_string(goal)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO goals(id, goal) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET goal = excluded.goal",
            params![goal.id, json],
        )?;
        Ok(())
    })
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM goals WHERE id = ?1", [id])?))?;
    if n == 0 {
        Err(StoreError::NotFound("goal"))
    } else {
        Ok(())
    }
}

/// Adds an event's contribution once. Returns the updated goal, or `None` if this event
/// already counted (a redelivery must not count twice).
pub fn contribute(
    store: &Store,
    goal_id: &str,
    platform: Platform,
    source_id: &str,
    amount: i64,
) -> Result<Option<Goal>> {
    if amount == 0 {
        return Ok(None);
    }
    store.write(|tx| {
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO goal_contributions(goal_id, platform, source_id, amount) VALUES (?1, ?2, ?3, ?4)",
            params![goal_id, pkey(platform), source_id, amount],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        bump(tx, goal_id, amount)
    })
}

/// Takes back everything an event contributed (refund, chargeback). Returns changed goals.
pub fn reverse(store: &Store, platform: Platform, source_id: &str) -> Result<Vec<Goal>> {
    store.write(|tx| {
        let rows: Vec<(String, i64)> = {
            let mut st = tx.prepare("SELECT goal_id, amount FROM goal_contributions WHERE platform = ?1 AND source_id = ?2")?;
            st.query_map(params![pkey(platform), source_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "DELETE FROM goal_contributions WHERE platform = ?1 AND source_id = ?2",
            params![pkey(platform), source_id],
        )?;
        let mut changed = Vec::new();
        for (goal_id, amount) in rows {
            if let Some(g) = bump(tx, &goal_id, -amount)? {
                changed.push(g);
            }
        }
        Ok(changed)
    })
}

/// Manual adjustment from the dashboard ("+1 death").
pub fn adjust(store: &Store, goal_id: &str, delta: i64) -> Result<Goal> {
    store
        .write(|tx| bump(tx, goal_id, delta))?
        .ok_or(StoreError::NotFound("goal"))
}

fn bump(tx: &rusqlite::Transaction<'_>, goal_id: &str, delta: i64) -> Result<Option<Goal>> {
    let raw: Option<String> = tx
        .query_row("SELECT goal FROM goals WHERE id = ?1", [goal_id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let mut goal: Goal = serde_json::from_str(&raw)?;
    ferrocast_core::goal::apply(&mut goal, delta);
    tx.execute(
        "UPDATE goals SET goal = ?2 WHERE id = ?1",
        params![goal_id, serde_json::to_string(&goal)?],
    )?;
    Ok(Some(goal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::event::Millis;
    use ferrocast_core::goal::GoalKind;

    fn goal(id: &str, kind: GoalKind) -> Goal {
        Goal {
            id: id.into(),
            title: "Mic".into(),
            kind,
            target: 500_000,
            current: 0,
            currency: Some("INR".into()),
            starts_at: Millis(0),
            ends_at: None,
        }
    }

    #[test]
    fn contributions_count_once_and_reverse_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        save(&s, &goal("mic", GoalKind::Tips)).unwrap();
        save(&s, &goal("all", GoalKind::Tips)).unwrap();
        assert_eq!(
            contribute(&s, "mic", Platform::Tips, "pay_1", 50_000)
                .unwrap()
                .unwrap()
                .current,
            50_000
        );
        assert!(
            contribute(&s, "mic", Platform::Tips, "pay_1", 50_000)
                .unwrap()
                .is_none(),
            "redelivery"
        );
        contribute(&s, "all", Platform::Tips, "pay_1", 50_000).unwrap();
        contribute(&s, "mic", Platform::Tips, "pay_2", 10_000).unwrap();
        let changed = reverse(&s, Platform::Tips, "pay_1").unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(get(&s, "mic").unwrap().current, 10_000);
        assert_eq!(get(&s, "all").unwrap().current, 0);
        assert!(
            reverse(&s, Platform::Tips, "pay_1").unwrap().is_empty(),
            "reversing twice does nothing"
        );
    }

    #[test]
    fn validation_adjust_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut g = goal("g", GoalKind::Tips);
        g.currency = None;
        assert!(save(&s, &g).is_err());
        let mut g = goal("g", GoalKind::Follows);
        g.target = 0;
        assert!(save(&s, &g).is_err());
        g.target = 10;
        g.title = " ".into();
        assert!(save(&s, &g).is_err());
        g.title = "Follows".into();
        save(&s, &g).unwrap();
        assert_eq!(adjust(&s, "g", 3).unwrap().current, 3);
        assert!(matches!(
            adjust(&s, "nope", 1),
            Err(StoreError::NotFound(_))
        ));
        delete(&s, "g").unwrap();
        assert!(list(&s).unwrap().is_empty());
    }

    #[test]
    fn contributions_vanish_with_their_goal() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        save(&s, &goal("g", GoalKind::Tips)).unwrap();
        contribute(&s, "g", Platform::Tips, "p", 5).unwrap();
        delete(&s, "g").unwrap();
        let n: i64 = s
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM goal_contributions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0);
    }
}
