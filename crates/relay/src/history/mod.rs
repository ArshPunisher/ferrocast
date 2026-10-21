//! Ferrocast Pro F7, stream history (docs/plan/PRO.md F7, milestone M1; API.md §9.1).
//!
//! The desktop uploads each stream's totals when it ends (one row per platform, keyed by
//! the desktop's own `key`, so a retry is the same row) and a viewer sample every five
//! minutes while live. The dashboard reads it back as streams plus weekly totals, one
//! stream's viewer samples, and a CSV.
//!
//! Who keeps it (1.4, the owner's decision): a **free account** keeps the last
//! [`FREE_KEEP_DAYS`] days (rows keyed by the account id), **Pro** keeps [`PRO_KEEP_DAYS`]
//! (rows keyed by the subscription, shared by the account's PCs). A PC that isn't signed in
//! and has no Pro gets 403 `account_required`. History of a subscription whose Pro ended is
//! kept 30 days more (hourly housekeeping), and its export stays open during those 30 days
//! (R-04). Only counts are stored: never chat, names or viewers (R-51).

mod store;
#[cfg(test)]
mod tests;

use axum::Router;
use axum::extract::{FromRequest, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) use store::SCHEMA;
use store::{Sampled, Upserted};

use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, authed, now_ms};

pub const PLATFORMS: [&str; 3] = ["twitch", "youtube", "kick"];
/// Samples in one call.
pub const MAX_SAMPLES: usize = 100;
/// An ended stream can still be corrected (a late upload, a retry) for a day.
pub const LOCK_AFTER_MS: i64 = 86_400_000;
/// History stays this long after Pro ends.
pub const KEEP_AFTER_LAPSE_MS: i64 = 30 * 86_400_000;
/// A free account's history: the last 30 days.
pub const FREE_KEEP_DAYS: i64 = 30;
/// Pro's history: the last year.
pub const PRO_KEEP_DAYS: i64 = 365;
const DAY_MS: i64 = 86_400_000;
/// Viewer samples are kept a year and a bit (the longest range the dashboard shows).
pub const SAMPLE_KEEP_MS: i64 = 400 * 86_400_000;
/// The longest range one request may ask for.
pub const MAX_RANGE_MS: i64 = 400 * 86_400_000;
/// Without `from`: the last four weeks.
pub const DEFAULT_RANGE_MS: i64 = 28 * 86_400_000;
/// Streams in one answer (newest first); `truncated` says when there were more.
pub const MAX_LISTED: usize = 2_000;
/// The longest stream accepted (subathons run for days).
const MAX_STREAM_MS: i64 = 31 * 86_400_000;
const MAX_COUNT: i64 = 100_000_000;
const WEEK_MS: i64 = 7 * 86_400_000;

/// Requests per subscription per minute, every history route together. A request rate,
/// not a quota a streamer uses up, so it isn't listed in `/v1/pro/status`.
pub const RATE: Limit = Limit { name: "history_requests", period: Period::Minute, limit: 60 };

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/history", get(list))
        .route("/v1/pro/history/streams", post(upsert_stream))
        .route("/v1/pro/history/samples", post(add_samples).get(get_samples))
        .route("/v1/pro/history/export.csv", get(export_csv))
}

/// `features.history`: built, and needs nothing configured beyond the database.
pub(crate) fn available(_st: &AppState) -> bool {
    true
}

/// No counter to show: [`RATE`] is a request rate.
pub(crate) const QUOTAS: &[Limit] = &[];

// ── Shapes ───────────────────────────────────────────────────────────────────────────

/// Money in its own currency, minor units (never converted, R-32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub currency: String,
    pub minor: i64,
}

/// A platform's own currency this stream: Bits on Twitch, KICKs on Kick, Jewels on YouTube.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformAmount {
    pub unit: String,
    pub amount: i64,
}

