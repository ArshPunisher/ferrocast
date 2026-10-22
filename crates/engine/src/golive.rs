//! Go live everywhere (2.0, Track A; docs/plan/LIVE-STUDIO.md, API.md "Go live").
//!
//! OBS streams once to the local ingest ([`ferrocast_golive::Ingest`], 127.0.0.1 with this
//! install's key). When a stream starts, every ticked destination's key is fetched from its
//! platform (custom servers: the saved key) and one forwarder per destination sends the stream
//! on, each on its own. When OBS stops, they stop. Free: it all runs on the streamer's PC.
//!
//! Settings: `golive` (the destinations, top first). Secrets: the ingest key, each custom
//! destination's key, and OBS's own stream setting from before "Point OBS at Ferrocast".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ferrocast_core::event::Platform;
use ferrocast_golive::probe::{self, Need, Service, TWITCH_BANDWIDTH_TEST};
use ferrocast_golive::{Destination, Health, Hub, Ingest, PushState, StreamStats, Target, Verdict};
use ferrocast_obs::control::{self as obs, StreamService};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::frames::LiveFrame;
use crate::studio::save_setting;
use crate::{Engine, EngineError, now};

pub(crate) const CONFIG_KEY: &str = "golive";
const INGEST_KEY_SECRET: &str = "golive_ingest_key";
const OBS_BEFORE_SECRET: &str = "golive_obs_before";
const CUSTOM_KEY_PREFIX: &str = "golive_dest_";
/// Custom destinations at most (the upload is the real limit long before).
const MAX_CUSTOM: usize = 8;
/// How long the upload test sends to Twitch's bandwidth-test mode.
const UPLOAD_TEST: Duration = Duration::from_secs(6);
/// While live, the dashboard gets the health lines this often.
const HEALTH_EVERY: Duration = Duration::from_secs(1);
/// Before anything streamed, what a 1080p60 destination is planned at (video and audio).
const DEFAULT_NEED_KBPS: u32 = 6_160;

/// One place the stream goes, in the streamer's order (top first; the bottom one pauses first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DestinationConfig {
    /// `twitch`, `youtube`, `kick`, or `c_<random>` for a custom server.
    pub id: String,
    pub kind: Service,
    /// Custom servers only: the name the streamer gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Custom servers only: `rtmp://` or `rtmps://` (the key is a secret, not here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoLiveConfig {
    pub destinations: Vec<DestinationConfig>,
}

impl Default for GoLiveConfig {
    /// The three platforms, all off until the streamer ticks them.
    fn default() -> Self {
        let p = |id: &str, kind| DestinationConfig { id: id.into(), kind, name: None, url: None, enabled: false };
        Self {
            destinations: vec![p("twitch", Service::Twitch), p("youtube", Service::Youtube), p("kick", Service::Kick)],
        }
    }
}

/// One row of the last upload check.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UploadRow {
    pub id: String,
    pub name: String,
    /// The platform's server host, or why it couldn't be reached.
    pub server: Option<String>,
    pub delay_ms: Option<u32>,
    pub need_kbps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UploadCheck {
    pub at: i64,
    /// How the speed was measured: `twitch` (bandwidth-test mode) or `estimate`.
    pub measured_by: String,
    pub rows: Vec<UploadRow>,
    pub verdict: Verdict,
}

