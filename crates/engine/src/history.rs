//! Stream history for Ferrocast Pro (docs/plan/PRO.md F7, API.md §9.1).
//!
//! While Pro is active (the remembered status, never a network check) each platform's
//! stream is counted here as events go by: viewers, chat lines, chatters, follows, subs,
//! tips per currency, Bits/KICKs/Jewels. The stream is uploaded when it starts and again
//! with its totals when it ends, and a viewer sample goes up every five minutes while
//! live. Uploads wait in a queue saved in the local store (settings key
//! `history_upload`), so a relay that's down, a lost connection or a restart loses
//! nothing; they're retried with backoff. Counting is a few integer bumps under a lock
//! and uploads run on their own task: the free app is never slowed or blocked (R-01).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Origin, Platform};
use ferrocast_core::money::{Money, ValueUnit};
use ferrocast_core::welcome::ChatterMark;
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Engine, EngineError, now, platform_key};

const SAVE_KEY: &str = "history_upload";
/// A viewer sample this often while live.
pub(crate) const SAMPLE_EVERY_MS: i64 = 5 * 60_000;
/// How often the upload task looks at the queue.
const TICK: Duration = Duration::from_secs(30);
/// Samples per upload (the relay takes up to 100).
const SAMPLES_PER_ITEM: usize = 100;
/// Uploads kept while the relay can't be reached: weeks of streaming. Past it, the oldest
/// viewer samples go first, then the oldest streams.
const MAX_QUEUE: usize = 1_000;
/// Chatter ids remembered per stream (to count each chatter once).
const MAX_CHATTERS: usize = 50_000;
/// A stream restored after a restart whose platform isn't live again within this long
/// ended while Ferrocast was closed.
const RESUME_GRACE_MS: i64 = 10 * 60_000;
/// Retry waits: 30 s doubling up to 15 minutes.
const BACKOFF_MIN_MS: i64 = 30_000;
const BACKOFF_MAX_MS: i64 = 15 * 60_000;

/// One platform's stream being counted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Track {
    pub key: String,
    pub started_at: i64,
    pub title: Option<String>,
    pub category: Option<String>,
    pub viewers: u64,
    pub peak_viewers: u64,
    /// Sum and number of viewer readings, for the average.
    pub viewer_sum: u64,
    pub viewer_readings: u64,
    pub chat_lines: u64,
    pub chatters: HashSet<String>,
    pub new_chatters: u64,
    pub follows: u64,
    pub subs: u64,
    pub tips: Vec<Money>,
    pub platform_value: BTreeMap<String, u64>,
    pub last_sample_at: i64,
    /// The last time anything was seen for it (the end, if Ferrocast closes mid-stream).
    pub last_seen: i64,
}

impl Track {
    fn new(key: String, started_at: i64) -> Track {
        Track { key, started_at, last_sample_at: started_at, last_seen: started_at, ..Default::default() }
    }

    fn avg_viewers(&self) -> u64 {
        if self.viewer_readings == 0 {
            return 0;
        }
        (self.viewer_sum + self.viewer_readings / 2) / self.viewer_readings
    }

    /// The upload body (API.md §9.1 `POST /v1/pro/history/streams`).
    fn body(&self, platform: &str, ended_at: Option<i64>) -> Value {
        let peak = self.peak_viewers.max(self.viewers);
        json!({
            "platform": platform,
            "key": self.key,
            "started_at": self.started_at,
            "ended_at": ended_at,
            "title": self.title,
            "category": self.category,
            "peak_viewers": peak,
            "avg_viewers": self.avg_viewers().min(peak),
            "chat_lines": self.chat_lines,
            "chatters": self.chatters.len(),
            "new_chatters": self.new_chatters.min(self.chatters.len() as u64),
            "follows": self.follows,
            "subs": self.subs,
            "tips": self.tips.iter().map(|m| json!({ "currency": m.currency, "minor": m.minor })).collect::<Vec<_>>(),
            "platform_value": self.platform_value.iter().map(|(u, a)| json!({ "unit": u, "amount": a })).collect::<Vec<_>>(),
        })
    }
}

/// One upload waiting for the relay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Upload {
    Stream { body: Value },
    Samples { platform: String, key: String, samples: Vec<(i64, u64)> },
}

