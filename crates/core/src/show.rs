//! "Start show" and "End show": the streamer's go-live checklist, its pre-flight checks,
//! the scene switcher's rules (chat `!scene`, BRB) and OBS health warnings. Pure config,
//! validation and decisions; the engine talks to OBS and the platforms.

use serde::{Deserialize, Serialize};

/// Scene names as OBS allows them (OBS itself has no hard limit; we keep them sane).
pub fn valid_scene_name(s: &str) -> bool {
    let n = s.chars().count();
    (1..=100).contains(&n) && !s.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowConfig {
    /// Push this stream-info preset to every platform first.
    pub preset_id: Option<String>,
    /// Switch OBS here when the show starts ("Starting Soon").
    pub starting_scene: Option<String>,
    /// Count down this long (10–3600 s), then switch to `live_scene`. 0 = no countdown.
    pub countdown_s: u32,
    pub live_scene: Option<String>,
    /// Resume the alert queue if it was paused.
    pub alerts_on: bool,
    /// Post the Discord go-live message (the Discord settings' template) right away.
    pub discord: bool,
    /// Turn the bot on (commands and timers run while live).
    pub bot_on: bool,
    /// Start OBS's replay buffer so "clip that" works.
    pub replay_buffer: bool,
    /// Press OBS's Start Streaming as the show starts (after the starting scene, so viewers
    /// see it during the countdown). Off by default: an existing show never goes live by itself.
    pub start_stream: bool,
    /// Ending: switch here.
    pub ending_scene: Option<String>,
    /// Ending: press OBS's Stop Streaming `stop_delay_s` after the ending scene.
    pub stop_stream: bool,
    /// 0–600 s, so viewers see the ending scene and the thank-you first.
    pub stop_delay_s: u32,
    /// Ending: post a thank-you in chat (`{thanks}`, `{follows}`, `{subs}`, `{duration}`).
    pub recap_message: Option<String>,
    /// Pre-flight: these OBS audio inputs must not be muted.
    pub mic_sources: Vec<String>,
    /// Pre-flight: these OBS sources must be visible in the live scene.
    pub cam_sources: Vec<String>,
    /// Pre-flight: warn below this much free space where recordings go (GB).
    pub min_disk_gb: u32,
    /// Pre-flight: these platforms should be connected.
    pub platforms: Vec<crate::event::Platform>,
    /// What happens when the countdown reaches zero (each on or off).
    pub at_zero: AtZero,
    /// While live: warn when a mic (`mic_sources`) goes silent, a camera (`cam_sources`) goes
    /// black or freezes, or a destination stops sending.
    pub watch: LiveWatchConfig,
}

/// When the live watch speaks up (each 0: off).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveWatchConfig {
    pub on: bool,
    /// A mic this long without a sound (30 to 1800 s).
    pub mic_silent_s: u32,
    /// A camera this long black (5 to 600 s).
    pub camera_black_s: u32,
    /// A camera this long showing the very same picture (10 to 600 s).
    pub camera_frozen_s: u32,
    /// A destination this long not sending (10 to 600 s).
    pub destination_down_s: u32,
}

impl Default for LiveWatchConfig {
    fn default() -> Self {
        LiveWatchConfig { on: true, mic_silent_s: 120, camera_black_s: 10, camera_frozen_s: 30, destination_down_s: 30 }
    }
}

/// When Start show's countdown reaches zero (owner, 24 Oct: "configurable, each on or off").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AtZero {
    /// Switch OBS to `live_scene`.
    pub switch: bool,
    /// Unmute the pre-flight mics (`mic_sources`).
    pub unmute: bool,
    /// Press OBS's Start Streaming, when it isn't streaming yet.
    pub start_stream: bool,
    /// Press OBS's Start Recording.
    pub record: bool,
    /// Say this in chat on every connected platform.
    pub chat: Option<String>,
    /// Add a "Live" marker.
    pub marker: bool,
}

impl Default for AtZero {
    fn default() -> Self {
        AtZero { switch: true, unmute: false, start_stream: false, record: false, chat: None, marker: true }
    }
}

impl Default for ShowConfig {
    fn default() -> Self {
        ShowConfig {
            preset_id: None,
            starting_scene: None,
            // no countdown until a live scene is chosen, so the default config is valid
            countdown_s: 0,
            live_scene: None,
            alerts_on: true,
            discord: true,
            bot_on: true,
            replay_buffer: false,
            start_stream: false,
            ending_scene: None,
            stop_stream: false,
            stop_delay_s: 30,
            recap_message: Some("Thanks for hanging out! Big love to {thanks}. See you next stream.".into()),
            mic_sources: Vec::new(),
            cam_sources: Vec::new(),
            min_disk_gb: 10,
            platforms: Vec::new(),
            at_zero: AtZero::default(),
            watch: LiveWatchConfig::default(),
        }
    }
}

