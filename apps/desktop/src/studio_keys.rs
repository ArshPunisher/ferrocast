//! The Studio's keyboard shortcuts (2.1): global, like OBS's hotkeys, so they work while a
//! game has the keyboard. Each is an action from the Studio's settings, as OBS has them:
//! streaming, recording (pause, split, chapter), replay buffer, virtual camera, Studio Mode
//! (Transition, quick transitions, swap, fade to black), a scene, a source's show and hide, a
//! filter on or off, media controls, screenshots, undo, mute, push to talk and push to mute
//! (held down).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ferrocast_engine::studio::push_hold;
use ferrocast_engine::studio_host::{AudioChange, FilterEdit, ItemEdit, MediaAct, StudioAct};
use ferrocast_engine::{Engine, EngineError};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// A request (not a shortcut action: none can be set to it) for the key worker to let every key
/// held down go and put its items back, for when the keys can't report their release any more.
pub(crate) const RELEASE_HOLDS: &str = "release_holds";

/// The Studio's shortcuts registered now and what each does, and the queue their presses go
/// through: one worker runs them in order, so a quick push-to-talk tap never ends unmuted.
pub struct StudioKeys {
    keys: Mutex<HashMap<Shortcut, Vec<String>>>,
    queue: tokio::sync::mpsc::UnboundedSender<(String, bool)>,
    /// What the settings last asked for (kept while the keys are let go for a shortcut field).
    wanted: Mutex<BTreeMap<String, String>>,
    /// Whether a shortcut field is waiting for a key (the global shortcuts are let go).
    gate: Mutex<CaptureGate>,
    /// Which shortcuts are down now, per held action: an action with two keys (`F8|F9`) is let go
    /// only when the last is.
    down: Mutex<PressedKeys>,
}

/// The physical shortcuts down for each action that acts on letting go. Two keys bound to one
/// action are one hold: it starts with the first down and ends with the last up.
#[derive(Debug, Default)]
pub(crate) struct PressedKeys(HashMap<String, HashSet<String>>);

impl PressedKeys {
    /// `key` went down for `action` → whether the action's press goes on (the first key down).
    pub(crate) fn down(&mut self, action: &str, key: &str) -> bool {
        let set = self.0.entry(action.to_string()).or_default();
        let first = set.is_empty();
        set.insert(key.to_string());
        first
    }

    /// `key` came up → whether the action's release goes on (nothing else of it is down). A key
    /// that was never seen going down (pressed before the shortcuts were replaced) still ends it
    /// when nothing is down.
    pub(crate) fn up(&mut self, action: &str, key: &str) -> bool {
        let Some(set) = self.0.get_mut(action) else { return true };
        set.remove(key);
        if set.is_empty() {
            self.0.remove(action);
            true
        } else {
            false
        }
    }

    /// The shortcuts were replaced or let go: nothing is down any more.
    /// Every action that had a key down, forgotten: they are to be let go by the caller (a key
    /// held while the shortcuts were replaced would never be seen coming up).
    pub(crate) fn clear(&mut self) -> Vec<String> {
        self.0.drain().map(|(a, _)| a).collect()
    }
}

/// How long a shortcut field's "waiting for a key" holds the global shortcuts without being
/// asked again (the page asks every few seconds): if the page goes away without saying so,
/// they come back by themselves.
pub(crate) const CAPTURE_HOLD: Duration = Duration::from_secs(30);

/// What a "waiting for a key" call changes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Capture {
    /// The shortcuts are to be let go now.
    Suspend,
    /// Still waiting: the hold is extended.
    Renew,
    /// The shortcuts are to be put back now.
    Resume,
    /// Nothing changes.
    Nothing,
}

/// Whether the global shortcuts are let go for a shortcut field, and until when (the pure part,
/// tested without a window).
#[derive(Debug, Default)]
pub(crate) struct CaptureGate {
    until: Option<Instant>,
}

impl CaptureGate {
    /// The page says it is waiting for a key (`on`) or has stopped.
    pub(crate) fn set(&mut self, on: bool, now: Instant) -> Capture {
        match (on, self.until) {
            (true, None) => {
                self.until = Some(now + CAPTURE_HOLD);
                Capture::Suspend
            }
            (true, Some(_)) => {
                self.until = Some(now + CAPTURE_HOLD);
                Capture::Renew
            }
            (false, Some(_)) => {
                self.until = None;
                Capture::Resume
            }
            (false, None) => Capture::Nothing,
        }
    }

    /// The hold ran out (the page never said it stopped) → the shortcuts are to be put back.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        match self.until {
            Some(t) if now >= t => {
                self.until = None;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.until.is_some()
    }
}

