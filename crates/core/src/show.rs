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
        Ok(())
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
}

impl Default for ScenesConfig {
    fn default() -> Self {
        ScenesConfig {
            chat_enabled: false,
            chat_allowed: Vec::new(),
            hotkeys: Vec::new(),
            brb_scene: None,
            brb_on_ads: false,
            brb_mute: Vec::new(),
            clip_command: true,
            clip_on_big_moments: false,
            vertical_clips: false,
            away_minutes: 0,
            clip_by_voice: false,
        }
    }
}

impl ScenesConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.chat_allowed.len() > 50 {
            return Err("chat_allowed: 50 scenes at most.".into());
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
