//! The 24/7 cloud bot on the PC's side (Pro F1, API.md §9.7).
//!
//! While Pro (the remembered status, never a network check) has `features.cloud_bot` and
//! the streamer switched it on, this keeps the relay's copy of the bot config current, keeps
//! the relay link open, and says "this PC is answering" there every 30 s (`bot_seen`, sent by
//! the link itself in `relay.rs`). The desktop bot never waits for any of it (R-01): it
//! answers as always, and the relay only steps in after 90 s without a `bot_seen`.
//!
//! - **Pushes**: every second the config's hash is compared (commands, timers, auto-replies,
//!   counter definitions, the bot switch). A change that stands still for 2 s is pushed with
//!   the version last seen; counter *values* ride along, at most once a minute on their own.
//!   Failures back off (2 s doubling to 5 min, `Retry-After` honoured); a 409 `stale_version`
//!   reads the relay's version and pushes again once (this PC's change is newer); 400/413/
//!   415/422 stop pushing that config until it changes.
//! - **Counters**: a `bot_counter` item from the link applies the cloud's change as it was
//!   made (`add` a delta, or `set`), so a change made here meanwhile isn't overwritten; the
//!   next push reports `counters_seen`, so the relay drops what this PC already has.
//! - **Who answers**: `bot_seen` names the chat platforms this PC is connected to, so the
//!   cloud covers the others.
//! - **Connecting Twitch**: both steps send this PC's own Twitch token (`owner_token`), so the
//!   relay can check the channel is this PC's.
//! - Settings `cloud_bot`: `{ enabled, version, pushed_hash, synced_at, counters_seen, error }`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrocast_core::chatbot::CounterOp;
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Engine, EngineError, now};

pub(crate) const SAVE_KEY: &str = "cloud_bot";
const TICK: Duration = Duration::from_secs(1);
/// A change must stand still this long before it's pushed.
pub(crate) const DEBOUNCE_MS: i64 = 2_000;
/// Counter values alone are pushed at most this often.
pub(crate) const VALUES_EVERY_MS: i64 = 60_000;
pub(crate) const BACKOFF_MIN_MS: i64 = 2_000;
pub(crate) const BACKOFF_MAX_MS: i64 = 5 * 60_000;
/// `bot_seen` on the link this often.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(30);
/// Every PC of the subscription reads the relay's switch this often, so one that never opened
/// the panel still stands the cloud down while it's answering (no double replies).
pub(crate) const FOLLOW_EVERY_MS: i64 = 5 * 60_000;

/// What's kept in the local store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Saved {
    pub enabled: bool,
    /// The relay's version this PC last saw.
    pub version: i64,
    /// The definitions' hash last pushed.
    pub pushed_hash: Option<String>,
    pub synced_at: Option<i64>,
    /// The highest cloud counter change applied here.
    pub counters_seen: i64,
    /// The last push's problem (`{code, message, hint}`).
    pub error: Option<Value>,
}

/// When to push (pure, so the timing is tested without a clock).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pusher {
    /// The definitions' hash seen last and since when it has stood still.
    seen: Option<(String, i64)>,
    pushed_defs: Option<String>,
    pushed_values: Option<String>,
    pushed_at: i64,
    failures: u32,
    next_try: i64,
    /// A config the relay refused (definitions + values): not sent again until it changes.
    blocked: Option<(String, String)>,
}

impl Pusher {
    pub fn with_pushed(defs: Option<String>) -> Pusher {
        Pusher { pushed_defs: defs, ..Default::default() }
    }

    /// Whether to push now, given this tick's hashes.
    pub fn due(&mut self, defs: &str, values: &str, now: i64) -> bool {
        if self.seen.as_ref().is_none_or(|(h, _)| h != defs) {
            self.seen = Some((defs.to_string(), now));
        }
        if now < self.next_try {
            return false;
        }
        if self.blocked.as_ref().is_some_and(|(d, v)| d == defs && v == values) {
            return false;
        }
        if self.pushed_defs.as_deref() != Some(defs) {
            let since = self.seen.as_ref().map_or(now, |(_, at)| *at);
            return now - since >= DEBOUNCE_MS;
        }
        self.pushed_values.as_deref() != Some(values) && now - self.pushed_at >= VALUES_EVERY_MS
    }

    pub fn pushed(&mut self, defs: &str, values: &str, now: i64) {
        self.pushed_defs = Some(defs.to_string());
        self.pushed_values = Some(values.to_string());
        self.pushed_at = now;
        self.failures = 0;
        self.next_try = 0;
        self.blocked = None;
    }