impl StudioKeys {
    /// Starts the worker that runs shortcut actions, one after another.
    pub fn new(engine: Engine) -> StudioKeys {
        let (queue, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
        tauri::async_runtime::spawn(async move {
            while let Some((action, pressed)) = rx.recv().await {
                if let Err(e) = run(&engine, &action, pressed).await {
                    tracing::warn!(action, error = %e, "a Studio shortcut didn't work");
                }
            }
        });
        StudioKeys {
            keys: Mutex::new(HashMap::new()),
            queue,
            wanted: Mutex::new(BTreeMap::new()),
            gate: Mutex::new(CaptureGate::default()),
            down: Mutex::new(PressedKeys::default()),
        }
    }
}

/// What the settings ask for: each key once, with every action it runs (a key may be shared by
/// several actions, as in OBS, and an action may have several keys), and the keys that aren't
/// keys this computer knows.
pub(crate) fn plan(wanted: &BTreeMap<String, String>) -> (Vec<(Shortcut, Vec<String>)>, Vec<String>) {
    let mut by_key: Vec<(Shortcut, Vec<String>)> = Vec::new();
    let mut unknown = Vec::new();
    for (action, value) in wanted {
        for keys in value.split('|') {
            match keys.parse::<Shortcut>() {
                Ok(sc) => match by_key.iter_mut().find(|(k, _)| *k == sc) {
                    Some((_, actions)) => {
                        if !actions.contains(action) {
                            actions.push(action.clone());
                        }
                    }
                    None => by_key.push((sc, vec![action.clone()])),
                },
                Err(_) => unknown.push(keys.to_string()),
            }
        }
    }
    (by_key, unknown)
}

/// Registers these shortcuts in place of the old ones → the keys that couldn't be (taken by
/// another app, or not keys this computer knows).
pub fn apply(app: &AppHandle, wanted: &BTreeMap<String, String>) -> Vec<String> {
    let Some(state) = app.try_state::<StudioKeys>() else { return Vec::new() };
    *state.wanted.lock().unwrap_or_else(|p| p.into_inner()) = wanted.clone();
    // A shortcut field is waiting for a key: they are put in when it is done.
    if state.gate.lock().unwrap_or_else(|p| p.into_inner()).active() {
        return Vec::new();
    }
    let mut map = state.keys.lock().unwrap_or_else(|p| p.into_inner());
    // Shortcuts about to be replaced can't report a key let go any more: nothing stays held.
    let _ = state.queue.send((RELEASE_HOLDS.to_string(), false));
    let held = state.down.lock().unwrap_or_else(|p| p.into_inner()).clear();
    for action in held.into_iter().filter(|a| !push_hold::is_push_hold(a)) {
        let _ = state.queue.send((action, false));
    }
    for (sc, _) in map.drain() {
        let _ = app.global_shortcut().unregister(sc);
    }
    let (planned, mut failed) = plan(wanted);
    for (sc, actions) in planned {
        match app.global_shortcut().register(sc) {
            Ok(()) => {
                tracing::info!(keys = %sc, ?actions, "a Studio shortcut is set");
                map.insert(sc, actions);
            }
            Err(e) => {
                tracing::warn!(keys = %sc, error = %e, "a Studio shortcut couldn't be registered");
                failed.push(sc.to_string());
            }
        }
    }
    failed
}

/// A shortcut field of the dashboard waits for a key (`on`) or has stopped: every global
/// shortcut of the app is let go for the wait, so the page gets each key (those assigned already
/// too) and no action runs from it, then they are put back (also when the page goes away
/// without saying so: after [`CAPTURE_HOLD`]).
pub(crate) fn capture(app: &AppHandle, on: bool) {
    let Some(state) = app.try_state::<StudioKeys>() else { return };
    let change = state.gate.lock().unwrap_or_else(|p| p.into_inner()).set(on, Instant::now());
    match change {
        Capture::Suspend => {
            tracing::info!("the shortcuts are let go while a shortcut field waits for a key");
            // A key held now would never be seen coming up.
            let _ = state.queue.send((RELEASE_HOLDS.to_string(), false));
            let held = state.down.lock().unwrap_or_else(|p| p.into_inner()).clear();
            for action in held.into_iter().filter(|a| !push_hold::is_push_hold(a)) {
                let _ = state.queue.send((action, false));
            }
            let _ = app.global_shortcut().unregister_all();
            watch_capture(app.clone());
        }
        Capture::Renew => watch_capture(app.clone()),
        Capture::Resume => restore(app),
        Capture::Nothing => {}
    }
}

/// Puts the shortcuts back when the hold runs out.
fn watch_capture(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(CAPTURE_HOLD + Duration::from_millis(200)).await;
        let Some(state) = app.try_state::<StudioKeys>() else { return };
        let expired = state.gate.lock().unwrap_or_else(|p| p.into_inner()).expire(Instant::now());
        if expired {
            tracing::warn!("a shortcut field never said it stopped waiting: the shortcuts are back");
            restore(&app);
        }
    });
}

