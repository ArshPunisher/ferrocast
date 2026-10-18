//! Ferrocast voices (Pro F3, docs/plan/PRO.md, API.md §9.4): Google Cloud TTS through the
//! relay. Only a line that already passed the local TTS rules (R-48: minimum, approval list,
//! word filter, length, panic pause) gets here: alerts carry `tts_text` only after
//! `ferrocast_core::tts::decide` said Speak, and held lines only once a mod approves them.
//!
//! The audio is saved to the media library like the system voice's WAV and played the same
//! way. When Pro has lapsed, the relay is down or slow (8 s for the whole call), or the month's
//! characters are used up, the line is read with the system voice instead and the dashboard
//! gets one notice (not one per alert) until a Ferrocast voice works again. The alert queue
//! never waits for any of this: the audio is attached when it's ready.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::frames::{LiveFrame, Notice, NoticeAction, NoticeLevel};
use crate::{Engine, EngineError, now};

/// Ferrocast voice ids start with their provider: `google:hi-IN-Neural2-A`.
pub const CLOUD_PREFIX: &str = "google:";
/// The whole cloud call (registering with the relay included), before the system voice reads.
pub const CLOUD_TIMEOUT: Duration = Duration::from_secs(8);
/// The relay reads up to this many characters at a time; longer lines use the system voice.
pub const CLOUD_MAX_CHARS: usize = 300;
/// The fallback notice's key: one line on the dashboard, replaced rather than stacked.
pub const NOTICE_KEY: &str = "tts-cloud";
const SETTINGS_KEY: &str = "tts_cloud";
const VOICES_FRESH_MS: i64 = 3_600_000;

pub fn is_cloud(voice: &str) -> bool {
    voice.starts_with(CLOUD_PREFIX)
}

/// One Ferrocast voice, as the relay lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudVoice {
    pub id: String,
    pub name: String,
    pub language: String,
    #[serde(default)]
    pub gender: String,
    #[serde(default)]
    pub family: String,
}

/// The subscription's characters this month (shared by its PCs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUsage {
    pub used: i64,
    pub limit: i64,
    pub reset_at: i64,
}

/// Settings `tts_cloud`: the Ferrocast voice picked, if any. The system voice in the TTS
/// policy stays as the backup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct CloudChoice {
    voice: Option<String>,
}

/// What the engine keeps in memory about Ferrocast voices.
#[derive(Debug, Default)]
pub(crate) struct CloudTts {
    /// (fetched at, voices, resting) as the relay last listed them.
    voices: Mutex<Option<(i64, Vec<CloudVoice>, bool)>>,
    usage: Mutex<Option<CloudUsage>>,
    /// The fallback notice is up: it isn't sent again until a Ferrocast voice works again.
    noticed: AtomicBool,
    /// Why the last line was read with the system voice.
    last_fallback: Mutex<Option<String>>,
    /// Preview audio per Ferrocast voice this run, so a preview never spends characters twice.
    previews: Mutex<HashMap<String, String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `GET /api/v1/tts/cloud`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudView {
    /// The Ferrocast voice picked (`null`: the system voice reads).
    pub voice: Option<String>,
    /// Pro is active and the relay has Ferrocast voices (the remembered status).
    pub available: bool,
    /// The relay's monthly cap for everyone is reached: back on the 1st.
    pub resting: bool,
    pub usage: Option<CloudUsage>,
    /// Why the last line used the system voice, until a Ferrocast voice works again.
    pub fallback: Option<String>,
    pub max_chars: usize,
    pub voices: Vec<CloudVoice>,
}

/// A line spoken into the media library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Spoken {
    pub media_id: String,
    /// `/api/v1/media/<id>` (the dashboard adds its session).
    pub url: String,
    /// The voice that read it (`null`: the computer's default voice).
    pub voice: Option<String>,
    /// A Ferrocast voice read it.
    pub cloud: bool,
    /// Why a Ferrocast voice didn't, when one was asked for.
    pub notice: Option<String>,
}

/// The preview line, in the voice's language.
pub fn sample_line(language: &str) -> &'static str {
    if language.starts_with("hi") {
        "आशा, टिप के लिए शुक्रिया! आपके मैसेज ऐसे सुनाई देंगे।"
    } else {
        "Thanks for the tip, Asha! This is how your messages will sound."
    }
}

