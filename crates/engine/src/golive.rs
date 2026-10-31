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
pub use ferrocast_golive::cut::{CutBox, CutConfig, Layout};
pub use ferrocast_golive::cutter::CutState as CutStateView;
use ferrocast_golive::cutter::CutState;
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
/// Aitum Vertical's first output before Ferrocast pointed it here: `{ "server", "key" }`.
const VERTICAL_BEFORE_SECRET: &str = "golive_vertical_before";
const CUSTOM_KEY_PREFIX: &str = "golive_dest_";
/// Custom destinations at most (the upload is the real limit long before).
const MAX_CUSTOM: usize = 8;
/// How long the upload test sends to Twitch's bandwidth-test mode.
const UPLOAD_TEST: Duration = Duration::from_secs(6);
/// While live, the dashboard gets the health lines this often.
const HEALTH_EVERY: Duration = Duration::from_secs(1);
/// Before anything streamed, what a 1080p60 destination is planned at (video and audio).
const DEFAULT_NEED_KBPS: u32 = 6_160;
/// The same for an upright 1080×1920 stream from OBS's vertical canvas.
const DEFAULT_VERTICAL_NEED_KBPS: u32 = 4_660;
/// YouTube's ingest, for the upload check of the vertical stream (resolving it would make one).
const YOUTUBE_INGEST: &str = "rtmps://a.rtmps.youtube.com/live2";

/// Which picture a destination gets: OBS's normal stream, or its vertical canvas (A2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Canvas {
    #[default]
    Main,
    Vertical,
}

fn is_main(c: &Canvas) -> bool {
    *c == Canvas::Main
}

/// One place the stream goes, in the streamer's order (top first; the bottom one pauses first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DestinationConfig {
    /// `twitch`, `youtube`, `kick`, or `c_<random>` for a custom server.
    pub id: String,
    pub kind: Service,
    /// Custom servers only: the name the streamer gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Custom servers only: `rtmp://` or `rtmps://`, or `srt://`, `rist://` or an `https://`
    /// WebRTC (WHIP) address sent straight from the Ferrocast Studio (the key is a secret,
    /// not here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: Canvas,
    /// Gets the lighter stream (720p, 3,000 kbps) the Ferrocast Studio sends beside the full
    /// one: for a platform whose route is slow, or that takes less. The main picture only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub light: bool,
}

impl DestinationConfig {
    /// Takes the lighter stream (the main picture, an RTMP platform).
    pub fn wants_light(&self) -> bool {
        self.light && self.canvas == Canvas::Main && self.url.as_deref().and_then(direct_kind).is_none()
    }
}

/// A server address the Studio sends to itself (SRT, RIST, WebRTC/WHIP), not an RTMP
/// forwarder: "srt", "rist" or "whip".
pub fn direct_kind(url: &str) -> Option<&'static str> {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("srt://") {
        Some("srt")
    } else if u.starts_with("rist://") {
        Some("rist")
    } else if u.starts_with("https://") || is_local_http(&u) {
        Some("whip")
    } else {
        None
    }
}

/// `http://` on this computer only (a local WHIP server); anywhere else WebRTC needs https.
fn is_local_http(u: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| u.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with([':', '/'])))
}

/// A direct address's host and port, for the list.
fn direct_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let host = rest.split(['/', '?']).next()?;
    (!host.is_empty()).then(|| host.chars().take(80).collect())
}

/// How often a direct destination's health is read from the Studio.
const DIRECT_EVERY: Duration = Duration::from_secs(1);

/// YouTube Shorts live: the vertical stream's platform destination.
pub const YOUTUBE_VERTICAL: &str = "youtube_vertical";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoLiveConfig {
    pub destinations: Vec<DestinationConfig>,
    /// Upload protection (A1.8): pause the bottom destination when the upload can't carry
    /// them all. On unless the streamer turns it off.
    #[serde(default = "on")]
    pub protect: bool,
    /// The streamer said they run Aitum Multistream on purpose: the setup note stays hidden
    /// (the live "sent twice" warning still shows).
    #[serde(default)]
    pub aitum_ok: bool,
    /// Where the vertical picture comes from (A2.3 canvas, A2.4 cut).
    #[serde(default)]
    pub vertical_source: VerticalSource,
    /// The cut-from-stream layout (A2.4).
    #[serde(default)]
    pub cut: CutConfig,
    /// Advanced mode's settings (A2.5): defaults that work for most people.
    #[serde(default)]
    pub advanced: Advanced,
}

/// Go live's Advanced settings (board items 2B and 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// Upload protection's rule.
    pub rules: ferrocast_golive::protect::Rules,
    /// How much of the upload the check keeps free for the game and chat, in percent.
    pub keep_free_pct: u8,
    /// The local server's port (used from the next start).
    pub port: u16,
    /// Per destination: its own server address and retry timing.
    pub per: HashMap<String, DestAdvanced>,
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            rules: Default::default(),
            keep_free_pct: 20,
            port: ferrocast_golive::ingest::DEFAULT_PORT,
            per: HashMap::new(),
        }
    }
}

/// One destination's Advanced settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DestAdvanced {
    /// A platform's server instead of the one it gives (another region; rtmp or rtmps).
    pub server: Option<String>,
    pub first_retry_s: u32,
    pub max_retry_s: u32,
}

impl Default for DestAdvanced {
    fn default() -> Self {
        Self { server: None, first_retry_s: 1, max_retry_s: 30 }
    }
}

impl Advanced {
    /// Messages name the field.
    pub fn validate(&self, destinations: &[DestinationConfig]) -> Result<(), String> {
        self.rules.validate().map_err(|m| format!("rules.{m}"))?;
        if let Some(id) = &self.rules.first_to_pause
            && !destinations.iter().any(|d| &d.id == id)
        {
            return Err("rules.first_to_pause: no such destination.".into());
        }
        if self.keep_free_pct > 50 {
            return Err("keep_free_pct: 0 to 50 percent.".into());
        }
        if self.port < 1024 {
            return Err("port: 1024 to 65535.".into());
        }
        for (id, a) in &self.per {
            let d =
                destinations.iter().find(|d| &d.id == id).ok_or_else(|| format!("per.{id}: no such destination."))?;
            if let Some(server) = &a.server {
                if d.kind == Service::Custom {
                    return Err(format!("per.{id}.server: a server of your own already has its address."));
                }
                Target::parse(server).map_err(|e| format!("per.{id}.server: {e}"))?;
            }
            if !(1..=30).contains(&a.first_retry_s) {
                return Err(format!("per.{id}.first_retry_s: 1 to 30 seconds."));
            }
            if a.max_retry_s < a.first_retry_s || a.max_retry_s > 600 {
                return Err(format!("per.{id}.max_retry_s: at least the first wait, at most 600 seconds."));
            }
        }
        Ok(())
    }

    fn retry(&self, id: &str) -> ferrocast_golive::push::Retry {
        let a = self.per.get(id).cloned().unwrap_or_default();
        ferrocast_golive::push::Retry {
            first: Duration::from_secs(u64::from(a.first_retry_s)),
            max: Duration::from_secs(u64::from(a.max_retry_s)),
        }
    }
}

/// Where the vertical picture comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerticalSource {
    /// OBS's vertical canvas (Aitum Vertical) streams to the vertical key.
    #[default]
    Canvas,
    /// Ferrocast cuts it from the normal stream with ffmpeg.
    Cut,
}

