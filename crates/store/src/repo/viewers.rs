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
                v
            }
        };
        Ok(Some(viewer_id))
    })
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
            "SELECT platform, user_id, login, display_name, first_seen, last_seen, messages
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ViewerCard { viewer_id, note, identities })
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
