//! Text-to-speech voices for Settings › Text-to-speech (API.md §4 TTS, §9.4): the computer's
//! own voices plus, with Pro, Ferrocast voices; which Ferrocast voice reads; and a short
//! preview through the same path alerts use.

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::ApiResult;
use crate::extract::Body;

/// `GET /api/v1/tts/voices`: this computer's voices (`pro: false`), then Ferrocast voices
/// (`pro: true`) when Pro is active and the relay has them.
pub async fn voices(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.tts_voices().await))
}

/// `GET /api/v1/tts/cloud`.
pub async fn cloud(State(app): State<AppState>) -> Json<Value> {
    Json(json!(app.engine.tts_cloud().await))
}

#[derive(Deserialize)]
pub struct CloudBody {
    /// A Ferrocast voice id, or `null` to read with this computer's voice.
    voice: Option<String>,
}

/// `PUT /api/v1/tts/cloud {voice}`.
pub async fn set_cloud(State(app): State<AppState>, Body(b): Body<CloudBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.set_tts_cloud(b.voice).await?)))
}

#[derive(Deserialize, Default)]
pub struct PreviewBody {
    #[serde(default)]
    voice: Option<String>,
}

/// `POST /api/v1/tts/preview {voice?}` → `{ media_id, url, voice, cloud, notice }`.
pub async fn preview(State(app): State<AppState>, Body(b): Body<PreviewBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.tts_preview(b.voice).await?)))
}
