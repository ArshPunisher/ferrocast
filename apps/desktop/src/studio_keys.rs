//! The Studio's keyboard shortcuts (2.1): global, like OBS's hotkeys, so they work while a
//! game has the keyboard. Each is an action from the Studio's settings, as OBS has them:
//! streaming, recording (pause, split, chapter), replay buffer, virtual camera, Studio Mode
//! (Transition, quick transitions, swap, fade to black), a scene, a source's show and hide, a
//! filter on or off, media controls, screenshots, undo, mute, push to talk and push to mute
//! (held down).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use ferrocast_engine::studio_host::{AudioChange, FilterEdit, ItemEdit, MediaAct, StudioAct};
use ferrocast_engine::{Engine, EngineError};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// The Studio's shortcuts registered now and what each does, and the queue their presses go
/// through: one worker runs them in order, so a quick push-to-talk tap never ends unmuted.
pub struct StudioKeys {
    keys: Mutex<HashMap<Shortcut, Vec<String>>>,
    queue: tokio::sync::mpsc::UnboundedSender<(String, bool)>,
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
        StudioKeys { keys: Mutex::new(HashMap::new()), queue }
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
    let mut map = state.keys.lock().unwrap_or_else(|p| p.into_inner());
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

/// Runs the Studio's action for this shortcut → false when it isn't one of the Studio's.
pub fn handle(app: &AppHandle, shortcut: &Shortcut, state: ShortcutState) -> bool {
    let Some(keys) = app.try_state::<StudioKeys>() else { return false };
    let Some(actions) = keys.keys.lock().unwrap_or_else(|p| p.into_inner()).get(shortcut).cloned() else {
        return false;
    };
    let pressed = state == ShortcutState::Pressed;
    for action in actions {
        tracing::debug!(action, pressed, "a Studio shortcut");
        // Only push to talk and push to mute act on letting go.
        if !pressed && !action.starts_with("push_to_talk:") && !action.starts_with("push_to_mute:") {
            continue;
        }
        let _ = keys.queue.send((action, pressed));
    }
    true
}

async fn run(engine: &Engine, action: &str, pressed: bool) -> Result<(), EngineError> {
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
                let flat: Vec<&Value> = items
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|i| std::iter::once(i).chain(i["children"].as_array().into_iter().flatten()))
                    .collect();
                let id = flat.iter().find(|i| i["name"] == Value::String(source.into())).and_then(|i| i["id"].as_i64());
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
}