/// The app's shortcuts back after a shortcut field: the show-window key, the panic key and the
/// Studio's.
fn restore(app: &AppHandle) {
    crate::restore_app_shortcuts(app);
    let wanted = match app.try_state::<StudioKeys>() {
        Some(s) => s.wanted.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        None => return,
    };
    let _ = apply(app, &wanted);
}

/// Runs the Studio's action for this shortcut → false when it isn't one of the Studio's.
pub fn handle(app: &AppHandle, shortcut: &Shortcut, state: ShortcutState) -> bool {
    let Some(keys) = app.try_state::<StudioKeys>() else { return false };
    let Some(actions) = keys.keys.lock().unwrap_or_else(|p| p.into_inner()).get(shortcut).cloned() else {
        return false;
    };
    let pressed = state == ShortcutState::Pressed;
    let key = shortcut.to_string();
    for action in actions {
        tracing::debug!(action, pressed, "a Studio shortcut");
        if !pressed && !acts_on_release(&action) {
            continue;
        }
        if acts_on_release(&action) {
            let mut down = keys.down.lock().unwrap_or_else(|p| p.into_inner());
            let go = if pressed { down.down(&action, &key) } else { down.up(&action, &key) };
            if !go {
                continue;
            }
        }
        let _ = keys.queue.send((action, pressed));
    }
    true
}

/// Only push to talk, push to mute and the push-to-show/hide keys act on letting go.
pub(crate) fn acts_on_release(action: &str) -> bool {
    action.starts_with("push_to_talk:") || action.starts_with("push_to_mute:") || push_hold::is_push_hold(action)
}