    /// The relay or the network failed: wait 2 s doubling to 5 min (or what it asked).
    pub fn failed(&mut self, now: i64, retry_after_s: Option<u64>) -> i64 {
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(20);
        let wait = BACKOFF_MIN_MS.saturating_mul(1 << shift).min(BACKOFF_MAX_MS);
        let asked = retry_after_s.map_or(0, |s| i64::try_from(s.saturating_mul(1000)).unwrap_or(BACKOFF_MAX_MS));
        let wait = wait.max(asked.min(BACKOFF_MAX_MS));
        self.next_try = now + wait;
        wait
    }

    /// The relay refused this config itself (it's too big or wrong): wait for a change.
    pub fn refused(&mut self, defs: &str, values: &str) {
        self.blocked = Some((defs.to_string(), values.to_string()));
        self.failures = 0;
        self.next_try = 0;
    }
}

/// The engine's cloud bot memory.
#[derive(Debug, Default)]
pub(crate) struct CloudBot {
    pub saved: Mutex<Saved>,
    pub pusher: Mutex<Pusher>,
    /// `pending` while a change waits, `synced`, `error`, `off`.
    pub sync: Mutex<&'static str>,
    /// One push (or switch) at a time.
    pub io: tokio::sync::Mutex<()>,
    /// When the relay's switch was last read (ms).
    pub followed_at: Mutex<i64>,
    /// The connected Twitch account's sign-in (set by `accounts.rs`): its token proves the
    /// channel when connecting the cloud bot.
    pub desktop_twitch: Mutex<Option<Arc<ferrocast_twitch::Auth>>>,
    /// The connected YouTube channel's sign-in and its live chat (set by `accounts.rs`): its
    /// token proves the channel when connecting YouTube's cloud bot (M12), and `bot_seen`
    /// says whether this PC sees the stream live.
    pub desktop_youtube: Mutex<Option<DesktopYoutube>>,
}

/// This PC's YouTube: the API (its sign-in) and the live chat its reader follows.
#[derive(Debug, Clone)]
pub(crate) struct DesktopYoutube {
    pub api: Arc<ferrocast_youtube::Api>,
    pub live: ferrocast_youtube::stream::LiveChat,
}

