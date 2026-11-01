//! Undo and redo, as OBS's Edit menu has them. Sources in scenes: moving, resizing and Edit
//! Transform, show and hide, lock, order, fit, flips and turns, blending and scaling, adding,
//! duplicating and removing. Scenes: adding, copying, removing, reordering, renaming and their
//! own transition. Filters (every change), a source's settings, and its sound (volume, mute,
//! monitoring, balance, sync, mono, tracks).
//!
//! Each step keeps the whole collection as it was before and after (the scene store hands it
//! over as one document), so undoing and redoing put back exactly what was there: a removed
//! source or scene with everything it had, ids and all.

use std::time::{Duration, Instant};

use ferrocast_engine::studio_host::{ItemEdit as Edit, StudioAct};
use ferrocast_studio::stage::Stage as Collection;
use serde_json::Value;

/// Steps kept each way.
const KEEP: usize = 100;
/// Edit Transform changes this close together are one step.
const MERGE: Duration = Duration::from_secs(3);
/// Arrow-key nudges this close together are one step.
const NUDGES: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Step {
    label: String,
    /// The collection before the step and after it.
    before: Value,
    after: Value,
    /// What a following act of the same kind on the same item joins ("box", "transform").
    key: Option<(&'static str, String, i64)>,
    at: Instant,
    /// A drag still going: its moves join this step until the one that saves.
    open: bool,
}

/// What `before` noted, for `after` to finish once the act ran.
#[derive(Debug)]
pub struct Before {
    label: String,
    doc: Value,
    key: Option<(&'static str, String, i64)>,
    /// Joins the newest step instead of making one.
    join: bool,
    /// The drag ends with this act.
    closes: bool,
    open: bool,
}

#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
}

fn verb(e: &Edit) -> &'static str {
    match e {
        Edit::Show => "Show",
        Edit::Hide => "Hide",
        Edit::Lock => "Lock",
        Edit::Unlock => "Unlock",
        Edit::Fit => "Fit",
        Edit::Remove => "Remove",
        Edit::Up | Edit::Down | Edit::Top | Edit::Bottom => "Reorder",
        Edit::Stretch => "Stretch",
        Edit::Center => "Centre",
        Edit::Reset => "Reset",
        Edit::FlipH | Edit::FlipV => "Flip",
        Edit::RotateCw | Edit::RotateCcw | Edit::Rotate180 => "Turn",
        Edit::Duplicate => "Duplicate",
        Edit::BlendNormal
        | Edit::BlendAdditive
        | Edit::BlendSubtract
        | Edit::BlendScreen
        | Edit::BlendMultiply
        | Edit::BlendLighten
        | Edit::BlendDarken
        | Edit::BlendMethodDefault
        | Edit::BlendMethodSrgbOff => "Change blending of",
        Edit::ScaleOff
        | Edit::ScalePoint
        | Edit::ScaleBicubic
        | Edit::ScaleBilinear
        | Edit::ScaleLanczos
        | Edit::ScaleArea => "Change scaling of",
    }
}

/// An item's source name, for a step's label.
fn item_name(c: &Collection, scene: &str, id: i64) -> Option<String> {
    c.items(scene).ok()?.into_iter().find(|i| i.id == id).map(|i| i.name)
}

