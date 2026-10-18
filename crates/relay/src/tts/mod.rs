//! Ferrocast Pro F3, premium TTS voices (docs/plan/PRO.md F3, milestone M4; API.md §9.4).
//!
//! The desktop sends a line that already passed its own TTS rules (R-48: minimum, approval,
//! filters, length) and gets Ogg Opus audio back from Google Cloud Text-to-Speech.
//!
//! **Money safety.** Google bills past its free tier, and the owner must never get a bill:
//! - per subscription, 20,000 characters a calendar month (`tts_chars`, `quota`), given back
//!   when the voice provider fails;
//! - relay-wide, `TTS_GLOBAL_MONTHLY_CHARS` (default 800,000, never above 1,000,000: Google's
//!   free Neural2 tier) counted in `tts_usage` before Google is called; once reached every
//!   request is a 503 and Google is never called until the next month;
//! - 60 requests a minute relay-wide and 20 per subscription, so a stuck loop can't burn it.
//!
//! Stored: those counters only. Never the text, the voice or the audio (R-51).

mod fake;
mod google;
mod provider;
mod store;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

pub use fake::{FAKE_OGG, FakeTts};
pub use google::GoogleTts;
pub use provider::{Audio, CloudVoice, SynthFuture, TtsError, TtsProvider};
pub(crate) use store::SCHEMA;

use crate::billing::gate::require_pro;
use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, now_ms};

/// The longest line read in one call (the dashboard's own limit is 500; alerts default to 200).
pub const MAX_CHARS: usize = 300;
/// Characters per subscription per UTC calendar month.
pub const CHARS: Limit = Limit { name: "tts_chars", period: Period::Month, limit: 20_000 };
/// Requests per subscription per minute: a request rate, not a quota, so not in `quotas`.
pub const SUB_RATE: Limit = Limit { name: "tts_requests", period: Period::Minute, limit: 20 };
/// Requests per minute from everyone together.
pub const GLOBAL_PER_MINUTE: u32 = 60;
/// `TTS_GLOBAL_MONTHLY_CHARS` when unset: safely under Google's 1M free characters.
pub const DEFAULT_GLOBAL_MONTHLY: i64 = 800_000;
/// The cap can never be set above Google's free tier.
pub const MAX_GLOBAL_MONTHLY: i64 = 1_000_000;
/// One provider call, start to end: under the engine's 8 s, so the relay answers (and gives
/// the characters back) before the PC gives up and reads with its own voice.
const SYNTH_TIMEOUT: Duration = Duration::from_secs(7);

/// The voice provider and the relay-wide limits, shared by every request.
#[derive(Debug)]
pub(crate) struct Runtime {
    provider: Option<Arc<dyn TtsProvider>>,
    global_monthly: i64,
    /// (UTC minute, requests in it), relay-wide.
    minute: Mutex<(i64, u32)>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime { provider: None, global_monthly: DEFAULT_GLOBAL_MONTHLY, minute: Mutex::new((0, 0)) }
    }
}

impl Runtime {
    pub(crate) fn new(provider: Option<Arc<dyn TtsProvider>>, global_monthly: i64) -> Runtime {
        Runtime { provider, global_monthly: global_monthly.clamp(0, MAX_GLOBAL_MONTHLY), ..Runtime::default() }
    }

    /// One more request this minute, if the relay-wide rate allows it.
    fn take_minute(&self, now: i64) -> bool {
        let minute = now.div_euclid(60_000);
        let mut m = self.minute.lock().unwrap_or_else(|p| p.into_inner());
        if m.0 != minute {
            *m = (minute, 0);
        }
        if m.1 >= GLOBAL_PER_MINUTE {
            return false;
        }
        m.1 += 1;
        true
    }
}

/// `TTS_GLOBAL_MONTHLY_CHARS`: unset or unreadable is the default; above Google's free tier
/// is brought down to it.
pub fn global_cap_from_env() -> i64 {
    parse_cap(std::env::var("TTS_GLOBAL_MONTHLY_CHARS").ok().as_deref())
}

