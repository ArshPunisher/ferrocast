//! Ferrocast Pro F3, premium TTS voices (docs/plan/PRO.md, milestone M4): not built yet.
//! Coming here: `TtsProvider` (Google Cloud TTS, a fake for tests), `/v1/pro/tts*`. Its routes, tables (`migrate`) and limits stay in this module.

use axum::Router;

use crate::AppState;
use crate::quota::Limit;

/// This feature's routes (none yet), merged in `crate::router`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
}

/// `features.tts` in `/v1/pro/status`: true only once it's built and configured here.
pub(crate) fn available(_st: &AppState) -> bool {
    false
}

/// Its per-subscription counters, listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[];