async fn run(engine: &Engine, action: &str, pressed: bool) -> Result<(), EngineError> {
    if action == RELEASE_HOLDS {
        engine.studio_release_holds().await;
        return Ok(());
    }
    if push_hold::is_push_hold(action) {
        return engine.studio_push_hold(action, pressed).await;
    }
    match action {
        "go_live" => {
            engine.go_live().await?;
        }
        "end_stream" => {
            engine.end_live().await?;
        }
        "record" => {
            let on = engine.studio_stats().await?["record"]["active"].as_bool().unwrap_or(false);
            engine.studio_record(!on).await?;
        }
        "start_record" => {
            let on = engine.studio_stats().await?["record"]["active"].as_bool().unwrap_or(false);
            if !on {
                engine.studio_record(true).await?;
            }
        }
        "stop_record" => {
            let on = engine.studio_stats().await?["record"]["active"].as_bool().unwrap_or(false);
            if on {
                engine.studio_record(false).await?;
            }
        }
        "unpause_record" => {
            let paused = engine.studio_stats().await?["record"]["paused"].as_bool().unwrap_or(false);
            if paused {
                engine.studio_record_pause(false).await?;
            }
        }
        "start_replay" | "stop_replay" => {
            let on = engine.studio_stats().await?["replay"]["active"].as_bool().unwrap_or(false);
            if on != (action == "start_replay") {
                engine.studio_replay(action == "start_replay").await?;
            }
        }
        "start_virtualcam" | "stop_virtualcam" => {
            let on = engine.studio_stats().await?["virtualcam"]["active"].as_bool().unwrap_or(false);
            if on != (action == "start_virtualcam") {
                engine.studio_virtual_camera(action == "start_virtualcam", None).await?;
            }
        }
        "pause_record" => {
            let paused = engine.studio_stats().await?["record"]["paused"].as_bool().unwrap_or(false);
            engine.studio_record_pause(!paused).await?;
        }
        "replay" => {
            let on = engine.studio_stats().await?["replay"]["active"].as_bool().unwrap_or(false);
            engine.studio_replay(!on).await?;
        }
        "save_replay" => {
            engine.studio_replay_save().await?;
        }
        "virtualcam" => {
            let on = engine.studio_stats().await?["virtualcam"]["active"].as_bool().unwrap_or(false);
            engine.studio_virtual_camera(!on, None).await?;
        }
        "split_record" => engine.studio_record_split().await?,
        "chapter" => engine.studio_record_chapter(String::new()).await?,
        "studio_mode" => {
            let on = !engine.studio_act(StudioAct::Scenes).await?["preview"].is_null();
            engine.studio_act(StudioAct::StudioMode { on: !on }).await?;
        }
        "transition" => {
            engine.studio_act(StudioAct::TransitionNow { quick: None }).await?;
        }
        "swap_scenes" => {
            engine.studio_act(StudioAct::SwapScenes).await?;
        }
        "fade_to_black" => {
            engine.studio_act(StudioAct::FadeToBlack).await?;
        }
        "screenshot" => {
            engine.studio_act(StudioAct::ScreenshotProgram).await?;
        }
        "vertical:show_preview" | "vertical:hide_preview" | "vertical:toggle_preview" => {
            // There is no preview of its own to show or hide (the page places it): these keys
            // turn the vertical picture on and off, and while the Studio streams (connecting
            // and reconnecting too) that ends the vertical stream or starts one: not then.
            if !engine.studio_stream_stats().await?.is_null() {
                tracing::warn!(action, "the vertical preview keys do nothing while the Studio streams");
                return Ok(());
            }
            let on = !engine.studio_act(StudioAct::Scenes).await?["vertical"].is_null();
            let want = match action {
                "vertical:show_preview" => true,
                "vertical:hide_preview" => false,
                _ => !on,
            };
            if want && !on {
                start_vertical_with_fallback(engine).await?;
            } else if !want && on {
                engine.studio_act(StudioAct::StopVertical).await?;
            }
        }
        "vertical:screenshot" => {
            let scenes = engine.studio_act(StudioAct::Scenes).await?;
            if let Some(name) = scenes["vertical"]["current"].as_str().filter(|s| !s.is_empty()) {
                engine.studio_act(StudioAct::Screenshot { name: name.to_string() }).await?;
            }
        }
        "performance_mode" => {
            let on = engine.studio_preview_paused().await?;
            engine.studio_set_preview_paused(!on).await?;
        }
        "performance_mode_on" | "performance_mode_off" => {
            engine.studio_set_preview_paused(action == "performance_mode_on").await?;
        }
        "undo" => {
            engine.studio_act(StudioAct::Undo).await?;
        }
        "redo" => {
            engine.studio_act(StudioAct::Redo).await?;
        }
        a => {
            let audio = |name: &str, muted: bool| StudioAct::SetAudio {
                name: name.to_string(),
                change: AudioChange { muted: Some(muted), by_shortcut: true, ..Default::default() },
            };
            let pair = |p: &str| a.strip_prefix(p).and_then(|r| r.split_once('\n'));
            if let Some(scene) = a.strip_prefix("scene:") {
                engine.studio_act(StudioAct::SetCurrent { scene: scene.to_string() }).await?;
            } else if let Some(n) = a.strip_prefix("quick:").and_then(|n| n.parse::<usize>().ok()) {
                // Quick transitions are numbered from 1, as they're listed.
                let scenes = engine.studio_act(StudioAct::Scenes).await?;
                if let Some(q) = scenes["quick"].get(n.saturating_sub(1)) {
                    let quick = serde_json::from_value(q.clone()).ok();
                    engine.studio_act(StudioAct::TransitionNow { quick }).await?;
                }
            } else if let Some((scene, source)) = pair("show:").or_else(|| pair("hide:")) {
                let items = engine.studio_act(StudioAct::Items { scene: scene.to_string() }).await?;
                // Inside a group too.
                let id = push_hold::find_item(&items, source).map(|(id, _)| id);
                if let Some(id) = id {
                    let edit = if a.starts_with("show:") { ItemEdit::Show } else { ItemEdit::Hide };
                    engine.studio_act(StudioAct::EditItem { scene: scene.to_string(), id, edit }).await?;
                }
            } else if let Some((source, filter)) = pair("filter:") {
                let list = engine.studio_act(StudioAct::Filters { name: source.to_string(), item: None }).await?;
                let on = list
                    .as_array()
                    .and_then(|l| l.iter().find(|f| f["name"] == Value::String(filter.into())))
                    .and_then(|f| f["enabled"].as_bool())
                    .unwrap_or(false);
                let edit = if on { FilterEdit::Disable } else { FilterEdit::Enable };
                engine
                    .studio_act(StudioAct::EditFilter {
                        name: source.to_string(),
                        filter: filter.to_string(),
                        edit,
                        item: None,
                    })
                    .await?;
            } else if let Some(name) = a.strip_prefix("screenshot:") {
                engine.studio_act(StudioAct::Screenshot { name: name.to_string() }).await?;
            } else if let Some((what, name)) = a.strip_prefix("media_").and_then(|r| r.split_once(':')) {
                let what = match what {
                    "play_pause" => MediaAct::PlayPause,
                    "restart" => MediaAct::Restart,
                    "stop" => MediaAct::Stop,
                    "next" => MediaAct::Next,
                    "previous" => MediaAct::Previous,
                    _ => return Ok(()),
                };
                engine.studio_act(StudioAct::Media { name: name.to_string(), what: Some(what), seek_ms: None }).await?;
            } else if let Some(name) = a.strip_prefix("push_to_mute:") {
                // Switched on for the sound: the mixer gates it (with its release delay, in samples).
                // Not: held mutes it, let go hears it, as before.
                let gated = engine
                    .studio_act(StudioAct::PushKey { name: name.to_string(), talk: false, down: pressed })
                    .await?;
                if gated != Value::Bool(true) {
                    engine.studio_act(audio(name, pressed)).await?;
                }
            } else if let Some(name) = a.strip_prefix("mute:") {
                let muted = muted(engine, name).await?;
                engine.studio_act(audio(name, !muted)).await?;
            } else if let Some(name) = a.strip_prefix("push_to_talk:") {
                let gated =
                    engine.studio_act(StudioAct::PushKey { name: name.to_string(), talk: true, down: pressed }).await?;
                if gated != Value::Bool(true) {
                    // Held: heard; let go: muted.
                    engine.studio_act(audio(name, !pressed)).await?;
                }
            }
        }
    }
    Ok(())
}

