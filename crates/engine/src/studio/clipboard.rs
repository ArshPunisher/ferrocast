//! The Studio's clipboard: copy a source (a group with its sources) or a list of filters in one
//! collection and paste it in another. The engine holds the copy as plain data, so it survives
//! a collection switch; the desktop reads it out and puts it in (`StudioAct::Copy*`/`Paste*`).

use serde_json::{Value, json};

use crate::studio_host::{ItemTarget, StudioAct};
use crate::{Engine, EngineError};

/// What was copied.
#[derive(Debug, Clone, PartialEq)]
pub enum Clip {
    Source { value: Value, name: String, from: String, sources: usize },
    Filters { value: Value, name: String, from: String, count: usize },
}

impl Clip {
    /// What the dashboard shows (never the copied settings themselves).
    pub fn summary(&self) -> Value {
        match self {
            Clip::Source { name, from, sources, .. } => {
                json!({ "kind": "source", "name": name, "from": from, "count": sources })
            }
            Clip::Filters { name, from, count, .. } => {
                json!({ "kind": "filters", "name": name, "from": from, "count": count })
            }
        }
    }
}

fn empty() -> EngineError {
    EngineError::Missing {
        code: "clipboard_empty",
        message: "Nothing is copied yet.",
        hint: "Copy a source or its filters first, then paste.",
    }
}

fn wrong(holds: &str, wanted: &str) -> EngineError {
    EngineError::Invalid(format!("The clipboard holds {holds}, not {wanted}: copy {wanted} first."))
}

/// A source copied as plain data: its first source's name and how many sources it carries.
pub(crate) fn source_clip(value: Value, from: &str) -> Result<Clip, EngineError> {
    let sources = value.get("sources").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let name = value.pointer("/sources/0/name").and_then(Value::as_str).map(str::to_string);
    match (sources, name) {
        (n, Some(name)) if n > 0 => Ok(Clip::Source { value, name, from: from.into(), sources: n }),
        _ => Err(EngineError::Internal("the Studio sent back a copy with no source in it".into())),
    }
}

pub(crate) fn filter_clip(value: Value) -> Result<Clip, EngineError> {
    let count = value.get("filters").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let from = value.get("from").and_then(Value::as_str).unwrap_or("").to_string();
    if count == 0 {
        return Err(EngineError::Invalid(format!("\"{from}\" has no filters to copy.")));
    }
    Ok(Clip::Filters { value, name: from.clone(), from, count })
}

impl Engine {
    fn clip_now(&self) -> Option<Clip> {
        self.studio().clipboard.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn clip_set(&self, c: Option<Clip>) {
        *self.studio().clipboard.lock().unwrap_or_else(|p| p.into_inner()) = c;
    }

    /// `GET /studio/engine/clipboard`: what is copied. 404 `clipboard_empty` when nothing is.
    pub fn studio_clipboard(&self) -> Result<Value, EngineError> {
        self.clip_now().map(|c| c.summary()).ok_or_else(empty)
    }

    /// `DELETE /studio/engine/clipboard`.
    pub fn studio_clipboard_clear(&self) {
        self.clip_set(None);
    }

    /// Copies an item and its source(s).
    pub async fn studio_copy_source(&self, scene: String, id: i64) -> Result<Value, EngineError> {
        let out = self.studio_act(StudioAct::CopySource { scene: scene.clone(), id }).await?;
        let clip = source_clip(out, &scene)?;
        let summary = clip.summary();
        self.clip_set(Some(clip));
        Ok(summary)
    }

    /// Copies a source's (or an item's own) filters.
    pub async fn studio_copy_filters(&self, name: String, item: Option<ItemTarget>) -> Result<Value, EngineError> {
        let out = self.studio_act(StudioAct::CopyFilters { name, item }).await?;
        let clip = filter_clip(out)?;
        let summary = clip.summary();
        self.clip_set(Some(clip));
        Ok(summary)
    }

    /// Pastes the copied source into `scene` → `{ item, notes }`.
    pub async fn studio_paste_source(&self, scene: String) -> Result<Value, EngineError> {
        match self.clip_now() {
            None => Err(empty()),
            Some(Clip::Filters { .. }) => Err(wrong("filters", "a source")),
            Some(Clip::Source { value, .. }) => self.studio_act(StudioAct::PasteSource { scene, clip: value }).await,
        }
    }

    /// Pastes the copied filters onto a source or an item → `{ notes }`.
    pub async fn studio_paste_filters(&self, name: String, item: Option<ItemTarget>) -> Result<Value, EngineError> {
        match self.clip_now() {
            None => Err(empty()),
            Some(Clip::Source { .. }) => Err(wrong("a source", "filters")),
            Some(Clip::Filters { value, .. }) => {
                self.studio_act(StudioAct::PasteFilterClip { name, item, clip: value }).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_copy_needs_a_source_in_it() {
        assert!(source_clip(json!({ "item": {}, "sources": [] }), "Main").is_err());
        assert!(source_clip(json!({}), "Main").is_err());
        let c = source_clip(json!({ "item": {}, "sources": [{ "name": "Cam" }, { "name": "Mic" }] }), "Main").unwrap();
        assert_eq!(c.summary(), json!({ "kind": "source", "name": "Cam", "from": "Main", "count": 2 }));
    }

    #[test]
    fn no_filters_is_not_a_copy() {
        let e = filter_clip(json!({ "filters": [], "from": "Mic" })).unwrap_err();
        assert!(matches!(e, EngineError::Invalid(m) if m.contains("Mic")));
        let c = filter_clip(json!({ "filters": [{ "name": "Gate" }], "from": "Mic" })).unwrap();
        assert_eq!(c.summary(), json!({ "kind": "filters", "name": "Mic", "from": "Mic", "count": 1 }));
    }
}
