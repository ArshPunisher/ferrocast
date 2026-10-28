//! Music for the stream screens (owner, 25 Oct: music pieces that move with the beat and show
//! the song, only while music plays).
//!
//! Two things are read, each only when Settings › Music asks for it:
//! - the beat: how loud OBS hears one audio source (its volume meters, on a connection of
//!   their own, about 20 readings a second);
//! - the song: what Spotify or Music plays (macOS), the system's media player (Windows,
//!   Linux), or the first lines of a text file another app writes.
//!
//! Overlays with a music, motion or sound layer get `music` frames (API.md §6), at most 15 a
//! second and only when something changed. Song titles come from other apps, so overlays set
//! them as text only.

use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use ferrocast_store::repo;
use serde::{Deserialize, Serialize};

use crate::frames::{Audience, OverlayFrame};
use crate::{Engine, EngineError};

pub(crate) mod song;

pub const MUSIC_KEY: &str = "music";
/// Louder than this counts as music playing (about -46 dB).
const LOUD: f32 = 0.08;
/// Music counts as playing this long after the last loud reading (quiet bars in a song).
const LOUD_HOLD: Duration = Duration::from_millis(2500);
/// At most this often to overlays.
const FRAME_GAP: Duration = Duration::from_millis(66);
const SONG_EVERY: Duration = if cfg!(windows) { Duration::from_secs(5) } else { Duration::from_secs(3) };
const RETRY: Duration = Duration::from_secs(5);
const MAX_NAME: usize = 256;
const MAX_PATH: usize = 1024;

/// Where the song's name comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongSource {
    /// No song name.
    #[default]
    Off,
    /// Spotify or Music (macOS), or the system's media player (Windows, Linux).
    Player,
    /// A text file another app writes (Snip, Tuna…): the first line, and the second as artist.
    File,
}

/// Settings › Music.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MusicConfig {
    /// The OBS audio source the beat is read from (for example "Desktop Audio"); none = no beat.
    pub beat_input: Option<String>,
    pub song: SongSource,
    /// The text file, when `song` is `file`.
    pub file: Option<String>,
}

impl MusicConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(n) = &self.beat_input
            && (n.trim().is_empty() || n.chars().count() > MAX_NAME || n.chars().any(char::is_control))
        {
            return Err(format!("beat_input: the name of an OBS audio source, up to {MAX_NAME} letters."));
        }
        if let Some(f) = &self.file {
            let p = std::path::Path::new(f);
            if f.chars().count() > MAX_PATH || f.chars().any(char::is_control) || !p.is_absolute() {
                return Err("file: pick the text file with its full path.".into());
            }
            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("txt")) {
                return Err("file: pick a .txt file (what Snip or Tuna write).".into());
            }
        }
        if self.song == SongSource::File && self.file.is_none() {
            return Err("file: pick the text file the song's name is in.".into());
        }
        Ok(())
    }
}

/// The music right now, as overlays get it.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct MusicNow {
    pub playing: bool,
    /// 0 to 1, how loud it looks, to two places (an f64, so JSON says 0.34, not 0.3400000035).
    pub level: f64,
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// What a player or file says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Song {
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The player says it's playing (`None`: it can't say, as with a file).
    pub playing: Option<bool>,
}

/// `GET /music`: the settings, what's playing, and what can't be read (with what to do).
#[derive(Debug, Clone, Serialize)]
pub struct MusicView {
    pub config: MusicConfig,
    pub now: MusicNow,
    pub beat_problem: Option<String>,
    pub song_problem: Option<String>,
}

#[derive(Debug, Default)]
struct Live {
    level: f32,
    loud_at: Option<Instant>,
    song: Song,
    sent: Option<(Instant, MusicNow)>,
    beat_problem: Option<String>,
    song_problem: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Music {
    cfg: RwLock<MusicConfig>,
    live: Mutex<Live>,
    /// The settings changed: readers start over with them.
    wake_beat: tokio::sync::Notify,
    wake_song: tokio::sync::Notify,
}

/// Whether music plays, from what the player says and how loud OBS hears it.
fn playing(cfg: &MusicConfig, live: &Live, at: Instant) -> bool {
    let loud = live.loud_at.is_some_and(|t| at.duration_since(t) < LOUD_HOLD);
    match cfg.song {
        SongSource::Player => live.song.playing == Some(true) || loud,
        // A file can't say it's paused: OBS's ears decide when there are any.
        SongSource::File if cfg.beat_input.is_none() => live.song.title.is_some(),
        _ => loud,
    }
}

fn now_of(cfg: &MusicConfig, live: &Live, at: Instant) -> MusicNow {
    let on = playing(cfg, live, at);
    MusicNow {
        playing: on,
        level: if on { (f64::from(live.level) * 100.0).round() / 100.0 } else { 0.0 },
        title: live.song.title.clone(),
        artist: live.song.artist.clone(),
    }
}

impl Engine {
    fn music(&self) -> &Music {
        &self.inner.music
    }

