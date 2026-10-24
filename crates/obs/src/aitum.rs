//! Aitum Multistream: an OBS plugin that also sends the stream to several platforms. With it
//! and Ferrocast's Go live both on, a platform can get the stream twice and the upload carries
//! double, so Ferrocast looks for it (docs/plan/LIVE-STUDIO.md, A2.2).
//!
//! The plugin answers obs-websocket vendor requests (`aitum-multistream`, `get_outputs`) with
//! its outputs' names, and runs each as an OBS output called `aitum_multi_output_<name>`.
//! Where an output goes is only in the plugin's own settings file,
//! `<OBS config>/plugin_config/aitum-multistream/config.json`; Ferrocast reads each output's
//! name and server there, never its key.

use std::collections::HashMap;
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
/// the plugin.
pub async fn outputs(client: &Client) -> Result<Option<Vec<(String, bool)>>, ObsError> {
    let req =
        CallVendorRequest { vendor_name: VENDOR, request_type: "get_outputs", request_data: &serde_json::json!({}) };
    let Ok(res) = client.general().call_vendor_request::<_, Outputs>(req).await else {
        return Ok(None);
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
    let list = res
        .response_data
        .outputs
        .into_iter()
        .filter(|o| !o.name.is_empty())
        .map(|o| {
            let live = active.contains(&o.name);
            (o.name, live)
        })
        .collect();
    Ok(Some(list))
}

/// The plugin's settings file next to obs-websocket's (`…/plugin_config/obs-websocket/config.json`).
pub fn config_path(websocket_config: &Path) -> Option<PathBuf> {
    Some(websocket_config.parent()?.parent()?.join(VENDOR).join("config.json"))
}

/// Each output's server host, by output name, for OBS profile `profile`. Keys are never read.
pub fn servers(config: &[u8], profile: &str) -> HashMap<String, String> {
    let Ok(v) = serde_json::from_slice::<Value>(config) else { return HashMap::new() };
    let Some(profiles) = v.get("profiles").and_then(Value::as_array) else { return HashMap::new() };
    let Some(p) = profiles.iter().find(|p| p.get("name").and_then(Value::as_str) == Some(profile)) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for key in ["outputs", "vertical_outputs"] {
        for o in p.get(key).and_then(Value::as_array).into_iter().flatten() {
            let name = o.get("name").and_then(Value::as_str).unwrap_or_default();
            let server = o.get("stream_server").and_then(Value::as_str).unwrap_or_default();
            if let (false, Some(host)) = (name.is_empty(), host_of(server)) {
                out.insert(name.to_string(), host);
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
        assert_eq!(s.len(), 3);
        assert_eq!(s["Twitch Output"], "aps30.contribute.live-video.net");
        assert_eq!(s["Kick"], "fa723fc1b171.global-contribute.live-video.net");
        assert_eq!(s["Shorts"], "a.rtmps.youtube.com");
        assert!(!format!("{s:?}").contains("secret"));
        assert!(servers(b"{nope", "Untitled").is_empty());
        assert!(servers(&serde_json::to_vec(&cfg).unwrap(), "Missing").is_empty());
        let ws = Path::new("/c/obs-studio/plugin_config/obs-websocket/config.json");
        assert_eq!(config_path(ws).unwrap(), Path::new("/c/obs-studio/plugin_config/aitum-multistream/config.json"));
    }
}
