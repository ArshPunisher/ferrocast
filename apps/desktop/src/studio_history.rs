//! Undo and redo, as OBS's Edit menu has them. Sources in scenes: moving, resizing and Edit
//! Transform, show and hide, lock, order, fit, flips and turns, blending and scaling, adding,
//! duplicating and removing. Scenes: adding, copying, removing, reordering, renaming and their
//! own transition. Filters (every change), a source's settings, and its sound (volume, mute,
//! monitoring, balance, sync, mono, tracks). Each step remembers things as they were (a removed
//! source or scene is held, so it can come back); undoing one remembers them as they are, for
//! redo.

use std::time::{Duration, Instant};

use ferrocast_engine::studio_host::{ItemEdit as Edit, StudioAct};
use ferrocast_studio::{AudioSnap, Collection, FilterSnap, ItemEdit, ItemSnap, SceneSnap, TransitionInfo};
use serde_json::Value;

/// Steps kept each way.
const KEEP: usize = 100;
/// Edit Transform changes this close together are one step.
const MERGE: Duration = Duration::from_secs(3);
/// Arrow-key nudges this close together are one step.
const NUDGES: Duration = Duration::from_secs(1);

#[derive(Debug)]
enum Change {
    /// Put this item back as it was.
    Item(ItemSnap),
    /// This item wasn't there: take it out.
    Gone { scene: String, id: i64 },
    /// Bring this removed scene back.
    SceneBack(SceneSnap),
    /// This scene wasn't there: take it out.
    SceneGone(String),
    /// Put this scene back at this place in the list.
    SceneAt(String, usize),
    /// Rename the first back to the second.
    Rename(String, String),
    /// Put this source's filters back.
    Filters(FilterSnap),
    /// Put this source's settings back.
    Settings(String, Value),
    /// Put this sound's mixer settings back.
    Audio(AudioSnap),
    /// Give this scene back its own transition (or none).
    SceneTransition(String, Option<TransitionInfo>),
    /// Put this source's deinterlacing back (mode, top field first).
    Deinterlace(String, String, bool),
}

