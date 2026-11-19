//! Twitch Enhanced Broadcasting's plan (docs/plan/TWITCH-EB.md): before going live, the PC asks
//! Twitch which qualities to encode and where to send them, as OBS does
//! (`frontend/utility/GoLiveAPI_*.cpp`). The answer drives the extra encoders; a failure here
//! never stops going live, the stream just goes out with one quality.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where Twitch answers (OBS's `multitrack_video_configuration_url` for Twitch).
pub const CONFIG_URL: &str = "https://ingest.twitch.tv/api/v3/GetClientConfiguration";

/// What the PC tells Twitch about itself and its stream.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub stream_key: String,
    /// Ferrocast's version.
    pub version: String,
    /// The codecs its encoders can make ("h264", "h265", "av1").
    pub codecs: Vec<String>,
    /// The stream's size and the canvas it's scaled from.
    pub width: u32,
    pub height: u32,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub fps: (u32, u32),
    /// A second audio track for the VOD (Settings › Stream › Twitch VOD track).
    pub vod_track_audio: bool,
    /// Caps the owner set (kbps total; tracks).
    pub max_bitrate_kbps: Option<u64>,
    pub max_tracks: Option<u32>,
    pub audio_rate: u32,
    pub audio_channels: u32,
    /// The computer, as OBS describes it (`system_info`); fields Twitch may use to pick.
    pub capabilities: Value,
}

/// The request body, as OBS sends it (schema 2025-01-25).
pub fn body(a: &Ask) -> Value {
    let mut prefs = serde_json::json!({
        "vod_track_audio": a.vod_track_audio,
        "canvases": [{
            "width": a.width,
            "height": a.height,
            "canvas_width": a.canvas_width,
            "canvas_height": a.canvas_height,
            "framerate": { "numerator": a.fps.0, "denominator": a.fps.1.max(1) },
        }],
        "audio_samples_per_sec": a.audio_rate,
        "audio_channels": a.audio_channels,
        "audio_fixed_buffering": false,
        "audio_max_buffering_ms": 0,
    });
    if let Some(b) = a.max_bitrate_kbps {
        prefs["maximum_aggregate_bitrate"] = Value::from(b);
    }
    if let Some(t) = a.max_tracks {
        prefs["maximum_video_tracks"] = Value::from(t.min(MAX_TRACKS));
    }
    serde_json::json!({
        "service": "IVS",
        "schema_version": "2025-01-25",
        "authentication": a.stream_key,
        "client": { "name": "ferrocast", "version": a.version, "supported_codecs": a.codecs },
        "capabilities": a.capabilities,
        "preferences": prefs,
    })
}

/// The most encoders run at once (OBS's MAX_OUTPUT_VIDEO_ENCODERS is 10; fewer here).
pub const MAX_TRACKS: u32 = 5;

/// One quality to encode.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Encode {
    /// The encoder OBS would use ("obs_x264", "jim_nvenc", "com.apple.videotoolbox…").
    #[serde(rename = "type", default)]
    pub kind: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub framerate: Option<Fps>,
    /// Its encoder settings (`bitrate` kbps, `keyint_sec`, `profile`, `bf`, …), OBS's keys.
    #[serde(default)]
    pub settings: Value,
    #[serde(default)]
    pub canvas_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Fps {
    pub numerator: u32,
    pub denominator: u32,
}

impl Encode {
    /// Its bitrate in kbps (0 when the plan gave none).
    pub fn kbps(&self) -> u32 {
        self.settings.get("bitrate").and_then(Value::as_u64).map_or(0, |b| b.min(u64::from(u32::MAX)) as u32)
    }
}

#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    meta: Option<Meta>,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    ingest_endpoints: Vec<Endpoint>,
    #[serde(default)]
    encoder_configurations: Vec<Encode>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    #[serde(default)]
    config_id: String,
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    result: String,
    #[serde(default)]
    html_en_us: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    url_template: String,
    #[serde(default)]
    authentication: Option<String>,
}

/// Twitch's plan: what to encode (highest first, as Twitch lists them) and where to send it.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub config_id: String,
    /// The RTMP(S) address with the key in it.
    pub url: String,
    pub encodes: Vec<Encode>,
    /// Twitch's warning, in words, when it said go ahead anyway (asked before going on).
    pub warning: Option<String>,
}

