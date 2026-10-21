//! Ferrocast Pro F1, the 24/7 cloud bot (docs/plan/PRO.md F1, milestone M7; API.md §9.7):
//! Twitch and Kick (M7), YouTube as a beta (M12, `youtube`).
//!
//! - The PC pushes its bot config (`PUT /v1/pro/bot/config`, versioned); the cloud answers
//!   with the same rules as the desktop (`ferrocast_core::cloudbot`).
//! - Who answers is the lease (`lease`), per platform: the PC while it sends `bot_seen` for
//!   that platform on its link, the cloud after 90 s of silence, the PC again on its next
//!   `bot_seen`.
//! - Twitch (`twitch`): a confidential app "Ferrocast Cloud", EventSub through a Conduit
//!   with a webhook shard, replies with the app token. Kick (`kick`): its webhooks already
//!   reach the relay; replies with an opt-in `chat:write` sign-in. Both are tied to the
//!   channel the PC itself is signed in to.
//! - Refresh tokens are sealed in the vault. **Chat is never stored** (R-51): a ring of the
//!   last 200 lines per channel lives in memory only, for the offline dashboard (M10).
//!
//! Env: `TWITCH_CLOUD_CLIENT_ID`, `TWITCH_CLOUD_CLIENT_SECRET`, `TWITCH_CLOUD_EVENTSUB_SECRET`
//! (all three, or Twitch is off), `TWITCH_CLOUD_DESKTOP_CLIENT_ID` (the desktop app's public
//! client id, defaulting to the one this relay was built with); Kick uses the relay's
//! `KICK_CLIENT_ID` / `KICK_CLIENT_SECRET`; YouTube `YOUTUBE_CLOUD_CLIENT_ID` and
//! `YOUTUBE_CLOUD_CLIENT_SECRET` (both, or YouTube is off), `YOUTUBE_CLOUD_DESKTOP_CLIENT_ID`,
//! `YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB` (1,500) and `YOUTUBE_CLOUD_DAILY_UNITS` (9,000).

pub mod check;
pub(crate) mod kick;
mod lease;
pub(crate) mod moderate;
pub(crate) mod store;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod twitch;
pub(crate) mod youtube;
#[cfg(test)]
pub(crate) mod youtube_tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use ferrocast_core::chatbot::CounterOp;
use ferrocast_core::cloudbot::{self, BotConfig, Brain, Chat, ConfigError, CounterChange};
use ferrocast_core::event::{Event, EventKind, Millis, Origin, Platform};
use ferrocast_core::template::Vars;
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

pub use kick::{HttpKick, KickBot, KickError, KickTokens};
pub use lease::{Lease, TAKEOVER_AFTER, Who};
pub(crate) use store::SCHEMA;
pub use twitch::{HttpTwitch, ModTarget, ShardInfo, SubInfo, TokenInfo, TwitchCloud, TwitchError, UserTokens};
pub use youtube::{HttpYoutube, YoutubeCloud, YoutubeConfig, YoutubeError};

use crate::billing::gate::{ProCtx, require_pro};
use crate::db::Db;
use crate::quota::{Limit, Period, TakeErr};
use crate::{ApiError, AppState, authed, now_ms};

/// A boxed future for the platform clients.
pub type Fut<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

pub(crate) const DAY_MS: i64 = 86_400_000;
/// A signed `state` lasts this long.
pub const STATE_TTL_MS: i64 = 10 * 60_000;
/// Requests per subscription per minute over these routes: a rate, not a quota.
pub const RATE: Limit = Limit { name: "bot_requests", period: Period::Minute, limit: 120 };
/// Messages the cloud sends per channel per minute.
pub const MESSAGES_PER_MINUTE: i64 = 20;
/// Chat lines kept in memory per channel (never stored).
pub const RING: usize = 200;
/// Message ids remembered per channel, so a retry is never answered twice.
const DEDUPE_KEEP: usize = 2_048;
/// The cloud's own sent message ids remembered per channel (so it never answers itself).
const SENT_KEEP: usize = 256;
/// A line from the bot account with exactly the text the cloud just sent is its own echo.
const ECHO_WINDOW_MS: i64 = 5_000;
/// Data is kept this long after Pro ends (the hourly sweep).
pub const KEEP_AFTER_LAPSE_MS: i64 = 30 * DAY_MS;
/// Counter changes no PC picked up in this long are dropped.
const COUNTER_CHANGES_KEEP_MS: i64 = 30 * DAY_MS;
/// How often timers are looked at (the desktop's own tick).
pub const TIMER_TICK: Duration = Duration::from_secs(20);
/// Webhook work waiting per source (Twitch, Kick); more is dropped and counted.
pub const QUEUE_CAP: usize = 1_024;
/// Webhook work running at once per source.
pub const WORKERS: usize = 16;
/// Channels whose timers run at once.
pub const TIMER_CONCURRENCY: usize = 16;
/// After the conduit is set up, shard 0 is looked at again this much later: Twitch checks
/// the callback after the PATCH answered, so a failed check would otherwise go unseen.
pub const SHARD_CHECK_AFTER: Duration = Duration::from_secs(20);
/// Set-ups in a row when shard 0 doesn't come up enabled (then the hourly sweep tries).
pub const SHARD_CHECK_TRIES: u32 = 3;
/// A rare cloud bot event (a skip reason, chat arriving) is logged at info once per this
/// long per channel; the rest at debug.
pub const NOTE_EVERY_MS: i64 = 3_600_000;

/// The chat platforms a lease covers (a plain `bot_seen` covers all of them).
pub const PLATFORMS: [Platform; 3] = [Platform::Twitch, Platform::Kick, Platform::Youtube];

/// The Twitch "Ferrocast Cloud" app. `Debug` never shows the secrets.
#[derive(Clone)]
pub struct TwitchConfig {
    pub client_id: String,
    pub client_secret: String,
    pub eventsub_secret: String,
    /// The desktop app's public client id: the token a PC sends with `connect` must be one
    /// of its. `None`: any app's token for the channel proves it (logged at start).
    pub desktop_client_id: Option<String>,
}

impl std::fmt::Debug for TwitchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwitchConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("eventsub_secret", &"<redacted>")
            .field("desktop_client_id", &self.desktop_client_id)
            .finish()
    }
}

/// The official desktop app's public Twitch client id (the releases' FERROCAST_TWITCH_CLIENT_ID),
/// so the channel check never runs without one. A relay for another build sets
/// TWITCH_CLOUD_DESKTOP_CLIENT_ID.
const DESKTOP_TWITCH_CLIENT_ID: &str = "6bbtw1fdkxm81g4x4nan8ufxfmg3x3";

impl TwitchConfig {
    /// `Ok(None)` when none of the three is set; an error names what's missing or wrong.
    pub fn from_env() -> Result<Option<TwitchConfig>, String> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let desktop = env("TWITCH_CLOUD_DESKTOP_CLIENT_ID")
            .or_else(|| option_env!("FERROCAST_TWITCH_CLIENT_ID").map(String::from).filter(|v| !v.is_empty()))
            .or_else(|| Some(DESKTOP_TWITCH_CLIENT_ID.to_string()));
        Ok(Self::from_values(
            env("TWITCH_CLOUD_CLIENT_ID"),
            env("TWITCH_CLOUD_CLIENT_SECRET"),
            env("TWITCH_CLOUD_EVENTSUB_SECRET"),
        )?
        .map(|c| c.with_desktop_client_id(desktop)))
    }

    pub fn from_values(
        client_id: Option<String>,
        client_secret: Option<String>,
        eventsub_secret: Option<String>,
    ) -> Result<Option<TwitchConfig>, String> {
        match (client_id, client_secret, eventsub_secret) {
            (None, None, None) => Ok(None),
            (Some(client_id), Some(client_secret), Some(eventsub_secret)) => {
                // Twitch takes 10 to 100 ASCII characters for a webhook secret.
                if !(10..=100).contains(&eventsub_secret.len()) || !eventsub_secret.is_ascii() {
                    return Err("TWITCH_CLOUD_EVENTSUB_SECRET must be 10 to 100 ASCII characters".into());
                }
                Ok(Some(TwitchConfig { client_id, client_secret, eventsub_secret, desktop_client_id: None }))
            }
            _ => {
                Err("set all three of TWITCH_CLOUD_CLIENT_ID, TWITCH_CLOUD_CLIENT_SECRET, TWITCH_CLOUD_EVENTSUB_SECRET"
                    .into())
            }
        }
    }

    pub fn with_desktop_client_id(mut self, id: Option<String>) -> TwitchConfig {
        self.desktop_client_id = id.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self
    }
}

/// Why a reply didn't go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendErr {
    NotReady,
    SignedOut,
    Refused(String),
    /// Taken but not shown (AutoMod, slow mode, too long): recorded, never blocking.
    Dropped(String),
    RateLimited(u64),
    Unavailable(String),
    /// YouTube's daily units for the cloud bot are spent (whose, until when).
    Quota(youtube::Spent),
}

/// One chat line in memory (R-51: never written anywhere). `event` is the line as the
/// dashboard's chat shows it (without the platform's raw payload), for the phone's offline
/// chat (M10).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatLine {
    pub at: i64,
    pub user: String,
    pub text: String,
    pub event: Arc<Event>,
}

/// What the cloud sent on a channel lately: message ids, and texts with when (for an echo
/// that arrives before Twitch's answer, or a platform that gave no id).
#[derive(Debug, Default)]
struct Sent {
    ids: VecDeque<String>,
    set: HashSet<String>,
    texts: VecDeque<(String, i64)>,
}

impl Sent {
    fn text(&mut self, text: &str, at: i64) {
        self.texts.push_back((text.trim().to_string(), at));
        while self.texts.len() > SENT_KEEP || self.texts.front().is_some_and(|(_, t)| at - t > ECHO_WINDOW_MS) {
            self.texts.pop_front();
        }
    }

