//! Push to show and push to hide (items), push to enable and push to disable (filters): while the
//! key is held the item or filter is shown, hidden, on or off; on release it goes back to how it
//! was when the key went down.
//!
//! The change is never saved and never an undo step (`StudioAct::PushItem` / `PushFilter`): it
//! is a hold, not an edit. What is remembered is the exact target (scene and item id, or the
//! filter's source and name) and its state before the first key went down, so:
//! - a key repeating (auto-repeat, or the same key twice) changes nothing;
//! - a scene switched while the key is held doesn't matter: the item is put back in its own scene;
//! - an item or filter deleted meanwhile is skipped silently when the key is let go;
//! - two keys held on one target restore it only when the last is let go, to the state before
//!   the first;
//! - every hold is let go at once when keys can't report their release any more (the shortcuts
//!   were taken for a shortcut field, or replaced by new settings), at a collection switch and
//!   when the Studio stops.

use std::collections::HashMap;

use serde_json::Value;

use crate::studio_host::StudioAct;
use crate::{Engine, EngineError};

/// What a hold changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    /// An item of a scene (inside a group too: its id is the scene's).
    Item { scene: String, id: i64 },
    /// A filter on a source.
    Filter { source: String, filter: String },
}

/// The four actions: what the key does while held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Show,
    Hide,
    FilterOn,
    FilterOff,
}

impl Kind {
    /// The state the target has while the key is held.
    fn held_state(self) -> bool {
        matches!(self, Kind::Show | Kind::FilterOn)
    }
}

/// An action id (`push_show:<scene>\n<source>`…) taken apart: what it does, and the two names.
pub(crate) fn parse(action: &str) -> Option<(Kind, &str, &str)> {
    const KINDS: [(&str, Kind); 4] = [
        ("push_show:", Kind::Show),
        ("push_hide:", Kind::Hide),
        ("push_filter_on:", Kind::FilterOn),
        ("push_filter_off:", Kind::FilterOff),
    ];
    let (kind, rest) = KINDS.iter().find_map(|(p, k)| action.strip_prefix(p).map(|r| (*k, r)))?;
    let (a, b) = rest.split_once('\n')?;
    (!a.is_empty() && !b.is_empty()).then_some((kind, a, b))
}

/// Whether this action is one of the four (the key's release matters for it).
pub fn is_push_hold(action: &str) -> bool {
    parse(action).is_some()
}

/// The item called `source` in `items` (a scene's items, top first), looking inside groups at any
/// depth → its id and whether it is shown. The first one wins when a name repeats.
pub fn find_item(items: &Value, source: &str) -> Option<(i64, bool)> {
    for it in items.as_array()? {
        if it["name"].as_str() == Some(source)
            && let Some(id) = it["id"].as_i64()
        {
            return Some((id, it["visible"].as_bool().unwrap_or(true)));
        }
        if let Some(found) = find_item(&it["children"], source) {
            return Some(found);
        }
    }
    None
}

/// The keys held now, and what each target was before.
#[derive(Debug, Default)]
pub(crate) struct Holds {
    /// The target each held action took.
    by_action: HashMap<String, Target>,
    /// Each held target: its state before the first hold, and the actions holding it.
    by_target: HashMap<Target, (bool, Vec<String>)>,
}

impl Holds {
    pub(crate) fn holding(&self, action: &str) -> bool {
        self.by_action.contains_key(action)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_action.is_empty()
    }

    /// The key for `action` went down on `target`, which is `now` shown or on. False when this
    /// action already holds (a repeat: nothing is recorded again).
    pub(crate) fn press(&mut self, action: &str, target: Target, now: bool) -> bool {
        if self.holding(action) {
            return false;
        }
        let entry = self.by_target.entry(target.clone()).or_insert_with(|| (now, Vec::new()));
        entry.1.push(action.to_string());
        self.by_action.insert(action.to_string(), target);
        true
    }

    /// Undoes a `press` that couldn't be applied.
    pub(crate) fn forget(&mut self, action: &str) {
        let _ = self.release(action);
    }