impl ShowConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (field, s) in [
            ("starting_scene", &self.starting_scene),
            ("live_scene", &self.live_scene),
            ("ending_scene", &self.ending_scene),
        ] {
            if let Some(s) = s
                && !valid_scene_name(s)
            {
                return Err(format!("{field}: a scene name of 1 to 100 characters."));
            }
        }
        if self.countdown_s != 0 && !(10..=3600).contains(&self.countdown_s) {
            return Err("countdown_s: 10 seconds to 1 hour, or 0 for no countdown.".into());
        }
        if let Some(m) = &self.at_zero.chat
            && (m.trim().is_empty() || m.chars().count() > 400 || m.chars().any(char::is_control))
        {
            return Err("at_zero.chat: 1 to 400 characters on one line, or turn it off.".into());
        }
        if self.countdown_s > 0 && self.at_zero.switch && self.live_scene.is_none() {
            return Err("live_scene: pick the scene to switch to when the countdown ends.".into());
        }
        if let Some(m) = &self.recap_message {
            if m.trim().is_empty() || m.chars().count() > 400 {
                return Err("recap_message: 1 to 400 characters, or turn it off.".into());
            }
            crate::template::Template::parse(m).map_err(|e| format!("recap_message: {e}"))?;
        }
        if self.mic_sources.len() > 10 || self.cam_sources.len() > 10 {
            return Err("mic_sources: 10 sources at most each.".into());
        }
        for s in self.mic_sources.iter().chain(&self.cam_sources) {
            if !valid_scene_name(s) {
                return Err("mic_sources: source names are 1 to 100 characters.".into());
            }
        }
        if self.stop_delay_s > 600 {
            return Err("stop_delay_s: 0 to 600 seconds.".into());
        }
        if self.min_disk_gb > 10_000 {
            return Err("min_disk_gb: up to 10000 GB.".into());
        }
        let w = &self.watch;
        for (field, v, range) in [
            ("watch.mic_silent_s", w.mic_silent_s, 30..=1800),
            ("watch.camera_black_s", w.camera_black_s, 5..=600),
            ("watch.camera_frozen_s", w.camera_frozen_s, 10..=600),
            ("watch.destination_down_s", w.destination_down_s, 10..=600),
        ] {
            if v != 0 && !range.contains(&v) {
                return Err(format!("{field}: {} to {} seconds, or 0 for off.", range.start(), range.end()));
            }
        }
        Ok(())
    }
}

/// Quieter than this (dBFS) over a second and a half: a mic that hears nothing.
pub const SILENT_DB: f32 = -60.0;

// ------------------------------------------------------------------ live watch

/// One look at what's going out, every couple of seconds while live (ms since the epoch).
#[derive(Debug, Clone, Default)]
pub struct WatchSample {
    pub at: i64,
    /// Each watched mic's loudest peak since the last sample (dBFS); missing: not known.
    pub mics: Vec<(String, f32)>,
    /// Each watched camera: black, and its picture's fingerprint.
    pub cameras: Vec<(String, bool, u64)>,
    /// Each running destination (its name) and whether it's sending.
    pub destinations: Vec<(String, bool)>,
}

/// A live mistake to tell the streamer about, or that it's fixed (`cleared`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveAlert {
    pub id: String,
    /// `warn` or `error`.
    pub level: &'static str,
    pub message: String,
    pub cleared: bool,
}

/// Since when each thing has been wrong, and what's been said: a pure state machine the
/// engine feeds a sample every couple of seconds while live.
#[derive(Debug, Default)]
pub struct LiveWatch {
    /// The last time each mic heard something, each camera's picture changed (and its
    /// fingerprint), each camera went black, each destination last sent.
    heard: std::collections::HashMap<String, i64>,
    changed: std::collections::HashMap<String, (u64, i64)>,
    dark: std::collections::HashMap<String, i64>,
    sending: std::collections::HashMap<String, i64>,
    /// Alerts raised and not yet cleared.
    raised: std::collections::BTreeSet<String>,
}

impl LiveWatch {
    /// A new stream: everything starts fresh.
    pub fn reset(&mut self) {
        *self = LiveWatch::default();
    }

    /// The alerts raised now (and not cleared).
    pub fn raised(&self) -> Vec<String> {
        self.raised.iter().cloned().collect()
    }

