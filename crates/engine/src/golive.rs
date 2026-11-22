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
pub use ferrocast_golive::probe::{CappedInfo, PlatformLimit, RuleCode, RuleNotice};
use ferrocast_golive::{Destination, Health, Hub, Ingest, PushState, StreamStats, Target, Verdict};
use ferrocast_obs::control::{self as obs, StreamService};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::frames::LiveFrame;
use crate::studio::save_setting;
use crate::{Engine, EngineError, now};

pub(crate) const CONFIG_KEY: &str = "golive";
pub(crate) const INGEST_KEY_SECRET: &str = "golive_ingest_key";
pub(crate) const OBS_BEFORE_SECRET: &str = "golive_obs_before";
/// Aitum Vertical's first output before Ferrocast pointed it here: `{ "server", "key" }`.
pub(crate) const VERTICAL_BEFORE_SECRET: &str = "golive_vertical_before";
pub(crate) const CUSTOM_KEY_PREFIX: &str = "golive_dest_";
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
    /// Twitch Enhanced Broadcasting (docs/plan/TWITCH-EB.md): the Studio encodes the qualities
    /// Twitch asks for and sends them all to Twitch in one stream. Twitch, main picture only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enhanced: bool,
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
    if u.starts_with("srt://") || u.starts_with("srtla://") {
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
    /// Auto quality: the Studio's stream lowers its bitrate when it can't get out, and goes
    /// back up when it can. On unless the streamer turns it off.
    #[serde(default = "on")]
    pub auto_quality: bool,
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
    /// Power-user override matching OBS `IgnoreRecommended` (C-6): disables automatic Twitch/Kick
    /// Studio encoding caps so the user's configured bitrate/codec go out untouched.
    pub ignore_service_limits: bool,
    /// Which IP version the RTMP destinations connect over (`auto`, `v4`, `v6`): `v4` for a server
    /// whose IPv6 route is broken.
    pub ip_family: ferrocast_golive::IpFamily,
    /// Send over this connection only (its name or address from `GET /golive/networks`); none:
    /// the system's way, moving to another connection when it fails. A pinned one never moves.
    pub bind: Option<String>,
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            rules: Default::default(),
            keep_free_pct: 20,
            port: ferrocast_golive::ingest::DEFAULT_PORT,
            per: HashMap::new(),
            ignore_service_limits: false,
            ip_family: Default::default(),
            bind: None,
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
    /// Another destination that sends instead while this one can't (an SRT server's RTMP
    /// twin): it waits on standby, starts after `BACKUP_AFTER` of failing, and stops once this
    /// one is back for as long.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// A WebRTC (WHIP) server: the Studio's lighter stream goes beside the full one as a second
    /// layer (simulcast), for a server that gives each viewer the one that fits.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub simulcast: bool,
}

impl Default for DestAdvanced {
    fn default() -> Self {
        Self { server: None, first_retry_s: 1, max_retry_s: 30, backup: None, simulcast: false }
    }
}

/// How long a destination fails before its backup sends, and is back before the backup stops.
const BACKUP_AFTER: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });

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
        if let Some(b) = &self.bind
            && (b.trim().is_empty() || b.chars().count() > 64 || b.chars().any(char::is_control))
        {
            return Err("bind: a connection's name or address (up to 64 characters), or none.".into());
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
            if a.simulcast && d.url.as_deref().and_then(direct_kind) != Some("whip") {
                return Err(format!("per.{id}.simulcast: only a WebRTC (WHIP) server takes simulcast."));
            }
            if let Some(b) = &a.backup {
                let backup = destinations
                    .iter()
                    .find(|x| &x.id == b)
                    .ok_or_else(|| format!("per.{id}.backup: no such destination."))?;
                if b == id {
                    return Err(format!("per.{id}.backup: a destination can't back itself up."));
                }
                if backup.canvas != d.canvas {
                    return Err(format!("per.{id}.backup: the backup sends the same picture (main or vertical)."));
                }
                if self.per.get(b).is_some_and(|x| x.backup.is_some()) {
                    return Err(format!("per.{id}.backup: that one has a backup of its own; pick another."));
                }
                if self.per.iter().any(|(other, x)| other != id && x.backup.as_ref() == Some(b)) {
                    return Err(format!("per.{id}.backup: that one already backs up another destination."));
                }
            }
        }
        Ok(())
    }

    fn retry(&self, id: &str) -> ferrocast_golive::push::Retry {
        let a = self.per.get(id).cloned().unwrap_or_default();
        ferrocast_golive::push::Retry {
            first: Duration::from_secs(u64::from(a.first_retry_s)),
            max: Duration::from_secs(u64::from(a.max_retry_s)),
            ..ferrocast_golive::push::Retry::default()
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
            enhanced: false,
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
            auto_quality: true,
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
    /// Auto quality is on.
    pub auto_quality: bool,
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
    /// Its rules broken by the stream OBS is sending (kept in sync with `issues` for older clients).
    pub rules: Vec<String>,
    /// Structured rules broken by the active stream (Contract C-2).
    #[serde(default)]
    pub issues: Vec<RuleNotice>,
    /// Calm cap badge shown when Studio fits its shared encode to Twitch/Kick limits (Contract C-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capped: Option<CappedInfo>,
    /// Sent straight from the Ferrocast Studio by this protocol ("srt", "rist", "whip");
    /// absent for RTMP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct: Option<&'static str>,
    /// Takes the Studio's lighter stream (left out when not).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub light: bool,
    /// Twitch Enhanced Broadcasting is on for it (left out when not).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub enhanced: bool,
    /// On the lighter stream for now: its route fell behind while the others kept up. It tries
    /// the full stream again by itself.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lighter_for_route: bool,
    /// A backup sending now, instead of this destination (its name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_for: Option<String>,
    /// A backup on standby: it sends only while this destination (its name) can't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standby_for: Option<String>,
    /// Which stream key a platform destination sent to the last time it started ("Ferrocast ·
    /// variable"), for its details; never the key itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_note: Option<String>,
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
    pub(crate) blocked: Mutex<HashMap<String, String>>,
    upload: Mutex<Option<UploadCheck>>,
    /// Destinations turned on or off during this stream (for the session record).
    pub(crate) dest_log: Mutex<Vec<ferrocast_core::diagnostics::DestinationEvent>>,
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
    /// Which stream key each platform destination was last started with (see `DestinationView`).
    key_notes: Mutex<HashMap<String, String>>,
    /// Destinations that gave up (`PushState::Failed`) in this or the just-ended stream, kept
    /// visible until the next stream starts or the streamer unticks/retries them.
    failed: Mutex<HashMap<String, Health>>,
    /// Destinations for which a terminal `live_alert` has already been sent in this stream.
    failed_alerted: Mutex<std::collections::HashSet<String>>,
    /// The cutter for the vertical picture, while it runs (A2.4).
    cut: Mutex<Option<CutRun>>,
    /// The engine's data folder (where fetched ffmpeg lives).
    data_dir: Mutex<Option<std::path::PathBuf>>,
    /// Stops the local server (for a new key) and its task, to wait for the port to free.
    ingest_task: Mutex<Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)>>,
    /// Fetching ffmpeg: progress or why it failed (`None`: not fetching).
    ffmpeg_fetch: Mutex<Option<crate::ffmpeg_get::FfmpegState>>,
    /// Backups: since when each destination with one has been failing (or back), and those
    /// failing long enough that their backup sends.
    backup: Mutex<BackupState>,
    /// Quality per destination: the ones moved to the lighter stream because their own route
    /// couldn't keep up while the others could.
    quality: Mutex<ferrocast_golive::quality::Quality>,
    /// Pre-flight's last Test connection per destination, and when.
    tested: Mutex<HashMap<String, (std::time::Instant, ferrocast_golive::reach::Reach)>>,
    /// Twitch Enhanced Broadcasting's plan for the stream on now (None: a normal stream).
    pub(crate) enhanced: Mutex<Option<ferrocast_golive::twitch_eb::Plan>>,
    /// Cached Studio encode settings so `view_with` can build the C-2 cap badge synchronously.
    studio_video: Mutex<Option<StudioCapConfig>>,
    /// Whether Ferrocast Studio itself started the live main stream (vs external OBS ingest).
    studio_live: std::sync::atomic::AtomicBool,
    /// The Studio couldn't put the platform's cap on the stream on air (its encoder is shared with
    /// a recording, say): the stream isn't held to it, so no badge says it is and no warning about
    /// its bitrate is held back.
    cap_failed: std::sync::atomic::AtomicBool,
    /// Set while a destination change is applied to a running stream: the Studio's encoder is
    /// shared by every other output (and a "Same as stream" recording), so its cap isn't touched.
    cap_frozen: std::sync::atomic::AtomicBool,
    /// A platform said the stream ended while the hub still runs (every destination was
    /// switched off, the stream and recording go on): the session record closes with the hub.
    health_end_pending: std::sync::atomic::AtomicBool,
}

/// The last title and category pushed count for this stream only when they were pushed lately
/// and some platform took them; a push from weeks ago (or one that failed everywhere) would put
/// an old title on a platform the streamer set up by hand today.
pub(crate) fn recent_push(h: &ferrocast_core::stream_info::HistoryEntry, now_ms: i64) -> bool {
    const HOURS_12_MS: i64 = 12 * 3_600_000;
    now_ms.saturating_sub(h.at) < HOURS_12_MS && h.results.iter().any(|r| r.ok)
}

/// What the sound and the packaging add to a stream's measured rate over its video (the rules' own slack).
const AUDIO_SLACK_KBPS: u32 = 400;
/// How far a measured keyframe gap may run over a platform's before it counts as too long.
const KEYFRAME_SLACK_MS: u32 = 500;

/// Answers to the questions a mid-stream change asks first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DestinationConfirm {
    /// Add it although the last upload check says it won't fit.
    pub upload: bool,
    /// Stop sending to the last destination.
    pub last: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StudioCapConfig {
    video_kbps: u32,
    is_hevc: bool,
    keyframe_s: u32,
    vbr: bool,
    bframes: bool,
}

