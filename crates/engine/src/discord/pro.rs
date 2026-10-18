//! The Ferrocast bot for Discord (Pro, docs/plan/PRO.md F4, API.md §9.6), as the desktop
//! sees it:
//! - **Go-live.** With Pro (the remembered status) and the relay's bot, a platform going live
//!   is reported to the relay, which posts its embed and edits it at the end. When the relay
//!   posts (or the stream came back within 30 minutes), the free webhook post is skipped; when
//!   it can't (bot go-live off, no channel, relay down) the free webhook posts as before.
//!   The end of a stream is queued (settings `discord_pro`) and retried until the relay has
//!   it, with when it really ended; a stream the bot reported live is remembered across a
//!   restart (with when viewers were last counted), so "online" again mid-stream is the same
//!   stream and a stream whose end was missed is closed before the next one is reported.
//! - **Perks.** Subs, YouTube memberships and paid messages with a platform user id are queued
//!   (settings key `discord_pro`, so a restart or a relay that's down loses nothing) and sent
//!   in batches of up to 100, retried with backoff like stream history.
//! - **The dashboard's Settings › Discord bot section**: the relay's routes passed through.
//!
//! Nothing here runs without Pro, and nothing ever waits on the relay in the pipeline (R-01).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::Duration;

use ferrocast_core::event::{Event, EventKind, Origin, Platform, Tier};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::frames::{LiveFrame, Notice, NoticeAction, NoticeLevel};
use crate::{Engine, EngineError, now, platform_key};

pub(crate) const SAVE_KEY: &str = "discord_pro";
/// Perk events per request (the relay takes up to 100).
const BATCH: usize = 100;
/// Perk events kept while the relay can't take them; the oldest go first past it.
const MAX_QUEUE: usize = 1_000;
/// The relay ignores perk events older than 30 days; they're not worth keeping either.
const MAX_AGE_MS: i64 = 30 * 86_400_000;
const TICK: Duration = Duration::from_secs(30);
const BACKOFF_MIN_MS: i64 = 30_000;
const BACKOFF_MAX_MS: i64 = 15 * 60_000;
/// Stream ends not delivered yet; older ones aren't worth an edit any more.
const MAX_ENDS: usize = 20;
const MAX_END_AGE_MS: i64 = 86_400_000;
/// A stream back within this long (of its last sign of life) is the same stream.
const SAME_STREAM_MS: i64 = 30 * 60_000;

/// One queued item, with a number so an upload removes exactly what it sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Queued {
    pub seq: u64,
    pub event: Value,
}

/// A stream the bot was told about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LiveRun {
    /// What the relay was told (it matches the end to the post by it).
    pub started_at: i64,
    pub peak: u64,
    /// The last sign the stream was on (a viewer count, or the start).
    pub seen_at: i64,
}

/// What's saved in the store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Saved {
    /// The bot posts go-live (as the relay last said): the free webhook post is skipped.
    pub go_live: bool,
    /// Perk events not sent yet, oldest first.
    pub queue: VecDeque<Queued>,
    /// Stream-end reports not delivered yet, oldest first.
    pub ends: VecDeque<Queued>,
    /// Platforms reported live (platform key → the stream), so a restart keeps them.
    pub live: BTreeMap<String, LiveRun>,
    pub next_seq: u64,
}

/// The engine's Discord bot state (in `Inner`).
#[derive(Debug, Default)]
pub(crate) struct DiscordPro {
    pub saved: Saved,
    /// Pro active and the relay has the bot (remembered status, refreshed each round).
    pub on: bool,
    pub dirty: bool,
    pub retry_at: i64,
    pub failures: u32,
    /// One "Ferrocast's role is too low" notice per run.
    pub warned: bool,
}

