//! OBS control over obs-websocket v5 (built into OBS 28+).
//!
//! The one job that matters most is "Add to OBS": put an overlay into the streamer's
//! current scene as a Browser Source in one click, or fix the URL of one that's already
//! there, so nobody has to copy a URL into a dialog (VISION: few clicks).

pub mod detect;

use std::time::Duration;

use obws::Client;
use obws::requests::inputs::{Create, InputId, SetSettings};
use obws::requests::scene_items::{CreateSceneItem, Id};
use obws::requests::scenes::SceneId;
use obws::requests::sources::SourceId;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Where OBS listens. The password, if any, is looked up from the keychain by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig { host: "127.0.0.1".into(), port: 4455 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObsError {
    #[error("OBS isn't running, or its WebSocket server is off (Tools → WebSocket Server Settings).")]
    NotReachable,
    #[error("OBS refused the password. Copy it from Tools → WebSocket Server Settings → Show Connect Info.")]
    WrongPassword,
    #[error("OBS answered with an error: {0}")]
    Refused(String),
}

fn map_err(e: obws::error::Error) -> ObsError {
    let text = e.to_string();
    let lower = text.to_lowercase();
    if lower.contains("authentication") || lower.contains("4009") {
        ObsError::WrongPassword
    } else if lower.contains("connect")
        || lower.contains("timeout")
        || lower.contains("closed")
        || lower.contains("handshake")
    {
        ObsError::NotReachable
    } else {
        ObsError::Refused(text)
    }
}

/// What "Add to OBS" did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Added {
    pub scene: String,
    pub source: String,
    /// `false` when a source with this name existed and only its URL and size changed.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObsStatus {
    pub obs_version: String,
    pub websocket_version: String,
    pub scene: String,
}

/// Connects (with a short timeout so the dashboard never hangs on a closed OBS).
pub async fn connect(cfg: &ObsConfig, password: Option<&str>) -> Result<Client, ObsError> {
    let fut = Client::connect(cfg.host.as_str(), cfg.port, password);
    match tokio::time::timeout(Duration::from_secs(4), fut).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(map_err(e)),
        Err(_) => Err(ObsError::NotReachable),
    }
}

pub async fn status(client: &Client) -> Result<ObsStatus, ObsError> {
    let v = client.general().version().await.map_err(map_err)?;
    let scene = client.scenes().current_program_scene().await.map_err(map_err)?;
    Ok(ObsStatus {
        obs_version: v.obs_studio_version.to_string(),
        websocket_version: v.obs_web_socket_version.to_string(),
        scene: scene.id.name,
    })
}

/// The browser-source name for an overlay. Kept stable so adding twice updates in place.
pub fn source_name(overlay_name: &str) -> String {
    let clean: String = overlay_name.chars().filter(|c| !c.is_control()).take(60).collect();
    format!("Ferrocast: {}", clean.trim())
}

/// Adds the overlay to the current scene, or updates the existing source and makes sure
/// it's in the current scene. Idempotent.
pub async fn add_browser_source(
    client: &Client,
    name: &str,
    url: &str,
    width: u32,
    height: u32,
) -> Result<Added, ObsError> {
    let scene = client.scenes().current_program_scene().await.map_err(map_err)?.id;
    let scene_id = SceneId::Uuid(scene.uuid);
    // Keep the source running when hidden so a queued alert isn't lost on a scene switch;
    // audio stays with OBS's defaults.
    let settings =
        json!({ "url": url, "width": width, "height": height, "shutdown": false, "restart_when_active": false });
    let existing =
        client.inputs().list(Some("browser_source")).await.map_err(map_err)?.into_iter().find(|i| i.id.name == name);
    match existing {
        Some(input) => {
            client
                .inputs()
                .set_settings(SetSettings {
                    input: InputId::Uuid(input.id.uuid),
                    settings: &settings,
                    overlay: Some(true),
                })
                .await
                .map_err(map_err)?;
            let in_scene =
                client.scene_items().id(Id { scene: scene_id, source: name, search_offset: None }).await.is_ok();
            if !in_scene {
                client
                    .scene_items()
                    .create(CreateSceneItem {
                        scene: scene_id,
                        source: SourceId::Uuid(input.id.uuid),
                        enabled: Some(true),
                    })
                    .await
                    .map_err(map_err)?;
            }
            Ok(Added { scene: scene.name, source: name.to_string(), created: false })
        }
        None => {
            client
                .inputs()
                .create(Create {
                    scene: scene_id,
                    input: name,
                    kind: "browser_source",
                    settings: Some(&settings),
                    enabled: Some(true),
                })
                .await
                .map_err(map_err)?;
            Ok(Added { scene: scene.name, source: name.to_string(), created: true })
        }
    }
}

#[cfg(test)]
mod tests;
