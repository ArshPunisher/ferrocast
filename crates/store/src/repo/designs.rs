//! The streamer's own designs (API.md §4 Designs). Built-ins live in the app, not here.
//!
//! Every row holds the whole `Design` as JSON; `kind` and `name` are copied out so lists and
//! "is this in use" checks don't parse every document.

use ferrocast_core::design::{Design, DesignKind};
use rusqlite::{OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

/// Settings key for the alert design every alert plays with (`null` = the rules' own look).
pub const ACTIVE_ALERT_KEY: &str = "alert_design";

fn checked(d: &Design) -> Result<()> {
    if d.built_in || d.branded {
        return Err(StoreError::Conflict("Built-in designs ship with the app and can't be saved as your own.".into()));
    }
    d.validate().map_err(|e| StoreError::Conflict(e.to_string()))
}

/// Newest first.
pub fn list(store: &Store) -> Result<Vec<Design>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT data FROM designs ORDER BY updated_at DESC, id")?;
        Ok(st.query_map([], |r| super::from_json(0, r.get(0)?))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get(store: &Store, id: &str) -> Result<Design> {
    store
        .read(|c| {
            Ok(c.query_row("SELECT data FROM designs WHERE id = ?1", [id], |r| super::from_json(0, r.get(0)?))
                .optional()?)
        })?
        .ok_or(StoreError::NotFound("design"))
}

/// Saves a new design. The caller has already made it the streamer's own
/// (`Design::into_user`), so the id is fresh and branding is off.
pub fn insert(store: &Store, d: &Design) -> Result<Design> {
    checked(d)?;
    let json = serde_json::to_string(d)?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO designs(id, kind, name, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![d.id, d.kind().key(), d.name.trim(), json, d.created_at, d.updated_at],
        )?;
        Ok(())
    })?;
    Ok(d.clone())
}

/// Saves an edit. The kind can't change (an alert look doesn't become a theme), and the
/// creation time and origin stay what they were.
pub fn update(store: &Store, d: &Design) -> Result<Design> {
    checked(d)?;
    let current = get(store, &d.id)?;
    if current.kind() != d.kind() {
        return Err(StoreError::Conflict(format!(
            "This design is a{} {} design; it can't become a {} design.",
            if current.kind() == DesignKind::Alert { "n" } else { "" },
            current.kind().key(),
            d.kind().key()
        )));
    }
    let mut saved = d.clone();
    saved.created_at = current.created_at;
    saved.origin = current.origin;
    saved.updated_at = now_ms().max(current.updated_at + 1);
    saved.name = saved.name.trim().to_string();
    // Only built-ins are part of the paid catalogue; the streamer's own never are.
    saved.pro = false;
    let json = serde_json::to_string(&saved)?;
    store.write(|tx| {
        tx.execute(
            "UPDATE designs SET name = ?2, data = ?3, updated_at = ?4 WHERE id = ?1",
            params![saved.id, saved.name, json, saved.updated_at],
        )?;
        Ok(())
    })?;
    Ok(saved)
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM designs WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("design")) } else { Ok(()) }
}

/// The alert design in use, if any.
pub fn active_alert(store: &Store) -> Result<Option<String>> {
    Ok(super::settings::get::<Option<String>>(store, ACTIVE_ALERT_KEY)?.flatten())
}

pub fn set_active_alert(store: &Store, id: Option<&str>) -> Result<()> {
    super::settings::set(store, ACTIVE_ALERT_KEY, &id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_core::design::{Origin, OriginKind};

    fn design(id: &str) -> Design {
        let raw = include_str!("../../../core/fixtures/designs/alert-design.json");
        let mut d: Design = serde_json::from_str(raw).unwrap();
        d.id = id.into();
        d
    }

    #[test]
    fn insert_list_update_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(list(&s).unwrap().is_empty());
        let a = insert(&s, &design("a")).unwrap();
        let mut b = design("b");
        b.updated_at = a.updated_at + 10;
        insert(&s, &b).unwrap();
        assert_eq!(list(&s).unwrap().iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["b", "a"], "newest first");
        let mut edited = get(&s, "a").unwrap();
        edited.name = "  Renamed  ".into();
        edited.origin = Origin { kind: OriginKind::Import, from: Some("forged".into()) };
        edited.created_at = 999;
        edited.pro = true;
        let saved = update(&s, &edited).unwrap();
        assert!(!saved.pro, "your own design is never Pro");
        assert_eq!(saved.name, "Renamed");
        assert_eq!(saved.origin, a.origin, "the origin can't be rewritten");
        assert_eq!(saved.created_at, a.created_at);
        assert!(saved.updated_at > a.updated_at);
        assert_eq!(list(&s).unwrap()[0].id, "a", "an edit moves it to the top");
        delete(&s, "a").unwrap();
        assert!(matches!(get(&s, "a"), Err(StoreError::NotFound("design"))));
        assert!(matches!(delete(&s, "a"), Err(StoreError::NotFound(_))));
        assert!(matches!(update(&s, &design("missing")), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn built_ins_invalid_designs_and_kind_changes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut branded = design("x");
        branded.built_in = true;
        branded.branded = true;
        assert!(matches!(insert(&s, &branded), Err(StoreError::Conflict(_))));
        let mut bad = design("y");
        bad.name = String::new();
        assert!(matches!(insert(&s, &bad), Err(StoreError::Conflict(m)) if m.starts_with("name:")));
        insert(&s, &design("z")).unwrap();
        let scene = include_str!("../../../core/fixtures/designs/scene-design.json");
        let mut other: Design = serde_json::from_str(scene).unwrap();
        other.id = "z".into();
        assert!(matches!(update(&s, &other), Err(StoreError::Conflict(m)) if m.contains("can't become")));
        assert!(matches!(insert(&s, &design("z")), Err(StoreError::Sql(_))), "ids are unique");
    }

    #[test]
    fn the_active_alert_design_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(active_alert(&s).unwrap(), None);
        set_active_alert(&s, Some("builtin.molten")).unwrap();
        assert_eq!(active_alert(&s).unwrap().as_deref(), Some("builtin.molten"));
        set_active_alert(&s, None).unwrap();
        assert_eq!(active_alert(&s).unwrap(), None);
    }
}