/// What one perk-event upload got.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    /// Taken. `blocked`: the relay couldn't give some roles (its hint for the notice).
    Done {
        blocked: Option<String>,
    },
    Later {
        wait_ms: Option<i64>,
    },
    /// 403 (an older relay): the streamer must move Ferrocast's role; kept, retried slowly.
    Blocked,
    /// 400 / 422: the relay can't use this upload. One event at a time finds the bad one.
    Refused,
}

/// Relay answers that sending the same again can't change.
fn permanent(status: u16) -> bool {
    matches!(status, 400 | 403 | 404 | 409 | 413 | 415 | 422)
}

fn tier_key(t: &Tier) -> Option<String> {
    match t {
        Tier::Prime => Some("prime".into()),
        Tier::Tier1 => Some("1".into()),
        Tier::Tier2 => Some("2".into()),
        Tier::Tier3 => Some("3".into()),
        Tier::Level { name, .. } => Some(name.chars().take(100).collect()),
    }
}

/// The perk event for a live event, if it can earn a role: a sub (Twitch, Kick), a YouTube
/// membership, or money with the payer's platform account (a Super Chat). Tip-page tips have no
/// platform account, so they can't reach a linked Discord user.
pub(crate) fn perk_event(e: &Event) -> Option<Value> {
    if e.origin != Origin::Live {
        return None;
    }
    let platform = match e.platform {
        Platform::Twitch | Platform::Youtube | Platform::Kick => platform_key(e.platform),
        _ => return None,
    };
    let at = e.occurred_at.0;
    match &e.kind {
        EventKind::Subscription { actor, tier, .. } => {
            let id = actor.id.clone().filter(|i| !i.is_empty())?;
            let kind = if e.platform == Platform::Youtube { "member" } else { "sub" };
            Some(
                json!({ "platform": platform, "platform_user_id": id, "kind": kind, "tier": tier_key(tier), "at": at }),
            )
        }
        EventKind::PaidMessage { actor, money, .. } => {
            let id = actor.id.clone().filter(|i| !i.is_empty())?;
            (money.minor > 0).then(|| {
                json!({ "platform": platform, "platform_user_id": id, "kind": "tip",
                        "amount_minor": money.minor, "currency": money.currency, "at": at })
            })
        }
        _ => None,
    }
}

impl DiscordPro {
    fn seq(&mut self) -> u64 {
        let n = self.saved.next_seq;
        self.saved.next_seq = n.wrapping_add(1);
        n
    }

    fn push(&mut self, v: Value) {
        let seq = self.seq();
        self.saved.queue.push_back(Queued { seq, event: v });
        while self.saved.queue.len() > MAX_QUEUE {
            self.saved.queue.pop_front();
        }
        self.dirty = true;
    }

    /// Queues the end of a stream for the relay (its post is edited with it).
    fn push_end(&mut self, platform: &str, run: LiveRun, ended_at: i64) {
        let seq = self.seq();
        let event = json!({ "platform": platform, "live": false, "started_at": run.started_at,
                            "ended_at": ended_at.max(run.started_at), "peak_viewers": (run.peak > 0).then_some(run.peak) });
        self.saved.ends.push_back(Queued { seq, event });
        while self.saved.ends.len() > MAX_ENDS {
            self.saved.ends.pop_front();
        }
        self.dirty = true;
    }

    /// Removes what an upload sent (by number: events queued or cleared meanwhile are safe).
    fn sent(queue: &mut VecDeque<Queued>, seqs: &HashSet<u64>) {
        queue.retain(|q| !seqs.contains(&q.seq));
    }

    /// Drops events the relay would ignore as too old, and stale stream ends.
    fn age_out(&mut self, now: i64) {
        let before = (self.saved.queue.len(), self.saved.ends.len());
        self.saved.queue.retain(|q| q.event["at"].as_i64().is_some_and(|at| now - at <= MAX_AGE_MS));
        self.saved.ends.retain(|q| q.event["ended_at"].as_i64().is_some_and(|at| now - at <= MAX_END_AGE_MS));
        if (self.saved.queue.len(), self.saved.ends.len()) != before {
            self.dirty = true;
        }
    }
}

