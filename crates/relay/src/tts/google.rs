//! Google Cloud Text-to-Speech over its REST API (`v1/text:synthesize`), Ogg Opus out.
//!
//! The API key comes from `GOOGLE_TTS_KEY` (restricted to the Text-to-Speech API in the
//! Google Cloud console). It travels in the `X-Goog-Api-Key` header rather than `?key=`, so
//! it can never end up in a URL that an error message or a proxy log prints, and it's never
//! logged or shown by `Debug`. The text is never logged either.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use super::provider::{Audio, CloudVoice, SynthFuture, TtsError, TtsProvider, is_ogg};

pub const DEFAULT_URL: &str = "https://texttospeech.googleapis.com/v1/text:synthesize";
/// The whole call, connect to last byte: inside the relay's 7 s and the engine's 8 s.
const TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A 300-character line in Opus is a few dozen KB; anything near this is wrong.
const MAX_AUDIO: usize = 2 * 1024 * 1024;

/// `(google name, language, gender)`: Hindi and Indian English first (most Ferrocast streamers
/// are in India), then two US English voices. Neural2 and WaveNet only: their free tier is
/// what the global cap keeps us under.
const CURATED: &[(&str, &str, &str)] = &[
    ("hi-IN-Neural2-A", "hi-IN", "female"),
    ("hi-IN-Neural2-B", "hi-IN", "male"),
    ("hi-IN-Wavenet-D", "hi-IN", "female"),
    ("hi-IN-Wavenet-C", "hi-IN", "male"),
    ("en-IN-Neural2-A", "en-IN", "female"),
    ("en-IN-Neural2-B", "en-IN", "male"),
    ("en-IN-Wavenet-D", "en-IN", "female"),
    ("en-IN-Wavenet-C", "en-IN", "male"),
    ("en-US-Neural2-F", "en-US", "female"),
    ("en-US-Neural2-D", "en-US", "male"),
];

fn language_name(lang: &str) -> &'static str {
    match lang {
        "hi-IN" => "Hindi",
        "en-IN" => "Indian English",
        "en-US" => "US English",
        _ => "English",
    }
}

/// The curated voices, as every provider in this module lists them.
pub fn voices() -> Vec<CloudVoice> {
    CURATED
        .iter()
        .map(|(name, lang, gender)| {
            let family = if name.contains("Neural2") { "Neural2" } else { "WaveNet" };
            CloudVoice {
                id: format!("google:{name}"),
                name: format!("{}, {gender} ({family})", language_name(lang)),
                language: (*lang).to_string(),
                gender: if *gender == "female" { "female" } else { "male" },
                family,
            }
        })
        .collect()
}

pub struct GoogleTts {
    key: String,
    url: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for GoogleTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleTts").field("key", &"<redacted>").field("url", &self.url).finish()
    }
}

impl GoogleTts {
    /// `url` is Google's endpoint in production (tests point it at a fake).
    pub fn new(key: String, url: String) -> GoogleTts {
        let http =
            reqwest::Client::builder().timeout(TIMEOUT).connect_timeout(CONNECT_TIMEOUT).build().unwrap_or_default();
        GoogleTts { key, url, http }
    }

    /// From `GOOGLE_TTS_KEY` (and `GOOGLE_TTS_URL`, only for tests); `None` without a key.
    pub fn from_env() -> Option<GoogleTts> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let key = env("GOOGLE_TTS_KEY")?;
        Some(GoogleTts::new(key, env("GOOGLE_TTS_URL").unwrap_or_else(|| DEFAULT_URL.into())))
    }

    async fn call(&self, text: &str, voice: &str) -> Result<Audio, TtsError> {
        let name = voice.strip_prefix("google:").ok_or(TtsError::BadVoice)?;
        let (_, lang, _) = CURATED.iter().find(|(n, _, _)| *n == name).ok_or(TtsError::BadVoice)?;
        let body = json!({
            "input": { "text": text },
            "voice": { "languageCode": lang, "name": name },
            "audioConfig": { "audioEncoding": "OGG_OPUS" },
        });
        let res =
            self.http.post(&self.url).header("x-goog-api-key", &self.key).json(&body).send().await.map_err(|e| {
                TtsError::Unavailable(if e.is_timeout() { "timed out".into() } else { "unreachable".into() })
            })?;
        let status = res.status().as_u16();
        if status != 200 {
            // Google's message can quote the input; read only what we need, never log it.
            let v: Value = res.json().await.unwrap_or(Value::Null);
            let message = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
            return Err(match status {
                429 => TtsError::Quota,
                400 if message.contains("longer than") || message.contains("too long") => TtsError::TooLong,
                400 if message.contains("voice") => TtsError::BadVoice,
                400 => TtsError::Unavailable("Google refused the request".into()),
                401 | 403 => TtsError::Unavailable("Google refused the API key".into()),
                s if s >= 500 => TtsError::Unavailable(format!("Google answered {s}")),
                s => TtsError::Unavailable(format!("Google answered {s}")),
            });
        }
        let v: Value = res.json().await.map_err(|_| TtsError::Unavailable("an unreadable answer".into()))?;
        let b64 = v["audioContent"].as_str().ok_or_else(|| TtsError::Unavailable("no audio in the answer".into()))?;
        if b64.len() > MAX_AUDIO * 4 / 3 + 4 {
            return Err(TtsError::Unavailable("the audio was too big".into()));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| TtsError::Unavailable("audio that isn't base64".into()))?;
        if !is_ogg(&bytes) {
            return Err(TtsError::Unavailable("audio that isn't Ogg".into()));
        }
        Ok(Audio { bytes, mime: "audio/ogg" })
    }
}

impl TtsProvider for GoogleTts {
    fn voices(&self) -> Vec<CloudVoice> {
        voices()
    }

    fn synthesize<'a>(&'a self, text: &'a str, voice: &'a str) -> SynthFuture<'a> {
        Box::pin(self.call(text, voice))
    }
}