/// `POST /v1/pro/history/streams`.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamIn {
    pub platform: String,
    pub key: String,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub peak_viewers: i64,
    #[serde(default)]
    pub avg_viewers: i64,
    #[serde(default)]
    pub chat_lines: i64,
    #[serde(default)]
    pub chatters: i64,
    #[serde(default)]
    pub new_chatters: i64,
    #[serde(default)]
    pub follows: i64,
    #[serde(default)]
    pub subs: i64,
    #[serde(default)]
    pub tips: Vec<Money>,
    #[serde(default)]
    pub platform_value: Vec<PlatformAmount>,
}

/// One stream as the dashboard gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamOut {
    pub platform: String,
    pub key: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub peak_viewers: i64,
    pub avg_viewers: i64,
    pub chat_lines: i64,
    pub chatters: i64,
    pub new_chatters: i64,
    pub follows: i64,
    pub subs: i64,
    pub tips: Vec<Money>,
    pub platform_value: Vec<PlatformAmount>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct SamplesIn {
    platform: String,
    key: String,
    samples: Vec<SampleIn>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct SampleIn {
    at: i64,
    viewers: i64,
}

/// One UTC week (Monday 00:00) of streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Week {
    pub week_start: i64,
    /// Streams that started this week (one per platform when simulcasting).
    pub streams: i64,
    pub by_platform: std::collections::BTreeMap<String, i64>,
    /// Minutes live, all streams together.
    pub minutes: i64,
    pub peak_viewers: i64,
    /// Average viewers, weighted by each stream's length.
    pub avg_viewers: i64,
    pub follows: i64,
    pub subs: i64,
    pub chat_lines: i64,
}

// ── Errors ───────────────────────────────────────────────────────────────────────────

fn invalid(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", message)
}

/// 403 `account_required`: no Pro and not signed in to a Ferrocast account on this PC.
pub(crate) fn account_required() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "account_required",
        "Stream history needs a free Ferrocast account on this PC.",
    )
    .hint("Sign in to Ferrocast on this PC (the account bar in the sidebar). Free accounts keep 30 days of stream history, Pro keeps a year.")
}

fn locked() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "ended_stream_locked", "That stream ended over a day ago and can't change now.")
        .hint("Nothing to do: its history is saved as it was.")
}

/// A JSON request body, read as bytes so the Pro gate answers before the body is judged.
#[derive(Debug)]
struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    format!("History uploads can be up to {} KB.", crate::BODY_LIMIT / 1024),
                )
            } else {
                bad_request("The request body couldn't be read.")
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
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send history as JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<T>(body).map_err(|e| {
        let what = if e.is_syntax() || e.is_eof() { "isn't JSON" } else { "has a missing or wrong field" };
        bad_request(format!("The request {what}: {e}"))
    })
}

/// Whose history a request is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owner {
    pub install: String,
    /// Where this PC's streams are written: the Pro subscription, else the free account.
    pub key: String,
    pub pro: bool,
    /// Every key whose history this PC may read: its subscription (lapsed too, while kept)
    /// and its account (what it kept while free).
    pub reads: Vec<String>,
}

impl Owner {
    pub(crate) fn keep_days(&self) -> i64 {
        if self.pro { PRO_KEEP_DAYS } else { FREE_KEEP_DAYS }
    }

    /// The oldest stream start this owner sees and may upload.
    pub(crate) fn earliest(&self, now: i64) -> i64 {
        now - self.keep_days() * DAY_MS
    }
}

/// Pro (the install's own subscription or its account's) or a signed-in free account.
pub(crate) fn owner_of(st: &AppState, install: String) -> Result<Owner, ApiError> {
    let now = now_ms();
    let sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?;
    let account = st.db.account_for_install(&install, now).map_err(ApiError::db)?;
    let mut reads: Vec<String> = sub.iter().map(|s| s.id.clone()).collect();
    reads.extend(account.iter().cloned());
    match (sub.filter(|s| crate::billing::access::is_pro(&s.access(), now)), account) {
        (Some(s), _) => Ok(Owner { install, key: s.id, pro: true, reads }),
        (None, Some(a)) => Ok(Owner { install, key: a, pro: false, reads }),
        (None, None) => Err(account_required()),
    }
}