fn on() -> bool {
    true
}

impl GoLiveConfig {
    /// Saved before 2.0's vertical stream: YouTube Shorts live is added, off, at the end.
    fn with_vertical(mut self) -> Self {
        if !self.destinations.iter().any(|d| d.id == YOUTUBE_VERTICAL) {
            let yt = GoLiveConfig::default().destinations.into_iter().find(|d| d.id == YOUTUBE_VERTICAL);
            self.destinations.extend(yt);
        }
        self
    }
}

impl Default for GoLiveConfig {
    /// The three platforms, all off until the streamer ticks them.
    fn default() -> Self {
        let p = |id: &str, kind| DestinationConfig {
            id: id.into(),
            kind,
            name: None,
            url: None,
            enabled: false,
            canvas: Canvas::Main,
            light: false,
        };
        Self {
            destinations: vec![
                p("twitch", Service::Twitch),
                p("youtube", Service::Youtube),
                p("kick", Service::Kick),
                DestinationConfig { canvas: Canvas::Vertical, ..p(YOUTUBE_VERTICAL, Service::Youtube) },
            ],
            protect: true,
            aitum_ok: false,
            vertical_source: VerticalSource::Canvas,
            cut: CutConfig::default(),
            advanced: Advanced::default(),
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
    /// OBS's vertical canvas is streaming to Ferrocast (its second key).
    pub vertical_live: bool,
    pub vertical_stats: StreamStats,
    pub destinations: Vec<DestinationView>,
    pub upload: Option<UploadCheck>,
    /// Upload protection is on (A1.8).
    pub protect: bool,
    /// The streamer said they run Aitum Multistream on purpose (A2.2).
    pub aitum_ok: bool,
    /// The vertical source and, for cutting, its layout and what the cutter is doing (A2.4).
    pub cut: CutView,
    /// Advanced mode's settings (A2.5).
    pub advanced: Advanced,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CutView {
    pub source: VerticalSource,
    pub config: CutConfig,
    /// `None` while it isn't needed (another source, or no vertical destination ticked).
    pub state: Option<CutState>,
    /// Whether ffmpeg is here or being fetched (cutting needs it).
    pub ffmpeg: crate::ffmpeg_get::FfmpegState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObsView {
    pub connected: bool,
    /// OBS's stream setting points at Ferrocast.
    pub points_here: bool,
    pub streaming: bool,
    /// What "Put OBS back" restores (`Twitch`, `a custom server`), when there's a saved one.
    pub before: Option<String>,
    /// Aitum Multistream's outputs, when OBS has that plugin (A2.2).
    pub aitum: Option<Vec<AitumOutput>>,
    /// OBS's vertical canvas (Aitum Vertical), when OBS has it (A2.3).
    pub vertical: Option<VerticalObs>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerticalObs {
    /// Its stream goes to Ferrocast's vertical key.
    pub points_here: bool,
    pub streaming: bool,
    /// Where "Put it back" sends it again (the server's host, never the key).
    pub before: Option<String>,
}

/// Aitum Vertical's setting before Ferrocast, kept in the secret store (it holds a key).
#[derive(Serialize, Deserialize)]
struct VerticalBefore {
    server: String,
    key: String,
}

/// One Aitum Multistream output: where it goes (when Ferrocast can tell) and whether it's
/// sending now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AitumOutput {
    pub name: String,
    pub platform: Option<Service>,
    pub live: bool,
}

/// Which platform an Aitum output streams to, from its server (else its name).
pub(crate) fn aitum_platform(host: Option<&str>, name: &str) -> Option<Service> {
    if let Some(h) = host {
        // Kick runs on Amazon IVS like Twitch: its own ingest channel comes first.
        if h.starts_with("fa723fc1b171.") || h.contains("kick") {
            return Some(Service::Kick);
        }
        if h == "youtube.com" || h.ends_with(".youtube.com") {
            return Some(Service::Youtube);
        }
        if h.ends_with("contribute.live-video.net") || h.ends_with(".twitch.tv") {
            return Some(Service::Twitch);
        }
    }
    let n = name.to_lowercase();
    [("twitch", Service::Twitch), ("youtube", Service::Youtube), ("kick", Service::Kick)]
        .into_iter()
        .find(|(w, _)| n.contains(w))
        .map(|(_, s)| s)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DestinationView {
    pub id: String,
    pub kind: Service,
    pub canvas: Canvas,
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
    /// Sent straight from the Ferrocast Studio by this protocol ("srt", "rist", "whip");
    /// absent for RTMP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct: Option<&'static str>,
    /// Takes the Studio's lighter stream (left out when not).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub light: bool,
}

/// Go live's runtime state, one per engine.
#[derive(Debug)]
pub(crate) struct GoLive {
    pub hub: Arc<Hub>,
    /// OBS's vertical canvas (A2.3).
    pub vhub: Arc<Hub>,
    /// The Studio's lighter stream, for platforms that take it (`DestinationConfig::light`).
    pub lhub: Arc<Hub>,
    pub cfg: RwLock<GoLiveConfig>,
    ingest: Mutex<Option<String>>,
    running: Mutex<HashMap<String, Running>>,
    blocked: Mutex<HashMap<String, String>>,
    upload: Mutex<Option<UploadCheck>>,
    /// The last stream's measured rate, for planning the next one (0: none yet).
    last_kbps: std::sync::atomic::AtomicU32,
    /// One upload check or OBS change at a time.
    busy: tokio::sync::Mutex<()>,
    /// One start/stop pass at a time, so a destination is never started twice.
    reconciling: tokio::sync::Mutex<()>,
    /// Upload protection's state: what's paused and until when.
    protector: Mutex<ferrocast_golive::protect::Protector>,
    /// Paused destinations' health lines (their forwarders aren't running).
    paused: Mutex<HashMap<String, Health>>,
    /// The cutter for the vertical picture, while it runs (A2.4).
    cut: Mutex<Option<CutRun>>,
    /// The engine's data folder (where fetched ffmpeg lives).
    data_dir: Mutex<Option<std::path::PathBuf>>,
    /// Stops the local server (for a new key) and its task, to wait for the port to free.
    ingest_task: Mutex<Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)>>,
    /// Fetching ffmpeg: progress or why it failed (`None`: not fetching).
    ffmpeg_fetch: Mutex<Option<crate::ffmpeg_get::FfmpegState>>,
}

#[derive(Debug)]
struct CutRun {
    config: CutConfig,
    stop: watch::Sender<bool>,
    state: watch::Receiver<CutState>,
}

impl Default for GoLive {
    fn default() -> Self {
        Self {
            hub: Hub::new(),
            vhub: Hub::new(),
            lhub: Hub::new(),
            cfg: RwLock::new(GoLiveConfig::default()),
            ingest: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
            blocked: Mutex::new(HashMap::new()),
            upload: Mutex::new(None),
            last_kbps: std::sync::atomic::AtomicU32::new(0),
            busy: tokio::sync::Mutex::new(()),
            reconciling: tokio::sync::Mutex::new(()),
            protector: Mutex::new(Default::default()),
            paused: Mutex::new(HashMap::new()),
            cut: Mutex::new(None),
            data_dir: Mutex::new(None),
            ingest_task: Mutex::new(None),
            ffmpeg_fetch: Mutex::new(None),
        }
    }
}

#[derive(Debug)]
struct Running {
    stop: watch::Sender<bool>,
    health: watch::Receiver<Health>,
    /// Sending the lighter stream (it moves when that starts or stops).
    light: bool,
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
        Some(Platform::Youtube) if d.canvas == Canvas::Vertical => "YouTube Shorts live".into(),
        Some(p) => p.label().to_string(),
        None => d.name.clone().unwrap_or_else(|| "Custom server".into()),
    }
}

fn cfg_cut_source(c: &GoLiveConfig) -> bool {
    c.vertical_source == VerticalSource::Cut
}

/// ffmpeg for cutting the vertical picture: `FERROCAST_FFMPEG`, next to the app, then the
/// usual places it's installed, then `PATH`.
pub fn find_ffmpeg() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("FERROCAST_FFMPEG") {
        c.push(PathBuf::from(p));
    }
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        c.push(dir.join(exe));
        c.push(dir.join("../Resources").join(exe));
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        c.push(PathBuf::from(d).join(exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        c.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    c.into_iter().find(|p| p.is_file())
}

/// The vertical canvas's local key: unguessable like the main one, and never the same.
fn vertical_key(main: &str) -> String {
    format!("{main}v")
}

/// The lighter stream's local key, likewise.
fn light_key(main: &str) -> String {
    format!("{main}l")
}

pub(crate) fn refused_pub(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> EngineError {
    refused(code, message, hint)
}

fn refused(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> EngineError {
    EngineError::Refused { status: 422, code, message: message.into(), hint: hint.into(), retry_after: None }
}

impl Engine {
    fn golive(&self) -> &GoLive {
        &self.inner.golive
    }

    /// Whether a stream is coming in to Go live now, and its changes.
    pub(crate) fn golive_live(&self) -> tokio::sync::watch::Receiver<ferrocast_golive::hub::Live> {
        self.golive().hub.live()
    }

    /// Loads the destinations and starts the local ingest on `port` (another free one if it's
    /// taken; 0 picks any, for tests) and the task that runs the forwarders.
    pub async fn start_golive(&self, port: u16) {
        use ferrocast_store::repo::settings::get_or_default;
        match self.db(|s| Ok(get_or_default::<GoLiveConfig>(s, CONFIG_KEY))).await {
            Ok(Ok(cfg)) => {
                lock(&self.golive().protector).set_rules(cfg.advanced.rules.clone());
                *self.golive().cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg.with_vertical();
            }
            Ok(Err(e)) | Err(EngineError::Store(e)) => {
                tracing::error!(error = %e, "the Go live setting couldn't be read")
            }
            Err(e) => tracing::error!(error = %e, "the Go live setting couldn't be read"),
        }
        if let Ok(dir) = self.db(|s| Ok(s.dir().to_path_buf())).await {
            *lock(&self.golive().data_dir) = Some(dir);
        }
        let key = match self.ingest_key() {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "Go live: no ingest key, the local server stays off");
                return;
            }
        };
        // The streamer's port from Advanced mode (0 is for tests: any free port).
        let port = if port == 0 { 0 } else { self.golive_config().advanced.port };
        let (hub, vhub, lhub) = (self.golive().hub.clone(), self.golive().vhub.clone(), self.golive().lhub.clone());
        let (vkey, lkey) = (vertical_key(&key), light_key(&key));
        let bound = match Ingest::bind(port, key.clone(), hub.clone()).await {
            Ok(i) => Ok(i),
            Err(_) if port != 0 => Ingest::bind(0, key, hub).await,
            Err(e) => Err(e),
        };
        let ingest = match bound.map(|i| i.with_route(vkey, vhub).with_route(lkey, lhub)) {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "Go live: the local server couldn't start");
                return;
            }
        };
        *lock(&self.golive().ingest) = Some(ingest.server_url());
        tracing::info!(server = %ingest.server_url(), "Go live: OBS can stream here");
        // The ingest runs for the engine's life (it only listens on 127.0.0.1), or until a new
        // key replaces it.
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(ingest.run(stop_rx));
        *lock(&self.golive().ingest_task) = Some((stop, task));
        tokio::spawn(run(self.clone()));
    }

    /// "Make a new key": a new local key (and vertical key), the local server restarted with it,
    /// and OBS's stream setting and Aitum Vertical pointed at it again if they pointed here.
    /// Never while either stream is live.
    pub async fn golive_new_key(&self) -> Result<(), EngineError> {
        let g = self.golive();
        if g.hub.live().borrow().on || g.vhub.live().borrow().on || g.lhub.live().borrow().on {
            return Err(refused(
                "golive_streaming",
                "A stream is coming in right now.",
                "Stop streaming in OBS first; the key changes between streams.",
            ));
        }
        let ingest = lock(&g.ingest).clone();
        let obs = self.golive_obs_view(ingest.as_deref()).await;
        let (obs_here, vertical_here) = (obs.points_here, obs.vertical.as_ref().is_some_and(|v| v.points_here));
        let port = ingest
            .as_deref()
            .and_then(|u| u.rsplit(':').next())
            .and_then(|p| p.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(ferrocast_golive::ingest::DEFAULT_PORT);
        {
            let _one = g.busy.lock().await;
            self.inner.secrets.delete(INGEST_KEY_SECRET).map_err(|e| EngineError::Unavailable(e.to_string()))?;
            let key = self.ingest_key()?;
            let old = lock(&g.ingest_task).take();
            if let Some((stop, task)) = old {
                stop.send_replace(true);
                let _ = task.await;
            }
            // The same port, else any free one (OBS is pointed at the new address below).
            let bound = match Ingest::bind(port, key.clone(), g.hub.clone()).await {
                Ok(i) => Ok(i),
                Err(_) => Ingest::bind(0, key.clone(), g.hub.clone()).await,
            };
            let ingest = bound
                .map_err(|e| EngineError::Unavailable(format!("the local server couldn't restart: {e}")))?
                .with_route(vertical_key(&key), g.vhub.clone())
                .with_route(light_key(&key), g.lhub.clone());
            *lock(&g.ingest) = Some(ingest.server_url());
            let (stop, stop_rx) = watch::channel(false);
            let task = tokio::spawn(ingest.run(stop_rx));
            *lock(&g.ingest_task) = Some((stop, task));
        }
        tracing::info!("Go live: a new local stream key");
        // Both are tried even if one fails, so neither keeps the old key silently.
        let main = if obs_here { self.golive_point_obs().await.map(|_| ()) } else { Ok(()) };
        let vertical = if vertical_here { self.golive_point_vertical().await.map(|_| ()) } else { Ok(()) };
        self.reconcile().await;
        main.and(vertical)
    }

    pub(crate) fn golive_data_dir(&self) -> Option<std::path::PathBuf> {
        lock(&self.golive().data_dir).clone()
    }

    pub(crate) fn golive_ffmpeg_fetch(&self) -> Option<crate::ffmpeg_get::FfmpegState> {
        lock(&self.golive().ffmpeg_fetch).clone()
    }

    /// Marks ffmpeg as being fetched unless it already is (one check under one lock, so two
    /// requests at once start one download). True when this caller should fetch it.
    pub(crate) fn claim_ffmpeg_fetch(&self) -> bool {
        use crate::ffmpeg_get::FfmpegState;
        let mut f = lock(&self.golive().ffmpeg_fetch);
        if matches!(*f, Some(FfmpegState::Downloading { .. })) {
            return false;
        }
        *f = Some(FfmpegState::Downloading { percent: 0 });
        true
    }

    pub(crate) fn set_golive_ffmpeg_fetch(&self, s: Option<crate::ffmpeg_get::FfmpegState>) {
        *lock(&self.golive().ffmpeg_fetch) = s;
        self.send_golive();
    }

    pub(crate) fn send_golive_pub(&self) {
        self.send_golive();
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

    /// The key for OBS's vertical canvas: the same local server, its own key.
    pub async fn golive_vertical_ingest(&self) -> Result<(String, String), EngineError> {
        let (server, key) = self.golive_ingest().await?;
        Ok((server, vertical_key(&key)))
    }

    /// Where the Studio sends its lighter stream: the local server and the light key.
    pub async fn golive_light_ingest(&self) -> Result<(String, String), EngineError> {
        let (server, key) = self.golive_ingest().await?;
        Ok((server, light_key(&key)))
    }

    /// Whether a destination takes the lighter stream now: it wants it and the Studio sends it.
    /// Until then (or streaming from OBS, which has no lighter copy) it takes the full one.
    fn on_light(&self, d: &DestinationConfig) -> bool {
        d.wants_light() && self.golive().lhub.live().borrow().on
    }

    /// The stream a destination takes: the lighter one, the vertical one or the main one.
    fn hub_of(&self, d: &DestinationConfig) -> &Arc<Hub> {
        if self.on_light(d) { &self.golive().lhub } else { self.hub_for(d.canvas) }
    }

    fn hub_for(&self, c: Canvas) -> &Arc<Hub> {
        match c {
            Canvas::Main => &self.golive().hub,
            Canvas::Vertical => &self.golive().vhub,
        }
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
        let vertical_live = g.vhub.live().borrow().on;
        let vertical_stats = g.vhub.stats();
        let cfg = self.golive_config();
        let running = lock(&g.running);
        let paused = lock(&g.paused);
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
                    None => (
                        true,
                        d.url.as_deref().and_then(|u| match direct_kind(u) {
                            Some(_) => direct_host(u),
                            None => Target::parse(u).ok().map(|t| t.host),
                        }),
                    ),
                };
                DestinationView {
                    id: d.id.clone(),
                    kind: d.kind,
                    canvas: d.canvas,
                    name: dest_name(d),
                    account,
                    enabled: d.enabled,
                    connected,
                    health: running
                        .get(&d.id)
                        .map(|r| r.health.borrow().clone())
                        .or_else(|| paused.get(&d.id).cloned()),
                    blocked: blocked.get(&d.id).cloned(),
                    rules: match (d.enabled, d.canvas) {
                        (false, _) => Vec::new(),
                        (true, Canvas::Main) => probe::rules(&stats, d.kind),
                        (true, Canvas::Vertical) => probe::rules(&vertical_stats, d.kind),
                    },
                    direct: d.url.as_deref().and_then(direct_kind),
                    light: d.wants_light(),
                }
            })
            .collect();
        GoLiveView {
            ingest,
            obs,
            live,
            stats,
            vertical_live,
            vertical_stats,
            destinations,
            upload: lock(&g.upload).clone(),
            protect: cfg.protect,
            aitum_ok: cfg.aitum_ok,
            advanced: cfg.advanced.clone(),
            cut: CutView {
                source: cfg.vertical_source,
                config: cfg.cut.clone(),
                state: lock(&g.cut).as_ref().map(|c| c.state.borrow().clone()),
                ffmpeg: self.ffmpeg_state(),
            },
        }
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
            return ObsView {
                connected: false,
                points_here: false,
                streaming: false,
                before,
                aitum: None,
                vertical: None,
            };
        };
        let service = obs::stream_service(&c).await.ok();
        let streaming = obs::streaming(&c).await.unwrap_or(false);
        let points_here = matches!((&service, ingest), (Some(s), Some(i)) if s.points_at(i));
        let aitum = self.aitum_outputs(&c).await;
        let vertical = self.vertical_obs(&c).await;
        ObsView { connected: true, points_here, streaming, before, aitum, vertical }
    }

