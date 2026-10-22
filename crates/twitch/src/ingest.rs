//! Go live everywhere: this channel's stream key (Get Stream Key, `channel:read:stream_key`) and
//! the nearest ingest server from Twitch's list (nearest first, as OBS's "auto" picks). We take
//! the server's secure address: OBS's "auto" uses plain RTMP, which sends the key unencrypted.

use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use crate::TwitchError;
use crate::helix::Helix;

/// Twitch's own secure address, used when the ingest list can't be read.
pub const FALLBACK_INGEST: &str = "rtmps://live.twitch.tv:443/app";
const LIST_DEADLINE: Duration = Duration::from_secs(5);

impl Helix {
    /// The channel's stream key. Twitch only gives it to its owner.
    pub async fn stream_key(&self, broadcaster: &str) -> Result<String, TwitchError> {
        let v = self.call(Method::GET, "/streams/key", &[("broadcaster_id", broadcaster)], None).await?;
        v.pointer("/data/0/stream_key")
            .and_then(Value::as_str)
            .filter(|k| !k.trim().is_empty())
            .map(|k| k.trim().to_string())
            .ok_or_else(|| TwitchError::Refused("Twitch didn't give out the stream key".into()))
    }

    /// The nearest server's secure address, or [`FALLBACK_INGEST`].
    pub async fn nearest_ingest(&self) -> String {
        let url = format!("{}/ingests", self.auth.ep.ingest);
        let list: Option<Value> = async {
            let res = self.auth.http.get(url).timeout(LIST_DEADLINE).send().await.ok()?;
            if !res.status().is_success() {
                return None;
            }
            res.json().await.ok()
        }
        .await;
        list.as_ref().and_then(pick_ingest).unwrap_or_else(|| FALLBACK_INGEST.to_string())
    }
}

/// The first available server in Twitch's list (it comes nearest first), secure address.
pub(crate) fn pick_ingest(list: &Value) -> Option<String> {
    list.get("ingests")?.as_array()?.iter().find_map(|i| {
        let available = i.get("availability").and_then(Value::as_f64).unwrap_or(1.0) > 0.0;
        let template = i.get("url_template_secure").and_then(Value::as_str)?;
        available.then(|| server_of(template)).flatten()
    })
}

/// `rtmps://host:443/app/{stream_key}` → `rtmps://host:443/app`.
fn server_of(template: &str) -> Option<String> {
    let s = template.trim().strip_suffix("{stream_key}")?.trim_end_matches('/');
    (s.starts_with("rtmps://") && s.len() > 8).then(|| s.to_string())
}
