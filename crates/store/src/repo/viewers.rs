//! Viewers: one person, possibly several platform accounts, with a mod note.

use ferrocast_core::event::{Actor, Platform};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

fn pkey(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerCard {
    pub viewer_id: i64,
    pub note: String,
    pub identities: Vec<Identity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub platform: String,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub messages: i64,
    #[serde(default)]
    pub timeouts: i64,
}

/// Records that `actor` was seen (and chatted, if `chatted`). Returns the viewer id.
/// Actors without an id (anonymous tips) aren't tracked.
pub fn touch(store: &Store, actor: &Actor, chatted: bool) -> Result<Option<i64>> {
    let Some(user_id) = actor.id.as_deref() else {
        return Ok(None);
    };
    let now = now_ms();
    let login = actor.login.clone().unwrap_or_else(|| actor.display_name.clone());
    store.write(|tx| {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT viewer_id FROM viewer_identities WHERE platform = ?1 AND user_id = ?2",
                params![pkey(actor.platform), user_id],
                |r| r.get(0),
            )
            .optional()?;
        let viewer_id = match existing {
            Some(v) => {
                tx.execute(
                    "UPDATE viewer_identities SET login = ?3, display_name = ?4, last_seen = ?5, messages = messages + ?6
                     WHERE platform = ?1 AND user_id = ?2",
                    params![pkey(actor.platform), user_id, login, actor.display_name, now, i64::from(chatted)],
                )?;
                v
            }
            None => {
                tx.execute("INSERT INTO viewers(note) VALUES ('')", [])?;
                let v = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO viewer_identities(platform, user_id, viewer_id, login, display_name, first_seen, last_seen, messages)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                    params![pkey(actor.platform), user_id, v, login, actor.display_name, now, i64::from(chatted)],
                )?;
                // Points an import is holding for this name are theirs now.
                // Only a real login counts here, never a display name standing in for one.
                crate::import::claim_pending(tx, &pkey(actor.platform), user_id, actor.login.as_deref(), v)?;
                v
            }
        };
        Ok(Some(viewer_id))
    })
}

/// What Ferrocast knew about a person just before this chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatTouch {
    pub viewer_id: i64,
    /// Their first message ever, on any of their linked accounts.
    pub first_message: bool,
    /// When they last chatted before this, on any linked account.
    pub previous_message_at: Option<i64>,
}