    pub(crate) async fn load_music(&self) {
        match self.db(|s| repo::settings::get_or_default::<MusicConfig>(s, MUSIC_KEY)).await {
            Ok(cfg) => *self.music().cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg,
            Err(err) => tracing::warn!(%err, "music settings couldn't be read"),
        }
    }

    pub fn music_config(&self) -> MusicConfig {
        self.music().cfg.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// What overlays get about the music (snapshots and `music` frames).
    pub fn music_now(&self) -> MusicNow {
        let cfg = self.music_config();
        let live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
        now_of(&cfg, &live, Instant::now())
    }

    pub fn music_view(&self) -> MusicView {
        let cfg = self.music_config();
        let live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
        MusicView {
            now: now_of(&cfg, &live, Instant::now()),
            beat_problem: live.beat_problem.clone(),
            song_problem: live.song_problem.clone(),
            config: cfg,
        }
    }

    pub async fn patch_music_config(
        &self,
        patch: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<MusicView, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut cfg = crate::patch::merge(&self.music_config(), patch)?;
        for s in [&mut cfg.beat_input, &mut cfg.file] {
            if s.as_deref().is_some_and(|v| v.trim().is_empty()) {
                *s = None;
            }
        }
        cfg.validate().map_err(EngineError::Invalid)?;
        crate::studio::save_setting(self, MUSIC_KEY, &cfg).await?;
        *self.music().cfg.write().unwrap_or_else(|p| p.into_inner()) = cfg;
        {
            let mut live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
            live.beat_problem = None;
            live.song_problem = None;
        }
        self.music().wake_beat.notify_one();
        self.music().wake_song.notify_one();
        self.push_music(true);
        Ok(self.music_view())
    }

    /// Sends a `music` frame when something changed: at once for the song or playing,
    /// at most 15 a second for the loudness.
    fn push_music(&self, force: bool) {
        let cfg = self.music_config();
        let frame = {
            let mut live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
            let at = Instant::now();
            let now = now_of(&cfg, &live, at);
            let send = match &live.sent {
                None => true,
                Some((when, last)) => {
                    force
                        || last.playing != now.playing
                        || last.title != now.title
                        || last.artist != now.artist
                        || (at.duration_since(*when) >= FRAME_GAP && (last.level - now.level).abs() >= 0.01)
                }
            };
            if !send {
                return;
            }
            live.sent = Some((at, now.clone()));
            now
        };
        self.send_overlay(OverlayFrame::new(Audience::Music, 0, "music", &frame));
    }

    fn set_level(&self, level: f32) {
        {
            let mut live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
            live.level = level;
            if level > LOUD {
                live.loud_at = Some(Instant::now());
            }
            live.beat_problem = None;
        }
        self.push_music(false);
    }

    fn set_problem(&self, beat: bool, why: Option<String>) {
        let mut live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
        if beat {
            live.beat_problem = why;
        } else {
            live.song_problem = why;
        }
    }

    fn set_song(&self, song: Song) {
        {
            let mut live = self.music().live.lock().unwrap_or_else(|p| p.into_inner());
            if live.song == song {
                return;
            }
            live.song = song;
        }
        self.push_music(false);
    }

    /// Reads the beat while Settings › Music names a sound (the Studio's while it runs, else
    /// OBS's); starts over when it changes.
    async fn beat_loop(&self) {
        loop {
            let Some(input) = self.music_config().beat_input else {
                self.set_level(0.0);
                self.music().wake_beat.notified().await;
                continue;
            };
            if self.studio_status().await.running {
                self.studio_beat(&input).await;
                continue;
            }
            let target = match self.obs_target().await {
                Ok(t) => t,
                Err(e) => {
                    self.set_problem(true, Some(e.to_string()));
                    self.wait_or_wake(true).await;
                    continue;
                }
            };
            let mut meters = match ferrocast_obs::meters::connect(&target.cfg, target.password.as_deref()).await {
                Ok(m) => m,
                Err(e) => {
                    self.set_problem(true, Some(e.to_string()));
                    self.wait_or_wake(true).await;
                    continue;
                }
            };
            loop {
                tokio::select! {
                    reading = meters.next(&input) => match reading {
                        Some(peak) => self.set_level(ferrocast_obs::meters::loudness(peak)),
                        None => {
                            // OBS closed: quiet until it's back.
                            self.set_level(0.0);
                            break;
                        }
                    },
                    () = self.music().wake_beat.notified() => break,
                }
            }
        }
    }

    /// The Studio's meter for `input`, read 20 times a second (as often as OBS sends its
    /// meters) until the setting changes or the Studio stops.
    async fn studio_beat(&self, input: &str) {
        // An earlier "OBS isn't running" no longer applies.
        self.set_problem(true, None);
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = self.music().wake_beat.notified() => return,
            }
            let act = crate::studio_host::StudioAct::Levels { names: vec![input.to_string()] };
            let Ok(v) = self.studio_act(act).await else {
                // Stopped (or the sound is gone): quiet, then see what runs now.
                self.set_level(0.0);
                self.wait_or_wake(true).await;
                return;
            };
            let db = v
                .pointer(&format!("/{}/peak", input.replace('~', "~0").replace('/', "~1")))
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(serde_json::Value::as_f64).fold(-100.0_f64, f64::max))
                .unwrap_or(-100.0);
            #[allow(clippy::cast_possible_truncation)]
            let peak = 10f64.powf(db / 20.0) as f32;
            self.set_level(ferrocast_obs::meters::loudness(peak));
        }
    }

    async fn wait_or_wake(&self, beat: bool) {
        let wake = if beat { &self.music().wake_beat } else { &self.music().wake_song };
        tokio::select! {
            () = tokio::time::sleep(RETRY) => {}
            () = wake.notified() => {}
        }
    }

    /// Reads the song every few seconds while Settings › Music asks for one.
    async fn song_loop(&self) {
        loop {
            let cfg = self.music_config();
            let got = match cfg.song {
                SongSource::Off => Ok(Song::default()),
                SongSource::Player => song::from_player().await,
                SongSource::File => match &cfg.file {
                    Some(f) => song::from_file(std::path::Path::new(f)).await,
                    None => Ok(Song::default()),
                },
            };
            match got {
                Ok(s) => {
                    self.set_problem(false, None);
                    self.set_song(s);
                }
                Err(why) => {
                    self.set_problem(false, Some(why));
                    self.set_song(Song::default());
                }
            }
            if cfg.song == SongSource::Off {
                self.music().wake_song.notified().await;
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(SONG_EVERY) => {}
                () = self.music().wake_song.notified() => {}
            }
        }
    }
}