fn refused(status: u16, code: &'static str, message: &str, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

impl Engine {
    fn cloud_tts(&self) -> &CloudTts {
        &self.inner.cloud_tts
    }

    async fn cloud_choice(&self) -> Option<String> {
        self.db(|s| repo::settings::get::<CloudChoice>(s, SETTINGS_KEY)).await.ok().flatten().and_then(|c| c.voice)
    }

    fn note_usage(&self, u: CloudUsage) {
        *lock(&self.cloud_tts().usage) = Some(u);
    }

    /// Ferrocast voices and whether they're resting, from the relay (an hour's cache unless
    /// `fresh`). Nothing without Pro; the last list when the relay can't be reached.
    async fn cloud_voices(&self, fresh: bool) -> (Vec<CloudVoice>, bool) {
        if !self.pro_has(|f| f.tts).await {
            return (Vec::new(), false);
        }
        let cached = lock(&self.cloud_tts().voices).clone();
        if !fresh
            && let Some((at, v, resting)) = &cached
            && now().0.saturating_sub(*at) < VOICES_FRESH_MS
        {
            return (v.clone(), *resting);
        }
        let asked =
            tokio::time::timeout(CLOUD_TIMEOUT, self.pro_call(reqwest::Method::GET, "/v1/pro/tts/voices", None));
        match asked.await {
            Ok(Ok(v)) => {
                let voices: Vec<CloudVoice> = serde_json::from_value(v["voices"].clone()).unwrap_or_default();
                let resting = v["resting"].as_bool().unwrap_or(false);
                if let Ok(u) = serde_json::from_value::<CloudUsage>(v["usage"].clone()) {
                    self.note_usage(u);
                }
                *lock(&self.cloud_tts().voices) = Some((now().0, voices.clone(), resting));
                (voices, resting)
            }
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "couldn't list Ferrocast voices");
                cached.map(|(_, v, r)| (v, r)).unwrap_or_default()
            }
            Err(_) => cached.map(|(_, v, r)| (v, r)).unwrap_or_default(),
        }
    }

    /// `GET /api/v1/tts/voices`: the computer's voices (free), then Ferrocast voices
    /// (`pro: true`) when Pro is active and the relay has them.
    pub async fn tts_voices(&self) -> Vec<Value> {
        let mut list: Vec<Value> = super::voices()
            .await
            .into_iter()
            .map(|v| json!({ "id": v.id, "name": v.name, "language": v.language, "pro": false }))
            .collect();
        let (cloud, _) = self.cloud_voices(false).await;
        list.extend(cloud.into_iter().map(|v| {
            json!({ "id": v.id, "name": v.name, "language": v.language, "pro": true, "gender": v.gender, "family": v.family })
        }));
        list
    }

    /// `GET /api/v1/tts/cloud`: the picked voice, this month's usage and the voices.
    pub async fn tts_cloud(&self) -> CloudView {
        let available = self.pro_has(|f| f.tts).await;
        let (voices, resting) = if available { self.cloud_voices(true).await } else { (Vec::new(), false) };
        let usage = if available { *lock(&self.cloud_tts().usage) } else { None };
        CloudView {
            voice: self.cloud_choice().await,
            available,
            resting,
            usage,
            fallback: lock(&self.cloud_tts().last_fallback).clone(),
            max_chars: CLOUD_MAX_CHARS,
            voices,
        }
    }

    /// `PUT /api/v1/tts/cloud {voice}`: picks a Ferrocast voice (needs Pro and a voice the
    /// relay lists), or `null` to read with the system voice again (always allowed).
    pub async fn set_tts_cloud(&self, voice: Option<String>) -> Result<CloudView, EngineError> {
        let voice = voice.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(v) = &voice {
            if !self.pro_has(|f| f.tts).await {
                return Err(refused(
                    402,
                    "pro_required",
                    "This runs on Ferrocast's servers and is part of Pro.",
                    "See Settings › Pro. The voices on this computer stay free.",
                ));
            }
            let (voices, _) = self.cloud_voices(false).await;
            if !voices.iter().any(|x| x.id == *v) {
                return Err(refused(
                    404,
                    "voice_not_found",
                    "That Ferrocast voice doesn't exist.",
                    "Pick one from the list again.",
                ));
            }
        }
        let saved = CloudChoice { voice };
        self.db(move |s| repo::settings::set(s, SETTINGS_KEY, &saved)).await?;
        self.cloud_tts().noticed.store(false, Ordering::Relaxed);
        *lock(&self.cloud_tts().last_fallback) = None;
        Ok(self.tts_cloud().await)
    }

    /// Asks the relay to read `text` in `voice`: the Ogg bytes, or why not (in words).
    async fn cloud_speak(&self, text: &str, voice: &str) -> Result<Vec<u8>, String> {
        if !self.pro_has(|f| f.tts).await {
            return Err("Ferrocast voices are part of Pro, and Pro isn't active on this PC.".into());
        }
        if text.chars().count() > CLOUD_MAX_CHARS {
            return Err(format!(
                "Ferrocast voices read up to {CLOUD_MAX_CHARS} characters, and this message is longer."
            ));
        }
        let call = async {
            let url = self.relay_url().map_err(|e| e.to_string())?;
            let auth = self.relay_auth().await.map_err(|_| "Couldn't reach Ferrocast voices.".to_string())?;
            let res = self
                .relay_http()
                .post(format!("{url}/v1/pro/tts"))
                .bearer_auth(auth)
                .timeout(CLOUD_TIMEOUT)
                .json(&json!({ "text": text, "voice": voice }))
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        "Ferrocast voices didn't answer in time."
                    } else {
                        "Couldn't reach Ferrocast voices."
                    }
                    .to_string()
                })?;
            if res.status() == reqwest::StatusCode::UNAUTHORIZED
                && let Err(e) = self.inner.secrets.delete(crate::relay::INSTALL)
            {
                tracing::warn!(error = %e, "couldn't forget the relay install");
            }
            if !res.status().is_success() {
                return Err(crate::pro::relay_error(res).await.to_string());
            }
            let h = |k: &str| res.headers().get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
            if let (Some(used), Some(limit), Some(reset_at)) =
                (h("x-tts-chars-used"), h("x-tts-chars-limit"), h("x-quota-reset"))
            {
                self.note_usage(CloudUsage { used, limit, reset_at });
            }
            let bytes = res.bytes().await.map_err(|_| "Ferrocast voices stopped halfway.".to_string())?;
            if !bytes.starts_with(b"OggS") {
                return Err("Ferrocast voices sent audio Ferrocast can't play.".into());
            }
            Ok(bytes.to_vec())
        };
        match tokio::time::timeout(CLOUD_TIMEOUT, call).await {
            Ok(r) => r,
            Err(_) => Err("Ferrocast voices didn't answer in time.".into()),
        }
    }

    /// The system voice reads instead: remember why, and tell the dashboard once.
    fn cloud_fell_back(&self, why: &str, system_voice: Option<&str>) {
        *lock(&self.cloud_tts().last_fallback) = Some(why.to_string());
        tracing::info!(why, "a Ferrocast voice couldn't read; using the system voice");
        if self.cloud_tts().noticed.swap(true, Ordering::Relaxed) {
            return;
        }
        let instead =
            system_voice.map_or_else(|| "this PC's own voice".to_string(), |v| format!("this PC's voice ({v})"));
        let end = if why.ends_with('.') { "" } else { "." };
        self.send_live(LiveFrame::new(
            "notice",
            &Notice {
                key: Some(NOTICE_KEY.into()),
                level: NoticeLevel::Warn,
                message: format!("{why}{end} Reading with {instead} for now."),
                action: Some(NoticeAction { label: "Voice settings".into(), href: "#/settings/tts".into() }),
            },
        ));
    }

    /// Speaks an approved line into the media library. `voice` picks one for a preview;
    /// `None` is what alerts use: the picked Ferrocast voice, else the system voice. A
    /// Ferrocast voice that can't answer falls back to the system voice (one notice). `None`
    /// when no voice can (the overlay then uses the browser's own).
    pub(crate) async fn speak_line(&self, text: &str, voice: Option<&str>) -> Option<Spoken> {
        let cloud = match voice {
            Some(v) => is_cloud(v).then(|| v.to_string()),
            None => self.cloud_choice().await,
        };
        let system_voice = match voice {
            Some(v) if !is_cloud(v) => Some(v.to_string()),
            _ => self.state().tts.voice.clone(),
        };
        let mut notice = None;
        if let Some(cv) = &cloud {
            match self.cloud_speak(text, cv).await {
                Ok(bytes) => {
                    match self.db(move |s| Ok(repo::media::save(s, "speech.ogg", &bytes).ok())).await.ok().flatten() {
                        Some(m) => {
                            self.cloud_tts().noticed.store(false, Ordering::Relaxed);
                            *lock(&self.cloud_tts().last_fallback) = None;
                            return Some(Spoken {
                                url: format!("/api/v1/media/{}", m.id),
                                media_id: m.id,
                                voice: Some(cv.clone()),
                                cloud: true,
                                notice: None,
                            });
                        }
                        None => notice = Some("Couldn't save the Ferrocast voice's audio.".to_string()),
                    }
                }
                Err(why) => notice = Some(why),
            }
            if let Some(why) = &notice {
                self.cloud_fell_back(why, system_voice.as_deref());
            }
        }
        let work = self.store().dir().join("tmp");
        let out = work.join(format!("tts-{}.wav", ferrocast_store::new_id()));
        if let Err(e) = super::synthesize(text, system_voice.as_deref(), &work, &out).await {
            tracing::warn!(error = %e, "text-to-speech failed; the overlay will use the browser voice");
            return None;
        }
        let bytes = tokio::fs::read(&out).await.ok()?;
        let _ = tokio::fs::remove_file(&out).await;
        let saved = self.db(move |s| Ok(repo::media::save(s, "speech.wav", &bytes).ok())).await.ok().flatten()?;
        Some(Spoken {
            url: format!("/api/v1/media/{}", saved.id),
            media_id: saved.id,
            voice: system_voice,
            cloud: false,
            notice,
        })
    }

    /// `POST /api/v1/tts/preview {voice?}`: a short sample line in `voice` (default: what
    /// alerts use), through the same path as alerts. A Ferrocast voice's preview is made once
    /// per run and reused.
    pub async fn tts_preview(&self, voice: Option<String>) -> Result<Spoken, EngineError> {
        let voice = voice.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let picked = match voice {
            Some(v) => Some(v),
            None => self.cloud_choice().await.or_else(|| self.state().tts.voice.clone()),
        };
        if let Some(v) = &picked
            && is_cloud(v)
        {
            let (voices, _) = self.cloud_voices(false).await;
            if self.pro_has(|f| f.tts).await && !voices.iter().any(|x| x.id == *v) {
                return Err(refused(
                    404,
                    "voice_not_found",
                    "That Ferrocast voice doesn't exist.",
                    "Pick one from the list again.",
                ));
            }
            let cached = lock(&self.cloud_tts().previews).get(v).cloned();
            if let Some(id) = cached {
                let lookup = id.clone();
                let store = self.store();
                let exists = tokio::task::spawn_blocking(move || repo::media::get(&store, &lookup))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten()
                    .is_some();
                if exists {
                    return Ok(Spoken {
                        url: format!("/api/v1/media/{id}"),
                        media_id: id,
                        voice: Some(v.clone()),
                        cloud: true,
                        notice: None,
                    });
                }
            }
        }
        let language = match &picked {
            Some(v) if is_cloud(v) => lock(&self.cloud_tts().voices)
                .as_ref()
                .and_then(|(_, list, _)| list.iter().find(|x| x.id == *v).map(|x| x.language.clone()))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let spoken = self.speak_line(sample_line(&language), picked.as_deref()).await.ok_or_else(|| {
            EngineError::Unavailable(
                "This computer has no voice Ferrocast can use. On Linux, install espeak-ng from your package manager."
                    .into(),
            )
        })?;
        if spoken.cloud
            && let Some(v) = &spoken.voice
        {
            lock(&self.cloud_tts().previews).insert(v.clone(), spoken.media_id.clone());
        }
        Ok(spoken)
    }
}
