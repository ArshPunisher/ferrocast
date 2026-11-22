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
/// What the steps may hold in all (before and after of each): a big collection's hundred steps
/// were hundreds of MB of JSON. The oldest go first.
const MAX_BYTES: usize = 64 * 1024 * 1024;
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
    /// About what `before` and `after` take in memory.
    bytes: usize,
}

/// About how much memory a JSON value takes.
fn approx_bytes(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 8,
        Value::Number(_) => 16,
        Value::String(s) => 24 + s.len(),
        Value::Array(a) => 24 + a.iter().map(approx_bytes).sum::<usize>(),
        Value::Object(o) => 48 + o.iter().map(|(k, v)| 24 + k.len() + approx_bytes(v)).sum::<usize>(),
    }
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
        Edit::StreamAndRecording | Edit::StreamOnly | Edit::RecordingOnly => "Set stream or recording for",
    }
}

/// An item's source name, for a step's label.
fn item_name(c: &Collection, scene: &str, id: i64) -> Option<String> {
    // Inside a group too: an edit to a group's source is an undo step like any other (owner's
    // check, 11 Nov: they weren't, so Undo took back the step before instead).
    c.items(scene)
        .ok()?
        .into_iter()
        .flat_map(|i| std::iter::once(i.clone()).chain(i.children))
        .find(|i| i.id == id)
        .map(|i| i.name)
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
                    // An unsaved move joining a step already closed reopens it: the drag goes on.
                    return Some(Before { open: !*save, ..joined(*save) });
                }
                Some(fresh(format!("Move {}", item_name(c, scene, *id)?), Some(key), !*save))
            }
            StudioAct::SetItemBoxes { scene, items, save } => {
                // The same ids in any order are the same drag.
                let mut ids: Vec<i64> = items.iter().map(|b| b.id).collect();
                ids.sort_unstable();
                // The whole sorted list is the key: two sets that happen to hash alike are two drags.
                let key = ("boxes", format!("{scene}\u{1}{ids:?}"), 0);
                if self.undo.last().is_some_and(|s| s.key.as_ref() == Some(&key) && (s.open || s.at.elapsed() < NUDGES))
                {
                    return Some(Before { open: !*save, ..joined(*save) });
                }
                let label = match ids.as_slice() {
                    [one] => format!("Move {}", item_name(c, scene, *one)?),
                    many => format!("Move {} sources", many.len()),
                };
                Some(fresh(label, Some(key), !*save))
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
            StudioAct::SetGroup { scene, id, group } => {
                let what = item_name(c, scene, *id)?;
                Some(fresh(
                    match group.and_then(|g| item_name(c, scene, g)) {
                        Some(g) => format!("Move {what} into {g}"),
                        None => format!("Take {what} out of its group"),
                    },
                    None,
                    false,
                ))
            }
            StudioAct::Ungroup { scene, id } => {
                Some(fresh(format!("Ungroup {}", item_name(c, scene, *id)?), None, false))
            }
            StudioAct::PasteCopy { name, .. } => Some(fresh(format!("Paste a copy of {name}"), None, false)),
            StudioAct::PasteSource { clip, .. } => {
                let name = clip.pointer("/sources/0/name").and_then(|n| n.as_str()).unwrap_or("source");
                Some(fresh(format!("Paste {name}"), None, false))
            }
            StudioAct::PasteFilterClip { name, item, clip } => {
                let from = clip.get("from").and_then(|n| n.as_str()).unwrap_or("a source");
                let to = match item {
                    Some(it) => item_name(c, &it.scene, it.id).unwrap_or_else(|| it.scene.clone()),
                    None => name.clone(),
                };
                Some(fresh(format!("Paste filters from {from} to {to}"), None, false))
            }
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
            StudioAct::SetItemFilterScope { scene, id, own } => {
                let who = item_name(c, scene, *id)?;
                let verb = if *own { "Use own filters for" } else { "Use shared filters for" };
                Some(fresh(format!("{verb} {who}"), None, false))
            }
            StudioAct::PasteFilters { name, from, item, from_item } => {
                let src = match from_item {
                    Some(f) => item_name(c, &f.scene, f.id).unwrap_or_else(|| f.scene.clone()),
                    None => from.clone(),
                };
                let to = match item {
                    Some(it) => item_name(c, &it.scene, it.id).unwrap_or_else(|| it.scene.clone()),
                    None => name.clone(),
                };
                Some(fresh(format!("Paste filters from {src} to {to}"), None, false))
            }
            StudioAct::UpdateFilter { name, filter, item, .. } => {
                let key = match item {
                    Some(it) => ("filter", format!("{}\n{filter}", it.scene), it.id),
                    None => ("filter", format!("{name}\n{filter}"), 0),
                };
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
                } else if b.open {
                    top.open = true;
                }
            }
            self.redo.clear();
            return;
        }
        if now == b.doc {
            return;
        }
        self.redo.clear();
        let bytes = approx_bytes(&b.doc) + approx_bytes(&now);
        self.push_step(Step {
            label: b.label,
            before: b.doc,
            after: now,
            key: b.key,
            at: Instant::now(),
            open: b.open,
            bytes,
        });
    }

    /// Keeps a step, forgetting the oldest while there are too many or they hold too much.
    fn push_step(&mut self, step: Step) {
        self.undo.push(step);
        while self.undo.len() > KEEP
            || (self.undo.len() > 1 && self.undo.iter().chain(&self.redo).map(|s| s.bytes).sum::<usize>() > MAX_BYTES)
        {
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
    fn a_big_collections_history_stays_under_its_memory_cap() {
        let mut h = History::default();
        // A collection of about 2 MB of JSON, edited 100 times.
        let big = |n: usize| {
            serde_json::json!({ "n": n, "sources": (0..2_000).map(|i| serde_json::json!({
            "name": format!("source number {i} with a longish name"), "settings": { "text": "x".repeat(600) } })).collect::<Vec<_>>() })
        };
        for n in 0..100 {
            let (before, after) = (big(n), big(n + 1));
            let bytes = approx_bytes(&before) + approx_bytes(&after);
            h.push_step(Step {
                label: format!("step {n}"),
                before,
                after,
                key: None,
                at: Instant::now(),
                open: false,
                bytes,
            });
            let held: usize = h.undo.iter().map(|s| s.bytes).sum();
            assert!(held <= MAX_BYTES || h.undo.len() == 1, "{n}: {held} bytes in {} steps", h.undo.len());
        }
        assert!(h.undo.len() < KEEP, "the oldest went: {} steps kept", h.undo.len());
        assert_eq!(h.labels().0, Some("step 99"), "the newest stays");
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

    /// Moving, aligning or spacing out several items is one step however many there are, and a
    /// drag of them is one step too; undo puts every one of them back.
    #[test]
    fn several_items_moved_at_once_are_one_undo_step() {
        use ferrocast_engine::studio_host::ItemBoxIn;
        let c = stage();
        let mut h = History::default();
        let add = |name: &str| {
            c.add_source("Main", "color_source_v3", name, &serde_json::json!({ "width": 10, "height": 10 }), false)
                .unwrap_or_else(|e| panic!("{e}"))
                .id
        };
        let (a, b, d) = (add("A"), add("B"), add("C"));
        let x_of = |id| c.transform("Main", id).map(|t| t.x).ok();
        let run = |h: &mut History, ids: &[i64], x: f32, save: bool| {
            let items: Vec<ItemBoxIn> = ids.iter().map(|&id| ItemBoxIn { id, x, y: 0.0, w: 10.0, h: 10.0 }).collect();
            let act = StudioAct::SetItemBoxes { scene: "Main".into(), items, save };
            let before = h.before(&c, &act).unwrap_or_else(|| panic!("a step"));
            let boxes: Vec<_> = ids.iter().map(|&id| (id, x, 0.0, 10.0, 10.0)).collect();
            let out: Result<Value, String> =
                c.set_item_boxes("Main", &boxes).map(|_| Value::Null).map_err(|e| e.to_string());
            h.after(&c, before, &out);
        };
        // A drag of three: its moves are one step, labelled by how many.
        for (x, save) in [(10.0, false), (20.0, false), (30.0, true)] {
            run(&mut h, &[a, b, d], x, save);
        }
        assert_eq!(h.list().0, ["Move 3 sources"]);
        assert_eq!([x_of(a), x_of(b), x_of(d)], [Some(30.0); 3]);
        assert_eq!(h.undo(&c), Ok(Some("Move 3 sources".into())));
        assert_eq!([x_of(a), x_of(b), x_of(d)], [Some(0.0); 3], "all three back");
        assert_eq!(h.redo(&c), Ok(Some("Move 3 sources".into())));
        assert_eq!([x_of(a), x_of(b), x_of(d)], [Some(30.0); 3]);
        // Two of them is its own step; one alone reads as moving that one.
        run(&mut h, &[a, b], 50.0, true);
        run(&mut h, &[d], 60.0, true);
        assert_eq!(h.list().0, ["Move C", "Move 2 sources", "Move 3 sources"]);
    }

    /// A drag that starts just after a finished move of the same sources is part of that step, and
    /// stays so however long it is held still (the step is open again until the drag saves).
    #[test]
    fn a_drag_joining_a_finished_move_stays_one_step() {
        use ferrocast_engine::studio_host::ItemBoxIn;
        let c = stage();
        let mut h = History::default();
        let go = |h: &mut History, x: f32, save: bool| {
            let items = vec![
                ItemBoxIn { id: 1, x, y: 0.0, w: 10.0, h: 10.0 },
                ItemBoxIn { id: 2, x, y: 0.0, w: 10.0, h: 10.0 },
            ];
            let act = StudioAct::SetItemBoxes { scene: "Main".into(), items, save };
            let before = h.before(&c, &act).unwrap_or_else(|| panic!("a step"));
            let out: Result<Value, String> = c
                .set_item_boxes("Main", &[(1, x, 0.0, 10.0, 10.0), (2, x, 0.0, 10.0, 10.0)])
                .map(|_| Value::Null)
                .map_err(|e| e.to_string());
            h.after(&c, before, &out);
        };
        c.add_source("Main", "color_source_v3", "A", &serde_json::json!({ "width": 10, "height": 10 }), false)
            .unwrap_or_else(|e| panic!("{e}"));
        c.add_source("Main", "color_source_v3", "B", &serde_json::json!({ "width": 10, "height": 10 }), false)
            .unwrap_or_else(|e| panic!("{e}"));
        go(&mut h, 50.0, true); // a finished move
        go(&mut h, 60.0, false); // a drag starts right after: joins it
        assert!(h.undo.last().is_some_and(|s| s.open), "an unsaved drag keeps the step open");
        // Held still past the nudge window: the next move of the drag is still that step.
        if let Some(s) = h.undo.last_mut() {
            s.at -= NUDGES * 2;
        }
        go(&mut h, 70.0, false);
        go(&mut h, 80.0, true);
        assert_eq!(h.list().0.len(), 1, "one step: {:?}", h.list().0);
        assert!(h.undo.last().is_some_and(|s| !s.open), "the save closes it");
    }

    /// Two different sets of items moved within a second of each other are two steps, even when
    /// their ids would hash alike ([1, 34] and [2, 3] do).
    #[test]
    fn different_sets_of_items_are_different_steps() {
        use ferrocast_engine::studio_host::ItemBoxIn;
        let c = stage();
        let mut h = History::default();
        for n in 0..34 {
            c.add_source(
                "Main",
                "color_source_v3",
                &format!("S{n}"),
                &serde_json::json!({ "width": 10, "height": 10 }),
                false,
            )
            .unwrap_or_else(|e| panic!("{e}"));
        }
        let go = |h: &mut History, ids: [i64; 2], x: f32| {
            let items: Vec<ItemBoxIn> = ids.iter().map(|&id| ItemBoxIn { id, x, y: 0.0, w: 10.0, h: 10.0 }).collect();
            let act = StudioAct::SetItemBoxes { scene: "Main".into(), items, save: true };
            let before = h.before(&c, &act).unwrap_or_else(|| panic!("a step"));
            let boxes: Vec<_> = ids.iter().map(|&id| (id, x, 0.0, 10.0, 10.0)).collect();
            let out: Result<Value, String> =
                c.set_item_boxes("Main", &boxes).map(|_| Value::Null).map_err(|e| e.to_string());
            h.after(&c, before, &out);
        };
        go(&mut h, [1, 34], 100.0);
        go(&mut h, [2, 3], 200.0);
        assert_eq!(h.list().0.len(), 2, "two steps: {:?}", h.list().0);
        // The same two in the other order are the same drag.
        go(&mut h, [3, 2], 300.0);
        assert_eq!(h.list().0.len(), 2, "the same set merges: {:?}", h.list().0);
    }

    /// Owner's check, 11 Nov: edits to a group's sources, and moving a source in or out of a
    /// group, weren't undo steps (Undo took back the step before them instead).
    #[test]
    fn group_edits_are_undo_steps() {
        let c = stage();
        let mut h = History::default();
        let step = |h: &mut History, act: StudioAct, run: &dyn Fn() -> Result<(), String>| {
            let b = h.before(&c, &act).unwrap_or_else(|| panic!("{act:?} is an undo step"));
            h.after(&c, b, &run().map(|_| Value::Null));
        };
        let id = c
            .add_source("Main", "color_source_v3", "Box", &serde_json::json!({ "width": 10, "height": 10 }), false)
            .unwrap_or_else(|e| panic!("{e}"))
            .id;
        let g = c.add_group("Main", "Pair").unwrap_or_else(|e| panic!("{e}")).id;
        step(&mut h, StudioAct::SetGroup { scene: "Main".into(), id, group: Some(g) }, &|| {
            c.set_group("Main", id, Some(g)).map_err(|e| e.to_string())
        });
        let inside = c.items("Main").unwrap_or_default().into_iter().flat_map(|i| i.children).next();
        let inside = inside.unwrap_or_else(|| panic!("in the group")).id;
        step(
            &mut h,
            StudioAct::EditItem {
                scene: "Main".into(),
                id: inside,
                edit: ferrocast_engine::studio_host::ItemEdit::Hide,
            },
            &|| c.edit_item("Main", inside, ferrocast_studio::ItemEdit::Hide).map_err(|e| e.to_string()),
        );
        assert_eq!(h.list().0, ["Hide Box", "Move Box into Pair"]);
        assert_eq!(h.undo(&c), Ok(Some("Hide Box".into())));
        assert_eq!(h.undo(&c), Ok(Some("Move Box into Pair".into())));
        assert!(c.items("Main").unwrap_or_default().iter().any(|i| i.name == "Box"), "out of the group again");
    }

    /// Where an item is shown is an undo step like any other edit, and comes back with Redo.
    #[test]
    fn choosing_stream_or_recording_for_an_item_undoes_and_redoes() {
        use ferrocast_engine::studio_host::ItemEdit;
        let c = stage();
        let mut h = History::default();
        let it = c
            .add_source("Main", "color_source_v3", "Chat", &serde_json::json!({ "width": 64, "height": 36 }), false)
            .unwrap();
        let show = |c: &Collection| c.items("Main").unwrap()[0].show.clone();
        for edit in [ItemEdit::StreamOnly, ItemEdit::RecordingOnly] {
            let act = StudioAct::EditItem { scene: "Main".into(), id: it.id, edit };
            let b = h.before(&c, &act).unwrap_or_else(|| panic!("{act:?} is an undo step"));
            let r = c.edit_item(
                "Main",
                it.id,
                match edit {
                    ItemEdit::StreamOnly => ferrocast_studio::ItemEdit::StreamOnly,
                    _ => ferrocast_studio::ItemEdit::RecordingOnly,
                },
            );
            h.after(&c, b, &r.map(|()| Value::Null).map_err(|e| e.to_string()));
        }
        assert_eq!(show(&c), "recording");
        assert_eq!(h.list().0, ["Set stream or recording for Chat", "Set stream or recording for Chat"]);
        assert_eq!(h.undo(&c), Ok(Some("Set stream or recording for Chat".into())));
        assert_eq!(show(&c), "stream");
        assert_eq!(h.undo(&c), Ok(Some("Set stream or recording for Chat".into())));
        assert_eq!(show(&c), "both");
        assert!(!c.has_selective_items());
        assert_eq!(h.redo(&c), Ok(Some("Set stream or recording for Chat".into())));
        assert_eq!(show(&c), "stream");
    }

    #[test]
    fn item_filter_and_scope_edits_undo_redo_and_merge_sliders_per_item() {
        use ferrocast_engine::studio_host::ItemTarget;

        let c = stage();
        let mut h = History::default();
        let step = |h: &mut History, act: StudioAct, run: &dyn Fn() -> Result<(), String>| {
            let b = h.before(&c, &act).unwrap_or_else(|| panic!("{act:?} is an undo step"));
            h.after(&c, b, &run().map(|_| Value::Null));
        };
        c.start_vertical("Vertical").unwrap();
        let main_it = c
            .add_source("Main", "color_source_v3", "Card", &serde_json::json!({ "width": 64, "height": 36 }), false)
            .unwrap();
        let vert_it = c.add_existing("Vertical", "Card").unwrap();
        let tgt = Some(ItemTarget { scene: "Vertical".into(), id: vert_it.id });

        step(&mut h, StudioAct::SetItemFilterScope { scene: "Vertical".into(), id: vert_it.id, own: true }, &|| {
            c.set_item_filter_scope("Vertical", vert_it.id, true).map(|_| ()).map_err(|e| e.to_string())
        });
        step(
            &mut h,
            StudioAct::AddFilter {
                name: String::new(),
                filter: ferrocast_engine::studio_host::NewSource {
                    kind: "ferrocast_rounded_corners".into(),
                    name: "Round".into(),
                    settings: serde_json::json!({ "radius": 8 }),
                },
                item: tgt.clone(),
            },
            &|| {
                c.add_item_filter(
                    "Vertical",
                    vert_it.id,
                    "ferrocast_rounded_corners",
                    "Round",
                    &serde_json::json!({ "radius": 8 }),
                )
                .map_err(|e| e.to_string())
            },
        );
        // Two slider drags on Vertical item merge into one undo step.
        for r in [14, 24] {
            step(
                &mut h,
                StudioAct::UpdateFilter {
                    name: String::new(),
                    filter: "Round".into(),
                    settings: serde_json::json!({ "radius": r }),
                    item: tgt.clone(),
                },
                &|| {
                    c.update_item_filter("Vertical", vert_it.id, "Round", &serde_json::json!({ "radius": r }))
                        .map_err(|e| e.to_string())
                },
            );
        }
        assert_eq!(h.list().0, ["Change filter Round", "Add filter Round", "Use own filters for Card"]);
        assert_eq!(c.item_filters("Main", main_it.id).unwrap().len(), 0);
        assert_eq!(c.item_filter_settings("Vertical", vert_it.id, "Round").unwrap()["radius"], 24);

        // Undo slider change restores radius to 8; undo again removes Round cleanly.
        assert_eq!(h.undo(&c), Ok(Some("Change filter Round".into())));
        assert_eq!(c.item_filter_settings("Vertical", vert_it.id, "Round").unwrap()["radius"], 8);
        assert_eq!(h.undo(&c), Ok(Some("Add filter Round".into())));
        assert!(c.item_filters("Vertical", vert_it.id).unwrap().is_empty());
        // Undo scope switch reverts own_filters back to false.
        assert_eq!(h.undo(&c), Ok(Some("Use own filters for Card".into())));
        assert!(!c.items("Vertical").unwrap()[0].own_filters);
        // Redo scope + add restores them.
        assert_eq!(h.redo(&c), Ok(Some("Use own filters for Card".into())));
        assert!(c.items("Vertical").unwrap()[0].own_filters);
    }
}