    fn vertical_before(&self) -> Option<VerticalBefore> {
        let j = self.inner.secrets.get(VERTICAL_BEFORE_SECRET).ok().flatten()?;
        serde_json::from_str(&j).ok()
    }

    async fn vertical_obs(&self, c: &ferrocast_obs::Client) -> Option<VerticalObs> {
        let streaming = ferrocast_obs::aitum::vertical_streaming(c).await?;
        let before = self.vertical_before();
        Some(VerticalObs {
            points_here: before.is_some(),
            streaming,
            before: before.and_then(|b| ferrocast_obs::aitum::server_host(&b.server)),
        })
    }

    /// Aitum Vertical's first output from its settings file (server and key, to put back).
    async fn vertical_setting_on_disk(&self) -> Option<(String, String)> {
        let files: Vec<_> =
            self.obs_paths().iter().filter_map(|p| ferrocast_obs::aitum::vertical_config_path(p)).collect();
        tokio::task::spawn_blocking(move || {
            let path = files.into_iter().find(|p| p.is_file())?;
            if std::fs::metadata(&path).ok()?.len() > ferrocast_obs::aitum::MAX_CONFIG_BYTES {
                return None;
            }
            ferrocast_obs::aitum::vertical_first_output(&std::fs::read(path).ok()?)
        })
        .await
        .ok()
        .flatten()
    }