    fn id(&mut self, id: String) {
        if self.set.insert(id.clone()) {
            self.ids.push_back(id);
        }
        while self.ids.len() > SENT_KEEP {
            if let Some(old) = self.ids.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    fn echo(&self, text: &str, now: i64) -> bool {
        let t = text.trim();
        self.texts.iter().any(|(s, at)| s == t && now - at <= ECHO_WINDOW_MS)
    }
}

/// What the cloud remembers for a subscription while the relay runs.
#[derive(Debug, Default)]
struct SubMem {
    /// The pushed config (version) with newer cloud counter changes on top.
    config: Option<(i64, BotConfig)>,
    brains: HashMap<Platform, Brain>,
    ring: HashMap<Platform, VecDeque<ChatLine>>,
    seen: HashMap<Platform, (VecDeque<String>, HashSet<String>)>,
    sent: HashMap<Platform, Sent>,
}

type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A bounded queue of webhook work with at most [`WORKERS`] running at once; a full queue
/// drops the work (counted, logged now and then) instead of piling up tasks.
#[derive(Debug, Default)]
struct Work {
    tx: OnceLock<mpsc::Sender<Job>>,
    dropped: AtomicU64,
}

impl Work {
    fn push(&self, what: &'static str, job: Job) {
        let tx = self.tx.get_or_init(|| {
            let (tx, mut rx) = mpsc::channel::<Job>(QUEUE_CAP);
            tokio::spawn(async move {
                let slots = Arc::new(Semaphore::new(WORKERS));
                while let Some(job) = rx.recv().await {
                    let Ok(slot) = slots.clone().acquire_owned().await else { break };
                    tokio::spawn(async move {
                        job.await;
                        drop(slot);
                    });
                }
            });
            tx
        });
        if tx.try_send(job).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n.is_multiple_of(100) {
                tracing::warn!(source = what, dropped = n, "the cloud bot is behind: dropped webhook work");
            }
        }
    }
}

/// The cloud bot: platform clients, leases, memory. Shared by every request.
#[derive(Debug)]
pub struct Runtime {
    twitch_cfg: Option<TwitchConfig>,
    twitch: Option<Arc<dyn TwitchCloud>>,
    kick: Option<Arc<dyn KickBot>>,
    /// The relay's start: a PC never heard from counts its 90 s from here.
    started: (Instant, i64),
    /// Per subscription and platform.
    leases: Mutex<HashMap<(String, Platform), Lease>>,
    /// The install that last said `bot_seen` for a subscription, and when.
    seen_by: Mutex<HashMap<String, (String, i64)>>,
    mem: Mutex<HashMap<String, SubMem>>,
    app_token: tokio::sync::Mutex<Option<(String, Instant)>>,
    conduit: tokio::sync::Mutex<Option<String>>,
    kick_access: Mutex<HashMap<String, (String, i64)>>,
    /// The Twitch bot account's own access token per subscription, for moderation (M10).
    twitch_user: Mutex<HashMap<String, (String, i64)>>,
    /// One Kick refresh at a time per subscription.
    kick_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One Twitch user-token refresh at a time per subscription (moderation, M10).
    twitch_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    twitch_work: Work,
    kick_work: Work,
    /// Bad EventSub signatures: (the minute of the last warning, how many since).
    bad_sig: Mutex<(i64, u64)>,
    /// Stale EventSub timestamps: the same.
    stale: Mutex<(i64, u64)>,
    /// When each rare event was last logged at info: (subscription, platform, what) → ms.
    notes: Mutex<HashMap<(String, Platform, &'static str), i64>>,
    youtube_cfg: Option<YoutubeConfig>,
    pub(crate) youtube: Option<Arc<dyn YoutubeCloud>>,
    /// Per subscription: YouTube's access token, live chat, reader, cadence (memory only).
    yt: Mutex<HashMap<String, youtube::YtMem>>,
    yt_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Google said the project's quota is used up until then (ms, midnight Pacific).
    pub(crate) yt_google_out: AtomicI64,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(None, None, None)
    }
}

impl Runtime {
    /// Twitch needs its config and its client together.
    pub fn new(
        twitch_cfg: Option<TwitchConfig>,
        twitch: Option<Arc<dyn TwitchCloud>>,
        kick: Option<Arc<dyn KickBot>>,
    ) -> Runtime {
        let (twitch_cfg, twitch) = match (twitch_cfg, twitch) {
            (Some(c), Some(t)) => (Some(c), Some(t)),
            _ => (None, None),
        };
        if twitch_cfg.as_ref().is_some_and(|c| c.desktop_client_id.is_none()) {
            tracing::warn!(
                "TWITCH_CLOUD_DESKTOP_CLIENT_ID isn't set: the cloud bot accepts any app's Twitch token as the PC's"
            );
        }
        Runtime {
            twitch_cfg,
            twitch,
            kick,
            started: (Instant::now(), now_ms()),
            leases: Mutex::default(),
            seen_by: Mutex::default(),
            mem: Mutex::default(),
            app_token: tokio::sync::Mutex::default(),
            conduit: tokio::sync::Mutex::default(),
            kick_access: Mutex::default(),
            twitch_user: Mutex::default(),
            kick_locks: Mutex::default(),
            twitch_locks: Mutex::default(),
            twitch_work: Work::default(),
            kick_work: Work::default(),
            bad_sig: Mutex::default(),
            stale: Mutex::default(),
            notes: Mutex::default(),
            youtube_cfg: None,
            youtube: None,
            yt: Mutex::default(),
            yt_locks: Mutex::default(),
            yt_google_out: AtomicI64::new(0),
        }
    }

    /// YouTube needs its config and its client together (M12).
    pub fn with_youtube(mut self, cfg: Option<YoutubeConfig>, client: Option<Arc<dyn YoutubeCloud>>) -> Runtime {
        (self.youtube_cfg, self.youtube) = match (cfg, client) {
            (Some(c), Some(y)) => (Some(c), Some(y)),
            _ => (None, None),
        };
        if self.youtube_cfg.as_ref().is_some_and(|c| c.desktop_client_id.is_none()) {
            tracing::warn!(
                "YOUTUBE_CLOUD_DESKTOP_CLIENT_ID is empty: the cloud bot accepts any app's YouTube token as the PC's"
            );
        }
        self
    }

    pub(crate) fn youtube_cfg(&self) -> Option<&YoutubeConfig> {
        self.youtube_cfg.as_ref()
    }

    pub(crate) fn youtube(&self) -> Result<&dyn YoutubeCloud, ApiError> {
        self.youtube.as_deref().ok_or_else(|| platform_unavailable("youtube"))
    }

    /// The production clients for the relay's settings.
    pub fn from_config(twitch: Option<TwitchConfig>, cfg: &crate::Config) -> Runtime {
        let api = twitch.as_ref().map(|c| Arc::new(HttpTwitch::new(c)) as Arc<dyn TwitchCloud>);
        let kick = HttpKick::new(cfg).map(|k| Arc::new(k) as Arc<dyn KickBot>);
        Runtime::new(twitch, api, kick)
    }

    pub(crate) fn twitch_cfg(&self) -> Option<&TwitchConfig> {
        self.twitch_cfg.as_ref()
    }

    pub(crate) fn twitch(&self) -> Result<&dyn TwitchCloud, ApiError> {
        self.twitch.as_deref().ok_or_else(|| platform_unavailable("twitch"))
    }

    pub(crate) fn kick(&self) -> Result<&dyn KickBot, ApiError> {
        self.kick.as_deref().ok_or_else(|| platform_unavailable("kick"))
    }

    fn has(&self, platform: Platform) -> bool {
        match platform {
            Platform::Twitch => self.twitch.is_some(),
            Platform::Kick => self.kick.is_some(),
            Platform::Youtube => self.youtube.is_some(),
            _ => false,
        }
    }

    /// EventSub work (chat, live status, revocations), bounded.
    pub(crate) fn work_twitch(&self, job: Job) {
        self.twitch_work.push("twitch", job);
    }

    /// Kick webhook work, bounded.
    pub(crate) fn work_kick(&self, job: Job) {
        self.kick_work.push("kick", job);
    }

    /// Webhook work dropped because a queue was full: (Twitch, Kick).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn dropped_work(&self) -> (u64, u64) {
        (self.twitch_work.dropped.load(Ordering::Relaxed), self.kick_work.dropped.load(Ordering::Relaxed))
    }

    /// A refused EventSub signature: a warning the first time in a minute, debug after (a
    /// flood of junk never floods the log).
    pub(crate) fn note_bad_signature(&self, now: i64) -> bool {
        let mut b = self.bad_sig.lock().unwrap_or_else(|p| p.into_inner());
        let minute = now.div_euclid(60_000);
        if b.0 != minute {
            let before = b.1;
            *b = (minute, 0);
            tracing::warn!(
                refused_before = before,
                "refused an EventSub message with a bad signature (if this repeats, TWITCH_CLOUD_EVENTSUB_SECRET may differ from the secret on the conduit's shard: run bot-check)"
            );
            true
        } else {
            b.1 += 1;
            tracing::debug!("refused an EventSub message with a bad signature");
            false
        }
    }

    /// A refused EventSub timestamp (too old or from the future): like a bad signature.
    pub(crate) fn note_stale(&self, now: i64) -> bool {
        let mut b = self.stale.lock().unwrap_or_else(|p| p.into_inner());
        let minute = now.div_euclid(60_000);
        if b.0 != minute {
            let before = b.1;
            *b = (minute, 0);
            tracing::warn!(
                refused_before = before,
                "refused an EventSub message with a stale timestamp (is the server's clock right?)"
            );
            true
        } else {
            b.1 += 1;
            tracing::debug!("refused an EventSub message with a stale timestamp");
            false
        }
    }

    /// Whether a rare event (`what`) on `sub`'s `platform` should be logged at info now:
    /// the first time, then once per [`NOTE_EVERY_MS`].
    pub(crate) fn note(&self, sub: &str, platform: Platform, what: &'static str, now: i64) -> bool {
        let mut m = self.notes.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 10_000 {
            m.retain(|_, at| now - *at < NOTE_EVERY_MS);
        }
        let key = (sub.to_string(), platform, what);
        match m.get(&key) {
            Some(at) if now - at < NOTE_EVERY_MS => false,
            _ => {
                m.insert(key, now);
                true
            }
        }
    }

    /// An app access token, cached until a minute before it ends.
    pub(crate) async fn app_token(&self) -> Result<String, TwitchError> {
        let mut slot = self.app_token.lock().await;
        if let Some((t, until)) = slot.as_ref()
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let api = self.twitch.as_deref().ok_or_else(|| TwitchError::Unavailable("not configured".into()))?;
        let (t, life) = api.app_token().await?;
        *slot = Some((t.clone(), Instant::now() + Duration::from_secs(life.saturating_sub(60).max(60))));
        Ok(t)
    }

    pub(crate) async fn forget_app_token(&self) {
        *self.app_token.lock().await = None;
    }

    /// The conduit, set up once per relay run (its shard points at this relay). Its id is
    /// kept in `bot_kv`: when Twitch hands back another one (the old one was deleted), every
    /// bound Twitch channel is subscribed again on it, or chat would stop arriving.
    pub(crate) fn conduit<'a>(&'a self, st: &'a AppState) -> Fut<'a, String, TwitchError> {
        self.conduit_try(st, 1)
    }

    fn conduit_try<'a>(&'a self, st: &'a AppState, attempt: u32) -> Fut<'a, String, TwitchError> {
        // Boxed: setting it up may subscribe channels again, which asks for the conduit.
        Box::pin(self.conduit_inner(st, attempt))
    }

    async fn conduit_inner(&self, st: &AppState, attempt: u32) -> Result<String, TwitchError> {
        let (id, changed) = {
            let mut slot = self.conduit.lock().await;
            if let Some(c) = slot.as_ref() {
                return Ok(c.clone());
            }
            let (api, cfg) = match (self.twitch.as_deref(), self.twitch_cfg.as_ref()) {
                (Some(a), Some(c)) => (a, c),
                _ => return Err(TwitchError::Unavailable("not configured".into())),
            };
            let app = self.app_token().await?;
            let id = api.conduit(&app, &twitch::callback_url(st), &cfg.eventsub_secret).await?;
            let before = st.db.bot_kv(store::KV_CONDUIT).ok().flatten();
            if before.as_deref() != Some(id.as_str())
                && let Err(e) = st.db.bot_put_kv(store::KV_CONDUIT, &id)
            {
                tracing::warn!(error = %e, "couldn't remember Twitch's conduit");
            }
            *slot = Some(id.clone());
            let changed = before.is_some_and(|b| b != id);
            (id, changed)
        };
        // Twitch checks the callback after the PATCH answered: look again a little later.
        tokio::spawn(check_shard(st.clone(), id.clone(), attempt));
        if changed {
            tracing::info!("Twitch's EventSub conduit changed: subscribing the cloud bot's channels again");
            resubscribe_all(st).await;
        }
        Ok(id)
    }

