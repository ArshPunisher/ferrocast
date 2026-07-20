//! Loyalty point balances: one per person (viewer row), never below zero, never above
//! [`MAX_BALANCE`]. Every change happens inside one write transaction, so two quick
//! `!removepoints` (or a spend racing an award) can't both read the old balance.

use ferrocast_core::event::Platform;
use ferrocast_core::loyalty::MAX_BALANCE;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

fn pkey(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// What a change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The new balance.
    Done(i64),
    /// Not enough points for a strict change; nothing changed.
    NotEnough { have: i64 },
    /// It would pass [`MAX_BALANCE`]; nothing changed.
    TooMuch { have: i64 },
}

/// How a removal below zero is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floor {
    /// Refuse (spending points, the dashboard taking more than someone has).
    Strict,
    /// Stop at zero (a mod's `!removepoints 500` on someone with 200 leaves 0).
    Clamp,
}

/// One row of the leaderboard, shown with the account the person used most recently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leader {
    pub viewer_id: i64,
    pub platform: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub points: i64,
}

fn viewer_of(c: &Connection, platform: Platform, user_id: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        "SELECT viewer_id FROM viewer_identities WHERE platform = ?1 AND user_id = ?2",
        params![pkey(platform), user_id],
        |r| r.get(0),
    )
    .optional()
}