    /// The key for `action` came up → the target and the state to put it in: as it was before
    /// the first hold when no other key holds it, else what the key still held asks for.
    pub(crate) fn release(&mut self, action: &str) -> Option<(Target, bool)> {
        let target = self.by_action.remove(action)?;
        let (was, holders) = self.by_target.get_mut(&target)?;
        holders.retain(|h| h != action);
        if holders.is_empty() {
            let was = *was;
            self.by_target.remove(&target);
            Some((target, was))
        } else {
            // Another key still holds it, and its state counts again (show held, hide let go:
            // shown). The most recent holder left decides.
            let state = holders.last().and_then(|h| parse(h)).map(|(k, _, _)| k.held_state())?;
            Some((target, state))
        }
    }

    /// Every hold let go → what to put back.
    pub(crate) fn release_all(&mut self) -> Vec<(Target, bool)> {
        let out = self.by_target.drain().map(|(t, (was, _))| (t, was)).collect();
        self.by_action.clear();
        out
    }
}

impl Engine {
    /// A push-to-show / hide / filter key went down (`pressed`) or came up. Down: the target is
    /// found by its names now, remembered with its state, and put in its held state. Up: put back.
    /// A name that isn't there (renamed, deleted) is a plain error on the way down and nothing
    /// on the way up.
    pub async fn studio_push_hold(&self, action: &str, pressed: bool) -> Result<(), EngineError> {
        let Some((kind, a, b)) = parse(action) else {
            return Err(EngineError::Invalid("action: a push-to-show, push-to-hide or push-to-filter key.".into()));
        };
        if !pressed {
            let back = self.studio().holds.lock().unwrap_or_else(|p| p.into_inner()).release(action);
            if let Some((target, was)) = back
                && let Err(e) = self.apply_hold(&target, was).await
            {
                // Deleted or renamed while held: nothing to put back.
                tracing::debug!(action, error = %e, "a held item or filter couldn't be put back");
            }
            return Ok(());
        }
        if self.studio().holds.lock().unwrap_or_else(|p| p.into_inner()).holding(action) {
            return Ok(());
        }
        let (target, now) = match kind {
            Kind::Show | Kind::Hide => {
                let items = self.studio_act(StudioAct::Items { scene: a.to_string() }).await?;
                let (id, shown) = find_item(&items, b).ok_or_else(|| {
                    EngineError::Unavailable(format!("There is no \"{b}\" in \"{a}\" any more: set the key again."))
                })?;
                (Target::Item { scene: a.to_string(), id }, shown)
            }
            Kind::FilterOn | Kind::FilterOff => {
                let list = self.studio_act(StudioAct::Filters { name: a.to_string(), item: None }).await?;
                let on = list
                    .as_array()
                    .and_then(|l| l.iter().find(|f| f["name"].as_str() == Some(b)))
                    .and_then(|f| f["enabled"].as_bool())
                    .ok_or_else(|| {
                        EngineError::Unavailable(format!("\"{a}\" has no filter \"{b}\" any more: set the key again."))
                    })?;
                (Target::Filter { source: a.to_string(), filter: b.to_string() }, on)
            }
        };
        let first = self.studio().holds.lock().unwrap_or_else(|p| p.into_inner()).press(action, target.clone(), now);
        if first && let Err(e) = self.apply_hold(&target, kind.held_state()).await {
            self.studio().holds.lock().unwrap_or_else(|p| p.into_inner()).forget(action);
            return Err(e);
        }
        Ok(())
    }

    /// Lets every hold go and puts each target back (keys that can't report their release any
    /// more, a collection switch, the Studio stopping). Nothing held: nothing happens.
    pub async fn studio_release_holds(&self) {
        let back = self.studio().holds.lock().unwrap_or_else(|p| p.into_inner()).release_all();
        for (target, was) in back {
            if let Err(e) = self.apply_hold(&target, was).await {
                tracing::debug!(error = %e, "a held item or filter couldn't be put back");
            }
        }
    }