    pub(crate) fn kick_access(&self, sub: &str, now: i64) -> Option<String> {
        let m = self.kick_access.lock().unwrap_or_else(|p| p.into_inner());
        m.get(sub).filter(|(_, until)| now < until - 60_000).map(|(t, _)| t.clone())
    }

    pub(crate) fn cache_kick_access(&self, sub: &str, token: &str, until: i64) {
        self.kick_access.lock().unwrap_or_else(|p| p.into_inner()).insert(sub.to_string(), (token.to_string(), until));
    }

    pub(crate) fn forget_kick_access(&self, sub: &str) {
        self.kick_access.lock().unwrap_or_else(|p| p.into_inner()).remove(sub);
    }

    /// Forgets the cached token only if it's still `token` (Kick refused it).
    pub(crate) fn forget_kick_access_if(&self, sub: &str, token: &str) {
        let mut m = self.kick_access.lock().unwrap_or_else(|p| p.into_inner());
        if m.get(sub).is_some_and(|(t, _)| t == token) {
            m.remove(sub);
        }
    }

    /// The bot account's cached Twitch access token (for moderation), if it lasts a minute more.
    pub(crate) fn twitch_user(&self, sub: &str, now: i64) -> Option<String> {
        let m = self.twitch_user.lock().unwrap_or_else(|p| p.into_inner());
        m.get(sub).filter(|(_, until)| now < until - 60_000).map(|(t, _)| t.clone())
    }

    pub(crate) fn cache_twitch_user(&self, sub: &str, token: &str, until: i64) {
        self.twitch_user.lock().unwrap_or_else(|p| p.into_inner()).insert(sub.to_string(), (token.to_string(), until));
    }

    pub(crate) fn forget_twitch_user(&self, sub: &str) {
        self.twitch_user.lock().unwrap_or_else(|p| p.into_inner()).remove(sub);
    }

    /// Forgets the cached token only if it's still `token` (Twitch refused it; a newer one
    /// another request got meanwhile stays).
    pub(crate) fn forget_twitch_user_if(&self, sub: &str, token: &str) {
        let mut m = self.twitch_user.lock().unwrap_or_else(|p| p.into_inner());
        if m.get(sub).is_some_and(|(t, _)| t == token) {
            m.remove(sub);
        }
    }

    pub(crate) fn twitch_lock(&self, sub: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.twitch_locks.lock().unwrap_or_else(|p| p.into_inner()).entry(sub.to_string()).or_default().clone()
    }

    pub(crate) fn kick_lock(&self, sub: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.kick_locks.lock().unwrap_or_else(|p| p.into_inner()).entry(sub.to_string()).or_default().clone()
    }

    fn lease<T>(&self, sub: &str, platform: Platform, f: impl FnOnce(&mut Lease) -> T) -> T {
        let mut m = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        let l = m.entry((sub.to_string(), platform)).or_insert_with(|| Lease::new(self.started.0, self.started.1));
        f(l)
    }

    fn mem<T>(&self, sub: &str, f: impl FnOnce(&mut SubMem) -> T) -> T {
        let mut m = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        f(m.entry(sub.to_string()).or_default())
    }

    fn forget_mem(&self, sub: &str, platform: Option<Platform>) {
        let mut m = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        match platform {
            None => {
                m.remove(sub);
                self.forget_twitch_user(sub);
                self.forget_yt(sub);
            }
            Some(p) => {
                if p == Platform::Twitch {
                    self.forget_twitch_user(sub);
                }
                if p == Platform::Youtube {
                    self.forget_yt(sub);
                }
                if let Some(s) = m.get_mut(sub) {
                    s.brains.remove(&p);
                    s.ring.remove(&p);
                    s.seen.remove(&p);
                    s.sent.remove(&p);
                }
            }
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/pro/bot", get(status))
        .route("/v1/pro/bot/config", put(put_config))
        .route("/v1/pro/bot/enabled", put(put_enabled))
        .route("/v1/pro/bot/{platform}/connect", post(connect))
        .route("/v1/pro/bot/{platform}", delete(disconnect))
        .route("/v1/bot/twitch/callback", get(twitch::callback))
        .route("/v1/bot/twitch/eventsub", post(twitch::eventsub))
        .route("/v1/bot/youtube/callback", get(youtube::callback))
}

/// `features.cloud_bot`: a vault for the sign-ins and at least one platform set up.
pub(crate) fn available(st: &AppState) -> bool {
    st.vault().is_ok() && (st.bot.twitch.is_some() || st.bot.kick.is_some() || st.bot.youtube.is_some())
}

/// Its per-subscription counters in `/v1/pro/status` `quotas`: none per day or month (one
/// channel per platform, 50 timers and 500 commands are totals in the view's `limits`).
pub(crate) const QUOTAS: &[Limit] = &[];

// ── Keys, codes, small helpers ───────────────────────────────────────────────────────

/// HKDF-SHA256 (RFC 5869, one block): a 32-byte key for one `label` from a secret, so the
/// state signatures and PC codes never use an app secret itself as their key.
pub(crate) fn derive_key(secret: &[u8], label: &str) -> [u8; 32] {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut extract = Hmac::<Sha256>::new_from_slice(b"ferrocast relay cloud bot hkdf v1").expect("hmac takes any key");
    extract.update(secret);
    let prk = extract.finalize().into_bytes();
    #[allow(clippy::expect_used)]
    let mut expand = Hmac::<Sha256>::new_from_slice(&prk).expect("hmac takes any key");
    expand.update(label.as_bytes());
    expand.update(&[1]);
    expand.finalize().into_bytes().into()
}

/// A short code for an install ("7F3A-90C2", like the Discord bot's), shown on the sign-in
/// pages and in the dashboard, so a streamer can see a sign-in went to *their* PC. It gives
/// nothing about the install id away.
pub(crate) fn pc_code(st: &AppState, install: &str) -> String {
    let secret = st
        .bot
        .twitch_cfg()
        .map(|c| c.client_secret.clone())
        .or_else(|| st.cfg.kick_client_secret.clone())
        .or_else(|| st.bot.youtube_cfg().map(|c| c.client_secret.clone()))
        .unwrap_or_default();
    let key = derive_key(secret.as_bytes(), "cloud bot pc code v1");
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("hmac takes any key");
    mac.update(install.as_bytes());
    let b = mac.finalize().into_bytes();
    format!("{:02X}{:02X}-{:02X}{:02X}", b[0], b[1], b[2], b[3])
}

/// Database work off the async threads (webhook paths).
pub(crate) async fn db<T: Send + 'static>(
    st: &AppState,
    f: impl FnOnce(&Db) -> rusqlite::Result<T> + Send + 'static,
) -> rusqlite::Result<T> {
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || f(&db)).await.unwrap_or(Err(rusqlite::Error::InvalidQuery))
}

pub(crate) fn storage_busy() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage", "The relay's storage is busy.")
        .hint("Twitch sends the message again by itself.")
}

pub(crate) fn platform_unavailable(platform: &str) -> ApiError {
    let name = match platform {
        "twitch" => "Twitch",
        "kick" => "Kick",
        "youtube" => "YouTube",
        _ => "This platform",
    };
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "platform_unavailable",
        format!("The cloud bot for {name} isn't set up on Ferrocast's servers yet."),
    )
    .hint("The bot on this PC keeps working as before.")
}

fn platform_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "platform_not_found", "The cloud bot runs on Twitch, Kick and YouTube.")
        .hint("Use twitch, kick or youtube.")
}

/// The platform in a path: Twitch, Kick or YouTube (a beta, M12).
fn platform_of(raw: &str) -> Result<(Platform, &'static str), ApiError> {
    match raw {
        "twitch" => Ok((Platform::Twitch, "twitch")),
        "kick" => Ok((Platform::Kick, "kick")),
        "youtube" => Ok((Platform::Youtube, "youtube")),
        _ => Err(platform_not_found()),
    }
}

fn key(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
        Platform::Youtube => "youtube",
        _ => "other",
    }
}

fn platform_from_key(k: &str) -> Option<Platform> {
    match k {
        "twitch" => Some(Platform::Twitch),
        "kick" => Some(Platform::Kick),
        "youtube" => Some(Platform::Youtube),
        _ => None,
    }
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

/// `{code, message, hint, at}` as kept on a channel.
pub(crate) fn error_json(code: &str, message: &str, hint: &str) -> Value {
    json!({ "code": code, "message": message, "hint": hint, "at": now_ms() })
}

/// The vault's purpose for a sealed token: `bot_<platform>_<bot|channel>` (AAD `sub|purpose`).
pub(crate) fn token_purpose(platform: &str, purpose: &str) -> String {
    format!("bot_{platform}_{purpose}")
}

/// A stored refresh token, opened. `None` when there's none or it doesn't open (logged).
pub(crate) fn open_token(st: &AppState, sub: &str, platform: &str, purpose: &str) -> Option<String> {
    let sealed = st.db.bot_token(sub, platform, purpose).ok().flatten()?;
    let vault = st.vault().ok()?;
    match vault.open(sub, &token_purpose(platform, purpose), &sealed) {
        Ok(b) => String::from_utf8(b).ok(),
        Err(e) => {
            crate::vault::damaged(&e);
            None
        }
    }
}

/// Seals and stores a (new) refresh token.
pub(crate) fn seal_token(st: &AppState, sub: &str, platform: &str, purpose: &str, token: &str) {
    let Ok(vault) = st.vault() else { return };
    match vault.seal(sub, &token_purpose(platform, purpose), token.as_bytes()) {
        Ok(s) => {
            if let Err(e) = st.db.bot_put_token(sub, platform, purpose, &s) {
                tracing::warn!(error = %e, "couldn't save a refreshed sign-in");
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't seal a refreshed sign-in"),
    }
}

/// The install's subscription, if it grants Pro now.
pub(crate) fn pro_for_install(st: &AppState, install: &str) -> rusqlite::Result<Option<ProCtx>> {
    Ok(st
        .db
        .pro_sub_for_install(install)?
        .filter(|s| crate::billing::access::is_pro(&s.access(), now_ms()))
        .map(|s| ProCtx { install: install.to_string(), sub_id: s.id }))
}

fn sub_is_pro(db: &Db, sub: &str) -> bool {
    // A cloud mode account (docs/CLOUD.md §6): "Pro" means cloud mode on and awake.
    if crate::cloud::is_account_key(sub) {
        return crate::cloud::bot_allowed(db, sub);
    }
    matches!(db.pro_sub(sub), Ok(Some(s)) if crate::billing::access::is_pro(&s.access(), now_ms()))
}

/// The cloud bot is switched on for `sub` and its Pro is active.
fn sub_is_on(db: &Db, sub: &str) -> rusqlite::Result<bool> {
    Ok(db.bot_config(sub)?.is_some_and(|r| r.enabled) && sub_is_pro(db, sub))
}

/// A body read as bytes, so the Pro gate answers before the body is judged.
#[derive(Debug)]
pub(crate) struct RawBody(axum::body::Bytes);

impl<S: Send + Sync> axum::extract::FromRequest<S> for RawBody {
    type Rejection = ApiError;
    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(RawBody).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The bot settings can be up to 256 KB.")
                    .hint("Remove some commands or long replies.")
            } else {
                invalid("The request body couldn't be read.")
            }
        })
    }
}

