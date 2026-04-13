//! The event log: every normalised event, deduped by platform id (R-29), plus the TTS
//! approval list.

use ferrocast_core::event::{Event, Platform};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::{Store, now_ms};

fn platform_key(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::Kick => "kick",
        Platform::Tips => "tips",
        Platform::Ferrocast => "ferrocast",
    }
}

fn origin_key(e: &Event) -> &'static str {
    match e.origin {
        ferrocast_core::event::Origin::Live => "live",
        ferrocast_core::event::Origin::Backfill => "backfill",
        ferrocast_core::event::Origin::Replay => "replay",
        ferrocast_core::event::Origin::Test => "test",
    }
}

/// Stores an event and returns its log sequence number, or `None` if the platform already
/// delivered it (same platform + source id). Test events aren't stored: they'd pollute
/// session stats and "recent events" lists.
pub fn insert(store: &Store, e: &Event) -> Result<Option<i64>> {
    if e.origin == ferrocast_core::event::Origin::Test {
        return Ok(None);
    }
    let data = serde_json::to_string(&e.kind)?;
    let actor = e.kind.actor();
    store.write(|tx| {
        let n = tx.execute(
            "INSERT OR IGNORE INTO events(id, platform, source_id, kind, origin, occurred_at, received_at, actor_id, actor_name, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                e.id,
                platform_key(e.platform),
                e.source_id.as_deref().filter(|s| !s.is_empty()),
                e.kind.name(),
                origin_key(e),
                e.occurred_at.0,
                e.received_at.0,
                actor.and_then(|a| a.id.clone()),
                actor.map(|a| a.display_name.clone()),
                data,
            ],
        )?;
        Ok(if n == 0 { None } else { Some(tx.last_insert_rowid()) })
    })
}

/// A page of events, newest first. `before` is a log sequence number; `kinds` filters by
/// kind name. The limit is clamped to 1..=500.
pub fn recent(
    store: &Store,
    before: Option<i64>,
    limit: u32,
    kinds: &[String],
) -> Result<Vec<(i64, Event)>> {
    page(store, before, limit, kinds, false)
}

/// Like [`recent`] but without messages moderation removed: what overlays may show.
pub fn recent_visible(
    store: &Store,
    before: Option<i64>,
    limit: u32,
    kinds: &[String],
) -> Result<Vec<(i64, Event)>> {
    page(store, before, limit, kinds, true)
}

