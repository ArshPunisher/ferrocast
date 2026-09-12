//! Studio data: counter values (the `counters` table), quotes (listing and editing; adding
//! is in `bot`), stream markers and watch time.

use ferrocast_core::event::Platform;
use ferrocast_core::moments::{Marker, MomentKind};
use rusqlite::{OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::repo::bot::Quote;
use crate::{Store, now_ms};

fn pkey(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn platform_of(s: &str) -> Option<Platform> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

// ------------------------------------------------------------------ counters

/// Every counter value, by name.
pub fn counter_values(store: &Store) -> Result<Vec<(String, i64)>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT name, value FROM counters ORDER BY name")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Sets a counter (creating it).
pub fn set_counter(store: &Store, name: &str, value: i64) -> Result<i64> {
    store.write(|tx| {
        tx.execute(
            "INSERT INTO counters(name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            params![name, value],
        )?;
        Ok(value)
    })
}

/// Applies `f` to the current value (0 when new) in one write, so two mods pressing `+`
/// at once both count.
pub fn update_counter(store: &Store, name: &str, f: impl FnOnce(i64) -> i64) -> Result<i64> {
    store.write(|tx| {
        let now: i64 =
            tx.query_row("SELECT value FROM counters WHERE name = ?1", [name], |r| r.get(0)).optional()?.unwrap_or(0);
        let next = f(now);
        tx.execute(
            "INSERT INTO counters(name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            params![name, next],
        )?;
        Ok(next)
    })
}

pub fn delete_counter(store: &Store, name: &str) -> Result<()> {
    store.write(|tx| {
        tx.execute("DELETE FROM counters WHERE name = ?1", [name])?;
        Ok(())
    })
}

// ------------------------------------------------------------------ quotes

/// Every quote, oldest first.
pub fn quotes(store: &Store) -> Result<Vec<Quote>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT id, text, added_by, at FROM quotes ORDER BY id")?;
        Ok(st
            .query_map([], |r| Ok(Quote { id: r.get(0)?, text: r.get(1)?, added_by: r.get(2)?, at: r.get(3)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn quote_count(store: &Store) -> Result<i64> {
    store.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM quotes", [], |r| r.get(0))?))
}

pub fn edit_quote(store: &Store, id: i64, text: &str) -> Result<Quote> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > ferrocast_core::chatbot::MAX_QUOTE_CHARS {
        return Err(StoreError::Conflict("quotes are 1 to 400 characters".into()));
    }
    store.write(|tx| {
        let n = tx.execute("UPDATE quotes SET text = ?2 WHERE id = ?1", params![id, text])?;
        if n == 0 {
            return Err(StoreError::NotFound("quote"));
        }
        Ok(tx.query_row("SELECT id, text, added_by, at FROM quotes WHERE id = ?1", [id], |r| {
            Ok(Quote { id: r.get(0)?, text: r.get(1)?, added_by: r.get(2)?, at: r.get(3)? })
        })?)
    })
}

// ------------------------------------------------------------------ markers

/// A new marker. `stream_started_at` ties it to the stream that was live.
pub fn add_marker(
    store: &Store,
    kind: MomentKind,
    label: &str,
    platform: Option<Platform>,
    stream_started_at: Option<i64>,
    clip: bool,
) -> Result<Marker> {
    let at = now_ms();
    let label: String = label.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect();
    let label = label.trim().to_string();
    let id = store.write(|tx| {
        tx.execute(
            "INSERT INTO markers(at, stream_started_at, kind, label, platform, clip) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![at, stream_started_at, kind.key(), label, platform.map(pkey), clip],
        )?;
        // Keep the table small: the newest 20,000 markers.
        tx.execute("DELETE FROM markers WHERE id <= (SELECT MAX(id) - 20000 FROM markers)", [])?;
        Ok(tx.last_insert_rowid())
    })?;
    Ok(Marker { id, at, offset_ms: stream_started_at.map(|s| (at - s).max(0)), kind, label, platform, clip })
}

/// Markers at or after `since` (ms), oldest first, up to `limit`.
pub fn markers(store: &Store, since: i64, limit: u32) -> Result<Vec<Marker>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, at, stream_started_at, kind, label, platform, clip FROM markers
             WHERE at >= ?1 ORDER BY at, id LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![since, limit], |r| {
                let at: i64 = r.get(1)?;
                let start: Option<i64> = r.get(2)?;
                let kind: String = r.get(3)?;
                let platform: Option<String> = r.get(5)?;
                Ok(Marker {
                    id: r.get(0)?,
                    at,
                    offset_ms: start.map(|s| (at - s).max(0)),
                    kind: MomentKind::from_key(&kind).unwrap_or(MomentKind::Manual),
                    label: r.get(4)?,
                    platform: platform.as_deref().and_then(platform_of),
                    clip: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn delete_marker(store: &Store, id: i64) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM markers WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("marker")) } else { Ok(()) }
}

/// Marks a marker as clipped (the replay buffer saved it).
pub fn mark_clipped(store: &Store, id: i64) -> Result<()> {
    store.write(|tx| {
        tx.execute("UPDATE markers SET clip = 1 WHERE id = ?1", [id])?;
        Ok(())
    })
}

// ------------------------------------------------------------------ watch time

/// Adds `minutes` to each account.
pub fn add_watch_time(store: &Store, who: &[(Platform, String)], minutes: i64) -> Result<usize> {
    if who.is_empty() {
        return Ok(0);
    }
    let at = now_ms();
    store.write(|tx| {
        let mut st = tx.prepare(
            "INSERT INTO watch_time(platform, user_id, minutes, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(platform, user_id) DO UPDATE SET minutes = MIN(watch_time.minutes + excluded.minutes, 100000000),
             updated_at = excluded.updated_at",
        )?;
        for (p, id) in who {
            st.execute(params![pkey(*p), id, minutes, at])?;
        }
        Ok(who.len())
    })
}

pub fn watch_time(store: &Store, platform: Platform, user_id: &str) -> Result<i64> {
    Ok(store
        .read(|c| {
            Ok(c.query_row(
                "SELECT minutes FROM watch_time WHERE platform = ?1 AND user_id = ?2",
                params![pkey(platform), user_id],
                |r| r.get(0),
            )
            .optional()?)
        })?
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::bot;

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::open(dir.path()).unwrap(), dir)
    }

    #[test]
    fn counters_update_in_one_write() {
        let (s, _d) = store();
        assert_eq!(update_counter(&s, "deaths", |v| v + 1).unwrap(), 1);
        assert_eq!(update_counter(&s, "deaths", |v| v + 5).unwrap(), 6);
        set_counter(&s, "wins", -2).unwrap();
        assert_eq!(counter_values(&s).unwrap(), vec![("deaths".into(), 6), ("wins".into(), -2)]);
        delete_counter(&s, "wins").unwrap();
        delete_counter(&s, "never").unwrap();
        assert_eq!(counter_values(&s).unwrap().len(), 1);
        assert_eq!(bot::counter(&s, "deaths").unwrap(), 6, "the bot module reads the same table");
    }

    #[test]
    fn quotes_list_and_edit() {
        let (s, _d) = store();
        bot::add_quote(&s, "first", "Asha").unwrap();
        let q = bot::add_quote(&s, "second", "Bo").unwrap();
        assert_eq!(quote_count(&s).unwrap(), 2);
        let e = edit_quote(&s, q.id, "  second, fixed ").unwrap();
        assert_eq!(e.text, "second, fixed");
        assert!(matches!(edit_quote(&s, 99, "x"), Err(StoreError::NotFound("quote"))));
        assert!(matches!(edit_quote(&s, q.id, " "), Err(StoreError::Conflict(_))));
        assert_eq!(
            quotes(&s).unwrap().iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
            vec!["first", "second, fixed"]
        );
    }

    #[test]
    fn markers_keep_their_stream_offset() {
        let (s, _d) = store();
        let start = now_ms() - 65_000;
        let m =
            add_marker(&s, MomentKind::Raid, "Bo raided\nwith 3", Some(Platform::Twitch), Some(start), false).unwrap();
        assert_eq!(m.label, "Bo raided with 3");
        assert!(m.offset_ms.unwrap() >= 65_000);
        add_marker(&s, MomentKind::Manual, &"x".repeat(500), None, None, true).unwrap();
        let all = markers(&s, 0, 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].platform, Some(Platform::Twitch));
        assert_eq!(all[1].label.chars().count(), 200);
        assert!(all[1].clip && all[1].offset_ms.is_none());
        mark_clipped(&s, all[0].id).unwrap();
        assert!(markers(&s, 0, 100).unwrap()[0].clip);
        assert!(markers(&s, now_ms() + 10_000, 100).unwrap().is_empty());
        delete_marker(&s, all[0].id).unwrap();
        assert!(matches!(delete_marker(&s, all[0].id), Err(StoreError::NotFound("marker"))));
    }

    #[test]
    fn watch_time_adds_up() {
        let (s, _d) = store();
        let who = vec![(Platform::Twitch, "1".to_string()), (Platform::Kick, "1".to_string())];
        add_watch_time(&s, &who, 5).unwrap();
        add_watch_time(&s, &who[..1], 5).unwrap();
        assert_eq!(add_watch_time(&s, &[], 5).unwrap(), 0);
        assert_eq!(watch_time(&s, Platform::Twitch, "1").unwrap(), 10);
        assert_eq!(watch_time(&s, Platform::Kick, "1").unwrap(), 5);
        assert_eq!(watch_time(&s, Platform::Youtube, "1").unwrap(), 0);
    }

    #[test]
    fn older_databases_gain_the_studio_tables_and_keep_counters_and_quotes() {
        use rusqlite::Connection;
        let dir = tempfile::tempdir().unwrap();
        {
            let mut c = Connection::open(dir.path().join("ferrocast.db")).unwrap();
            let migrations: Vec<&str> = vec![
                include_str!("../../migrations/0001_init.sql"),
                include_str!("../../migrations/0002_moderation_flags.sql"),
                include_str!("../../migrations/0003_held_and_timeouts.sql"),
                include_str!("../../migrations/0004_tts_pending_details.sql"),
                include_str!("../../migrations/0005_loyalty.sql"),
                include_str!("../../migrations/0006_giveaway_entries.sql"),
                include_str!("../../migrations/0007_last_message.sql"),
                include_str!("../../migrations/0008_imports.sql"),
                include_str!("../../migrations/0009_designs.sql"),
            ];
            for (i, sql) in migrations.iter().enumerate() {
                let tx = c.transaction().unwrap();
                tx.execute_batch(sql).unwrap();
                tx.pragma_update(None, "user_version", i as i64 + 1).unwrap();
                tx.commit().unwrap();
            }
            c.execute("INSERT INTO counters(name, value) VALUES ('deaths', 12)", []).unwrap();
            c.execute("INSERT INTO quotes(text, added_by, at) VALUES ('gg', 'Asha', 1)", []).unwrap();
        }
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(bot::counter(&s, "deaths").unwrap(), 12);
        assert_eq!(quotes(&s).unwrap()[0].text, "gg");
        add_marker(&s, MomentKind::Manual, "after the upgrade", None, None, false).unwrap();
        add_watch_time(&s, &[(Platform::Twitch, "1".into())], 5).unwrap();
    }
}