fn json_body<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &[u8], required: bool) -> Result<T, ApiError> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("application/json"));
    if body.is_empty() && !required {
        return serde_json::from_slice(b"{}").map_err(|_| invalid("The request isn't JSON."));
    }
    if !is_json {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type", "Send JSON.")
            .hint("Set Content-Type: application/json."));
    }
    serde_json::from_slice::<T>(body).map_err(|e| {
        if e.is_syntax() || e.is_eof() {
            invalid("The request isn't JSON.")
        } else {
            invalid(format!("The request has a missing or wrong field: {e}"))
        }
    })
}

fn take_rate(st: &AppState, sub: &str) -> Result<(), ApiError> {
    match st.db.try_take(sub, RATE.name, RATE.period, 1, RATE.limit) {
        Ok(()) => Ok(()),
        Err(TakeErr::Over(q)) => {
            let wait = u64::try_from(q.reset_at - now_ms()).unwrap_or(0).div_ceil(1000).max(1);
            Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "bot_rate", "Too many cloud bot requests. Wait a minute.")
                .header(HeaderName::from_static("x-quota-reset"), q.reset_at.to_string())
                .header(header::RETRY_AFTER, wait.to_string()))
        }
        Err(TakeErr::Db(e)) => Err(ApiError::db(e)),
    }
}

/// The Pro gate, then the per-subscription rate.
async fn pro(st: &AppState, headers: &HeaderMap) -> Result<ProCtx, ApiError> {
    let ctx = require_pro(st, headers).await?;
    take_rate(st, &ctx.sub_id)?;
    Ok(ctx)
}

fn no_store(v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    r
}

// ── Config in memory ─────────────────────────────────────────────────────────────────

/// Applies counter changes the PCs haven't taken yet on top of a config's values, in order:
/// `add` steps add, `set` sets.
fn overlay_changes(cfg: &mut BotConfig, changes: &[store::CounterChange]) {
    for ch in changes {
        if let Some(c) = cfg.counters.iter_mut().find(|c| c.name == ch.name) {
            c.value = match ch.op.as_str() {
                "add" => ferrocast_core::chatbot::apply_counter(c.value, CounterOp::Add(ch.delta)),
                _ => ch.value,
            };
        }
    }
}

/// Makes sure the subscription's config is in memory; its version.
fn load_config(st: &AppState, sub: &str) -> Option<i64> {
    if let Some(v) = st.bot.mem(sub, |m| m.config.as_ref().map(|(v, _)| *v)) {
        return Some(v);
    }
    let row = st.db.bot_config(sub).ok().flatten()?;
    // No PC ever pushed this subscription's config (version 0): its YouTube bot uses the
    // cloud mode account's Bot page (docs/CLOUD.md §11), not an empty config.
    if row.version == 0
        && !crate::cloud::is_account_key(sub)
        && let Some(cfg) = account_config_for_sub(st, sub)
    {
        st.bot.mem(sub, |m| {
            if m.config.is_none() {
                m.config = Some((0, cfg));
            }
        });
        return Some(0);
    }
    let mut cfg: BotConfig = serde_json::from_str(&row.json).unwrap_or_default();
    overlay_changes(&mut cfg, &st.db.bot_counter_changes(sub).unwrap_or_default());
    let v = row.version;
    st.bot.mem(sub, |m| {
        if m.config.is_none() {
            m.config = Some((v, cfg));
        }
    });
    Some(v)
}

// ── The view ─────────────────────────────────────────────────────────────────────────

fn channel_view(st: &AppState, ch: &store::Channel, duty: &str) -> Value {
    let bound = ch.bot_id.is_some() && ch.channel_id.is_some();
    let state = if bound && ch.blocked() {
        "error"
    } else if bound {
        "ready"
    } else {
        "needs_channel"
    };
    let acct = |id: &Option<String>, login: &Option<String>| match id {
        Some(i) => json!({ "id": i, "login": login.clone().unwrap_or_default() }),
        None => Value::Null,
    };
    let error = ch.error.clone().map(|mut e| {
        // A retryable error gets a "Try again" (re-subscribes); a blocking one needs Connect.
        e["retry"] = json!(bound && !ch.blocked());
        e
    });
    let mut v = json!({
        "platform": ch.platform, "state": state,
        "bot": acct(&ch.bot_id, &ch.bot_login), "channel": acct(&ch.channel_id, &ch.channel_login),
        "moderation": ch.moderation, "connected_at": ch.connected_at, "last_reply_at": ch.last_reply_at,
        "error": error, "last_drop": ch.last_drop, "live": ch.live_since.is_some(), "duty": duty,
    });
    if ch.platform == "youtube" {
        // A beta with a daily budget of YouTube's quota units (M12).
        v["beta"] = json!(true);
        v["quota"] = youtube::quota_view(st, &ch.sub_id);
    }
    v
}

/// Who's on duty on one platform now: (`desktop`|`cloud`, since).
fn platform_duty(st: &AppState, sub: &str, platform: Platform, now: Instant) -> (Who, i64, Option<i64>) {
    st.bot.lease(sub, platform, |l| (l.who(now), l.since(now), l.seen_at()))
}

/// Whether the cloud answers on `platform` for `sub` right now.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn cloud_on_duty(st: &AppState, sub: &str, platform: Platform) -> bool {
    st.bot.lease(sub, platform, |l| l.who(Instant::now()) == Who::Cloud)
}

/// Since when the cloud has `platform` (wall ms), if it has it.
fn cloud_since(st: &AppState, sub: &str, platform: Platform) -> Option<i64> {
    let now = Instant::now();
    st.bot.lease(sub, platform, |l| (l.who(now) == Who::Cloud).then(|| l.since(now)))
}

fn view(st: &AppState, sub: &str, install: &str) -> Result<Value, ApiError> {
    let row = st.db.bot_config(sub).map_err(ApiError::db)?;
    let channels = st.db.bot_channels(sub).map_err(ApiError::db)?;
    let enabled = row.as_ref().is_some_and(|r| r.enabled);
    let now = Instant::now();
    let mut seen: Option<i64> = None;
    let mut cloud_since: Option<i64> = None;
    let mut desk_since: Option<i64> = None;
    let mut views = Vec::new();
    for ch in &channels {
        let p = platform_from_key(&ch.platform);
        let duty = match p {
            Some(p) if enabled && ch.ready() => {
                let (who, since, s) = platform_duty(st, sub, p, now);
                seen = seen.max(s);
                match who {
                    Who::Cloud => {
                        cloud_since = Some(cloud_since.map_or(since, |c| c.min(since)));
                        "cloud"
                    }
                    Who::Desktop => {
                        desk_since = Some(desk_since.map_or(since, |c| c.min(since)));
                        "desktop"
                    }
                }
            }
            _ => "off",
        };
        views.push(channel_view(st, ch, duty));
    }
    if seen.is_none() {
        seen = PLATFORMS.iter().filter_map(|p| platform_duty(st, sub, *p, now).2).max();
    }
    let (who, since) = match (cloud_since, desk_since) {
        (Some(c), _) => ("cloud", Some(c)),
        (None, Some(d)) => ("desktop", Some(d)),
        (None, None) => ("off", None),
    };
    let this_pc = st.bot.seen_by.lock().unwrap_or_else(|p| p.into_inner()).get(sub).is_some_and(|(i, _)| i == install);
    load_config(st, sub);
    let counts = st.bot.mem(sub, |m| match &m.config {
        Some((_, c)) => json!({ "commands": c.commands.len(), "timers": c.timers.len(),
                                 "counters": c.counters.len(), "auto_replies": c.auto_replies.len() }),
        None => json!({ "commands": 0, "timers": 0, "counters": 0, "auto_replies": 0 }),
    });
    Ok(json!({
        "enabled": enabled,
        "duty": { "who": who, "since": since, "desktop_seen_at": seen, "this_pc": this_pc },
        "version": row.as_ref().map_or(0, |r| r.version),
        "config_updated_at": row.as_ref().filter(|r| r.version > 0).map(|r| r.updated_at),
        "counts": counts,
        "channels": views,
        "available": { "twitch": st.bot.has(Platform::Twitch), "kick": st.bot.has(Platform::Kick),
                       "youtube": st.bot.has(Platform::Youtube) },
        "pc_code": pc_code(st, install),
        "limits": {
            "commands": cloudbot::MAX_COMMANDS, "timers": cloudbot::MAX_TIMERS,
            "config_bytes": cloudbot::MAX_CONFIG_BYTES, "messages_per_minute": MESSAGES_PER_MINUTE,
            "channels_per_platform": 1, "takeover_after_s": TAKEOVER_AFTER.as_secs(),
            "youtube_daily_units": st.bot.youtube_cfg().map(|c| c.units_per_sub),
        },
    }))
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

/// `GET /v1/pro/bot`.
async fn status(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    Ok(no_store(view(&st, &ctx.sub_id, &ctx.install)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigIn {
    version: i64,
    config: Value,
    #[serde(default)]
    counters_seen: i64,
}

/// `PUT /v1/pro/bot/config {version, config, counters_seen?}`.
async fn put_config(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: ConfigIn = json_body(&headers, &body, true)?;
    if b.version < 0 || b.counters_seen < 0 {
        return Err(invalid("version and counters_seen are whole numbers from 0."));
    }
    let cfg: BotConfig = serde_json::from_value(b.config).map_err(|e| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_config", format!("config: {e}"))
            .hint("Update Ferrocast on this PC: the relay didn't understand these bot settings.")
    })?;
    cfg.validate().map_err(|e| match e {
        ConfigError::TooManyCommands => {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_many_commands", e.to_string())
                .hint("The bot on this PC still runs all of them.")
        }
        ConfigError::TooManyTimers => ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_many_timers", e.to_string())
            .hint("The bot on this PC still runs all of them."),
        ConfigError::Invalid(m) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_config", m)
            .hint("Fix that item on the Bot page; the cloud keeps the last good settings."),
    })?;
    let json = serde_json::to_string(&cfg).map_err(|_| invalid("The bot settings couldn't be read."))?;
    let now = now_ms();
    let version = match st.db.bot_push_config(&ctx.sub_id, b.version, &json, b.counters_seen, now) {
        Ok(v) => v,
        Err(store::PushErr::Stale(current)) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "stale_version",
                format!("The bot settings changed on another PC (version {current})."),
            )
            .hint("Ferrocast reads the new version and sends this PC's settings again."));
        }
        Err(store::PushErr::Db(e)) => return Err(ApiError::db(e)),
    };
    let mut cfg = cfg;
    overlay_changes(&mut cfg, &st.db.bot_counter_changes(&ctx.sub_id).map_err(ApiError::db)?);
    st.bot.mem(&ctx.sub_id, |m| m.config = Some((version, cfg)));
    Ok(no_store(json!({ "version": version, "updated_at": now })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledIn {
    enabled: bool,
}

/// `PUT /v1/pro/bot/enabled {enabled}`. Off: Twitch stops sending chat (its subscriptions
/// are deleted). On: they're made again and the live status is read.
async fn put_enabled(
    State(st): State<AppState>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let b: EnabledIn = json_body(&headers, &body, true)?;
    Ok(no_store(set_enabled(&st, &ctx, b.enabled).await?))
}

/// `PUT /r/api/v1/pro/bot/enabled` from a phone while the PC is off (M10): the same switch,
/// answered by the relay itself (the bot's own rate, then the body).
pub(crate) async fn phone_set_enabled(
    st: &AppState,
    ctx: &ProCtx,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    take_rate(st, &ctx.sub_id)?;
    let b: EnabledIn = json_body(headers, body, true)?;
    set_enabled(st, ctx, b.enabled).await
}

/// Switches the cloud bot on or off for `ctx`'s subscription; the view.
async fn set_enabled(st: &AppState, ctx: &ProCtx, enabled: bool) -> Result<Value, ApiError> {
    let st = st.clone();
    let b = EnabledIn { enabled };
    let channels = st.db.bot_channels(&ctx.sub_id).map_err(ApiError::db)?;
    if b.enabled {
        if !channels.iter().any(store::Channel::ready) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "no_platform",
                "Connect Twitch, Kick or YouTube for the cloud bot first.",
            )
            .hint("Press Connect next to a platform below."));
        }
        // The 90 s start again, so switching on never jumps in front of a PC that's on.
        for p in PLATFORMS {
            st.bot.lease(&ctx.sub_id, p, |l| l.restart(Instant::now(), now_ms()));
        }
    }
    let was_on = st.db.bot_config(&ctx.sub_id).map_err(ApiError::db)?.is_some_and(|r| r.enabled);
    st.db.bot_set_enabled(&ctx.sub_id, b.enabled, now_ms()).map_err(ApiError::db)?;
    for ch in channels.iter().filter(|c| c.platform == "twitch" && c.ready()) {
        if b.enabled && (!was_on || ch.subscriptions.is_empty()) {
            // A problem here shows on the channel (error with Try again); the switch stays on.
            let _ = twitch::subscribe_channel(&st, ch).await;
            twitch::reconcile_live(&st, ch).await;
        } else if !b.enabled {
            twitch::unsubscribe_all(&st, ch).await;
        }
    }
    if !b.enabled {
        // YouTube is never asked anything while it's off.
        st.bot.yt(&ctx.sub_id, youtube::YtMem::stop);
        // Nothing of the chat is kept while it's off.
        for p in PLATFORMS {
            st.bot.mem(&ctx.sub_id, |m| {
                m.ring.remove(&p);
            });
        }
    }
    // Phones of a PC that's off hear whether chat and bans still work.
    crate::remote::cloud_changed(&st, &ctx.sub_id);
    view(&st, &ctx.sub_id, &ctx.install)
}