fn page(
    store: &Store,
    before: Option<i64>,
    limit: u32,
    kinds: &[String],
    visible_only: bool,
) -> Result<Vec<(i64, Event)>> {
    let limit = i64::from(limit.clamp(1, 500));
    store.read(|c| {
        let kinds_json = serde_json::to_string(kinds)?;
        let mut st = c.prepare(
            "SELECT seq, id, platform, source_id, origin, occurred_at, received_at, data FROM events
             WHERE (?1 IS NULL OR seq < ?1)
               AND (json_array_length(?2) = 0 OR kind IN (SELECT value FROM json_each(?2)))
               AND (?4 = 0 OR hidden = 0)
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![before, kinds_json, limit, visible_only], |r| {
            let seq: i64 = r.get(0)?;
            let platform: String = r.get(2)?;
            let origin: String = r.get(4)?;
            let event = Event {
                id: r.get(1)?,
                seq: u64::try_from(seq).unwrap_or(0),
                platform: super::from_json(2, format!("\"{platform}\""))?,
                channel: None,
                source_id: r.get(3)?,
                occurred_at: ferrocast_core::event::Millis(r.get(5)?),
                received_at: ferrocast_core::event::Millis(r.get(6)?),
                origin: super::from_json(4, format!("\"{origin}\""))?,
                kind: super::from_json(7, r.get(7)?)?,
                raw: None,
            };
            Ok((seq, event))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Looks an event up by Ferrocast id.
pub fn get(store: &Store, id: &str) -> Result<Option<Event>> {
    let seq: Option<i64> = store.read(|c| {
        Ok(
            c.query_row("SELECT seq FROM events WHERE id = ?1", [id], |r| r.get(0))
                .optional()?,
        )
    })?;
    let Some(seq) = seq else { return Ok(None) };
    Ok(recent(store, Some(seq + 1), 1, &[])?
        .into_iter()
        .next()
        .map(|(_, e)| e))
}

/// Marks a message hidden (filtered, deleted by a mod, from a banned account) so it never
/// returns in overlay snapshots, even after a restart. Returns whether a row changed.
pub fn hide(store: &Store, platform: Platform, source_id: &str) -> Result<bool> {
    store.write(|tx| {
        Ok(tx.execute(
            "UPDATE events SET hidden = 1 WHERE platform = ?1 AND source_id = ?2 AND hidden = 0",
            params![platform_key(platform), source_id],
        )? > 0)
    })
}

/// Hides a person's chat since `since_ms` (they were banned or their messages cleared).
pub fn hide_user_chat(
    store: &Store,
    platform: Platform,
    actor_id: &str,
    since_ms: i64,
) -> Result<usize> {
    store.write(|tx| {
        Ok(tx.execute(
            "UPDATE events SET hidden = 1
             WHERE platform = ?1 AND actor_id = ?2 AND kind = 'chat_message' AND received_at >= ?3 AND hidden = 0",
            params![platform_key(platform), actor_id, since_ms],
        )?)
    })
}

/// Finds the event a refund refers to.
pub fn by_source(store: &Store, platform: Platform, source_id: &str) -> Result<Option<String>> {
    store.read(|c| {
        Ok(c.query_row(
            "SELECT id FROM events WHERE platform = ?1 AND source_id = ?2",
            params![platform_key(platform), source_id],
            |r| r.get(0),
        )
        .optional()?)
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTts {
    pub id: String,
    pub event_id: String,
    pub text: String,
    pub created_at: i64,
}

pub fn add_pending_tts(store: &Store, p: &PendingTts) -> Result<()> {
    store.write(|tx| {
        tx.execute(
            "INSERT OR REPLACE INTO tts_pending(id, event_id, text, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![p.id, p.event_id, p.text, p.created_at],
        )?;
        Ok(())
    })
}

pub fn pending_tts(store: &Store) -> Result<Vec<PendingTts>> {
    store.read(|c| {
        let mut st = c.prepare(
            "SELECT id, event_id, text, created_at FROM tts_pending ORDER BY created_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PendingTts {
                id: r.get(0)?,
                event_id: r.get(1)?,
                text: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

/// Removes and returns a pending item (approve or reject both take it off the list).
pub fn take_pending_tts(store: &Store, id: &str) -> Result<Option<PendingTts>> {
    store.write(|tx| {
        let p = tx
            .query_row(
                "SELECT id, event_id, text, created_at FROM tts_pending WHERE id = ?1",
                [id],
                |r| {
                    Ok(PendingTts {
                        id: r.get(0)?,
                        event_id: r.get(1)?,
                        text: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?;
        tx.execute("DELETE FROM tts_pending WHERE id = ?1", [id])?;
        Ok(p)
    })
}

/// Drops pending TTS for an event (a mod deleted the message).
pub fn drop_pending_tts_for_event(store: &Store, event_id: &str) -> Result<usize> {
    store.write(|tx| Ok(tx.execute("DELETE FROM tts_pending WHERE event_id = ?1", [event_id])?))
}

/// Deletes chat older than `days` (alerts and tips are kept for history and taxes).
pub fn prune_chat(store: &Store, days: u32) -> Result<usize> {
    let cutoff = now_ms() - i64::from(days) * 86_400_000;
    store.write(|tx| {
        Ok(tx.execute(
            "DELETE FROM events WHERE kind = 'chat_message' AND received_at < ?1",
            [cutoff],
        )?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::event::{Actor, Content, EventKind, Millis, Origin};

    fn chat(id: &str, source: Option<&str>, text: &str) -> Event {
        let mut e = Event::live(
            Platform::Twitch,
            source.map(String::from),
            Millis(1),
            EventKind::ChatMessage {
                actor: Actor::named(Platform::Twitch, "v"),
                content: Content::plain(text),
                reply_to: None,
                is_action: false,
            },
        );
        e.id = id.into();
        e
    }

    #[test]
    fn duplicates_by_source_are_ignored_but_idless_events_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(insert(&s, &chat("a", Some("m1"), "hi")).unwrap().is_some());
        assert!(
            insert(&s, &chat("b", Some("m1"), "hi again"))
                .unwrap()
                .is_none()
        );
        assert!(insert(&s, &chat("c", None, "x")).unwrap().is_some());
        assert!(insert(&s, &chat("d", None, "x")).unwrap().is_some());
        assert!(
            insert(&s, &chat("e", Some(""), "empty id counts as none"))
                .unwrap()
                .is_some()
        );
        assert!(
            insert(&s, &chat("f", Some(""), "empty id counts as none"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_events_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut e = chat("t", None, "x");
        e.origin = Origin::Test;
        assert!(insert(&s, &e).unwrap().is_none());
        assert!(recent(&s, None, 10, &[]).unwrap().is_empty());
    }

    #[test]
    fn paging_and_kind_filters() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for i in 0..30 {
            insert(&s, &chat(&format!("c{i}"), Some(&format!("m{i}")), "x")).unwrap();
        }
        let mut f = Event::live(
            Platform::Kick,
            Some("f1".into()),
            Millis(2),
            EventKind::Follow {
                actor: Actor::named(Platform::Kick, "fan"),
            },
        );
        f.id = "follow".into();
        insert(&s, &f).unwrap();
        let page = recent(&s, None, 10, &[]).unwrap();
        assert_eq!(page.len(), 10);
        assert_eq!(page[0].1.id, "follow");
        let next = recent(&s, Some(page.last().unwrap().0), 10, &[]).unwrap();
        assert!(next[0].0 < page.last().unwrap().0);
        let follows = recent(&s, None, 50, &["follow".into()]).unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0].1.platform, Platform::Kick);
        assert_eq!(
            recent(&s, None, 0, &[]).unwrap().len(),
            1,
            "limit clamps up to 1"
        );
        assert_eq!(
            recent(&s, None, 100_000, &["nonsense' OR 1=1".into()])
                .unwrap()
                .len(),
            0
        );
        assert_eq!(get(&s, "follow").unwrap().unwrap().kind.name(), "follow");
        assert_eq!(
            by_source(&s, Platform::Kick, "f1").unwrap().as_deref(),
            Some("follow")
        );
    }

    #[test]
    fn hidden_messages_stay_out_of_visible_pages() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut a = chat("a", Some("m1"), "spam");
        if let EventKind::ChatMessage { actor, .. } = &mut a.kind {
            actor.id = Some("troll".into());
        }
        insert(&s, &a).unwrap();
        insert(&s, &chat("b", Some("m2"), "nice")).unwrap();
        insert(&s, &chat("c", Some("m3"), "also fine")).unwrap();
        assert!(hide(&s, Platform::Twitch, "m3").unwrap());
        assert!(!hide(&s, Platform::Twitch, "m3").unwrap(), "already hidden");
        assert_eq!(hide_user_chat(&s, Platform::Twitch, "troll", 0).unwrap(), 1);
        let visible: Vec<_> = recent_visible(&s, None, 50, &[])
            .unwrap()
            .into_iter()
            .map(|(_, e)| e.id)
            .collect();
        assert_eq!(visible, vec!["b"]);
        assert_eq!(
            recent(&s, None, 50, &[]).unwrap().len(),
            3,
            "the mod log keeps everything"
        );
    }

    #[test]
    fn tts_pending_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let p = PendingTts {
            id: "t1".into(),
            event_id: "e1".into(),
            text: "hi".into(),
            created_at: 1,
        };
        add_pending_tts(&s, &p).unwrap();
        add_pending_tts(
            &s,
            &PendingTts {
                id: "t2".into(),
                ..p.clone()
            },
        )
        .unwrap();
        assert_eq!(pending_tts(&s).unwrap().len(), 2);
        assert_eq!(take_pending_tts(&s, "t1").unwrap(), Some(p));
        assert_eq!(take_pending_tts(&s, "t1").unwrap(), None);
        assert_eq!(drop_pending_tts_for_event(&s, "e1").unwrap(), 1);
        assert!(pending_tts(&s).unwrap().is_empty());
    }
}