impl From<&crate::studio_host::StudioSettings> for StudioCapConfig {
    fn from(s: &crate::studio_host::StudioSettings) -> Self {
        Self {
            video_kbps: s.video_kbps,
            is_hevc: s.video_encoder.as_deref().is_some_and(|e| e.to_ascii_lowercase().contains("hevc")),
            keyframe_s: s.keyframe_s,
            vbr: s.vbr,
            bframes: s.bframes,
        }
    }
}

#[derive(Debug, Default)]
struct BackupState {
    /// When a destination with a backup last changed between sending and not.
    since: HashMap<String, (bool, std::time::Instant)>,
    /// Destinations whose backup is sending for them.
    failing: std::collections::HashSet<String>,
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
            dest_log: Mutex::new(Vec::new()),
            last_kbps: std::sync::atomic::AtomicU32::new(0),
            busy: tokio::sync::Mutex::new(()),
            reconciling: tokio::sync::Mutex::new(()),
            protector: Mutex::new(Default::default()),
            paused: Mutex::new(HashMap::new()),
            key_notes: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            failed_alerted: Mutex::new(std::collections::HashSet::new()),
            cut: Mutex::new(None),
            data_dir: Mutex::new(None),
            ingest_task: Mutex::new(None),
            ffmpeg_fetch: Mutex::new(None),
            backup: Mutex::new(BackupState::default()),
            quality: Mutex::new(Default::default()),
            tested: Mutex::new(HashMap::new()),
            enhanced: Mutex::new(None),
            studio_video: Mutex::new(None),
            studio_live: std::sync::atomic::AtomicBool::new(false),
            cap_failed: std::sync::atomic::AtomicBool::new(false),
            cap_frozen: std::sync::atomic::AtomicBool::new(false),
            health_end_pending: std::sync::atomic::AtomicBool::new(false),
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

/// Twitch Enhanced Broadcasting's plan for the Studio stream on now (None: none).
pub(crate) fn lock_enhanced(e: &Engine) -> std::sync::MutexGuard<'_, Option<ferrocast_golive::twitch_eb::Plan>> {
    lock(&e.golive().enhanced)
}