#[derive(Debug)]
struct Step {
    label: String,
    changes: Vec<Change>,
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
    changes: Vec<Change>,
    key: Option<(&'static str, String, i64)>,
    /// Joins the newest step instead of making one.
    join: bool,
    /// The scene and its item ids before an act that adds items.
    added: Option<(String, Vec<i64>)>,
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

impl History {
    /// What to remember before `act` runs, or None for acts that aren't undone.
    pub fn before(&mut self, c: &Collection, act: &StudioAct) -> Option<Before> {
        let fresh = |label: String, changes, key, open| Before {
            label,
            changes,
            key,
            join: false,
            added: None,
            closes: false,
            open,
        };
        match act {
            StudioAct::EditItem { scene, id, edit: Edit::Duplicate } => {
                let snap = c.snap(scene, *id).ok()?;
                Some(Before {
                    added: Some((scene.clone(), c.item_ids(scene).ok()?)),
                    ..fresh(format!("Duplicate {}", snap.name), Vec::new(), None, false)
                })
            }
            StudioAct::EditItem { scene, id, edit } => {
                let snap = c.snap(scene, *id).ok()?;
                Some(fresh(format!("{} {}", verb(edit), snap.name), vec![Change::Item(snap)], None, false))
            }
            StudioAct::SetItemBox { scene, id, save, .. } => {
                let key = ("box", scene.clone(), *id);
                // A drag's moves, or nudges with the arrow keys in a row, are one step.
                if self.undo.last().is_some_and(|s| s.key.as_ref() == Some(&key) && (s.open || s.at.elapsed() < NUDGES))
                {
                    return Some(Before { join: true, closes: *save, ..fresh(String::new(), Vec::new(), None, false) });
                }
                let snap = c.snap(scene, *id).ok()?;
                Some(fresh(format!("Move {}", snap.name), vec![Change::Item(snap)], Some(key), !*save))
            }
            StudioAct::SetTransform { scene, id, .. } => {
                let key = ("transform", scene.clone(), *id);
                if self.joins(&key) {
                    return Some(Before { join: true, ..fresh(String::new(), Vec::new(), None, false) });
                }
                let snap = c.snap(scene, *id).ok()?;
                Some(fresh(format!("Transform {}", snap.name), vec![Change::Item(snap)], Some(key), false))
            }
            StudioAct::AddSource { scene, source, .. } => Some(Before {
                added: Some((scene.clone(), c.item_ids(scene).ok()?)),
                ..fresh(format!("Add {}", source.name), Vec::new(), None, false)
            }),
            StudioAct::AddExisting { scene, name } => Some(Before {
                added: Some((scene.clone(), c.item_ids(scene).ok()?)),
                ..fresh(format!("Add {name}"), Vec::new(), None, false)
            }),
            StudioAct::SetItemTransition { scene, id, show, .. } => {
                let snap = c.snap(scene, *id).ok()?;
                let what = if *show { "Show" } else { "Hide" };
                Some(fresh(format!("{what} transition of {}", snap.name), vec![Change::Item(snap)], None, false))
            }
            StudioAct::SetDeinterlace { name, .. } => {
                let (mode, top) = c.deinterlace_of(name).ok()?;
                Some(fresh(
                    format!("Deinterlacing of {name}"),
                    vec![Change::Deinterlace(name.clone(), mode, top)],
                    None,
                    false,
                ))
            }
            // A source's own file: its settings come back; a filter's: the source's filters.
            StudioAct::ReplaceFile { source, parent, .. } if !source.is_empty() => match parent {
                Some(p) => self.filters(c, p, format!("Replace a file of {source}"), None),
                None => {
                    let was = c.snap_settings(source).ok()?;
                    Some(fresh(
                        format!("Replace a file of {source}"),
                        vec![Change::Settings(source.clone(), was)],
                        None,
                        false,
                    ))
                }
            },
            StudioAct::AddScene { name } => {
                Some(fresh(format!("Add scene {name}"), vec![Change::SceneGone(name.clone())], None, false))
            }
            StudioAct::DuplicateScene { scene, name } => {
                Some(fresh(format!("Copy scene {scene}"), vec![Change::SceneGone(name.clone())], None, false))
            }
            StudioAct::RemoveScene { scene } => {
                let snap = c.snap_scene(scene).ok()?;
                Some(fresh(format!("Remove scene {scene}"), vec![Change::SceneBack(snap)], None, false))
            }
            StudioAct::MoveScene { scene, .. } => {
                let at = c.scene_index(scene).ok()?;
                Some(fresh(format!("Reorder {scene}"), vec![Change::SceneAt(scene.clone(), at)], None, false))
            }
            StudioAct::Rename { name, new_name } if name != new_name => {
                Some(fresh(format!("Rename {name}"), vec![Change::Rename(new_name.clone(), name.clone())], None, false))
            }
            StudioAct::SetSceneTransition { scene, .. } => {
                let t = c.scene_transition_of(scene).ok()?;
                Some(fresh(
                    format!("Change transition of {scene}"),
                    vec![Change::SceneTransition(scene.clone(), t)],
                    None,
                    false,
                ))
            }
            StudioAct::Update { name, .. } => {
                let key = ("settings", name.clone(), 0);
                if self.joins(&key) {
                    return Some(Before { join: true, ..fresh(String::new(), Vec::new(), None, false) });
                }
                let was = c.snap_settings(name).ok()?;
                Some(fresh(
                    format!("Change settings of {name}"),
                    vec![Change::Settings(name.clone(), was)],
                    Some(key),
                    false,
                ))
            }
            // Shortcuts (mute, push-to-talk) aren't undo steps, as in OBS.
            StudioAct::SetAudio { change, .. } if change.by_shortcut => None,
            StudioAct::SetAudio { name, change } => {
                let key = ("audio", name.clone(), 0);
                // A volume or balance slider's moves are one step.
                let slide = change.volume_db.is_some() || change.balance.is_some() || change.sync_ms.is_some();
                if slide && self.joins(&key) {
                    return Some(Before { join: true, ..fresh(String::new(), Vec::new(), None, false) });
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
                let snap = c.snap_audio(name).ok()?;
                Some(fresh(label, vec![Change::Audio(snap)], slide.then_some(key), false))
            }
            StudioAct::AddFilter { name, filter } => self.filters(c, name, format!("Add filter {}", filter.name), None),
            StudioAct::RemoveFilter { name, filter } => self.filters(c, name, format!("Remove filter {filter}"), None),
            StudioAct::EditFilter { name, filter, .. } => {
                self.filters(c, name, format!("Change filter {filter}"), None)
            }
            StudioAct::PasteFilters { name, from } => {
                self.filters(c, name, format!("Paste filters from {from} to {name}"), None)
            }
            StudioAct::UpdateFilter { name, filter, .. } => {
                let key = ("filter", format!("{name}\n{filter}"), 0);
                if self.joins(&key) {
                    return Some(Before { join: true, ..fresh(String::new(), Vec::new(), None, false) });
                }
                self.filters(c, name, format!("Change filter {filter}"), Some(key))
            }
            _ => None,
        }
    }

    /// Whether an act on `key` joins the newest step (a slider still moving).
    fn joins(&self, key: &(&'static str, String, i64)) -> bool {
        self.undo.last().is_some_and(|s| s.key.as_ref() == Some(key) && s.at.elapsed() < MERGE)
    }

    fn filters(
        &self,
        c: &Collection,
        name: &str,
        label: String,
        key: Option<(&'static str, String, i64)>,
    ) -> Option<Before> {
        let snap = c.snap_filters(name).ok()?;
        Some(Before {
            label,
            changes: vec![Change::Filters(snap)],
            key,
            join: false,
            added: None,
            closes: false,
            open: false,
        })
    }

    /// Records the step once the act ran (nothing if it failed).
    pub fn after<E>(&mut self, c: &Collection, b: Before, out: &Result<Value, E>) {
        if out.is_err() {
            return;
        }
        self.redo.clear();
        if b.join {
            if let Some(top) = self.undo.last_mut() {
                top.at = Instant::now();
                if b.closes {
                    top.open = false;
                }
            }
            return;
        }
        let mut changes = b.changes;
        if let Some((scene, before)) = b.added {
            let now = c.item_ids(&scene).unwrap_or_default();
            changes.extend(
                now.into_iter().filter(|id| !before.contains(id)).map(|id| Change::Gone { scene: scene.clone(), id }),
            );
        }
        if changes.is_empty() {
            return;
        }
        self.undo.push(Step { label: b.label, changes, key: b.key, at: Instant::now(), open: b.open });
        if self.undo.len() > KEEP {
            self.undo.remove(0);
        }
    }

    /// Undoes the newest step → its label. Err when it can't be (its scene is gone); the step
    /// is dropped then.
    pub fn undo(&mut self, c: &Collection) -> Result<Option<String>, String> {
        self.step(c, true)
    }

    pub fn redo(&mut self, c: &Collection) -> Result<Option<String>, String> {
        self.step(c, false)
    }

    fn step(&mut self, c: &Collection, back: bool) -> Result<Option<String>, String> {
        let popped = if back { self.undo.pop() } else { self.redo.pop() };
        let Some(step) = popped else { return Ok(None) };
        let mut inverse = Vec::new();
        let mut moved: Vec<Remap> = Vec::new();
        for change in step.changes.into_iter().rev() {
            match apply(c, change) {
                Ok((inv, remap)) => {
                    inverse.push(inv);
                    moved.extend(remap);
                }
                Err(e) => {
                    return Err(format!(
                        "\"{}\" can't be {} any more: {e}",
                        step.label,
                        if back { "undone" } else { "redone" }
                    ));
                }
            }
        }
        // A source that came back has a new id: later steps follow it.
        for (scene, old, new) in &moved {
            for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
                for ch in &mut s.changes {
                    match ch {
                        Change::Item(snap) if snap.scene == *scene && snap.id == *old => snap.id = *new,
                        Change::Gone { scene: sc, id } if sc == scene && *id == *old => *id = *new,
                        _ => {}
                    }
                }
            }
            for ch in &mut inverse {
                match ch {
                    Change::Item(snap) if snap.scene == *scene && snap.id == *old => snap.id = *new,
                    Change::Gone { scene: sc, id } if sc == scene && *id == *old => *id = *new,
                    _ => {}
                }
            }
        }
        let label = step.label.clone();
        let done = Step { label: step.label, changes: inverse, key: None, at: Instant::now(), open: false };
        if back {
            self.redo.push(done)
        } else {
            self.undo.push(done)
        }
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

/// A source that came back under a new id: (scene, old id, new id).
type Remap = (String, i64, i64);

/// Applies one change → its opposite, and the new id when an item came back.
fn apply(c: &Collection, change: Change) -> Result<(Change, Option<Remap>), String> {
    match change {
        Change::Item(snap) => {
            let now = c.snap(&snap.scene, snap.id).ok();
            let id = c.restore(&snap).map_err(|e| e.to_string())?;
            let remap = (id != snap.id).then(|| (snap.scene.clone(), snap.id, id));
            let inv = now.map(Change::Item).unwrap_or(Change::Gone { scene: snap.scene.clone(), id });
            Ok((inv, remap))
        }
        Change::Gone { scene, id } => {
            let now = c.snap(&scene, id).map_err(|e| e.to_string())?;
            c.edit_item(&scene, id, ItemEdit::Remove).map_err(|e| e.to_string())?;
            Ok((Change::Item(now), None))
        }
        Change::SceneBack(snap) => {
            c.restore_scene(&snap).map_err(|e| e.to_string())?;
            Ok((Change::SceneGone(snap.name.clone()), None))
        }
        Change::SceneGone(name) => {
            let now = c.snap_scene(&name).map_err(|e| e.to_string())?;
            c.remove_scene(&name).map_err(|e| e.to_string())?;
            Ok((Change::SceneBack(now), None))
        }
        Change::SceneAt(name, at) => {
            let now = c.scene_index(&name).map_err(|e| e.to_string())?;
            c.move_scene(&name, at).map_err(|e| e.to_string())?;
            Ok((Change::SceneAt(name, now), None))
        }
        Change::Rename(from, to) => {
            c.rename(&from, &to).map_err(|e| e.to_string())?;
            Ok((Change::Rename(to, from), None))
        }
        Change::Filters(snap) => {
            let now = c.snap_filters(&snap.source).map_err(|e| e.to_string())?;
            c.restore_filters(&snap).map_err(|e| e.to_string())?;
            Ok((Change::Filters(now), None))
        }
        Change::Settings(name, was) => {
            let now = c.snap_settings(&name).map_err(|e| e.to_string())?;
            c.restore_settings(&name, &was).map_err(|e| e.to_string())?;
            Ok((Change::Settings(name, now), None))
        }
        Change::Audio(snap) => {
            let now = c.snap_audio(&snap.source).map_err(|e| e.to_string())?;
            c.restore_audio(&snap).map_err(|e| e.to_string())?;
            Ok((Change::Audio(now), None))
        }
        Change::Deinterlace(name, mode, top) => {
            let (now, now_top) = c.deinterlace_of(&name).map_err(|e| e.to_string())?;
            c.set_deinterlace(&name, &mode, top).map_err(|e| e.to_string())?;
            Ok((Change::Deinterlace(name, now, now_top), None))
        }
        Change::SceneTransition(scene, t) => {
            let now = c.scene_transition_of(&scene).map_err(|e| e.to_string())?;
            c.set_scene_transition(&scene, t.as_ref()).map_err(|e| e.to_string())?;
            Ok((Change::SceneTransition(scene, now), None))
        }
    }
}
