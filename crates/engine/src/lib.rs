//! Wires adapters, the bot, moderation and alerts together (ARCHITECTURE.md §1).
//!
//! One task reads every event from a single channel, in order, and runs the pipeline in
//! [`pipeline`]. A second task drives the alert queue. Everything else (the HTTP server,
//! adapters) talks to the engine through [`Engine`]'s methods and channels.
//!
//! Startup is staged (R-30): the store loads, state is built, and only then does the
//! pipeline start reading. Adapters may connect earlier; their events wait in the channel.

mod accounts;
mod actions;
mod dispatcher;
mod error;
mod giveaways;
mod loyalty;
mod obs;
mod patch;
mod pipeline;
mod polls;
mod relay;
mod snapshot;
pub mod speech;
mod timers;

pub mod frames;
pub mod state;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ferrocast_connector::{Connector, EventSink, SINK_CAPACITY};
use ferrocast_core::command::Cooldowns;
use ferrocast_core::dedupe::Deduper;
use ferrocast_core::event::{Event, Millis, Platform};
use ferrocast_core::queue::AlertQueue;
use ferrocast_store::{Store, StoreError, repo};
use tokio::sync::{Notify, broadcast, mpsc};

pub use accounts::{ConnectFlow, PlatformApps};
pub use error::EngineError;
use frames::{LiveFrame, OverlayFrame};
pub use obs::{ObsView, PasswordChange};
use state::{ConnectorView, EngineSettings, State};

/// Current wall-clock time. Only the engine reads the clock; `core` gets time passed in.
pub fn now() -> Millis {
    Millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0),
    )
}

/// Message ids we hid (filtered, deleted, banned): kept out of overlay snapshots.
const HIDDEN_KEEP: usize = 2_000;

/// Hidden message keys as a set for lookups plus their order for trimming.
type Hidden = (HashSet<(Platform, String)>, VecDeque<(Platform, String)>);

/// The engine. Cheap to clone; all clones share one instance.
#[derive(Debug, Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Arc<Store>,
    state: RwLock<State>,
    connectors: RwLock<HashMap<Platform, Arc<dyn Connector>>>,
    queue: tokio::sync::Mutex<AlertQueue>,
    queue_changed: Notify,
    /// One settings save at a time, so two quick saves can't land in the wrong order.
    config: tokio::sync::Mutex<()>,
    dedupe: Mutex<Deduper>,
    cooldowns: Mutex<Cooldowns>,
    hidden: Mutex<Hidden>,
    /// The last dashboard `event` frames, so a socket that reconnects can catch up (R11).
    recent_events: Mutex<Recent>,
    seq: AtomicU64,
    sink: EventSink,
    live_tx: broadcast::Sender<LiveFrame>,
    overlay_tx: broadcast::Sender<OverlayFrame>,
    base_url: RwLock<String>,
    secrets: Arc<dyn ferrocast_secrets::Secrets>,
    apps: accounts::PlatformApps,
    adapters: Mutex<HashMap<Platform, tokio::task::JoinHandle<()>>>,
    youtube_wake: Mutex<Option<Arc<Notify>>>,
    relay_link: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Who chatted this interval, for watch-time points.
    activity: Mutex<ferrocast_core::loyalty::Activity>,
    points_cooldown: Mutex<loyalty::PointsCooldowns>,
    poll: Mutex<polls::PollSlot>,
    giveaway: tokio::sync::Mutex<giveaways::GiveawaySlot>,
}

impl Engine {
    /// Opens the store in `data_dir`, loads state, and starts the pipeline and alert tasks.
    /// Must be called inside a tokio runtime.
    pub async fn start(data_dir: &Path) -> Result<Engine, EngineError> {
        Self::start_with(data_dir, Arc::from(ferrocast_secrets::best_available()), PlatformApps::from_env()).await
    }

    /// Like [`Engine::start`] with a chosen secret store (tests use memory, never the
    /// developer's real keychain).
    pub async fn start_with_secrets(
        data_dir: &Path,
        secrets: Arc<dyn ferrocast_secrets::Secrets>,
    ) -> Result<Engine, EngineError> {
        Self::start_with(data_dir, secrets, PlatformApps::default()).await
    }