fn points_of(c: &Connection, viewer_id: i64) -> rusqlite::Result<i64> {
    Ok(c.query_row("SELECT points FROM loyalty_balances WHERE viewer_id = ?1", [viewer_id], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

fn set_points(c: &Connection, viewer_id: i64, points: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO loyalty_balances(viewer_id, points, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(viewer_id) DO UPDATE SET points = excluded.points, updated_at = excluded.updated_at",
        params![viewer_id, points.clamp(0, MAX_BALANCE), now_ms()],
    )?;
    Ok(())
}

/// Someone's balance; `None` when Ferrocast has never seen them.
pub fn balance(store: &Store, platform: Platform, user_id: &str) -> Result<Option<i64>> {
    store.read(|c| match viewer_of(c, platform, user_id)? {
        Some(v) => Ok(Some(points_of(c, v)?)),
        None => Ok(None),
    })
}

/// Adds `delta` (negative takes points away). `NotFound("viewer")` for someone unknown.
pub fn change(store: &Store, platform: Platform, user_id: &str, delta: i64, floor: Floor) -> Result<Change> {
    store.write(|tx| {
        let v = viewer_of(tx, platform, user_id)?.ok_or(StoreError::NotFound("viewer"))?;
        change_viewer(tx, v, delta, floor)
    })
}

/// [`change`] by viewer id, inside an open transaction (giveaways spend and refund here).
pub fn change_viewer(tx: &Connection, viewer_id: i64, delta: i64, floor: Floor) -> Result<Change> {
    let have = points_of(tx, viewer_id)?;
    let next = match have.checked_add(delta) {
        Some(n) if n > MAX_BALANCE => return Ok(Change::TooMuch { have }),
        Some(n) if n < 0 && floor == Floor::Strict => return Ok(Change::NotEnough { have }),
        Some(n) => n.max(0),
        None if delta > 0 => return Ok(Change::TooMuch { have }),
        None => 0,
    };
    set_points(tx, viewer_id, next)?;
    Ok(Change::Done(next))
}

/// Pays `amount` to each of these people once (linked accounts count as one person).
/// Balances stop at [`MAX_BALANCE`]. Unknown accounts are skipped. Returns how many were paid.
pub fn award(store: &Store, who: &[(Platform, String)], amount: u32) -> Result<usize> {
    if amount == 0 || who.is_empty() {
        return Ok(0);
    }
    store.write(|tx| {
        let mut paid = std::collections::BTreeSet::new();
        for (p, uid) in who {
            if let Some(v) = viewer_of(tx, *p, uid)? {
                paid.insert(v);
            }
        }
        for v in &paid {
            tx.execute(
                "INSERT INTO loyalty_balances(viewer_id, points, updated_at) VALUES (?1, MIN(?2, ?3), ?4)
                 ON CONFLICT(viewer_id) DO UPDATE SET points = MIN(points + excluded.points, ?3),
                   updated_at = excluded.updated_at",
                params![v, i64::from(amount), MAX_BALANCE, now_ms()],
            )?;
        }
        Ok(paid.len())
    })
}

/// Pays a support event's bonus once, ever, however often the platform delivers it.
/// Returns the new balance, or `None` when it already paid (or the person is unknown).
pub fn award_once(store: &Store, source: (Platform, &str), who: (Platform, &str), amount: u32) -> Result<Option<i64>> {
    if amount == 0 {
        return Ok(None);
    }
    store.write(|tx| {
        let Some(v) = viewer_of(tx, who.0, who.1)? else { return Ok(None) };
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO loyalty_awards(platform, source_id, at) VALUES (?1, ?2, ?3)",
            params![pkey(source.0), source.1, now_ms()],
        )?;
        if fresh == 0 {
            return Ok(None);
        }
        let next = points_of(tx, v)?.saturating_add(i64::from(amount)).min(MAX_BALANCE);
        set_points(tx, v, next)?;
        Ok(Some(next))
    })
}

/// The top `limit` balances above zero, highest first.
pub fn leaderboard(store: &Store, limit: u32) -> Result<Vec<Leader>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT b.viewer_id, i.platform, i.user_id, i.login, i.display_name, b.points
             FROM loyalty_balances b
             JOIN viewer_identities i ON i.rowid = (
                 SELECT rowid FROM viewer_identities WHERE viewer_id = b.viewer_id ORDER BY last_seen DESC LIMIT 1)
             WHERE b.points > 0
             ORDER BY b.points DESC, b.viewer_id ASC
             LIMIT ?1",
        )?;
        let rows = st.query_map([limit.clamp(1, 500)], |r| {
            Ok(Leader {
                viewer_id: r.get(0)?,
                platform: r.get(1)?,
                user_id: r.get(2)?,
                login: r.get(3)?,
                display_name: r.get(4)?,
                points: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Finds someone on `platform` by login (any case, with or without `@`): their user id
/// and display name.
pub fn find_login(store: &Store, platform: Platform, login: &str) -> Result<Option<(String, String)>> {
    let login = login.trim().trim_start_matches('@').to_string();
    if login.is_empty() {
        return Ok(None);
    }
    store.read(|c| {
        Ok(c.query_row(
            "SELECT user_id, display_name FROM viewer_identities
             WHERE platform = ?1 AND (login = ?2 COLLATE NOCASE OR display_name = ?2 COLLATE NOCASE)
             ORDER BY last_seen DESC LIMIT 1",
            params![pkey(platform), login],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::viewers;
    use ferrocast_core::event::Actor;

    fn actor(p: Platform, id: &str, name: &str) -> Actor {
        let mut a = Actor::named(p, name);
        a.id = Some(id.into());
        a
    }

    fn store() -> (Store, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        (Store::open(d.path()).unwrap(), d)
    }

    #[test]
    fn balances_never_go_negative_or_overflow() {
        let (s, _d) = store();
        viewers::touch(&s, &actor(Platform::Twitch, "1", "Sam"), true).unwrap();
        assert_eq!(balance(&s, Platform::Twitch, "1").unwrap(), Some(0));
        assert_eq!(balance(&s, Platform::Twitch, "nobody").unwrap(), None);
        assert_eq!(change(&s, Platform::Twitch, "1", 100, Floor::Strict).unwrap(), Change::Done(100));
        assert_eq!(change(&s, Platform::Twitch, "1", -101, Floor::Strict).unwrap(), Change::NotEnough { have: 100 });
        assert_eq!(balance(&s, Platform::Twitch, "1").unwrap(), Some(100), "a refused change changes nothing");
        assert_eq!(change(&s, Platform::Twitch, "1", -500, Floor::Clamp).unwrap(), Change::Done(0));
        assert_eq!(change(&s, Platform::Twitch, "1", i64::MIN, Floor::Clamp).unwrap(), Change::Done(0));
        assert_eq!(change(&s, Platform::Twitch, "1", MAX_BALANCE, Floor::Strict).unwrap(), Change::Done(MAX_BALANCE));
        assert_eq!(change(&s, Platform::Twitch, "1", 1, Floor::Strict).unwrap(), Change::TooMuch { have: MAX_BALANCE });
        assert_eq!(
            change(&s, Platform::Twitch, "1", i64::MAX, Floor::Strict).unwrap(),
            Change::TooMuch { have: MAX_BALANCE }
        );
        assert!(matches!(change(&s, Platform::Kick, "x", 5, Floor::Strict), Err(StoreError::NotFound(_))));
        award(&s, &[(Platform::Twitch, "1".into())], u32::MAX).unwrap();
        assert_eq!(balance(&s, Platform::Twitch, "1").unwrap(), Some(MAX_BALANCE), "awards stop at the cap");
        // The database itself refuses a negative balance, whatever code tries.
        let r = s.write(|tx| Ok(tx.execute("UPDATE loyalty_balances SET points = -1", [])?));
        assert!(r.is_err());
    }

    #[test]
    fn linked_accounts_share_one_balance_and_awards_pay_people_once() {
        let (s, _d) = store();
        viewers::touch(&s, &actor(Platform::Twitch, "t", "Sam"), true).unwrap();
        viewers::touch(&s, &actor(Platform::Kick, "k", "sam_k"), true).unwrap();
        change(&s, Platform::Kick, "k", 40, Floor::Strict).unwrap();
        change(&s, Platform::Twitch, "t", 10, Floor::Strict).unwrap();
        viewers::link(&s, Platform::Kick, "k", Platform::Twitch, "t").unwrap();
        assert_eq!(balance(&s, Platform::Kick, "k").unwrap(), Some(50), "the Kick balance came along");
        assert_eq!(balance(&s, Platform::Twitch, "t").unwrap(), Some(50));
        let paid = award(
            &s,
            &[(Platform::Twitch, "t".into()), (Platform::Kick, "k".into()), (Platform::Youtube, "?".into())],
            10,
        )
        .unwrap();
        assert_eq!(paid, 1, "one person, two accounts, one payment; unknown skipped");
        assert_eq!(balance(&s, Platform::Twitch, "t").unwrap(), Some(60));
    }

    #[test]
    fn a_bonus_pays_once_per_event() {
        let (s, _d) = store();
        viewers::touch(&s, &actor(Platform::Twitch, "1", "Sam"), false).unwrap();
        assert_eq!(award_once(&s, (Platform::Twitch, "follow-1"), (Platform::Twitch, "1"), 50).unwrap(), Some(50));
        assert_eq!(award_once(&s, (Platform::Twitch, "follow-1"), (Platform::Twitch, "1"), 50).unwrap(), None);
        assert_eq!(award_once(&s, (Platform::Twitch, "sub-1"), (Platform::Twitch, "nobody"), 50).unwrap(), None);
        assert_eq!(award_once(&s, (Platform::Twitch, "zero"), (Platform::Twitch, "1"), 0).unwrap(), None);
        assert_eq!(balance(&s, Platform::Twitch, "1").unwrap(), Some(50));
    }

    #[test]
    fn leaderboard_and_lookup() {
        let (s, _d) = store();
        for (id, name, pts) in [("1", "Ana", 30), ("2", "Bo", 90), ("3", "Cy", 0), ("4", "Di", 90)] {
            viewers::touch(&s, &actor(Platform::Twitch, id, name), true).unwrap();
            change(&s, Platform::Twitch, id, pts, Floor::Strict).unwrap();
        }
        let top = leaderboard(&s, 10).unwrap();
        let names: Vec<_> = top.iter().map(|l| l.display_name.as_str()).collect();
        assert_eq!(names, ["Bo", "Di", "Ana"], "zero balances aren't listed; ties keep a stable order");
        assert_eq!(leaderboard(&s, 1).unwrap().len(), 1);
        assert_eq!(find_login(&s, Platform::Twitch, "@ANA").unwrap(), Some(("1".into(), "Ana".into())));
        assert_eq!(find_login(&s, Platform::Kick, "ana").unwrap(), None);
        assert_eq!(find_login(&s, Platform::Twitch, " @ ").unwrap(), None);
    }
}
