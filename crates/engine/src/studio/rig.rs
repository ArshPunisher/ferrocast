//! Where the show acts: Ferrocast's own Studio while it runs, OBS otherwise. Start show,
//! its pre-flight, BRB, `!scene` and clips go through here, so they work the same with
//! either and never talk to OBS while the Studio is the one on air.

use serde_json::Value;

use super::obs_error;
use crate::studio_host::{AudioChange, ItemEdit, StudioAct};
use crate::{Engine, EngineError};

/// The streaming app the show drives.
pub(crate) enum Rig {
    Studio,
    Obs(ferrocast_obs::Client),
}

impl Rig {
    /// "the Studio" or "OBS", for sentences.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Rig::Studio => "the Studio",
            Rig::Obs(_) => "OBS",
        }
    }

    /// "The Studio" or "OBS", to start a sentence.
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Rig::Studio => "The Studio",
            Rig::Obs(_) => "OBS",
        }
    }
}

/// Scans a Studio scene's items JSON for any visible non-group, non-scene source.
/// Visible nested scenes are pushed into `nested` for the caller to inspect with cycle detection.
fn studio_items_have_visible(items: &Value, nested: &mut Vec<String>) -> bool {
    let Some(list) = items.as_array() else { return false };
    for item in list {
        if item.get("visible").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let kind = item.get("kind").and_then(Value::as_str);
        let is_group = item.get("group").and_then(Value::as_bool) == Some(true)
            || kind == Some("group")
            || item.get("children").is_some_and(Value::is_array);
        if is_group {
            if let Some(children) = item.get("children")
                && studio_items_have_visible(children, nested)
            {
                return true;
            }
            continue;
        }
        if kind == Some("scene") {
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                nested.push(name.to_string());
            }
            continue;
        }
        return true;
    }
    false
}

/// Deep emptiness check for OBS scenes: ignores disabled items and recurses into nested scenes
/// with cycle protection. Returns `None` if `scene` isn't in `scenes`.
fn obs_scene_is_empty(scenes: &ferrocast_obs::control::Scenes, scene: &str) -> Option<bool> {
    scenes.scenes.iter().find(|s| s.name == scene)?;
    let mut visited = std::collections::HashSet::new();
    let mut todo = vec![scene];
    visited.insert(scene);
    while let Some(name) = todo.pop() {
        let Some(sc) = scenes.scenes.iter().find(|s| s.name == name) else { continue };
        for item in &sc.items {
            if !item.enabled {
                continue;
            }
            if item.kind.is_none()
                && let Some(sub) = scenes.scenes.iter().find(|s| s.name == item.source)
            {
                if visited.insert(sub.name.as_str()) {
                    todo.push(sub.name.as_str());
                }
                continue;
            }
            return Some(false);
        }
    }
    Some(true)
}

/// A source called `name` among a scene's items (groups' children too) → its id and whether
/// it shows.
fn find_item(items: &Value, name: &str) -> Option<(i64, bool)> {
    items.as_array()?.iter().find_map(|i| {
        if i.get("name").and_then(Value::as_str) == Some(name) {
            let id = i.get("id").and_then(Value::as_i64)?;
            return Some((id, i.get("visible").and_then(Value::as_bool).unwrap_or(true)));
        }
        i.get("children").and_then(|c| find_item(c, name))
    })
}

impl Engine {
    /// The Studio when it's running, else OBS. With neither, the answer points at the Studio
    /// (OBS is the fallback); a wrong OBS password still says so.
    pub(crate) async fn rig(&self) -> Result<Rig, EngineError> {
        if self.studio_status().await.running {
            return Ok(Rig::Studio);
        }
        match self.obs_client().await {
            Ok(c) => Ok(Rig::Obs(c)),
            Err(e @ EngineError::Refused { code: "obs_password", .. }) => Err(e),
            Err(_) => Err(EngineError::Refused {
                status: 409,
                code: "studio_stopped",
                message: "Studio isn't started.".into(),
                hint: "Start it in Go live, or open OBS if you use OBS.".into(),
                retry_after: None,
            }),
        }
    }