/// Everything the Go live tab shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoLiveView {
    /// Where OBS sends its stream (the key is fetched on its own, for manual setup).
    pub ingest: Option<String>,
    pub obs: ObsView,
    pub live: bool,
    pub stats: StreamStats,
    pub destinations: Vec<DestinationView>,
    pub upload: Option<UploadCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObsView {
    pub connected: bool,
    /// OBS's stream setting points at Ferrocast.
    pub points_here: bool,
    pub streaming: bool,
    /// What "Put OBS back" restores (`Twitch`, `a custom server`), when there's a saved one.
    pub before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DestinationView {
    pub id: String,
    pub kind: Service,
    pub name: String,
    /// The connected account's name (platforms), or the server host (custom).
    pub account: Option<String>,
    pub enabled: bool,
    /// The platform account is connected (custom servers: always).
    pub connected: bool,
    pub health: Option<Health>,
    /// Why this destination can't start (no account, a missing permission), in words.
    pub blocked: Option<String>,
    /// Its rules broken by the stream OBS is sending.
    pub rules: Vec<String>,
}

/// Go live's runtime state, one per engine.
#[derive(Debug)]
pub(crate) struct GoLive {
    pub hub: Arc<Hub>,
    pub cfg: RwLock<GoLiveConfig>,
    ingest: Mutex<Option<String>>,
    running: Mutex<HashMap<String, Running>>,
    blocked: Mutex<HashMap<String, String>>,
    upload: Mutex<Option<UploadCheck>>,
    /// One upload check or OBS change at a time.
    busy: tokio::sync::Mutex<()>,
    /// One start/stop pass at a time, so a destination is never started twice.
    reconciling: tokio::sync::Mutex<()>,
}

impl Default for GoLive {
    fn default() -> Self {
        Self {
            hub: Hub::new(),
            cfg: RwLock::new(GoLiveConfig::default()),
            ingest: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
            blocked: Mutex::new(HashMap::new()),
            upload: Mutex::new(None),
            busy: tokio::sync::Mutex::new(()),
            reconciling: tokio::sync::Mutex::new(()),
        }
    }
}

#[derive(Debug)]
struct Running {
    stop: watch::Sender<bool>,
    health: watch::Receiver<Health>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn platform_of(kind: Service) -> Option<Platform> {
    match kind {
        Service::Twitch => Some(Platform::Twitch),
        Service::Youtube => Some(Platform::Youtube),
        Service::Kick => Some(Platform::Kick),
        Service::Custom => None,
    }
}

fn dest_name(d: &DestinationConfig) -> String {
    match platform_of(d.kind) {
        Some(p) => p.label().to_string(),
        None => d.name.clone().unwrap_or_else(|| "Custom server".into()),
    }
}

fn refused(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> EngineError {
    EngineError::Refused { status: 422, code, message: message.into(), hint: hint.into(), retry_after: None }
}

impl Engine {
    fn golive(&self) -> &GoLive {
        &self.inner.golive
    }

    /// Loads the destinations and starts the local ingest on `port` (another free one if it's
    /// taken; 0 picks any, for tests) and the task that runs the forwarders.
    pub async fn start_golive(&self, port: u16) {
        use ferrocast_store::repo::settings::get_or_default;
        match self.db(|s| Ok(get_or_default::<GoLiveConfig>(s, CONFIG_KEY))).await {
            Ok(Ok(cfg)) => *self.golive().cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg,
            Ok(Err(e)) | Err(EngineError::Store(e)) => {
                tracing::error!(error = %e, "the Go live setting couldn't be read")
            }
            Err(e) => tracing::error!(error = %e, "the Go live setting couldn't be read"),
        }
        let key = match self.ingest_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "Go live: no ingest key, the local server stays off");
                return;
            }
        };
        let hub = self.golive().hub.clone();
        let ingest = match Ingest::bind(port, key.clone(), hub.clone()).await {
            Ok(i) => i,
            Err(_) if port != 0 => match Ingest::bind(0, key, hub).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::error!(error = %e, "Go live: the local server couldn't start");
                    return;
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "Go live: the local server couldn't start");
                return;
            }
        };
        *lock(&self.golive().ingest) = Some(ingest.server_url());
        tracing::info!(server = %ingest.server_url(), "Go live: OBS can stream here");
        // The ingest runs for the engine's life (it only listens on 127.0.0.1).
        let (_keep, never) = watch::channel(false);
        tokio::spawn(async move {
            let _keep = _keep;
            ingest.run(never).await;
        });
        tokio::spawn(run(self.clone()));
    }

    /// This install's key for the local ingest, made once and kept in the secret store.
    fn ingest_key(&self) -> Result<String, EngineError> {
        let secrets = &self.inner.secrets;
        if let Some(k) = secrets.get(INGEST_KEY_SECRET).map_err(|e| EngineError::Unavailable(e.to_string()))? {
            return Ok(k);
        }
        let mut b = [0u8; 18];
        getrandom::fill(&mut b).map_err(|e| EngineError::Internal(e.to_string()))?;
        let key = format!("fc_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        secrets.set(INGEST_KEY_SECRET, &key).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        Ok(key)
    }

    /// The local server and key, for setting OBS up by hand.
    pub async fn golive_ingest(&self) -> Result<(String, String), EngineError> {
        let server = lock(&self.golive().ingest).clone().ok_or_else(|| {
            refused("golive_off", "Ferrocast's local stream server isn't running.", "Restart Ferrocast and try again.")
        })?;
        Ok((server, self.ingest_key()?))
    }

    pub fn golive_config(&self) -> GoLiveConfig {
        self.golive().cfg.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The Go live tab: destinations with their health, OBS's setting, the stream's numbers.
    pub async fn golive_view(&self) -> GoLiveView {
        let ingest = lock(&self.golive().ingest).clone();
        let obs = self.golive_obs_view(ingest.as_deref()).await;
        self.view_with(ingest, obs)
    }

    fn view_with(&self, ingest: Option<String>, obs: ObsView) -> GoLiveView {
        let g = self.golive();
        let live = g.hub.live().borrow().on;
        let stats = g.hub.stats();
        let cfg = self.golive_config();
        let running = lock(&g.running);
        let blocked = lock(&g.blocked);
        let st = self.state();
        let destinations = cfg
            .destinations
            .iter()
            .map(|d| {
                let (connected, account) = match platform_of(d.kind) {
                    Some(p) => {
                        let c = st.connectors.get(crate::platform_key(p));
                        (self.connector(p).is_some(), c.and_then(|c| c.account.as_ref()).map(|a| a.name.clone()))
                    }
                    None => (true, d.url.as_deref().and_then(|u| Target::parse(u).ok()).map(|t| t.host)),
                };
                DestinationView {
                    id: d.id.clone(),
                    kind: d.kind,
                    name: dest_name(d),
                    account,
                    enabled: d.enabled,
                    connected,
                    health: running.get(&d.id).map(|r| r.health.borrow().clone()),
                    blocked: blocked.get(&d.id).cloned(),
                    rules: if d.enabled { probe::rules(&stats, d.kind) } else { Vec::new() },
                }
            })
            .collect();
        GoLiveView { ingest, obs, live, stats, destinations, upload: lock(&g.upload).clone() }
    }

    async fn golive_obs_view(&self, ingest: Option<&str>) -> ObsView {
        let before = self
            .inner
            .secrets
            .get(OBS_BEFORE_SECRET)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str::<StreamService>(&j).ok());
        let before = before.map(|b| describe_service(&b));
        let Ok(c) = self.obs_client().await else {
            return ObsView { connected: false, points_here: false, streaming: false, before };
        };
        let service = obs::stream_service(&c).await.ok();
        let streaming = obs::streaming(&c).await.unwrap_or(false);
        let points_here = matches!((&service, ingest), (Some(s), Some(i)) if s.points_at(i));
        ObsView { connected: true, points_here, streaming, before }
    }

    fn send_golive(&self) {
        let ingest = lock(&self.golive().ingest).clone();
        // OBS isn't asked on every tick: the dashboard reads it with GET.
        let obs = ObsView { connected: false, points_here: false, streaming: false, before: None };
        let mut v = self.view_with(ingest, obs);
        v.upload = None;
        self.send_live(LiveFrame::new(
            "golive",
            &serde_json::json!({ "live": v.live, "stats": v.stats, "destinations": v.destinations }),
        ));
    }

    /// Saves the destinations' order and switches (custom ones keep their name and server).
    /// While live, a newly ticked one starts and an unticked one stops at once.
    pub async fn set_golive_destinations(&self, order: Vec<(String, bool)>) -> Result<GoLiveConfig, EngineError> {
        let cfg = self.golive_config();
        if order.len() != cfg.destinations.len() {
            return Err(EngineError::Invalid("destinations: list every destination once, in the new order.".into()));
        }
        let mut next = Vec::with_capacity(order.len());
        for (id, enabled) in &order {
            let d = cfg.destinations.iter().find(|d| &d.id == id).ok_or_else(|| {
                EngineError::Invalid(format!(
                    "destinations: no destination \"{}\".",
                    id.chars().take(40).collect::<String>()
                ))
            })?;
            if next.iter().any(|x: &DestinationConfig| &x.id == id) {
                return Err(EngineError::Invalid("destinations: each destination only once.".into()));
            }
            next.push(DestinationConfig { enabled: *enabled, ..d.clone() });
        }
        let next = GoLiveConfig { destinations: next };
        self.save_golive(next.clone()).await?;
        self.reconcile().await;
        Ok(next)
    }

    /// Adds a custom destination: a name, an `rtmp://` or `rtmps://` server, and its key
    /// (kept in the secret store). It starts ticked.
    pub async fn add_golive_destination(
        &self,
        name: &str,
        url: &str,
        key: &str,
    ) -> Result<DestinationConfig, EngineError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 || name.chars().any(char::is_control) {
            return Err(EngineError::Invalid("name: 1 to 40 characters.".into()));
        }
        let target = Target::parse(url).map_err(|e| EngineError::Invalid(format!("url: {e}")))?;
        let key = key.trim();
        if key.is_empty() || key.len() > 512 || key.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(EngineError::Invalid("key: the stream key the server gave you, without spaces.".into()));
        }
        let mut cfg = self.golive_config();
        if cfg.destinations.iter().filter(|d| d.kind == Service::Custom).count() >= MAX_CUSTOM {
            return Err(EngineError::Conflict(format!("You can add up to {MAX_CUSTOM} servers of your own.")));
        }
        let mut b = [0u8; 6];
        getrandom::fill(&mut b).map_err(|e| EngineError::Internal(e.to_string()))?;
        let id = format!("c_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        self.inner
            .secrets
            .set(&format!("{CUSTOM_KEY_PREFIX}{id}"), key)
            .map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let d = DestinationConfig {
            id,
            kind: Service::Custom,
            name: Some(name.to_string()),
            url: Some(target.tc_url.clone()),
            enabled: true,
        };
        cfg.destinations.push(d.clone());
        self.save_golive(cfg).await?;
        self.reconcile().await;
        Ok(d)
    }

    /// Removes a custom destination and its key. The three platforms can't be removed, only
    /// switched off.
    pub async fn remove_golive_destination(&self, id: &str) -> Result<(), EngineError> {
        let mut cfg = self.golive_config();
        let Some(i) = cfg.destinations.iter().position(|d| d.id == id) else {
            return Err(EngineError::NotFound("destination"));
        };
        if cfg.destinations[i].kind != Service::Custom {
            return Err(EngineError::Invalid("Twitch, YouTube and Kick can be switched off, not removed.".into()));
        }
        cfg.destinations.remove(i);
        self.save_golive(cfg).await?;
        let _ = self.inner.secrets.delete(&format!("{CUSTOM_KEY_PREFIX}{id}"));
        self.reconcile().await;
        Ok(())
    }

    async fn save_golive(&self, cfg: GoLiveConfig) -> Result<(), EngineError> {
        save_setting(self, CONFIG_KEY, &cfg).await?;
        *self.golive().cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg;
        self.send_golive();
        Ok(())
    }

    /// Points OBS's stream setting at Ferrocast, keeping what was there to put back.
    /// Never while OBS streams (OBS refuses, and the stream would be cut).
    pub async fn golive_point_obs(&self) -> Result<ObsView, EngineError> {
        let _one = self.golive().busy.lock().await;
        let (server, key) = self.golive_ingest().await?;
        let c = self.obs_client().await?;
        if obs::streaming(&c).await.map_err(crate::studio::obs_error)? {
            return Err(refused(
                "obs_streaming",
                "OBS is streaming right now.",
                "Stop the stream in OBS first; Ferrocast changes this between streams.",
            ));
        }
        let now = obs::stream_service(&c).await.map_err(crate::studio::obs_error)?;
        if !now.points_at(&server) {
            let json = serde_json::to_string(&now).map_err(|e| EngineError::Internal(e.to_string()))?;
            self.inner.secrets.set(OBS_BEFORE_SECRET, &json).map_err(|e| EngineError::Unavailable(e.to_string()))?;
            obs::set_stream_service(&c, &StreamService::custom(&server, &key))
                .await
                .map_err(crate::studio::obs_error)?;
        }
        Ok(self.golive_obs_view(Some(&server)).await)
    }

    /// Puts OBS's stream setting back to what it was before [`Engine::golive_point_obs`].
    pub async fn golive_restore_obs(&self) -> Result<ObsView, EngineError> {
        let _one = self.golive().busy.lock().await;
        let saved = self.inner.secrets.get(OBS_BEFORE_SECRET).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        let before: StreamService = saved.as_deref().and_then(|j| serde_json::from_str(j).ok()).ok_or_else(|| {
            refused(
                "golive_nothing_saved",
                "There's no earlier OBS setting to put back.",
                "Set OBS's stream in OBS, Settings, Stream.",
            )
        })?;
        let c = self.obs_client().await?;
        if obs::streaming(&c).await.map_err(crate::studio::obs_error)? {
            return Err(refused(
                "obs_streaming",
                "OBS is streaming right now.",
                "Stop the stream in OBS first; Ferrocast changes this between streams.",
            ));
        }
        obs::set_stream_service(&c, &before).await.map_err(crate::studio::obs_error)?;
        let _ = self.inner.secrets.delete(OBS_BEFORE_SECRET);
        let ingest = lock(&self.golive().ingest).clone();
        Ok(self.golive_obs_view(ingest.as_deref()).await)
    }

    /// The upload check (pick 4B): each ticked destination's server and delay, the upload
    /// speed (Twitch's bandwidth-test mode when Twitch is ticked), and the verdict.
    pub async fn golive_upload_check(&self) -> Result<UploadCheck, EngineError> {
        let _one = self
            .golive()
            .busy
            .try_lock()
            .map_err(|_| EngineError::TooMany("An upload check or OBS change is already running.".into()))?;
        let cfg = self.golive_config();
        let ticked: Vec<DestinationConfig> = cfg.destinations.into_iter().filter(|d| d.enabled).collect();
        if ticked.is_empty() {
            return Err(refused(
                "golive_none",
                "No platform is ticked.",
                "Tick at least one platform, then check again.",
            ));
        }
        let stats = self.golive().hub.stats();
        let need = if stats.kbps > 0 { stats.kbps } else { DEFAULT_NEED_KBPS };
        let mut rows = Vec::new();
        let mut twitch: Option<(Target, String)> = None;
        for d in &ticked {
            let mut row = UploadRow {
                id: d.id.clone(),
                name: dest_name(d),
                server: None,
                delay_ms: None,
                need_kbps: need,
                problem: None,
            };
            match self.resolve(d).await {
                Ok(dest) => {
                    row.server = Some(dest.target.host.clone());
                    match probe::delay(&dest.target).await {
                        Ok(t) => row.delay_ms = Some(t.as_millis() as u32),
                        Err(e) => row.problem = Some(e.to_string()),
                    }
                    if d.kind == Service::Twitch && twitch.is_none() {
                        twitch = Some((dest.target.clone(), dest.key.clone()));
                    }
                }
                Err(reason) => row.problem = Some(reason),
            }
            rows.push(row);
        }
        let (upload, measured_by) = match twitch {
            Some((t, key)) => match probe::upload(&t, &format!("{key}{TWITCH_BANDWIDTH_TEST}"), UPLOAD_TEST).await {
                Ok(k) => (k, "twitch"),
                Err(e) => {
                    return Err(refused(
                        "golive_upload",
                        format!("The upload test couldn't run: {e}"),
                        "Check your internet connection, then try again.",
                    ));
                }
            },
            // Without Twitch: YouTube and Kick have no test mode (sending would go live), so
            // the speed comes from an upload to Cloudflare's speed test.
            None => match http_upload_kbps().await {
                Ok(k) => (k, "cloudflare"),
                Err(e) => {
                    return Err(refused(
                        "golive_upload",
                        format!("The upload test couldn't run: {e}"),
                        "Check your internet connection, then try again.",
                    ));
                }
            },
        };
        let needs: Vec<Need> = rows
            .iter()
            .filter(|r| r.problem.is_none())
            .map(|r| Need { name: r.name.clone(), kbps: r.need_kbps })
            .collect();
        let check =
            UploadCheck { at: now().0, measured_by: measured_by.into(), verdict: probe::verdict(upload, &needs), rows };
        *lock(&self.golive().upload) = Some(check.clone());
        Ok(check)
    }

    /// A destination's server and key: fetched from its platform, or the saved custom one.
    /// The error is a sentence for the streamer.
    async fn resolve(&self, d: &DestinationConfig) -> Result<Destination, String> {
        let (url, key) = match platform_of(d.kind) {
            Some(p) => {
                let c = self.connector(p).ok_or_else(|| format!("Connect {p} in Settings, Accounts first."))?;
                match c.ingest().await {
                    Ok(i) => (i.url, i.key),
                    Err(ferrocast_connector::ConnectorError::MissingScope { .. }) => {
                        return Err(format!(
                            "{p} needs one more permission to stream: reconnect {p} in Settings, Accounts."
                        ));
                    }
                    Err(e) => return Err(format!("Couldn't get the stream key from {p}: {e}")),
                }
            }
            None => {
                let url = d.url.clone().ok_or_else(|| "This server has no address.".to_string())?;
                let key =
                    self.inner.secrets.get(&format!("{CUSTOM_KEY_PREFIX}{}", d.id)).ok().flatten().ok_or_else(
                        || "This server's stream key is missing: remove it and add it again.".to_string(),
                    )?;
                (url, key)
            }
        };
        let target = Target::parse(&url).map_err(|e| e.to_string())?;
        Ok(Destination { id: d.id.clone(), target, key })
    }

    /// While live: starts forwarders for ticked destinations that aren't running and stops
    /// the ones no longer ticked.
    async fn reconcile(&self) {
        let g = self.golive();
        let _one = g.reconciling.lock().await;
        if !g.hub.live().borrow().on {
            return;
        }
        let ticked: Vec<DestinationConfig> =
            self.golive_config().destinations.into_iter().filter(|d| d.enabled).collect();
        let stale: Vec<String> =
            lock(&g.running).keys().filter(|id| !ticked.iter().any(|d| &d.id == *id)).cloned().collect();
        for id in stale {
            if let Some(r) = lock(&g.running).remove(&id) {
                r.stop.send_replace(true);
            }
            lock(&g.blocked).remove(&id);
        }
        for d in ticked {
            if lock(&g.running).contains_key(&d.id) {
                continue;
            }
            self.start_destination(&d).await;
        }
        self.send_golive();
    }

    async fn start_destination(&self, d: &DestinationConfig) {
        let g = self.golive();
        match self.resolve(d).await {
            Ok(dest) => {
                lock(&g.blocked).remove(&d.id);
                let (health_tx, health) =
                    watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
                let (stop, stop_rx) = watch::channel(false);
                tokio::spawn(ferrocast_golive::push::run(dest, g.hub.clone(), health_tx, stop_rx));
                lock(&g.running).insert(d.id.clone(), Running { stop, health });
            }
            Err(reason) => {
                tracing::info!(dest = %d.id, %reason, "Go live: a destination can't start");
                lock(&g.blocked).insert(d.id.clone(), reason);
            }
        }
    }

    fn stop_all(&self) {
        let g = self.golive();
        for (_, r) in lock(&g.running).drain() {
            r.stop.send_replace(true);
        }
        lock(&g.blocked).clear();
    }
}