/// Whether the cloud bot can stand in for a PC that's off (M10): `on` when it's switched on
/// with a ready channel and Pro, `paused` when a ready channel is there but it's switched off;
/// `moderation`: per platform, whether it can ban, time out and delete there (only while
/// `on`, with the opt-in and every moderator scope granted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CloudState {
    pub on: bool,
    pub paused: bool,
    pub moderation: ModCaps,
}

/// Where the cloud bot can moderate (`cloud_moderation` on `/r/v1/me` and the `remote` frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ModCaps {
    pub twitch: bool,
    pub kick: bool,
    pub youtube: bool,
}

impl ModCaps {
    /// Bans work on at least one platform.
    pub(crate) fn any(self) -> bool {
        self.twitch || self.kick || self.youtube
    }

    pub(crate) fn json(self) -> Value {
        json!({ "twitch": self.twitch, "kick": self.kick, "youtube": self.youtube })
    }
}

/// The opt-in with every moderator scope the platform needs (bans, timeouts and deletes).
pub(crate) fn can_moderate(c: &store::Channel, platform: Platform) -> bool {
    let needed: &[&str] = match platform {
        Platform::Twitch => twitch::MOD_SCOPES,
        Platform::Kick => kick::MOD_SCOPES,
        Platform::Youtube => youtube::MOD_SCOPES,
        _ => return false,
    };
    c.moderation && needed.iter().all(|n| c.scopes.split_whitespace().any(|s| s == *n))
}

pub(crate) fn cloud_state(st: &AppState, sub: &str) -> CloudState {
    if !available(st) || !sub_is_pro(&st.db, sub) {
        return CloudState::default();
    }
    let channels = st.db.bot_channels(sub).unwrap_or_default();
    let usable: Vec<(Platform, &store::Channel)> = channels
        .iter()
        .filter(|c| c.ready())
        .filter_map(|c| platform_from_key(&c.platform).filter(|p| st.bot.has(*p)).map(|p| (p, c)))
        .collect();
    let ready = !usable.is_empty();
    let enabled = st.db.bot_config(sub).ok().flatten().is_some_and(|r| r.enabled);
    let on = ready && enabled;
    let can = |want: Platform| on && usable.iter().any(|(p, c)| *p == want && can_moderate(c, want));
    CloudState {
        on,
        paused: ready && !enabled,
        moderation: ModCaps {
            twitch: can(Platform::Twitch),
            kick: can(Platform::Kick),
            youtube: can(Platform::Youtube),
        },
    }
}

/// The last chat lines the cloud kept for `sub` (every platform, oldest first, at most
/// `limit`), as the dashboard's chat shows them: the phone's chat while the PC is off.
pub(crate) fn recent_events(st: &AppState, sub: &str, limit: usize) -> Vec<Arc<Event>> {
    let mut all: Vec<(i64, Arc<Event>)> =
        st.bot.mem(sub, |m| m.ring.values().flat_map(|r| r.iter().map(|l| (l.at, l.event.clone()))).collect());
    all.sort_by_key(|(at, _)| *at);
    let skip = all.len().saturating_sub(limit);
    all.into_iter().skip(skip).map(|(_, e)| e).collect()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ConnectIn {
    account: Option<String>,
    moderation: bool,
    /// The PC's own desktop-app Twitch or YouTube token: proves whose channel it is. Only
    /// checked with the platform, never kept or logged.
    owner_token: Option<String>,
    /// Try a connected channel's retryable error again (re-subscribes) instead of signing in.
    retry: bool,
}

fn already_connected(platform: &str) -> ApiError {
    let name = match platform {
        "twitch" => "Twitch",
        "youtube" => "YouTube",
        _ => "Kick",
    };
    ApiError::new(StatusCode::CONFLICT, "already_connected", format!("{name} is already connected for the cloud bot."))
        .hint("Disconnect it first to use another account.")
}

/// `POST /v1/pro/bot/{platform}/connect {account?, moderation?, owner_token?, retry?}` → 201
/// `{auth_url, expires_at}`, or with `retry` 200 and the view.
async fn connect(
    State(st): State<AppState>,
    Path(platform): Path<String>,
    headers: HeaderMap,
    RawBody(body): RawBody,
) -> Result<Response, ApiError> {
    let ctx = pro(&st, &headers).await?;
    let (_, p) = platform_of(&platform)?;
    let b: ConnectIn = json_body(&headers, &body, false)?;
    // Not set up here first (the more useful answer), then the vault for the sign-ins.
    if !st.bot.has(platform_from_key(p).unwrap_or(Platform::Kick)) {
        return Err(platform_unavailable(p));
    }
    st.vault()?;
    let existing = st.db.bot_channel(&ctx.sub_id, p).map_err(ApiError::db)?;
    if b.retry {
        return retry(&st, &ctx, p, existing).await;
    }
    let now = now_ms();
    let auth_url = match p {
        "twitch" => {
            let cfg = st.bot.twitch_cfg().ok_or_else(|| platform_unavailable("twitch"))?;
            let step = match b.account.as_deref().unwrap_or("bot") {
                "bot" => {
                    // Signing the bot account in again with the moderator scopes (the opt-in,
                    // M10) keeps the channel; anything else needs a disconnect first.
                    if existing.as_ref().is_some_and(|c| c.bot_id.is_some() && !(b.moderation && !c.moderation)) {
                        return Err(already_connected(p));
                    }
                    twitch::Step::Bot { moderation: b.moderation }
                }
                "channel" => {
                    match &existing {
                        Some(c) if c.channel_id.is_some() => return Err(already_connected(p)),
                        Some(c) if c.bot_id.is_some() => {}
                        _ => {
                            return Err(ApiError::new(
                                StatusCode::CONFLICT,
                                "bot_account_first",
                                "Connect the bot account first.",
                            )
                            .hint("Then allow it in your channel with your own Twitch account."));
                        }
                    }
                    twitch::Step::Channel
                }
                _ => return Err(invalid("account: bot or channel.")),
            };
            // Both steps are tied to the Twitch account this PC is signed in to.
            let (owner_id, owner_login) = twitch::owner_of(&st, cfg, b.owner_token.as_deref()).await?;
            twitch::auth_url(&st, cfg, step, &ctx.install, (&owner_id, &owner_login), now)?
        }
        "youtube" => {
            let cfg = st.bot.youtube_cfg().ok_or_else(|| platform_unavailable("youtube"))?;
            // One step, the channel itself; again only to add the moderation opt-in.
            if existing.as_ref().is_some_and(|c| c.bot_id.is_some() && !(b.moderation && !c.moderation)) {
                return Err(already_connected(p));
            }
            if b.account.as_deref().is_some_and(|a| a != "bot" && a != "channel") {
                return Err(invalid("account: YouTube has one step, your channel."));
            }
            // Tied to the channel this PC is signed in to.
            let owner = youtube::owner_of(&st, cfg, b.owner_token.as_deref()).await?;
            youtube::auth_url(&st, cfg, b.moderation, &ctx.install, &owner, now)?
        }
        _ => {
            st.bot.kick()?;
            // Signing in again with the moderation scopes (the opt-in, M10) is the one way to
            // connect over an existing Kick sign-in.
            if existing.as_ref().is_some_and(|c| !(b.moderation && !c.moderation)) {
                return Err(already_connected(p));
            }
            if b.account.as_deref().is_some_and(|a| a != "bot") {
                return Err(invalid("account: Kick has one step, the bot account."));
            }
            if st.db.bot_kick_channel_of(&ctx.install).map_err(ApiError::db)?.is_none() {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "kick_not_linked",
                    "Connect Kick in Accounts on this PC first.",
                )
                .hint("Kick's chat reaches the cloud through that connection."));
            }
            kick::auth_url(&st, &ctx, b.moderation)?
        }
    };
    Ok((StatusCode::CREATED, no_store(json!({ "auth_url": auth_url, "expires_at": now + STATE_TTL_MS })))
        .into_response())
}

/// `connect {retry: true}`: tries a connected channel again (Twitch: subscribes it again;
/// Kick: clears the error so the next message tries). 200 with the view.
async fn retry(
    st: &AppState,
    ctx: &ProCtx,
    p: &'static str,
    existing: Option<store::Channel>,
) -> Result<Response, ApiError> {
    let Some(ch) = existing.filter(|c| c.bot_id.is_some() && c.channel_id.is_some()) else {
        return Err(ApiError::new(StatusCode::CONFLICT, "not_connected", "Connect it for the cloud bot first.")
            .hint("Press Connect next to the platform."));
    };
    if ch.blocked() {
        return Err(ApiError::new(StatusCode::CONFLICT, "connect_again", "This sign-in has to be made again.")
            .hint("Disconnect it here and connect it again."));
    }
    if p == "twitch" {
        // The outcome is on the channel (cleared, or the new error with its hint).
        let _ = twitch::subscribe_channel(st, &ch).await;
        twitch::reconcile_live(st, &ch).await;
    } else if p == "youtube" {
        // Looked at again on the next pass (if the budget allows).
        st.db.bot_clear_transient_error(&ch.sub_id, p).map_err(ApiError::db)?;
        st.bot.yt(&ch.sub_id, |m| m.retry_now());
    } else {
        st.db.bot_clear_transient_error(&ch.sub_id, p).map_err(ApiError::db)?;
    }
    Ok(no_store(view(st, &ctx.sub_id, &ctx.install)?))
}