    /// Full control over secrets and platform apps (tests point the apps at fakes).
    pub async fn start_with(
        data_dir: &Path,
        secrets: Arc<dyn ferrocast_secrets::Secrets>,
        apps: PlatformApps,
    ) -> Result<Engine, EngineError> {
        let dir = data_dir.to_path_buf();
        let store = tokio::task::spawn_blocking(move || -> Result<Store, StoreError> {
            if let Some(id) = ferrocast_store::backup::apply_scheduled_restore(&dir)? {
                tracing::warn!(backup = %id, "restored a backup on start");
            }
            Store::open(&dir)
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        let store = Arc::new(store);

        let loaded = {
            let s = store.clone();
            tokio::task::spawn_blocking(move || load_state(&s))
                .await
                .map_err(|e| EngineError::Internal(e.to_string()))??
        };
        let (state, queue) = loaded;

        let (sink, rx) = mpsc::channel(SINK_CAPACITY);
        let (live_tx, _) = broadcast::channel(1_024);
        let (overlay_tx, _) = broadcast::channel(1_024);
        // Seqs start from the clock, so they keep growing across restarts and a dashboard
        // reconnecting with an old `since` still gets new events.
        let start_seq = u64::try_from(now().0).unwrap_or(0).saturating_mul(1000);
        let engine = Engine {
            inner: Arc::new(Inner {
                store,
                state: RwLock::new(state),
                connectors: RwLock::new(HashMap::new()),
                queue: tokio::sync::Mutex::new(queue),
                queue_changed: Notify::new(),
                config: tokio::sync::Mutex::new(()),
                dedupe: Mutex::new(Deduper::default()),
                cooldowns: Mutex::new(Cooldowns::default()),
                hidden: Mutex::new((HashSet::new(), VecDeque::new())),
                // Anything from before this start is gone: an older `since` gets `resync`.
                recent_events: Mutex::new(Recent { frames: VecDeque::new(), evicted_up_to: start_seq }),
                seq: AtomicU64::new(start_seq),
                sink,
                live_tx,
                overlay_tx,
                base_url: RwLock::new("http://127.0.0.1:47820".into()),
                secrets,
                apps,
                adapters: Mutex::new(HashMap::new()),
                youtube_wake: Mutex::new(None),
                relay_link: Mutex::new(None),
                activity: Mutex::new(Default::default()),
                points_cooldown: Mutex::new(Default::default()),
                poll: Mutex::new(Default::default()),
                giveaway: tokio::sync::Mutex::new(Default::default()),
            }),
        };
        tokio::spawn(prune(engine.clone()));
        tokio::spawn(pipeline::run(engine.clone(), rx));
        tokio::spawn(dispatcher::run(engine.clone()));
        tokio::spawn(timers::run(engine.clone()));
        tokio::spawn(loyalty::run(engine.clone()));
        tokio::spawn(polls::run(engine.clone()));
        tokio::spawn(giveaways::run(engine.clone()));
        engine.resume_accounts().await;
        Ok(engine)
    }

    /// Where adapters send events.
    pub fn sink(&self) -> EventSink {
        self.inner.sink.clone()
    }

    /// Submits an event (used by the API for tests and by in-process sources).
    pub async fn submit(&self, event: Event) -> Result<(), EngineError> {
        self.inner.sink.send(event).await.map_err(|_| EngineError::Unavailable("the engine is shutting down".into()))
    }

    pub fn subscribe_live(&self) -> broadcast::Receiver<LiveFrame> {
        self.inner.live_tx.subscribe()
    }

    pub fn subscribe_overlays(&self) -> broadcast::Receiver<OverlayFrame> {
        self.inner.overlay_tx.subscribe()
    }

    /// The URL the overlays are served from, set by the server once it knows its port.
    pub fn set_base_url(&self, url: &str) {
        *self.inner.base_url.write().unwrap_or_else(|p| p.into_inner()) = url.trim_end_matches('/').to_string();
    }

    pub fn base_url(&self) -> String {
        self.inner.base_url.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Registers a connected platform. Replaces any previous connector for it.
    pub fn register_connector(&self, c: Arc<dyn Connector>) {
        let platform = c.platform();
        self.inner.connectors.write().unwrap_or_else(|p| p.into_inner()).insert(platform, c);
    }

    pub fn unregister_connector(&self, platform: Platform) {
        self.inner.connectors.write().unwrap_or_else(|p| p.into_inner()).remove(&platform);
        let mut st = self.state_mut();
        st.connectors.insert(platform_key(platform).into(), ConnectorView::off());
        st.live.remove(&platform);
        st.viewers.remove(&platform);
    }

    /// Held while saving any setting (see `Inner::config`).
    pub(crate) async fn config_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.config.lock().await
    }

    pub(crate) fn connector(&self, platform: Platform) -> Option<Arc<dyn Connector>> {
        self.inner.connectors.read().unwrap_or_else(|p| p.into_inner()).get(&platform).cloned()
    }

    pub(crate) fn connectors(&self) -> Vec<Arc<dyn Connector>> {
        self.inner.connectors.read().unwrap_or_else(|p| p.into_inner()).values().cloned().collect()
    }

    /// Runs a store call on the blocking pool.
    pub async fn db<T, F>(&self, f: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?
            .map_err(EngineError::from)
    }

    pub fn store(&self) -> Arc<Store> {
        self.inner.store.clone()
    }

    pub(crate) fn state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner.state.read().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn state_mut(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.inner.state.write().unwrap_or_else(|p| p.into_inner())
    }

    /// A new frame number. Overlays drop frames at or below the last one they saw, so
    /// every frame sent after a snapshot needs its own.
    pub fn next_seq(&self) -> u64 {
        self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The latest sequence number handed out (for overlay `hello`).
    pub fn current_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::Relaxed)
    }

    pub(crate) fn send_live(&self, frame: LiveFrame) {
        let _ = self.inner.live_tx.send(frame);
    }

    /// Sends an `event` (or its `held` mark) and remembers it for sockets that reconnect.
    pub(crate) fn send_live_event(&self, seq: u64, frame: LiveFrame) {
        {
            let mut r = self.inner.recent_events.lock().unwrap_or_else(|p| p.into_inner());
            r.frames.push_back((seq, frame.clone()));
            while r.frames.len() > RECENT_EVENTS {
                if let Some((old, _)) = r.frames.pop_front() {
                    r.evicted_up_to = r.evicted_up_to.max(old);
                }
            }
        }
        self.send_live(frame);
    }

    /// Frames remembered after `since`, oldest first, and whether some after `since` were
    /// already forgotten (the dashboard should refetch).
    pub fn events_since(&self, since: u64) -> (Vec<LiveFrame>, bool) {
        let r = self.inner.recent_events.lock().unwrap_or_else(|p| p.into_inner());
        let gap = r.evicted_up_to > since;
        (r.frames.iter().filter(|(seq, _)| *seq > since).map(|(_, f)| f.clone()).collect(), gap)
    }

    pub(crate) fn send_overlay(&self, frame: OverlayFrame) {
        let _ = self.inner.overlay_tx.send(frame);
    }

    /// Hides a message from overlays now (memory) and for good (the log), so a reload or a
    /// restart never brings moderated chat back on stream.
    pub(crate) fn hide_message(&self, platform: Platform, id: &str) {
        if id.is_empty() {
            return;
        }
        self.remember_hidden(platform, id);
        let engine = self.clone();
        let sid = id.to_string();
        tokio::spawn(async move {
            if let Err(err) = engine.db(move |s| repo::events::hide(s, platform, &sid)).await {
                tracing::error!(%err, "couldn't mark a message hidden");
            }
        });
    }

    /// Hides a person's recent chat (banned or messages cleared).
    pub(crate) fn hide_user(&self, platform: Platform, actor_id: &str) {
        let engine = self.clone();
        let aid = actor_id.to_string();
        let since = now().0 - 24 * 3_600_000;
        tokio::spawn(async move {
            if let Err(err) = engine.db(move |s| repo::events::hide_user_chat(s, platform, &aid, since)).await {
                tracing::error!(%err, "couldn't hide a user's messages");
            }
        });
    }

    pub(crate) fn remember_hidden_pub(&self, platform: Platform, id: &str) {
        if !id.is_empty() {
            self.remember_hidden(platform, id);
        }
    }

    pub(crate) fn forget_hidden(&self, platform: Platform, id: &str) {
        self.inner.hidden.lock().unwrap_or_else(|p| p.into_inner()).0.remove(&(platform, id.to_string()));
    }

    fn remember_hidden(&self, platform: Platform, id: &str) {
        let mut h = self.inner.hidden.lock().unwrap_or_else(|p| p.into_inner());
        let key = (platform, id.to_string());
        if h.0.insert(key.clone()) {
            h.1.push_back(key);
            while h.1.len() > HIDDEN_KEEP {
                if let Some(old) = h.1.pop_front() {
                    h.0.remove(&old);
                }
            }
        }
    }

    pub(crate) fn is_hidden(&self, platform: Platform, id: &str) -> bool {
        self.inner.hidden.lock().unwrap_or_else(|p| p.into_inner()).0.contains(&(platform, id.to_string()))
    }

    /// Engine settings as saved.
    pub fn settings(&self) -> EngineSettings {
        self.state().settings.clone()
    }

    /// Re-reads everything from the store (after an import). Live connections stay up.
    /// Re-reads saved settings (after an import or restore). What's happening right now
    /// stays: connections, live state, panic mode, the TTS pause and "why" answers.
    pub async fn reload(&self) -> Result<(), EngineError> {
        // No config save can land between the read and the swap and be overwritten.
        let _one_at_a_time = self.inner.config.lock().await;
        let (fresh, _) = self.db(load_state).await?;
        let mut st = self.state_mut();
        let connectors = std::mem::take(&mut st.connectors);
        let live = std::mem::take(&mut st.live);
        let viewers = std::mem::take(&mut st.viewers);
        let session = std::mem::take(&mut st.session);
        let why = std::mem::take(&mut st.why);
        let (panic_on, tts_paused) = (st.panic_on, st.tts_paused);
        *st = State { connectors, live, viewers, session, why, panic_on, tts_paused, ..fresh };
        Ok(())
    }

    /// Saves only the settings that were sent, under the config lock so two dashboards
    /// saving different fields at once both win. Errors name the field.
    pub async fn patch_settings(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<EngineSettings, EngineError> {
        let _one_at_a_time = self.inner.config.lock().await;
        let current = serde_json::to_value(self.settings()).map_err(|e| EngineError::Internal(e.to_string()))?;
        let mut merged = current.clone();
        for (k, v) in patch {
            let Some(slot) = merged.get_mut(k) else { continue }; // unknown keys are ignored
            let mut one = current.clone();
            one[k.as_str()] = v.clone();
            if serde_json::from_value::<EngineSettings>(one).is_err() {
                let want = match &current[k.as_str()] {
                    serde_json::Value::Bool(_) => "true or false",
                    serde_json::Value::Number(_) => "a whole number",
                    _ => "text",
                };
                return Err(EngineError::Invalid(format!("{k}: must be {want}.")));
            }
            *slot = v.clone();
        }
        let s: EngineSettings = serde_json::from_value(merged).map_err(|e| EngineError::Invalid(e.to_string()))?;
        s.validate().map_err(EngineError::Invalid)?;
        let saved = s.clone();
        self.db(move |st| repo::settings::set(st, "engine", &saved)).await?;
        self.state_mut().settings = s.clone();
        Ok(s)
    }

    pub async fn save_settings(&self, s: EngineSettings) -> Result<EngineSettings, EngineError> {
        let _one_at_a_time = self.inner.config.lock().await;
        let saved = s.clone();
        self.db(move |st| repo::settings::set(st, "engine", &saved)).await?;
        self.state_mut().settings = s.clone();
        Ok(s)
    }
}

/// Deletes old chat from the log at start and every six hours (alerts and tips are kept).
async fn prune(engine: Engine) {
    loop {
        let days = engine.settings().keep_chat_days.max(1);
        match engine.db(move |s| repo::events::prune_chat(s, days)).await {
            Ok(n) if n > 0 => tracing::info!(removed = n, "pruned old chat"),
            Ok(_) => {}
            Err(err) => tracing::warn!(%err, "couldn't prune old chat"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
    }
}

/// How many recent `event` frames a reconnecting dashboard can catch up on.
const RECENT_EVENTS: usize = 500;

/// Recent dashboard frames for replay, and the highest seq already dropped.
#[derive(Debug, Default)]
struct Recent {
    frames: VecDeque<(u64, LiveFrame)>,
    evicted_up_to: u64,
}

pub fn platform_key(p: Platform) -> &'static str {
    match p {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::Kick => "kick",
        Platform::Tips => "tips",
        Platform::Ferrocast => "ferrocast",
    }
}

/// Loads everything the pipeline needs. A setting that fails to parse stops startup with
/// a clear error rather than silently resetting the streamer's configuration (R-34).
fn load_state(store: &Store) -> Result<(State, AlertQueue), StoreError> {
    use repo::settings::get_or_default;
    let mut st = State {
        settings: get_or_default(store, "engine")?,
        rules: repo::rules::list(store)?,
        filter: get_or_default(store, "filter")?,
        tts: get_or_default(store, "tts")?,
        commands: repo::bot::commands(store)?,
        timers: repo::bot::timers(store)?,
        bans: repo::bot::ban_list(store)?,
        panic: get_or_default(store, "panic")?,
        goals: repo::goals::list(store)?,
        loyalty: get_or_default(store, "loyalty")?,
        ..State::default()
    };
    for key in ["twitch", "youtube", "kick", "obs", "relay"] {
        st.connectors.insert(key.into(), ConnectorView::off());
    }
    let queue = repo::rules::load_queue(store)?.unwrap_or_default();
    Ok((st, queue))
}

#[cfg(test)]
mod tests;