impl Upload {
    fn path(&self) -> &'static str {
        match self {
            Upload::Stream { .. } => "/v1/pro/history/streams",
            Upload::Samples { .. } => "/v1/pro/history/samples",
        }
    }

    fn body(&self) -> Value {
        match self {
            Upload::Stream { body } => body.clone(),
            Upload::Samples { platform, key, samples } => json!({
                "platform": platform, "key": key,
                "samples": samples.iter().map(|(at, v)| json!({ "at": at, "viewers": v })).collect::<Vec<_>>(),
            }),
        }
    }
}

/// What's saved in the store: streams being counted and uploads not sent yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Saved {
    pub tracks: BTreeMap<String, Track>,
    pub queue: VecDeque<Upload>,
}

impl Saved {
    fn push(&mut self, u: Upload) {
        // Samples join the last upload when it's the same stream's samples (one request for
        // a whole offline hour instead of twelve).
        if let Upload::Samples { platform, key, samples } = &u
            && let Some(Upload::Samples { platform: p, key: k, samples: s }) = self.queue.back_mut()
            && p == platform
            && k == key
            && s.len() + samples.len() <= SAMPLES_PER_ITEM
        {
            s.extend_from_slice(samples);
            return;
        }
        self.queue.push_back(u);
        while self.queue.len() > MAX_QUEUE {
            let oldest_samples = self.queue.iter().position(|u| matches!(u, Upload::Samples { .. }));
            self.queue.remove(oldest_samples.unwrap_or(0));
        }
    }
}

/// The engine's history state (in `Inner`).
#[derive(Debug, Default)]
pub(crate) struct History {
    pub saved: Saved,
    /// Pro with history on this relay, as last read from the remembered status.
    pub on: bool,
    /// Something changed that should reach the store.
    pub dirty: bool,
    /// The queue waits until then after a failure (ms).
    pub retry_at: i64,
    pub failures: u32,
    /// When this run started (for streams restored from before a restart).
    pub started: i64,
}

/// What the relay said to one upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// Saved, or refused for good (bad data, a locked or unknown stream): drop it.
    Done,
    /// Try again later (offline, 429, 5xx, not Pro right now).
    Later,
}

fn unit_key(u: &ValueUnit) -> String {
    match u {
        ValueUnit::Bits => "bits".into(),
        ValueUnit::Kicks => "kicks".into(),
        ValueUnit::Jewels => "jewels".into(),
        ValueUnit::Other(s) => {
            let k: String =
                s.to_ascii_lowercase().chars().map(|c| if c.is_ascii_lowercase() { c } else { '_' }).take(24).collect();
            if k.trim_matches('_').is_empty() { "other".into() } else { k }
        }
    }
}

fn add_money(list: &mut Vec<Money>, m: &Money) {
    match list.iter_mut().find(|x| x.currency == m.currency) {
        Some(x) => x.minor = x.minor.saturating_add(m.minor),
        None => list.push(m.clone()),
    }
}

/// Stream platforms only (tips arrive as `Platform::Tips`).
fn streams_on(p: Platform) -> bool {
    matches!(p, Platform::Twitch | Platform::Youtube | Platform::Kick)
}