/// Revokes a platform's sign-ins there, deletes Twitch's subscriptions and the rows (a
/// Disconnect, the lapse sweep).
pub(crate) async fn disconnect_platform(st: &AppState, sub: &str, platform: Platform) {
    let p = key(platform);
    if let Ok(Some(ch)) = st.db.bot_channel(sub, p) {
        match platform {
            Platform::Twitch => {
                twitch::unsubscribe_all(st, &ch).await;
                for purpose in st.db.bot_token_purposes(sub, p).unwrap_or_default() {
                    twitch::revoke_stored(st, sub, &purpose).await;
                }
            }
            Platform::Kick => kick::revoke_stored(st, sub).await,
            Platform::Youtube => youtube::revoke_stored(st, sub).await,
            _ => {}
        }
    }
    forget_platform(st, sub, platform);
}

/// Deletes a platform's rows and memory without revoking anything (YouTube: a binding whose
/// Google grant is the same as a newer sign-in for that channel, which a revoke would end).
pub(crate) fn forget_platform(st: &AppState, sub: &str, platform: Platform) {
    let p = key(platform);
    if let Err(e) = st.db.bot_forget_channel(sub, p) {
        tracing::warn!(error = %e, "couldn't forget a cloud bot channel");
    }
    st.bot.forget_mem(sub, Some(platform));
    if st.db.bot_channels(sub).map(|c| c.is_empty()).unwrap_or(false) {
        let _ = st.db.bot_set_enabled(sub, false, now_ms());
    }
}

/// `DELETE /v1/pro/bot/{platform}` → 204. Open after Pro ends (R-04).
async fn disconnect(
    State(st): State<AppState>,
    Path(platform): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let sub =
        st.db.pro_sub_for_install(&install).map_err(ApiError::db)?.ok_or_else(crate::billing::gate::pro_required)?;
    let (platform, _) = platform_of(&platform)?;
    take_rate(&st, &sub.id)?;
    disconnect_platform(&st, &sub.id, platform).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── The lease from the link ──────────────────────────────────────────────────────────

/// The `bot_seen` lease version a PC that names YouTube sends (`"lease": 2`, M12).
pub const LEASE_VERSION: i64 = 2;

/// The platforms a `bot_seen` frame names: `{platforms: ["twitch", …], lease: 2}` → those
/// (the chat platforms that PC is connected to); without `platforms`, all of them (older
/// PCs); with `platforms` but no `lease` (a PC from before M12, which never names YouTube
/// although it may answer there), those plus YouTube.
pub(crate) fn seen_platforms(frame: &Value) -> Vec<Platform> {
    match frame.get("platforms").and_then(Value::as_array) {
        Some(list) => {
            let mut out: Vec<Platform> = list.iter().filter_map(Value::as_str).filter_map(platform_from_key).collect();
            if frame.get("lease").and_then(Value::as_i64).is_none_or(|v| v < LEASE_VERSION) {
                out.push(Platform::Youtube);
            }
            out.sort_by_key(|p| PLATFORMS.iter().position(|q| q == p));
            out.dedup();
            out
        }
        None => PLATFORMS.to_vec(),
    }
}

/// A `bot_seen` frame from `install`'s link: the lease for the platforms it names, and
/// whether its YouTube was live (`youtube_live`), which sets how soon the cloud looks.
pub(crate) fn seen_frame(st: &AppState, install: &str, frame: &Value) {
    desktop_seen(st, install, &seen_platforms(frame));
    if let Some(live) = frame.get("youtube_live").and_then(Value::as_bool)
        && let Ok(Some(s)) = st.db.pro_sub_for_install(install)
    {
        youtube::hint(st, &s.id, live);
    }
}

/// A PC sent `bot_seen` on its relay link for `platforms`: the cloud stands by there, and
/// takes over (after 90 s) on a platform that PC isn't connected to.
pub(crate) fn desktop_seen(st: &AppState, install: &str, platforms: &[Platform]) {
    match st.db.pro_sub_for_install(install) {
        Ok(Some(s)) => {
            let (now, ms) = (Instant::now(), now_ms());
            for p in platforms {
                st.bot.lease(&s.id, *p, |l| l.seen(now, ms));
            }
            st.bot.seen_by.lock().unwrap_or_else(|p| p.into_inner()).insert(s.id, (install.to_string(), ms));
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, "couldn't look up a PC's subscription for the bot lease"),
    }
    // A PC signed in to a cloud mode account with its bot on: that account's cloud bot stands
    // by too, so the two never answer twice (docs/CLOUD.md §6).
    if let Ok(Some(account)) = st.db.account_for_install(install, now_ms())
        && crate::cloud::is_account_key(&account)
        && matches!(st.db.bot_channels(&account), Ok(c) if !c.is_empty())
    {
        let (now, ms) = (Instant::now(), now_ms());
        for p in platforms {
            st.bot.lease(&account, *p, |l| l.seen(now, ms));
        }
    }
}

// ── Answering ────────────────────────────────────────────────────────────────────────

/// Live since `at` (the stream's start), or offline. Kept on the channel, so timers and
/// `{uptime}` carry on after a relay restart mid-stream.
pub(crate) fn set_live(st: &AppState, sub: &str, platform: Platform, at: Option<i64>) {
    if let Err(e) = st.db.bot_set_live(sub, key(platform), at) {
        tracing::warn!(error = %e, "couldn't record a channel going live or offline");
    }
    // Live on Twitch or Kick: likely live on YouTube too, so the cloud looks now (free).
    if at.is_some() && platform != Platform::Youtube {
        youtube::wake(st, sub);
    }
}

/// The last chat lines kept in memory for a channel (the offline dashboard, M10).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn recent_chat(st: &AppState, sub: &str, platform: Platform) -> Vec<ChatLine> {
    st.bot.mem(sub, |m| m.ring.get(&platform).map(|r| r.iter().cloned().collect()).unwrap_or_default())
}

/// Remembers a line (ring and id): the line as kept, `None` when this message id was
/// already seen. A line gets an id of its own here (`cl-<platform>-<message id>`, the PC
/// gives its own when it takes events in), so the phone's chat can tell lines apart.
fn remember(st: &AppState, sub: &str, platform: Platform, e: &Event, user: &str, text: &str) -> Option<Arc<Event>> {
    st.bot.mem(sub, |m| {
        if let Some(id) = e.source_id.as_deref().filter(|s| !s.is_empty()) {
            let (order, set) = m.seen.entry(platform).or_default();
            if !set.insert(id.to_string()) {
                return None;
            }
            order.push_back(id.to_string());
            while order.len() > DEDUPE_KEEP {
                if let Some(old) = order.pop_front() {
                    set.remove(&old);
                }
            }
        }
        let ring = m.ring.entry(platform).or_default();
        let mut kept = e.clone();
        kept.raw = None;
        if kept.id.is_empty() {
            kept.id = match e.source_id.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => format!("cl-{}-{}", key(platform), id.chars().take(128).collect::<String>()),
                None => format!("cl-{}-{}", key(platform), crate::random(12)),
            };
        }
        let kept = Arc::new(kept);
        ring.push_back(ChatLine {
            at: e.occurred_at.0,
            user: user.to_string(),
            text: text.chars().take(500).collect(),
            event: kept.clone(),
        });
        while ring.len() > RING {
            ring.pop_front();
        }
        Some(kept)
    })
}

/// The cloud's own message: one of its sent ids, or (an echo that came before the answer,
/// or no id) the bot account saying exactly what the cloud just sent. The streamer's own
/// lines from the same account are answered as usual.
fn own_message(
    st: &AppState,
    ch: &store::Channel,
    platform: Platform,
    e: &Event,
    actor_id: Option<&str>,
    text: &str,
) -> bool {
    st.bot.mem(&ch.sub_id, |m| {
        let Some(sent) = m.sent.get(&platform) else { return false };
        if e.source_id.as_deref().is_some_and(|id| sent.set.contains(id)) {
            return true;
        }
        actor_id.is_some() && actor_id == ch.bot_id.as_deref() && sent.echo(text, now_ms())
    })
}

/// Why the cloud didn't answer a chat line: logged at info the first time per reason,
/// channel and hour, at debug after, so a quiet bot is never a mystery and chat never
/// floods the log.
pub(crate) fn quiet(st: &AppState, sub: &str, platform: Platform, why: &'static str) {
    if st.bot.note(sub, platform, why, now_ms()) {
        tracing::info!(sub, platform = key(platform), why, "the cloud bot didn't answer a chat line (first this hour)");
    } else {
        tracing::debug!(sub, platform = key(platform), why, "the cloud bot didn't answer a chat line");
    }
}

/// A chat line for a channel the cloud bot is bound to (from EventSub or Kick's webhook).
pub(crate) async fn on_chat(st: &AppState, ch: &store::Channel, platform: Platform, e: Event) {
    let EventKind::ChatMessage { actor, content, .. } = &e.kind else { return };
    if e.origin != Origin::Live {
        return;
    }
    let sub = ch.sub_id.as_str();
    // Switched off or Pro ended: nothing is kept or answered.
    let s2 = sub.to_string();
    if !matches!(db(st, move |db| sub_is_on(db, &s2)).await, Ok(true)) {
        quiet(st, sub, platform, "switched_off_or_no_pro");
        return;
    }
    let Some(kept) = remember(st, sub, platform, &e, &actor.display_name, &content.text) else {
        return; // a retry of a message already handled
    };
    // Phones of a PC that's off see the line as the PC would have sent it (M10).
    crate::remote::cloud_chat(st, sub, &kept);
    // Shared Chat from a partner channel, the cloud's own lines: never answered (R-11).
    if e.channel.is_some() {
        quiet(st, sub, platform, "shared_chat");
        return;
    }
    if own_message(st, ch, platform, &e, actor.id.as_deref(), &content.text) {
        quiet(st, sub, platform, "own_message");
        return;
    }
    if !ch.ready() {
        quiet(st, sub, platform, "channel_not_ready");
        return;
    }
    let Some(since) = cloud_since(st, sub, platform) else {
        quiet(st, sub, platform, "desktop_on_duty");
        return;
    };
    if load_config(st, sub).is_none() {
        quiet(st, sub, platform, "no_config");
        return;
    }
    let now = now_ms();
    let mut why: &'static str = "no_config";
    let answer = st.bot.mem(sub, |m| {
        let mut extra = Vars::new();
        cloudbot::uptime_vars(&mut extra, ch.live_since.map(Millis), Millis(now));
        extra.insert("channel".into(), ch.channel_login.clone().unwrap_or_default());
        let SubMem { config, brains, .. } = m;
        let (_, cfg) = config.as_mut()?;
        let brain = brains.entry(platform).or_default();
        brain.takeover(cfg, Millis(since));
        let answer = brain.on_chat(
            cfg,
            Chat { platform, actor, text: &content.text, live: ch.live_since.is_some() },
            &extra,
            Millis(now),
        );
        if answer.is_none() {
            why = cloudbot::why_quiet(cfg, &content.text);
        }
        answer
    });
    let Some(answer) = answer else {
        quiet(st, sub, platform, why);
        return;
    };
    if let Some(change) = &answer.counter {
        counter_changed(st, sub, change, now);
    }
    say(st, ch, platform, &answer.text, e.source_id.as_deref()).await;
}