    async fn apply_hold(&self, target: &Target, on: bool) -> Result<(), EngineError> {
        let act = match target {
            Target::Item { scene, id } => StudioAct::PushItem { scene: scene.clone(), id: *id, visible: on },
            Target::Filter { source, filter } => {
                StudioAct::PushFilter { name: source.clone(), filter: filter.clone(), enabled: on }
            }
        };
        self.studio_act(act).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::studio_host::{PreviewRect, StudioHost, StudioSettings, StudioStatus};

    fn item(scene: &str, id: i64) -> Target {
        Target::Item { scene: scene.into(), id }
    }

    #[test]
    fn the_four_actions_are_taken_apart_and_nothing_else_is() {
        assert_eq!(parse("push_show:Main\nCam"), Some((Kind::Show, "Main", "Cam")));
        assert_eq!(parse("push_hide:Main\nCam"), Some((Kind::Hide, "Main", "Cam")));
        assert_eq!(parse("push_filter_on:Cam\nBlur"), Some((Kind::FilterOn, "Cam", "Blur")));
        assert_eq!(parse("push_filter_off:Cam\nBlur"), Some((Kind::FilterOff, "Cam", "Blur")));
        // A name with a space or a colon in it is fine; only the first line break splits.
        assert_eq!(parse("push_show:My scene: 1\nCam 2"), Some((Kind::Show, "My scene: 1", "Cam 2")));
        for bad in [
            "push_show:Main",
            "push_show:\nCam",
            "push_show:Main\n",
            "push_show:",
            "show:Main\nCam",
            "push_to_talk:Mic",
            "push_showMain\nCam",
            "",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
            assert!(!is_push_hold(bad), "{bad:?}");
        }
        assert!(Kind::Show.held_state() && Kind::FilterOn.held_state());
        assert!(!Kind::Hide.held_state() && !Kind::FilterOff.held_state());
    }

    #[test]
    fn an_item_is_found_inside_groups_at_any_depth() {
        let items = json!([
            { "id": 1, "name": "Top", "visible": true, "children": [] },
            { "id": 2, "name": "Group", "visible": true, "children": [
                { "id": 5, "name": "Inside", "visible": false, "children": [] },
                { "id": 6, "name": "Deeper", "visible": true, "children": [
                    { "id": 9, "name": "Deepest", "visible": false, "children": [] }
                ] },
            ] },
        ]);
        assert_eq!(find_item(&items, "Top"), Some((1, true)));
        assert_eq!(find_item(&items, "Inside"), Some((5, false)));
        assert_eq!(find_item(&items, "Deepest"), Some((9, false)));
        assert_eq!(find_item(&items, "Group"), Some((2, true)));
        assert_eq!(find_item(&items, "Nope"), None);
        assert_eq!(find_item(&json!([]), "Top"), None);
        assert_eq!(find_item(&Value::Null, "Top"), None);
        // A repeated name: the first (top) one.
        let twice = json!([{ "id": 3, "name": "A", "children": [] }, { "id": 4, "name": "A", "children": [] }]);
        assert_eq!(find_item(&twice, "A"), Some((3, true)), "visible unknown counts as shown");
    }

    #[test]
    fn a_held_key_remembers_the_first_state_and_a_repeat_changes_nothing() {
        let mut h = Holds::default();
        assert!(h.is_empty());
        assert!(h.press("a", item("Main", 1), false));
        assert!(h.holding("a"));
        // Auto-repeat: the item is "shown" now, but what is remembered stays "hidden".
        assert!(!h.press("a", item("Main", 1), true));
        assert_eq!(h.release("a"), Some((item("Main", 1), false)));
        assert!(h.is_empty());
        // Let go twice, or never held: nothing.
        assert_eq!(h.release("a"), None);
        assert_eq!(h.release("never"), None);
    }

    #[test]
    fn two_keys_on_one_target_put_it_back_only_when_the_last_is_let_go() {
        let mut h = Holds::default();
        let (show, hide) = ("push_show:Main\nCam", "push_hide:Main\nCam");
        assert!(h.press(show, item("Main", 1), false));
        // The second key sees it shown (by the first) but the original was hidden.
        assert!(h.press(hide, item("Main", 1), true));
        assert_eq!(h.release(show), Some((item("Main", 1), false)), "the key still held (hide) decides");
        assert_eq!(h.release(hide), Some((item("Main", 1), false)), "the original");
        // In the other order too: letting go of hide leaves show held, so it shows again.
        assert!(h.press(show, item("Main", 1), true));
        assert!(h.press(hide, item("Main", 1), true));
        assert_eq!(h.release(hide), Some((item("Main", 1), true)));
        assert_eq!(h.release(show), Some((item("Main", 1), true)));
    }

    #[test]
    fn different_targets_are_held_separately_and_all_can_be_let_go_at_once() {
        let mut h = Holds::default();
        let filter = Target::Filter { source: "Cam".into(), filter: "Blur".into() };
        assert!(h.press("a", item("Main", 1), false));
        assert!(h.press("b", item("Other", 1), true));
        assert!(h.press("c", filter.clone(), true));
        // The same id in another scene is another item.
        assert_eq!(h.release("a"), Some((item("Main", 1), false)));
        let mut all = h.release_all();
        all.sort_by_key(|(_, was)| *was);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&(item("Other", 1), true)) && all.contains(&(filter, true)));
        assert!(h.is_empty() && !h.holding("b") && !h.holding("c"));
        assert!(h.release_all().is_empty(), "nothing held: nothing to put back");
    }

    #[test]
    fn a_press_that_could_not_be_applied_is_forgotten() {
        let mut h = Holds::default();
        assert!(h.press("a", item("Main", 1), false));
        h.forget("a");
        assert!(h.is_empty());
        assert!(h.press("a", item("Main", 1), true), "a new press counts again");
    }

    /// A Studio with scenes and filters that answers `Items`, `Filters` and the two push acts
    /// like the desktop's, and writes down every act.
    #[derive(Debug, Default)]
    struct Scripted {
        scenes: Mutex<BTreeMap<String, Value>>,
        filters: Mutex<BTreeMap<String, Value>>,
        log: Mutex<Vec<StudioAct>>,
        fail_pushes: std::sync::atomic::AtomicBool,
        stopped: std::sync::atomic::AtomicBool,
    }

    fn set_visible(items: &mut Value, id: i64, on: bool) -> bool {
        for it in items.as_array_mut().into_iter().flatten() {
            if it["id"].as_i64() == Some(id) {
                it["visible"] = json!(on);
                return true;
            }
            if set_visible(&mut it["children"], id, on) {
                return true;
            }
        }
        false
    }

    impl Scripted {
        fn shown(&self, scene: &str, source: &str) -> Option<bool> {
            let scenes = self.scenes.lock().unwrap();
            find_item(scenes.get(scene)?, source).map(|(_, v)| v)
        }
        fn enabled(&self, source: &str, filter: &str) -> Option<bool> {
            let f = self.filters.lock().unwrap();
            f.get(source)?.as_array()?.iter().find(|x| x["name"] == filter)?["enabled"].as_bool()
        }
        fn remove_item(&self, scene: &str, source: &str) {
            let mut scenes = self.scenes.lock().unwrap();
            if let Some(items) = scenes.get_mut(scene).and_then(Value::as_array_mut) {
                items.retain(|i| i["name"] != source);
            }
        }
        fn pushes(&self) -> usize {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|a| matches!(a, StudioAct::PushItem { .. } | StudioAct::PushFilter { .. }))
                .count()
        }
    }

    #[async_trait::async_trait]
    impl StudioHost for Scripted {
        async fn status(&self) -> StudioStatus {
            StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<StudioStatus, String> {
            self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(self.status().await)
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn options(&self, _kind: String) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn preview(&self, _rect: Option<PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn settings(&self) -> StudioSettings {
            StudioSettings::default()
        }
        async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn record(&self, _on: bool) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn record_pause(&self, _on: bool) -> Result<(), String> {
            Ok(())
        }
        async fn monitoring(&self) -> Result<Value, String> {
            Ok(json!({ "devices": [] }))
        }
        async fn stream_start(&self, _s: String, _k: String, _t: Vec<(u32, u32, u32)>) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        async fn import_obs(&self, _id: String) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn act(&self, act: StudioAct) -> Result<Value, String> {
            self.log.lock().unwrap().push(act.clone());
            match act {
                StudioAct::Items { scene } => {
                    self.scenes.lock().unwrap().get(&scene).cloned().ok_or_else(|| format!("No scene \"{scene}\"."))
                }
                StudioAct::Filters { name, .. } => {
                    self.filters.lock().unwrap().get(&name).cloned().ok_or_else(|| format!("No source \"{name}\"."))
                }
                StudioAct::PushItem { scene, id, visible } => {
                    if self.fail_pushes.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err("The Studio is busy.".into());
                    }
                    let mut scenes = self.scenes.lock().unwrap();
                    let items = scenes.get_mut(&scene).ok_or("No such scene.")?;
                    if set_visible(items, id, visible) { Ok(Value::Null) } else { Err("No such item.".into()) }
                }
                StudioAct::PushFilter { name, filter, enabled } => {
                    let mut filters = self.filters.lock().unwrap();
                    let list = filters.get_mut(&name).and_then(Value::as_array_mut).ok_or("No such source.")?;
                    let f = list.iter_mut().find(|f| f["name"] == filter).ok_or("No such filter.")?;
                    f["enabled"] = json!(enabled);
                    Ok(Value::Null)
                }
                _ => Ok(Value::Null),
            }
        }
    }

    async fn engine_with(host: Arc<Scripted>) -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
            .await
            .unwrap();
        e.set_studio_host(host);
        (e, dir)
    }

    fn world() -> Arc<Scripted> {
        let host = Arc::new(Scripted::default());
        host.scenes.lock().unwrap().extend([
            (
                "Main".to_string(),
                json!([
                    { "id": 1, "name": "Cam", "visible": false, "children": [] },
                    { "id": 2, "name": "Logo", "visible": true, "children": [] },
                    { "id": 3, "name": "Group", "visible": true, "children": [
                        { "id": 7, "name": "Banner", "visible": false, "children": [] }
                    ] },
                ]),
            ),
            ("Other".to_string(), json!([{ "id": 1, "name": "Cam", "visible": true, "children": [] }])),
        ]);
        host.filters.lock().unwrap().insert(
            "Cam".into(),
            json!([{ "name": "Blur", "kind": "blur", "enabled": false }, { "name": "Glow", "kind": "glow", "enabled": true }]),
        );
        host
    }

    #[test]
    fn a_hold_is_never_saved_and_never_an_undo_step() {
        assert!(!StudioAct::PushItem { scene: "Main".into(), id: 1, visible: true }.changes());
        assert!(!StudioAct::PushFilter { name: "Cam".into(), filter: "Blur".into(), enabled: true }.changes());
        assert!(StudioAct::PushItem { scene: "".into(), id: 1, visible: true }.check().is_err());
        assert!(StudioAct::PushFilter { name: "Cam".into(), filter: "x\ny".into(), enabled: true }.check().is_err());
    }

    #[tokio::test]
    async fn push_to_show_shows_while_held_and_puts_it_back_hidden() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        let key = "push_show:Main\nCam";
        e.studio_push_hold(key, true).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(true));
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false));
        // The same item in another scene was never touched.
        assert_eq!(host.shown("Other", "Cam"), Some(true));
    }

    #[tokio::test]
    async fn push_to_hide_hides_while_held_and_puts_it_back_shown() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        let key = "push_hide:Main\nLogo";
        e.studio_push_hold(key, true).await.unwrap();
        assert_eq!(host.shown("Main", "Logo"), Some(false));
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Logo"), Some(true));
    }

    #[tokio::test]
    async fn an_item_already_in_the_held_state_stays_so_after_the_key() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        // Push to show an item that is already shown: it must not end up hidden.
        e.studio_push_hold("push_show:Main\nLogo", true).await.unwrap();
        assert_eq!(host.shown("Main", "Logo"), Some(true));
        e.studio_push_hold("push_show:Main\nLogo", false).await.unwrap();
        assert_eq!(host.shown("Main", "Logo"), Some(true));
        // And push to hide on one already hidden stays hidden.
        e.studio_push_hold("push_hide:Main\nCam", true).await.unwrap();
        e.studio_push_hold("push_hide:Main\nCam", false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false));
    }

    #[tokio::test]
    async fn key_repeat_while_held_does_not_make_it_stay_after_release() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        let key = "push_show:Main\nCam";
        for _ in 0..5 {
            e.studio_push_hold(key, true).await.unwrap();
        }
        assert_eq!(host.pushes(), 1, "only the first press changed anything");
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false), "back to hidden, not stuck shown");
        // A second release is harmless.
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false));
    }

    #[tokio::test]
    async fn an_item_inside_a_group_is_held_too() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        e.studio_push_hold("push_show:Main\nBanner", true).await.unwrap();
        assert_eq!(host.shown("Main", "Banner"), Some(true));
        assert_eq!(host.shown("Main", "Group"), Some(true));
        e.studio_push_hold("push_show:Main\nBanner", false).await.unwrap();
        assert_eq!(host.shown("Main", "Banner"), Some(false));
    }

    /// The key is held over a scene switch: the item goes back in its own scene, and the scene
    /// switched to is not touched.
    #[tokio::test]
    async fn holding_across_a_scene_switch_puts_the_item_back_in_its_own_scene() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        let key = "push_show:Main\nCam";
        e.studio_push_hold(key, true).await.unwrap();
        e.studio_act(StudioAct::SetCurrent { scene: "Other".into() }).await.unwrap();
        assert_eq!(host.shown("Other", "Cam"), Some(true), "the other scene's own Cam is as it was");
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false));
        assert_eq!(host.shown("Other", "Cam"), Some(true));
        // The acts for it named the scene it was pressed in, both times.
        let log = host.log.lock().unwrap();
        let pushes: Vec<_> = log.iter().filter(|a| matches!(a, StudioAct::PushItem { .. })).collect();
        assert_eq!(pushes.len(), 2);
        assert!(pushes.iter().all(|a| matches!(a, StudioAct::PushItem { scene, id: 1, .. } if scene == "Main")));
    }

    #[tokio::test]
    async fn an_item_deleted_while_held_is_skipped_quietly_on_release() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        let key = "push_show:Main\nCam";
        e.studio_push_hold(key, true).await.unwrap();
        host.remove_item("Main", "Cam");
        e.studio_push_hold(key, false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), None);
        // And the key can be used again (it was let go): a new press finds nothing, plainly.
        let err = e.studio_push_hold(key, true).await.unwrap_err();
        assert!(
            matches!(err, EngineError::Unavailable(ref m) if m.contains("\"Cam\"") && m.contains("set the key again")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_missing_scene_item_or_filter_says_what_to_do_and_holds_nothing() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        for key in [
            "push_show:Main\nNothing",
            "push_hide:Gone\nCam",
            "push_filter_on:Cam\nNothing",
            "push_filter_off:Nothing\nBlur",
        ] {
            let err = e.studio_push_hold(key, true).await.unwrap_err();
            assert!(!err.to_string().is_empty(), "{key}");
            assert!(e.studio().holds.lock().unwrap().is_empty(), "{key}");
            // Its release is nothing.
            e.studio_push_hold(key, false).await.unwrap();
        }
        assert_eq!(host.pushes(), 0);
        assert!(matches!(e.studio_push_hold("push_show:Main", true).await, Err(EngineError::Invalid(_))));
        assert!(matches!(e.studio_push_hold("mute:Mic", true).await, Err(EngineError::Invalid(_))));
    }

    #[tokio::test]
    async fn a_hold_the_studio_refuses_leaves_nothing_held() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        host.fail_pushes.store(true, std::sync::atomic::Ordering::SeqCst);
        let key = "push_show:Main\nCam";
        assert!(e.studio_push_hold(key, true).await.is_err());
        assert!(e.studio().holds.lock().unwrap().is_empty());
        host.fail_pushes.store(false, std::sync::atomic::Ordering::SeqCst);
        e.studio_push_hold(key, true).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(true));
    }

    #[tokio::test]
    async fn push_to_enable_and_disable_a_filter() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        e.studio_push_hold("push_filter_on:Cam\nBlur", true).await.unwrap();
        assert_eq!(host.enabled("Cam", "Blur"), Some(true));
        e.studio_push_hold("push_filter_on:Cam\nBlur", false).await.unwrap();
        assert_eq!(host.enabled("Cam", "Blur"), Some(false));
        e.studio_push_hold("push_filter_off:Cam\nGlow", true).await.unwrap();
        assert_eq!(host.enabled("Cam", "Glow"), Some(false));
        e.studio_push_hold("push_filter_off:Cam\nGlow", false).await.unwrap();
        assert_eq!(host.enabled("Cam", "Glow"), Some(true));
    }

    #[tokio::test]
    async fn two_keys_on_one_item_end_as_it_began() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        e.studio_push_hold("push_show:Main\nCam", true).await.unwrap();
        e.studio_push_hold("push_hide:Main\nCam", true).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false), "the later key wins while both are held");
        e.studio_push_hold("push_hide:Main\nCam", false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(true), "hide let go while show is still held: shown again");
        e.studio_push_hold("push_show:Main\nCam", false).await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false), "...and at the end, as it began");
    }

    /// Keys that can't report their release (the shortcuts were let go for a shortcut field, or
    /// replaced) put everything back.
    #[tokio::test]
    async fn releasing_every_hold_puts_everything_back() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        e.studio_push_hold("push_show:Main\nCam", true).await.unwrap();
        e.studio_push_hold("push_hide:Main\nLogo", true).await.unwrap();
        e.studio_push_hold("push_filter_on:Cam\nBlur", true).await.unwrap();
        e.studio_release_holds().await;
        assert_eq!(host.shown("Main", "Cam"), Some(false));
        assert_eq!(host.shown("Main", "Logo"), Some(true));
        assert_eq!(host.enabled("Cam", "Blur"), Some(false));
        assert!(e.studio().holds.lock().unwrap().is_empty());
        // Nothing held: nothing happens, and a late release of a key is a no-op.
        let before = host.pushes();
        e.studio_release_holds().await;
        e.studio_push_hold("push_show:Main\nCam", false).await.unwrap();
        assert_eq!(host.pushes(), before);
    }

    #[tokio::test]
    async fn stopping_the_studio_puts_held_items_back_first() {
        let host = world();
        let (e, _d) = engine_with(host.clone()).await;
        e.studio_push_hold("push_show:Main\nCam", true).await.unwrap();
        e.studio_stop().await.unwrap();
        assert_eq!(host.shown("Main", "Cam"), Some(false));
        assert!(host.stopped.load(std::sync::atomic::Ordering::SeqCst));
        // The put-back came before the stop.
        let log = host.log.lock().unwrap();
        assert!(matches!(log.last(), Some(StudioAct::PushItem { visible: false, .. })));
    }

    #[test]
    fn the_hold_keys_are_valid_shortcut_settings_and_bad_names_are_not() {
        let mut s = StudioSettings::default();
        for a in ["push_show:Main\nCam", "push_hide:Main\nCam", "push_filter_on:Cam\nBlur", "push_filter_off:Cam\nBlur"]
        {
            s.hotkeys.insert(a.into(), "Alt+H".into());
        }
        assert!(s.check().is_ok());
        let mut bad = StudioSettings::default();
        bad.hotkeys.insert("push_show:Main".into(), "Alt+H".into());
        assert!(bad.check().is_err());
        let mut bad = StudioSettings::default();
        bad.hotkeys.insert("push_blink:Main\nCam".into(), "Alt+H".into());
        assert!(bad.check().is_err());
    }
}
