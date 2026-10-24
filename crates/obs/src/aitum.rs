//! Aitum Multistream: an OBS plugin that also sends the stream to several platforms. With it
//! and Ferrocast's Go live both on, a platform can get the stream twice and the upload carries
//! double, so Ferrocast looks for it (docs/plan/LIVE-STUDIO.md, A2.2).
//!
//! From 1.0.9 the plugin answers obs-websocket vendor requests (`aitum-multistream`,
//! `get_outputs`) with its outputs' names; 1.0.8 and earlier don't, so then the names come from
//! its settings file. It runs each as an OBS output called `aitum_multi_output_<name>`.
//! Where an output goes is only in the plugin's own settings file,
//! `<OBS config>/plugin_config/aitum-multistream/config.json`; Ferrocast reads each output's
//! name and server there, never its key.

use std::path::{Path, PathBuf};

use obws::Client;
use obws::requests::general::CallVendorRequest;
use serde::Deserialize;
use serde_json::Value;

use crate::ObsError;
use crate::control::err;

pub const VENDOR: &str = "aitum-multistream";
const OUTPUT_PREFIX: &str = "aitum_multi_output_";
/// Bigger than any real settings file; a huge file is a damaged one.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
struct Outputs {
    #[serde(default)]
    outputs: Vec<Named>,
}

#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    name: String,
}

/// The plugin's outputs and whether each is sending now, or `None` when this OBS doesn't have
/// the plugin. `from_file` is the output names in its settings file, used when the plugin is
/// too old to answer (none there: not installed).
pub async fn outputs(client: &Client, from_file: &[String]) -> Result<Option<Vec<(String, bool)>>, ObsError> {
    let req =
        CallVendorRequest { vendor_name: VENDOR, request_type: "get_outputs", request_data: &serde_json::json!({}) };
    let names: Vec<String> = match client.general().call_vendor_request::<_, Outputs>(req).await {
        Ok(res) => res.response_data.outputs.into_iter().map(|o| o.name).filter(|n| !n.is_empty()).collect(),
        Err(_) if !from_file.is_empty() => from_file.to_vec(),
        Err(_) => return Ok(None),
    };
    let active: Vec<String> = client
        .outputs()
        .list()
        .await
        .map_err(err)?
        .into_iter()
        .filter(|o| o.active)
        .filter_map(|o| o.name.strip_prefix(OUTPUT_PREFIX).map(String::from))
        .collect();
    Ok(Some(
        names
            .into_iter()
            .map(|name| {
                let live = active.contains(&name);
                (name, live)
            })
            .collect(),
    ))
}

/// The plugin's settings file next to obs-websocket's (`…/plugin_config/obs-websocket/config.json`).
pub fn config_path(websocket_config: &Path) -> Option<PathBuf> {
    Some(websocket_config.parent()?.parent()?.join(VENDOR).join("config.json"))
}