/// The gate, then the owner's request rate.
async fn gate(st: &AppState, headers: &HeaderMap) -> Result<Owner, ApiError> {
    let install = authed(st, headers)?;
    let owner = owner_of(st, install)?;
    take_rate(st, &owner.key)?;
    Ok(owner)
}

fn take_rate(st: &AppState, sub: &str) -> Result<(), ApiError> {
    match st.db.try_take(sub, RATE.name, RATE.period, 1, RATE.limit) {
        Ok(()) => Ok(()),
        Err(TakeErr::Over(q)) => {
            let wait_s = u64::try_from(q.reset_at - now_ms()).unwrap_or(0).div_ceil(1000).max(1);
            Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "history_rate",
                format!("Too many history requests: {} a minute at most.", RATE.limit),
            )
            .hint("Wait a moment and try again. Ferrocast keeps unsent history and retries by itself.")
            .header(HeaderName::from_static("x-quota-reset"), q.reset_at.to_string())
            .header(header::RETRY_AFTER, wait_s.to_string()))
        }
        Err(TakeErr::Db(e)) => Err(ApiError::db(e)),
    }
}

// ── Validation ───────────────────────────────────────────────────────────────────────

fn platform_ok(p: &str) -> bool {
    PLATFORMS.contains(&p)
}

/// Keys are the desktop's own ids for a stream (`twitch:1792411200000`).
fn key_ok(k: &str) -> bool {
    (1..=64).contains(&k.len())
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.'))
}

/// Text from the platform (a title, a category): control and direction characters out,
/// trimmed, at most `max` characters. `None` when nothing is left.
fn clean(s: Option<&str>, max: usize) -> Option<String> {
    let s = s?;
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    let t: String = s.chars().filter(|c| !bad(*c)).collect::<String>().trim().chars().take(max).collect();
    (!t.is_empty()).then_some(t)
}