impl History {
    /// Counts one event. Returns true when an upload was queued (worth saving now).
    pub(crate) fn saw(&mut self, e: &Event, mark: Option<&ChatterMark>, category: Option<String>) -> bool {
        if !self.on || e.origin != Origin::Live {
            return false;
        }
        let at = e.occurred_at.0;
        let key = platform_key(e.platform);
        match &e.kind {
            EventKind::StreamOnline { title } if streams_on(e.platform) => {
                match self.saved.tracks.get(key).map(|t| t.last_seen) {
                    // Seen moments ago: the same stream, said twice by a platform.
                    Some(seen) if at - seen <= RESUME_GRACE_MS => return false,
                    // Left over from an earlier run (a crash, a closed app): that stream ended
                    // when it was last seen, and this is a new one.
                    Some(seen) => {
                        self.end(key, seen);
                    }
                    None => {}
                }
                let mut t = Track::new(format!("{key}:{at}"), at);
                t.title = title.clone().filter(|s| !s.trim().is_empty());
                t.category = category;
                let body = t.body(key, None);
                self.saved.tracks.insert(key.to_string(), t);
                self.saved.push(Upload::Stream { body });
                self.dirty = true;
                return true;
            }
            EventKind::StreamOffline => return self.end(key, at),
            _ => {}
        }
        // Tips through the tip page belong to the stream that started first.
        let track_key = if e.platform == Platform::Tips {
            match self.saved.tracks.iter().min_by_key(|(_, t)| t.started_at) {
                Some((k, _)) => k.clone(),
                None => return false,
            }
        } else {
            key.to_string()
        };
        let Some(t) = self.saved.tracks.get_mut(&track_key) else { return false };
        t.last_seen = t.last_seen.max(at);
        match &e.kind {
            EventKind::ViewerCount { count } => {
                t.viewers = *count;
                t.peak_viewers = t.peak_viewers.max(*count);
                t.viewer_sum = t.viewer_sum.saturating_add(*count);
                t.viewer_readings += 1;
            }
            EventKind::ChatMessage { actor, .. } => {
                t.chat_lines += 1;
                let who = actor.id.clone().unwrap_or_else(|| actor.display_name.to_lowercase());
                if t.chatters.len() < MAX_CHATTERS && t.chatters.insert(who) && matches!(mark, Some(ChatterMark::First))
                {
                    t.new_chatters += 1;
                }
            }
            EventKind::Follow { .. } => t.follows += 1,
            EventKind::Subscription { gifted_by: None, .. } => t.subs += 1,
            EventKind::GiftBatch { count, .. } => t.subs += u64::from(*count),
            EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. } if e.kind.is_alertable() => {
                add_money(&mut t.tips, money);
            }
            EventKind::PlatformGift { value, .. } => {
                *t.platform_value.entry(unit_key(&value.unit)).or_default() += value.amount;
            }
            _ => return false,
        }
        self.dirty = true;
        false
    }

    /// The stream on `platform` ended at `at`: its totals are queued.
    pub(crate) fn end(&mut self, platform: &str, at: i64) -> bool {
        let Some(t) = self.saved.tracks.remove(platform) else { return false };
        let ended = at.max(t.started_at);
        if t.viewers > 0 && ended > t.last_sample_at {
            self.saved.push(Upload::Samples {
                platform: platform.into(),
                key: t.key.clone(),
                samples: vec![(ended, t.viewers)],
            });
        }
        self.saved.push(Upload::Stream { body: t.body(platform, Some(ended)) });
        self.dirty = true;
        true
    }

    /// Due viewer samples, and streams restored from before a restart that aren't live any
    /// more. `live` is which platforms are live now.
    pub(crate) fn tick(&mut self, now: i64, live: &[&str]) {
        let stale: Vec<(String, i64)> = self
            .saved
            .tracks
            .iter()
            .filter(|(p, t)| !live.contains(&p.as_str()) && now - self.started.max(t.last_seen) > RESUME_GRACE_MS)
            .map(|(p, t)| (p.clone(), t.last_seen))
            .collect();
        for (p, last) in stale {
            self.end(&p, last);
        }
        let mut due = Vec::new();
        for (p, t) in &mut self.saved.tracks {
            if live.contains(&p.as_str()) && now - t.last_sample_at >= SAMPLE_EVERY_MS {
                t.last_sample_at = now;
                due.push(Upload::Samples { platform: p.clone(), key: t.key.clone(), samples: vec![(now, t.viewers)] });
            }
        }
        for u in due {
            self.saved.push(u);
            self.dirty = true;
        }
    }
}