/// Each output's name and server host (when it has one), in order, for OBS profile `profile`.
/// Keys are never read.
pub fn servers(config: &[u8], profile: &str) -> Vec<(String, Option<String>)> {
    let Ok(v) = serde_json::from_slice::<Value>(config) else { return Vec::new() };
    let Some(profiles) = v.get("profiles").and_then(Value::as_array) else { return Vec::new() };
    let Some(p) = profiles.iter().find(|p| p.get("name").and_then(Value::as_str) == Some(profile)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["outputs", "vertical_outputs"] {
        for o in p.get(key).and_then(Value::as_array).into_iter().flatten() {
            let name = o.get("name").and_then(Value::as_str).unwrap_or_default();
            let server = o.get("stream_server").and_then(Value::as_str).unwrap_or_default();
            if !name.is_empty() {
                out.push((name.to_string(), host_of(server)));
            }
        }
    }
    out
}

/// `rtmps://a.rtmp.youtube.com:443/live2` → `a.rtmp.youtube.com`.
fn host_of(server: &str) -> Option<String> {
    let rest = server.split_once("://")?.1;
    let host = rest.split(['/', ':', '?']).next()?.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Aitum Vertical (OBS's vertical canvas plugin): its vendor name for obs-websocket.
pub const VERTICAL_VENDOR: &str = "aitum-vertical-canvas";

#[derive(Deserialize)]
struct VerticalStatus {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    streaming: bool,
}

#[derive(Deserialize)]
struct Done {
    #[serde(default)]
    success: bool,
}

/// Whether OBS's vertical canvas is streaming, or `None` when OBS doesn't have Aitum Vertical.
pub async fn vertical_streaming(client: &Client) -> Option<bool> {
    let req = CallVendorRequest {
        vendor_name: VERTICAL_VENDOR,
        request_type: "status",
        request_data: &serde_json::json!({}),
    };
    let res = client.general().call_vendor_request::<_, VerticalStatus>(req).await.ok()?;
    res.response_data.success.then_some(res.response_data.streaming)
}

/// Points the vertical canvas's first stream output at `server` with `key`.
pub async fn set_vertical_stream(client: &Client, server: &str, key: &str) -> Result<(), ObsError> {
    for (request_type, data) in [
        ("update_stream_server", serde_json::json!({ "stream_server": server, "index": 0 })),
        ("update_stream_key", serde_json::json!({ "stream_key": key, "index": 0 })),
    ] {
        let req = CallVendorRequest { vendor_name: VERTICAL_VENDOR, request_type, request_data: &data };
        let res = client.general().call_vendor_request::<_, Done>(req).await.map_err(err)?;
        if !res.response_data.success {
            return Err(ObsError::Refused(
                "Aitum Vertical didn't take the change. Open its settings in OBS once (the vertical dock's gear), then try again.".into(),
            ));
        }
    }
    Ok(())
}

/// Aitum Vertical's settings file next to obs-websocket's.
pub fn vertical_config_path(websocket_config: &Path) -> Option<PathBuf> {
    Some(websocket_config.parent()?.parent()?.join("vertical-canvas").join("config.json"))
}

/// The vertical canvas's first stream output: its server and key, to put back later.
pub fn vertical_first_output(config: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(config).ok()?;
    let canvas = v.get("canvas")?.as_array()?.first()?;
    let out = canvas.get("stream_outputs").and_then(Value::as_array).and_then(|o| o.first()).unwrap_or(canvas);
    let server = out.get("stream_server").and_then(Value::as_str)?.trim().to_string();
    let key = out.get("stream_key").and_then(Value::as_str).unwrap_or_default().to_string();
    (!server.is_empty()).then_some((server, key))
}

/// A server's host, for saying where something streams to without its key.
pub fn server_host(server: &str) -> Option<String> {
    host_of(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_names_and_hosts_never_keys() {
        let cfg = serde_json::json!({
            "profiles": [
                { "name": "Other", "outputs": [{ "name": "Old", "stream_server": "rtmp://old.example.com/live" }] },
                { "name": "Untitled", "outputs": [
                    { "name": "Twitch Output", "stream_server": "rtmps://aps30.contribute.live-video.net/app", "stream_key": "live_secret" },
                    { "name": "Kick", "stream_server": "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app/" },
                    { "name": "Broken", "stream_server": "not a url" },
                    { "stream_server": "rtmp://nameless.example.com/live" }
                ], "vertical_outputs": [{ "name": "Shorts", "stream_server": "rtmps://a.rtmps.youtube.com/live2" }] }
            ]
        });
        let s = servers(&serde_json::to_vec(&cfg).unwrap(), "Untitled");
        let host = |n: &str| s.iter().find(|(x, _)| x == n).and_then(|(_, h)| h.clone());
        assert_eq!(
            s.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["Twitch Output", "Kick", "Broken", "Shorts"],
            "in order; nameless ones left out"
        );
        assert_eq!(host("Twitch Output").as_deref(), Some("aps30.contribute.live-video.net"));
        assert_eq!(host("Kick").as_deref(), Some("fa723fc1b171.global-contribute.live-video.net"));
        assert_eq!(host("Shorts").as_deref(), Some("a.rtmps.youtube.com"));
        assert_eq!(host("Broken"), None);
        assert!(!format!("{s:?}").contains("secret"));
        assert!(servers(b"{nope", "Untitled").is_empty());
        let vertical = serde_json::json!({ "canvas": [{ "width": 1080, "height": 1920, "stream_outputs": [
            { "name": "YouTube", "enabled": true, "stream_server": "rtmps://a.rtmps.youtube.com/live2", "stream_key": "yt-vertical" }
        ] }] });
        assert_eq!(
            vertical_first_output(&serde_json::to_vec(&vertical).unwrap()),
            Some(("rtmps://a.rtmps.youtube.com/live2".into(), "yt-vertical".into()))
        );
        let legacy =
            serde_json::json!({ "canvas": [{ "stream_server": "rtmp://x.example.com/live", "stream_key": "k" }] });
        assert_eq!(
            vertical_first_output(&serde_json::to_vec(&legacy).unwrap()).unwrap().0,
            "rtmp://x.example.com/live"
        );
        assert_eq!(vertical_first_output(br#"{"canvas":[{}]}"#), None);
        assert!(servers(&serde_json::to_vec(&cfg).unwrap(), "Missing").is_empty());
        let ws = Path::new("/c/obs-studio/plugin_config/obs-websocket/config.json");
        assert_eq!(config_path(ws).unwrap(), Path::new("/c/obs-studio/plugin_config/aitum-multistream/config.json"));
    }
}