/// Cloudflare's speed-test upload endpoint (no account, nothing kept).
const SPEED_TEST_URL: &str = "https://speed.cloudflare.com/__up";

/// Upload speed by HTTP: a small upload to gauge the line, then one sized to take about four
/// seconds, so a slow line isn't kept busy for a minute.
async fn http_upload_kbps() -> Result<u32, String> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let send = |bytes: usize| {
        let http = http.clone();
        async move {
            let body: Vec<u8> = (0..bytes).map(|_| fastrand::u8(..)).collect();
            let start = std::time::Instant::now();
            let res = http.post(SPEED_TEST_URL).body(body).send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("the speed test answered {}", res.status().as_u16()));
            }
            Ok::<f64, String>(bytes as f64 * 8.0 / 1000.0 / start.elapsed().as_secs_f64().max(0.001))
        }
    };
    let rough = send(1_000_000).await?;
    let size = ((rough * 1000.0 / 8.0) * 4.0).clamp(1_000_000.0, 25_000_000.0) as usize;
    Ok(send(size).await? as u32)
}

/// "Twitch", "a custom server": what OBS's earlier setting was, without its key.
fn describe_service(s: &StreamService) -> String {
    match s.settings.get("service").and_then(|v| v.as_str()) {
        Some(name) if s.kind == "rtmp_common" => name.chars().take(40).collect(),
        _ => "a custom server".into(),
    }
}

/// Starts the destinations when OBS starts streaming and stops them when it stops; while
/// live, sends the health lines every second.
pub(crate) async fn run(engine: Engine) {
    let mut live = engine.golive().hub.live();
    let mut done = 0u64;
    loop {
        let Ok(stream) = live.wait_for(|l| l.on && l.stream > done).await.map(|l| l.stream) else { return };
        tracing::info!(stream, "Go live: OBS started streaming");
        engine.reconcile().await;
        loop {
            tokio::select! {
                changed = live.wait_for(|l| !(l.on && l.stream == stream)) => {
                    if changed.is_err() { return; }
                    break;
                }
                _ = tokio::time::sleep(HEALTH_EVERY) => engine.send_golive(),
            }
        }
        engine.stop_all();
        done = stream;
        engine.send_golive();
        tracing::info!(stream, "Go live: OBS stopped streaming");
    }
}