impl Engine {
    pub(crate) fn discord_pro(&self) -> std::sync::MutexGuard<'_, DiscordPro> {
        self.inner.discord_pro.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Loads what the last run left (the go-live flag, unsent perk events).
    pub(crate) async fn load_discord_pro(&self) {
        let saved = match self.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read unsent Discord perks; starting empty");
                Saved::default()
            }
        };
        let on = self.pro_has(|f| f.discord).await;
        let mut d = self.discord_pro();
        d.saved = saved;
        // Never hand out a number that's still queued.
        let top = d.saved.queue.iter().chain(&d.saved.ends).map(|q| q.seq.wrapping_add(1)).max().unwrap_or(0);
        d.saved.next_seq = d.saved.next_seq.max(top);
        d.on = on;
    }

    /// The bot posts go-live instead of the free webhook (for the dashboard's free section).
    pub(crate) fn discord_pro_go_live(&self) -> bool {
        let d = self.discord_pro();
        d.on && d.saved.go_live
    }

    /// From the pipeline, for every event: perks queued, viewer peaks, stream ends. Cheap and
    /// never waits (nothing happens without Pro).
    pub(crate) fn discord_saw(&self, e: &Event) {
        if e.origin != Origin::Live {
            return;
        }
        let key = platform_key(e.platform);
        let at = e.occurred_at.0;
        let mut d = self.discord_pro();
        if !d.on {
            return;
        }
        match &e.kind {
            EventKind::ViewerCount { count } => {
                if let Some(l) = d.saved.live.get_mut(key) {
                    l.peak = l.peak.max(*count);
                    l.seen_at = l.seen_at.max(at);
                    // Saved with the next round (every 30 s), not on every count.
                    d.dirty = true;
                }
            }
            EventKind::StreamOffline => {
                // Not reported live this time (or before a restart): the relay still closes
                // an open post for it.
                let run = d.saved.live.remove(key).unwrap_or(LiveRun { started_at: at, peak: 0, seen_at: at });
                d.push_end(key, run, at);
                drop(d);
                let engine = self.clone();
                tokio::spawn(async move {
                    engine.flush_discord_ends().await;
                    engine.save_discord_pro().await;
                });
            }
            _ => {
                if let Some(v) = perk_event(e) {
                    d.push(v);
                    drop(d);
                    self.save_discord_pro_soon();
                }
            }
        }
    }

    /// Sends the queued stream ends, oldest first, until one can't go now (kept for the next
    /// round). One the relay refuses for good is dropped.
    pub(crate) async fn flush_discord_ends(&self) {
        // Its own lock: a go-live post never waits behind a long perk upload.
        let _one = self.inner.discord_ends_io.lock().await;
        loop {
            let Some(q) = self.discord_pro().saved.ends.front().cloned() else { break };
            let delivered =
                match self.pro_call(reqwest::Method::POST, "/v1/pro/discord/live", Some(q.event.clone())).await {
                    Ok(_) => true,
                    Err(EngineError::Refused { status, .. }) if permanent(status) => {
                        tracing::debug!(status, "the Discord bot refused a stream end; dropped");
                        true
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "couldn't tell the Discord bot the stream ended; will retry");
                        false
                    }
                };
            if !delivered {
                break;
            }
            let mut d = self.discord_pro();
            DiscordPro::sent(&mut d.saved.ends, &HashSet::from([q.seq]));
            d.dirty = true;
        }
    }

    /// A platform went live with Pro: the relay's bot posts. True when it did (or the post is
    /// already there), so the free webhook stays quiet; false to let the webhook post.
    pub(crate) async fn discord_pro_go_live_post(&self, platform: Platform, title: Option<String>, at: i64) -> bool {
        if !self.discord_pro().on {
            return false;
        }
        let key = platform_key(platform);
        // The same stream as one already reported (a restart, "online" twice) keeps its start;
        // one whose end was missed (nothing seen for 30 minutes) is closed first.
        let (started_at, same_stream) = {
            let mut d = self.discord_pro();
            match d.saved.live.get(key).copied() {
                Some(l) if at - l.seen_at.max(l.started_at) < SAME_STREAM_MS => (l.started_at, true),
                Some(l) => {
                    d.saved.live.remove(key);
                    d.push_end(key, l, l.seen_at.max(l.started_at));
                    (at, false)
                }
                None => (at, false),
            }
        };
        // The relay sees any queued end before this start.
        self.flush_discord_ends().await;
        let account =
            self.db(repo::settings::accounts).await.ok().and_then(|l| l.into_iter().find(|a| a.platform == key));
        let (name, url, thumbnail) = match &account {
            Some(a) => {
                let (url, thumb) = match platform {
                    Platform::Twitch => (
                        format!("https://www.twitch.tv/{}", a.login),
                        Some(format!(
                            "https://static-cdn.jtvnw.net/previews-ttv/live_user_{}-1280x720.jpg",
                            a.login.to_lowercase()
                        )),
                    ),
                    Platform::Kick => (format!("https://kick.com/{}", a.login), None),
                    _ => (format!("https://www.youtube.com/channel/{}/live", a.user_id), None),
                };
                (Some(a.display_name.clone()), Some(url), thumb)
            }
            None => (None, None, None),
        };
        let game = self
            .stream_info_saved()
            .await
            .ok()
            .and_then(|s| s.history.into_iter().next())
            .and_then(|h| h.info.category.map(|c| c.name));
        let body = json!({
            "platform": key, "live": true, "name": name, "title": title, "game": game,
            "url": url, "thumbnail_url": thumbnail, "started_at": started_at,
        });
        match self.pro_call(reqwest::Method::POST, "/v1/pro/discord/live", Some(body)).await {
            Ok(v) => {
                // "recent" (a post for this stream is up) only counts when it's a stream this
                // desktop reported; otherwise the webhook posts rather than nobody.
                let handled = v["posted"] == true || v["edited"] == true || (v["reason"] == "recent" && same_stream);
                {
                    let mut d = self.discord_pro();
                    if handled {
                        let run = d.saved.live.entry(key.to_string()).or_default();
                        run.started_at = started_at;
                        run.seen_at = run.seen_at.max(at);
                        d.dirty = true;
                    }
                    let go_live =
                        handled || !matches!(v["reason"].as_str(), Some("off" | "no_channel" | "not_connected"));
                    if d.saved.go_live != go_live {
                        d.saved.go_live = go_live;
                        d.dirty = true;
                    }
                }
                self.save_discord_pro_soon();
                handled
            }
            Err(e) => {
                tracing::warn!(error = %e, "the Discord bot couldn't post go-live; using the webhook if there is one");
                if let EngineError::Refused { status: 403, .. } = &e {
                    self.discord_role_notice(&e.to_string());
                }
                false
            }
        }
    }

    fn discord_role_notice(&self, message: &str) {
        self.send_live(LiveFrame::new(
            "notice",
            &Notice {
                key: Some("discord-pro".into()),
                level: NoticeLevel::Warn,
                message: format!("The Ferrocast bot couldn't do that in Discord. {message}"),
                action: Some(NoticeAction { label: "Discord settings".into(), href: "#/settings/discord".into() }),
            },
        ));
    }

    fn save_discord_pro_soon(&self) {
        let engine = self.clone();
        tokio::spawn(async move { engine.save_discord_pro().await });
    }

    async fn save_discord_pro(&self) {
        let _one = self.inner.discord_pro_io.lock().await;
        let saved = {
            let mut d = self.discord_pro();
            if !d.dirty {
                return;
            }
            d.dirty = false;
            d.saved.clone()
        };
        if let Err(e) = self.db(move |s| repo::settings::set(s, SAVE_KEY, &saved)).await {
            tracing::warn!(error = %e, "couldn't save Discord perks; will try again");
            self.discord_pro().dirty = true;
        }
    }

    /// One round of the perk task: re-read Pro, send what's queued.
    pub(crate) async fn discord_pro_round(&self) {
        let on = self.pro_has(|f| f.discord).await;
        {
            let mut d = self.discord_pro();
            d.on = on;
            d.age_out(now().0);
        }
        if on {
            self.flush_discord_ends().await;
            self.flush_discord_perks().await;
        }
        self.save_discord_pro().await;
    }

    async fn flush_discord_perks(&self) {
        let _one = self.inner.discord_pro_io.lock().await;
        if self.discord_pro().retry_at > now().0 {
            return;
        }
        loop {
            let batch: Vec<Queued> = self.discord_pro().saved.queue.iter().take(BATCH).cloned().collect();
            if batch.is_empty() {
                break;
            }
            let sent = self.send_perks(&batch).await;
            let ok = match sent {
                Sent::Refused if batch.len() > 1 => self.send_perks_one_by_one(&batch).await,
                other => self.after_perks(&batch, other),
            };
            if !ok {
                break;
            }
        }
    }

    /// After an upload: what was sent leaves the queue, or the queue waits. False to stop.
    fn after_perks(&self, sent_batch: &[Queued], sent: Sent) -> bool {
        let mut d = self.discord_pro();
        match sent {
            Sent::Done { blocked } => {
                // Only what was sent: events may have joined, or the queue been cleared, meanwhile.
                DiscordPro::sent(&mut d.saved.queue, &sent_batch.iter().map(|q| q.seq).collect());
                d.dirty = true;
                d.failures = 0;
                if let Some(hint) = blocked
                    && !std::mem::replace(&mut d.warned, true)
                {
                    drop(d);
                    self.discord_role_notice(&hint);
                }
                true
            }
            Sent::Refused => {
                tracing::warn!(events = sent_batch.len(), "Discord perk events refused; dropped");
                DiscordPro::sent(&mut d.saved.queue, &sent_batch.iter().map(|q| q.seq).collect());
                d.dirty = true;
                true
            }
            Sent::Later { wait_ms } => {
                d.failures = d.failures.saturating_add(1);
                let backoff =
                    BACKOFF_MIN_MS.saturating_mul(1 << d.failures.min(6).saturating_sub(1)).min(BACKOFF_MAX_MS);
                d.retry_at = now().0 + wait_ms.unwrap_or(backoff).max(1_000);
                false
            }
            Sent::Blocked => {
                d.failures = d.failures.saturating_add(1);
                d.retry_at = now().0 + BACKOFF_MAX_MS;
                false
            }
        }
    }

    /// A refused batch, one event at a time: only the bad ones are dropped.
    async fn send_perks_one_by_one(&self, batch: &[Queued]) -> bool {
        for q in batch {
            let one = std::slice::from_ref(q);
            let sent = self.send_perks(one).await;
            if !self.after_perks(one, sent) {
                return false;
            }
        }
        true
    }

    async fn send_perks(&self, batch: &[Queued]) -> Sent {
        let events: Vec<&Value> = batch.iter().map(|q| &q.event).collect();
        let Ok(url) = self.relay_url() else { return Sent::Later { wait_ms: None } };
        let Ok(auth) = self.relay_auth().await else { return Sent::Later { wait_ms: None } };
        let res = self
            .relay_http()
            .post(format!("{url}/v1/pro/discord/perk-events"))
            .bearer_auth(auth)
            .json(&events)
            .send()
            .await;
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e.without_url(), "Discord perks: relay unreachable, will retry");
                return Sent::Later { wait_ms: None };
            }
        };
        let status = res.status().as_u16();
        let wait = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|s| s.clamp(1, 3_600) * 1000);
        match status {
            200..=299 => {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                if v["invalid"].as_u64().is_some_and(|n| n > 0) {
                    tracing::warn!(invalid = %v["invalid"], "some Discord perk events were skipped by the relay");
                }
                let blocked = v["blocked"].as_u64().is_some_and(|n| n > 0).then(|| match v["issue"]["code"].as_str() {
                    Some("guild_missing") => "Ferrocast isn't in your server any more. Connect it again.".to_string(),
                    _ => "Some roles couldn't be given. Move Ferrocast's role above the perk roles in Server Settings › Roles.".to_string(),
                });
                Sent::Done { blocked }
            }
            403 => {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                let first = !std::mem::replace(&mut self.discord_pro().warned, true);
                if first {
                    let m = v["error"]["hint"].as_str().unwrap_or("Check Ferrocast's role in your server.").to_string();
                    self.discord_role_notice(&m);
                }
                Sent::Blocked
            }
            // A body the relay can't use: one event at a time finds the bad one.
            400 | 422 => Sent::Refused,
            // No server connected, the bot was removed, too big: sending again can't help.
            404 | 409 | 413 | 415 => {
                let v: Value = res.json().await.unwrap_or(Value::Null);
                tracing::warn!(status, code = %v["error"]["code"], "Discord perk events refused; dropped");
                Sent::Done { blocked: None }
            }
            429 => Sent::Later { wait_ms: wait },
            // 401, 402 (Pro lapsed), 5xx: keep them for later.
            _ => Sent::Later { wait_ms: None },
        }
    }

    // ── The dashboard's routes (API.md §9.6), passed through ─────────────────────────

    /// Remembers whether the bot posts go-live from a view the relay sent.
    fn remember_discord_view(&self, v: &Value) {
        let go_live = v["connected"] == true && v["go_live"]["enabled"] == true && v["channel_id"].is_string();
        {
            let mut d = self.discord_pro();
            // The relay answered with its bot: Pro and the feature are on, whatever the last
            // remembered status said (so the free section says who posts right away).
            d.on = true;
            if d.saved.go_live == go_live {
                return;
            }
            d.saved.go_live = go_live;
            d.dirty = true;
        }
        self.save_discord_pro_soon();
    }

    /// `GET /api/v1/pro/discord`.
    pub async fn discord_bot(&self) -> Result<Value, EngineError> {
        let v = self.pro_call(reqwest::Method::GET, "/v1/pro/discord", None).await?;
        self.remember_discord_view(&v);
        Ok(v)
    }

    /// `POST /api/v1/pro/discord/connect` → `{invite_url, expires_at}`.
    pub async fn discord_bot_connect(&self) -> Result<Value, EngineError> {
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/discord/connect", None).await?;
        let url = v["invite_url"].as_str().unwrap_or("");
        if !url.starts_with("https://discord.com/") {
            return Err(crate::pro::unreachable("it sent no Discord link"));
        }
        Ok(json!({ "invite_url": url, "expires_at": v["expires_at"] }))
    }

    /// `PUT` channel, go-live, a rule (`path` under `/v1/pro/discord`): the new view.
    pub async fn discord_bot_put(&self, path: &str, body: Value) -> Result<Value, EngineError> {
        let v = self.pro_call(reqwest::Method::PUT, &format!("/v1/pro/discord{path}"), Some(body)).await?;
        self.remember_discord_view(&v);
        Ok(v)
    }

    /// `DELETE` a rule or the whole connection (`path` `""`).
    pub async fn discord_bot_delete(&self, path: &str) -> Result<(), EngineError> {
        self.pro_raw(reqwest::Method::DELETE, &format!("/v1/pro/discord{path}"), None).await?;
        if path.is_empty() {
            {
                let mut d = self.discord_pro();
                d.saved.go_live = false;
                d.saved.queue.clear();
                d.saved.ends.clear();
                d.saved.live.clear();
                d.dirty = true;
            }
            self.save_discord_pro().await;
        }
        Ok(())
    }
}

/// The perk task: a round every 30 seconds, never in the pipeline's way.
pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        engine.discord_pro_round().await;
    }
}
