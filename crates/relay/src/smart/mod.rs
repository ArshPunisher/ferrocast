//! Ferrocast Pro smart features (1.4; API.md §9.10): smart moderation suggestions that read
//! Hinglish, smart replies to chat questions from the streamer's own approved facts, and a
//! written stream recap. The PC sends the text; the relay asks a language model on a **free
//! tier** (`provider`) and checks its answer (`prompts`); the app decides what happens.
//! Never a ban on its own: the app only suggests, and acts alone (if the streamer allows it)
//! with short timeouts it can undo.
//!
//! **Off until the owner sets a key** (`deploy/pro-keys.sh --ai`): without one every route
//! answers 503 `smart_off` and `features.smart` is false, so the app hides the switches.
//!
//! **Hard caps, so a free tier can't run out under someone else or ever bill:**
//! - per subscription a UTC day: [`MODERATION`] checks (up to [`MAX_MESSAGES`] messages
//!   each), [`REPLIES`] replies, [`RECAPS`] recaps, and [`SUB_RATE`] calls a minute;
//! - relay-wide: `SMART_DAILY_CALLS` model calls a UTC day (default [`DEFAULT_DAILY_CALLS`],
//!   never above [`MAX_DAILY_CALLS`]) and [`GLOBAL_PER_MINUTE`] a minute. Once the day's
//!   calls are used, or the provider says its free allowance is, nothing is asked of it
//!   until midnight UTC.
//! - **A backup**: with both Cloudflare and Gemini set, Gemini answers once Cloudflare rests
//!   (or is down), up to `SMART_BACKUP_DAILY_CALLS` a UTC day (default
//!   [`DEFAULT_BACKUP_DAILY_CALLS`]). Only when both rest does a call get 503 `smart_resting`.
//! - Each call's input and answer are bounded (message count and length, `max_tokens`).
//!
//! Stored: those counters only. Never the messages, the facts, the questions or the answers
//! (R-51), and nothing of them is logged.

pub mod prompts;
pub mod provider;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

pub use provider::{Cloudflare, FakeSmart, Gemini, Prompt, SmartError, SmartProvider};

use crate::billing::gate::{ProCtx, require_pro};
use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, now_ms};

/// Smart moderation checks per subscription per UTC day (each up to 20 messages).
pub const MODERATION: Limit = Limit { name: "smart_moderation", period: Period::Day, limit: 30 };
/// Smart replies per subscription per UTC day.
pub const REPLIES: Limit = Limit { name: "smart_replies", period: Period::Day, limit: 20 };
/// Stream recaps per subscription per UTC day.
pub const RECAPS: Limit = Limit { name: "smart_recaps", period: Period::Day, limit: 3 };
/// Calls per subscription per minute: a request rate, not a quota.
pub const SUB_RATE: Limit = Limit { name: "smart_requests", period: Period::Minute, limit: 10 };
/// Relay-wide model calls a UTC day when `SMART_DAILY_CALLS` is unset. With the inputs and
/// answers bounded as they are, 140 calls stay under Cloudflare's 10,000 free Neurons a day
/// even in the worst case (about 70 each; a usual call is under 30).
pub const DEFAULT_DAILY_CALLS: i64 = 140;
/// The setting can't go above this (Gemini's free tier allows about 1,000 a day).
pub const MAX_DAILY_CALLS: i64 = 1_000;
/// Backup calls a UTC day when `SMART_BACKUP_DAILY_CALLS` is unset: half of Gemini's free
/// tier (about 1,000 a day), so the free allowance never runs out under us.
pub const DEFAULT_BACKUP_DAILY_CALLS: i64 = 500;
/// Relay-wide calls a minute (under the free tiers' own rate limits).
pub const GLOBAL_PER_MINUTE: u32 = 12;
/// Messages in one moderation check, and the longest one sent.
pub const MAX_MESSAGES: usize = 20;
pub const MAX_TEXT: usize = 300;
/// The streamer's extra moderation rules, the reply facts, a question.
pub const MAX_RULES: usize = 500;
pub const MAX_ABOUT: usize = 2_000;
pub const MAX_QUESTION: usize = 300;
/// One model call, start to end.
const CALL_TIMEOUT: Duration = Duration::from_secs(14);
/// The relay-wide day counter's owner in `quota_usage`.
const RELAY_KEY: &str = "*smart";
const RELAY_CALLS: &str = "smart_calls";
const BACKUP_CALLS: &str = "smart_backup_calls";