/// A counter the cloud changed: kept until a PC applies it, and sent to each PC as the op
/// (`add` with `delta`, or `set`), so a change the PC made meanwhile isn't overwritten.
fn counter_changed(st: &AppState, sub: &str, change: &CounterChange, at: i64) {
    let (op, delta) = match change.op {
        CounterOp::Add(d) => ("add", d),
        _ => ("set", 0),
    };
    let id = match st.db.bot_add_counter_change(sub, &change.name, op, delta, change.value, at) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't record a cloud counter change");
            return;
        }
    };
    let payload = json!({ "id": id, "name": change.name, "op": op, "delta": delta, "value": change.value, "at": at });
    for install in st.db.bot_installs(sub).unwrap_or_default() {
        if let Err(e) = st.enqueue(&install, "bot_counter", &payload) {
            tracing::warn!(error = e.message(), "couldn't queue a counter change for a PC");
        }
    }
}

/// Sends a line within the channel's 20 a minute; records what happened. Only signed out
/// blocks the channel; a refusal shows (and the next good message clears it), a dropped
/// message is noted.
async fn say(st: &AppState, ch: &store::Channel, platform: Platform, text: &str, reply_to: Option<&str>) -> bool {
    let p = key(platform);
    match st.db.try_take(&ch.sub_id, &format!("bot_messages_{p}"), Period::Minute, 1, MESSAGES_PER_MINUTE) {
        Ok(()) => {}
        Err(TakeErr::Over(_)) => {
            quiet(st, &ch.sub_id, platform, "messages_per_minute_used_up");
            return false;
        }
        Err(TakeErr::Db(e)) => {
            tracing::warn!(error = %e, "couldn't count a cloud bot message");
            return false;
        }
    }
    // Remembered before it goes: Twitch's echo can arrive before its answer.
    st.bot.mem(&ch.sub_id, |m| m.sent.entry(platform).or_default().text(text, now_ms()));
    if st.bot.note(&ch.sub_id, platform, "answering", now_ms()) {
        tracing::info!(sub = %ch.sub_id, platform = p, "the cloud bot is answering chat (first reply this hour)");
    }
    let sent = match platform {
        Platform::Twitch => twitch::send(st, ch, text, reply_to).await,
        Platform::Kick => kick::send(st, ch, text, reply_to).await,
        Platform::Youtube => youtube::send(st, ch, text).await,
        _ => Err(SendErr::NotReady),
    };
    let now = now_ms();
    match sent {
        Ok(id) => {
            if let Some(id) = id {
                st.bot.mem(&ch.sub_id, |m| m.sent.entry(platform).or_default().id(id));
            }
            let _ = st.db.bot_replied(&ch.sub_id, p, now);
            if ch.error.is_some() {
                let _ = st.db.bot_clear_transient_error(&ch.sub_id, p);
            }
            true
        }
        Err(SendErr::RateLimited(s)) => {
            quiet(st, &ch.sub_id, platform, "platform_rate_limited");
            st.bot.mem(&ch.sub_id, |m| m.brains.entry(platform).or_default().rate_limited(platform, s, Millis(now)));
            false
        }
        Err(SendErr::SignedOut) => {
            let err = error_json(
                &format!("{p}_signed_out"),
                "The platform signed the cloud bot out.",
                "Disconnect it here and connect it again.",
            );
            let _ = st.db.bot_set_error(&ch.sub_id, p, Some(&err));
            false
        }
        Err(SendErr::Refused(m)) => {
            tracing::info!(platform = p, why = %m, "a platform refused a cloud bot message");
            let err = error_json(
                &format!("{p}_refused"),
                "The platform didn't let the cloud bot speak in this chat.",
                "Check the bot isn't banned or timed out, and that it's allowed in the channel. The next message tries again.",
            );
            let _ = st.db.bot_set_error(&ch.sub_id, p, Some(&err));
            false
        }
        Err(SendErr::Dropped(m)) => {
            tracing::debug!(platform = p, why = %m, "a platform didn't show a cloud bot message");
            let reason: String = if m.trim().is_empty() { "dropped".into() } else { m.chars().take(200).collect() };
            let _ = st.db.bot_set_drop(&ch.sub_id, p, &json!({ "reason": reason, "at": now }));
            false
        }
        Err(SendErr::Quota(s)) => {
            // Shown once (the channel keeps it until the reset, or a reply goes out).
            if ch.error_code() != Some("youtube_quota") {
                let _ = st.db.bot_set_error(&ch.sub_id, p, Some(&youtube::spent_error(s)));
            }
            false
        }
        Err(SendErr::NotReady) => {
            quiet(st, &ch.sub_id, platform, "send_not_ready");
            false
        }
        Err(SendErr::Unavailable(m)) => {
            if st.bot.note(&ch.sub_id, platform, "send_failed", now) {
                tracing::warn!(platform = p, why = %m, "couldn't send a cloud bot message");
            } else {
                tracing::debug!(platform = p, why = %m, "couldn't send a cloud bot message");
            }
            false
        }
    }
}

/// One channel's due timers (the cloud has that platform).
async fn timers_for(st: &AppState, ch: &store::Channel, platform: Platform, since: i64) {
    if load_config(st, &ch.sub_id).is_none() {
        return;
    }
    let live = ch.live_since.is_some();
    let due = st.bot.mem(&ch.sub_id, |m| {
        let SubMem { config, brains, .. } = m;
        match config.as_ref() {
            Some((_, cfg)) => {
                let brain = brains.entry(platform).or_default();
                brain.takeover(cfg, Millis(since));
                brain.due_timers(cfg, platform, live, Millis(now_ms()))
            }
            None => Vec::new(),
        }
    });
    for text in due {
        // YouTube: at most one timer post per `timer_gap` (each is 50 units); the rest wait
        // for their next turn.
        if platform == Platform::Youtube && !youtube::timer_slot(st, &ch.sub_id) {
            break;
        }
        say(st, ch, platform, &text, None).await;
    }
}