fn check_stream(mut s: StreamIn, now: i64, owner: &Owner) -> Result<StreamIn, ApiError> {
    let bad = |m: String| invalid("invalid_stream", m);
    if !platform_ok(&s.platform) {
        return Err(bad("platform: use twitch, youtube or kick.".into()));
    }
    if !key_ok(&s.key) {
        return Err(bad("key: 1 to 64 letters, digits or : _ - . characters.".into()));
    }
    if s.started_at <= 0 || s.started_at > now + 10 * 60_000 {
        return Err(bad("started_at: a time in milliseconds, not in the future.".into()));
    }
    if s.started_at < owner.earliest(now) {
        let what = if owner.pro { "a year ago on Pro" } else { "30 days ago on a free account" };
        return Err(bad(format!("started_at: streams that started more than {what} aren't kept.")));
    }
    if let Some(e) = s.ended_at
        && (e < s.started_at || e - s.started_at > MAX_STREAM_MS || e > now + 10 * 60_000)
    {
        return Err(bad("ended_at: after started_at, within 31 days of it, and not in the future.".into()));
    }
    let counts = [
        ("peak_viewers", s.peak_viewers),
        ("avg_viewers", s.avg_viewers),
        ("chat_lines", s.chat_lines),
        ("chatters", s.chatters),
        ("new_chatters", s.new_chatters),
        ("follows", s.follows),
        ("subs", s.subs),
    ];
    if let Some((name, _)) = counts.iter().find(|(_, v)| !(0..=MAX_COUNT).contains(v)) {
        return Err(bad(format!("{name}: a whole number from 0 to {MAX_COUNT}.")));
    }
    if s.avg_viewers > s.peak_viewers {
        return Err(bad("avg_viewers: can't be more than peak_viewers.".into()));
    }
    if s.new_chatters > s.chatters {
        return Err(bad("new_chatters: can't be more than chatters.".into()));
    }
    if s.tips.len() > 20 {
        return Err(bad("tips: at most 20 currencies.".into()));
    }
    for (i, m) in s.tips.iter().enumerate() {
        if m.currency.len() != 3 || !m.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(bad(format!("tips[{i}].currency: a three-letter code like INR or USD.")));
        }
        if !(0..=1_000_000_000_000).contains(&m.minor) {
            return Err(bad(format!("tips[{i}].minor: a whole amount in minor units, 0 or more.")));
        }
    }
    let mut seen = std::collections::HashSet::new();
    if s.tips.iter().any(|m| !seen.insert(m.currency.clone())) {
        return Err(bad("tips: one entry per currency.".into()));
    }
    if s.platform_value.len() > 5 {
        return Err(bad("platform_value: at most 5 units.".into()));
    }
    for (i, v) in s.platform_value.iter().enumerate() {
        let unit_ok = (1..=24).contains(&v.unit.len()) && v.unit.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        if !unit_ok {
            return Err(bad(format!("platform_value[{i}].unit: a short name like bits, kicks or jewels.")));
        }
        if !(0..=1_000_000_000_000).contains(&v.amount) {
            return Err(bad(format!("platform_value[{i}].amount: a whole number, 0 or more.")));
        }
    }
    s.title = clean(s.title.as_deref(), 300);
    s.category = clean(s.category.as_deref(), 120);
    Ok(s)
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// `POST /v1/pro/history/streams`: 201 when new, 200 when it replaced the totals.
async fn upsert_stream(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let ctx = gate(&st, &headers).await?;
        let now = now_ms();
        let s = check_stream(parse::<StreamIn>(&headers, &body)?, now, &ctx)?;
        let done = st.db.upsert_stream(&ctx.key, &ctx.install, &s, now).map_err(ApiError::db)?;
        let status = match done {
            Upserted::Created => StatusCode::CREATED,
            Upserted::Updated => StatusCode::OK,
            Upserted::Locked => return Err(locked()),
        };
        let saved = st
            .db
            .list_streams(&ctx.key, s.started_at, s.started_at + 1, Some(&s.platform), MAX_LISTED)
            .map_err(ApiError::db)?
            .into_iter()
            .find(|x| x.key == s.key);
        Ok::<_, ApiError>((status, axum::Json(json!({ "stream": saved }))).into_response())
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

/// `POST /v1/pro/history/samples {platform, key, samples: [{at, viewers}]}` (1 to 100).
async fn add_samples(State(st): State<AppState>, headers: HeaderMap, RawBody(body): RawBody) -> Response {
    let run = async {
        let ctx = gate(&st, &headers).await?;
        let b = parse::<SamplesIn>(&headers, &body)?;
        if b.samples.len() > MAX_SAMPLES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_many_samples",
                format!("Send at most {MAX_SAMPLES} samples at a time."),
            )
            .hint("Split them over several requests."));
        }
        let bad = |m: String| invalid("invalid_sample", m);
        if b.samples.is_empty() {
            return Err(bad("samples: send at least one.".into()));
        }
        if !platform_ok(&b.platform) {
            return Err(bad("platform: use twitch, youtube or kick.".into()));
        }
        if !key_ok(&b.key) {
            return Err(bad("key: 1 to 64 letters, digits or : _ - . characters.".into()));
        }
        if let Some((i, _)) = b.samples.iter().enumerate().find(|(_, s)| !(0..=MAX_COUNT).contains(&s.viewers)) {
            return Err(bad(format!("samples[{i}].viewers: a whole number from 0 to {MAX_COUNT}.")));
        }
        let list: Vec<(i64, i64)> = b.samples.iter().map(|s| (s.at, s.viewers)).collect();
        // The stream is under the key it was started with (free, then Pro mid-stream).
        let mut done = Sampled::NotFound;
        for key in std::iter::once(&ctx.key).chain(ctx.reads.iter().filter(|k| **k != ctx.key)) {
            done = st.db.add_samples(key, &b.platform, &b.key, &list, now_ms()).map_err(ApiError::db)?;
            if done != Sampled::NotFound {
                break;
            }
        }
        match done {
            Sampled::Saved(n) => Ok(axum::Json(json!({ "saved": n })).into_response()),
            Sampled::NotFound => Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "stream_not_found",
                "There's no stream with that key to add viewers to.",
            )
            .hint("Upload the stream first (POST /v1/pro/history/streams).")),
            Sampled::Locked => Err(locked()),
            Sampled::OutOfRange(at) => {
                Err(bad(format!("samples: {at} is outside the stream's start and end (or now).")))
            }
        }
    };
    run.await.unwrap_or_else(IntoResponse::into_response)
}