    /// One sample → the alerts to raise and to clear.
    pub fn tick(&mut self, cfg: &LiveWatchConfig, s: &WatchSample) -> Vec<LiveAlert> {
        let mut out = Vec::new();
        if !cfg.on {
            return out;
        }
        let secs = |since: i64| (s.at - since).max(0) / 1000;
        let mut set =
            |out: &mut Vec<LiveAlert>, id: String, bad: bool, level: &'static str, message: String, fixed: String| {
                match (bad, self.raised.contains(&id)) {
                    (true, false) => {
                        self.raised.insert(id.clone());
                        out.push(LiveAlert { id, level, message, cleared: false });
                    }
                    (false, true) => {
                        self.raised.remove(&id);
                        out.push(LiveAlert { id, level, message: fixed, cleared: true });
                    }
                    _ => {}
                }
            };
        for (name, db) in &s.mics {
            let heard = *self.heard.entry(name.clone()).or_insert(s.at);
            let heard = if *db > SILENT_DB { s.at } else { heard };
            self.heard.insert(name.clone(), heard);
            let bad = cfg.mic_silent_s > 0 && secs(heard) >= i64::from(cfg.mic_silent_s);
            set(
                &mut out,
                format!("mic:{name}"),
                bad,
                "error",
                format!(
                    "{name} hasn't heard a sound for {} minutes: is it unplugged or muted on the device?",
                    secs(heard) / 60
                ),
                format!("{name} hears you again."),
            );
        }
        for (name, black, sig) in &s.cameras {
            let dark_since = if *black {
                *self.dark.entry(name.clone()).or_insert(s.at)
            } else {
                self.dark.remove(name);
                s.at
            };
            let changed = match self.changed.get(name) {
                Some((was, at)) if was == sig => *at,
                _ => s.at,
            };
            self.changed.insert(name.clone(), (*sig, changed));
            let black_bad = cfg.camera_black_s > 0 && *black && secs(dark_since) >= i64::from(cfg.camera_black_s);
            set(
                &mut out,
                format!("camera:{name}:black"),
                black_bad,
                "error",
                format!(
                    "{name} has been black for {} s: is the lens covered, or another app using it?",
                    secs(dark_since)
                ),
                format!("{name} has a picture again."),
            );
            let frozen_bad = cfg.camera_frozen_s > 0 && !*black && secs(changed) >= i64::from(cfg.camera_frozen_s);
            set(
                &mut out,
                format!("camera:{name}:frozen"),
                frozen_bad,
                "warn",
                format!("{name} has shown the very same picture for {} s: it may have frozen.", secs(changed)),
                format!("{name} is moving again."),
            );
        }
        for (name, up) in &s.destinations {
            let sent = if *up { s.at } else { *self.sending.entry(name.clone()).or_insert(s.at) };
            self.sending.insert(name.clone(), sent);
            let bad = cfg.destination_down_s > 0 && !*up && secs(sent) >= i64::from(cfg.destination_down_s);
            set(
                &mut out,
                format!("destination:{name}"),
                bad,
                "error",
                format!("{name} hasn't been getting your stream for {} s.", secs(sent)),
                format!("{name} is getting your stream again."),
            );
        }
        out
    }
}

/// One pre-flight check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
    /// How to fix it: a dashboard link (`#/settings/accounts`) or an OBS action.
    pub fix: Option<Fix>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub label: String,
    /// A dashboard route, or `action:<name>` for a one-click fix the dashboard can run
    /// (`action:unmute:<source>`, `action:start_replay_buffer`).
    pub href: String,
}

impl Check {
    pub fn new(id: &str, label: &str, status: CheckStatus, detail: impl Into<String>, fix: Option<Fix>) -> Check {
        Check { id: id.into(), label: label.into(), status, detail: detail.into(), fix }
    }
}

pub fn fix(label: &str, href: impl Into<String>) -> Option<Fix> {
    Some(Fix { label: label.into(), href: href.into() })
}

/// Ready when nothing failed (warnings don't stop the show).
pub fn ready(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.status == CheckStatus::Fail)
}

/// The disk check, from the free space where recordings go (MB).
pub fn disk_check(free_mb: Option<f64>, min_gb: u32) -> Check {
    match free_mb {
        None => Check::new(
            "disk",
            "Disk space",
            CheckStatus::Skip,
            "The Studio isn't started, so free space is unknown.",
            None,
        ),
        Some(mb) => {
            let gb = mb / 1024.0;
            let label = format!("{gb:.0} GB free where recordings go.");
            if gb < 1.0 {
                Check::new(
                    "disk",
                    "Disk space",
                    CheckStatus::Fail,
                    format!("{label} Recordings and replays will fail."),
                    fix("Free up space", "#/studio/golive"),
                )
            } else if gb < f64::from(min_gb) {
                Check::new(
                    "disk",
                    "Disk space",
                    CheckStatus::Warn,
                    format!("{label} Less than the {min_gb} GB you asked for."),
                    None,
                )
            } else {
                Check::new("disk", "Disk space", CheckStatus::Ok, label, None)
            }
        }
    }
}

// ------------------------------------------------------------------ scene switcher

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenesConfig {
    /// Mods may type `!scene <name>` for scenes in `chat_allowed`.
    pub chat_enabled: bool,
    pub chat_allowed: Vec<String>,
    /// Dashboard shortcuts: Alt+1 … Alt+9 switch to these, in order.
    pub hotkeys: Vec<String>,
    /// The BRB scene (`!brb` / the BRB button, and during Twitch ads when `brb_on_ads`).
    pub brb_scene: Option<String>,
    pub brb_on_ads: bool,
    /// While live, BRB by itself when a watched camera (Show's `cam_sources`) has been black for
    /// the live watch's `camera_black_s`, and back when its picture returns.
    pub brb_when_black: bool,
    /// Mute these inputs while on BRB.
    pub brb_mute: Vec<String>,
    /// Mods may type `!clip` to save the replay buffer.
    pub clip_command: bool,
    /// Save the replay buffer by itself on big moments (see `moments`).
    pub clip_on_big_moments: bool,
    /// Each saved clip also as a 1080×1920 copy for Shorts, TikTok and Reels (ffmpeg).
    pub vertical_clips: bool,
    /// Away: while live, the BRB scene after this many minutes with the Show's mics silent,
    /// and back when you talk again (0: off; the Studio only).
    pub away_minutes: u32,
    /// Saying "clip that" while live captions listen saves a clip (the Studio only).
    pub clip_by_voice: bool,
    /// Each saved clip gets a subtitle file (.srt) beside it, and its vertical copy the words
    /// burned in: Whisper on this computer (a downloaded model) and ffmpeg.
    pub clip_captions: bool,
    /// While live captions listen: "be right back", "I'm back", "switch to <scene>", "start
    /// recording" and "stop recording" do that (the Studio only).
    pub voice_commands: bool,
    /// While live captions listen: each finished sentence is kept with its time, on this
    /// computer, to search later.
    pub keep_transcript: bool,
    /// A scene per game: when that app comes to the front, its scene goes on air (the Studio
    /// on a Mac). Empty: off.
    pub game_scenes: Vec<GameScene>,
    /// "When X, do Y": a stream event puts a scene on air, or shows a source, for a while.
    pub event_rules: Vec<EventRule>,
}

