//! Your voice while live captions listen (29–30 Oct): "clip that" saves a clip as `!clip` does
//! (the desktop app counts the times it heard it; this watches the count), voice commands do
//! what they say, and finished sentences go to the transcript when it's kept.

use std::time::Duration;

use crate::Engine;

use super::obs_tools::CLIP_COOLDOWN_S;
use super::rig::Rig;
use super::transcript::Line;
use super::voice_commands::{Command, commands, scene_command};

/// What a new count asks for: nothing the first time it's read (a count from before isn't a
/// new ask), else a clip when it went up.
#[derive(Debug, Default)]
pub(crate) struct Heard {
    last: Option<u64>,
}

impl Heard {
    pub(crate) fn asks(&mut self, count: Option<u64>) -> bool {
        let Some(n) = count else {
            self.last = None;
            return false;
        };
        let asks = self.last.is_some_and(|l| n > l);
        self.last = Some(n);
        asks
    }
}

/// What recent sentences already did (by their number), so partial results repeating a
/// command, and the same sentence coming back finished, don't do it again.
#[derive(Debug, Default)]
struct Acted {
    done: std::collections::VecDeque<(u64, Vec<Command>)>,
    /// The sentence being said, its words last read, and since when they haven't changed.
    sentence: u64,
    words: String,
    still_since: Option<std::time::Instant>,
}

impl Acted {
    /// True the first time `c` comes up in sentence `id`.
    fn first(&mut self, id: u64, c: &Command) -> bool {
        if let Some((_, list)) = self.done.iter_mut().find(|(i, _)| *i == id) {
            if list.contains(c) {
                return false;
            }
            list.push(c.clone());
        } else {
            self.done.push_back((id, vec![c.clone()]));
            while self.done.len() > 16 {
                self.done.pop_front();
            }
        }
        true
    }
}

/// A scene's name is read once the words have been still this long (or the sentence ended).
const STILL: Duration = Duration::from_millis(1400);

pub(crate) async fn run(engine: Engine) {
    let mut heard = Heard::default();
    let mut acted = Acted::default();
    // Sentences finished before now aren't kept (captions may have been on without it).
    let mut kept_until = crate::now().0;
    loop {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let cfg = engine.scenes_config();
        if !cfg.clip_by_voice && !cfg.voice_commands && !cfg.keep_transcript {
            heard.asks(None);
            kept_until = crate::now().0;
            continue;
        }
        let v = match engine.studio_captions(None).await {
            Ok(v) if v["on"] == true => v,
            _ => {
                heard.asks(None);
                continue;
            }
        };
        // The transcript: sentences finished since the last look.
        let fresh_ids: Vec<(u64, Line)> = v["done"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some((d["id"].as_u64()?, Line { at: d["at"].as_i64()?, text: d["text"].as_str()?.to_string() }))
            })
            .filter(|(_, l)| l.at > kept_until)
            .collect();
        let fresh: Vec<Line> = fresh_ids.iter().map(|(_, l)| l.clone()).collect();
        if let Some(last) = fresh.iter().map(|l| l.at).max() {
            kept_until = last;
        }
        if cfg.keep_transcript
            && !fresh.is_empty()
            && let Some(dir) = engine.transcript_dir()
        {
            let lines = fresh.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(err) = super::transcript::keep(&dir, &lines) {
                    tracing::warn!(%err, "the transcript couldn't be saved");
                }
            })
            .await;
        }
        // "clip that".
        if cfg.clip_by_voice
            && heard.asks(v["clips_said"].as_u64())
            && engine.studio_cooldown("voice:clip", CLIP_COOLDOWN_S)
        {
            match engine.clip_that(Some("Clip by voice")).await {
                Ok(_) => tracing::info!("clip by voice saved"),
                Err(err) => tracing::warn!(%err, "clip by voice didn't save"),
            }
        }
        if cfg.voice_commands {
            let id = v["sentence"]["id"].as_u64().unwrap_or(0);
            let said = v["sentence"]["said"].as_str().unwrap_or("").to_string();
            voice_commands(&engine, &mut acted, id, &said, &fresh_ids).await;
        }
    }
}

