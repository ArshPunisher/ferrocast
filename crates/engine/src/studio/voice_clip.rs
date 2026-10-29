//! "Clip that" by voice (plan item 6, 29 Oct): while live captions listen (the Studio's own
//! speech recognition, on the Mac), saying "clip that" saves a clip as `!clip` does. The
//! desktop app counts the times it heard it; this watches the count.

use std::time::Duration;

use crate::Engine;

use super::obs_tools::CLIP_COOLDOWN_S;

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

pub(crate) async fn run(engine: Engine) {
    let mut heard = Heard::default();
    loop {
        tokio::time::sleep(Duration::from_millis(700)).await;
        if !engine.scenes_config().clip_by_voice {
            heard.asks(None);
            continue;
        }
        let count = match engine.studio_captions(None).await {
            Ok(v) if v["on"] == true => v["clips_said"].as_u64(),
            _ => None,
        };
        if !heard.asks(count) {
            continue;
        }
        if !engine.studio_cooldown("voice:clip", CLIP_COOLDOWN_S) {
            continue;
        }
        match engine.clip_that(Some("Clip by voice")).await {
            Ok(_) => tracing::info!("clip by voice saved"),
            Err(err) => tracing::warn!(%err, "clip by voice didn't save"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