/// What sets a rule off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleWhen {
    Follow,
    /// A new or renewed sub (not one of a gifted batch: that's `gift`).
    Sub,
    /// Gifted subs: `min` is how many in the batch.
    Gift,
    /// A raid: `min` is how many viewers.
    Raid,
    /// A tip, a Super Chat or Bits: `min` is the amount (whole units of its currency, or Bits).
    Money,
}

/// A rule: when `when` happens (at least `min`), put `scene` on air, or show `source` in the
/// scene on air, for `seconds`, then back as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRule {
    pub when: RuleWhen,
    #[serde(default)]
    pub min: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub seconds: u32,
}

impl EventRule {
    /// Whether `kind` sets this rule off: its kind, at least `min` of it.
    pub fn fires(&self, kind: &crate::event::EventKind) -> bool {
        use crate::event::{EventKind as K, PaymentStatus};
        let min = u64::from(self.min);
        match (self.when, kind) {
            (RuleWhen::Follow, K::Follow { .. }) => true,
            (RuleWhen::Sub, K::Subscription { gifted_by: None, .. }) => true,
            (RuleWhen::Gift, K::GiftBatch { count, .. }) => u64::from(*count) >= min.max(1),
            (RuleWhen::Raid, K::Raid { viewers, .. }) => u64::from(*viewers) >= min,
            (RuleWhen::Money, K::Tip { money, status, .. }) => {
                *status == PaymentStatus::Captured && money.major() >= min as f64
            }
            (RuleWhen::Money, K::PaidMessage { money, .. }) => money.major() >= min as f64,
            (RuleWhen::Money, K::PlatformGift { value, .. }) => value.amount >= min,
            _ => false,
        }
    }
}

/// An app (by the name the Dock shows) and the scene for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameScene {
    pub app: String,
    pub scene: String,
}

impl Default for ScenesConfig {
    fn default() -> Self {
        ScenesConfig {
            chat_enabled: false,
            chat_allowed: Vec::new(),
            hotkeys: Vec::new(),
            brb_scene: None,
            brb_on_ads: false,
            brb_when_black: false,
            brb_mute: Vec::new(),
            clip_command: true,
            clip_on_big_moments: false,
            vertical_clips: false,
            away_minutes: 0,
            clip_by_voice: false,
            clip_captions: false,
            voice_commands: false,
            keep_transcript: false,
            game_scenes: Vec::new(),
            event_rules: Vec::new(),
        }
    }
}

impl ScenesConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.chat_allowed.len() > 50 {
            return Err("chat_allowed: 50 scenes at most.".into());
        }
        if self.game_scenes.len() > 20 {
            return Err("game_scenes: 20 at most.".into());
        }
        for (i, g) in self.game_scenes.iter().enumerate() {
            for (field, v) in [("app", &g.app), ("scene", &g.scene)] {
                let t = v.trim();
                if t.is_empty() || t.chars().count() > 100 || t.contains('\n') {
                    return Err(format!("game_scenes.{i}.{field}: 1 to 100 characters on one line."));
                }
            }
        }
        if self.event_rules.len() > 20 {
            return Err("event_rules: 20 at most.".into());
        }
        for (i, r) in self.event_rules.iter().enumerate() {
            match (&r.scene, &r.source) {
                (Some(_), Some(_)) | (None, None) => {
                    return Err(format!("event_rules.{i}: a scene or a source, one of them."));
                }
                (Some(n), None) | (None, Some(n)) if !valid_scene_name(n) => {
                    return Err(format!("event_rules.{i}: names are 1 to 100 characters."));
                }
                _ => {}
            }
            if !(1..=600).contains(&r.seconds) {
                return Err(format!("event_rules.{i}.seconds: 1 to 600."));
            }
            if r.min > 1_000_000 {
                return Err(format!("event_rules.{i}.min: 0 to 1000000."));
            }
        }
        if self.away_minutes > 60 {
            return Err("away_minutes: 0 (off) to 60 minutes.".into());
        }
        if self.hotkeys.len() > 9 {
            return Err("hotkeys: 9 at most (Alt+1 to Alt+9).".into());
        }
        if self.brb_mute.len() > 10 {
            return Err("brb_mute: 10 inputs at most.".into());
        }
        for (field, list) in
            [("chat_allowed", &self.chat_allowed), ("hotkeys", &self.hotkeys), ("brb_mute", &self.brb_mute)]
        {
            if list.iter().any(|s| !valid_scene_name(s)) {
                return Err(format!("{field}: names are 1 to 100 characters."));
            }
        }
        if let Some(s) = &self.brb_scene
            && !valid_scene_name(s)
        {
            return Err("brb_scene: a scene name of 1 to 100 characters.".into());
        }
        if self.brb_on_ads && self.brb_scene.is_none() {
            return Err("brb_scene: pick the BRB scene to switch to during ads.".into());
        }
        if self.brb_when_black && self.brb_scene.is_none() {
            return Err("brb_scene: pick the BRB scene to switch to when a camera goes black.".into());
        }
        Ok(())
    }

    /// The allowed scene `asked` names (case-blind, by prefix when it's unique), for `!scene`.
    pub fn chat_scene<'a>(&'a self, asked: &str) -> Result<&'a str, String> {
        let asked = asked.trim();
        if asked.is_empty() {
            return Err(if self.chat_allowed.is_empty() {
                "No scenes are allowed from chat yet (Studio, OBS).".into()
            } else {
                format!("Scenes: {}.", self.chat_allowed.join(", "))
            });
        }
        if let Some(s) = self.chat_allowed.iter().find(|s| s.eq_ignore_ascii_case(asked)) {
            return Ok(s);
        }
        let lower = asked.to_lowercase();
        let matches: Vec<&String> = self.chat_allowed.iter().filter(|s| s.to_lowercase().starts_with(&lower)).collect();
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("\"{}\" isn't a scene mods can switch to.", asked.chars().take(40).collect::<String>())),
            _ => Err(format!("Which one? {}.", matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }
}

