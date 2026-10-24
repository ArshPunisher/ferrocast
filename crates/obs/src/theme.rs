//! Putting a whole theme into OBS (the 24 Oct "theme to OBS" board): scenes made by name, a
//! browser source kept on top of a scene, the streamer's own scene nested under it, and the
//! old StreamElements / Streamlabs widgets found by their address. Every call only adds,
//! hides or removes what it's told to, so the engine can undo exactly what it did.

use obws::Client;
use obws::requests::inputs::{Create, InputId, SetSettings};
use obws::requests::scene_items::{CreateSceneItem, SetIndex};
use obws::requests::scenes::SceneId;
use obws::requests::sources::SourceId;
use serde::Serialize;
use serde_json::{Value, json};

use crate::ObsError;
use crate::control::err;

/// Input kinds that capture a game, a window or a screen (Windows, macOS and Linux).
pub const CAPTURE_KINDS: [&str; 8] = [
    "game_capture",
    "window_capture",
    "monitor_capture",
    "screen_capture",
    "display_capture",
    "xcomposite_input",
    "pipewire-screen-capture-source",
    "pipewire-window-capture-source",
];

/// Input kinds for a camera (Windows, macOS old and new, Linux).
pub const CAMERA_KINDS: [&str; 6] =
    ["dshow_input", "av_capture_input", "av_capture_input_v2", "macos-avcapture", "macos-avcapture-fast", "v4l2_input"];

/// Makes `name` unless OBS has it. `true` when it was made now.
pub async fn ensure_scene(client: &Client, name: &str) -> Result<bool, ObsError> {
    let list = client.scenes().list().await.map_err(err)?;
    if list.scenes.iter().any(|s| s.id.name == name) {
        return Ok(false);
    }
    client.scenes().create(name).await.map_err(err)?;
    Ok(true)
}

/// Removes a scene; one that's gone already counts as removed.
pub async fn remove_scene(client: &Client, name: &str) -> Result<(), ObsError> {
    gone_is_fine(client.scenes().remove(SceneId::Name(name)).await)
}

/// Renames a scene; `false` when it isn't there or the new name is taken.
pub async fn rename_scene(client: &Client, from: &str, to: &str) -> Result<bool, ObsError> {
    let list = client.scenes().list().await.map_err(err)?;
    if !list.scenes.iter().any(|s| s.id.name == from) || list.scenes.iter().any(|s| s.id.name == to) {
        return Ok(false);
    }
    client.scenes().set_name(SceneId::Name(from), to).await.map_err(err)?;
    Ok(true)
}

/// Renames an input; `false` when it isn't there or the new name is taken.
pub async fn rename_input(client: &Client, from: &str, to: &str) -> Result<bool, ObsError> {
    let inputs = client.inputs().list(None).await.map_err(err)?;
    if !inputs.iter().any(|i| i.id.name == from) || inputs.iter().any(|i| i.id.name == to) {
        return Ok(false);
    }
    client.inputs().set_name(InputId::Name(from), to).await.map_err(err)?;
    Ok(true)
}

/// Shows a source again; one that's gone (or its scene) counts as done.
pub async fn show_item(client: &Client, scene: &str, item_id: i64) -> Result<(), ObsError> {
    use obws::requests::scene_items::SetEnabled;
    gone_is_fine(
        client.scene_items().set_enabled(SetEnabled { scene: SceneId::Name(scene), item_id, enabled: true }).await,
    )
}

fn gone_is_fine(r: Result<(), obws::error::Error>) -> Result<(), ObsError> {
    match r {
        Err(obws::error::Error::Api { code: obws::responses::StatusCode::ResourceNotFound, .. }) => Ok(()),
        other => other.map_err(err),
    }
}

/// What [`put_overlay`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Placed {
    /// The browser source was made now (not found by its name).
    pub input_created: bool,
    /// It wasn't in this scene before.
    pub item_added: bool,
}