impl History {
    /// What to remember before `act` runs, or None for acts that aren't undone.
    pub fn before(&mut self, c: &Collection, act: &StudioAct) -> Option<Before> {
        let fresh =
            |label: String, key, open| Before { label, doc: c.snapshot(), key, join: false, closes: false, open };
        let joined =
            |closes| Before { label: String::new(), doc: Value::Null, key: None, join: true, closes, open: false };
        match act {
            StudioAct::EditItem { scene, id, edit } => {
                Some(fresh(format!("{} {}", verb(edit), item_name(c, scene, *id)?), None, false))
            }
            StudioAct::ItemOrder { scene, id, .. } => {
                Some(fresh(format!("Reorder {}", item_name(c, scene, *id)?), None, false))
            }
            StudioAct::SetItemBox { scene, id, save, .. } => {
                let key = ("box", scene.clone(), *id);
                // A drag's moves, or nudges with the arrow keys in a row, are one step.
                if self.undo.last().is_some_and(|s| s.key.as_ref() == Some(&key) && (s.open || s.at.elapsed() < NUDGES))
                {
                    return Some(joined(*save));
                }
                Some(fresh(format!("Move {}", item_name(c, scene, *id)?), Some(key), !*save))
            }
            StudioAct::SetItemCrop { scene, id, save, .. } => {
                let key = ("crop", scene.clone(), *id);
                // A drag's crops are one step.
                if self.undo.last().is_some_and(|s| s.key.as_ref() == Some(&key) && s.open) {
                    return Some(joined(*save));
                }
                Some(fresh(format!("Crop {}", item_name(c, scene, *id)?), Some(key), !*save))
            }
            StudioAct::SetTransform { scene, id, .. } => {
                let key = ("transform", scene.clone(), *id);
                if self.joins(&key) {
                    return Some(joined(false));
                }
                Some(fresh(format!("Transform {}", item_name(c, scene, *id)?), Some(key), false))
            }
            StudioAct::AddSource { source, .. } => Some(fresh(format!("Add {}", source.name), None, false)),
            StudioAct::AddGroup { name, .. } => Some(fresh(format!("Add group {name}"), None, false)),
            StudioAct::GroupItems { name, .. } => Some(fresh(format!("Group into {name}"), None, false)),
            StudioAct::PasteCopy { name, .. } => Some(fresh(format!("Paste a copy of {name}"), None, false)),
            StudioAct::AddExisting { name, .. } => Some(fresh(format!("Add {name}"), None, false)),
            StudioAct::SetItemTransition { scene, id, show, .. } => {
                let what = if *show { "Show" } else { "Hide" };
                Some(fresh(format!("{what} transition of {}", item_name(c, scene, *id)?), None, false))
            }
            StudioAct::SetDeinterlace { name, .. } => Some(fresh(format!("Deinterlacing of {name}"), None, false)),
            StudioAct::ReplaceFile { source, .. } if !source.is_empty() => {
                Some(fresh(format!("Replace a file of {source}"), None, false))
            }
            StudioAct::AddScene { name, .. } => Some(fresh(format!("Add scene {name}"), None, false)),
            StudioAct::DuplicateScene { scene, .. } => Some(fresh(format!("Copy scene {scene}"), None, false)),
            StudioAct::RemoveScene { scene } => Some(fresh(format!("Remove scene {scene}"), None, false)),
            StudioAct::MoveScene { scene, .. } => Some(fresh(format!("Reorder {scene}"), None, false)),
            StudioAct::Rename { name, new_name } if name != new_name => {
                Some(fresh(format!("Rename {name}"), None, false))
            }
            StudioAct::SetSceneTransition { scene, .. } => {
                Some(fresh(format!("Change transition of {scene}"), None, false))
            }
            StudioAct::Update { name, .. } => {
                let key = ("settings", name.clone(), 0);
                if self.joins(&key) {
                    return Some(joined(false));
                }
                Some(fresh(format!("Change settings of {name}"), Some(key), false))
            }
            // Shortcuts (mute, push-to-talk) aren't undo steps, as in OBS.
            StudioAct::SetAudio { change, .. } if change.by_shortcut => None,
            StudioAct::SetAudio { name, change } => {
                let key = ("audio", name.clone(), 0);
                // A volume or balance slider's moves are one step.
                let slide = change.volume_db.is_some() || change.balance.is_some() || change.sync_ms.is_some();
                if slide && self.joins(&key) {
                    return Some(joined(false));
                }
                let label = match (change.muted, change.mono) {
                    (Some(true), _) => format!("Mute {name}"),
                    (Some(false), _) => format!("Unmute {name}"),
                    (_, Some(_)) => format!("Change mono of {name}"),
                    _ if change.monitor.is_some() => format!("Change monitoring of {name}"),
                    _ if change.tracks.is_some() => format!("Change tracks of {name}"),
                    _ if change.balance.is_some() => format!("Change balance of {name}"),
                    _ if change.sync_ms.is_some() => format!("Change sync of {name}"),
                    _ => format!("Change volume of {name}"),
                };
                Some(fresh(label, slide.then_some(key), false))
            }
            StudioAct::AddFilter { filter, .. } => Some(fresh(format!("Add filter {}", filter.name), None, false)),
            StudioAct::RemoveFilter { filter, .. } => Some(fresh(format!("Remove filter {filter}"), None, false)),
            StudioAct::EditFilter { filter, .. } => Some(fresh(format!("Change filter {filter}"), None, false)),
            StudioAct::PasteFilters { name, from } => {
                Some(fresh(format!("Paste filters from {from} to {name}"), None, false))
            }
            StudioAct::UpdateFilter { name, filter, .. } => {
                let key = ("filter", format!("{name}\n{filter}"), 0);
                if self.joins(&key) {
                    return Some(joined(false));
                }
                Some(fresh(format!("Change filter {filter}"), Some(key), false))
            }
            _ => None,
        }
    }