pub(crate) fn parse_cap(raw: Option<&str>) -> i64 {
    let v = raw.unwrap_or("").trim().replace(['_', ','], "");
    if v.is_empty() {
        return DEFAULT_GLOBAL_MONTHLY;
    }
    match v.parse::<i64>() {
        Ok(n) if (0..=MAX_GLOBAL_MONTHLY).contains(&n) => n,
        Ok(n) if n > MAX_GLOBAL_MONTHLY => {
            tracing::warn!(set = n, used = MAX_GLOBAL_MONTHLY, "TTS_GLOBAL_MONTHLY_CHARS is above Google's free tier");
            MAX_GLOBAL_MONTHLY
        }
        _ => {
            tracing::warn!("TTS_GLOBAL_MONTHLY_CHARS isn't a whole number from 0 up; using the default");
            DEFAULT_GLOBAL_MONTHLY
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/v1/pro/tts", post(speak)).route("/v1/pro/tts/voices", get(voices))
}

/// `features.tts`: true only with a voice provider (`GOOGLE_TTS_KEY` set, or a fake in tests).
pub(crate) fn available(st: &AppState) -> bool {
    st.tts.provider.is_some()
}

/// Its per-subscription counters, listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[CHARS];

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn unavailable(message: &str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "tts_unavailable", message)
        .hint("Ferrocast reads it with this PC's own voice instead. Nothing to do.")
}

fn not_configured() -> ApiError {
    unavailable("Ferrocast voices aren't set up on this relay yet.")
}

fn resting(now: i64) -> ApiError {
    let reset = Period::Month.reset_at(now);
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "tts_unavailable", "Ferrocast voices are resting until next month.")
        .hint(format!(
            "They're back on {} (UTC). Until then Ferrocast reads with this PC's own voice.",
            crate::billing::date(reset)
        ))
        .header(HeaderName::from_static("x-quota-reset"), reset.to_string())
        .header(header::RETRY_AFTER, u64::try_from(reset - now).unwrap_or(0).div_ceil(1000).to_string())
}

fn busy(now: i64) -> ApiError {
    let reset = Period::Minute.reset_at(now);
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "tts_rate",
        "Too many voice requests right now. Try again in a minute.",
    )
    .hint("Ferrocast reads it with this PC's own voice meanwhile.")
    .header(HeaderName::from_static("x-quota-reset"), reset.to_string())
    .header(header::RETRY_AFTER, u64::try_from(reset - now).unwrap_or(0).div_ceil(1000).max(1).to_string())
}

fn too_long() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_long",
        format!("Ferrocast voices read up to {MAX_CHARS} characters at a time."),
    )
    .hint("Lower “Longest message” in Settings › Text-to-speech, or let this PC's voice read longer ones.")
}

fn voice_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "voice_not_found", "That Ferrocast voice doesn't exist.")
        .hint("Pick a voice again in Settings › Text-to-speech.")
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", message)
}

/// A JSON body, read as bytes so the Pro gate answers before the body is judged.
#[derive(Debug)]
struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                too_long()
            } else {
                bad_request("The request body couldn't be read.")
            }
        })
    }
}

#[derive(Debug, Deserialize)]
struct SpeakIn {
    text: String,
    voice: String,
}

fn parse(headers: &HeaderMap, body: &[u8]) -> Result<SpeakIn, ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if !json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send the text as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<SpeakIn>(body).map_err(|e| {
        let what = if e.is_syntax() || e.is_eof() { "isn't JSON" } else { "needs text and voice" };
        bad_request(format!("The request {what}."))
    })
}