/// Why there's no plan, in words for the Go live screen (the stream then goes out as usual).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EbError {
    #[error("Twitch didn't answer about Enhanced Broadcasting ({0})")]
    Unreachable(String),
    #[error("Twitch's answer about Enhanced Broadcasting couldn't be read")]
    Unreadable,
    #[error("Twitch said no to Enhanced Broadcasting: {0}")]
    Refused(String),
    #[error("Twitch gave no qualities or address for Enhanced Broadcasting")]
    Empty,
}

/// Reads Twitch's answer into a plan for `stream_key`.
pub fn plan(answer: &[u8], stream_key: &str) -> Result<Plan, EbError> {
    let a: Answer = serde_json::from_slice(answer).map_err(|_| EbError::Unreadable)?;
    let words = |s: &Status| s.html_en_us.as_deref().map(plain).unwrap_or_else(|| s.result.clone());
    let mut warning = None;
    if let Some(s) = &a.status {
        match s.result.as_str() {
            "" | "success" => {}
            "error" => return Err(EbError::Refused(words(s))),
            // A warning (or a result OBS doesn't know): go on if there's a plan, asking first.
            _ if a.encoder_configurations.is_empty() => return Err(EbError::Refused(words(s))),
            _ => warning = Some(words(s)),
        }
    }
    let e = a
        .ingest_endpoints
        .iter()
        .find(|e| e.protocol.eq_ignore_ascii_case("RTMPS"))
        .or_else(|| a.ingest_endpoints.iter().find(|e| e.protocol.eq_ignore_ascii_case("RTMP")))
        .ok_or(EbError::Empty)?;
    let key = e.authentication.as_deref().filter(|k| !k.is_empty()).unwrap_or(stream_key);
    let url = e.url_template.replace("{stream_key}", key);
    let encodes: Vec<Encode> = a
        .encoder_configurations
        .into_iter()
        .filter(|c| c.width > 0 && c.height > 0)
        .take(MAX_TRACKS as usize)
        .collect();
    if encodes.is_empty() || url.is_empty() {
        return Err(EbError::Empty);
    }
    Ok(Plan { config_id: a.meta.map(|m| m.config_id).unwrap_or_default(), url, encodes, warning })
}