/// `from`, `to` and `platform` from the query, with the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Range {
    from: i64,
    to: i64,
    platform: Option<String>,
}

fn range(raw: Option<&str>, now: i64, default_from: Option<i64>) -> Result<Range, ApiError> {
    let mut from = None;
    let mut to = None;
    let mut platform = None;
    for (k, v) in url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()) {
        let ms = |v: &str| v.trim().parse::<i64>().map_err(|_| bad_request(format!("{k}: a time in milliseconds.")));
        match k.as_ref() {
            "from" => from = Some(ms(&v)?),
            "to" => to = Some(ms(&v)?),
            "platform" if !v.is_empty() => platform = Some(v.into_owned()),
            _ => {}
        }
    }
    let to = to.unwrap_or(now);
    let from = from.or(default_from).unwrap_or(to - DEFAULT_RANGE_MS);
    if from < 0 || to <= from {
        return Err(invalid("invalid_range", "from must be before to.").hint("Pick a range again."));
    }
    if to - from > MAX_RANGE_MS {
        return Err(invalid("invalid_range", "A range can be up to 400 days.").hint("Pick a shorter range."));
    }
    if let Some(p) = &platform
        && !platform_ok(p)
    {
        return Err(invalid("invalid_platform", "platform: use twitch, youtube or kick."));
    }
    Ok(Range { from, to, platform })
}

/// Monday 00:00 UTC of the week `ms` falls in.
pub fn week_start(ms: i64) -> i64 {
    let day = ms.div_euclid(86_400_000);
    // 1970-01-01 was a Thursday: Monday is three days before.
    (day - (day + 3).rem_euclid(7)) * 86_400_000
}

/// Every week from `from`'s to `to`'s (empty ones too), oldest first.
pub fn weekly(streams: &[StreamOut], from: i64, to: i64, now: i64) -> Vec<Week> {
    let first = week_start(from);
    let last = week_start((to - 1).max(from));
    let n = usize::try_from((last - first) / WEEK_MS + 1).unwrap_or(0);
    let mut weeks: Vec<(Week, i128)> = (0..n)
        .map(|i| {
            let start = first + i64::try_from(i).unwrap_or(0) * WEEK_MS;
            let by_platform = PLATFORMS.iter().map(|p| ((*p).to_string(), 0)).collect();
            let w = Week {
                week_start: start,
                streams: 0,
                by_platform,
                minutes: 0,
                peak_viewers: 0,
                avg_viewers: 0,
                follows: 0,
                subs: 0,
                chat_lines: 0,
            };
            (w, 0)
        })
        .collect();
    for s in streams {
        let i = (week_start(s.started_at) - first) / WEEK_MS;
        let Some((w, weighted)) = usize::try_from(i).ok().and_then(|i| weeks.get_mut(i)) else { continue };
        let minutes = (s.ended_at.unwrap_or(now) - s.started_at).max(0) / 60_000;
        w.streams += 1;
        *w.by_platform.entry(s.platform.clone()).or_default() += 1;
        w.minutes += minutes;
        w.peak_viewers = w.peak_viewers.max(s.peak_viewers);
        w.follows += s.follows;
        w.subs += s.subs;
        w.chat_lines += s.chat_lines;
        *weighted += i128::from(s.avg_viewers) * i128::from(minutes.max(1));
    }
    weeks
        .into_iter()
        .map(|(mut w, weighted)| {
            let total = i128::from(w.minutes.max(w.streams));
            if total > 0 {
                w.avg_viewers = i64::try_from((weighted + total / 2) / total).unwrap_or(0);
            }
            w
        })
        .collect()
}