// ------------------------------------------------------------------ OBS health

/// One reading from OBS (GetStats plus the stream output's status).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HealthSample {
    pub at: i64,
    pub cpu: f64,
    pub memory_mb: f64,
    pub disk_mb: f64,
    pub fps: f64,
    pub render_ms: f64,
    pub render_skipped: u64,
    pub render_total: u64,
    pub output_skipped: u64,
    pub output_total: u64,
    pub streaming: bool,
    pub reconnecting: bool,
    pub stream_bytes: u64,
    pub stream_skipped: u64,
    pub stream_total: u64,
    pub congestion: f64,
}

/// What the dashboard shows, worked out from two readings a few seconds apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub at: i64,
    pub streaming: bool,
    pub cpu: f64,
    pub memory_mb: f64,
    pub disk_gb: f64,
    pub fps: f64,
    /// % of frames skipped since the last reading: render lag (GPU), encoder lag, network.
    pub render_skipped_pct: f64,
    pub encoder_skipped_pct: f64,
    pub dropped_pct: f64,
    pub bitrate_kbps: f64,
    pub warnings: Vec<HealthWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthWarning {
    pub id: String,
    pub level: String,
    pub message: String,
    pub hint: String,
}

fn pct(skipped: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { (skipped as f64 / total as f64 * 100.0).clamp(0.0, 100.0) }
}