impl Engine {
    pub(crate) fn history(&self) -> std::sync::MutexGuard<'_, History> {
        self.inner.history.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Loads what the last run left (streams being counted, uploads not sent).
    pub(crate) async fn load_history(&self) {
        let saved = match self.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read unsent stream history; starting empty");
                Saved::default()
            }
        };
        let on = self.pro_has(|f| f.history).await;
        let mut h = self.history();
        h.saved = saved;
        h.on = on;
        h.started = now().0;
    }

    /// Counts one event from the pipeline (cheap; does nothing without Pro).
    pub(crate) fn history_saw(&self, e: &Event, mark: Option<&ChatterMark>) {
        let category = if matches!(e.kind, EventKind::StreamOnline { .. }) && self.history().on {
            // The category last set from Ferrocast (the platforms don't say it on going live).
            self.studio_last_category()
        } else {
            None
        };
        let queued = self.history().saw(e, mark, category);
        if queued {
            self.save_history_soon();
        }
    }

    fn studio_last_category(&self) -> Option<String> {
        self.inner.history_category.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Saves the history state to the store in the background.
    fn save_history_soon(&self) {
        let engine = self.clone();
        tokio::spawn(async move { engine.save_history().await });
    }

    async fn save_history(&self) {
        let _one = self.inner.history_io.lock().await;
        let saved = {
            let mut h = self.history();
            if !h.dirty {
                return;
            }
            h.dirty = false;
            h.saved.clone()
        };
        if let Err(e) = self.db(move |s| repo::settings::set(s, SAVE_KEY, &saved)).await {
            tracing::warn!(error = %e, "couldn't save unsent stream history; will try again");
            self.history().dirty = true;
        }
    }

    /// One round of the upload task: re-read Pro, take samples, send what's queued.
    pub(crate) async fn history_round(&self) {
        let on = self.pro_has(|f| f.history).await;
        // The category last set from Ferrocast, for the next stream that starts.
        let category = self
            .stream_info_saved()
            .await
            .ok()
            .and_then(|s| s.history.into_iter().next())
            .and_then(|h| h.info.category.map(|c| c.name));
        *self.inner.history_category.lock().unwrap_or_else(|p| p.into_inner()) = category;
        let live: Vec<&'static str> =
            self.state().live.iter().filter(|(_, l)| **l).map(|(p, _)| platform_key(*p)).collect();
        {
            let mut h = self.history();
            h.on = on;
            if on {
                h.tick(now().0, &live);
            }
        }
        if on {
            self.flush_history().await;
        }
        self.save_history().await;
    }

    /// Sends queued uploads in order until one has to wait.
    async fn flush_history(&self) {
        let _one = self.inner.history_io.lock().await;
        if self.history().retry_at > now().0 {
            return;
        }
        loop {
            let Some(next) = self.history().saved.queue.front().cloned() else { break };
            match self.send_history(&next).await {
                Sent::Done => {
                    let mut h = self.history();
                    // Only drop what was sent: samples may have joined it meanwhile.
                    if h.saved.queue.front() == Some(&next) {
                        h.saved.queue.pop_front();
                    } else if let (Some(Upload::Samples { samples, .. }), Upload::Samples { samples: sent, .. }) =
                        (h.saved.queue.front_mut(), &next)
                    {
                        samples.drain(..sent.len().min(samples.len()));
                        if samples.is_empty() {
                            h.saved.queue.pop_front();
                        }
                    }
                    h.dirty = true;
                    h.failures = 0;
                }
                Sent::Later => {
                    let mut h = self.history();
                    h.failures = h.failures.saturating_add(1);
                    let wait =
                        BACKOFF_MIN_MS.saturating_mul(1 << h.failures.min(6).saturating_sub(1)).min(BACKOFF_MAX_MS);
                    h.retry_at = now().0 + wait;
                    break;
                }
            }
        }
    }

    async fn send_history(&self, u: &Upload) -> Sent {
        let Ok(url) = self.relay_url() else { return Sent::Later };
        let Ok(auth) = self.relay_auth().await else { return Sent::Later };
        let res = self.relay_http().post(format!("{url}{}", u.path())).bearer_auth(auth).json(&u.body()).send().await;
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e.without_url(), "stream history: relay unreachable, will retry");
                return Sent::Later;
            }
        };
        let status = res.status().as_u16();
        match status {
            200..=299 => Sent::Done,
            // Bad data, an unknown or locked stream: sending it again can't help.
            400 | 404 | 409 | 413 | 415 | 422 => {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                tracing::warn!(status, code = %v["error"]["code"], "stream history upload refused; dropped");
                Sent::Done
            }
            // 401 (install unknown), 402 (Pro lapsed), 429, 5xx: keep it for later.
            _ => Sent::Later,
        }
    }

    /// Stream history from the relay for the dashboard (`GET /api/v1/pro/history`).
    pub async fn stream_history(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        platform: Option<Platform>,
    ) -> Result<Value, EngineError> {
        self.pro_call(reqwest::Method::GET, &history_path("/v1/pro/history", from, to, platform), None).await
    }

    /// The CSV export (`GET /api/v1/pro/history/export.csv`).
    pub async fn stream_history_csv(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        platform: Option<Platform>,
    ) -> Result<Vec<u8>, EngineError> {
        let (_, bytes) = self.pro_download(&history_path("/v1/pro/history/export.csv", from, to, platform)).await?;
        Ok(bytes)
    }
}

fn history_path(base: &str, from: Option<i64>, to: Option<i64>, platform: Option<Platform>) -> String {
    let mut q = Vec::new();
    if let Some(f) = from {
        q.push(format!("from={f}"));
    }
    if let Some(t) = to {
        q.push(format!("to={t}"));
    }
    if let Some(p) = platform {
        q.push(format!("platform={}", platform_key(p)));
    }
    if q.is_empty() { base.to_string() } else { format!("{base}?{}", q.join("&")) }
}

/// The upload task: a round every 30 seconds, never in the pipeline's way.
pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        engine.history_round().await;
    }
}

#[cfg(test)]
mod tests;