/// Starts the music readers (they sleep while Settings › Music asks for nothing).
pub(crate) async fn run(engine: Engine) {
    let beat = engine.clone();
    tokio::spawn(async move { beat.beat_loop().await });
    // Loudness can fall silent without a new reading (OBS stops sending an idle source):
    // a slow tick lets "playing" end on time.
    let tick = engine.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            tick.push_music(false);
        }
    });
    engine.song_loop().await;
}

#[cfg(test)]
#[path = "music_tests.rs"]
mod engine_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_checked() {
        assert!(MusicConfig::default().validate().is_ok());
        let ok = MusicConfig { beat_input: Some("Desktop Audio".into()), song: SongSource::Player, file: None };
        assert!(ok.validate().is_ok());
        let bad = [
            MusicConfig { beat_input: Some("  ".into()), ..Default::default() },
            MusicConfig { beat_input: Some("a\nb".into()), ..Default::default() },
            MusicConfig { song: SongSource::File, ..Default::default() },
            MusicConfig { file: Some("song.txt".into()), ..Default::default() },
            MusicConfig { file: Some("/etc/passwd".into()), ..Default::default() },
        ];
        for b in bad {
            let e = b.validate().unwrap_err();
            assert!(e.ends_with('.'), "{e}");
        }
    }

    #[test]
    fn playing_follows_the_player_or_the_loudness() {
        let at = Instant::now();
        let mut live = Live::default();
        let player = MusicConfig { song: SongSource::Player, ..Default::default() };
        assert!(!playing(&player, &live, at));
        live.song.playing = Some(true);
        assert!(playing(&player, &live, at), "the player says so");
        live.song.playing = Some(false);
        live.loud_at = Some(at);
        assert!(playing(&player, &live, at), "OBS hears music from somewhere else");
        assert!(!playing(&player, &live, at + LOUD_HOLD), "quiet long enough: stopped");

        let file = MusicConfig { song: SongSource::File, file: Some("/x/now.txt".into()), beat_input: None };
        let mut live = Live::default();
        assert!(!playing(&file, &live, at));
        live.song.title = Some("Midnight City".into());
        assert!(playing(&file, &live, at), "a file with a song and nothing to hear by");
        let file_beat = MusicConfig { beat_input: Some("Desktop Audio".into()), ..file };
        assert!(!playing(&file_beat, &live, at), "with a beat source, silence means stopped");

        let n = now_of(&file_beat, &Live { level: 0.7, ..Default::default() }, at);
        assert_eq!(n.level, 0.0, "no level while not playing");
    }
}