    /// The scene on air and every scene's name, in order.
    pub(crate) async fn rig_scenes(&self, rig: &Rig) -> Result<(Option<String>, Vec<String>), EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::scene_names(c).await.map_err(obs_error),
            Rig::Studio => {
                let v = self.studio_act(StudioAct::Scenes).await?;
                let current = v.get("current").and_then(Value::as_str).map(String::from);
                let names = v
                    .get("scenes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|s| s.get("name")?.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                Ok((current, names))
            }
        }
    }

    /// Whether `scene` has no visible sources at all (recursing through groups and nested scenes,
    /// ignoring hidden items and empty groups). `None` if the scene list can't be read or the
    /// scene doesn't exist, so `rig_switch` surfaces the real error.
    pub(crate) async fn rig_scene_is_empty(&self, rig: &Rig, scene: &str) -> Option<bool> {
        match rig {
            Rig::Obs(c) => {
                let scenes = ferrocast_obs::control::scenes(c).await.ok()?;
                obs_scene_is_empty(&scenes, scene)
            }
            Rig::Studio => {
                let root = self.studio_act(StudioAct::Items { scene: scene.into() }).await.ok()?;
                let mut visited = std::collections::HashSet::new();
                visited.insert(scene.to_string());
                let mut nested = Vec::new();
                if studio_items_have_visible(&root, &mut nested) {
                    return Some(false);
                }
                while let Some(sub) = nested.pop() {
                    if !visited.insert(sub.clone()) {
                        continue;
                    }
                    let Ok(items) = self.studio_act(StudioAct::Items { scene: sub }).await else {
                        continue;
                    };
                    if studio_items_have_visible(&items, &mut nested) {
                        return Some(false);
                    }
                }
                Some(true)
            }
        }
    }

    pub(crate) async fn rig_switch(&self, rig: &Rig, scene: &str) -> Result<(), EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::switch_scene(c, scene).await.map_err(obs_error),
            Rig::Studio => self.studio_act(StudioAct::SetCurrent { scene: scene.into() }).await.map(drop),
        }
    }

    /// Whether a sound is muted. In the Studio a sound lives in scenes' mixers (the global
    /// devices in all of them), so each scene is looked through until it turns up.
    pub(crate) async fn rig_muted(&self, rig: &Rig, name: &str) -> Result<bool, EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::muted(c, name).await.map_err(obs_error),
            Rig::Studio => {
                let (current, scenes) = self.rig_scenes(rig).await?;
                let order =
                    current.clone().into_iter().chain(scenes.into_iter().filter(|s| Some(s) != current.as_ref()));
                for scene in order {
                    let Ok(mixer) = self.studio_act(StudioAct::Audio { scene }).await else { continue };
                    let found = mixer
                        .as_array()
                        .and_then(|a| a.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name)));
                    if let Some(s) = found {
                        return Ok(s.get("muted").and_then(Value::as_bool).unwrap_or(false));
                    }
                }
                Err(no_source(name))
            }
        }
    }

    pub(crate) async fn rig_set_muted(&self, rig: &Rig, name: &str, muted: bool) -> Result<(), EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::set_muted(c, name, muted).await.map_err(obs_error),
            Rig::Studio => self
                .studio_act(StudioAct::SetAudio {
                    name: name.into(),
                    change: AudioChange { muted: Some(muted), ..Default::default() },
                })
                .await
                .map(drop),
        }
    }

    /// Whether a source shows in a scene: None when it isn't in it.
    /// The loudest each of these sounds got over about a second and a half (dBFS), sampled a
    /// few times: a mic that's on but hears nothing stays near silence. The Studio only (None
    /// with OBS).
    pub(crate) async fn rig_loudest(
        &self,
        rig: &Rig,
        names: &[String],
    ) -> Option<std::collections::HashMap<String, f32>> {
        if !matches!(rig, Rig::Studio) || names.is_empty() {
            return None;
        }
        let mut loudest: std::collections::HashMap<String, f32> = Default::default();
        for i in 0..6 {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let Ok(v) = self.studio_act(StudioAct::Levels { names: names.to_vec() }).await else { return None };
            for n in names {
                let peak =
                    v[n]["peak"].as_array().into_iter().flatten().filter_map(Value::as_f64).fold(f64::MIN, f64::max);
                if peak > f64::MIN {
                    let e = loudest.entry(n.clone()).or_insert(f32::MIN);
                    *e = e.max(peak as f32);
                }
            }
        }
        Some(loudest)
    }

    /// How these sources' pictures look now (`StudioAct::Look`). The Studio only.
    pub(crate) async fn rig_look(&self, rig: &Rig, names: &[String]) -> Option<Value> {
        if !matches!(rig, Rig::Studio) || names.is_empty() {
            return None;
        }
        self.studio_act(StudioAct::Look { names: names.to_vec() }).await.ok()
    }

    pub(crate) async fn rig_visible(&self, rig: &Rig, scene: &str, source: &str) -> Result<Option<bool>, EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::source_visible(c, scene, source).await.map_err(obs_error),
            Rig::Studio => {
                let items = self.studio_act(StudioAct::Items { scene: scene.into() }).await?;
                Ok(find_item(&items, source).map(|(_, shown)| shown))
            }
        }
    }

    /// Shows or hides a source in a scene. 400 when it isn't in it.
    pub(crate) async fn rig_set_visible(
        &self,
        rig: &Rig,
        scene: &str,
        source: &str,
        shown: bool,
    ) -> Result<(), EngineError> {
        let not_in = || EngineError::Invalid(format!("\"{source}\" isn't in \"{scene}\"."));
        match rig {
            Rig::Obs(c) => {
                let id =
                    ferrocast_obs::control::item_id(c, scene, source).await.map_err(obs_error)?.ok_or_else(not_in)?;
                ferrocast_obs::control::set_visible(c, scene, id, shown).await.map_err(obs_error)
            }
            Rig::Studio => {
                let items = self.studio_act(StudioAct::Items { scene: scene.into() }).await?;
                let (id, _) = find_item(&items, source).ok_or_else(not_in)?;
                let edit = if shown { ItemEdit::Show } else { ItemEdit::Hide };
                self.studio_act(StudioAct::EditItem { scene: scene.into(), id, edit }).await.map(drop)
            }
        }
    }

    pub(crate) async fn rig_streaming(&self, rig: &Rig) -> Result<bool, EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::streaming(c).await.map_err(obs_error),
            Rig::Studio => {
                Ok(self.studio_stream_stats().await?.get("active").and_then(Value::as_bool).unwrap_or(false))
            }
        }
    }

    pub(crate) async fn rig_set_streaming(&self, rig: &Rig, on: bool) -> Result<(), EngineError> {
        let _out = self.studio().output_lock.lock().await;
        self.rig_set_streaming_locked(rig, on).await
    }

    /// Runs Start/Stop Streaming on `rig` while the caller already holds `studio().output_lock`.
    pub(crate) async fn rig_set_streaming_locked(&self, rig: &Rig, on: bool) -> Result<(), EngineError> {
        match rig {
            Rig::Obs(c) if on => {
                ferrocast_obs::control::start_stream(c).await.map_err(obs_error)?;
                self.on_stream_started();
                Ok(())
            }
            Rig::Obs(c) => ferrocast_obs::control::stop_stream(c).await.map_err(obs_error),
            Rig::Studio => self.studio_stream_locked(on).await.map(drop),
        }
    }

    pub(crate) async fn rig_start_record(&self, rig: &Rig) -> Result<(), EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::start_record(c).await.map_err(obs_error),
            Rig::Studio => self.studio_record(true).await.map(drop),
        }
    }

    pub(crate) async fn rig_replay_active(&self, rig: &Rig) -> Result<bool, EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::replay_buffer_active(c).await.map_err(obs_error),
            Rig::Studio => {
                Ok(self.studio_stats().await?.pointer("/replay/active").and_then(Value::as_bool).unwrap_or(false))
            }
        }
    }

    pub(crate) async fn rig_start_replay(&self, rig: &Rig) -> Result<(), EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::start_replay_buffer(c).await.map_err(obs_error),
            Rig::Studio => self.studio_replay(true).await.map(drop),
        }
    }

    /// Saves the last seconds of the replay buffer → the file, when it's known already (the
    /// Studio writes it in the background).
    pub(crate) async fn rig_save_replay(&self, rig: &Rig) -> Result<Option<String>, EngineError> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::save_replay(c).await.map_err(obs_error),
            Rig::Studio => {
                // The Studio writes the file in the background: its name shows in the stats
                // once it's done, and a vertical copy (when asked for) is made from it then.
                let before = self.studio_last_replay().await;
                self.studio_replay_save().await?;
                let cfg = self.scenes_config();
                if cfg.vertical_clips || cfg.clip_captions {
                    let engine = self.clone();
                    tokio::spawn(async move {
                        for _ in 0..40 {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let now = engine.studio_last_replay().await;
                            if now.is_some() && now != before {
                                engine.vertical_copy(now.as_deref());
                                return;
                            }
                        }
                        tracing::info!("no vertical clip: the replay's file didn't show up");
                    });
                }
                Ok(None)
            }
        }
    }

    /// The Studio's last saved replay file, from its stats.
    async fn studio_last_replay(&self) -> Option<String> {
        self.studio_stats().await.ok()?.get("replay")?.get("last")?.as_str().map(str::to_string)
    }

    /// Free space where recordings go, in MB (None when it can't be read).
    pub(crate) async fn rig_disk_mb(&self, rig: &Rig) -> Option<f64> {
        match rig {
            Rig::Obs(c) => ferrocast_obs::control::health_sample(c, crate::now().0).await.ok().map(|s| s.disk_mb),
            Rig::Studio => self.studio_stats().await.ok()?.get("disk_free_mb")?.as_f64(),
        }
    }
}