/// Enables the vertical canvas, picking a free default scene name if `"Vertical"` is already
/// used by a main scene or source (`studio_name_taken`). If vertical scenes already exist from
/// before, the engine ignores `scene` and restores them.
async fn start_vertical_with_fallback(engine: &Engine) -> Result<(), EngineError> {
    for n in 1..=10 {
        let scene = if n == 1 { "Vertical".to_string() } else { format!("Vertical {n}") };
        match engine.studio_act(StudioAct::StartVertical { scene }).await {
            Ok(_) => return Ok(()),
            Err(EngineError::Refused { code: "studio_name_taken", .. }) if n < 10 => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Whether a sound in the scene on air (or a global device) is muted now.
async fn muted(engine: &Engine, name: &str) -> Result<bool, EngineError> {
    let scenes = engine.studio_act(StudioAct::Scenes).await?;
    let scene = scenes["current"].as_str().unwrap_or_default().to_string();
    let list = engine.studio_act(StudioAct::Audio { scene }).await?;
    Ok(list
        .as_array()
        .and_then(|l| l.iter().find(|a| a["name"] == Value::String(name.into())))
        .and_then(|a| a["muted"].as_bool())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_keys_on_one_action_are_one_hold() {
        let mut k = PressedKeys::default();
        let a = "push_show:Main\nCam";
        assert!(k.down(a, "F8"), "the first key starts the hold");
        assert!(!k.down(a, "F9"), "the second is part of it");
        assert!(!k.down(a, "F9"), "a repeat changes nothing");
        assert!(!k.up(a, "F8"), "F9 is still down: not let go");
        assert!(k.up(a, "F9"), "the last key ends it");
        // Starts fresh afterwards, and each action counts on its own.
        assert!(k.down(a, "F8"));
        assert!(k.down("push_hide:Main\nLogo", "F8"), "another action on the same key holds on its own");
        assert!(k.up(a, "F8"));
    }

    #[test]
    fn a_release_of_a_key_never_seen_down_ends_the_hold_and_clear_forgets_all() {
        let mut k = PressedKeys::default();
        let a = "push_show:Main\nCam";
        assert!(k.up(a, "F8"), "nothing down: it ends");
        assert!(k.down(a, "F8"));
        assert_eq!(k.clear(), vec![a.to_string()], "the action that was down is handed back to be let go");
        assert!(k.down(a, "F9"), "after the shortcuts were replaced it starts again");
    }

    use super::{CAPTURE_HOLD, Capture, CaptureGate};
    use std::time::{Duration, Instant};

    #[test]
    fn waiting_for_a_key_lets_the_shortcuts_go_and_stopping_puts_them_back() {
        let mut g = CaptureGate::default();
        let t = Instant::now();
        assert!(!g.active());
        assert_eq!(g.set(true, t), Capture::Suspend);
        assert!(g.active());
        // The page asks again every few seconds: nothing is let go twice.
        assert_eq!(g.set(true, t + Duration::from_secs(10)), Capture::Renew);
        assert_eq!(g.set(false, t + Duration::from_secs(12)), Capture::Resume);
        assert!(!g.active());
        // Stopping when it wasn't waiting changes nothing.
        assert_eq!(g.set(false, t + Duration::from_secs(13)), Capture::Nothing);
    }

    #[test]
    fn a_page_that_goes_away_gives_the_shortcuts_back_after_the_hold() {
        let mut g = CaptureGate::default();
        let t = Instant::now();
        g.set(true, t);
        assert!(!g.expire(t + CAPTURE_HOLD - Duration::from_secs(1)));
        assert!(g.active());
        // Asked again at 20 s: held until 50 s.
        g.set(true, t + Duration::from_secs(20));
        assert!(!g.expire(t + CAPTURE_HOLD + Duration::from_secs(1)));
        assert!(g.expire(t + Duration::from_secs(20) + CAPTURE_HOLD));
        assert!(!g.active());
        // Nothing to give back twice.
        assert!(!g.expire(t + Duration::from_secs(100)));
    }

    use super::*;

    fn wanted(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter().map(|(a, k)| (a.to_string(), k.to_string())).collect()
    }

    #[test]
    fn an_action_may_have_several_keys() {
        let (planned, unknown) = plan(&wanted(&[("record", "Alt+R|F9")]));
        assert!(unknown.is_empty());
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|(_, a)| a == &["record".to_string()]));
    }

    #[test]
    fn a_key_shared_by_two_actions_runs_both() {
        let (planned, _) = plan(&wanted(&[("mute:Mic", "F8"), ("scene:BRB", "F8"), ("record", "F9")]));
        assert_eq!(planned.len(), 2);
        let shared = planned.iter().find(|(_, a)| a.len() == 2).map(|(_, a)| a.clone()).unwrap();
        assert_eq!(shared, vec!["mute:Mic".to_string(), "scene:BRB".to_string()]);
    }

    #[test]
    fn keys_this_computer_does_not_know_are_reported_not_dropped_silently() {
        let (planned, unknown) = plan(&wanted(&[("record", "F9|Nonsense+Key"), ("undo", "Alt+Z")]));
        assert_eq!(planned.len(), 2);
        assert_eq!(unknown, vec!["Nonsense+Key".to_string()]);
    }

    #[test]
    fn the_same_key_twice_on_one_action_counts_once() {
        let (planned, _) = plan(&wanted(&[("record", "F9|F9")]));
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].1.len(), 1);
    }

    #[derive(Debug, Default)]
    struct FakeHost {
        vertical_on: std::sync::atomic::AtomicBool,
        vertical_current: Mutex<Option<String>>,
        taken_names: Mutex<Vec<String>>,
        acts: Mutex<Vec<StudioAct>>,
        /// The Studio streams (its stream stats aren't null).
        streaming: std::sync::atomic::AtomicBool,
        /// How often the vertical stream was told to stop.
        vstream_stops: std::sync::atomic::AtomicUsize,
        /// Performance mode: the preview is paused.
        paused: std::sync::atomic::AtomicBool,
    }

    impl FakeHost {
        fn take_acts(&self) -> Vec<StudioAct> {
            std::mem::take(&mut *self.acts.lock().unwrap_or_else(|p| p.into_inner()))
        }
    }

    #[async_trait::async_trait]
    impl ferrocast_engine::studio_host::StudioHost for FakeHost {
        async fn status(&self) -> ferrocast_engine::studio_host::StudioStatus {
            ferrocast_engine::studio_host::StudioStatus { available: true, running: true, ..Default::default() }
        }
        async fn start(&self) -> Result<ferrocast_engine::studio_host::StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn stop(&self) -> Result<ferrocast_engine::studio_host::StudioStatus, String> {
            Ok(self.status().await)
        }
        async fn kinds(&self) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn options(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn preview(&self, _rect: Option<ferrocast_engine::studio_host::PreviewRect>) -> Result<(), String> {
            Ok(())
        }
        async fn set_preview_paused(&self, paused: bool) -> Result<(), String> {
            self.paused.store(paused, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn preview_paused(&self) -> bool {
            self.paused.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn defaults(&self, _kind: String) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }
        async fn act(&self, act: StudioAct) -> Result<Value, String> {
            self.acts.lock().unwrap_or_else(|p| p.into_inner()).push(act.clone());
            match act {
                StudioAct::Scenes => {
                    let on = self.vertical_on.load(std::sync::atomic::Ordering::SeqCst);
                    let cur = self.vertical_current.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    Ok(serde_json::json!({
                        "scenes": [],
                        "current": "Scene",
                        "vertical": if on {
                            serde_json::json!({
                                "width": 1080,
                                "height": 1920,
                                "scenes": [],
                                "current": cur.unwrap_or_default()
                            })
                        } else {
                            Value::Null
                        }
                    }))
                }
                StudioAct::StartVertical { scene } => {
                    if self.taken_names.lock().unwrap_or_else(|p| p.into_inner()).contains(&scene) {
                        return Err(format!("name: \"{scene}\" is taken: pick another name."));
                    }
                    self.vertical_on.store(true, std::sync::atomic::Ordering::SeqCst);
                    let mut cur = self.vertical_current.lock().unwrap_or_else(|p| p.into_inner());
                    if cur.is_none() {
                        *cur = Some(scene);
                    }
                    Ok(Value::Null)
                }
                StudioAct::StopVertical => {
                    self.vertical_on.store(false, std::sync::atomic::Ordering::SeqCst);
                    Ok(Value::Null)
                }
                StudioAct::Items { .. } => Ok(serde_json::json!([
                    { "id": 4, "name": "Banner", "visible": false, "children": [
                        { "id": 9, "name": "Deep", "visible": false, "children": [] }
                    ] },
                ])),
                _ => Ok(Value::Null),
            }
        }
        async fn settings(&self) -> ferrocast_engine::studio_host::StudioSettings {
            Default::default()
        }
        async fn set_settings(
            &self,
            _s: ferrocast_engine::studio_host::StudioSettings,
        ) -> Result<Option<String>, String> {
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
            Ok(serde_json::json!({ "devices": [] }))
        }
        async fn stream_start(
            &self,
            _server: String,
            _key: String,
            _tracks: Vec<(u32, u32, u32)>,
        ) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn stream_stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stream_stats(&self) -> Result<Value, String> {
            Ok(if self.streaming.load(std::sync::atomic::Ordering::SeqCst) {
                serde_json::json!({ "active": true, "reconnecting": false, "error": null })
            } else {
                Value::Null
            })
        }
        async fn vertical_stream_stop(&self) -> Result<(), String> {
            self.vstream_stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn obs_collections(&self) -> Result<Value, String> {
            Ok(serde_json::json!([]))
        }
        async fn import_obs(&self, _id: String) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }

    async fn test_engine(host: std::sync::Arc<FakeHost>) -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap_or_else(|e| panic!("engine: {e}"));
        engine.set_studio_host(host);
        (engine, dir)
    }

    /// Performance mode's keys: one toggles, the others say on or off (so a key can never leave
    /// it the wrong way round), and none of them touches the outputs.
    #[tokio::test]
    async fn performance_mode_keys_toggle_pause_and_resume_the_preview_only() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;
        let paused = || host.paused.load(std::sync::atomic::Ordering::SeqCst);
        run(&engine, "performance_mode", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(paused());
        run(&engine, "performance_mode", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(!paused());
        // On twice stays on; off twice stays off.
        for _ in 0..2 {
            run(&engine, "performance_mode_on", true).await.unwrap_or_else(|e| panic!("{e}"));
            assert!(paused());
        }
        for _ in 0..2 {
            run(&engine, "performance_mode_off", true).await.unwrap_or_else(|e| panic!("{e}"));
            assert!(!paused());
        }
        // Nothing was asked of the scenes, the stream or the recording.
        assert!(host.take_acts().is_empty());
        assert_eq!(host.vstream_stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        engine.shutdown();
    }

    #[tokio::test]
    async fn performance_mode_while_streaming_leaves_the_stream_running() {
        use std::sync::atomic::Ordering::SeqCst;
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;
        host.streaming.store(true, SeqCst);
        run(&engine, "performance_mode_on", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(host.paused.load(SeqCst));
        assert!(host.streaming.load(SeqCst), "the stream is still on");
        assert_eq!(host.vstream_stops.load(SeqCst), 0, "no stream was stopped");
        engine.shutdown();
    }

    #[test]
    fn only_the_held_keys_act_on_letting_go() {
        for a in ["push_to_talk:Mic", "push_to_mute:Mic", "push_show:Main\nCam", "push_hide:Main\nCam"] {
            assert!(acts_on_release(a), "{a}");
        }
        for a in ["push_filter_on:Cam\nBlur", "push_filter_off:Cam\nBlur"] {
            assert!(acts_on_release(a), "{a}");
        }
        for a in ["show:Main\nCam", "hide:Main\nCam", "filter:Cam\nBlur", "mute:Mic", "go_live", "scene:Main"] {
            assert!(!acts_on_release(a), "{a}");
        }
    }

    /// The key worker's path: down shows (even inside a group), up puts back, and nothing the
    /// Studio is asked is a saved edit or an undo step.
    #[tokio::test]
    async fn push_to_show_runs_down_and_up_through_the_engine_without_edits() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;
        run(&engine, "push_show:Main\nDeep", true).await.unwrap_or_else(|e| panic!("{e}"));
        run(&engine, "push_show:Main\nDeep", false).await.unwrap_or_else(|e| panic!("{e}"));
        let acts = host.take_acts();
        assert_eq!(
            acts,
            vec![
                StudioAct::Items { scene: "Main".into() },
                StudioAct::PushItem { scene: "Main".into(), id: 9, visible: true, release: false },
                StudioAct::PushItem { scene: "Main".into(), id: 9, visible: false, release: true },
            ]
        );
        assert!(acts.iter().all(|a| !matches!(a, StudioAct::EditItem { .. }) && !a.changes()));
        // A key let go while the shortcuts were taken away: the worker's own request puts it back.
        run(&engine, "push_show:Main\nBanner", true).await.unwrap_or_else(|e| panic!("{e}"));
        host.take_acts();
        run(&engine, RELEASE_HOLDS, false).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            host.take_acts(),
            vec![StudioAct::PushItem { scene: "Main".into(), id: 4, visible: false, release: true }]
        );
        // Nothing held: asking again does nothing.
        run(&engine, RELEASE_HOLDS, false).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(host.take_acts().is_empty());
        engine.shutdown();
    }

    /// The old show/hide keys (a tap) reach an item in a group too.
    #[tokio::test]
    async fn show_and_hide_keys_reach_items_in_groups() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;
        run(&engine, "show:Main\nDeep", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            host.take_acts(),
            vec![
                StudioAct::Items { scene: "Main".into() },
                StudioAct::EditItem { scene: "Main".into(), id: 9, edit: ItemEdit::Show },
            ]
        );
        engine.shutdown();
    }

    #[tokio::test]
    async fn vertical_preview_actions_toggle_start_and_stop_through_the_engine() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;

        run(&engine, "vertical:hide_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes]);

        run(&engine, "vertical:show_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::StartVertical { scene: "Vertical".into() }]);

        run(&engine, "vertical:show_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes]);

        run(&engine, "vertical:toggle_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::StopVertical]);

        run(&engine, "vertical:toggle_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::StartVertical { scene: "Vertical".into() }]);

        run(&engine, "vertical:hide_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::StopVertical]);

        engine.shutdown();
    }

    /// While the Studio streams, the vertical preview keys would end the vertical stream (hide,
    /// toggle) or start one and make a scene (show): they do nothing then.
    #[tokio::test]
    async fn vertical_preview_keys_never_turn_the_vertical_picture_on_or_off_while_streaming() {
        use std::sync::atomic::Ordering::SeqCst;
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;
        host.streaming.store(true, SeqCst);

        // On air with the vertical picture on: hide and toggle leave it (and its stream) on.
        host.vertical_on.store(true, SeqCst);
        for key in ["vertical:hide_preview", "vertical:toggle_preview", "vertical:show_preview"] {
            run(&engine, key, true).await.unwrap_or_else(|e| panic!("{key}: {e}"));
            let acts = host.take_acts();
            assert!(
                !acts.iter().any(|a| matches!(a, StudioAct::StopVertical | StudioAct::StartVertical { .. })),
                "{key} while live: {acts:?}"
            );
            assert!(host.vertical_on.load(SeqCst), "{key} turned the vertical picture off while live");
        }
        assert_eq!(host.vstream_stops.load(SeqCst), 0, "the vertical stream was stopped while live");

        // On air with it off: show and toggle don't start it (that would start a vertical stream).
        host.vertical_on.store(false, SeqCst);
        for key in ["vertical:show_preview", "vertical:toggle_preview"] {
            run(&engine, key, true).await.unwrap_or_else(|e| panic!("{key}: {e}"));
            let acts = host.take_acts();
            assert!(!acts.iter().any(|a| matches!(a, StudioAct::StartVertical { .. })), "{key} while live: {acts:?}");
            assert!(!host.vertical_on.load(SeqCst), "{key} turned the vertical picture on while live");
        }

        // Off air the keys work again.
        host.streaming.store(false, SeqCst);
        run(&engine, "vertical:toggle_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::StartVertical { scene: "Vertical".into() }]);
        engine.shutdown();
    }

    #[tokio::test]
    async fn vertical_show_preview_picks_a_free_scene_name_if_vertical_is_taken() {
        let host = std::sync::Arc::new(FakeHost::default());
        host.taken_names.lock().unwrap_or_else(|p| p.into_inner()).push("Vertical".into());
        let (engine, _dir) = test_engine(host.clone()).await;

        run(&engine, "vertical:show_preview", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            host.take_acts(),
            vec![
                StudioAct::Scenes,
                StudioAct::StartVertical { scene: "Vertical".into() },
                StudioAct::StartVertical { scene: "Vertical 2".into() },
            ]
        );
        engine.shutdown();
    }

    #[tokio::test]
    async fn vertical_screenshot_captures_the_active_vertical_scene_when_on() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;

        run(&engine, "vertical:screenshot", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes]);

        host.vertical_on.store(true, std::sync::atomic::Ordering::SeqCst);
        *host.vertical_current.lock().unwrap_or_else(|p| p.into_inner()) = Some("Tall".into());

        run(&engine, "vertical:screenshot", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::Scenes, StudioAct::Screenshot { name: "Tall".into() }]);
        engine.shutdown();
    }

    #[tokio::test]
    async fn scene_hotkey_forwards_vertical_scene_to_set_current() {
        let host = std::sync::Arc::new(FakeHost::default());
        let (engine, _dir) = test_engine(host.clone()).await;

        run(&engine, "scene:Tall", true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(host.take_acts(), vec![StudioAct::SetCurrent { scene: "Tall".into() }]);
        engine.shutdown();
    }
}