/// Does what this sentence asks, once each; a scene once its name has stopped changing (or in
/// a sentence just finished).
async fn voice_commands(engine: &Engine, acted: &mut Acted, id: u64, said: &str, finished: &[(u64, Line)]) {
    let Ok(Rig::Studio) = engine.rig().await else { return };
    if acted.sentence != id {
        acted.sentence = id;
        acted.words.clear();
        acted.still_since = None;
    }
    if acted.words != said {
        acted.words = said.to_string();
        acted.still_since = Some(std::time::Instant::now());
    }
    let mut asked: Vec<(u64, Command)> = commands(said).into_iter().map(|c| (id, c)).collect();
    let still = acted.still_since.is_some_and(|t| t.elapsed() >= STILL);
    let scenes = engine.rig_scenes(&Rig::Studio).await.map(|(_, s)| s).unwrap_or_default();
    if still && let Some(c) = scene_command(said, &scenes) {
        asked.push((id, c));
    }
    // A sentence that just ended between looks: its commands, scenes too (once each: what it
    // already did while being said counts, by its number).
    for (n, l) in finished {
        asked.extend(commands(&l.text).into_iter().map(|c| (*n, c)));
        asked.extend(scene_command(&l.text, &scenes).map(|c| (*n, c)));
    }
    for (n, c) in asked {
        if !acted.first(n, &c) {
            continue;
        }
        if !engine.studio_cooldown(&format!("voice:{c:?}"), 5) {
            continue;
        }
        let cfg = engine.scenes_config();
        let done = match &c {
            Command::Brb if cfg.brb_scene.is_some() => engine.set_brb(true).await.map(|_| ()),
            Command::Back => {
                let on_air = engine.rig_scenes(&Rig::Studio).await.ok().and_then(|(c, _)| c);
                if on_air.is_some() && on_air == cfg.brb_scene {
                    engine.set_brb(false).await.map(|_| ())
                } else {
                    Ok(())
                }
            }
            Command::RecordOn => engine.studio_record(true).await.map(|_| ()),
            Command::RecordOff => engine.studio_record(false).await.map(|_| ()),
            Command::Scene(s) => engine.rig_switch(&Rig::Studio, s).await,
            Command::Brb => Ok(()),
        };
        match done {
            Ok(()) => tracing::info!(command = ?c, "voice command done"),
            Err(err) => tracing::warn!(command = ?c, %err, "voice command didn't work"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_does_each_command_once_even_when_it_comes_back_finished() {
        let mut a = Acted::default();
        assert!(a.first(7, &Command::RecordOn));
        // Heard again while being said, then again finished: done already.
        assert!(!a.first(7, &Command::RecordOn));
        assert!(!a.first(7, &Command::RecordOn));
        // Another command in it, or the same in the next sentence, still counts.
        assert!(a.first(7, &Command::Brb));
        assert!(a.first(8, &Command::RecordOn));
        // Only recent sentences are remembered.
        for n in 100..120 {
            a.first(n, &Command::Back);
        }
        assert!(a.done.len() <= 16);
    }

    #[test]
    fn only_a_new_ask_clips() {
        let mut h = Heard::default();
        // Said before we looked: not a new ask.
        assert!(!h.asks(Some(3)));
        assert!(!h.asks(Some(3)));
        assert!(h.asks(Some(4)));
        assert!(!h.asks(Some(4)));
        // Two at once is one clip (the cooldown would stop the second anyway).
        assert!(h.asks(Some(6)));
        // Captions off forgets the count; back on starts fresh.
        assert!(!h.asks(None));
        assert!(!h.asks(Some(9)));
        assert!(h.asks(Some(10)));
    }
}