/// Twitch's HTML message as plain words (its tags taken out, its spaces tidied).
fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    let out = out.replace("&amp;", "&").replace("&nbsp;", " ").replace("&#39;", "'").replace("&quot;", "\"");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Asks Twitch (5 s at most, as OBS waits).
pub async fn fetch(client: &reqwest::Client, ask: &Ask) -> Result<Plan, EbError> {
    let reply = client
        .post(CONFIG_URL)
        .timeout(std::time::Duration::from_secs(5))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body(ask).to_string())
        .send()
        .await
        .map_err(|e| EbError::Unreachable(e.without_url().to_string()))?;
    let status = reply.status();
    let bytes = reply.bytes().await.map_err(|e| EbError::Unreachable(e.without_url().to_string()))?;
    match plan(&bytes, &ask.stream_key) {
        Err(EbError::Unreadable) if !status.is_success() => Err(EbError::Unreachable(format!("HTTP {status}"))),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer shaped as Twitch's (OBS's model; values made up).
    const ANSWER: &str = r#"{
        "meta": { "service": "IVS", "schema_version": "2025-01-25", "config_id": "cfg-123" },
        "status": { "result": "success" },
        "ingest_endpoints": [
            { "protocol": "RTMP", "url_template": "rtmp://ingest.test/app/{stream_key}" },
            { "protocol": "RTMPS", "url_template": "rtmps://ingest.test:443/app/{stream_key}" }
        ],
        "encoder_configurations": [
            { "type": "com.apple.videotoolbox.videoencoder.ave.avc", "width": 1920, "height": 1080,
              "framerate": { "numerator": 60, "denominator": 1 },
              "settings": { "bitrate": 6000, "keyint_sec": 2, "profile": "high" } },
            { "type": "com.apple.videotoolbox.videoencoder.ave.avc", "width": 1280, "height": 720,
              "settings": { "bitrate": 3000 } },
            { "type": "com.apple.videotoolbox.videoencoder.ave.avc", "width": 852, "height": 480,
              "settings": { "bitrate": 1200 } }
        ],
        "audio_configurations": { "live": [{ "codec": "aac", "track_id": 0, "channels": 2, "settings": { "bitrate": 160 } }] }
    }"#;

    #[test]
    fn twitchs_plan_is_read_with_the_secure_address_first() {
        let p = plan(ANSWER.as_bytes(), "live_abc").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.config_id, "cfg-123");
        assert_eq!(p.url, "rtmps://ingest.test:443/app/live_abc");
        assert_eq!(
            p.encodes.iter().map(|e| (e.width, e.height, e.kbps())).collect::<Vec<_>>(),
            [(1920, 1080, 6000), (1280, 720, 3000), (852, 480, 1200)]
        );
        assert_eq!(p.encodes[0].framerate, Some(Fps { numerator: 60, denominator: 1 }));
        assert_eq!(p.warning, None);
    }

    #[test]
    fn a_refusal_or_an_empty_plan_says_why_in_words() {
        let no = r#"{ "status": { "result": "error", "html_en_us": "<p>Your <b>GPU</b> isn&#39;t supported.</p>" } }"#;
        assert_eq!(plan(no.as_bytes(), "k"), Err(EbError::Refused("Your GPU isn't supported.".into())));
        let warn_only = r#"{ "status": { "result": "warning", "html_en_us": "Low upload" } }"#;
        assert_eq!(plan(warn_only.as_bytes(), "k"), Err(EbError::Refused("Low upload".into())));
        assert_eq!(plan(b"{}", "k"), Err(EbError::Empty));
        assert_eq!(plan(b"not json", "k"), Err(EbError::Unreadable));
    }

    #[test]
    fn a_warning_with_a_plan_goes_on_and_is_said() {
        let mut v: Value = serde_json::from_str(ANSWER).unwrap_or_default();
        v["status"] = serde_json::json!({ "result": "warning", "html_en_us": "Your upload is close to the limit." });
        let p = plan(v.to_string().as_bytes(), "k").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.warning.as_deref(), Some("Your upload is close to the limit."));
    }

    #[test]
    fn an_endpoints_own_key_and_the_track_cap_are_kept() {
        let mut v: Value = serde_json::from_str(ANSWER).unwrap_or_default();
        v["ingest_endpoints"] = serde_json::json!([{ "protocol": "RTMP", "url_template": "rtmp://x/app/{stream_key}", "authentication": "eb_key" }]);
        let many: Vec<Value> = (0..8).map(|i| serde_json::json!({ "width": 640 + i, "height": 360 })).collect();
        v["encoder_configurations"] = Value::from(many);
        let p = plan(v.to_string().as_bytes(), "live_abc").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.url, "rtmp://x/app/eb_key");
        assert_eq!(p.encodes.len(), MAX_TRACKS as usize);
    }

    #[test]
    fn the_request_is_shaped_as_obss() {
        let b = body(&Ask {
            stream_key: "live_abc".into(),
            version: "3.2.0".into(),
            codecs: vec!["h264".into()],
            width: 1920,
            height: 1080,
            canvas_width: 2560,
            canvas_height: 1440,
            fps: (60, 1),
            max_tracks: Some(9),
            audio_rate: 48000,
            audio_channels: 2,
            capabilities: serde_json::json!({ "cpu": { "logical_cores": 10 } }),
            ..Default::default()
        });
        assert_eq!(b["service"], "IVS");
        assert_eq!(b["schema_version"], "2025-01-25");
        assert_eq!(b["authentication"], "live_abc");
        assert_eq!(b["client"]["supported_codecs"][0], "h264");
        assert_eq!(b["preferences"]["canvases"][0]["canvas_width"], 2560);
        assert_eq!(b["preferences"]["canvases"][0]["framerate"]["numerator"], 60);
        assert_eq!(b["preferences"]["maximum_video_tracks"], MAX_TRACKS);
        assert!(b["preferences"].get("maximum_aggregate_bitrate").is_none());
    }
}