/// [`touch`] for a chat message, also telling whether it's the person's first message
/// ever and when they last chatted. `None` for actors without an id.
pub fn touch_chat(store: &Store, actor: &Actor) -> Result<Option<ChatTouch>> {
    let Some(user_id) = actor.id.as_deref() else {
        return Ok(None);
    };
    let before: Option<(i64, Option<i64>, i64)> = store.read(|c| {
        Ok(c.query_row(
            "SELECT i.viewer_id, MAX(o.last_message_at), SUM(o.messages)
             FROM viewer_identities i JOIN viewer_identities o ON o.viewer_id = i.viewer_id
             WHERE i.platform = ?1 AND i.user_id = ?2 GROUP BY i.viewer_id",
            params![pkey(actor.platform), user_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
    })?;
    let Some(viewer_id) = touch(store, actor, true)? else { return Ok(None) };
    store.write(|tx| {
        tx.execute(
            "UPDATE viewer_identities SET last_message_at = ?3 WHERE platform = ?1 AND user_id = ?2",
            params![pkey(actor.platform), user_id, now_ms()],
        )?;
        Ok(())
    })?;
    let (previous_message_at, messages) = match before {
        Some((_, at, n)) => (at, n),
        None => (None, 0),
    };
    Ok(Some(ChatTouch { viewer_id, first_message: messages == 0, previous_message_at }))
}

pub fn card(store: &Store, platform: Platform, user_id: &str) -> Result<ViewerCard> {
    store.read(|c| {
        let viewer_id: i64 = c
            .query_row(
                "SELECT viewer_id FROM viewer_identities WHERE platform = ?1 AND user_id = ?2",
                params![pkey(platform), user_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound("viewer"))?;
        let note: String = c.query_row("SELECT note FROM viewers WHERE id = ?1", [viewer_id], |r| r.get(0))?;
        let mut st = c.prepare(
            "SELECT platform, user_id, login, display_name, first_seen, last_seen, messages, timeouts
             FROM viewer_identities WHERE viewer_id = ?1 ORDER BY first_seen",
        )?;
        let identities = st
            .query_map([viewer_id], |r| {
                Ok(Identity {
                    platform: r.get(0)?,
                    user_id: r.get(1)?,
                    login: r.get(2)?,
                    display_name: r.get(3)?,
                    first_seen: r.get(4)?,
                    last_seen: r.get(5)?,
                    messages: r.get(6)?,
                    timeouts: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ViewerCard { viewer_id, note, identities })
    })
}

/// Counts a timeout we gave this person. `false` if we've never seen them.
pub fn add_timeout(store: &Store, platform: Platform, user_id: &str) -> Result<bool> {
    store.write(|tx| {
        Ok(tx.execute(
            "UPDATE viewer_identities SET timeouts = timeouts + 1 WHERE platform = ?1 AND user_id = ?2",
            params![pkey(platform), user_id],
        )? > 0)
    })
}

pub fn set_note(store: &Store, platform: Platform, user_id: &str, note: &str) -> Result<()> {
    if note.chars().count() > 2_000 {
        return Err(StoreError::Conflict("notes can be up to 2,000 characters".into()));
    }
    let v = card(store, platform, user_id)?.viewer_id;
    store.write(|tx| {
        tx.execute("UPDATE viewers SET note = ?2 WHERE id = ?1", params![v, note])?;
        Ok(())
    })
}

/// Links `(platform, user_id)` to the viewer that owns `(to_platform, to_user_id)`, so the
/// two accounts share a note and a ban. The old viewer row is dropped if it's now empty.
pub fn link(store: &Store, platform: Platform, user_id: &str, to_platform: Platform, to_user_id: &str) -> Result<()> {
    let from = card(store, platform, user_id)?.viewer_id;
    let to = card(store, to_platform, to_user_id)?.viewer_id;
    if from == to {
        return Ok(());
    }
    store.write(|tx| {
        tx.execute(
            "UPDATE viewer_identities SET viewer_id = ?1 WHERE platform = ?2 AND user_id = ?3",
            params![to, pkey(platform), user_id],
        )?;
        // When the old person row is left empty, its points move to the person it joined
        // (capped, never overflowing) before the row and its balance go.
        let emptied: bool =
            tx.query_row("SELECT NOT EXISTS (SELECT 1 FROM viewer_identities WHERE viewer_id = ?1)", [from], |r| {
                r.get(0)
            })?;
        if emptied {
            tx.execute(
                "INSERT INTO loyalty_balances(viewer_id, points, updated_at)
                   SELECT ?1, points, ?3 FROM loyalty_balances WHERE viewer_id = ?2
                 ON CONFLICT(viewer_id) DO UPDATE SET points = MIN(points + excluded.points, ?4),
                   updated_at = excluded.updated_at",
                params![to, from, now_ms(), ferrocast_core::loyalty::MAX_BALANCE],
            )?;
        }
        tx.execute(
            "DELETE FROM viewers WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM viewer_identities WHERE viewer_id = ?1)",
            [from],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(p: Platform, id: &str, name: &str) -> Actor {
        let mut a = Actor::named(p, name);
        a.id = Some(id.into());
        a
    }

    #[test]
    fn touch_counts_messages_and_tracks_renames() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let v = touch(&s, &actor(Platform::Twitch, "1", "Old"), true).unwrap().unwrap();
        assert_eq!(touch(&s, &actor(Platform::Twitch, "1", "New"), true).unwrap(), Some(v));
        touch(&s, &actor(Platform::Twitch, "1", "New"), false).unwrap();
        let c = card(&s, Platform::Twitch, "1").unwrap();
        assert_eq!(c.identities[0].display_name, "New");
        assert_eq!(c.identities[0].messages, 2);
        assert_eq!(touch(&s, &Actor::anonymous(Platform::Tips), false).unwrap(), None);
    }

    #[test]
    fn first_messages_count_across_linked_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        // Seen through a follow first: that isn't a message.
        touch(&s, &actor(Platform::Twitch, "t", "Sam"), false).unwrap();
        let first = touch_chat(&s, &actor(Platform::Twitch, "t", "Sam")).unwrap().unwrap();
        assert!(first.first_message);
        assert_eq!(first.previous_message_at, None);
        let second = touch_chat(&s, &actor(Platform::Twitch, "t", "Sam")).unwrap().unwrap();
        assert!(!second.first_message);
        assert!(second.previous_message_at.is_some());
        // Sam's Kick account, once linked, is the same person: not a first message.
        touch(&s, &actor(Platform::Kick, "k", "sam_k"), false).unwrap();
        link(&s, Platform::Kick, "k", Platform::Twitch, "t").unwrap();
        let kick = touch_chat(&s, &actor(Platform::Kick, "k", "sam_k")).unwrap().unwrap();
        assert!(!kick.first_message);
        assert_eq!(touch_chat(&s, &Actor::anonymous(Platform::Tips)).unwrap(), None);
    }

    #[test]
    fn notes_and_links_are_shared() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        touch(&s, &actor(Platform::Twitch, "t", "Sam"), true).unwrap();
        touch(&s, &actor(Platform::Kick, "k", "sam_k"), true).unwrap();
        set_note(&s, Platform::Twitch, "t", "regular, plays support").unwrap();
        link(&s, Platform::Kick, "k", Platform::Twitch, "t").unwrap();
        let c = card(&s, Platform::Kick, "k").unwrap();
        assert_eq!(c.note, "regular, plays support");
        assert_eq!(c.identities.len(), 2);
        let n: i64 = s.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM viewers", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 1, "the empty viewer row is removed");
        assert!(set_note(&s, Platform::Twitch, "t", &"x".repeat(2_001)).is_err());
        assert!(matches!(card(&s, Platform::Youtube, "nobody"), Err(StoreError::NotFound(_))));
    }
}