/// Health from the previous and current reading (counters reset when OBS restarts or
/// the stream starts again; a counter going down starts over from zero).
pub fn health(prev: Option<&HealthSample>, cur: &HealthSample) -> Health {
    let delta = |now: u64, before: Option<u64>| match before {
        Some(b) if now >= b => now - b,
        _ => now,
    };
    let p = prev;
    let render =
        pct(delta(cur.render_skipped, p.map(|p| p.render_skipped)), delta(cur.render_total, p.map(|p| p.render_total)));
    let encoder =
        pct(delta(cur.output_skipped, p.map(|p| p.output_skipped)), delta(cur.output_total, p.map(|p| p.output_total)));
    let dropped =
        pct(delta(cur.stream_skipped, p.map(|p| p.stream_skipped)), delta(cur.stream_total, p.map(|p| p.stream_total)));
    let bitrate = match p {
        Some(p) if cur.streaming && p.streaming && cur.at > p.at && cur.stream_bytes >= p.stream_bytes => {
            (cur.stream_bytes - p.stream_bytes) as f64 * 8.0 / ((cur.at - p.at) as f64 / 1000.0) / 1000.0
        }
        _ => 0.0,
    };
    let mut warnings = Vec::new();
    let mut warn = |id: &str, level: &str, message: String, hint: &str| {
        warnings.push(HealthWarning { id: id.into(), level: level.into(), message, hint: hint.into() })
    };
    if cur.reconnecting {
        warn(
            "reconnecting",
            "error",
            "OBS lost the connection to the platform and is reconnecting.".into(),
            "Check your internet. If it keeps happening, lower the bitrate in OBS Settings, Output.",
        );
    }
    if cur.streaming && dropped >= 1.0 {
        warn(
            "dropped",
            if dropped >= 5.0 { "error" } else { "warn" },
            format!("{dropped:.1}% of frames dropped on the way to the platform."),
            "Your upload can't keep up: lower the bitrate in OBS Settings, Output, or use a wired connection.",
        );
    }
    // OBS's encoder-lag counter is global, and extra canvases or outputs (vertical video,
    // multistream plugins) push it near 100% while every real output runs at full rate. So
    // it only counts while streaming, between two readings, when the stream itself fell
    // short of the frame rate.
    let stream_short = match p {
        Some(p) if cur.streaming && p.streaming && cur.at > p.at && cur.fps > 0.0 => {
            let expected = cur.fps * (cur.at - p.at) as f64 / 1000.0;
            (delta(cur.stream_total, Some(p.stream_total)) as f64) < expected * 0.9
        }
        _ => false,
    };
    if encoder >= 1.0 && stream_short {
        warn(
            "encoder",
            if encoder >= 5.0 { "error" } else { "warn" },
            format!("{encoder:.1}% of frames skipped by the encoder."),
            "The encoder is overloaded: pick a faster preset or the hardware encoder (NVENC, AMF, QuickSync, Apple VT).",
        );
    }
    if render >= 1.0 {
        warn(
            "render",
            if render >= 5.0 { "error" } else { "warn" },
            format!("{render:.1}% of frames missed while rendering."),
            "The GPU is busy: cap the game's frame rate, or lower OBS's canvas or FPS.",
        );
    }
    if cur.cpu >= 90.0 {
        warn(
            "cpu",
            "warn",
            format!("OBS is using {:.0}% CPU.", cur.cpu),
            "Close other apps, or switch OBS to a hardware encoder.",
        );
    }
    if cur.disk_mb > 0.0 && cur.disk_mb < 5.0 * 1024.0 {
        warn(
            "disk",
            if cur.disk_mb < 1024.0 { "error" } else { "warn" },
            format!("Only {:.1} GB free where recordings go.", cur.disk_mb / 1024.0),
            "Free up space before recordings or replay saves fail.",
        );
    }
    Health {
        at: cur.at,
        streaming: cur.streaming,
        cpu: (cur.cpu * 10.0).round() / 10.0,
        memory_mb: cur.memory_mb.round(),
        disk_gb: (cur.disk_mb / 1024.0 * 10.0).round() / 10.0,
        fps: (cur.fps * 10.0).round() / 10.0,
        render_skipped_pct: (render * 10.0).round() / 10.0,
        encoder_skipped_pct: (encoder * 10.0).round() / 10.0,
        dropped_pct: (dropped * 10.0).round() / 10.0,
        bitrate_kbps: bitrate.round(),
        warnings,
    }
}