/// The destination `id` backs up, if any.
fn backs_up<'a>(cfg: &'a GoLiveConfig, id: &str) -> Option<&'a DestinationConfig> {
    let (primary, _) = cfg.advanced.per.iter().find(|(_, a)| a.backup.as_deref() == Some(id))?;
    cfg.destinations.iter().find(|d| &d.id == primary)
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
    pub(crate) fn golive(&self) -> &GoLive {
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
                "Stop streaming first; the key changes between streams.",
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

    /// Tests: pretend the local stream server runs, so a stream can "start".
    #[cfg(test)]
    pub(crate) fn set_test_ingest(&self) {
        *lock(&self.golive().ingest) = Some("rtmp://127.0.0.1:1935/live".into());
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
        self.wants_light_now(d) && self.golive().lhub.live().borrow().on
    }

    /// Takes the lighter stream: set so, or moved there for a slow route (RTMP only: SRT,
    /// RIST and WebRTC go from the Studio's own stream).
    pub(crate) fn wants_light_now(&self, d: &DestinationConfig) -> bool {
        d.wants_light()
            || (d.canvas == Canvas::Main
                && d.url.as_deref().and_then(direct_kind).is_none()
                && lock(&self.golive().quality).is_light(&d.id))
    }

    /// A WebRTC server set to simulcast (the lighter stream as its second layer).
    pub(crate) fn simulcast(&self, d: &DestinationConfig) -> bool {
        d.canvas == Canvas::Main
            && d.url.as_deref().and_then(direct_kind) == Some("whip")
            && self.golive_config().advanced.per.get(&d.id).is_some_and(|a| a.simulcast)
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

    /// Test helpers: a measured upload to judge a change against, and a destination that gave up.
    #[cfg(test)]
    pub(crate) fn seed_upload_for_test(&self, upload_kbps: u32) {
        let verdict = probe::verdict_keeping(upload_kbps, &[], 20);
        *lock(&self.golive().upload) =
            Some(UploadCheck { at: now().0, measured_by: "twitch".into(), rows: Vec::new(), verdict });
    }

    #[cfg(test)]
    pub(crate) fn upload_now_for_test(&self, cfg: &GoLiveConfig) -> Option<UploadCheck> {
        self.upload_now(cfg)
    }

    #[cfg(test)]
    pub(crate) fn seed_failed_for_test(&self, id: &str) {
        let h = Health {
            state: PushState::Failed { attempts: 3, reason: "refused".into(), detail: None, refused: false },
            kbps: 0,
            dropped: 0,
            sent: 0,
            link: None,
            via: None,
            since_ms: 0,
        };
        lock(&self.golive().failed).insert(id.to_string(), h);
        if let Some(r) = lock(&self.golive().running).remove(id) {
            r.stop.send_replace(true);
        }
    }

    /// Whether a stream is running that a destination change would be applied to.
    pub(crate) fn health_end_deferred(&self) {
        self.golive().health_end_pending.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn golive_stream_running(&self) -> bool {
        self.golive().hub.live().borrow().on || self.golive().vhub.live().borrow().on
    }

    /// `reconcile`, for a change made while a stream runs: starts and stops only the outputs
    /// that changed and leaves the Studio's encoder (its cap and shape) alone.
    pub(crate) async fn reconcile_midstream(&self) {
        let g = self.golive();
        if !self.golive_stream_running() {
            return self.reconcile().await;
        }
        g.cap_frozen.store(true, std::sync::atomic::Ordering::Release);
        self.reconcile().await;
        g.cap_frozen.store(false, std::sync::atomic::Ordering::Release);
    }

    /// The destinations sending right now (ids).
    pub fn golive_running(&self) -> std::collections::BTreeSet<String> {
        lock(&self.golive().running).keys().cloned().collect()
    }

    pub(crate) fn note_studio_settings(&self, s: &crate::studio_host::StudioSettings) {
        *lock(&self.golive().studio_video) = Some(StudioCapConfig::from(s));
    }

    pub(crate) fn set_studio_live(&self, on: bool) {
        self.golive().studio_live.store(on, std::sync::atomic::Ordering::Release);
    }

    /// Active platform ceiling for Studio's shared main stream (Twitch 6,000 kbps / Kick
    /// 8,000 kbps H.264 2 s), or `None` when disabled by `ignore_service_limits`, Enhanced
    /// Broadcasting, manual light routing, or when only uncapped destinations are enabled.
    pub fn studio_limit(&self) -> Option<PlatformLimit> {
        let cfg = self.golive_config();
        if cfg.advanced.ignore_service_limits {
            return None;
        }
        // One that can't start (no account, no key) isn't on this stream, so it sets no limit: a
        // YouTube stream was held to 6,000 kbps by a Twitch switch with nobody signed in.
        let blocked = lock(&self.golive().blocked).clone();
        probe::effective_limit(
            cfg.destinations
                .iter()
                .filter(|d| {
                    d.enabled
                        && d.canvas == Canvas::Main
                        && d.url.as_deref().and_then(direct_kind).is_none()
                        && !self.wants_light_now(d)
                        && !blocked.contains_key(&d.id)
                })
                .map(|d| (d.kind, d.enhanced)),
        )
    }

    /// Whether any enabled (or standby-backup for an enabled) main WebRTC (WHIP) destination
    /// needs B-frames disabled (WebRTC cannot carry B-frames regardless of `ignore_service_limits`).
    pub(crate) fn studio_no_bframes(&self) -> bool {
        let cfg = self.golive_config();
        let blocked = lock(&self.golive().blocked);
        cfg.destinations.iter().any(|d| {
            d.canvas == Canvas::Main
                && d.url.as_deref().and_then(direct_kind) == Some("whip")
                && !blocked.contains_key(&d.id)
                && (d.enabled || backs_up(&cfg, &d.id).is_some_and(|p| p.enabled))
        })
    }

    pub(crate) async fn sync_stream_cap(&self) {
        if self.golive().cap_frozen.load(std::sync::atomic::Ordering::Acquire) {
            // A destination was added or removed on a running stream: its encoder stays as it is.
            self.send_golive_pub();
            return;
        }
        let Ok(host) = self.studio_host() else { return };
        let limit = self.studio_limit();
        let no_bframes = self.studio_no_bframes();
        let cap = (limit.is_some() || no_bframes).then(|| crate::studio_host::StreamCap {
            max_kbps: limit.map(|l| l.max_kbps),
            h264_only: limit.is_some_and(|l| l.h264_only),
            keyframe_secs: limit.map_or(0, |l| l.keyframe_secs),
            no_bframes,
        });
        let failed = &self.golive().cap_failed;
        match host.set_stream_cap(cap).await {
            Ok(()) => failed.store(false, std::sync::atomic::Ordering::Release),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't update the Studio's stream cap");
                // The stream on air isn't held to the platform's limit: said, and the view stops
                // claiming it is (the badge, the held-back bitrate warning).
                failed.store(true, std::sync::atomic::Ordering::Release);
                let what = match cap {
                    Some(c) if c.max_kbps.is_some() => format!("{} kbps", c.max_kbps.unwrap_or_default()),
                    Some(c) if c.no_bframes => "no B-frames for WebRTC".to_string(),
                    _ => "the platform".to_string(),
                };
                self.notice_detail(
                    "stream-cap",
                    crate::frames::NoticeLevel::Warn,
                    "This stream isn't held to its platform's limit",
                    &format!("Ferrocast couldn't set {what} on the stream on air: {e}"),
                );
            }
        }
        self.send_golive_pub();
    }

    /// The Go live tab: destinations with their health, OBS's setting, the stream's numbers.
    pub async fn golive_view(&self) -> GoLiveView {
        if lock(&self.golive().studio_video).is_none()
            && let Ok(host) = self.studio_host()
        {
            self.note_studio_settings(&host.settings().await);
        }
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
        // Before `blocked` is held: the limit reads it too.
        let eff_limit = self.studio_limit();
        let failed = lock(&g.failed);
        let blocked = lock(&g.blocked);
        let st = self.state();
        let failing = lock(&g.backup).failing.clone();
        let studio_cfg = *lock(&g.studio_video);
        let studio_streaming = live && g.studio_live.load(std::sync::atomic::Ordering::Acquire);
        let cap_held = !g.cap_failed.load(std::sync::atomic::Ordering::Acquire);
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
                let dest_stats = self.hub_of(d).stats();
                let mut issues =
                    if d.enabled { probe::rules_structured(&dest_stats, d.kind, d.enhanced) } else { Vec::new() };
                let is_whip = d.url.as_deref().and_then(direct_kind) == Some("whip");
                let capped = if is_whip
                    && d.enabled
                    && d.canvas == Canvas::Main
                    && cap_held
                    && (!live || studio_streaming)
                    && let Some(sv) = studio_cfg
                    && sv.bframes
                {
                    let kbps = eff_limit.map_or(sv.video_kbps, |l| sv.video_kbps.min(l.max_kbps));
                    Some(probe::CappedInfo {
                        kbps,
                        resolution: None,
                        separate_encode: false,
                        summary: "B-frames off for WebRTC".into(),
                        detail: "B-frames are turned off for this stream because WebRTC (WHIP) can't carry them."
                            .into(),
                    })
                } else if d.enabled
                    && d.canvas == Canvas::Main
                    && d.url.as_deref().and_then(direct_kind).is_none()
                    && !d.enhanced
                    && !d.wants_light()
                    && cap_held
                    && (!live || studio_streaming)
                    && let Some(limit) = eff_limit
                    && probe::ceiling(d.kind, d.enhanced).is_some()
                    && let Some(sv) = studio_cfg
                {
                    probe::capped_info(limit, sv.video_kbps, sv.is_hevc, sv.keyframe_s, sv.vbr)
                } else {
                    None
                };
                if capped.is_some() && studio_streaming && !self.on_light(d) {
                    // Studio is holding the shared encode at the platform ceiling with 1x VBV /
                    // DataRateLimits; hold back the bitrate warning (the measured rate counts the
                    // audio and the container). The codec and keyframe rules still speak: an
                    // encoder that isn't H.264 (AV1) must not be hidden by the badge.
                    issues.retain(|i| i.code != probe::RuleCode::Bitrate);
                }
                let rules = issues.iter().map(|i| i.detail.clone()).collect();
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
                        .or_else(|| paused.get(&d.id).cloned())
                        .or_else(|| failed.get(&d.id).cloned()),
                    blocked: blocked.get(&d.id).cloned(),
                    rules,
                    issues,
                    capped,
                    direct: d.url.as_deref().and_then(direct_kind),
                    light: d.wants_light(),
                    enhanced: d.enhanced,
                    lighter_for_route: !d.wants_light() && self.on_light(d),
                    backup_for: backs_up(&cfg, &d.id).filter(|p| failing.contains(&p.id)).map(dest_name),
                    standby_for: backs_up(&cfg, &d.id).filter(|p| p.enabled && !failing.contains(&p.id)).map(dest_name),
                    key_note: lock(&self.golive().key_notes).get(&d.id).cloned(),
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
            upload: if live || vertical_live {
                self.upload_now(&cfg).or_else(|| lock(&g.upload).clone())
            } else {
                lock(&g.upload).clone()
            },
            protect: cfg.protect,
            aitum_ok: cfg.aitum_ok,
            advanced: cfg.advanced.clone(),
            auto_quality: cfg.auto_quality,
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

    /// Auto quality on or off.
    pub async fn set_golive_auto_quality(&self, on: bool) -> Result<GoLiveConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        cfg.auto_quality = on;
        self.save_golive(cfg.clone()).await?;
        if !on {
            self.auto_quality_off().await;
        }
        Ok(cfg)
    }

    /// Trouble getting the stream out: packets the platforms' senders skipped (all told) and
    /// whether upload protection has one paused.
    pub(crate) fn golive_trouble(&self) -> (u64, bool) {
        let g = self.golive();
        let dropped = lock(&g.running).values().map(|r| r.health.borrow().dropped).sum();
        (dropped, !lock(&g.paused).is_empty())
    }

    /// Each platform Go live runs: whether it is live, what it skipped and for how long (auto quality
    /// judges the upload by them together).
    pub(crate) fn golive_senders(&self) -> Vec<ferrocast_core::auto_quality::Dest> {
        let now_ms = u64::try_from(now().0).unwrap_or(0);
        lock(&self.golive().running)
            .iter()
            .map(|(id, r)| {
                let h = r.health.borrow();
                let live = h.state == PushState::Live;
                ferrocast_core::auto_quality::Dest {
                    id: id.clone(),
                    dropped: h.dropped,
                    live_ms: if live { now_ms.saturating_sub(h.since_ms) } else { 0 },
                    live,
                }
            })
            .collect()
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
        self.set_golive_destinations_checked(order, DestinationConfirm { upload: true, last: true }).await
    }

    /// Orders and ticks the destinations. While a stream runs, a newly ticked destination
    /// starts only its own output and an unticked one stops only its own; the others, the
    /// recording and the replay buffer are never touched. A destination the running encode
    /// can't serve is refused (422), one that would not fit the measured upload, and removing
    /// the last one, need `confirm` first (409).
    pub async fn set_golive_destinations_checked(
        &self,
        order: Vec<(String, bool)>,
        confirm: DestinationConfirm,
    ) -> Result<GoLiveConfig, EngineError> {
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
        let added: Vec<DestinationConfig> = next
            .iter()
            .filter(|n| n.enabled && cfg.destinations.iter().any(|o| o.id == n.id && !o.enabled))
            .cloned()
            .collect();
        let live = self.golive().hub.live().borrow().on || self.golive().vhub.live().borrow().on;
        if live {
            self.check_midstream(&cfg, &next, &added, confirm)?;
        }
        let next = GoLiveConfig { destinations: next, ..self.golive_config() };
        self.save_golive(next.clone()).await?;
        // Unticking a paused or failed destination clears it (ticking it again starts fresh):
        // under the config lock, so a second toggle can't have its fresh state cleared by this one.
        for d in next.destinations.iter().filter(|d| !d.enabled) {
            lock(&self.golive().protector).release(&d.id);
            lock(&self.golive().paused).remove(&d.id);
            lock(&self.golive().failed).remove(&d.id);
            lock(&self.golive().failed_alerted).remove(&d.id);
        }
        drop(one_at_a_time);
        self.reconcile_midstream().await;
        if live {
            // Only the ones still ticked and sending: a second toggle may have taken one away.
            let now = self.golive_config();
            let running = self.golive_running();
            let still: Vec<DestinationConfig> = added
                .into_iter()
                .filter(|d| now.destinations.iter().any(|n| n.id == d.id && n.enabled) && running.contains(&d.id))
                .collect();
            self.push_stream_info_to(&still);
        }
        Ok(next)
    }

    /// What may not happen to the running stream: a destination its encode can't serve, an
    /// upload it would overload, and taking the last destination away.
    fn check_midstream(
        &self,
        before: &GoLiveConfig,
        next: &[DestinationConfig],
        added: &[DestinationConfig],
        confirm: DestinationConfirm,
    ) -> Result<(), EngineError> {
        for d in added {
            if let Some(why) = self.midstream_misfit(next, d) {
                return Err(refused("golive_needs_new_encode", why, "Stop and start the stream again with it ticked."));
            }
        }
        // Per live canvas: the last destination ticked on a picture that is on air.
        // Ones that have failed don't count as left: they send nothing, and with only failed
        // ones left the stream would be ended for the failure.
        let failed: std::collections::HashSet<String> = lock(&self.golive().failed).keys().cloned().collect();
        let emptied = |canvas: Canvas, on_air: bool| {
            on_air
                && before.destinations.iter().any(|d| d.enabled && d.canvas == canvas && !failed.contains(&d.id))
                && !next.iter().any(|d| d.enabled && d.canvas == canvas && !failed.contains(&d.id))
        };
        let g = self.golive();
        let last_one =
            emptied(Canvas::Main, g.hub.live().borrow().on) || emptied(Canvas::Vertical, g.vhub.live().borrow().on);
        if !confirm.last && last_one {
            return Err(EngineError::Refused {
                status: 409,
                code: "golive_last_destination",
                message: "That's the last destination: nothing will be sent to a platform any more.".into(),
                hint: "The stream and the recording keep going. Confirm to stop sending.".into(),
                retry_after: None,
            });
        }
        if !confirm.upload && !added.is_empty() {
            let after = GoLiveConfig { destinations: next.to_vec(), ..before.clone() };
            if let Some(check) = self.upload_now(&after)
                && !check.verdict.fits
            {
                return Err(EngineError::Refused {
                    status: 409,
                    code: "golive_upload_exceeded",
                    message: check.verdict.message,
                    hint: "Confirm to add it anyway, or lower the bitrate first.".into(),
                    retry_after: None,
                });
            }
        }
        Ok(())
    }

    /// Why `d` can't join the stream already running, or `None` when the running encode serves it
    /// as it is. Only `d` itself is judged (the others already take this stream), against the
    /// encode actually on air. Changing that encode would change every other output, so a
    /// destination that needs it changed is refused instead.
    fn midstream_misfit(&self, _next: &[DestinationConfig], d: &DestinationConfig) -> Option<String> {
        use ferrocast_golive::flv::VideoCodec;
        let g = self.golive();
        let name = dest_name(d);
        if d.canvas == Canvas::Vertical {
            return (!g.vhub.live().borrow().on)
                .then(|| "The vertical stream isn't running, so this destination can't join it now.".to_string());
        }
        // The main stream isn't running: it starts with it, nothing to disturb.
        if !g.hub.live().borrow().on {
            return None;
        }
        // On the Studio's lighter stream only while that stream really runs; from OBS "light"
        // has no lighter copy and the destination takes the full stream.
        if self.on_light(d) {
            return None;
        }
        let studio_on = g.studio_live.load(std::sync::atomic::Ordering::Acquire);
        let cfg = *lock(&g.studio_video);
        let stats = g.hub.stats();
        let direct = d.url.as_deref().and_then(direct_kind);
        if d.kind == Service::Twitch && d.enhanced && d.canvas == Canvas::Main && direct.is_none() {
            return Some(format!(
                "{name} with Enhanced Broadcasting starts at the beginning of a stream: it can't join this one."
            ));
        }
        if direct == Some("whip") {
            if !studio_on {
                return Some(format!(
                    "{name} (WebRTC) is sent by the Ferrocast Studio: it can't join a stream from OBS."
                ));
            }
            if cfg.is_some_and(|c| c.bframes) && !self.studio_no_bframes() {
                return Some(format!(
                    "{name} (WebRTC) can't carry B-frames, and this stream has them. Turning them off would change the stream every other output is sending."
                ));
            }
            if matches!(stats.video_codec, Some(VideoCodec::Av1 | VideoCodec::Vp9)) {
                return Some(format!("{name} (WebRTC) takes H.264 or HEVC and this stream is not."));
            }
            return None;
        }
        if direct.is_some() || self.golive_config().advanced.ignore_service_limits {
            return None;
        }
        let limit = probe::ceiling(d.kind, d.enhanced);
        // YouTube's own rules: no VP9.
        if d.kind == Service::Youtube && matches!(stats.video_codec, Some(VideoCodec::Vp9)) {
            return Some(format!("{name} doesn't take VP9, and this stream is VP9."));
        }
        let limit = limit?;
        // What is on air: the Studio's configured video rate held to the cap it already has, or
        // (from OBS) the measured rate less the sound and the packaging.
        let on_air_kbps = if studio_on && let Some(c) = cfg {
            c.video_kbps.min(self.studio_limit().map_or(u32::MAX, |l| l.max_kbps))
        } else {
            stats.kbps.saturating_sub(AUDIO_SLACK_KBPS)
        };
        if on_air_kbps > limit.max_kbps {
            return Some(format!(
                "{name} takes at most {} kbps and this stream is sending {}. Changing the live encode would change every other output.",
                limit.max_kbps, on_air_kbps
            ));
        }
        if limit.h264_only && stats.video_codec.is_some_and(|c| c != VideoCodec::H264) {
            return Some(format!(
                "{name} takes H.264 only and this stream is not. Changing the live encode would change every other output."
            ));
        }
        if limit.keyframe_secs > 0
            && stats.keyframe_interval_ms.is_some_and(|k| k > limit.keyframe_secs * 1000 + KEYFRAME_SLACK_MS)
        {
            return Some(format!(
                "{name} wants a keyframe every {} s and this stream sends one every {} s. Changing the live encode would change every other output.",
                limit.keyframe_secs,
                stats.keyframe_interval_ms.unwrap_or(0) / 1000
            ));
        }
        None
    }

    /// The last upload check, worked out again for the destinations ticked in `cfg` (a stream
    /// rate for each, as it is now); `None` when no check was ever made.
    fn upload_now(&self, cfg: &GoLiveConfig) -> Option<UploadCheck> {
        let g = self.golive();
        let last = lock(&g.upload).clone()?;
        let main = g.hub.stats().kbps;
        let vert = g.vhub.stats().kbps;
        let light = g.lhub.stats().kbps;
        let rate = |d: &DestinationConfig| {
            // What the destination really gets: the lighter stream's rate for a destination on it.
            let live_rate = if d.canvas == Canvas::Vertical {
                vert
            } else if self.on_light(d) && light > 0 {
                light
            } else {
                main
            };
            last.rows.iter().find(|r| r.id == d.id).map(|r| r.need_kbps).filter(|k| *k > 0 && live_rate == 0).unwrap_or(
                if live_rate > 0 {
                    live_rate
                } else if d.canvas == Canvas::Vertical {
                    DEFAULT_VERTICAL_NEED_KBPS
                } else {
                    DEFAULT_NEED_KBPS
                },
            )
        };
        // A backup waiting for its ticked main one sends only if that fails: not billed twice.
        let enabled: Vec<&DestinationConfig> =
            cfg.destinations.iter().filter(|d| d.enabled && !backs_up(cfg, &d.id).is_some_and(|p| p.enabled)).collect();
        let streaming = g.hub.live().borrow().on || g.vhub.live().borrow().on;
        let rows: Vec<UploadRow> = enabled
            .iter()
            .map(|d| {
                let old = last.rows.iter().find(|r| r.id == d.id);
                UploadRow {
                    id: d.id.clone(),
                    name: dest_name(d),
                    server: old.and_then(|r| r.server.clone()),
                    delay_ms: old.and_then(|r| r.delay_ms),
                    need_kbps: rate(d),
                    // A problem found by a check hours ago (unreachable then) isn't held against it
                    // on a stream that is running now: it is billed as the stream is.
                    problem: if streaming { None } else { old.and_then(|r| r.problem.clone()) },
                }
            })
            .collect();
        let needs: Vec<Need> = rows
            .iter()
            .filter(|r| r.problem.is_none())
            .map(|r| Need { name: r.name.clone(), kbps: r.need_kbps })
            .collect();
        let verdict = probe::verdict_keeping(last.verdict.upload_kbps, &needs, cfg.advanced.keep_free_pct);
        Some(UploadCheck { at: last.at, measured_by: last.measured_by, verdict, rows })
    }

    /// Gives a destination just added mid-stream the title and category already chosen for the
    /// others (their platform's own), without waiting for anyone.
    fn push_stream_info_to(&self, added: &[DestinationConfig]) {
        let platforms: Vec<Platform> = added.iter().filter_map(|d| platform_of(d.kind)).collect();
        if platforms.is_empty() {
            return;
        }
        let engine = self.clone();
        let want_youtube = platforms.contains(&Platform::Youtube);
        tokio::spawn(async move {
            let Ok(saved) = engine.stream_info_saved().await else { return };
            let Some(last) = saved.history.first().filter(|h| recent_push(h, now().0)) else { return };
            if let Err(e) = engine.apply_stream_info(last.info.clone(), Some(platforms), Some("added mid-stream")).await
            {
                tracing::warn!(error = %e, "couldn't give the title and category to a destination added mid-stream");
            }
            // YouTube may not have its broadcast yet: the title it waits with is given as soon as
            // it has one. (`youtube_when_live` only wakes when a stream starts, and this one
            // already has.)
            if want_youtube {
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    if !engine.golive_stream_running() || engine.apply_youtube_pending().await {
                        break;
                    }
                }
            }
        });
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
        // Off while live puts it on the full stream: that must fit as a new destination does.
        let moved = d.clone();
        if !on && moved.enabled && self.golive_stream_running() {
            let now: Vec<DestinationConfig> = cfg.destinations.clone();
            if let Some(why) = self.midstream_misfit(&now, &moved) {
                return Err(refused("golive_needs_new_encode", why, "Stop and start the stream again to change it."));
            }
        }
        self.save_golive(cfg.clone()).await?;
        drop(one_at_a_time);
        if let Some(r) = lock(&self.golive().running).remove(id) {
            r.stop.send_replace(true);
        }
        self.reconcile_midstream().await;
        Ok(cfg)
    }

    /// Twitch Enhanced Broadcasting for the Studio stream about to start (`out` its size,
    /// `fps`): Twitch's plan is fetched and kept for Twitch's destination → the other
    /// qualities for the Studio to encode (width, height, kbps), and a note in words when it
    /// was wanted but can't be (the stream then goes out as usual).
    pub(crate) async fn enhanced_tracks(
        &self,
        out: (u32, u32),
        fps: u32,
        vod: bool,
    ) -> (Vec<(u32, u32, u32)>, Option<String>) {
        use ferrocast_golive::twitch_eb;
        *lock(&self.golive().enhanced) = None;
        let wanted = self
            .golive_config()
            .destinations
            .iter()
            .any(|d| d.enabled && d.enhanced && d.kind == Service::Twitch && d.canvas == Canvas::Main);
        if !wanted {
            return (Vec::new(), None);
        }
        let off = |why: String| (Vec::new(), Some(format!("Enhanced Broadcasting is off: {why}")));
        let Some(c) = platform_of(Service::Twitch).and_then(|p| self.connector(p)) else {
            return off("connect Twitch in Settings, Accounts.".into());
        };
        let key = match c.ingest().await {
            Ok(i) => i.key,
            Err(e) => return off(format!("Twitch's stream key couldn't be read ({e}).")),
        };
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let ask = twitch_eb::Ask {
            stream_key: key,
            version: env!("CARGO_PKG_VERSION").into(),
            codecs: vec!["h264".into()],
            width: out.0,
            height: out.1,
            canvas_width: out.0,
            canvas_height: out.1,
            fps: (fps.max(1), 1),
            vod_track_audio: vod,
            max_bitrate_kbps: None,
            max_tracks: Some(twitch_eb::MAX_TRACKS),
            audio_rate: 48_000,
            audio_channels: 2,
            capabilities: serde_json::json!({
                "cpu": { "logical_cores": cores, "physical_cores": cores },
                "system": { "name": std::env::consts::OS, "bits": 64, "arm": cfg!(target_arch = "aarch64") },
            }),
        };
        let client = reqwest::Client::new();
        match twitch_eb::fetch(&client, &ask).await {
            Ok(plan) => {
                // The main picture is track 0; Twitch's smaller ones follow (even sides, a few).
                let tracks: Vec<(u32, u32, u32)> = plan
                    .encodes
                    .iter()
                    .filter(|e| e.height < out.1 && e.width % 2 == 0 && e.height % 2 == 0)
                    .map(|e| (e.width, e.height, e.kbps().max(300)))
                    .take(4)
                    .collect();
                tracing::info!(config = %plan.config_id, qualities = tracks.len() + 1, "Twitch Enhanced Broadcasting");
                let note = plan.warning.clone().map(|w| format!("Twitch says: {w}"));
                *lock(&self.golive().enhanced) = Some(plan);
                (tracks, note)
            }
            Err(e) => off(format!("{e}.")),
        }
    }

    /// Turns Twitch Enhanced Broadcasting on or off for destination `id` (Twitch, main picture).
    /// It takes effect the next time the Studio starts streaming.
    pub async fn set_golive_enhanced(&self, id: &str, on: bool) -> Result<GoLiveConfig, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        let d = cfg.destinations.iter_mut().find(|d| d.id == id).ok_or(EngineError::NotFound("destination"))?;
        if on && (d.kind != Service::Twitch || d.canvas != Canvas::Main) {
            return Err(EngineError::Invalid("enhanced: Twitch's main picture only.".into()));
        }
        if d.enhanced != on {
            d.enhanced = on;
            self.save_golive(cfg.clone()).await?;
        }
        // It takes effect the next time the Studio starts streaming: a running stream's encoder
        // is shared by every output and isn't reshaped for it.
        if !self.golive_stream_running() {
            self.sync_stream_cap().await;
        }
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
                        "url: a whole address like srt://host:port, srtla://host:port, rist://host:port or https://host/whip, without a user name.".into(),
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
            // While a stream runs it is added unticked: ticking it is the guarded step (the
            // encode check, the upload question), and it starts at the next stream otherwise.
            enabled: !self.golive_stream_running(),
            canvas,
            light: false,
            enhanced: false,
        };
        cfg.destinations.push(d.clone());
        self.save_golive(cfg).await?;
        drop(one_at_a_time);
        self.reconcile_midstream().await;
        Ok(d)
    }

    /// Removes a custom destination and its key. The three platforms can't be removed, only
    /// switched off.
    /// "Test connection" on a destination: tries its server as the stream would (a platform's
    /// RTMP server; a server of your own with its saved key) and says how it went.
    pub async fn golive_test(&self, id: &str) -> Result<ferrocast_golive::reach::Reach, EngineError> {
        let d = self
            .golive_config()
            .destinations
            .into_iter()
            .find(|d| d.id == id)
            .ok_or(EngineError::NotFound("destination"))?;
        if d.kind == Service::Custom {
            let url = d.url.clone().unwrap_or_default();
            let key =
                self.inner.secrets.get(&format!("{CUSTOM_KEY_PREFIX}{}", d.id)).ok().flatten().unwrap_or_default();
            return Ok(ferrocast_golive::reach::reach(&url, &key).await);
        }
        // Read-only server peek: never creates, binds or mutates broadcasts/keys during a test or
        // pre-flight (especially YouTube, where resolve() prepares a live broadcast).
        match self.peek_target(&d).await {
            Ok(t) => {
                let url = format!("{}://{}:{}/{}", if t.tls { "rtmps" } else { "rtmp" }, t.host, t.port, t.app);
                Ok(ferrocast_golive::reach::reach(&url, "").await)
            }
            Err(why) => {
                Ok(ferrocast_golive::reach::Reach { ok: false, rtt_ms: None, message: why, srt_latency_ms: None })
            }
        }
    }

    /// Each running destination (its name) and whether it's sending now, for the live watch.
    pub(crate) fn golive_sending(&self) -> Vec<(String, bool)> {
        let cfg = self.golive_config();
        lock(&self.golive().running)
            .iter()
            .filter_map(|(id, r)| {
                let d = cfg.destinations.iter().find(|d| &d.id == id)?;
                Some((dest_name(d), r.health.borrow().state == PushState::Live))
            })
            .collect()
    }

    /// This computer's network connections, for RIST's `via=` (two at once).
    pub fn golive_networks(&self) -> Vec<ferrocast_golive::links::Network> {
        ferrocast_golive::links::networks()
    }

    /// Pre-flight's Go live checks: each ticked destination tried (as Test connection; the
    /// answers kept a minute, so a pre-flight shown again doesn't reach every server again), and
    /// the last upload check's verdict when it doesn't fit.
    pub(crate) async fn golive_preflight(&self) -> Vec<ferrocast_core::show::Check> {
        use ferrocast_core::show::{Check, CheckStatus, Fix};
        use ferrocast_golive::reach::Reach;
        let g = self.golive();
        let cfg = self.golive_config();
        // Ticked ones, and the backups that may send for them (ticked or not).
        let ticked: Vec<DestinationConfig> = cfg
            .destinations
            .iter()
            .filter(|d| d.enabled || backs_up(&cfg, &d.id).is_some_and(|p| p.enabled))
            .cloned()
            .collect();
        let mut checks = Vec::new();
        let fresh = |at: &std::time::Instant| at.elapsed() < std::time::Duration::from_secs(60);
        let stale: Vec<DestinationConfig> = {
            let tested = lock(&g.tested);
            ticked.iter().filter(|d| !tested.get(&d.id).is_some_and(|(at, _)| fresh(at))).cloned().collect()
        };
        let results = futures_util::future::join_all(stale.iter().map(|d| async move {
            let r = tokio::time::timeout(std::time::Duration::from_secs(8), self.golive_test(&d.id)).await;
            (d.id.clone(), r)
        }))
        .await;
        for (id, r) in results {
            // A test that couldn't finish is said too, not left out (nor an older answer kept).
            let reach = match r {
                Ok(Ok(reach)) => reach,
                Ok(Err(e)) => {
                    Reach { ok: false, rtt_ms: None, message: format!("Couldn't test it: {e}"), srt_latency_ms: None }
                }
                Err(_) => Reach {
                    ok: false,
                    rtt_ms: None,
                    message: "Its server took too long to answer.".into(),
                    srt_latency_ms: None,
                },
            };
            lock(&g.tested).insert(id, (std::time::Instant::now(), reach));
        }
        let tested = lock(&g.tested).clone();
        for d in &ticked {
            let name = dest_name(d);
            let Some((_, r)) = tested.get(&d.id).filter(|(at, _)| fresh(at)) else { continue };
            let (status, detail) =
                if r.ok { (CheckStatus::Ok, r.message.clone()) } else { (CheckStatus::Fail, r.message.clone()) };
            let fix = (!r.ok).then(|| Fix { label: "Go live".into(), href: "#/studio/golive".into() });
            checks.push(Check::new(&format!("dest:{}", d.id), &name, status, detail, fix));
        }
        if let Some(u) = lock(&g.upload).clone()
            && !u.verdict.fits
            && !ticked.is_empty()
        {
            checks.push(Check::new(
                "upload",
                "Upload",
                CheckStatus::Warn,
                u.verdict.message.clone(),
                Some(Fix { label: "Test upload".into(), href: "#/studio/golive".into() }),
            ));
        }
        checks
    }

    /// "Test connection" in the add dialog, before the server is saved.
    pub async fn golive_test_address(
        &self,
        url: &str,
        key: &str,
    ) -> Result<ferrocast_golive::reach::Reach, EngineError> {
        if url.trim().is_empty() || url.len() > 2048 || key.len() > 512 {
            return Err(EngineError::Invalid("url: an address up to 2048 letters; key: up to 512.".into()));
        }
        Ok(ferrocast_golive::reach::reach(url, key).await)
    }

    pub async fn remove_golive_destination(&self, id: &str) -> Result<(), EngineError> {
        let one_at_a_time = self.config_lock().await;
        let mut cfg = self.golive_config();
        let Some(i) = cfg.destinations.iter().position(|d| d.id == id) else {
            return Err(EngineError::NotFound("destination"));
        };
        if cfg.destinations[i].kind != Service::Custom {
            return Err(EngineError::Invalid("Twitch, YouTube and Kick can be switched off, not removed.".into()));
        }
        // The last one sending is stopped by switching it off, which asks first.
        if cfg.destinations[i].enabled
            && self.golive_stream_running()
            && !cfg.destinations.iter().enumerate().any(|(j, d)| j != i && d.enabled)
        {
            return Err(EngineError::Refused {
                status: 409,
                code: "golive_last_destination",
                message: "That's the last destination sending: removing it stops the sending.".into(),
                hint: "Switch it off first (it asks), then remove it.".into(),
                retry_after: None,
            });
        }
        cfg.destinations.remove(i);
        // Its Advanced settings go with it, so a saved set never names a missing destination
        // (nor one that backed something up).
        cfg.advanced.per.remove(id);
        for a in cfg.advanced.per.values_mut() {
            if a.backup.as_deref() == Some(id) {
                a.backup = None;
            }
        }
        if cfg.advanced.rules.first_to_pause.as_deref() == Some(id) {
            cfg.advanced.rules.first_to_pause = None;
        }
        self.save_golive(cfg).await?;
        drop(one_at_a_time);
        let _ = self.inner.secrets.delete(&format!("{CUSTOM_KEY_PREFIX}{id}"));
        lock(&self.golive().failed).remove(id);
        lock(&self.golive().failed_alerted).remove(id);
        self.reconcile_midstream().await;
        Ok(())
    }

    pub(crate) async fn save_golive(&self, cfg: GoLiveConfig) -> Result<(), EngineError> {
        // A changed server (or any change) is tried again rather than trusting an old answer.
        lock(&self.golive().tested).clear();
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
        // Nothing ticked still measures the upload (Cloudflare's speed test): Set up for me
        // asks before any platform is picked, and the verdict says to turn one on.
        let ticked: Vec<DestinationConfig> = cfg.destinations.into_iter().filter(|d| d.enabled).collect();
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
            // Read-only peek for platforms that don't run bandwidth-test (YouTube, Kick) so an
            // upload check never creates or mutates YouTube broadcasts; Twitch needs its key for
            // `?bandwidthtest=true` (read-only on Twitch).
            let resolved: Result<(Target, Option<String>), String> = match d.kind {
                Service::Twitch | Service::Custom => self.resolve(d).await.map(|dest| (dest.target, Some(dest.key))),
                Service::Youtube | Service::Kick => self.peek_target(d).await.map(|t| (t, None)),
            };
            match resolved {
                Ok((target, key)) => {
                    row.server = Some(target.host.clone());
                    match probe::delay(&target).await {
                        Ok(t) => row.delay_ms = Some(t.as_millis() as u32),
                        Err(e) => row.problem = Some(e.to_string()),
                    }
                    if d.kind == Service::Twitch
                        && twitch.is_none()
                        && let Some(k) = key
                    {
                        twitch = Some((target, k));
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
                format!("Your upload is {}. {names} couldn't be reached, so nothing was counted.", probe::mbps(upload))
            } else {
                format!("{} {names} couldn't be reached, not counted.", verdict.message)
            };
        }
        let check = UploadCheck { at: now().0, measured_by: measured_by.into(), verdict, rows };
        *lock(&self.golive().upload) = Some(check.clone());
        Ok(check)
    }

    /// Read-only ingest server lookup for Test connection, Pre-flight and Upload check.
    /// Never creates, binds or mutates broadcasts or stream keys on any platform.
    async fn peek_target(&self, d: &DestinationConfig) -> Result<Target, String> {
        let url = match platform_of(d.kind) {
            Some(p) => {
                let c = self.connector(p).ok_or_else(|| format!("Connect {p} in Settings, Accounts first."))?;
                // The keys set by hand for the vertical canvas are never the normal stream's.
                c.set_vertical_keys(self.vertical_custom_keys());
                let got = match d.canvas {
                    Canvas::Main => c.peek_ingest().await,
                    Canvas::Vertical => c.peek_vertical_ingest().await,
                };
                let own_server = self.golive_config().advanced.per.get(&d.id).and_then(|a| a.server.clone());
                match got {
                    Ok(u) => own_server.unwrap_or(u),
                    Err(e) => return Err(format_platform_error(p, e, false)),
                }
            }
            None => d.url.clone().ok_or_else(|| "This server has no address.".to_string())?,
        };
        Target::parse(&url).map_err(|e| e.to_string())
    }

    /// How a destination reconnects: the Studio's Advanced settings (on or off, how many tries, how
    /// long the first wait) govern every destination, so changing them there changes Twitch, YouTube
    /// and Kick as well as the Studio's own feed. A destination's own first wait (Go live's advanced
    /// settings) still wins when it was changed from the default; a server's refusal still caps the
    /// tries at three. Streaming from OBS (no Studio) keeps Go live's own defaults.
    async fn destination_retry(&self, id: &str) -> ferrocast_golive::push::Retry {
        let advanced = self.golive_config().advanced;
        let mut r = advanced.retry(id);
        let Ok(host) = self.studio_host() else { return r };
        let s = host.settings().await;
        if !s.reconnect {
            return ferrocast_golive::push::Retry::never();
        }
        r.max_attempts = s.reconnect_tries;
        if advanced.per.get(id).is_none_or(|a| a.first_retry_s == DestAdvanced::default().first_retry_s) {
            r.first = Duration::from_secs(u64::from(s.reconnect_every_s.max(1)));
        }
        r
    }

    /// The saved stream keys of the custom servers on the vertical canvas.
    fn vertical_custom_keys(&self) -> Vec<String> {
        self.golive_config()
            .destinations
            .iter()
            .filter(|d| d.canvas == Canvas::Vertical && platform_of(d.kind).is_none())
            .filter_map(|d| self.inner.secrets.get(&format!("{CUSTOM_KEY_PREFIX}{}", d.id)).ok().flatten())
            .filter(|k| !k.is_empty())
            .collect()
    }

    /// A destination's server and key: fetched from its platform (write path, only when actually
    /// going live), or the saved custom one. The error is a clear one-line reason for the streamer.
    pub(crate) async fn resolve(&self, d: &DestinationConfig) -> Result<Destination, String> {
        let (url, key, backup) = match platform_of(d.kind) {
            Some(p) => {
                let c = self.connector(p).ok_or_else(|| format!("Connect {p} in Settings, Accounts first."))?;
                // The keys set by hand for the vertical canvas are never the normal stream's.
                c.set_vertical_keys(self.vertical_custom_keys());
                let got = match d.canvas {
                    Canvas::Main => c.ingest().await,
                    Canvas::Vertical => c.vertical_ingest().await,
                };
                let own_server = self.golive_config().advanced.per.get(&d.id).and_then(|a| a.server.clone());
                match got {
                    // A server the person chose themselves has no backup.
                    Ok(i) => {
                        let backup = if own_server.is_some() { None } else { i.backup };
                        match &i.note {
                            Some(n) => lock(&self.golive().key_notes).insert(d.id.clone(), n.clone()),
                            None => lock(&self.golive().key_notes).remove(&d.id),
                        };
                        (own_server.unwrap_or(i.url), i.key, backup)
                    }
                    Err(e) => return Err(format_platform_error(p, e, true)),
                }
            }
            None => {
                let url = d.url.clone().ok_or_else(|| "This server has no address.".to_string())?;
                let key =
                    self.inner.secrets.get(&format!("{CUSTOM_KEY_PREFIX}{}", d.id)).ok().flatten().ok_or_else(
                        || "This server's stream key is missing: remove it and add it again.".to_string(),
                    )?;
                (url, key, None)
            }
        };
        // Twitch Enhanced Broadcasting: to the address Twitch's plan gave, with every quality.
        let plan = (d.kind == Service::Twitch && d.enhanced && d.canvas == Canvas::Main)
            .then(|| lock(&self.golive().enhanced).clone())
            .flatten();
        let (url, key, extra_video) = match plan.as_ref().and_then(|p| p.url.rsplit_once('/')) {
            Some((server, k)) => (server.to_string(), k.to_string(), true),
            None => (url, key, false),
        };
        let mut target = Target::parse(&url).map_err(|e| e.to_string())?;
        // The streamer's choice of IP version and connection (the Studio's own feed to Go live's
        // local server doesn't pass here).
        let advanced = self.golive_config().advanced;
        target.family = advanced.ip_family;
        target.bind = advanced.bind.filter(|b| !b.trim().is_empty());
        // One that doesn't parse is left out: the primary is what matters.
        let backup = backup.as_deref().and_then(|b| Target::parse(b).ok()).map(|mut b| {
            b.family = target.family;
            b.bind = target.bind.clone();
            b
        });
        Ok(Destination {
            id: d.id.clone(),
            target,
            key,
            retry: self.destination_retry(&d.id).await,
            // Twitch's VOD track: only Twitch knows what to do with an extra audio track.
            extra_audio: d.kind == Service::Twitch,
            extra_video,
            backup,
        })
    }

    /// Starts forwarders for ticked destinations whose stream (normal or vertical) is live and
    /// stops the rest. When neither stream is live, the show's pauses and reasons are cleared.
    pub(crate) async fn reconcile(&self) {
        let g = self.golive();
        let _one = g.reconciling.lock().await;
        let was_running: std::collections::HashSet<String> = lock(&g.running).keys().cloned().collect();
        let (main_on, vertical_on, light_on) =
            (g.hub.live().borrow().on, g.vhub.live().borrow().on, g.lhub.live().borrow().on);
        let on = |d: &DestinationConfig| match d.canvas {
            Canvas::Main => main_on || (d.wants_light() && light_on),
            Canvas::Vertical => vertical_on,
        };
        let cfg = self.golive_config();
        let failing = lock(&g.backup).failing.clone();
        // A backup sends while the one it backs up fails, and waits otherwise.
        let sends = |d: &DestinationConfig| match backs_up(&cfg, &d.id) {
            Some(p) if p.enabled => failing.contains(&p.id),
            _ => d.enabled,
        };
        let want: Vec<DestinationConfig> = cfg.destinations.iter().filter(|d| sends(d) && on(d)).cloned().collect();
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
            if g.health_end_pending.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.health_stream_ended(now()).await;
            }
            lock(&g.protector).reset();
            // The next stream starts with every destination given its own chance again.
            *lock(&g.backup) = BackupState::default();
            lock(&g.quality).reset();
        }
        // Apply stream caps (e.g. WebRTC no-B-frames) BEFORE launching new direct destinations so
        // an already-live Studio stream reshapes before WHIP subscribes to it.
        if !want.is_empty() {
            self.sync_stream_cap().await;
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
        self.sync_stream_cap().await;
        // Which destinations started or stopped sending, for the session record.
        let now_running: std::collections::HashSet<String> = lock(&g.running).keys().cloned().collect();
        for d in &cfg.destinations {
            let (was, is) = (was_running.contains(&d.id), now_running.contains(&d.id));
            if was != is {
                self.note_destination(&d.id, &dest_name(d), is);
            }
        }
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
        self.sync_stream_cap().await;
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
                lock(&g.failed).remove(&d.id);
                lock(&g.failed_alerted).remove(&d.id);
                let (health_tx, health) = watch::channel(Health {
                    state: PushState::Waiting,
                    kbps: 0,
                    dropped: 0,
                    sent: 0,
                    link: None,
                    via: None,
                    since_ms: 0,
                });
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
        let simulcast = self.simulcast(d);
        if simulcast {
            // Its second layer, the lighter stream, starts first.
            self.studio_sync_light().await;
        }
        let started = match self.studio_host() {
            Ok(host) => host.direct_start(d.id.clone(), url, key, simulcast).await.map(|()| host),
            Err(_) => Err(String::new()),
        };
        let host = match started {
            Ok(h) => h,
            Err(why) => {
                let reason = if why.is_empty() || why.contains("isn't streaming") {
                    "SRT, RIST and WebRTC go out from Studio: go live there.".to_string()
                } else {
                    format!("Couldn't start: {why}")
                };
                tracing::info!(dest = %d.id, %reason, "Go live: a direct destination can't start");
                lock(&g.blocked).insert(d.id.clone(), reason);
                return;
            }
        };
        lock(&g.blocked).remove(&d.id);
        lock(&g.failed).remove(&d.id);
        lock(&g.failed_alerted).remove(&d.id);
        let now_ms = || u64::try_from(now().0).unwrap_or(0);
        let (health_tx, health) = watch::channel(Health {
            state: PushState::Connecting,
            kbps: 0,
            dropped: 0,
            sent: 0,
            link: None,
            via: None,
            since_ms: now_ms(),
        });
        let (stop, mut stop_rx) = watch::channel(false);
        let id = d.id.clone();
        let engine = self.clone();
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
                let state = if st["failed"].as_bool().unwrap_or(false) {
                    PushState::Failed {
                        attempts: st["attempts"].as_u64().unwrap_or(0) as u32,
                        reason: st["reason"]
                            .as_str()
                            .map_or_else(|| "Couldn't send to the server.".into(), String::from),
                        detail: st["detail"].as_str().map(String::from),
                        refused: st["refused"].as_bool().unwrap_or(false),
                    }
                } else if st["reconnecting"].as_bool().unwrap_or(false) {
                    PushState::Reconnecting {
                        attempt: 0,
                        retry_in_ms: 0,
                        reason: st["reason"]
                            .as_str()
                            .map_or_else(|| "The server dropped the connection; trying again.".into(), String::from),
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
                    h.link = serde_json::from_value(st["link"].clone()).ok();
                });
            }
            let _ = host.direct_stop(id.clone()).await;
            let mut running = lock(&engine.golive().running);
            if running.get(&id).is_some_and(|r| r.health.same_channel(&health_tx.subscribe())) {
                running.remove(&id);
            }
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

    /// "Resume now" on a paused or failed destination: it starts again at once.
    pub async fn golive_resume(&self, id: &str) -> Result<(), EngineError> {
        let g = self.golive();
        let is_paused = lock(&g.paused).contains_key(id);
        let is_failed = lock(&g.failed).contains_key(id)
            || lock(&g.running).get(id).is_some_and(|r| matches!(r.health.borrow().state, PushState::Failed { .. }));
        if !is_paused && !is_failed {
            return Err(refused("golive_not_paused", "That platform isn't paused.", "Nothing to resume."));
        }
        if is_failed {
            if let Some(r) = lock(&g.running).remove(id) {
                r.stop.send_replace(true);
            }
            lock(&g.failed).remove(id);
            lock(&g.failed_alerted).remove(id);
        }
        self.unpause(id).await;
        Ok(())
    }

    /// Clears saved failure lines for `canvas` (called when a fresh stream begins).
    pub(crate) fn clear_golive_failures(&self, canvas: Canvas) {
        let cfg = self.golive_config();
        let g = self.golive();
        let ids: std::collections::HashSet<String> =
            cfg.destinations.into_iter().filter(|d| d.canvas == canvas).map(|d| d.id).collect();
        lock(&g.failed).retain(|id, _| !ids.contains(id));
        lock(&g.failed_alerted).retain(|id| !ids.contains(id));
    }

    /// Returns a reason when at least one enabled main destination reached terminal
    /// `PushState::Failed` and *every* enabled main destination is now terminally `Failed` or
    /// `blocked` (zero live, connecting, reconnecting, waiting, or paused).
    pub(crate) fn golive_main_failed(&self) -> Option<String> {
        let cfg = self.golive_config();
        let g = self.golive();
        let running = lock(&g.running);
        let paused = lock(&g.paused);
        let blocked = lock(&g.blocked);
        let failed = lock(&g.failed);
        let enabled_main: Vec<&DestinationConfig> = cfg
            .destinations
            .iter()
            .filter(|d| d.canvas == Canvas::Main && d.enabled && !backs_up(&cfg, &d.id).is_some_and(|p| p.enabled))
            .collect();
        if enabled_main.is_empty() {
            return None;
        }
        let mut first_reason: Option<String> = None;
        for d in enabled_main {
            if paused.contains_key(&d.id) {
                return None;
            }
            // If this destination's backup is currently running and hasn't failed, stream is alive.
            if let Some(backup_id) = cfg.advanced.per.get(&d.id).and_then(|a| a.backup.as_deref())
                && let Some(br) = running.get(backup_id)
                && !matches!(br.health.borrow().state, PushState::Failed { .. })
            {
                return None;
            }
            if let Some(r) = running.get(&d.id) {
                match &r.health.borrow().state {
                    PushState::Failed { reason, .. } => {
                        if first_reason.is_none() {
                            first_reason = Some(reason.clone());
                        }
                    }
                    _ => return None,
                }
            } else if let Some(h) = failed.get(&d.id) {
                if let PushState::Failed { reason, .. } = &h.state
                    && first_reason.is_none()
                {
                    first_reason = Some(reason.clone());
                }
            } else if blocked.contains_key(&d.id) {
                continue;
            } else {
                // Not started yet (reconcile hasn't spawned it yet).
                return None;
            }
        }
        first_reason
    }

    /// The reason when every enabled main destination is waiting to connect again (none Live) and at
    /// least one is Reconnecting: the Studio's own stream still runs into Go live's local server, so
    /// without this the status bar shows LIVE, Health 100 and "not reconnecting" through a whole outage.
    pub(crate) fn golive_main_reconnecting(&self) -> Option<String> {
        let cfg = self.golive_config();
        let g = self.golive();
        let running = lock(&g.running);
        let main: Vec<&DestinationConfig> = cfg
            .destinations
            .iter()
            .filter(|d| d.canvas == Canvas::Main && d.enabled && !backs_up(&cfg, &d.id).is_some_and(|p| p.enabled))
            .collect();
        // A backup that is live carries the stream for its destination: nothing to report.
        for d in &main {
            if let Some(backup_id) = cfg.advanced.per.get(&d.id).and_then(|a| a.backup.as_deref())
                && running.get(backup_id).is_some_and(|r| matches!(r.health.borrow().state, PushState::Live))
            {
                return None;
            }
        }
        let states: Vec<PushState> =
            main.iter().filter_map(|d| running.get(&d.id).map(|r| r.health.borrow().state.clone())).collect();
        reconnecting_reason(&states)
    }

    /// The state of every enabled main destination (those that run, and those that gave up).
    pub(crate) fn golive_main_states(&self) -> Vec<PushState> {
        let cfg = self.golive_config();
        let g = self.golive();
        let running = lock(&g.running);
        let failed = lock(&g.failed);
        cfg.destinations
            .iter()
            .filter(|d| d.canvas == Canvas::Main && d.enabled && !backs_up(&cfg, &d.id).is_some_and(|p| p.enabled))
            .filter_map(|d| {
                running
                    .get(&d.id)
                    .map(|r| r.health.borrow().state.clone())
                    .or_else(|| failed.get(&d.id).map(|h| h.state.clone()))
            })
            .collect()
    }

    /// Records any destination that reached `PushState::Failed`, alerts the dashboard once per
    /// stream, and stops the Studio stream when every enabled main destination has failed so the
    /// Studio stops showing LIVE for a dead broadcast.
    async fn failure_tick(&self) {
        let cfg = self.golive_config();
        let g = self.golive();
        let newly_failed: Vec<(String, String, Option<String>)> = {
            let running = lock(&g.running);
            let mut failed = lock(&g.failed);
            let mut alerted = lock(&g.failed_alerted);
            let mut out = Vec::new();
            for (id, r) in running.iter() {
                let h = r.health.borrow().clone();
                // It came back by itself (the slow retries): no longer failed, and a later failure
                // alerts again.
                if matches!(h.state, PushState::Live) {
                    failed.remove(id);
                    alerted.remove(id);
                }
                if let PushState::Failed { reason, detail, .. } = &h.state {
                    failed.insert(id.clone(), h.clone());
                    if alerted.insert(id.clone()) {
                        let name =
                            cfg.destinations.iter().find(|d| &d.id == id).map(dest_name).unwrap_or_else(|| id.clone());
                        out.push((name, reason.clone(), detail.clone()));
                    }
                }
            }
            out
        };
        for (name, reason, detail) in newly_failed {
            let mut alert = ferrocast_core::show::LiveAlert::raised(format!("destination:{name}"), "error", reason);
            if let Some(d) = detail {
                alert = alert.detail(d);
            }
            self.studio_frame("live_alert", &alert);
        }
        if let Some(reason) = self.golive_main_failed()
            && let Ok(host) = self.studio_host()
            && host.stream_stats().await.ok().is_some_and(|v| !v.is_null())
        {
            tracing::warn!(%reason, "Go live: every enabled destination failed; stopping Studio stream");
            self.abort_live_on_destination_failure(&reason).await;
            self.send_golive();
        }
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
        // Unticked while the protector was choosing it: nothing to pause (and nothing to bring back).
        if !self.golive_config().destinations.iter().any(|d| d.id == id && d.enabled) {
            return;
        }
        if let Some(r) = lock(&g.running).remove(&id) {
            r.stop.send_replace(true);
        }
        let now_ms = u64::try_from(now().0).unwrap_or(0);
        let left = lock(&g.protector)
            .paused_until(&id)
            .map(|u| u.saturating_duration_since(std::time::Instant::now()).as_millis() as u64)
            .unwrap_or(0);
        tracing::info!(dest = %id, "Go live: upload protection paused a destination");
        let reason = format!("Paused to keep {top} smooth; it comes back by itself.");
        let health = Health {
            state: PushState::Paused { until_ms: now_ms + left, reason },
            kbps: 0,
            dropped: 0,
            sent: 0,
            link: None,
            via: None,
            since_ms: now_ms,
        };
        lock(&g.paused).insert(id, health);
        self.send_golive();
    }

    /// One second of backups while live: a destination that can't send for `BACKUP_AFTER`
    /// hands over to its backup, and takes back once it's sent for as long.
    async fn backup_tick(&self) {
        let g = self.golive();
        let cfg = self.golive_config();
        let now = std::time::Instant::now();
        let mut changed = Vec::new();
        {
            let running = lock(&g.running);
            let blocked = lock(&g.blocked);
            let mut b = lock(&g.backup);
            let primaries: Vec<&String> =
                cfg.advanced.per.iter().filter(|(_, a)| a.backup.is_some()).map(|(id, _)| id).collect();
            b.since.retain(|id, _| primaries.contains(&id));
            for id in primaries {
                let Some(d) = cfg.destinations.iter().find(|d| &d.id == id).filter(|d| d.enabled) else {
                    if b.failing.remove(id) {
                        changed.push((id.clone(), false));
                    }
                    continue;
                };
                let sending = running.get(id).is_some_and(|r| r.health.borrow().state == PushState::Live);
                // Running or not, one that can't start is failing too.
                let trying = running.contains_key(id) || blocked.contains_key(id);
                if !trying {
                    continue;
                }
                let since = match b.since.get(id) {
                    Some((was, at)) if *was == sending => *at,
                    _ => {
                        b.since.insert(id.clone(), (sending, now));
                        now
                    }
                };
                let long = now.duration_since(since) >= BACKUP_AFTER;
                let failing = b.failing.contains(id);
                if !sending && long && !failing {
                    tracing::info!(dest = %d.id, "Go live: a destination can't send, its backup takes over");
                    b.failing.insert(id.clone());
                    changed.push((id.clone(), true));
                } else if sending && long && failing {
                    tracing::info!(dest = %d.id, "Go live: a destination is back, its backup stops");
                    b.failing.remove(id);
                    changed.push((id.clone(), false));
                }
            }
        }
        if !changed.is_empty() {
            self.reconcile().await;
        }
    }

    /// One second of quality per destination: one that falls behind alone (others keep up, so
    /// it's its route, not the upload) takes the lighter stream, and tries the full one again
    /// later. OBS can only lower every destination at once.
    async fn quality_tick(&self) {
        use ferrocast_golive::protect::Sample;
        use ferrocast_golive::quality::Move;
        let g = self.golive();
        // Only the Ferrocast Studio makes a lighter stream (OBS sends one): with none to move
        // to, nothing moves.
        let studio = match self.studio_host() {
            Ok(h) => h.stream_stats().await.ok().is_some_and(|v| !v.is_null() && v["error"].is_null()),
            Err(_) => false,
        };
        if !studio {
            return;
        }
        // Nothing is sending to a platform (all switched off, the stream goes on for a
        // recording): there is nothing to protect or step down.
        if lock(&g.running).is_empty() {
            return;
        }
        let cfg = self.golive_config();
        let now_ms = u64::try_from(now().0).unwrap_or(0);
        let want_kbps = g.hub.stats().kbps;
        // RTMP destinations on the main picture's full stream (SRT, RIST and WebRTC go from the
        // Studio's own stream and can't move).
        let movable = |id: &str| {
            cfg.destinations
                .iter()
                .any(|d| d.id == id && d.canvas == Canvas::Main && d.url.as_deref().and_then(direct_kind).is_none())
        };
        let healths: Vec<(String, Health)> = lock(&g.running)
            .iter()
            .filter(|(id, r)| !r.light && movable(id))
            .map(|(id, r)| (id.clone(), r.health.borrow().clone()))
            .filter(|(_, h)| h.state == PushState::Live)
            .collect();
        let samples: Vec<Sample<'_>> = healths
            .iter()
            .map(|(id, h)| Sample {
                id,
                kbps: h.kbps,
                want_kbps,
                dropped: h.dropped,
                live_ms: now_ms.saturating_sub(h.since_ms),
            })
            .collect();
        let moves = lock(&g.quality).tick(std::time::Instant::now(), &samples, cfg.advanced.rules.slow_pct);
        for m in &moves {
            match m {
                Move::Lighter(id, wait) => {
                    tracing::info!(dest = %id, wait_s = wait.as_secs(), "Go live: a slow route takes the lighter stream");
                }
                Move::Full(id) => tracing::info!(dest = %id, "Go live: trying the full stream again"),
            }
        }
        if !moves.is_empty() {
            // The Studio's lighter stream starts (or stops) to match, then each moves.
            self.studio_sync_light().await;
            self.reconcile().await;
        }
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
        let healths: Vec<(String, Health, bool)> =
            lock(&g.running).iter().map(|(id, r)| (id.clone(), r.health.borrow().clone(), r.light)).collect();
        let canvas_of = |id: &str| cfg.destinations.iter().find(|d| d.id == id).map(|d| d.canvas).unwrap_or_default();
        let (main_kbps, vertical_kbps, light_kbps) = (g.hub.stats().kbps, g.vhub.stats().kbps, g.lhub.stats().kbps);
        let samples: Vec<Sample<'_>> = healths
            .iter()
            .filter(|(_, h, _)| h.state == PushState::Live)
            .map(|(id, h, light)| Sample {
                id,
                kbps: h.kbps,
                // Measured against the stream it sends: the lighter one is meant to be less.
                want_kbps: match canvas_of(id) {
                    Canvas::Main if *light => light_kbps,
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

/// One-line error message when checking or preparing a platform destination.
fn format_platform_error(p: Platform, err: ferrocast_connector::ConnectorError, write: bool) -> String {
    match err {
        ferrocast_connector::ConnectorError::MissingScope { .. } => {
            format!("{p} needs one more permission: reconnect it in Settings, Accounts.")
        }
        ferrocast_connector::ConnectorError::SignInNeeded(_) => {
            format!("{p} needs you to sign in again in Settings, Accounts.")
        }
        ferrocast_connector::ConnectorError::RateLimited { .. } => {
            format!("{p} is temporarily rate-limited; try again shortly.")
        }
        ferrocast_connector::ConnectorError::Refused { message, .. } if write => {
            format!("{p} refused to start the stream: {message}")
        }
        ferrocast_connector::ConnectorError::Refused { message, .. } => format!("{p}: {message}"),
        e if write => format!("Couldn't prepare {p}: {e}"),
        e => format!("Couldn't check {p}: {e}"),
    }
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
                if now.0 {
                    engine.clear_golive_failures(Canvas::Main);
                } else {
                    g.studio_live.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            if now.1 != was.1 {
                tracing::info!(live = now.1, "Go live: the vertical stream changed");
                if now.1 {
                    engine.clear_golive_failures(Canvas::Vertical);
                }
            }
            was = now;
            engine.reconcile().await;
            continue;
        }
        let k = g.hub.stats().kbps;
        if k > 0 {
            g.last_kbps.store(k, std::sync::atomic::Ordering::Relaxed);
        }
        engine.failure_tick().await;
        engine.backup_tick().await;
        engine.quality_tick().await;
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

/// The reason of the first Reconnecting destination when none of `states` is Live (or paused) and at
/// least one is Reconnecting.
fn reconnecting_reason(states: &[PushState]) -> Option<String> {
    if states.iter().any(|s| matches!(s, PushState::Live | PushState::Paused { .. })) {
        return None;
    }
    states.iter().find_map(|s| match s {
        PushState::Reconnecting { reason, .. } => Some(reason.clone()),
        _ => None,
    })
}

/// The most the stream's health may read while some, but not all, of the platforms it goes to
/// have failed or are reconnecting (the status bar showed 100 with Kick down; the API, a phone
/// and the logs said so too). All of them down is reported as the stream failing instead.
pub(crate) const PARTIAL_FAILURE_HEALTH: u64 = 75;

/// `Some(cap)` for the health score when part of the platforms are down.
pub(crate) fn partial_failure_cap(states: &[PushState]) -> Option<u64> {
    let down = |s: &PushState| matches!(s, PushState::Failed { .. } | PushState::Reconnecting { .. });
    let n = states.iter().filter(|s| down(s)).count();
    (n > 0 && n < states.len()).then_some(PARTIAL_FAILURE_HEALTH)
}

#[cfg(test)]
mod partial_failure_tests {
    use super::*;

    #[test]
    fn one_of_several_platforms_down_caps_the_health() {
        let failed = PushState::Failed { reason: "key refused".into(), attempts: 3, detail: None, refused: true };
        let re = PushState::Reconnecting { attempt: 2, retry_in_ms: 1000, reason: "net".into() };
        assert_eq!(partial_failure_cap(&[PushState::Live, failed.clone()]), Some(75));
        assert_eq!(partial_failure_cap(&[PushState::Live, PushState::Live, re.clone()]), Some(75));
        // All fine, or all down (the stream's own failure path says that), or none: no cap.
        assert_eq!(partial_failure_cap(&[PushState::Live, PushState::Live]), None);
        assert_eq!(partial_failure_cap(&[failed.clone(), re]), None);
        assert_eq!(partial_failure_cap(&[]), None);
    }
}

#[cfg(test)]
mod reconnecting_tests {
    use super::*;

    fn re() -> PushState {
        PushState::Reconnecting { attempt: 3, retry_in_ms: 1000, reason: "Your network is down".into() }
    }

    #[test]
    fn the_studio_is_reconnecting_only_while_no_main_destination_is_live() {
        assert_eq!(reconnecting_reason(&[re()]).as_deref(), Some("Your network is down"));
        assert_eq!(reconnecting_reason(&[re(), PushState::Connecting]).as_deref(), Some("Your network is down"));
        assert!(reconnecting_reason(&[re(), PushState::Live]).is_none(), "one platform is still live");
        assert!(reconnecting_reason(&[PushState::Connecting]).is_none(), "a first connect isn't a reconnect");
        assert!(reconnecting_reason(&[]).is_none());
    }
}
