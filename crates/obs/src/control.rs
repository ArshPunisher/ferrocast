//! Production helpers over obs-websocket v5: scenes, source visibility, mic mutes, the
//! replay buffer ("clip that") and health numbers (GetStats + the stream output). Each
//! function is one or two requests and turns OBS's answers into plain errors.

use obws::Client;
use obws::requests::inputs::InputId;
use obws::requests::scene_items::SetEnabled;
use obws::requests::scenes::SceneId;
use obws::responses::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ObsError;

pub(crate) fn err(e: obws::error::Error) -> ObsError {
    crate::map_err(e)
}

/// OBS's own status code for a failed request (obs-websocket `RequestStatus`).
fn code(e: &obws::error::Error) -> Option<StatusCode> {
    match e {
        obws::error::Error::Api { code, .. } => Some(*code),
        _ => None,
    }
}

/// "not found" from OBS, for a scene, source or input name.
fn not_found(e: obws::error::Error, what: &str, name: &str) -> ObsError {
    if code(&e) == Some(StatusCode::ResourceNotFound) {
        ObsError::Refused(format!("OBS has no {what} called \"{}\".", name.chars().take(80).collect::<String>()))
    } else {
        err(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneItem {
    pub id: i64,
    pub source: String,
    pub enabled: bool,
    /// `wasapi_input_capture`, `dshow_input`, `browser_source`… (`None` for nested scenes).
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scene {
    pub name: String,
    pub items: Vec<SceneItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scenes {
    pub current: Option<String>,
    pub scenes: Vec<Scene>,
}

/// Every scene (in OBS's order, top first) with its sources and whether each is shown.
pub async fn scenes(client: &Client) -> Result<Scenes, ObsError> {
    let list = client.scenes().list().await.map_err(err)?;
    let current = list.current_program_scene.map(|c| c.name);
    let mut out = Vec::new();
    // OBS lists scenes bottom-up; the dock shows them top-down.
    let mut ordered = list.scenes;
    ordered.sort_by_key(|a| std::cmp::Reverse(a.index));
    for s in ordered {
        let items = client.scene_items().list(SceneId::Name(&s.id.name)).await.map_err(err)?;
        let mut its = Vec::new();
        for i in items {
            let enabled = client.scene_items().enabled(SceneId::Name(&s.id.name), i.id).await.map_err(err)?;
            its.push(SceneItem { id: i.id, source: i.source_name, enabled, kind: i.input_kind });
        }
        out.push(Scene { name: s.id.name, items: its });
    }
    Ok(Scenes { current, scenes: out })
}

/// Scene names only (cheaper than [`scenes`]).
pub async fn scene_names(client: &Client) -> Result<(Option<String>, Vec<String>), ObsError> {
    let list = client.scenes().list().await.map_err(err)?;
    let mut s = list.scenes;
    s.sort_by_key(|a| std::cmp::Reverse(a.index));
    Ok((list.current_program_scene.map(|c| c.name), s.into_iter().map(|s| s.id.name).collect()))
}

/// Switches the program (live) scene.
pub async fn switch_scene(client: &Client, name: &str) -> Result<(), ObsError> {
    client.scenes().set_current_program_scene(SceneId::Name(name)).await.map_err(|e| not_found(e, "scene", name))
}

/// Shows or hides a source in a scene, by item id.
pub async fn set_visible(client: &Client, scene: &str, item_id: i64, visible: bool) -> Result<(), ObsError> {
    client
        .scene_items()
        .set_enabled(SetEnabled { scene: SceneId::Name(scene), item_id, enabled: visible })
        .await
        .map_err(|e| not_found(e, "scene", scene))
}

/// The item id of `source` in `scene`.
pub async fn item_id(client: &Client, scene: &str, source: &str) -> Result<Option<i64>, ObsError> {
    let items = client.scene_items().list(SceneId::Name(scene)).await.map_err(|e| not_found(e, "scene", scene))?;
    Ok(items.into_iter().find(|i| i.source_name == source).map(|i| i.id))
}

/// Whether a source is shown in a scene (`None` when it isn't in that scene).
pub async fn source_visible(client: &Client, scene: &str, source: &str) -> Result<Option<bool>, ObsError> {
    match item_id(client, scene, source).await? {
        Some(id) => Ok(Some(client.scene_items().enabled(SceneId::Name(scene), id).await.map_err(err)?)),
        None => Ok(None),
    }
}

/// Every input's name and kind (for the pre-flight's mic and camera pickers).
pub async fn inputs(client: &Client) -> Result<Vec<(String, String)>, ObsError> {
    Ok(client.inputs().list(None).await.map_err(err)?.into_iter().map(|i| (i.id.name, i.unversioned_kind)).collect())
}

pub async fn muted(client: &Client, input: &str) -> Result<bool, ObsError> {
    client.inputs().muted(InputId::Name(input)).await.map_err(|e| not_found(e, "audio source", input))
}

pub async fn set_muted(client: &Client, input: &str, muted: bool) -> Result<(), ObsError> {
    client.inputs().set_muted(InputId::Name(input), muted).await.map_err(|e| not_found(e, "audio source", input))
}

/// Whether the replay buffer is running.
pub async fn replay_buffer_active(client: &Client) -> Result<bool, ObsError> {
    client.replay_buffer().status().await.map_err(err)
}

pub async fn start_replay_buffer(client: &Client) -> Result<(), ObsError> {
    match client.replay_buffer().start().await {
        Ok(()) => Ok(()),
        // Already running is what the caller wanted.
        Err(e) if code(&e) == Some(StatusCode::OutputRunning) => Ok(()),
        Err(e) if matches!(code(&e), Some(StatusCode::InvalidResourceState | StatusCode::ResourceNotConfigurable)) => {
            Err(ObsError::Refused(
                "OBS's replay buffer is switched off. In OBS open Settings, Output, Replay Buffer and tick Enable \
                 Replay Buffer."
                    .into(),
            ))
        }
        Err(e) => Err(err(e)),
    }
}

/// Saves the replay buffer ("clip that"). Returns the saved file's path when OBS says.
pub async fn save_replay(client: &Client) -> Result<Option<String>, ObsError> {
    if let Err(e) = client.replay_buffer().save().await {
        return Err(if code(&e) == Some(StatusCode::OutputNotRunning) {
            ObsError::Refused(
                "OBS's replay buffer isn't running. Start it in OBS (Start Replay Buffer under Controls), or turn on \
                 \"Start the replay buffer\" in Studio's show checklist."
                    .into(),
            )
        } else {
            err(e)
        });
    }
    Ok(client.replay_buffer().last_replay().await.ok().filter(|p| !p.is_empty()))
}

/// Whether OBS is streaming right now.
pub async fn streaming(client: &Client) -> Result<bool, ObsError> {
    client.streaming().status().await.map(|s| s.active).map_err(err)
}

/// Presses OBS's Start Streaming. Already live is what the caller wanted.
pub async fn start_stream(client: &Client) -> Result<(), ObsError> {
    match client.streaming().start().await {
        Ok(()) => Ok(()),
        Err(e) if code(&e) == Some(StatusCode::OutputRunning) => Ok(()),
        Err(e) if matches!(code(&e), Some(StatusCode::InvalidResourceState | StatusCode::ResourceNotConfigurable)) => {
            Err(ObsError::Refused(
                "OBS has nowhere to stream to. In OBS open Settings, Stream, pick your platform and add the \
                 stream key, then try again."
                    .into(),
            ))
        }
        Err(e) => Err(err(e)),
    }
}

/// Presses OBS's Stop Streaming. Already stopped is what the caller wanted.
pub async fn stop_stream(client: &Client) -> Result<(), ObsError> {
    match client.streaming().stop().await {
        Ok(()) => Ok(()),
        Err(e) if code(&e) == Some(StatusCode::OutputNotRunning) => Ok(()),
        Err(e) => Err(err(e)),
    }
}

/// OBS's stream setting (Settings › Stream): the service type (`rtmp_common` for a listed
/// platform, `rtmp_custom` for a typed server) and its settings, which hold the stream key.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamService {
    pub kind: String,
    pub settings: Value,
}

impl std::fmt::Debug for StreamService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The settings carry the key: only the type and the server show.
        f.debug_struct("StreamService")
            .field("kind", &self.kind)
            .field("server", &self.settings.get("server"))
            .field("service", &self.settings.get("service"))
            .finish_non_exhaustive()
    }
}

impl StreamService {
    /// A typed server and key (OBS's "Custom..." service).
    pub fn custom(server: &str, key: &str) -> Self {
        Self {
            kind: "rtmp_custom".into(),
            settings: json!({ "server": server, "key": key, "use_auth": false, "bwtest": false }),
        }
    }

    /// Whether OBS already streams to `server`.
    pub fn points_at(&self, server: &str) -> bool {
        self.kind == "rtmp_custom" && self.settings.get("server").and_then(Value::as_str) == Some(server)
    }
}

/// OBS's current stream setting.
pub async fn stream_service(client: &Client) -> Result<StreamService, ObsError> {
    let s = client.config().stream_service_settings::<Value>().await.map_err(err)?;
    Ok(StreamService { kind: s.r#type, settings: s.settings })
}

/// Changes OBS's stream setting. OBS refuses while it streams, so this only happens between
/// streams. A different type replaces the old settings; the same type is merged over them.
pub async fn set_stream_service(client: &Client, s: &StreamService) -> Result<(), ObsError> {
    match client.config().set_stream_service_settings(&s.kind, &s.settings).await {
        Ok(()) => Ok(()),
        Err(e) if code(&e) == Some(StatusCode::OutputRunning) => Err(ObsError::Refused(
            "OBS is streaming right now. Stop the stream in OBS first; Ferrocast changes this between streams.".into(),
        )),
        Err(e) => Err(err(e)),
    }
}

/// Raw numbers for [`ferrocast_core::show::health`].
pub async fn health_sample(client: &Client, at: i64) -> Result<ferrocast_core::show::HealthSample, ObsError> {
    let s = client.general().stats().await.map_err(err)?;
    let stream = client.streaming().status().await.ok();
    Ok(ferrocast_core::show::HealthSample {
        at,
        cpu: s.cpu_usage,
        memory_mb: s.memory_usage,
        disk_mb: s.available_disk_space,
        fps: s.active_fps,
        render_ms: s.average_frame_render_time,
        render_skipped: u64::from(s.render_skipped_frames),
        render_total: u64::from(s.render_total_frames),
        output_skipped: u64::from(s.output_skipped_frames),
        output_total: u64::from(s.output_total_frames),
        streaming: stream.as_ref().is_some_and(|x| x.active),
        reconnecting: stream.as_ref().is_some_and(|x| x.reconnecting),
        stream_bytes: stream.as_ref().map_or(0, |x| x.bytes),
        stream_skipped: stream.as_ref().map_or(0, |x| u64::from(x.skipped_frames)),
        stream_total: stream.as_ref().map_or(0, |x| u64::from(x.total_frames)),
        congestion: stream.as_ref().map_or(0.0, |x| f64::from(x.congestion)),
    })
}

#[cfg(test)]
mod tests {
    use ferrocast_testkit::obs::{FakeObs, start};

    use super::*;
    use crate::{ObsConfig, connect};

    async fn client(st: &ferrocast_testkit::obs::St) -> Client {
        let port = start(st.clone()).await;
        connect(&ObsConfig { host: "127.0.0.1".into(), port }, None).await.unwrap()
    }

    #[tokio::test]
    async fn scenes_switching_and_sources() {
        let st = FakeObs::studio();
        let c = client(&st).await;
        let all = scenes(&c).await.unwrap();
        assert_eq!(all.current.as_deref(), Some("Starting Soon"));
        assert_eq!(
            all.scenes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["Starting Soon", "Gameplay", "BRB", "Ending"]
        );
        let gameplay = &all.scenes[1];
        let cam = gameplay.items.iter().find(|i| i.source == "Webcam").unwrap();
        assert!(cam.enabled);
        switch_scene(&c, "Gameplay").await.unwrap();
        assert_eq!(st.lock().unwrap().current, "Gameplay");
        let e = switch_scene(&c, "Nope").await.unwrap_err();
        assert_eq!(e, ObsError::Refused("OBS has no scene called \"Nope\".".into()));
        set_visible(&c, "Gameplay", cam.id, false).await.unwrap();
        assert_eq!(source_visible(&c, "Gameplay", "Webcam").await.unwrap(), Some(false));
        assert_eq!(source_visible(&c, "Gameplay", "Nothing").await.unwrap(), None);
        let (cur, names) = scene_names(&c).await.unwrap();
        assert_eq!((cur.as_deref(), names.len()), (Some("Gameplay"), 4));
    }

    #[tokio::test]
    async fn mic_mutes_and_inputs() {
        let st = FakeObs::studio();
        let c = client(&st).await;
        assert!(!muted(&c, "Mic").await.unwrap());
        set_muted(&c, "Mic", true).await.unwrap();
        assert!(muted(&c, "Mic").await.unwrap());
        assert!(matches!(muted(&c, "Ghost").await, Err(ObsError::Refused(m)) if m.contains("audio source")));
        let ins = inputs(&c).await.unwrap();
        assert!(ins.iter().any(|(n, k)| n == "Mic" && k == "wasapi_input_capture"));
    }

    #[tokio::test]
    async fn the_replay_buffer_saves_or_says_why_not() {
        let st = FakeObs::studio();
        let c = client(&st).await;
        assert!(!replay_buffer_active(&c).await.unwrap());
        let e = save_replay(&c).await.unwrap_err();
        assert!(matches!(&e, ObsError::Refused(m) if m.contains("isn't running")), "{e:?}");
        start_replay_buffer(&c).await.unwrap();
        start_replay_buffer(&c).await.unwrap();
        assert!(replay_buffer_active(&c).await.unwrap());
        let path = save_replay(&c).await.unwrap();
        assert_eq!(path.as_deref(), Some("/videos/Replay 1.mkv"));
        assert_eq!(st.lock().unwrap().replay_saves, 1);
        st.lock().unwrap().replay_enabled = false;
        st.lock().unwrap().replay_active = false;
        let off = start_replay_buffer(&c).await.unwrap_err();
        assert!(matches!(&off, ObsError::Refused(m) if m.contains("Enable Replay Buffer")), "{off:?}");
    }

    #[tokio::test]
    async fn health_numbers_come_from_get_stats_and_the_stream() {
        let st = FakeObs::studio();
        {
            let mut s = st.lock().unwrap();
            s.streaming = true;
            s.stream_bytes = 1_000_000;
            s.stream_skipped = 5;
            s.stream_total = 100;
            s.cpu = 12.5;
        }
        let c = client(&st).await;
        let h = health_sample(&c, 1_000).await.unwrap();
        assert!(h.streaming);
        assert_eq!((h.stream_bytes, h.stream_skipped, h.stream_total), (1_000_000, 5, 100));
        assert_eq!(h.cpu, 12.5);
        assert!(h.disk_mb > 0.0);
    }

    #[tokio::test]
    async fn start_and_stop_streaming_are_idempotent_and_explain_a_missing_key() {
        let st = FakeObs::studio();
        let c = client(&st).await;
        assert!(!streaming(&c).await.unwrap());
        start_stream(&c).await.unwrap();
        assert!(streaming(&c).await.unwrap());
        start_stream(&c).await.unwrap();
        stop_stream(&c).await.unwrap();
        assert!(!streaming(&c).await.unwrap());
        stop_stream(&c).await.unwrap();
        st.lock().unwrap().stream_unconfigured = true;
        let e = start_stream(&c).await.unwrap_err().to_string();
        assert!(e.contains("stream key"), "{e}");
    }

    #[tokio::test]
    async fn stream_service_points_at_ferrocast_and_goes_back_exactly() {
        let st = FakeObs::studio();
        let c = client(&st).await;
        let before = stream_service(&c).await.unwrap();
        assert_eq!(before.kind, "rtmp_common");
        assert!(!format!("{before:?}").contains("OldKey"), "the key never shows in Debug");
        let ours = StreamService::custom("rtmp://127.0.0.1:1935/live", "fc_k");
        set_stream_service(&c, &ours).await.unwrap();
        let now = stream_service(&c).await.unwrap();
        assert!(now.points_at("rtmp://127.0.0.1:1935/live"));
        assert!(!before.points_at("rtmp://127.0.0.1:1935/live"));
        // Put back: a different type replaces, so OBS has exactly what it had.
        set_stream_service(&c, &before).await.unwrap();
        assert_eq!(stream_service(&c).await.unwrap(), before);
        // Never while OBS streams.
        st.lock().unwrap().streaming = true;
        let e = set_stream_service(&c, &ours).await.unwrap_err();
        assert!(matches!(e, ObsError::Refused(m) if m.contains("Stop the stream in OBS first")));
        assert_eq!(stream_service(&c).await.unwrap(), before);
    }
}