/// `{thanks}`, `{follows}`, `{subs}`, `{duration}` for the end-of-show message.
pub fn recap_vars(thanks: &[String], follows: u64, subs: u64, duration_ms: i64) -> crate::template::Vars {
    let mut v = crate::template::Vars::new();
    let list = match thanks {
        [] => "everyone who stopped by".to_string(),
        [one] => one.clone(),
        many if many.len() <= 8 => {
            format!("{} and {}", many[..many.len() - 1].join(", "), many[many.len() - 1])
        }
        many => format!("{} and {} more", many[..8].join(", "), many.len() - 8),
    };
    v.insert("thanks".into(), list);
    v.insert("follows".into(), follows.to_string());
    v.insert("subs".into(), subs.to_string());
    v.insert("duration".into(), crate::chatbot::long_duration(duration_ms));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_rules_are_checked() {
        let rule = |scene: Option<&str>, source: Option<&str>, seconds: u32| EventRule {
            when: RuleWhen::Raid,
            min: 10,
            scene: scene.map(String::from),
            source: source.map(String::from),
            seconds,
        };
        let cfg = |r: EventRule| ScenesConfig { event_rules: vec![r], ..ScenesConfig::default() };
        assert!(cfg(rule(Some("Raid!"), None, 20)).validate().is_ok());
        assert!(cfg(rule(None, Some("Confetti"), 10)).validate().is_ok());
        assert!(cfg(rule(Some("A"), Some("B"), 10)).validate().is_err(), "one thing to do");
        assert!(cfg(rule(None, None, 10)).validate().is_err(), "something to do");
        assert!(cfg(rule(Some("A"), None, 0)).validate().is_err(), "for a while");
        assert!(cfg(rule(Some("A"), None, 601)).validate().is_err());
        assert!(cfg(rule(Some(""), None, 5)).validate().is_err());
        let many = ScenesConfig { event_rules: vec![rule(Some("A"), None, 5); 21], ..ScenesConfig::default() };
        assert!(many.validate().is_err());
        // What sets each off.
        use crate::event::{Actor, EventKind as K, Platform};
        let actor = || Actor::named(Platform::Twitch, "ana");
        let when =
            |w: RuleWhen, min: u32| EventRule { when: w, min, scene: Some("S".into()), source: None, seconds: 5 };
        assert!(when(RuleWhen::Follow, 0).fires(&K::Follow { actor: actor() }));
        assert!(!when(RuleWhen::Raid, 0).fires(&K::Follow { actor: actor() }));
        let raid = |viewers| K::Raid { from: actor(), viewers };
        assert!(when(RuleWhen::Raid, 10).fires(&raid(10)));
        assert!(!when(RuleWhen::Raid, 10).fires(&raid(9)));
        // As the dashboard sends it.
        let r: EventRule =
            serde_json::from_value(serde_json::json!({ "when": "money", "min": 5, "source": "Rain", "seconds": 8 }))
                .unwrap();
        assert_eq!(r.when, RuleWhen::Money);
    }

    #[test]
    fn game_scenes_are_checked() {
        let g = |app: &str, scene: &str| GameScene { app: app.into(), scene: scene.into() };
        let ok = ScenesConfig { game_scenes: vec![g("Minecraft", "In game")], ..ScenesConfig::default() };
        assert!(ok.validate().is_ok());
        for bad in [g("", "In game"), g("Minecraft", " "), g("a\nb", "In game"), g(&"x".repeat(101), "In game")] {
            let c = ScenesConfig { game_scenes: vec![bad.clone()], ..ScenesConfig::default() };
            assert!(c.validate().is_err(), "{bad:?}");
        }
        let many = ScenesConfig { game_scenes: vec![g("A", "B"); 21], ..ScenesConfig::default() };
        assert!(many.validate().is_err());
        // Saved before game scenes: none, still valid.
        let old: ScenesConfig = serde_json::from_str(r#"{"clip_command": true}"#).unwrap();
        assert!(old.game_scenes.is_empty());
    }

    #[test]
    fn show_config() {
        let mut c = ShowConfig::default();
        assert!(c.validate().is_ok(), "the default config saves as it is");
        c.countdown_s = 300;
        assert!(c.validate().unwrap_err().starts_with("live_scene"), "a countdown needs a live scene");
        c.live_scene = Some("Gameplay".into());
        assert!(c.validate().is_ok());
        c.countdown_s = 5;
        assert!(c.validate().unwrap_err().starts_with("countdown_s"));
        c.countdown_s = 0;
        c.live_scene = None;
        assert!(c.validate().is_ok());
        c.starting_scene = Some("".into());
        assert!(c.validate().unwrap_err().starts_with("starting_scene"));
        c.starting_scene = Some("Starting\nSoon".into());
        assert!(c.validate().is_err());
        c.starting_scene = None;
        c.recap_message = Some("{thanks".into());
        assert!(c.validate().unwrap_err().starts_with("recap_message"));
        c.recap_message = None;
        c.mic_sources = vec!["Mic".into(); 11];
        assert!(c.validate().is_err());
    }

    #[test]
    fn disk_and_ready() {
        assert_eq!(disk_check(None, 10).status, CheckStatus::Skip);
        assert_eq!(disk_check(Some(500.0), 10).status, CheckStatus::Fail);
        assert_eq!(disk_check(Some(5.0 * 1024.0), 10).status, CheckStatus::Warn);
        assert_eq!(disk_check(Some(50.0 * 1024.0), 10).status, CheckStatus::Ok);
        let warn = Check::new("a", "A", CheckStatus::Warn, "", None);
        let fail = Check::new("b", "B", CheckStatus::Fail, "", None);
        assert!(ready(std::slice::from_ref(&warn)));
        assert!(!ready(&[warn, fail]));
    }

    #[test]
    fn scene_switcher() {
        let mut c = ScenesConfig::default();
        assert!(c.validate().is_ok());
        assert!(c.chat_scene("x").is_err());
        assert!(c.chat_scene("").unwrap_err().contains("No scenes"));
        c.chat_allowed = vec!["Gameplay".into(), "Just Chatting".into(), "Game Over".into()];
        assert_eq!(c.chat_scene("gameplay"), Ok("Gameplay"));
        assert_eq!(c.chat_scene("just"), Ok("Just Chatting"));
        assert!(c.chat_scene("ga").unwrap_err().starts_with("Which one?"));
        assert!(c.chat_scene("secret").unwrap_err().contains("isn't a scene"));
        assert!(c.chat_scene("").unwrap_err().starts_with("Scenes:"));
        c.brb_on_ads = true;
        assert!(c.validate().unwrap_err().starts_with("brb_scene"));
        c.brb_scene = Some("BRB".into());
        assert!(c.validate().is_ok());
        c.hotkeys = vec!["A".into(); 10];
        assert!(c.validate().unwrap_err().starts_with("hotkeys"));
    }

    fn sample(at: i64, sent: u64, skipped: u64, total: u64, bytes: u64) -> HealthSample {
        HealthSample {
            at,
            cpu: 20.0,
            disk_mb: 200_000.0,
            streaming: true,
            stream_skipped: skipped,
            stream_total: total,
            stream_bytes: bytes,
            output_total: sent,
            render_total: sent,
            ..Default::default()
        }
    }

    #[test]
    fn health_from_two_readings() {
        let a = sample(0, 300, 0, 300, 0);
        let b = sample(5_000, 600, 6, 600, 3_750_000);
        let h = health(Some(&a), &b);
        assert_eq!(h.dropped_pct, 2.0);
        assert_eq!(h.bitrate_kbps, 6000.0);
        assert_eq!(h.warnings.len(), 1);
        assert_eq!(h.warnings[0].id, "dropped");
        assert_eq!(h.warnings[0].level, "warn");
        let healthy = health(Some(&a), &sample(5_000, 600, 0, 600, 3_750_000));
        assert!(healthy.warnings.is_empty());
        let first = health(None, &b);
        assert_eq!(first.dropped_pct, 1.0, "no earlier reading: since the start");
        assert_eq!(first.bitrate_kbps, 0.0);
        // OBS restarted: counters went down; never a negative or huge percentage.
        let restarted = health(Some(&b), &sample(10_000, 10, 0, 10, 100));
        assert_eq!(restarted.dropped_pct, 0.0);
        assert_eq!(restarted.bitrate_kbps, 0.0);
        // The stream sent 240 of the 300 frames 60 fps asks for in 5 s: the encoder lag is real.
        let mut bad = sample(5_000, 600, 0, 540, 0);
        bad.fps = 60.0;
        bad.cpu = 97.0;
        bad.disk_mb = 600.0;
        bad.reconnecting = true;
        bad.output_skipped = 60;
        bad.render_skipped = 3;
        let h = health(Some(&a), &bad);
        let ids: Vec<&str> = h.warnings.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, vec!["reconnecting", "encoder", "render", "cpu", "disk"]);
        assert!(h.warnings.iter().all(|w| !w.hint.is_empty()));
        assert_eq!(h.warnings[1].level, "error", "20% encoder lag");
    }

    #[test]
    fn encoder_lag_counts_only_when_the_stream_itself_falls_short() {
        // Tonight's real case: multistream outputs and a vertical canvas made OBS report 98%
        // encoder lag while the stream sent all 300 frames in 5 s.
        let a = sample(0, 300, 0, 300, 0);
        let mut b = sample(5_000, 600, 0, 600, 0);
        b.fps = 60.0;
        b.output_skipped = 290;
        assert!(health(Some(&a), &b).warnings.iter().all(|w| w.id != "encoder"));
        // Not streaming, or no earlier reading: lifetime counters say nothing about now.
        let mut idle = b.clone();
        idle.streaming = false;
        assert!(health(Some(&a), &idle).warnings.iter().all(|w| w.id != "encoder"));
        assert!(health(None, &b).warnings.iter().all(|w| w.id != "encoder"));
    }

    #[test]
    fn recap_message_names() {
        let v = recap_vars(&[], 0, 0, 0);
        assert_eq!(v["thanks"], "everyone who stopped by");
        let names: Vec<String> = ["Asha", "Bo", "Cy"].iter().map(|s| s.to_string()).collect();
        assert_eq!(recap_vars(&names, 1, 2, 3_600_000)["thanks"], "Asha, Bo and Cy");
        assert_eq!(recap_vars(&names, 1, 2, 3_600_000)["duration"], "1 hour");
        let many: Vec<String> = (0..12).map(|i| format!("n{i}")).collect();
        assert!(recap_vars(&many, 0, 0, 0)["thanks"].ends_with("and 4 more"));
    }
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    fn at(s: i64) -> i64 {
        1_000_000 + s * 1000
    }

    #[test]
    fn a_silent_mic_a_black_or_frozen_camera_and_a_down_destination_are_said_once_then_cleared() {
        let cfg = LiveWatchConfig::default();
        let mut w = LiveWatch::default();
        let sample = |t: i64, db: f32, black: bool, sig: u64, up: bool| WatchSample {
            at: at(t),
            mics: vec![("Mic".into(), db)],
            cameras: vec![("Cam".into(), black, sig)],
            destinations: vec![("Twitch".into(), up)],
        };
        // All fine, the picture moving.
        for t in 0..10 {
            assert!(w.tick(&cfg, &sample(t * 2, -20.0, false, t as u64, true)).is_empty());
        }
        // The camera goes black; 10 s later it's said, once.
        let mut said = Vec::new();
        for t in 10..20 {
            said.extend(w.tick(&cfg, &sample(t * 2, -20.0, true, 0, true)));
        }
        assert_eq!(said.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["camera:Cam:black"]);
        assert!(!said[0].cleared);
        // Back, moving: cleared.
        let back = w.tick(&cfg, &sample(40, -20.0, false, 99, true));
        assert!(back.iter().any(|a| a.id == "camera:Cam:black" && a.cleared));
        // Frozen (not black) for 30 s.
        let mut said = Vec::new();
        for t in 21..40 {
            said.extend(w.tick(&cfg, &sample(t * 2, -20.0, false, 7, true)));
        }
        assert!(said.iter().any(|a| a.id == "camera:Cam:frozen" && !a.cleared), "{said:?}");
        // The mic silent for 2 minutes, the destination down for 30 s.
        let mut said = Vec::new();
        for t in 40..110 {
            said.extend(w.tick(&cfg, &sample(t * 2, -90.0, false, t as u64, t < 80)));
        }
        let ids: Vec<&str> = said.iter().filter(|a| !a.cleared).map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"mic:Mic") && ids.contains(&"destination:Twitch"), "{ids:?}");
        assert_eq!(w.raised(), ["destination:Twitch", "mic:Mic"]);
        // Off: nothing said.
        let off = LiveWatchConfig { on: false, ..Default::default() };
        assert!(LiveWatch::default().tick(&off, &sample(0, -90.0, true, 0, false)).is_empty());
    }

    #[test]
    fn watch_times_are_checked() {
        let mut c = ShowConfig::default();
        c.watch.mic_silent_s = 5;
        assert!(c.validate().unwrap_err().contains("watch.mic_silent_s"));
        c.watch.mic_silent_s = 0;
        assert!(c.validate().is_ok());
    }
}