/// Puts the browser source `name` (made, or its address and size fixed) at the top of `scene`.
pub async fn put_overlay(
    client: &Client,
    scene: &str,
    name: &str,
    url: &str,
    width: u32,
    height: u32,
) -> Result<Placed, ObsError> {
    let settings =
        json!({ "url": url, "width": width, "height": height, "shutdown": false, "restart_when_active": false });
    let existing =
        client.inputs().list(Some("browser_source")).await.map_err(err)?.into_iter().any(|i| i.id.name == name);
    let mut placed = Placed { input_created: false, item_added: false };
    if existing {
        client
            .inputs()
            .set_settings(SetSettings { input: InputId::Name(name), settings: &settings, overlay: Some(true) })
            .await
            .map_err(err)?;
        if item(client, scene, name).await?.is_none() {
            client
                .scene_items()
                .create(CreateSceneItem {
                    scene: SceneId::Name(scene),
                    source: SourceId::Name(name),
                    enabled: Some(true),
                })
                .await
                .map_err(err)?;
            placed.item_added = true;
        }
    } else {
        client
            .inputs()
            .create(Create {
                scene: SceneId::Name(scene),
                input: name,
                kind: "browser_source",
                settings: Some(&settings),
                enabled: Some(true),
            })
            .await
            .map_err(err)?;
        placed = Placed { input_created: true, item_added: true };
    }
    to_top(client, scene, name).await?;
    if placed.item_added {
        fill_canvas(client, scene, name).await?;
    }
    Ok(placed)
}

