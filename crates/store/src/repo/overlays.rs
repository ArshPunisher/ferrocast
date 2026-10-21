//! Overlays and their secret URL tokens.

use ferrocast_core::overlay::{Overlay, Preset, preset_widgets};
use rusqlite::{OptionalExtension, Row, params};

use crate::error::{Result, StoreError};
use crate::{Store, new_id, new_token, now_ms};

const COLS: &str = "id, name, token, width, height, widgets, updated_at, design";

fn row(r: &Row<'_>) -> rusqlite::Result<Overlay> {
    Ok(Overlay {
        id: r.get(0)?,
        name: r.get(1)?,
        token: r.get(2)?,
        width: r.get(3)?,
        height: r.get(4)?,
        widgets: super::from_json(5, r.get(5)?)?,
        updated_at: r.get(6)?,
        design: match r.get::<_, Option<String>>(7)? {
            Some(raw) => super::from_json(7, raw)?,
            None => None,
        },
    })
}

pub fn list(store: &Store) -> Result<Vec<Overlay>> {
    store.read(|c| {
        let mut st = c.prepare(&format!("SELECT {COLS} FROM overlays ORDER BY created_at"))?;
        Ok(st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get(store: &Store, id: &str) -> Result<Overlay> {
    store
        .read(|c| Ok(c.query_row(&format!("SELECT {COLS} FROM overlays WHERE id = ?1"), [id], row).optional()?))?
        .ok_or(StoreError::NotFound("overlay"))
}

/// The overlay for a URL token. Compared in SQL by exact match; tokens are 128-bit random.
pub fn by_token(store: &Store, token: &str) -> Result<Option<Overlay>> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    store
        .read(|c| Ok(c.query_row(&format!("SELECT {COLS} FROM overlays WHERE token = ?1"), [token], row).optional()?))
}

/// Creates an overlay from a preset with a fresh token.
pub fn create(store: &Store, name: &str, preset: Preset) -> Result<Overlay> {
    let o = Overlay {
        id: new_id(),
        name: name.trim().to_string(),
        token: new_token(),
        width: 1920,
        height: 1080,
        widgets: preset_widgets(preset),
        updated_at: now_ms(),
        design: None,
    };
    insert(store, &o)?;
    Ok(o)
}

/// Creates an overlay from a full layout (a gallery scene), with a fresh id and token.
pub fn create_from(store: &Store, mut o: Overlay) -> Result<Overlay> {
    o.id = new_id();
    o.token = new_token();
    o.name = o.name.trim().to_string();
    o.updated_at = now_ms();
    insert(store, &o)?;
    Ok(o)
}

fn insert(store: &Store, o: &Overlay) -> Result<()> {
    o.validate().map_err(|e| StoreError::Conflict(e.to_string()))?;
    let widgets = serde_json::to_string(&o.widgets)?;
    let design = o.design.as_ref().map(serde_json::to_string).transpose()?;
    store.write(|tx| {
        tx.execute(
            "INSERT INTO overlays(id, name, token, width, height, widgets, created_at, updated_at, design)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
            params![o.id, o.name, o.token, o.width, o.height, widgets, o.updated_at, design],
        )?;
        Ok(())
    })
}

/// An overlay from cloud sync (docs/CLOUD.md §1.1) under its own id: a new one gets a fresh
/// token here, an existing one keeps this PC's token (so OBS keeps working).
pub fn put_synced(store: &Store, mut o: Overlay) -> Result<Overlay> {
    match get(store, &o.id) {
        Ok(current) => {
            o.token = current.token;
            update(store, &o)
        }
        Err(StoreError::NotFound(_)) => {
            o.token = new_token();
            o.name = o.name.trim().to_string();
            o.updated_at = now_ms();
            insert(store, &o)?;
            Ok(o)
        }
        Err(e) => Err(e),
    }
}

/// Saves name, size, widgets and the design link. The token can't be changed here, only
/// regenerated.
pub fn update(store: &Store, o: &Overlay) -> Result<Overlay> {
    o.validate().map_err(|e| StoreError::Conflict(e.to_string()))?;
    let widgets = serde_json::to_string(&o.widgets)?;
    let design = o.design.as_ref().map(serde_json::to_string).transpose()?;
    let n = store.write(|tx| {
        Ok(tx.execute(
            "UPDATE overlays SET name = ?2, width = ?3, height = ?4, widgets = ?5, updated_at = ?6, design = ?7
             WHERE id = ?1",
            params![o.id, o.name.trim(), o.width, o.height, widgets, now_ms(), design],
        )?)
    })?;
    if n == 0 {
        return Err(StoreError::NotFound("overlay"));
    }
    get(store, &o.id)
}

pub fn regenerate_token(store: &Store, id: &str) -> Result<Overlay> {
    let n = store.write(|tx| {
        Ok(tx.execute(
            "UPDATE overlays SET token = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, new_token(), now_ms()],
        )?)
    })?;
    if n == 0 {
        return Err(StoreError::NotFound("overlay"));
    }
    get(store, id)
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM overlays WHERE id = ?1", [id])?))?;
    if n == 0 { Err(StoreError::NotFound("overlay")) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_update_regenerate_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let o = create(&s, " Main ", Preset::Starter).unwrap();
        assert_eq!(o.name, "Main");
        assert_eq!(by_token(&s, &o.token).unwrap().unwrap().id, o.id);
        let mut changed = o.clone();
        changed.name = "Gaming".into();
        changed.token = "ffffffffffffffffffffffffffffffff".into();
        let saved = update(&s, &changed).unwrap();
        assert_eq!(saved.name, "Gaming");
        assert_eq!(saved.token, o.token, "update can't set the token");
        let fresh = regenerate_token(&s, &o.id).unwrap();
        assert_ne!(fresh.token, o.token);
        assert!(by_token(&s, &o.token).unwrap().is_none(), "old URL stops working");
        delete(&s, &o.id).unwrap();
        assert!(matches!(get(&s, &o.id), Err(StoreError::NotFound(_))));
        assert!(matches!(delete(&s, &o.id), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn the_design_link_is_saved_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mut o = create(&s, "Scene", Preset::Blank).unwrap();
        assert_eq!(o.design, None);
        o.design =
            Some(ferrocast_core::overlay::DesignRef { id: "builtin.forge".into(), scene: "brb".into(), branded: true });
        let saved = update(&s, &o).unwrap();
        assert_eq!(saved.design, o.design);
        let made = create_from(&s, saved.clone()).unwrap();
        assert_ne!(made.id, saved.id);
        assert_ne!(made.token, saved.token, "a copy never shares the secret URL");
        assert_eq!(get(&s, &made.id).unwrap().design, o.design);
    }

    #[test]
    fn invalid_overlays_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(create(&s, "   ", Preset::Blank).is_err());
        let mut o = create(&s, "ok", Preset::AlertsOnly).unwrap();
        o.widgets[0].x = 5_000;
        assert!(update(&s, &o).is_err());
    }

    #[test]
    fn junk_tokens_never_hit_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for t in ["", "' OR 1=1 --", "../../x", &"a".repeat(32 * 100), "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(by_token(&s, t).unwrap().is_none());
        }
    }
}
