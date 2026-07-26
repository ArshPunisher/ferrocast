//! Reads for the end-of-stream recap.

use ferrocast_core::event::{Event, Platform};
use ferrocast_core::recap::Chatter;
use rusqlite::{OptionalExtension, params};

use crate::Store;
use crate::error::Result;

const SUPPORT: &[&str] =
    &["follow", "subscription", "gift_batch", "tip", "paid_message", "platform_gift", "raid", "reversal"];

/// When the most recent stream started, from the log (after a restart the live session
/// is gone, but the log remembers).
pub fn last_online(store: &Store) -> Result<Option<i64>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT MAX(occurred_at) FROM events WHERE kind = 'stream_online' AND origin = 'live'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
    })
}

/// When the stream that started at `since` went offline, if it did.
pub fn offline_after(store: &Store, since: i64) -> Result<Option<i64>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT MAX(occurred_at) FROM events WHERE kind = 'stream_offline' AND origin = 'live' AND occurred_at >= ?1",
            [since],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
    })
}

/// Support events (and refunds) since `since`, oldest first, at most 5,000.
pub fn support_since(store: &Store, since: i64) -> Result<Vec<Event>> {
    let kinds = serde_json::to_string(SUPPORT)?;
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, platform, source_id, origin, occurred_at, received_at, data, seq FROM events
             WHERE occurred_at >= ?1 AND origin = 'live' AND kind IN (SELECT value FROM json_each(?2))
             ORDER BY seq ASC LIMIT 5000",
        )?;
        let rows = st.query_map(params![since, kinds], |r| {
            let platform: String = r.get(1)?;
            let origin: String = r.get(3)?;
            let seq: i64 = r.get(7)?;
            Ok(Event {
                id: r.get(0)?,
                seq: u64::try_from(seq).unwrap_or(0),
                platform: super::from_json(1, format!("\"{platform}\""))?,
                channel: None,
                source_id: r.get(2)?,
                occurred_at: ferrocast_core::event::Millis(r.get(4)?),
                received_at: ferrocast_core::event::Millis(r.get(5)?),
                origin: super::from_json(3, format!("\"{origin}\""))?,
                kind: super::from_json(6, r.get(6)?)?,
                raw: None,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Chat messages since `since`, and the people who sent the most (never the streamer or
/// bots).
pub fn chat_since(store: &Store, since: i64, top: u32) -> Result<(u64, Vec<Chatter>)> {
    store.read(|c| {
        let total: i64 = c.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'chat_message' AND origin = 'live' AND occurred_at >= ?1",
            [since],
            |r| r.get(0),
        )?;
        let mut st = c.prepare(
            "SELECT platform, MAX(actor_name), COUNT(*) AS n FROM events
             WHERE kind = 'chat_message' AND origin = 'live' AND occurred_at >= ?1 AND actor_id IS NOT NULL
               AND (COALESCE(json_extract(data, '$.actor.roles'), 0) & 257) = 0
             GROUP BY platform, actor_id ORDER BY n DESC, MAX(seq) DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![since, top], |r| {
            let platform: String = r.get(0)?;
            let n: i64 = r.get(2)?;
            Ok(Chatter {
                platform: super::from_json::<Platform>(0, format!("\"{platform}\""))?,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                messages: u64::try_from(n).unwrap_or(0),
            })
        })?;
        Ok((u64::try_from(total).unwrap_or(0), rows.collect::<rusqlite::Result<Vec<_>>>()?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::events::insert;
    use ferrocast_core::event::{Actor, Content, EventKind, Millis, Roles};

    fn chat(id: &str, user: &str, roles: Roles, at: i64) -> Event {
        let mut a = Actor::named(Platform::Twitch, user);
        a.id = Some(user.into());
        a.roles = roles;
        let mut e = Event::live(
            Platform::Twitch,
            Some(id.into()),
            Millis(at),
            EventKind::ChatMessage { actor: a, content: Content::plain("hi"), reply_to: None, is_action: false },
        );
        e.id = id.into();
        e
    }

    #[test]
    fn counts_chat_since_the_start_and_ranks_people_not_the_streamer() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        insert(&s, &chat("old", "ana", Roles::empty(), 5)).unwrap();
        for (i, u) in ["ana", "bo", "bo", "me", "me", "me", "bot"].iter().enumerate() {
            let roles = match *u {
                "me" => Roles::BROADCASTER,
                "bot" => Roles::BOT,
                _ => Roles::empty(),
            };
            insert(&s, &chat(&format!("m{i}"), u, roles, 100)).unwrap();
        }
        let mut on =
            Event::live(Platform::Twitch, Some("on".into()), Millis(100), EventKind::StreamOnline { title: None });
        on.id = "on".into();
        insert(&s, &on).unwrap();
        let (total, top) = chat_since(&s, 100, 10).unwrap();
        assert_eq!(total, 7);
        assert_eq!(top.iter().map(|c| (c.name.as_str(), c.messages)).collect::<Vec<_>>(), [("bo", 2), ("ana", 1)]);
        assert_eq!(last_online(&s).unwrap(), Some(100));
        assert_eq!(offline_after(&s, 100).unwrap(), None);
        assert!(support_since(&s, 0).unwrap().is_empty());
    }
}