/// `GET /v1/pro/history?from&to&platform` → `{from, to, streams, truncated, weekly, plan,
/// keep_days, earliest, limited}`.
async fn list(State(st): State<AppState>, headers: HeaderMap, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    let owner = gate(&st, &headers).await?;
    Ok(axum::Json(listed(&st, &owner, q.as_deref())?).into_response())
}

/// `GET /r/api/v1/pro/history` from an owner's phone while the PC is off (M10): the same
/// answer as `GET /v1/pro/history` for a Pro subscription, with the same per-subscription
/// rate, only while this relay has the feature (`features.history`; 503
/// `history_unavailable` otherwise).
pub(crate) fn phone_list(st: &AppState, sub: &str, query: Option<&str>) -> Result<Value, ApiError> {
    if !available(st) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "history_unavailable",
            "Stream history isn't available on Ferrocast's servers right now.",
        )
        .hint("Open History on the streaming PC, or try again later."));
    }
    take_rate(st, sub)?;
    let owner = Owner { install: String::new(), key: sub.to_string(), pro: true, reads: vec![sub.to_string()] };
    listed(st, &owner, query)
}

/// Streams that started in `[from, to)` under any of `keys`, newest first, one per stream:
/// a stream begun on a free account and finished on Pro is under both keys, and the newer
/// copy wins.
pub(crate) fn streams_of(
    st: &AppState,
    keys: &[String],
    from: i64,
    to: i64,
    platform: Option<&str>,
    limit: usize,
) -> Result<Vec<StreamOut>, ApiError> {
    let mut all = Vec::new();
    for k in keys {
        all.extend(st.db.list_streams(k, from, to, platform, limit).map_err(ApiError::db)?);
    }
    all.sort_by(|a, b| {
        b.started_at.cmp(&a.started_at).then_with(|| a.platform.cmp(&b.platform)).then(b.updated_at.cmp(&a.updated_at))
    });
    let mut seen = std::collections::HashSet::new();
    all.retain(|s| seen.insert((s.platform.clone(), s.key.clone())));
    all.truncate(limit);
    Ok(all)
}

fn listed(st: &AppState, owner: &Owner, query: Option<&str>) -> Result<Value, ApiError> {
    let now = now_ms();
    let r = range(query, now, None)?;
    // Older than the plan keeps is cut off (a lapsed Pro's year, while it's still kept);
    // `limited` tells the dashboard to say so.
    let earliest = owner.earliest(now);
    let limited = r.from < earliest;
    let from = r.from.max(earliest);
    let mut streams = if r.to > from {
        streams_of(st, &owner.reads, from, r.to, r.platform.as_deref(), MAX_LISTED + 1)?
    } else {
        Vec::new()
    };
    let truncated = streams.len() > MAX_LISTED;
    streams.truncate(MAX_LISTED);
    let weekly = weekly(&streams, from.min(r.to - 1), r.to, now);
    Ok(json!({
        "from": from, "to": r.to, "streams": streams, "truncated": truncated, "weekly": weekly,
        "plan": if owner.pro { "pro" } else { "free" }, "keep_days": owner.keep_days(),
        "earliest": earliest, "limited": limited,
    }))
}

/// `GET /v1/pro/history/samples?platform&key` → `{platform, key, samples: [{at, viewers}]}`,
/// oldest first: one stream's viewer count every five minutes (the per-stream chart).
async fn get_samples(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    let owner = gate(&st, &headers).await?;
    let mut platform = None;
    let mut key = None;
    for (k, v) in url::form_urlencoded::parse(q.as_deref().unwrap_or("").as_bytes()) {
        match k.as_ref() {
            "platform" => platform = Some(v.into_owned()),
            "key" => key = Some(v.into_owned()),
            _ => {}
        }
    }
    let platform = platform.filter(|p| platform_ok(p)).ok_or_else(|| {
        invalid("invalid_platform", "platform: use twitch, youtube or kick.").hint("Pick the stream again.")
    })?;
    let key = key
        .filter(|k| key_ok(k))
        .ok_or_else(|| invalid("invalid_key", "key: the stream's key from the list.").hint("Pick the stream again."))?;
    for k in &owner.reads {
        if let Some(samples) = st.db.stream_samples(k, &platform, &key).map_err(ApiError::db)? {
            let list: Vec<Value> = samples.iter().map(|(at, v)| json!({ "at": at, "viewers": v })).collect();
            return Ok(axum::Json(json!({ "platform": platform, "key": key, "samples": list })).into_response());
        }
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "stream_not_found", "That stream isn't in your history.")
        .hint("It may be older than your history keeps. Pick another stream."))
}