/// Scales a source to fill OBS's canvas (a 1080p overlay on a 1440p canvas fills it too).
pub async fn fill_canvas(client: &Client, scene: &str, source: &str) -> Result<(), ObsError> {
    use obws::common::BoundsType;
    use obws::requests::scene_items::{Bounds, Position, SceneItemTransform, SetTransform};
    let Some(id) = item(client, scene, source).await? else { return Ok(()) };
    let video = client.config().video_settings().await.map_err(err)?;
    client
        .scene_items()
        .set_transform(SetTransform {
            scene: SceneId::Name(scene),
            item_id: id,
            transform: SceneItemTransform {
                position: Some(Position { x: Some(0.0), y: Some(0.0) }),
                bounds: Some(Bounds {
                    r#type: Some(BoundsType::ScaleInner),
                    width: Some(video.base_width as f32),
                    height: Some(video.base_height as f32),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .map_err(err)
}

/// Puts `source` (a scene or an input the streamer already has) at the bottom of `scene`.
/// `true` when it was added now.
pub async fn put_under(client: &Client, scene: &str, source: &str) -> Result<bool, ObsError> {
    if item(client, scene, source).await?.is_some() {
        return Ok(false);
    }
    let id = client
        .scene_items()
        .create(CreateSceneItem { scene: SceneId::Name(scene), source: SourceId::Name(source), enabled: Some(true) })
        .await
        .map_err(|e| missing(e, source))?;
    client
        .scene_items()
        .set_index(SetIndex { scene: SceneId::Name(scene), item_id: id, index: 0 })
        .await
        .map_err(err)?;
    Ok(true)
}

/// Takes `source` out of `scene` (the source itself stays in OBS); gone already is fine.
pub async fn remove_item(client: &Client, scene: &str, source: &str) -> Result<(), ObsError> {
    let items = match client.scene_items().list(SceneId::Name(scene)).await {
        Ok(items) => items,
        Err(e) => return gone_is_fine(Err(e)),
    };
    match items.into_iter().find(|i| i.source_name == source) {
        Some(i) => gone_is_fine(client.scene_items().remove(SceneId::Name(scene), i.id).await),
        None => Ok(()),
    }
}

/// Removes an input from OBS (from every scene it's in); gone already is fine.
pub async fn remove_input(client: &Client, name: &str) -> Result<(), ObsError> {
    gone_is_fine(client.inputs().remove(InputId::Name(name)).await)
}

async fn item(client: &Client, scene: &str, source: &str) -> Result<Option<i64>, ObsError> {
    let items = client.scene_items().list(SceneId::Name(scene)).await.map_err(|e| missing(e, scene))?;
    Ok(items.into_iter().find(|i| i.source_name == source).map(|i| i.id))
}

async fn to_top(client: &Client, scene: &str, source: &str) -> Result<(), ObsError> {
    let items = client.scene_items().list(SceneId::Name(scene)).await.map_err(err)?;
    let top = items.iter().map(|i| i.index).max().unwrap_or(0);
    if let Some(i) = items.iter().find(|i| i.source_name == source)
        && i.index != top
    {
        client
            .scene_items()
            .set_index(SetIndex { scene: SceneId::Name(scene), item_id: i.id, index: top })
            .await
            .map_err(err)?;
    }
    Ok(())
}

fn missing(e: obws::error::Error, name: &str) -> ObsError {
    match &e {
        obws::error::Error::Api { code: obws::responses::StatusCode::ResourceNotFound, .. } => ObsError::Refused(
            format!("OBS has no scene or source called \"{}\".", name.chars().take(80).collect::<String>()),
        ),
        _ => err(e),
    }
}

/// Who made an old widget, from its address.
pub fn widget_vendor(url: &str) -> Option<&'static str> {
    let host = url.split("://").nth(1)?.split(['/', '?', '#']).next()?.split(':').next()?.to_ascii_lowercase();
    let is = |d: &str| host == d || host.ends_with(&format!(".{d}"));
    if is("streamelements.com") {
        Some("StreamElements")
    } else if is("streamlabs.com") {
        Some("Streamlabs")
    } else {
        None
    }
}

/// A browser source from another alerts service. Only its name, the service and where it's
/// shown are kept: its address holds that service's key and is never passed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OldWidget {
    pub source: String,
    pub vendor: String,
    /// Scenes it's in, with its item id there and whether it's shown.
    pub scenes: Vec<(String, i64, bool)>,
}

/// Every StreamElements and Streamlabs browser source, and where each is.
pub async fn old_widgets(client: &Client) -> Result<Vec<OldWidget>, ObsError> {
    let browsers = client.inputs().list(Some("browser_source")).await.map_err(err)?;
    let mut found = Vec::new();
    for b in browsers {
        let s = client.inputs().settings::<Value>(InputId::Name(&b.id.name)).await.map_err(err)?;
        let url = s.settings.get("url").and_then(Value::as_str).unwrap_or("");
        if let Some(v) = widget_vendor(url) {
            found.push(OldWidget { source: b.id.name, vendor: v.into(), scenes: Vec::new() });
        }
    }
    if found.is_empty() {
        return Ok(found);
    }
    for scene in crate::control::scenes(client).await?.scenes {
        for it in scene.items {
            if let Some(w) = found.iter_mut().find(|w| w.source == it.source) {
                w.scenes.push((scene.name.clone(), it.id, it.enabled));
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::widget_vendor;

    #[test]
    fn old_widgets_are_known_by_their_site_only() {
        assert_eq!(widget_vendor("https://streamelements.com/overlay/abc/def"), Some("StreamElements"));
        assert_eq!(widget_vendor("https://streamlabs.com/alert-box/v3/KEY"), Some("Streamlabs"));
        assert_eq!(widget_vendor("https://www.streamlabs.com:443/widgets/chat-box?token=x"), Some("Streamlabs"));
        assert_eq!(widget_vendor("http://127.0.0.1:47820/o/aaa/"), None);
        assert_eq!(widget_vendor("https://streamlabs.com.evil.example/x"), None);
        assert_eq!(widget_vendor("https://notstreamelements.com/x"), None);
        assert_eq!(widget_vendor(""), None);
    }
}