    /// Whether an act on `key` joins the newest step (a slider still moving).
    fn joins(&self, key: &(&'static str, String, i64)) -> bool {
        self.undo.last().is_some_and(|s| s.key.as_ref() == Some(key) && s.at.elapsed() < MERGE)
    }

    /// Records the step once the act ran (nothing if it failed or changed nothing).
    pub fn after<E>(&mut self, c: &Collection, b: Before, out: &Result<Value, E>) {
        if out.is_err() {
            return;
        }
        let now = c.snapshot();
        if b.join {
            if let Some(top) = self.undo.last_mut() {
                top.at = Instant::now();
                top.after = now;
                if b.closes {
                    top.open = false;
                }
            }
            self.redo.clear();
            return;
        }
        if now == b.doc {
            return;
        }
        self.redo.clear();
        self.undo.push(Step {
            label: b.label,
            before: b.doc,
            after: now,
            key: b.key,
            at: Instant::now(),
            open: b.open,
        });
        if self.undo.len() > KEEP {
            self.undo.remove(0);
        }
    }

    /// Undoes the newest step → its label.
    pub fn undo(&mut self, c: &Collection) -> Result<Option<String>, String> {
        let Some(mut step) = self.undo.pop() else { return Ok(None) };
        c.restore_all(&step.before);
        step.key = None;
        step.open = false;
        let label = step.label.clone();
        self.redo.push(step);
        Ok(Some(label))
    }

    pub fn redo(&mut self, c: &Collection) -> Result<Option<String>, String> {
        let Some(step) = self.redo.pop() else { return Ok(None) };
        c.restore_all(&step.after);
        let label = step.label.clone();
        self.undo.push(step);
        Ok(Some(label))
    }

    /// What Undo and Redo would do now, for the buttons.
    pub fn labels(&self) -> (Option<&str>, Option<&str>) {
        (self.undo.last().map(|s| s.label.as_str()), self.redo.last().map(|s| s.label.as_str()))
    }

    /// Every step each way, newest first, for the history list.
    pub fn list(&self) -> (Vec<&str>, Vec<&str>) {
        (
            self.undo.iter().rev().map(|s| s.label.as_str()).collect(),
            self.redo.iter().rev().map(|s| s.label.as_str()).collect(),
        )
    }

    /// Forgets everything (the scenes were replaced or the Studio stopped).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_studio::Canvas;

    fn stage() -> Collection {
        Collection::new(Canvas { width: 1920, height: 1080, fps: 30 }, "Main").unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn steps_undo_redo_and_drags_join() {
        let c = stage();
        let mut h = History::default();
        let add = StudioAct::AddScene { name: "Two".into(), vertical: false };
        let b = h.before(&c, &add);
        let out: Result<Value, String> = c.add_scene("Two").map(|_| Value::Null).map_err(|e| e.to_string());
        h.after(&c, b.unwrap_or_else(|| panic!("a step")), &out);
        assert_eq!(h.labels(), (Some("Add scene Two"), None));
        assert_eq!(h.undo(&c), Ok(Some("Add scene Two".into())));
        assert_eq!(c.scenes().len(), 1);
        assert_eq!(h.redo(&c), Ok(Some("Add scene Two".into())));
        assert_eq!(c.scenes().len(), 2);
        // A drag: its moves are one step, closed by the one that saves.
        let id = c
            .add_source("Main", "color_source_v3", "Box", &serde_json::json!({ "width": 10, "height": 10 }), false)
            .unwrap_or_else(|e| panic!("{e}"))
            .id;
        for (x, save) in [(10.0, false), (20.0, false), (30.0, true)] {
            let act = StudioAct::SetItemBox { scene: "Main".into(), id, x, y: 0.0, w: 10.0, h: 10.0, save };
            let b = h.before(&c, &act).unwrap_or_else(|| panic!("a step"));
            let out: Result<Value, String> =
                c.set_item_box("Main", id, x, 0.0, 10.0, 10.0).map(|_| Value::Null).map_err(|e| e.to_string());
            h.after(&c, b, &out);
        }
        assert_eq!(h.list().0, ["Move Box", "Add scene Two"]);
        assert_eq!(h.undo(&c), Ok(Some("Move Box".into())));
        assert_eq!(c.transform("Main", id).map(|t| t.x).ok(), Some(0.0), "back to before the drag");
    }
}
