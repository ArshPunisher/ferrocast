//! The Studio's keyboard shortcuts (2.1): global, like OBS's hotkeys, so they work while a
//! game has the keyboard. Each is an action from the Studio's settings: go live, end the
//! stream, record, pause, a scene, mute a sound, or push to talk (held down).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use ferrocast_engine::studio_host::StudioAct;
use ferrocast_engine::{Engine, EngineError};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// The Studio's shortcuts registered now and what each does, and the queue their presses go
/// through: one worker runs them in order, so a quick push-to-talk tap never ends unmuted.
pub struct StudioKeys {
    keys: Mutex<HashMap<Shortcut, String>>,
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

/// Registers these shortcuts in place of the old ones → the keys that couldn't be (taken by
/// another app, used twice, or not keys this computer knows).
pub fn apply(app: &AppHandle, wanted: &BTreeMap<String, String>) -> Vec<String> {
    let Some(state) = app.try_state::<StudioKeys>() else { return Vec::new() };
    let mut map = state.keys.lock().unwrap_or_else(|p| p.into_inner());
    for (sc, _) in map.drain() {
        let _ = app.global_shortcut().unregister(sc);
    }
    let mut failed = Vec::new();
    for (action, keys) in wanted {
        match keys.parse::<Shortcut>() {
            Ok(sc) if !map.contains_key(&sc) => match app.global_shortcut().register(sc) {
                Ok(()) => {
                    tracing::info!(keys, action, "a Studio shortcut is set");
                    map.insert(sc, action.clone());
                }
                Err(e) => {
                    tracing::warn!(keys, error = %e, "a Studio shortcut couldn't be registered");
                    failed.push(keys.clone());
                }
            },
            _ => failed.push(keys.clone()),
        }
    }
    failed
}

/// Runs the Studio's action for this shortcut → false when it isn't one of the Studio's.
pub fn handle(app: &AppHandle, shortcut: &Shortcut, state: ShortcutState) -> bool {
    let Some(keys) = app.try_state::<StudioKeys>() else { return false };
    let Some(action) = keys.keys.lock().unwrap_or_else(|p| p.into_inner()).get(shortcut).cloned() else {
        return false;
    };
    let pressed = state == ShortcutState::Pressed;
    tracing::debug!(action, pressed, "a Studio shortcut");
    // Only push to talk acts on letting go.
    if !pressed && !action.starts_with("push_to_talk:") {
        return true;
    }
    let _ = keys.queue.send((action, pressed));
    true
}

async fn run(engine: &Engine, action: &str, pressed: bool) -> Result<(), EngineError> {
    match action {
        "go_live" => {
            engine.studio_stream(true).await?;
        }
        "end_stream" => {
            engine.studio_stream(false).await?;
        }
        "record" => {
            let on = engine.studio_stats().await?["record"]["active"].as_bool().unwrap_or(false);
            engine.studio_record(!on).await?;
        }
        "pause_record" => {
            let paused = engine.studio_stats().await?["record"]["paused"].as_bool().unwrap_or(false);
            engine.studio_record_pause(!paused).await?;
        }
        a => {
            let audio = |name: &str, muted: bool| StudioAct::SetAudio {
                name: name.to_string(),
                volume_db: None,
                muted: Some(muted),
                monitor: None,
            };
            if let Some(scene) = a.strip_prefix("scene:") {
                engine.studio_act(StudioAct::SetCurrent { scene: scene.to_string() }).await?;
            } else if let Some(name) = a.strip_prefix("mute:") {
                let muted = muted(engine, name).await?;
                engine.studio_act(audio(name, !muted)).await?;
            } else if let Some(name) = a.strip_prefix("push_to_talk:") {
                // Held: heard; let go: muted.
                engine.studio_act(audio(name, !pressed)).await?;
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
