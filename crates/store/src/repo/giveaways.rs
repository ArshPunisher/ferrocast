//! Giveaway entry fees: taken and recorded together, refunded together.

use ferrocast_core::event::Platform;
use rusqlite::{OptionalExtension, params};

use super::loyalty::{Change, Floor, change_viewer};
use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

fn pkey(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// Takes `cost` points from this person and notes it against the giveaway, in one go.
/// `NotEnough` changes nothing. Paying twice for the same giveaway is refused as a conflict.
pub fn pay_entry(store: &Store, giveaway_id: &str, platform: Platform, user_id: &str, cost: i64) -> Result<Change> {
    store.write(|tx| {
        let v: i64 = tx
            .query_row(
                "SELECT viewer_id FROM viewer_identities WHERE platform = ?1 AND user_id = ?2",
                params![pkey(platform), user_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound("viewer"))?;
        let paid: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM giveaway_spend WHERE giveaway_id = ?1 AND viewer_id = ?2)",
            params![giveaway_id, v],
            |r| r.get(0),
        )?;
        if paid {
            return Err(StoreError::Conflict("already entered".into()));
        }
        let c = change_viewer(tx, v, -cost, Floor::Strict)?;
        if let Change::Done(_) = c {
            tx.execute(
                "INSERT INTO giveaway_spend(giveaway_id, viewer_id, amount, at) VALUES (?1, ?2, ?3, ?4)",
                params![giveaway_id, v, cost, now_ms()],
            )?;
        }
        Ok(c)
    })
}

/// Gives back every fee for one giveaway (or, with `None`, every unfinished one: the start
/// after a crash). Returns how many people were refunded.
pub fn refund(store: &Store, giveaway_id: Option<&str>) -> Result<usize> {
    store.write(|tx| {
        let rows: Vec<(String, i64, i64)> = {
            let mut st = tx.prepare(
                "SELECT giveaway_id, viewer_id, amount FROM giveaway_spend WHERE ?1 IS NULL OR giveaway_id = ?1",
            )?;
            st.query_map([giveaway_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (_, viewer, amount) in &rows {
            // Clamp: the refund can't fail, and the balance can't pass the cap.
            change_viewer(tx, *viewer, *amount, Floor::Clamp)?;
        }
        tx.execute("DELETE FROM giveaway_spend WHERE ?1 IS NULL OR giveaway_id = ?1", [giveaway_id])?;
        Ok(rows.len())
    })
}

/// The giveaway finished: the fees are spent for good.
pub fn settle(store: &Store, giveaway_id: &str) -> Result<()> {
    store.write(|tx| {
        tx.execute("DELETE FROM giveaway_spend WHERE giveaway_id = ?1", [giveaway_id])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{loyalty, viewers};
    use ferrocast_core::event::Actor;

    #[test]
    fn fees_are_taken_once_and_refunded_exactly() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        for id in ["a", "b"] {
            let mut a = Actor::named(Platform::Twitch, id);
            a.id = Some(id.into());
            viewers::touch(&s, &a, true).unwrap();
        }
        loyalty::change(&s, Platform::Twitch, "a", 100, Floor::Strict).unwrap();
        assert_eq!(pay_entry(&s, "g1", Platform::Twitch, "a", 60).unwrap(), Change::Done(40));
        assert!(matches!(pay_entry(&s, "g1", Platform::Twitch, "a", 10), Err(StoreError::Conflict(_))));
        assert_eq!(pay_entry(&s, "g2", Platform::Twitch, "a", 50).unwrap(), Change::NotEnough { have: 40 });
        assert_eq!(pay_entry(&s, "g2", Platform::Twitch, "b", 1).unwrap(), Change::NotEnough { have: 0 });
        assert!(matches!(pay_entry(&s, "g1", Platform::Kick, "x", 1), Err(StoreError::NotFound(_))));
        assert_eq!(pay_entry(&s, "g2", Platform::Twitch, "a", 40).unwrap(), Change::Done(0));
        assert_eq!(refund(&s, Some("g1")).unwrap(), 1);
        assert_eq!(loyalty::balance(&s, Platform::Twitch, "a").unwrap(), Some(60));
        assert_eq!(refund(&s, Some("g1")).unwrap(), 0, "refunds once");
        settle(&s, "g2").unwrap();
        assert_eq!(refund(&s, None).unwrap(), 0, "a settled giveaway keeps its fees");
        assert_eq!(loyalty::balance(&s, Platform::Twitch, "a").unwrap(), Some(60));
        pay_entry(&s, "g3", Platform::Twitch, "a", 10).unwrap();
        assert_eq!(refund(&s, None).unwrap(), 1, "the crash case refunds everything open");
        assert_eq!(loyalty::balance(&s, Platform::Twitch, "a").unwrap(), Some(60));
    }
}