    /// Points OBS's vertical canvas (Aitum Vertical) at Ferrocast's vertical key, keeping its
    /// setting to put back. Never while it streams.
    pub async fn golive_point_vertical(&self) -> Result<Option<VerticalObs>, EngineError> {
        let _one = self.golive().busy.lock().await;
        let (server, key) = self.golive_vertical_ingest().await?;
        let c = self.obs_client().await?;
        match ferrocast_obs::aitum::vertical_streaming(&c).await {
            None => {
                return Err(refused(
                    "aitum_vertical_missing",
                    "OBS doesn't have Aitum Vertical.",
                    "Install Aitum Vertical in OBS (aitum.tv), or use \"Cut it from your stream\".",
                ));
            }
            Some(true) => {
                return Err(refused(
                    "obs_streaming",
                    "OBS's vertical canvas is streaming right now.",
                    "Stop the vertical stream in OBS first; Ferrocast changes this between streams.",
                ));
            }
            Some(false) => {}
        }
        // Its update requests change an output that exists; with none, nothing would stream.
        let (old_server, old_key) = self.vertical_setting_on_disk().await.ok_or_else(|| {
            refused(
                "aitum_vertical_no_output",
                "Aitum Vertical has no stream output yet.",
                "In OBS, open the vertical dock's settings, add a stream output (any server), then try again.",
            )
        })?;
        if self.vertical_before().is_none() {
            // Already pointed here by hand: nothing to put back, but it points here.
            let before = if old_server == server {
                VerticalBefore { server: String::new(), key: String::new() }
            } else {
                VerticalBefore { server: old_server, key: old_key }
            };
            let j = serde_json::to_string(&before).map_err(|e| EngineError::Internal(e.to_string()))?;
            self.inner.secrets.set(VERTICAL_BEFORE_SECRET, &j).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        ferrocast_obs::aitum::set_vertical_stream(&c, &server, &key).await?;
        tracing::info!("Go live: Aitum Vertical now streams to Ferrocast");
        Ok(self.vertical_obs(&c).await)
    }

    /// Puts Aitum Vertical's own setting back exactly.
    pub async fn golive_restore_vertical(&self) -> Result<Option<VerticalObs>, EngineError> {
        let _one = self.golive().busy.lock().await;
        let before = self.vertical_before().filter(|b| !b.server.is_empty()).ok_or_else(|| {
            refused(
                "golive_nothing_saved",
                "There's no earlier Aitum Vertical setting to put back.",
                "Set its stream in OBS, in the vertical dock's settings.",
            )
        })?;
        let c = self.obs_client().await?;
        if ferrocast_obs::aitum::vertical_streaming(&c).await == Some(true) {
            return Err(refused(
                "obs_streaming",
                "OBS's vertical canvas is streaming right now.",
                "Stop the vertical stream in OBS first; Ferrocast changes this between streams.",
            ));
        }
        ferrocast_obs::aitum::set_vertical_stream(&c, &before.server, &before.key).await?;
        let _ = self.inner.secrets.delete(VERTICAL_BEFORE_SECRET);
        tracing::info!("Go live: Aitum Vertical's own setting is back");
        Ok(self.vertical_obs(&c).await)
    }

    /// Aitum Multistream's outputs: the names and live state from OBS, where each goes from
    /// the plugin's settings file (only names and servers are read, never keys).
    async fn aitum_outputs(&self, c: &ferrocast_obs::Client) -> Option<Vec<AitumOutput>> {
        use ferrocast_obs::aitum;
        let profile = c.profiles().current().await.unwrap_or_default();
        let files: Vec<_> = self.obs_paths().iter().filter_map(|p| aitum::config_path(p)).collect();
        let servers = tokio::task::spawn_blocking(move || {
            let path = files.into_iter().find(|p| p.is_file())?;
            if std::fs::metadata(&path).ok()?.len() > aitum::MAX_CONFIG_BYTES {
                return None;
            }
            Some(aitum::servers(&std::fs::read(path).ok()?, &profile))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        let names: Vec<String> = servers.iter().map(|(n, _)| n.clone()).collect();
        let list = aitum::outputs(c, &names).await.ok().flatten()?;
        let host = |n: &str| servers.iter().find(|(x, _)| x == n).and_then(|(_, h)| h.clone());
        Some(
            list.into_iter()
                .map(|(name, live)| AitumOutput { platform: aitum_platform(host(&name).as_deref(), &name), name, live })
                .collect(),
        )
    }

    /// "I use it on purpose": hides the Aitum Multistream setup note.
    pub async fn set_golive_aitum_ok(&self, ok: bool) -> Result<GoLiveConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        cfg.aitum_ok = ok;
        self.save_golive(cfg.clone()).await?;
        Ok(cfg)
    }

    fn send_golive(&self) {
        let ingest = lock(&self.golive().ingest).clone();
        // OBS isn't asked on every tick: the dashboard reads it with GET.
        let obs = ObsView {
            connected: false,
            points_here: false,
            streaming: false,
            before: None,
            aitum: None,
            vertical: None,
        };
        let mut v = self.view_with(ingest, obs);
        v.upload = None;
        self.send_live(LiveFrame::new(
            "golive",
            &serde_json::json!({ "live": v.live, "stats": v.stats, "vertical_live": v.vertical_live,
                                  "vertical_stats": v.vertical_stats, "destinations": v.destinations,
                                  "cut_state": v.cut.state, "ffmpeg": v.cut.ffmpeg }),
        ));
    }

    /// Saves the destinations' order and switches (custom ones keep their name and server).
    /// While live, a newly ticked one starts and an unticked one stops at once.
    pub async fn set_golive_destinations(&self, order: Vec<(String, bool)>) -> Result<GoLiveConfig, EngineError> {
        // Read, change and save under the config lock, so two saves never undo each other.
        let one_at_a_time = self.config_lock().await;
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
        let next = GoLiveConfig { destinations: next, ..self.golive_config() };
        self.save_golive(next.clone()).await?;
        drop(one_at_a_time);
        // Unticking a paused destination ends its pause (ticking it again starts it at once).
        for d in next.destinations.iter().filter(|d| !d.enabled) {
            lock(&self.golive().protector).release(&d.id);
            lock(&self.golive().paused).remove(&d.id);
        }
        self.reconcile().await;
        Ok(next)
    }

    /// Gives destination `id` the Studio's lighter stream (720p, 3,000 kbps) or the full one.
    /// Running, it starts again on the stream it now takes; the Studio's lighter stream follows.
    pub async fn set_golive_light(&self, id: &str, on: bool) -> Result<GoLiveConfig, EngineError> {
        let one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        let d = cfg.destinations.iter_mut().find(|d| d.id == id).ok_or(EngineError::NotFound("destination"))?;
        if on && d.canvas != Canvas::Main {
            return Err(EngineError::Invalid("light: the vertical stream has no lighter copy.".into()));
        }
        if on && d.url.as_deref().and_then(direct_kind).is_some() {
            return Err(EngineError::Invalid(
                "light: SRT, RIST and WebRTC servers get the full stream from the Studio.".into(),
            ));
        }
        if d.light == on {
            return Ok(cfg);
        }
        d.light = on;
        self.save_golive(cfg.clone()).await?;
        drop(one_at_a_time);
        if let Some(r) = lock(&self.golive().running).remove(id) {
            r.stop.send_replace(true);
        }
        self.reconcile().await;
        Ok(cfg)
    }

    /// Adds a custom destination: a name, an `rtmp://` or `rtmps://` server, and its key
    /// (kept in the secret store). It starts ticked.
    pub async fn add_golive_destination(
        &self,
        name: &str,
        url: &str,
        key: &str,
        canvas: Canvas,
    ) -> Result<DestinationConfig, EngineError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 || name.chars().any(char::is_control) {
            return Err(EngineError::Invalid("name: 1 to 40 characters.".into()));
        }
        let direct = direct_kind(url);
        let url = match direct {
            // SRT, RIST and WebRTC go straight from the Studio's main picture.
            Some(_) => {
                let u = url.trim();
                if u.len() > 512
                    || u.chars().any(|c| c.is_control() || c.is_whitespace())
                    || direct_host(u).is_none_or(|h| h.contains('@'))
                {
                    return Err(EngineError::Invalid(
                        "url: a whole address like srt://host:port, rist://host:port or https://host/whip, without a user name.".into(),
                    ));
                }
                if canvas != Canvas::Main {
                    return Err(EngineError::Invalid(
                        "canvas: SRT, RIST and WebRTC send the main picture only.".into(),
                    ));
                }
                u.to_string()
            }
            None => Target::parse(url).map_err(|e| EngineError::Invalid(format!("url: {e}")))?.tc_url,
        };
        let key = key.trim();
        // The key is a stream id, passphrase or token there, and may be left empty.
        if (key.is_empty() && direct.is_none())
            || key.len() > 512
            || key.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(EngineError::Invalid("key: the stream key the server gave you, without spaces.".into()));
        }
        let one_at_a_time = self.config_lock().await;
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
            url: Some(url),
            enabled: true,
            canvas,
            light: false,
        };
        cfg.destinations.push(d.clone());
        self.save_golive(cfg).await?;
        drop(one_at_a_time);
        self.reconcile().await;
        Ok(d)
    }

    /// Removes a custom destination and its key. The three platforms can't be removed, only
    /// switched off.
    pub async fn remove_golive_destination(&self, id: &str) -> Result<(), EngineError> {
        let one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        let Some(i) = cfg.destinations.iter().position(|d| d.id == id) else {
            return Err(EngineError::NotFound("destination"));
        };
        if cfg.destinations[i].kind != Service::Custom {
            return Err(EngineError::Invalid("Twitch, YouTube and Kick can be switched off, not removed.".into()));
        }
        cfg.destinations.remove(i);
        // Its Advanced settings go with it, so a saved set never names a missing destination.
        cfg.advanced.per.remove(id);
        if cfg.advanced.rules.first_to_pause.as_deref() == Some(id) {
            cfg.advanced.rules.first_to_pause = None;
        }
        self.save_golive(cfg).await?;
        drop(one_at_a_time);
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
        let here = now.points_at(&server);
        // Already pointed here with another key (a new key, a reinstall): only the key changes,
        // and the setting kept from before stays the first one.
        let same_key = now.settings.get("key").and_then(|k| k.as_str()) == Some(key.as_str());
        if !here {
            let json = serde_json::to_string(&now).map_err(|e| EngineError::Internal(e.to_string()))?;
            self.inner.secrets.set(OBS_BEFORE_SECRET, &json).map_err(|e| EngineError::Unavailable(e.to_string()))?;
        }
        if !here || !same_key {
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
        // The stream as it is now, else as it was last time, else a 1080p60 plan.
        let last = self.golive().last_kbps.load(std::sync::atomic::Ordering::Relaxed);
        let need_main = [stats.kbps, last].into_iter().find(|k| *k > 0).unwrap_or(DEFAULT_NEED_KBPS);
        let vertical = self.golive().vhub.stats().kbps;
        let planned = if cfg_cut_source(&self.golive_config()) {
            self.golive_config().cut.kbps + 160
        } else {
            DEFAULT_VERTICAL_NEED_KBPS
        };
        let need_vertical = if vertical > 0 { vertical } else { planned };
        let mut rows = Vec::new();
        let mut twitch: Option<(Target, String)> = None;
        for d in &ticked {
            let mut row = UploadRow {
                id: d.id.clone(),
                name: dest_name(d),
                server: None,
                delay_ms: None,
                need_kbps: if d.canvas == Canvas::Vertical { need_vertical } else { need_main },
                problem: None,
            };
            // SRT, RIST and WebRTC servers aren't RTMP: no delay check, but their need counts.
            if let Some(u) = d.url.as_deref().filter(|u| direct_kind(u).is_some()) {
                row.server = direct_host(u);
                rows.push(row);
                continue;
            }
            // YouTube's vertical stream is made when it's first needed, not for a check.
            let resolved = if d.canvas == Canvas::Vertical && d.kind == Service::Youtube {
                match self.connector(Platform::Youtube) {
                    Some(_) => Target::parse(YOUTUBE_INGEST)
                        .map(|target| Destination {
                            id: d.id.clone(),
                            target,
                            key: String::new(),
                            retry: Default::default(),
                            extra_audio: false,
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("Connect YouTube in Settings, Accounts first.".to_string()),
                }
            } else {
                self.resolve(d).await
            };
            match resolved {
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
        let mut verdict = probe::verdict_keeping(upload, &needs, self.golive_config().advanced.keep_free_pct);
        let unreachable: Vec<&str> = rows.iter().filter(|r| r.problem.is_some()).map(|r| r.name.as_str()).collect();
        if !unreachable.is_empty() {
            let names = unreachable.join(", ");
            verdict.message = if needs.is_empty() {
                format!(
                    "Your upload is {}. Ferrocast couldn't reach {names}, so nothing was counted yet.",
                    probe::mbps(upload)
                )
            } else {
                format!("{} {names} couldn't be reached and isn't counted.", verdict.message)
            };
        }
        let check = UploadCheck { at: now().0, measured_by: measured_by.into(), verdict, rows };
        *lock(&self.golive().upload) = Some(check.clone());
        Ok(check)
    }

    /// A destination's server and key: fetched from its platform, or the saved custom one.
    /// The error is a sentence for the streamer.
    async fn resolve(&self, d: &DestinationConfig) -> Result<Destination, String> {
        let (url, key) = match platform_of(d.kind) {
            Some(p) => {
                let c = self.connector(p).ok_or_else(|| format!("Connect {p} in Settings, Accounts first."))?;
                let got = match d.canvas {
                    Canvas::Main => c.ingest().await,
                    Canvas::Vertical => c.vertical_ingest().await,
                };
                let own_server = self.golive_config().advanced.per.get(&d.id).and_then(|a| a.server.clone());
                match got {
                    Ok(i) => (own_server.unwrap_or(i.url), i.key),
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
        Ok(Destination {
            id: d.id.clone(),
            target,
            key,
            retry: self.golive_config().advanced.retry(&d.id),
            // Twitch's VOD track: only Twitch knows what to do with an extra audio track.
            extra_audio: d.kind == Service::Twitch,
        })
    }

    /// Starts forwarders for ticked destinations whose stream (normal or vertical) is live and
    /// stops the rest. When neither stream is live, the show's pauses and reasons are cleared.
    pub(crate) async fn reconcile(&self) {
        let g = self.golive();
        let _one = g.reconciling.lock().await;
        let (main_on, vertical_on, light_on) =
            (g.hub.live().borrow().on, g.vhub.live().borrow().on, g.lhub.live().borrow().on);
        let on = |d: &DestinationConfig| match d.canvas {
            Canvas::Main => main_on || (d.wants_light() && light_on),
            Canvas::Vertical => vertical_on,
        };
        let want: Vec<DestinationConfig> =
            self.golive_config().destinations.into_iter().filter(|d| d.enabled && on(d)).collect();
        // One whose stream changed (the lighter one started or stopped) starts again on the other.
        for d in &want {
            let moved = lock(&g.running).get(&d.id).is_some_and(|r| r.light != self.on_light(d));
            if moved && let Some(r) = lock(&g.running).remove(&d.id) {
                r.stop.send_replace(true);
            }
        }
        let wanted = |id: &String| want.iter().any(|d| &d.id == id);
        let stale: Vec<String> = lock(&g.running).keys().filter(|id| !wanted(id)).cloned().collect();
        for id in stale {
            if let Some(r) = lock(&g.running).remove(&id) {
                r.stop.send_replace(true);
            }
        }
        lock(&g.blocked).retain(|id, _| wanted(id));
        lock(&g.paused).retain(|id, _| wanted(id));
        if !main_on && !vertical_on {
            lock(&g.protector).reset();
        }
        for d in want {
            if lock(&g.running).contains_key(&d.id) || lock(&g.paused).contains_key(&d.id) {
                continue;
            }
            self.start_destination(&d).await;
        }
        self.reconcile_cut().await;
        // The Studio's vertical and lighter streams follow the ticked platforms and the source.
        self.studio_sync_vertical().await;
        self.studio_sync_light().await;
        self.send_golive();
    }

    /// Runs the cutter while the vertical picture is cut from the stream and a vertical
    /// destination is ticked; a changed layout restarts it.
    async fn reconcile_cut(&self) {
        let g = self.golive();
        let cfg = self.golive_config();
        // Only while a vertical destination would send it (a paused one doesn't need the CPU).
        let paused = lock(&g.paused).keys().cloned().collect::<Vec<_>>();
        let wanted = cfg.vertical_source == VerticalSource::Cut
            && cfg.destinations.iter().any(|d| d.enabled && d.canvas == Canvas::Vertical && !paused.contains(&d.id));
        // A cutter whose task ended (it never should) is cleared, so a new one starts.
        let ended = lock(&g.cut).as_ref().is_some_and(|c| c.state.has_changed().is_err());
        if ended {
            tracing::warn!("Go live: the vertical cutter stopped unexpectedly, starting it again");
            lock(&g.cut).take();
        }
        let running_config = lock(&g.cut).as_ref().map(|c| c.config.clone());
        if running_config.is_some()
            && (!wanted || running_config.as_ref() != Some(&cfg.cut))
            && let Some(c) = lock(&g.cut).take()
        {
            c.stop.send_replace(true);
        }
        if !wanted || lock(&g.cut).is_some() {
            return;
        }
        let Some(ffmpeg) = self.ffmpeg_path() else { return };
        let Ok((server, key)) = self.golive_vertical_ingest().await else { return };
        let (stop, stop_rx) = watch::channel(false);
        let (state_tx, state) = watch::channel(CutState::Waiting);
        let encoders = ferrocast_golive::cut::encoders_for(cfg.cut.encoder);
        tokio::spawn(ferrocast_golive::cutter::run(
            ffmpeg,
            g.hub.clone(),
            cfg.cut.clone(),
            encoders,
            format!("{server}/{key}"),
            state_tx,
            stop_rx,
        ));
        *lock(&g.cut) = Some(CutRun { config: cfg.cut, stop, state });
    }

    /// What's on air, to mark the camera and game on (the cut editor): the Studio's own picture
    /// when it runs (it streams through here like OBS does), else OBS's.
    pub async fn golive_picture(&self) -> Result<serde_json::Value, EngineError> {
        if let Some(v) = self.studio_picture().await {
            return Ok(v);
        }
        let c = self.obs_client().await?;
        let (image, width, height) = obs::program_picture(&c, 960).await?;
        Ok(serde_json::json!({ "image": image, "width": width, "height": height }))
    }

    /// The Studio's scene on air as a picture (640 wide), or None while the Studio isn't running.
    async fn studio_picture(&self) -> Option<serde_json::Value> {
        let scenes = self.studio_act(crate::studio_host::StudioAct::Scenes).await.ok()?;
        let scene = scenes.get("current")?.as_str()?.to_string();
        let shot = self.studio_act(crate::studio_host::StudioAct::Thumbnail { scene, width: 640 }).await.ok()?;
        let image = shot.get("image")?.as_str()?.to_string();
        let (width, height) = png_size(&image)?;
        Some(serde_json::json!({ "image": image, "width": width, "height": height }))
    }

    /// Saves Go live's Advanced settings; the protection rule applies at once, the port at
    /// the next start.
    pub async fn set_golive_advanced(&self, advanced: Advanced) -> Result<GoLiveConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        advanced.validate(&cfg.destinations).map_err(EngineError::Invalid)?;
        let rules_changed = cfg.advanced.rules != advanced.rules;
        cfg.advanced = advanced;
        self.save_golive(cfg.clone()).await?;
        // After the save, and only when the rule changed (it restarts the pause doubling).
        if rules_changed {
            lock(&self.golive().protector).set_rules(cfg.advanced.rules.clone());
        }
        Ok(cfg)
    }

    /// Picks where the vertical picture comes from.
    pub async fn set_vertical_source(&self, source: VerticalSource) -> Result<GoLiveConfig, EngineError> {
        // Cutting needs ffmpeg: fetched now if it isn't here (the cut starts once it is).
        if source == VerticalSource::Cut && self.ffmpeg_path().is_none() {
            self.get_ffmpeg().await?;
        }
        let cfg = {
            let _one_at_a_time = self.config_lock().await;
            let mut cfg = self.golive_config();
            cfg.vertical_source = source;
            self.save_golive(cfg.clone()).await?;
            cfg
        };
        self.reconcile().await;
        Ok(cfg)
    }

    /// Saves the cut-from-stream layout; a running cut restarts with it.
    pub async fn set_vertical_cut(&self, cut: CutConfig) -> Result<GoLiveConfig, EngineError> {
        cut.validate().map_err(EngineError::Invalid)?;
        let cfg = {
            let _one_at_a_time = self.config_lock().await;
            let mut cfg = self.golive_config();
            cfg.cut = cut;
            self.save_golive(cfg.clone()).await?;
            cfg
        };
        self.reconcile().await;
        Ok(cfg)
    }

    async fn start_destination(&self, d: &DestinationConfig) {
        if let Some(url) = d.url.as_deref().filter(|u| direct_kind(u).is_some()) {
            return self.start_direct(d, url.to_string()).await;
        }
        let g = self.golive();
        match self.resolve(d).await {
            Ok(dest) => {
                lock(&g.blocked).remove(&d.id);
                let (health_tx, health) =
                    watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
                let (stop, stop_rx) = watch::channel(false);
                tokio::spawn(ferrocast_golive::push::run(dest, self.hub_of(d).clone(), health_tx, stop_rx));
                lock(&g.running).insert(d.id.clone(), Running { stop, health, light: self.on_light(d) });
            }
            Err(reason) => {
                tracing::info!(dest = %d.id, %reason, "Go live: a destination can't start");
                lock(&g.blocked).insert(d.id.clone(), reason);
            }
        }
    }

    /// Sends the Studio's stream straight to an SRT, RIST or WebRTC server, and reads its
    /// health every second until it's stopped.
    async fn start_direct(&self, d: &DestinationConfig, url: String) {
        let g = self.golive();
        let key = self.inner.secrets.get(&format!("{CUSTOM_KEY_PREFIX}{}", d.id)).ok().flatten().unwrap_or_default();
        let started = match self.studio_host() {
            Ok(host) => host.direct_start(d.id.clone(), url, key).await.map(|()| host),
            Err(_) => Err(String::new()),
        };
        let host = match started {
            Ok(h) => h,
            Err(why) => {
                let reason = if why.is_empty() || why.contains("isn't streaming") {
                    "SRT, RIST and WebRTC go straight from the Ferrocast Studio: go live with the Ferrocast picture."
                        .to_string()
                } else {
                    format!("Couldn't start: {why}")
                };
                tracing::info!(dest = %d.id, %reason, "Go live: a direct destination can't start");
                lock(&g.blocked).insert(d.id.clone(), reason);
                return;
            }
        };
        lock(&g.blocked).remove(&d.id);
        let now_ms = || u64::try_from(now().0).unwrap_or(0);
        let (health_tx, health) =
            watch::channel(Health { state: PushState::Connecting, kbps: 0, dropped: 0, since_ms: now_ms() });
        let (stop, mut stop_rx) = watch::channel(false);
        let id = d.id.clone();
        tokio::spawn(async move {
            let mut last_bytes = 0u64;
            let mut tick = tokio::time::interval(DIRECT_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stop_rx.changed() => break,
                }
                if *stop_rx.borrow() {
                    break;
                }
                let st = host.direct_stats(id.clone()).await.unwrap_or(serde_json::Value::Null);
                if st.is_null() {
                    // The Studio stopped streaming: Go live stops this one too.
                    break;
                }
                let bytes = st["bytes"].as_u64().unwrap_or(0);
                let kbps = u32::try_from(bytes.saturating_sub(last_bytes) * 8 / 1000).unwrap_or(u32::MAX);
                last_bytes = bytes;
                let state = if st["reconnecting"].as_bool().unwrap_or(false) {
                    PushState::Reconnecting {
                        attempt: 0,
                        retry_in_ms: 0,
                        reason: "The server dropped the connection; trying again.".into(),
                    }
                } else if st["active"].as_bool().unwrap_or(false) {
                    PushState::Live
                } else {
                    PushState::Connecting
                };
                health_tx.send_modify(|h| {
                    if std::mem::discriminant(&h.state) != std::mem::discriminant(&state) {
                        h.since_ms = now_ms();
                    }
                    h.state = state;
                    h.kbps = kbps;
                    h.dropped = st["dropped"].as_u64().unwrap_or(0);
                });
            }
            let _ = host.direct_stop(id).await;
        });
        lock(&g.running).insert(d.id.clone(), Running { stop, health, light: false });
    }

    /// Turns upload protection on or off. Off brings every paused destination back.
    pub async fn set_golive_protect(&self, on: bool) -> Result<GoLiveConfig, EngineError> {
        let cfg = {
            let _one_at_a_time = self.config_lock().await;
            let mut cfg = self.golive_config();
            cfg.protect = on;
            self.save_golive(cfg.clone()).await?;
            cfg
        };
        if !on {
            let ids: Vec<String> = lock(&self.golive().paused).keys().cloned().collect();
            for id in ids {
                self.unpause(&id).await;
            }
            // Switched on again later, it starts from a clean slate.
            lock(&self.golive().protector).reset();
        }
        Ok(cfg)
    }

    /// "Resume now" on a paused destination: it starts again at once.
    pub async fn golive_resume(&self, id: &str) -> Result<(), EngineError> {
        if !lock(&self.golive().paused).contains_key(id) {
            return Err(refused("golive_not_paused", "That platform isn't paused.", "Nothing to resume."));
        }
        self.unpause(id).await;
        Ok(())
    }

    async fn unpause(&self, id: &str) {
        let g = self.golive();
        lock(&g.protector).release(id);
        lock(&g.paused).remove(id);
        self.reconcile().await;
    }

    /// Stops a destination the protector just paused and shows why on its health line.
    pub(crate) fn pause(&self, id: String, top: String) {
        let g = self.golive();
        if let Some(r) = lock(&g.running).remove(&id) {
            r.stop.send_replace(true);
        }
        let now_ms = u64::try_from(now().0).unwrap_or(0);
        let left = lock(&g.protector)
            .paused_until(&id)
            .map(|u| u.saturating_duration_since(std::time::Instant::now()).as_millis() as u64)
            .unwrap_or(0);
        tracing::info!(dest = %id, "Go live: upload protection paused a destination");
        let reason = format!("Your upload dipped, so this paused to keep {top} smooth. It comes back by itself.");
        let health = Health {
            state: PushState::Paused { until_ms: now_ms + left, reason },
            kbps: 0,
            dropped: 0,
            since_ms: now_ms,
        };
        lock(&g.paused).insert(id, health);
        self.send_golive();
    }

    /// One second of upload protection while live.
    async fn protect_tick(&self) {
        use ferrocast_golive::protect::{Action, Sample};
        let g = self.golive();
        let cfg = self.golive_config();
        if !cfg.protect {
            return;
        }
        let now_ms = u64::try_from(now().0).unwrap_or(0);
        let healths: Vec<(String, Health)> =
            lock(&g.running).iter().map(|(id, r)| (id.clone(), r.health.borrow().clone())).collect();
        let canvas_of = |id: &str| cfg.destinations.iter().find(|d| d.id == id).map(|d| d.canvas).unwrap_or_default();
        let (main_kbps, vertical_kbps) = (g.hub.stats().kbps, g.vhub.stats().kbps);
        let samples: Vec<Sample<'_>> = healths
            .iter()
            .filter(|(_, h)| h.state == PushState::Live)
            .map(|(id, h)| Sample {
                id,
                kbps: h.kbps,
                want_kbps: match canvas_of(id) {
                    Canvas::Main => main_kbps,
                    Canvas::Vertical => vertical_kbps,
                },
                dropped: h.dropped,
                live_ms: now_ms.saturating_sub(h.since_ms),
            })
            .collect();
        // The vertical stream comes after every normal destination: it pauses first.
        let order: Vec<&str> = [Canvas::Main, Canvas::Vertical]
            .into_iter()
            .flat_map(|c| cfg.destinations.iter().filter(move |d| d.enabled && d.canvas == c))
            .map(|d| d.id.as_str())
            .collect();
        let action = lock(&g.protector).tick(std::time::Instant::now(), &samples, &order);
        match action {
            Some(Action::Pause(id)) => {
                // The top destination still sending is the one the pause keeps smooth.
                let top = order
                    .iter()
                    .filter(|o| **o != id && samples.iter().any(|s| s.id == **o))
                    .find_map(|o| cfg.destinations.iter().find(|d| d.id == *o))
                    .map(dest_name)
                    .unwrap_or_default();
                self.pause(id, top);
                self.reconcile_cut().await;
            }
            Some(Action::Resume(id)) => {
                tracing::info!(dest = %id, "Go live: upload protection brought a destination back");
                lock(&g.paused).remove(&id);
                self.reconcile().await;
            }
            None => {}
        }
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

/// Starts and stops the destinations as OBS's streams (normal and vertical) start and stop;
/// while either is live, sends the health lines and runs upload protection every second.
pub(crate) async fn run(engine: Engine) {
    let g = engine.golive();
    let (mut main, mut vertical, mut light) = (g.hub.live(), g.vhub.live(), g.lhub.live());
    let mut was = (false, false);
    loop {
        let any = was.0 || was.1;
        // The watch guards aren't Send, so values are read after the select.
        let changed = tokio::select! {
            r = main.changed() => { if r.is_err() { return; } true }
            r = vertical.changed() => { if r.is_err() { return; } true }
            r = light.changed() => { if r.is_err() { return; } true }
            _ = tokio::time::sleep(HEALTH_EVERY), if any => false,
        };
        if changed {
            let now = (main.borrow_and_update().on, vertical.borrow_and_update().on);
            if now.0 != was.0 {
                tracing::info!(live = now.0, "Go live: the stream changed");
            }
            if now.1 != was.1 {
                tracing::info!(live = now.1, "Go live: the vertical stream changed");
            }
            was = now;
            engine.reconcile().await;
            continue;
        }
        let k = g.hub.stats().kbps;
        if k > 0 {
            g.last_kbps.store(k, std::sync::atomic::Ordering::Relaxed);
        }
        engine.protect_tick().await;
        engine.send_golive();
    }
}

/// A PNG data URL's size, from its header (IHDR: width and height after the 16th byte).
fn png_size(data_url: &str) -> Option<(u32, u32)> {
    use base64::Engine as _;
    let b64 = data_url.strip_prefix("data:image/png;base64,")?;
    // The first 32 base64 characters hold the first 24 bytes.
    let head = base64::engine::general_purpose::STANDARD.decode(b64.get(..32)?).ok()?;
    let be = |i: usize| head.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    Some((be(16)?, be(20)?))
}

#[cfg(test)]
mod picture_tests {
    #[test]
    fn png_size_reads_the_header() {
        // A 256 × 144 PNG's first bytes (signature, IHDR length and type, width, height).
        use base64::Engine as _;
        let mut b = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        b.extend(256u32.to_be_bytes());
        b.extend(144u32.to_be_bytes());
        let url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&b));
        assert_eq!(super::png_size(&url), Some((256, 144)));
        assert_eq!(super::png_size("data:image/jpeg;base64,AAAA"), None);
    }
}