/// The config as the relay takes it, and its two hashes (definitions; values).
pub(crate) fn snapshot(engine: &Engine) -> (Value, String, String) {
    let (commands, timers, bot_on) = {
        let st = engine.state();
        (st.commands.clone(), st.timers.clone(), st.settings.bot_enabled)
    };
    let counters = engine.counters();
    let defs = json!({
        "bot_on": bot_on, "commands": commands, "timers": timers, "auto_replies": engine.autoreplies(),
        "counters": counters.iter().map(|c| json!({ "name": c.name, "command": c.command, "reply": c.reply,
                                                     "enabled": c.enabled, "value": 0 })).collect::<Vec<_>>(),
    });
    let values: Vec<(String, i64)> = counters.iter().map(|c| (c.name.clone(), c.value)).collect();
    let mut full = defs.clone();
    if let Some(list) = full["counters"].as_array_mut() {
        for (c, v) in list.iter_mut().zip(&counters) {
            c["value"] = json!(v.value);
        }
    }
    (full, hash(&defs.to_string()), hash(&json!(values).to_string()))
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn error_of(e: &EngineError) -> Value {
    match e {
        EngineError::Refused { code, message, hint, .. } => json!({ "code": code, "message": message, "hint": hint }),
        other => json!({ "code": "relay_unreachable", "message": other.to_string(),
                         "hint": "Ferrocast tries again by itself. The bot on this PC keeps answering." }),
    }
}

pub(crate) async fn run(engine: Engine) {
    loop {
        tokio::time::sleep(TICK).await;
        engine.cloud_bot_round().await;
    }
}

impl Engine {
    fn cbot(&self) -> &CloudBot {
        &self.inner.cloud_bot
    }

    pub(crate) async fn load_cloud_bot(&self) {
        let saved = self.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.ok().flatten().unwrap_or_default();
        *self.cbot().pusher.lock().unwrap_or_else(|p| p.into_inner()) = Pusher::with_pushed(saved.pushed_hash.clone());
        *self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()) = saved;
    }

    async fn save_cloud_bot(&self) {
        let saved = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Err(e) = self.db(move |s| repo::settings::set(s, SAVE_KEY, &saved)).await {
            tracing::warn!(error = %e, "couldn't save the cloud bot's state");
        }
    }

    /// The cloud bot is switched on here and Pro has it (remembered status).
    pub(crate) async fn cloud_bot_on(&self) -> bool {
        self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).enabled && self.pro_has(|f| f.cloud_bot).await
    }

    /// One round: follow the relay's switch now and then, push the config when it changed
    /// (debounced, backed off).
    pub(crate) async fn cloud_bot_round(&self) {
        self.cloud_bot_follow().await;
        if !self.cloud_bot_on().await {
            *self.cbot().sync.lock().unwrap_or_else(|p| p.into_inner()) = "off";
            return;
        }
        // The link carries `bot_seen` and the counter changes.
        self.ensure_relay_link();
        let (config, defs, values) = snapshot(self);
        let t = now().0;
        let due = self.cbot().pusher.lock().unwrap_or_else(|p| p.into_inner()).due(&defs, &values, t);
        {
            let synced = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).pushed_hash.as_deref()
                == Some(defs.as_str());
            let mut s = self.cbot().sync.lock().unwrap_or_else(|p| p.into_inner());
            if *s != "error" || due {
                *s = if synced { "synced" } else { "pending" };
            }
        }
        if due {
            let _one = self.cbot().io.lock().await;
            self.cloud_bot_push(config, &defs, &values).await;
        }
    }

    /// Reads the relay's switch every 5 minutes while Pro has the feature (switched on from
    /// another PC or a phone: this PC says `bot_seen` too). Unreachable: the last one holds.
    async fn cloud_bot_follow(&self) {
        let t = now().0;
        let at = *self.cbot().followed_at.lock().unwrap_or_else(|p| p.into_inner());
        if at != 0 && t - at < FOLLOW_EVERY_MS {
            return;
        }
        if !self.pro_has(|f| f.cloud_bot).await {
            return;
        }
        *self.cbot().followed_at.lock().unwrap_or_else(|p| p.into_inner()) = t;
        if let Err(e) = self.cloud_bot_view().await {
            tracing::debug!(error = %e, "couldn't read the cloud bot's switch");
        }
    }

    async fn cloud_bot_send(&self, config: &Value) -> Result<i64, EngineError> {
        let (version, seen) = {
            let s = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner());
            (s.version, s.counters_seen)
        };
        let body = json!({ "version": version, "config": config, "counters_seen": seen });
        let v = self.pro_call(reqwest::Method::PUT, "/v1/pro/bot/config", Some(body)).await?;
        Ok(v["version"].as_i64().unwrap_or(version + 1))
    }

    /// Pushes now; a stale version is read and pushed again once.
    pub(crate) async fn cloud_bot_push(&self, config: Value, defs: &str, values: &str) {
        let mut result = self.cloud_bot_send(&config).await;
        if let Err(EngineError::Refused { status: 409, code: "stale_version", .. }) = &result {
            match self.pro_call(reqwest::Method::GET, "/v1/pro/bot", None).await {
                Ok(v) => {
                    let current = v["version"].as_i64().unwrap_or(0);
                    self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).version = current;
                    result = self.cloud_bot_send(&config).await;
                }
                Err(e) => result = Err(e),
            }
        }
        let t = now().0;
        match result {
            Ok(version) => {
                {
                    let mut s = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner());
                    s.version = version;
                    s.pushed_hash = Some(defs.to_string());
                    s.synced_at = Some(t);
                    s.error = None;
                }
                self.cbot().pusher.lock().unwrap_or_else(|p| p.into_inner()).pushed(defs, values, t);
                *self.cbot().sync.lock().unwrap_or_else(|p| p.into_inner()) = "synced";
            }
            Err(e) => {
                let refused = matches!(&e, EngineError::Refused { status: 400 | 413 | 415 | 422, .. });
                let retry = if let EngineError::Refused { retry_after, .. } = &e { *retry_after } else { None };
                {
                    let mut p = self.cbot().pusher.lock().unwrap_or_else(|p| p.into_inner());
                    if refused {
                        p.refused(defs, values);
                    } else {
                        p.failed(t, retry);
                    }
                }
                tracing::debug!(error = %e, "the cloud bot config didn't go up");
                self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).error = Some(error_of(&e));
                *self.cbot().sync.lock().unwrap_or_else(|p| p.into_inner()) = "error";
            }
        }
        self.save_cloud_bot().await;
    }

    /// The Twitch sign-in of the account this PC is connected to (or none after Disconnect).
    pub(crate) fn cloud_bot_set_twitch(&self, auth: Option<Arc<ferrocast_twitch::Auth>>) {
        *self.cbot().desktop_twitch.lock().unwrap_or_else(|p| p.into_inner()) = auth;
    }

    /// The YouTube sign-in of the channel this PC is connected to (or none after Disconnect).
    pub(crate) fn cloud_bot_set_youtube(&self, yt: Option<DesktopYoutube>) {
        *self.cbot().desktop_youtube.lock().unwrap_or_else(|p| p.into_inner()) = yt;
    }

    /// Whether this PC's YouTube reader is in a live chat now (`None` without YouTube): the
    /// relay looks for the stream at once, or waits, when it takes over (`youtube_live`).
    pub(crate) fn cloud_bot_youtube_live(&self) -> Option<bool> {
        let yt = self.cbot().desktop_youtube.lock().unwrap_or_else(|p| p.into_inner()).clone()?;
        Some(yt.live.read().unwrap_or_else(|p| p.into_inner()).is_some())
    }

    /// The `bot_seen` frame: the platforms this PC answers, and its YouTube live state.
    pub(crate) fn cloud_bot_seen_frame(&self) -> Value {
        // `lease: 2`: `platforms` names YouTube too (a relay treats a frame without it as an
        // older PC's, which may answer YouTube without naming it).
        let mut f = json!({ "type": "bot_seen", "platforms": self.cloud_bot_platforms(), "lease": 2 });
        if let Some(live) = self.cloud_bot_youtube_live() {
            f["youtube_live"] = json!(live);
        }
        f
    }

    /// The chat platforms this PC answers right now (said with `bot_seen`): connected, or
    /// reconnecting for a moment. The cloud covers the rest.
    pub(crate) fn cloud_bot_platforms(&self) -> Vec<&'static str> {
        let st = self.state();
        ["twitch", "kick", "youtube"]
            .into_iter()
            .filter(|p| st.connectors.get(*p).is_some_and(|c| matches!(c.state.as_str(), "connected" | "reconnecting")))
            .collect()
    }

    /// A `bot_counter` item from the link: the cloud changed a counter while this PC was off.
    /// `add` adds its delta to this PC's value (a change made here meanwhile stays); `set`
    /// (or an item from an older relay, without `op`) sets the value.
    pub(crate) async fn apply_cloud_counter(&self, payload: &Value) {
        let (Some(id), Some(name), Some(value)) =
            (payload["id"].as_i64(), payload["name"].as_str(), payload["value"].as_i64())
        else {
            return;
        };
        if id <= self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).counters_seen {
            return; // already applied (the link resends until acked)
        }
        let op = match (payload["op"].as_str(), payload["delta"].as_i64()) {
            (Some("add"), Some(d)) => CounterOp::Add(d),
            _ => CounterOp::Set(value),
        };
        match self.change_counter(name, op).await {
            Ok(_) => {}
            // Deleted here meanwhile: nothing to set, but it's still seen.
            Err(e) => tracing::debug!(error = %e, "a cloud counter change didn't apply"),
        }
        self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).counters_seen = id;
        self.save_cloud_bot().await;
    }

    /// `GET /api/v1/pro/bot`: the relay's view, plus this PC's side.
    pub async fn cloud_bot_view(&self) -> Result<Value, EngineError> {
        let mut v = self.pro_call(reqwest::Method::GET, "/v1/pro/bot", None).await?;
        // The relay's switch is the truth; remember it so pushes and `bot_seen` follow it.
        let enabled = v["enabled"].as_bool().unwrap_or(false);
        let changed = {
            let mut s = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner());
            let changed = s.enabled != enabled;
            s.enabled = enabled;
            changed
        };
        if changed {
            self.save_cloud_bot().await;
        }
        self.add_local(&mut v).await;
        Ok(v)
    }

    async fn add_local(&self, v: &mut Value) {
        let saved = self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut state = *self.cbot().sync.lock().unwrap_or_else(|p| p.into_inner());
        if !saved.enabled {
            state = "off";
        }
        v["pro_available"] = json!(self.pro_has(|f| f.cloud_bot).await);
        v["sync"] = json!({ "state": state, "version": saved.version, "synced_at": saved.synced_at,
                            "error": saved.error });
    }

    /// `PUT /api/v1/pro/bot/enabled {enabled}`: switching on pushes the config first.
    pub async fn cloud_bot_enable(&self, enabled: bool) -> Result<Value, EngineError> {
        let _one = self.cbot().io.lock().await;
        if enabled {
            let (config, defs, values) = snapshot(self);
            self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).enabled = true;
            self.cloud_bot_push(config, &defs, &values).await;
            self.ensure_relay_link();
        }
        let res = self.pro_call(reqwest::Method::PUT, "/v1/pro/bot/enabled", Some(json!({ "enabled": enabled }))).await;
        let mut v = match res {
            Ok(v) => v,
            Err(e) => {
                if enabled {
                    // Not switched on there (no channel yet, no Pro): not here either.
                    self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).enabled = false;
                    self.save_cloud_bot().await;
                }
                return Err(e);
            }
        };
        self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).enabled = v["enabled"].as_bool().unwrap_or(enabled);
        self.save_cloud_bot().await;
        self.add_local(&mut v).await;
        Ok(v)
    }

    /// `POST /api/v1/pro/bot/{platform}/connect` → `{auth_url, expires_at}`. Twitch and YouTube
    /// get this PC's own token added (`owner_token`): the relay checks with the platform that
    /// the channel is this PC's. Without one the relay answers `twitch_not_linked` /
    /// `youtube_not_linked`.
    pub async fn cloud_bot_connect(&self, platform: &str, body: Value) -> Result<Value, EngineError> {
        let p = platform_path(platform)?;
        let mut body = if body.is_object() { body } else { json!({}) };
        if let Some(o) = body.as_object_mut() {
            // Only this PC's own token ever goes, never one from the request.
            o.remove("owner_token");
            o.remove("retry");
        }
        if p == "twitch" {
            let auth = self.cbot().desktop_twitch.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(auth) = auth {
                match auth.access().await {
                    Ok(token) => body["owner_token"] = json!(token),
                    Err(e) => tracing::debug!(error = %e, "no Twitch token for the cloud bot's check"),
                }
            }
        }
        if p == "youtube" {
            let yt = self.cbot().desktop_youtube.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(yt) = yt {
                match yt.api.access().await {
                    Ok(token) => body["owner_token"] = json!(token),
                    Err(e) => tracing::debug!(error = %e, "no YouTube token for the cloud bot's check"),
                }
            }
        }
        let v = self.pro_call(reqwest::Method::POST, &format!("/v1/pro/bot/{p}/connect"), Some(body)).await?;
        let url = v["auth_url"].as_str().unwrap_or("");
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(crate::pro::unreachable("it sent no sign-in link"));
        }
        Ok(json!({ "auth_url": url, "expires_at": v["expires_at"] }))
    }

    /// `POST /api/v1/pro/bot/{platform}/connect {retry: true}`: a connected channel's
    /// retryable error tried again on the relay → the view.
    pub async fn cloud_bot_retry(&self, platform: &str) -> Result<Value, EngineError> {
        let p = platform_path(platform)?;
        let mut v = self
            .pro_call(reqwest::Method::POST, &format!("/v1/pro/bot/{p}/connect"), Some(json!({ "retry": true })))
            .await?;
        self.add_local(&mut v).await;
        Ok(v)
    }

    /// `DELETE /api/v1/pro/bot/{platform}`.
    pub async fn cloud_bot_disconnect(&self, platform: &str) -> Result<(), EngineError> {
        let p = platform_path(platform)?;
        self.pro_raw(reqwest::Method::DELETE, &format!("/v1/pro/bot/{p}"), None).await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn cloud_bot_saved(&self) -> Saved {
        self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    #[cfg(test)]
    pub(crate) fn cloud_bot_set_enabled_for_tests(&self, on: bool) {
        self.cbot().saved.lock().unwrap_or_else(|p| p.into_inner()).enabled = on;
    }
}

/// Only the platforms the relay knows ever reach it (no path of the caller's choosing).
fn platform_path(p: &str) -> Result<&'static str, EngineError> {
    match p {
        "twitch" => Ok("twitch"),
        "kick" => Ok("kick"),
        "youtube" => Ok("youtube"),
        _ => Err(EngineError::Refused {
            status: 404,
            code: "platform_not_found",
            message: "The cloud bot runs on Twitch, Kick and YouTube.".into(),
            hint: "Use twitch, kick or youtube.".into(),
            retry_after: None,
        }),
    }
}

#[cfg(test)]
#[path = "cloud_bot_tests.rs"]
mod tests;