/// The provider and the relay-wide limits.
#[derive(Debug)]
pub(crate) struct Runtime {
    provider: Option<Arc<dyn SmartProvider>>,
    daily: i64,
    /// (UTC minute, calls in it), relay-wide.
    minute: Mutex<(i64, u32)>,
    /// No call until then (ms): the provider said its free allowance is used up.
    resting_until: AtomicI64,
    /// Asked when the provider rests or is down (Gemini behind Cloudflare), and its own day.
    backup: Option<Arc<dyn SmartProvider>>,
    backup_daily: i64,
    backup_resting_until: AtomicI64,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime {
            provider: None,
            daily: DEFAULT_DAILY_CALLS,
            minute: Mutex::new((0, 0)),
            resting_until: AtomicI64::new(0),
            backup: None,
            backup_daily: DEFAULT_BACKUP_DAILY_CALLS,
            backup_resting_until: AtomicI64::new(0),
        }
    }
}

impl Runtime {
    pub(crate) fn new(provider: Option<Arc<dyn SmartProvider>>, daily: i64) -> Runtime {
        Runtime { provider, daily: daily.clamp(0, MAX_DAILY_CALLS), ..Runtime::default() }
    }

    /// Adds the provider asked once the first one rests or is down.
    pub(crate) fn set_backup(&mut self, backup: Option<Arc<dyn SmartProvider>>, daily: i64) {
        self.backup = backup;
        self.backup_daily = daily.clamp(0, MAX_DAILY_CALLS);
    }

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

    /// Nothing can answer until midnight UTC: the provider rests, and the backup (if any) too.
    fn resting(&self, now: i64) -> bool {
        self.primary_resting(now) && (self.backup.is_none() || self.backup_resting(now))
    }

    fn primary_resting(&self, now: i64) -> bool {
        now < self.resting_until.load(Ordering::Relaxed)
    }

    fn backup_resting(&self, now: i64) -> bool {
        now < self.backup_resting_until.load(Ordering::Relaxed)
    }

    /// Which one answers now (`GET /v1/pro/smart`).
    fn answering(&self, now: i64) -> Option<&'static str> {
        match &self.backup {
            Some(b) if self.primary_resting(now) => Some(b.name()),
            _ => self.provider.as_ref().map(|p| p.name()),
        }
    }

    fn rest_until(&self, until: i64) {
        self.resting_until.fetch_max(until, Ordering::Relaxed);
    }
}

/// The provider from the environment: Cloudflare when both its settings are there, else
/// Gemini with its key, else none (smart features off).
pub fn provider_from_env() -> Option<Arc<dyn SmartProvider>> {
    if let Some(c) = Cloudflare::from_env() {
        return Some(Arc::new(c));
    }
    Gemini::from_env().map(|g| Arc::new(g) as Arc<dyn SmartProvider>)
}

/// The backup: Gemini, when Cloudflare is the provider and a Gemini key is set too.
pub fn backup_from_env() -> Option<Arc<dyn SmartProvider>> {
    Cloudflare::from_env()?;
    Gemini::from_env().map(|g| Arc::new(g) as Arc<dyn SmartProvider>)
}

/// `SMART_BACKUP_DAILY_CALLS`, read like `SMART_DAILY_CALLS` with its own default.
pub fn backup_daily_from_env() -> i64 {
    parse_backup_daily(std::env::var("SMART_BACKUP_DAILY_CALLS").ok().as_deref())
}

pub(crate) fn parse_backup_daily(raw: Option<&str>) -> i64 {
    parse_cap("SMART_BACKUP_DAILY_CALLS", raw, DEFAULT_BACKUP_DAILY_CALLS)
}

/// `SMART_DAILY_CALLS`: unset or unreadable is the default; above the ceiling is brought down.
pub fn daily_from_env() -> i64 {
    parse_daily(std::env::var("SMART_DAILY_CALLS").ok().as_deref())
}

pub(crate) fn parse_daily(raw: Option<&str>) -> i64 {
    parse_cap("SMART_DAILY_CALLS", raw, DEFAULT_DAILY_CALLS)
}

