//! Wires adapters, the bot, moderation and alerts together (ARCHITECTURE.md §1).
//!
//! One task reads every event from a single channel, in order, and runs the pipeline in
//! [`pipeline`]. A second task drives the alert queue. Everything else (the HTTP server,
//! adapters) talks to the engine through [`Engine`]'s methods and channels.
//!
//! Startup is staged (R-30): the store loads, state is built, and only then does the
//! pipeline start reading. Adapters may connect earlier; their events wait in the channel.

mod actions;
mod dispatcher;
mod error;
mod pipeline;
mod snapshot;
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

pub use error::EngineError;
use frames::{LiveFrame, OverlayFrame};
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
    dedupe: Mutex<Deduper>,
    cooldowns: Mutex<Cooldowns>,
    hidden: Mutex<Hidden>,
    seq: AtomicU64,
    sink: EventSink,
    live_tx: broadcast::Sender<LiveFrame>,
    overlay_tx: broadcast::Sender<OverlayFrame>,
    base_url: RwLock<String>,
}

impl Engine {
    /// Opens the store in `data_dir`, loads state, and starts the pipeline and alert tasks.
    /// Must be called inside a tokio runtime.
    pub async fn start(data_dir: &Path) -> Result<Engine, EngineError> {
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
        let engine = Engine {
            inner: Arc::new(Inner {
                store,
                state: RwLock::new(state),
                connectors: RwLock::new(HashMap::new()),
                queue: tokio::sync::Mutex::new(queue),
                queue_changed: Notify::new(),
                dedupe: Mutex::new(Deduper::default()),
                cooldowns: Mutex::new(Cooldowns::default()),
                hidden: Mutex::new((HashSet::new(), VecDeque::new())),
                seq: AtomicU64::new(0),
                sink,
                live_tx,
                overlay_tx,
                base_url: RwLock::new("http://127.0.0.1:47820".into()),
            }),
        };
        tokio::spawn(pipeline::run(engine.clone(), rx));
        tokio::spawn(dispatcher::run(engine.clone()));
        tokio::spawn(timers::run(engine.clone()));
        Ok(engine)
    }

    /// Where adapters send events.
    pub fn sink(&self) -> EventSink {
        self.inner.sink.clone()
    }

    /// Submits an event (used by the API for tests and by in-process sources).
    pub async fn submit(&self, event: Event) -> Result<(), EngineError> {
        self.inner
            .sink
            .send(event)
            .await
            .map_err(|_| EngineError::Unavailable("the engine is shutting down".into()))
    }

    pub fn subscribe_live(&self) -> broadcast::Receiver<LiveFrame> {
        self.inner.live_tx.subscribe()
    }

    pub fn subscribe_overlays(&self) -> broadcast::Receiver<OverlayFrame> {
        self.inner.overlay_tx.subscribe()
    }

    /// The URL the overlays are served from, set by the server once it knows its port.
    pub fn set_base_url(&self, url: &str) {
        *self
            .inner
            .base_url
            .write()
            .unwrap_or_else(|p| p.into_inner()) = url.trim_end_matches('/').to_string();
    }

    pub fn base_url(&self) -> String {
        self.inner
            .base_url
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Registers a connected platform. Replaces any previous connector for it.
    pub fn register_connector(&self, c: Arc<dyn Connector>) {
        let platform = c.platform();
        self.inner
            .connectors
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(platform, c);
    }

    pub fn unregister_connector(&self, platform: Platform) {
        self.inner
            .connectors
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&platform);
        let mut st = self.state_mut();
        st.connectors
            .insert(platform_key(platform).into(), ConnectorView::off());
        st.live.remove(&platform);
        st.viewers.remove(&platform);
    }

    pub(crate) fn connector(&self, platform: Platform) -> Option<Arc<dyn Connector>> {
        self.inner
            .connectors
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&platform)
            .cloned()
    }

    pub(crate) fn connectors(&self) -> Vec<Arc<dyn Connector>> {
        self.inner
            .connectors
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
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

    pub(crate) fn next_seq(&self) -> u64 {
        self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The latest sequence number handed out (for overlay `hello`).
    pub fn current_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::Relaxed)
    }

    pub(crate) fn send_live(&self, frame: LiveFrame) {
        let _ = self.inner.live_tx.send(frame);
    }

    pub(crate) fn send_overlay(&self, frame: OverlayFrame) {
        let _ = self.inner.overlay_tx.send(frame);
    }

    pub(crate) fn hide_message(&self, platform: Platform, id: &str) {
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
        self.inner
            .hidden
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .contains(&(platform, id.to_string()))
    }

    /// Engine settings as saved.
    pub fn settings(&self) -> EngineSettings {
        self.state().settings.clone()
    }

    pub async fn save_settings(&self, s: EngineSettings) -> Result<EngineSettings, EngineError> {
        let saved = s.clone();
        self.db(move |st| repo::settings::set(st, "engine", &saved))
            .await?;
        self.state_mut().settings = s.clone();
        Ok(s)
    }
}

pub(crate) fn platform_key(p: Platform) -> &'static str {
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