fn no_source(name: &str) -> EngineError {
    EngineError::Refused {
        status: 422,
        code: "studio_no_source",
        message: format!("The Studio has no sound called \"{name}\"."),
        hint: "Pick it again in Studio, Show.".into(),
        retry_after: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_found_in_groups_too() {
        let items = serde_json::json!([
            { "id": 1, "name": "Game", "visible": true },
            { "id": 2, "name": "Cams", "visible": true, "children": [
                { "id": 5, "name": "Camera", "visible": false }
            ] }
        ]);
        assert_eq!(find_item(&items, "Game"), Some((1, true)));
        assert_eq!(find_item(&items, "Camera"), Some((5, false)));
        assert_eq!(find_item(&items, "Mic"), None);
        assert_eq!(find_item(&Value::Null, "Game"), None);
    }

    #[test]
    fn studio_deep_empty_check_handles_hidden_items_groups_and_nested_scenes() {
        let mut nested = Vec::new();
        assert!(!studio_items_have_visible(&serde_json::json!([]), &mut nested));
        assert!(!studio_items_have_visible(
            &serde_json::json!([{ "id": 1, "name": "Cam", "kind": "av_capture_input", "visible": false }]),
            &mut nested,
        ));
        assert!(!studio_items_have_visible(
            &serde_json::json!([{ "id": 2, "name": "EmptyGrp", "kind": "group", "group": true, "visible": true, "children": [] }]),
            &mut nested,
        ));
        assert!(!studio_items_have_visible(
            &serde_json::json!([{
                "id": 3, "name": "AllHidden", "kind": "group", "group": true, "visible": true,
                "children": [{ "id": 4, "name": "Cam", "kind": "av_capture_input", "visible": false }]
            }]),
            &mut nested,
        ));
        assert!(!studio_items_have_visible(
            &serde_json::json!([{
                "id": 5, "name": "HiddenGrp", "kind": "group", "group": true, "visible": false,
                "children": [{ "id": 6, "name": "Cam", "kind": "av_capture_input", "visible": true }]
            }]),
            &mut nested,
        ));
        assert!(studio_items_have_visible(
            &serde_json::json!([{
                "id": 7, "name": "Outer", "kind": "group", "group": true, "visible": true,
                "children": [{
                    "id": 8, "name": "Inner", "kind": "group", "group": true, "visible": true,
                    "children": [{ "id": 9, "name": "Cam", "kind": "av_capture_input", "visible": true }]
                }]
            }]),
            &mut nested,
        ));
        // Visible nested scene is queued; hidden nested scene is ignored.
        nested.clear();
        assert!(!studio_items_have_visible(
            &serde_json::json!([
                { "id": 10, "name": "SubHidden", "kind": "scene", "visible": false },
                { "id": 11, "name": "SubVisible", "kind": "scene", "visible": true }
            ]),
            &mut nested,
        ));
        assert_eq!(nested, vec!["SubVisible"]);
    }

    #[test]
    fn obs_deep_empty_check_handles_disabled_items_and_cycles() {
        use ferrocast_obs::control::{Scene, SceneItem, Scenes};
        let scenes = Scenes {
            current: Some("A".into()),
            scenes: vec![
                Scene { name: "Empty".into(), items: vec![] },
                Scene {
                    name: "Hidden".into(),
                    items: vec![SceneItem { id: 1, source: "Cam".into(), enabled: false, kind: Some("dshow".into()) }],
                },
                Scene {
                    name: "CycleA".into(),
                    items: vec![SceneItem { id: 1, source: "CycleB".into(), enabled: true, kind: None }],
                },
                Scene {
                    name: "CycleB".into(),
                    items: vec![SceneItem { id: 1, source: "CycleA".into(), enabled: true, kind: None }],
                },
                Scene {
                    name: "ViaNested".into(),
                    items: vec![SceneItem { id: 1, source: "Leaf".into(), enabled: true, kind: None }],
                },
                Scene {
                    name: "Leaf".into(),
                    items: vec![SceneItem { id: 1, source: "Cam".into(), enabled: true, kind: Some("dshow".into()) }],
                },
            ],
        };
        assert_eq!(obs_scene_is_empty(&scenes, "Missing"), None);
        assert_eq!(obs_scene_is_empty(&scenes, "Empty"), Some(true));
        assert_eq!(obs_scene_is_empty(&scenes, "Hidden"), Some(true));
        assert_eq!(obs_scene_is_empty(&scenes, "CycleA"), Some(true));
        assert_eq!(obs_scene_is_empty(&scenes, "ViaNested"), Some(false));
    }
}