/// A daily call cap setting: unset or unreadable is `default`; above the ceiling is brought down.
fn parse_cap(name: &str, raw: Option<&str>, default: i64) -> i64 {
    let v = raw.unwrap_or("").trim().replace(['_', ','], "");
    if v.is_empty() {
        return default;
    }
    match v.parse::<i64>() {
        Ok(n) if (0..=MAX_DAILY_CALLS).contains(&n) => n,
        Ok(n) if n > MAX_DAILY_CALLS => {
            tracing::warn!(setting = name, set = n, used = MAX_DAILY_CALLS, "a smart call cap is above the ceiling");
            MAX_DAILY_CALLS
        }
        _ => {
            tracing::warn!(setting = name, "a smart call cap isn't a whole number from 0 up; using the default");
            default
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/smart", get(status))
        .route("/v1/pro/smart/moderate", post(moderate))
        .route("/v1/pro/smart/reply", post(reply))
        .route("/v1/pro/smart/recap", post(recap))
}

/// `features.smart`: a provider is set up.
pub(crate) fn available(st: &AppState) -> bool {
    st.smart.provider.is_some()
}

/// Listed in `/v1/pro/status` `quotas`.
pub(crate) const QUOTAS: &[Limit] = &[MODERATION, REPLIES, RECAPS];

/// For the Control Center: (calls used today relay-wide, the cap, resting, provider).
pub(crate) fn admin_view(st: &AppState) -> Value {
    let now = now_ms();
    json!({
        "provider": st.smart.provider.as_ref().map(|p| p.name()),
        "calls_today": st.db.quota_used(RELAY_KEY, RELAY_CALLS, Period::Day).unwrap_or(0),
        "daily_cap": st.smart.daily,
        "backup": st.smart.backup.as_ref().map(|p| p.name()),
        "backup_calls_today": st.db.quota_used(RELAY_KEY, BACKUP_CALLS, Period::Day).unwrap_or(0),
        "backup_daily_cap": st.smart.backup_daily,
        "resting": st.smart.resting(now),
        "resets_at": Period::Day.reset_at(now),
    })
}

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn off() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "smart_off",
        "Smart features aren't switched on on Ferrocast's servers yet.",
    )
    .hint("Nothing to do: everything else works as usual, and these turn on by themselves once they're set up.")
}

fn reset_headers(e: ApiError, reset: i64, now: i64) -> ApiError {
    e.header(HeaderName::from_static("x-quota-reset"), reset.to_string())
        .header(header::RETRY_AFTER, u64::try_from(reset - now).unwrap_or(0).div_ceil(1000).max(1).to_string())
}

fn resting(now: i64) -> ApiError {
    let reset = Period::Day.reset_at(now);
    reset_headers(
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "smart_resting", "Smart features are resting until tomorrow.")
            .hint("They're back at 00:00 UTC. Your own filters, commands and auto-replies keep working meanwhile."),
        reset,
        now,
    )
}

fn busy(now: i64) -> ApiError {
    reset_headers(
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, "smart_rate", "Too many smart requests right now.")
            .hint("Ferrocast tries again in a minute by itself."),
        Period::Minute.reset_at(now),
        now,
    )
}

fn unavailable() -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, "smart_unavailable", "The smart features' service didn't answer properly.")
        .hint("Try again in a minute. Your own filters, commands and auto-replies keep working.")
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

/// A JSON body, read as bytes so the gate answers before the body is judged.
#[derive(Debug)]
struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "That's too much text in one request.")
                    .hint(format!("Send at most {MAX_MESSAGES} messages of {MAX_TEXT} characters."))
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid", "The request body couldn't be read.")
            }
        })
    }
}

fn parse<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, ApiError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if !json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send it as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<T>(body).map_err(|e| {
        let what = if e.is_syntax() || e.is_eof() { "isn't JSON" } else { "has a missing or wrong field" };
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", format!("The request {what}: {e}"))
    })
}

/// Control and direction characters out, trimmed.
fn clean(s: &str) -> String {
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    s.chars().map(|c| if bad(c) { ' ' } else { c }).collect::<String>().trim().to_string()
}

// ── The gate and one call ────────────────────────────────────────────────────────────

/// Pro, then a provider.
async fn gate(st: &AppState, headers: &HeaderMap) -> Result<(ProCtx, Arc<dyn SmartProvider>), ApiError> {
    let ctx = require_pro(st, headers).await?;
    let provider = st.smart.provider.clone().ok_or_else(off)?;
    Ok((ctx, provider))
}