/// `GET /v1/pro/history/export.csv?from&to&platform` (all kept history by default). Open
/// to a subscription whose Pro ended, while its history is still kept (R-04), and to a free
/// account (its 30 days).
async fn export_csv(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let owner = match owner_of(&st, install.clone()) {
        Ok(o) => o,
        Err(e) => {
            // No Pro and no account: a lapsed subscription's kept history can still leave.
            let sub = st.db.pro_sub_for_install(&install).map_err(ApiError::db)?;
            match sub {
                Some(s) if st.db.has_history(&s.id).map_err(ApiError::db)? => {
                    Owner { install, key: s.id.clone(), pro: false, reads: vec![s.id] }
                }
                _ => return Err(e),
            }
        }
    };
    take_rate(&st, &owner.key)?;
    let now = now_ms();
    let r = range(q.as_deref(), now, Some(0.max(now - MAX_RANGE_MS)))?;
    let streams = streams_of(&st, &owner.reads, r.from, r.to, r.platform.as_deref(), usize::MAX)?;
    let mut res = csv(&streams, now).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/csv; charset=utf-8"));
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("attachment; filename=\"ferrocast-history.csv\""),
    );
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    Ok(res)
}

// ── CSV ──────────────────────────────────────────────────────────────────────────────

/// `2026-10-19 12:05` (UTC).
pub fn iso(ms: i64) -> String {
    let (y, m, d) = crate::quota::civil(ms.div_euclid(86_400_000));
    let mins = ms.rem_euclid(86_400_000) / 60_000;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
}

/// Currencies without minor units, as shown in the CSV.
const NO_DECIMALS: [&str; 6] = ["JPY", "KRW", "VND", "CLP", "ISK", "UGX"];

fn money(m: &Money) -> String {
    if NO_DECIMALS.contains(&m.currency.as_str()) {
        format!("{} {}", m.currency, m.minor)
    } else {
        format!("{} {}.{:02}", m.currency, m.minor / 100, m.minor % 100)
    }
}

/// One CSV field: quoted, quotes doubled, and a leading `= + - @` defused so a
/// spreadsheet never runs a stream title as a formula.
fn field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_string() };
    format!("\"{}\"", s.replace('"', "\"\""))
}

pub fn csv(streams: &[StreamOut], now: i64) -> String {
    let mut out = String::from(
        "platform,started_utc,ended_utc,minutes,title,category,peak_viewers,avg_viewers,chat_lines,chatters,new_chatters,follows,subs,tips,platform_value\r\n",
    );
    for s in streams.iter().rev() {
        let minutes = (s.ended_at.unwrap_or(now) - s.started_at).max(0) / 60_000;
        let tips = s.tips.iter().map(money).collect::<Vec<_>>().join("; ");
        let value = s.platform_value.iter().map(|v| format!("{} {}", v.amount, v.unit)).collect::<Vec<_>>().join("; ");
        out.push_str(&format!(
            "{},{},{},{minutes},{},{},{},{},{},{},{},{},{},{},{}\r\n",
            s.platform,
            iso(s.started_at),
            s.ended_at.map(iso).unwrap_or_default(),
            field(s.title.as_deref().unwrap_or("")),
            field(s.category.as_deref().unwrap_or("")),
            s.peak_viewers,
            s.avg_viewers,
            s.chat_lines,
            s.chatters,
            s.new_chatters,
            s.follows,
            s.subs,
            field(&tips),
            field(&value),
        ));
    }
    out
}