/// Control and direction characters out, trimmed. The engine cleaned it already; this is
/// what the relay sends on, whoever called.
fn clean(s: &str) -> String {
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    s.chars().map(|c| if bad(c) { ' ' } else { c }).collect::<String>().trim().to_string()
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// `GET /v1/pro/tts/voices` → `{ voices, usage, resting, max_chars }`.
async fn voices(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let provider = st.tts.provider.clone().ok_or_else(not_configured)?;
    let now = now_ms();
    let usage = st.db.usage(&ctx.sub_id, &CHARS).map_err(ApiError::db)?;
    let global = st.db.tts_global_used(now).map_err(ApiError::db)?;
    let mut res = axum::Json(json!({
        "voices": provider.voices(),
        "usage": usage,
        "resting": global >= st.tts.global_monthly,
        "max_chars": MAX_CHARS,
    }))
    .into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// `POST /v1/pro/tts {text, voice}` → `audio/ogg`.
async fn speak(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let ctx = require_pro(&st, &headers).await?;
        let provider = st.tts.provider.clone().ok_or_else(not_configured)?;
        let req = parse(&headers, &body)?;
        let text = clean(&req.text);
        if text.is_empty() {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "empty_text", "There's nothing to read."));
        }
        let chars = text.chars().count();
        if chars > MAX_CHARS {
            return Err(too_long());
        }
        if !provider.voices().iter().any(|v| v.id == req.voice) {
            return Err(voice_not_found());
        }
        let now = now_ms();
        let n = i64::try_from(chars).unwrap_or(i64::MAX);
        // Request rates first (cheap, nothing to give back), then the subscription's month,
        // then the relay-wide cap that keeps Google free. Google is only called once all pass.
        match st.db.try_take(&ctx.sub_id, SUB_RATE.name, SUB_RATE.period, 1, SUB_RATE.limit) {
            Ok(()) => {}
            Err(TakeErr::Over(_)) => return Err(busy(now)),
            Err(TakeErr::Db(e)) => return Err(ApiError::db(e)),
        }
        if !st.tts.take_minute(now) {
            return Err(busy(now));
        }
        st.db
            .try_take(&ctx.sub_id, CHARS.name, CHARS.period, n, CHARS.limit)
            .map_err(|e| e.into_api("tts_quota", "You've used this month's Ferrocast voice characters."))?;
        let give_back_sub = || {
            if let Err(e) = st.db.give_back(&ctx.sub_id, CHARS.name, CHARS.period, n) {
                tracing::warn!(error = %e, "couldn't give back TTS characters");
            }
        };
        let month = match st.db.tts_take_global(n, st.tts.global_monthly, now) {
            Ok(Some(m)) => m,
            Ok(None) => {
                give_back_sub();
                tracing::warn!("the relay-wide TTS cap is reached: Google isn't called until next month");
                return Err(resting(now));
            }
            Err(e) => {
                give_back_sub();
                return Err(ApiError::db(e));
            }
        };
        let result = match tokio::time::timeout(SYNTH_TIMEOUT, provider.synthesize(&text, &req.voice)).await {
            Ok(r) => r,
            Err(_) => Err(TtsError::Unavailable("timed out".into())),
        };
        let audio = match result {
            Ok(a) => a,
            Err(e) => {
                give_back_sub();
                // Google refused before doing the work (its quota, the voice, the length): not
                // billed, so the relay-wide count goes back too. A timeout or a 5xx may have
                // been billed, so it stays counted (the safe side).
                if matches!(e, TtsError::Quota | TtsError::BadVoice | TtsError::TooLong)
                    && let Err(err) = st.db.tts_give_back_global(&month, n)
                {
                    tracing::warn!(error = %err, "couldn't give back relay-wide TTS characters");
                }
                tracing::warn!(error = %e, "the voice provider failed");
                return Err(match e {
                    TtsError::Quota => unavailable("Ferrocast voices are busy right now.")
                        .header(header::RETRY_AFTER, "60".to_string()),
                    TtsError::BadVoice => voice_not_found(),
                    TtsError::TooLong => too_long(),
                    TtsError::Unavailable(_) => unavailable("Ferrocast voices can't answer right now."),
                });
            }
        };
        let used = st.db.quota_used(&ctx.sub_id, CHARS.name, CHARS.period).unwrap_or(0);
        let mut res = audio.bytes.into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(audio.mime));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        for (name, value) in [
            ("x-tts-chars-used", used),
            ("x-tts-chars-limit", CHARS.limit),
            ("x-quota-reset", CHARS.period.reset_at(now)),
        ] {
            if let Ok(v) = HeaderValue::from_str(&value.to_string()) {
                h.insert(HeaderName::from_static(name), v);
            }
        }
        Ok::<_, ApiError>(res)
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}