/// One model call for `ctx` against `limit`: the rates, the subscription's day, the relay's
/// day, then the provider. A failed call gives the subscription's count back; the relay's
/// stays (the provider may have counted it).
async fn call(
    st: &AppState,
    ctx: &ProCtx,
    provider: &dyn SmartProvider,
    limit: Limit,
    used_up: &'static str,
    prompt: &Prompt,
) -> Result<String, ApiError> {
    let now = now_ms();
    match st.db.try_take(&ctx.sub_id, SUB_RATE.name, SUB_RATE.period, 1, SUB_RATE.limit) {
        Ok(()) => {}
        Err(TakeErr::Over(_)) => return Err(busy(now)),
        Err(TakeErr::Db(e)) => return Err(ApiError::db(e)),
    }
    st.db.try_take(&ctx.sub_id, limit.name, limit.period, 1, limit.limit).map_err(|e| match e {
        TakeErr::Over(q) => q.into_api("smart_quota", used_up),
        TakeErr::Db(e) => ApiError::db(e),
    })?;
    let give_back = || {
        if let Err(e) = st.db.give_back(&ctx.sub_id, limit.name, limit.period, 1) {
            tracing::warn!(error = %e, "couldn't give back a smart call");
        }
    };
    if st.smart.resting(now) {
        give_back();
        return Err(resting(now));
    }
    if !st.smart.take_minute(now) {
        give_back();
        return Err(busy(now));
    }
    let reset = Period::Day.reset_at(now);
    let backup = st.smart.backup.clone();
    if !st.smart.primary_resting(now) {
        match st.db.try_take(RELAY_KEY, RELAY_CALLS, Period::Day, 1, st.smart.daily) {
            Ok(()) => match ask(provider, prompt).await {
                Ok(text) => return Ok(text),
                Err(SmartError::Quota) => st.smart.rest_until(reset),
                Err(SmartError::Unavailable(_)) if backup.is_none() => {
                    give_back();
                    return Err(unavailable());
                }
                Err(SmartError::Unavailable(_)) => {}
            },
            Err(TakeErr::Over(_)) => {
                st.smart.rest_until(reset);
                tracing::warn!("the relay-wide smart call cap is reached: the provider isn't asked until 00:00 UTC");
            }
            Err(TakeErr::Db(e)) => {
                give_back();
                return Err(ApiError::db(e));
            }
        }
    }
    // The provider rests (or was down just now): the backup answers, within its own day.
    let Some(backup) = backup else {
        give_back();
        return Err(resting(now));
    };
    if st.smart.backup_resting(now) {
        give_back();
        return Err(if st.smart.primary_resting(now) { resting(now) } else { unavailable() });
    }
    match st.db.try_take(RELAY_KEY, BACKUP_CALLS, Period::Day, 1, st.smart.backup_daily) {
        Ok(()) => {}
        Err(TakeErr::Over(_)) => {
            give_back();
            st.smart.backup_resting_until.fetch_max(reset, Ordering::Relaxed);
            tracing::warn!("the backup's smart call cap is reached: it rests until 00:00 UTC");
            return Err(if st.smart.primary_resting(now) { resting(now) } else { unavailable() });
        }
        Err(TakeErr::Db(e)) => {
            give_back();
            return Err(ApiError::db(e));
        }
    }
    match ask(backup.as_ref(), prompt).await {
        Ok(text) => Ok(text),
        Err(SmartError::Quota) => {
            give_back();
            st.smart.backup_resting_until.fetch_max(reset, Ordering::Relaxed);
            Err(if st.smart.primary_resting(now) { resting(now) } else { unavailable() })
        }
        Err(SmartError::Unavailable(_)) => {
            give_back();
            Err(unavailable())
        }
    }
}

/// One model call with the time limit; failures are logged without any of the text.
async fn ask(provider: &dyn SmartProvider, prompt: &Prompt) -> Result<String, SmartError> {
    let result = match tokio::time::timeout(CALL_TIMEOUT, provider.complete(prompt)).await {
        Ok(r) => r,
        Err(_) => Err(SmartError::Unavailable("timed out".into())),
    };
    if let Err(e) = &result {
        tracing::warn!(error = %e, provider = provider.name(), "the smart provider failed");
    }
    result
}