/// One pass over the timers of every channel the cloud is answering on: a task per channel,
/// at most [`TIMER_CONCURRENCY`] at once.
pub(crate) async fn timer_tick(st: &AppState) {
    let work = db(st, |db| {
        let mut out = Vec::new();
        for sub in db.bot_enabled_subs()? {
            if !sub_is_pro(db, &sub) {
                continue;
            }
            out.extend(db.bot_channels(&sub)?.into_iter().filter(store::Channel::ready));
        }
        Ok(out)
    })
    .await;
    let channels = match work {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list cloud bot channels");
            return;
        }
    };
    let slots = Arc::new(Semaphore::new(TIMER_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for ch in channels {
        let Some(platform) = platform_from_key(&ch.platform) else { continue };
        let Some(since) = cloud_since(st, &ch.sub_id, platform) else { continue };
        let Ok(slot) = slots.clone().acquire_owned().await else { break };
        let st2 = st.clone();
        tasks.spawn(async move {
            timers_for(&st2, &ch, platform, since).await;
            drop(slot);
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// The timer loop (spawned in `main.rs`); YouTube's live checks run in a loop of their own
/// so a slow Google call never holds up a Twitch or Kick timer.
pub async fn run(st: AppState) {
    if st.bot.youtube.is_some() {
        tokio::spawn(youtube::run(st.clone()));
    }
    loop {
        tokio::time::sleep(TIMER_TICK).await;
        timer_tick(&st).await;
    }
}

/// Shard 0 of `conduit`, looked at [`SHARD_CHECK_AFTER`] after it was set up: enabled at this
/// relay's callback, or a warning and the set-up again (at most [`SHARD_CHECK_TRIES`] in a
/// row; the hourly sweep after that). Without this a failed callback check (the relay was
/// restarting, a proxy wasn't ready) would leave chat silently off for up to an hour.
pub(crate) async fn check_shard(st: AppState, conduit: String, attempt: u32) {
    tokio::time::sleep(SHARD_CHECK_AFTER).await;
    let Some(api) = st.bot.twitch.clone() else { return };
    let Ok(app) = st.bot.app_token().await else { return };
    let shards = match api.shards(&app, &conduit).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "couldn't look at Twitch's EventSub shard");
            return;
        }
    };
    let want = twitch::callback_url(&st);
    let shard = shards.iter().find(|s| s.id == "0");
    if shard.is_some_and(|s| s.status == "enabled" && s.callback.as_deref() == Some(want.as_str())) {
        tracing::info!("Twitch's EventSub shard is enabled at this relay");
        return;
    }
    let status = shard.map_or("missing", |s| s.status.as_str());
    let here = shard.is_some_and(|s| s.callback.as_deref() == Some(want.as_str()));
    tracing::warn!(
        status,
        callback_is_this_relay = here,
        attempt,
        "Twitch's EventSub shard isn't enabled at this relay: Twitch chat isn't reaching the cloud bot"
    );
    if attempt >= SHARD_CHECK_TRIES {
        return;
    }
    {
        let mut slot = st.bot.conduit.lock().await;
        if slot.as_deref() == Some(conduit.as_str()) {
            *slot = None;
        }
    }
    if let Err(e) = st.bot.conduit_try(&st, attempt + 1).await {
        tracing::warn!(error = %e, "couldn't point Twitch's EventSub conduit at the relay");
    }
}

/// Every bound, unblocked Twitch channel of a switched-on subscription, subscribed again
/// (the conduit changed).
async fn resubscribe_all(st: &AppState) {
    for ch in st.db.bot_all_channels().unwrap_or_default() {
        if ch.platform == "twitch" && ch.ready() && matches!(sub_is_on(&st.db, &ch.sub_id), Ok(true)) {
            let _ = twitch::subscribe_channel(st, &ch).await;
        }
    }
}

/// The hourly sweep (and once at start): the conduit pointed here, Twitch sign-ins checked,
/// channels Twitch couldn't be reached for subscribed again, live status read, Twitch
/// subscriptions of switched-off or lapsed subscriptions deleted, subscriptions 30 days past
/// Pro disconnected (sign-ins revoked, everything deleted), old counter changes dropped.
pub async fn sweep(st: &AppState) {
    let now = now_ms();
    let all = st.db.bot_all_channels().unwrap_or_default();
    // Twitch turns a webhook shard off after failed deliveries (the relay was down): point it
    // here again once an hour while any Twitch channel uses it.
    let twitch_in_use = all.iter().any(|c| c.platform == "twitch" && c.ready());
    if twitch_in_use && st.bot.twitch_cfg().is_some() {
        *st.bot.conduit.lock().await = None;
        if let Err(e) = st.bot.conduit(st).await {
            tracing::warn!(error = %e, "couldn't point Twitch's EventSub conduit at the relay");
        }
    }
    for ch in &all {
        if ch.platform == "twitch" && ch.bot_id.is_some() && !ch.blocked() {
            twitch::check_signins(st, ch).await;
        }
        if ch.platform == "youtube" && ch.bot_id.is_some() && !ch.blocked() {
            youtube::check_signin(st, ch).await;
        }
    }
    for ch in st.db.bot_all_channels().unwrap_or_default() {
        if ch.platform != "twitch" || !ch.ready() {
            continue;
        }
        let on = matches!(sub_is_on(&st.db, &ch.sub_id), Ok(true));
        if !on {
            if !ch.subscriptions.is_empty() {
                twitch::unsubscribe_all(st, &ch).await;
            }
            continue;
        }
        if ch.error_code() == Some("twitch_unavailable") || ch.subscriptions.is_empty() {
            let _ = twitch::subscribe_channel(st, &ch).await;
        }
        twitch::reconcile_live(st, &ch).await;
    }
    for sub in st.db.bot_subs().unwrap_or_default() {
        // Cloud mode accounts aren't Pro subscriptions: cloud mode's own sweep looks after them.
        if crate::cloud::is_account_key(&sub) {
            continue;
        }
        let kept = match st.db.pro_sub(&sub) {
            Ok(None) => false,
            Ok(Some(s)) => {
                let access = s.access();
                crate::billing::access::is_pro(&access, now) || {
                    let stopped = s.stopped_at();
                    now < stopped.saturating_add(KEEP_AFTER_LAPSE_MS)
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't check a cloud bot subscription");
                continue;
            }
        };
        if !kept {
            for p in PLATFORMS {
                disconnect_platform(st, &sub, p).await;
            }
            match st.db.bot_forget(&sub) {
                Ok(()) => tracing::info!("a cloud bot was removed 30 days after Pro ended"),
                Err(e) => tracing::warn!(error = %e, "couldn't forget a lapsed cloud bot"),
            }
            st.bot.forget_mem(&sub, None);
        }
    }
    if let Err(e) = st.db.bot_prune(now - COUNTER_CHANGES_KEEP_MS) {
        tracing::warn!(error = %e, "couldn't prune cloud counter changes");
    }
}

// ── Cloud mode accounts (docs/CLOUD.md §6) ───────────────────────────────────────────

/// The cloud bot's view for a cloud mode account (keyed by the account id). There's no PC,
/// so no PC code.
pub(crate) fn account_view(st: &AppState, account: &str) -> Result<Value, ApiError> {
    let mut v = view(st, account, "")?;
    // YouTube (Pro) is bound to the account's subscription: whether its bot follows this page.
    let yt = youtube::web::view_json(st, account)?;
    let follows = youtube_follows_account(st, account).is_some();
    let own = st.db.bot_channels(account).map_err(ApiError::db)?.iter().any(store::Channel::ready);
    if let Some(o) = v.as_object_mut() {
        o.remove("pc_code");
        if follows && !own {
            // Only YouTube: the one switch is YouTube's bot.
            o.insert("enabled".into(), yt["bot_on"].clone());
        }
        o.insert(
            "youtube".into(),
            json!({ "channel": yt["channel"], "bot_on": yt["bot_on"], "pro": yt["pro"], "follows_this_page": follows }),
        );
    }
    Ok(v)
}

/// Switches a cloud mode account's bot on or off (the web dashboard's Bot page). For an
/// account whose YouTube bot follows the Bot page ([`youtube_follows_account`]), its YouTube
/// bot switches with it.
pub(crate) async fn account_set_enabled(st: &AppState, account: &str, enabled: bool) -> Result<Value, ApiError> {
    let ctx = ProCtx { install: String::new(), sub_id: account.to_string() };
    let yt = youtube_follows_account(st, account);
    // With only YouTube connected, the account key has no channel: the switch is YouTube's.
    let own = st.db.bot_channels(account).map_err(ApiError::db)?.iter().any(store::Channel::ready);
    if own || yt.is_none() || !enabled {
        set_enabled(st, &ctx, enabled).await?;
    }
    if let Some(sub) = yt {
        let ctx = ProCtx { install: String::new(), sub_id: sub };
        match set_enabled(st, &ctx, enabled).await {
            Ok(_) => {}
            // Twitch or Kick switched already: YouTube's own problem (a sign-in it needs)
            // shows on its channel, the switch still answers.
            Err(err) if own => {
                tracing::info!(error = err.message(), "the YouTube bot didn't switch with the Bot page")
            }
            Err(err) => return Err(err),
        }
    }
    account_view(st, account)
}

/// The Pro subscription whose YouTube bot follows a cloud mode account's Bot page: the
/// account's Pro subscription with a YouTube channel and **no PC** linked to it (docs/
/// CLOUD.md §11). With a PC, its pushed config stays the YouTube bot's (the PC is where the
/// streamer edits it), so the Bot page never overwrites it.
pub(crate) fn youtube_follows_account(st: &AppState, account: &str) -> Option<String> {
    let sub = youtube::web::account_pro(st, account).ok().flatten()?;
    st.db.bot_channel(&sub, "youtube").ok().flatten()?;
    let no_pc = st.db.pro_installs(&sub).ok()? == 0;
    // A PC is linked but never pushed a config: until it does, this page's is YouTube's.
    let never_pushed = st.db.bot_config(&sub).ok()?.is_none_or(|r| r.version == 0);
    (no_pc || never_pushed).then_some(sub)
}

/// The cloud mode account's Bot page config for a subscription attached to it (cloud mode
/// on), as the bot runs it (its own switch is the one switch, so `bot_on`).
fn account_config_for_sub(st: &AppState, sub: &str) -> Option<BotConfig> {
    let account = st.db.cloud_account_for_sub(sub).ok().flatten()?;
    st.db.cloud_account(&account).ok().flatten()?;
    let row = st.db.bot_config(&account).ok().flatten()?;
    let mut cfg: BotConfig = serde_json::from_str(&row.json).ok()?;
    cfg.bot_on = true;
    Some(cfg)
}

/// The YouTube bot's config follows the account's (no PC): the same commands, timers and
/// replies under the subscription's key. A failure is logged; the account's save stands.
fn mirror_to_youtube(st: &AppState, account: &str, cfg: &BotConfig) {
    let Some(sub) = youtube_follows_account(st, account) else { return };
    if st.db.pro_installs(&sub).ok() != Some(0) {
        // A PC could still push its own; until it does, the YouTube bot reads this page's
        // config (`load_config`), so nothing is written under the subscription.
        st.bot.mem(&sub, |m| m.config = Some((0, cfg.clone())));
        return;
    }
    let Ok(json) = serde_json::to_string(cfg) else { return };
    for _ in 0..2 {
        let base = st.db.bot_config(&sub).ok().flatten().map_or(0, |r| r.version);
        match st.db.bot_push_config(&sub, base, &json, 0, now_ms()) {
            Ok(version) => {
                let mut c = cfg.clone();
                if let Ok(changes) = st.db.bot_counter_changes(&sub) {
                    overlay_changes(&mut c, &changes);
                }
                st.bot.mem(&sub, |m| m.config = Some((version, c)));
                return;
            }
            Err(store::PushErr::Stale(_)) => continue,
            Err(store::PushErr::Db(e)) => {
                tracing::warn!(error = %e, "couldn't copy the Bot page's settings to the YouTube bot");
                return;
            }
        }
    }
    tracing::warn!("the YouTube bot's settings kept changing; the Bot page's save wasn't copied to it");
}

/// A cloud mode account's bot settings and their version (none saved yet: version 0).
pub(crate) fn account_config(st: &AppState, account: &str) -> Result<(i64, BotConfig), ApiError> {
    let row = st.db.bot_config(account).map_err(ApiError::db)?;
    Ok(match row {
        Some(r) => (r.version, serde_json::from_str(&r.json).unwrap_or_default()),
        None => (0, BotConfig::default()),
    })
}

/// Saves a cloud mode account's bot settings from the web dashboard, checked like a PC's
/// push; the new version. 409 `stale_version` when another tab saved meanwhile.
pub(crate) fn account_save_config(
    st: &AppState,
    account: &str,
    base: i64,
    mut cfg: BotConfig,
) -> Result<i64, ApiError> {
    // There's no desktop switch for a cloud account: the bot's own on/off is the one switch.
    cfg.bot_on = true;
    cfg.validate().map_err(|e| match e {
        ConfigError::TooManyCommands | ConfigError::TooManyTimers => {
            ApiError::new(StatusCode::CONFLICT, "cloud_limit", e.to_string()).hint("Delete one you don't use.")
        }
        ConfigError::Invalid(m) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m),
    })?;
    let json = serde_json::to_string(&cfg).map_err(|_| invalid("The bot settings couldn't be read."))?;
    let version = match st.db.bot_push_config(account, base, &json, 0, now_ms()) {
        Ok(v) => v,
        Err(store::PushErr::Stale(current)) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "stale_version",
                format!("The bot settings changed in another tab (version {current})."),
            )
            .hint("Reload the Bot page and make the change again."));
        }
        Err(store::PushErr::Db(e)) => return Err(ApiError::db(e)),
    };
    mirror_to_youtube(st, account, &cfg);
    overlay_changes(&mut cfg, &st.db.bot_counter_changes(account).map_err(ApiError::db)?);
    st.bot.mem(account, |m| m.config = Some((version, cfg)));
    Ok(version)
}

/// Most characters one chat message may carry per platform (YouTube's is the shortest).
fn chat_limit(p: Platform) -> usize {
    if p == Platform::Youtube { 200 } else { 500 }
}

/// `text` cut into chat messages of at most `max` characters, at spaces where it can.
pub(crate) fn split_chat(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        while word.chars().count() > max {
            let head: String = word.chars().take(max).collect();
            word = word.chars().skip(max).collect();
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(head);
        }
        let need = cur.chars().count() + usize::from(!cur.is_empty()) + word.chars().count();
        if need > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Posts a message as the streamer in a cloud mode account's chats (a poll, a giveaway, its
/// winner; docs/CLOUD.md §7.2), through the cloud bot: only where the bot is on and the cloud
/// has the chat (no PC answering), counted against its 20 messages a minute. `true` when it
/// reached at least one chat. YouTube is the account's subscription's bot when it follows the
/// Bot page.
pub(crate) async fn announce(st: &AppState, account: &str, text: &str) -> bool {
    let text = ferrocast_core::command::safe_reply(text);
    if text.is_empty() {
        return false;
    }
    let mut keys = vec![account.to_string()];
    if let Some(sub) = youtube_follows_account(st, account) {
        keys.push(sub);
    }
    let mut posted = false;
    for key in keys {
        let k2 = key.clone();
        if !matches!(db(st, move |db| sub_is_on(db, &k2)).await, Ok(true)) {
            continue;
        }
        for ch in st.db.bot_channels(&key).unwrap_or_default() {
            let Some(p) = platform_from_key(&ch.platform) else { continue };
            // The subscription only speaks for YouTube here; Twitch and Kick are the account's.
            if !ch.ready() || (key != account && p != Platform::Youtube) || cloud_since(st, &key, p).is_none() {
                continue;
            }
            for chunk in split_chat(&text, chat_limit(p)) {
                if !say(st, &ch, p, &chunk, None).await {
                    break;
                }
                posted = true;
            }
        }
    }
    posted
}