fn no_store(v: Value) -> Response {
    let mut res = axum::Json(v).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// `GET /v1/pro/smart` → `{provider, resting, quotas: {smart_moderation, smart_replies, smart_recaps}}`.
async fn status(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let (ctx, provider) = gate(&st, &headers).await?;
    let mut quotas = serde_json::Map::new();
    for l in QUOTAS {
        quotas.insert(l.name.into(), json!(st.db.usage(&ctx.sub_id, l).map_err(ApiError::db)?));
    }
    Ok(no_store(json!({
        "provider": st.smart.answering(now_ms()).unwrap_or(provider.name()),
        "resting": st.smart.resting(now_ms()),
        "max_messages": MAX_MESSAGES,
        "quotas": quotas,
    })))
}

#[derive(Debug, Deserialize)]
struct ModerateIn {
    messages: Vec<prompts::ModIn>,
    #[serde(default)]
    rules: Option<String>,
}

/// `POST /v1/pro/smart/moderate {messages: [{id, text, user?, first_time?}], rules?}` →
/// `{results: [{id, action, category, reason, confidence}]}`, one per message.
async fn moderate(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let (ctx, provider) = gate(&st, &headers).await?;
        let mut b: ModerateIn = parse(&headers, &body)?;
        if b.messages.is_empty() {
            return Err(invalid("messages: send at least one."));
        }
        if b.messages.len() > MAX_MESSAGES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_many_messages",
                format!("Send at most {MAX_MESSAGES} messages at a time."),
            )
            .hint("Split them over several requests."));
        }
        let mut ids = std::collections::HashSet::new();
        for (i, m) in b.messages.iter_mut().enumerate() {
            m.id = m.id.trim().to_string();
            if m.id.is_empty() || m.id.len() > 80 || !ids.insert(m.id.clone()) {
                return Err(invalid(format!("messages[{i}].id: 1 to 80 characters, each id once.")));
            }
            m.text = clean(&m.text).chars().take(MAX_TEXT).collect();
            if m.text.is_empty() {
                return Err(invalid(format!("messages[{i}].text: empty.")));
            }
            m.user = m.user.as_deref().map(|u| clean(u).chars().take(40).collect()).filter(|u: &String| !u.is_empty());
        }
        let rules = b.rules.as_deref().map(clean).filter(|r| !r.is_empty());
        if rules.as_ref().is_some_and(|r| r.chars().count() > MAX_RULES) {
            return Err(invalid(format!("rules: at most {MAX_RULES} characters.")));
        }
        let prompt = Prompt {
            system: prompts::MOD_SYSTEM.into(),
            user: prompts::mod_user(&b.messages, rules.as_deref()),
            max_tokens: 60 * u32::try_from(b.messages.len()).unwrap_or(20) + 40,
            json: true,
        };
        let raw =
            call(&st, &ctx, provider.as_ref(), MODERATION, "You've used today's smart moderation checks.", &prompt)
                .await?;
        let results = prompts::read_mod(&raw, &b.messages).ok_or_else(unavailable)?;
        Ok::<_, ApiError>(no_store(json!({ "results": results })))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

/// `POST /v1/pro/smart/reply {question, about, user?, channel?, title?, game?}` →
/// `{reply: "…" | null}` (null: the facts don't answer it, or it couldn't be sent safely).
async fn reply(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let (ctx, provider) = gate(&st, &headers).await?;
        let mut r: prompts::ReplyIn = parse(&headers, &body)?;
        r.question = clean(&r.question);
        r.about = r.about.trim().to_string();
        if r.question.is_empty() {
            return Err(invalid("question: empty."));
        }
        if r.question.chars().count() > MAX_QUESTION {
            return Err(invalid(format!("question: at most {MAX_QUESTION} characters.")));
        }
        if r.about.is_empty() || r.about.chars().count() > MAX_ABOUT {
            return Err(invalid(format!("about: your facts for replies, 1 to {MAX_ABOUT} characters."))
                .hint("Write them in Bot › Smart replies and approve them."));
        }
        let prompt =
            Prompt { system: prompts::reply_system(&r), user: prompts::reply_user(&r), max_tokens: 120, json: false };
        let raw = call(&st, &ctx, provider.as_ref(), REPLIES, "You've used today's smart replies.", &prompt).await?;
        Ok::<_, ApiError>(no_store(json!({ "reply": prompts::read_reply(&raw, &r.about) })))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

/// `POST /v1/pro/smart/recap {minutes, title?, game?, peak_viewers, follows, subs, messages,
/// chatters?, bits, tips, raids, thanks, previous?}` → `{summary}`.
async fn recap(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let (ctx, provider) = gate(&st, &headers).await?;
        let r: prompts::RecapIn = parse(&headers, &body)?;
        if r.minutes <= 0 && r.messages <= 0 && r.follows <= 0 {
            return Err(invalid("There's no stream to recap yet.").hint("Stream first, then ask for a recap."));
        }
        let prompt = Prompt {
            system: prompts::RECAP_SYSTEM.into(),
            user: prompts::recap_user(&r),
            max_tokens: 400,
            json: false,
        };
        let raw = call(&st, &ctx, provider.as_ref(), RECAPS, "You've used today's stream recaps.", &prompt).await?;
        let summary = prompts::read_recap(&raw).ok_or_else(unavailable)?;
        Ok::<_, ApiError>(no_store(json!({ "summary": summary })))
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}
