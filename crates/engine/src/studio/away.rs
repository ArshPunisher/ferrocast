//! Away (plan item 9, 29 Oct): while live on the Studio, the BRB scene after a few minutes of
//! silence on the Show's mics, and back to where you were once you talk again. Read once a
//! second from the mics' meters; anything you switch yourself wins.

use std::time::Duration;

use serde_json::Value;

use super::rig::Rig;
use crate::studio_host::StudioAct;
use crate::{Engine, now};

/// Louder than this on any mic is talking (a quiet room stays well under it).
pub const TALKING_DBFS: f64 = -40.0;
/// Seconds of talking that bring you back (a cough or a door doesn't).
pub const BACK_AFTER: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Stay,
    GoAway,
    Back,
}

/// What a second of listening decides. `brb`: the BRB scene is on air now.
#[derive(Debug, Default)]
pub struct Away {
    quiet_since: Option<i64>,
    talking_for: u32,
    /// BRB was put on by this, so this may take it off (one you put on stays).
    ours: bool,
}

impl Away {
    pub fn step(&mut self, at_ms: i64, talking: bool, live: bool, minutes: u32, brb: bool) -> Step {
        if !live || minutes == 0 {
            *self = Away::default();
            return Step::Stay;
        }
        // BRB taken off by hand (or a scene switched): it's not ours to undo any more.
        if self.ours && !brb {
            self.ours = false;
        }
        if talking {
            self.quiet_since = None;
            self.talking_for += 1;
            if self.ours && brb && self.talking_for >= BACK_AFTER {
                self.ours = false;
                return Step::Back;
            }
            return Step::Stay;
        }
        self.talking_for = 0;
        let since = *self.quiet_since.get_or_insert(at_ms);
        if !brb && at_ms - since >= i64::from(minutes) * 60_000 {
            self.ours = true;
            self.quiet_since = None;
            return Step::GoAway;
        }
        Step::Stay
    }
}

/// Talking: any of the mics louder than the threshold now (levels as the Studio sends them:
/// `{ name: { peak: [l, r] } }`).
pub fn talking(levels: &Value, mics: &[String]) -> bool {
    mics.iter().any(|m| {
        levels
            .get(m)
            .and_then(|l| l.get("peak"))
            .and_then(Value::as_array)
            .is_some_and(|p| p.iter().filter_map(Value::as_f64).any(|v| v > TALKING_DBFS))
    })
}

pub(crate) async fn run(engine: Engine) {
    let mut away = Away::default();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cfg = engine.scenes_config();
        let live = engine.stream_started_at().is_some();
        if cfg.away_minutes == 0 || !live || cfg.brb_scene.is_none() {
            away.step(now().0, false, false, 0, false);
            continue;
        }
        let Ok(Rig::Studio) = engine.rig().await else { continue };
        let mics = engine.show_config().mic_sources;
        if mics.is_empty() {
            continue;
        }
        let Ok(levels) = engine.studio_act(StudioAct::Levels { names: mics.clone() }).await else { continue };
        let brb_scene = cfg.brb_scene.clone().unwrap_or_default();
        let on_air = engine.rig_scenes(&Rig::Studio).await.ok().and_then(|(c, _)| c);
        let brb = on_air.as_deref() == Some(brb_scene.as_str());
        match away.step(now().0, talking(&levels, &mics), live, cfg.away_minutes, brb) {
            Step::Stay => {}
            Step::GoAway => {
                tracing::info!(minutes = cfg.away_minutes, "away: quiet a while, BRB on");
                if let Err(err) = engine.set_brb_away(true).await {
                    tracing::warn!(%err, "away: BRB couldn't go on");
                }
            }
            Step::Back => {
                tracing::info!("away: talking again, BRB off");
                if let Err(err) = engine.set_brb_away(false).await {
                    tracing::warn!(%err, "away: BRB couldn't come off");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MIN: i64 = 60_000;

    #[test]
    fn quiet_long_enough_goes_away_then_talking_comes_back() {
        let mut a = Away::default();
        assert_eq!(a.step(0, false, true, 3, false), Step::Stay);
        assert_eq!(a.step(3 * MIN - 1, false, true, 3, false), Step::Stay);
        assert_eq!(a.step(3 * MIN, false, true, 3, false), Step::GoAway);
        // On BRB now: one loud second isn't enough, two are.
        assert_eq!(a.step(3 * MIN + 1000, true, true, 3, true), Step::Stay);
        assert_eq!(a.step(3 * MIN + 2000, true, true, 3, true), Step::Back);
    }

    #[test]
    fn talking_now_and_then_keeps_it_from_going_away() {
        let mut a = Away::default();
        a.step(0, false, true, 2, false);
        a.step(MIN, true, true, 2, false);
        assert_eq!(a.step(2 * MIN + 30_000, false, true, 2, false), Step::Stay);
        assert_eq!(a.step(3 * MIN + 30_000, false, true, 2, false), Step::Stay);
        assert_eq!(a.step(4 * MIN + 30_000, false, true, 2, false), Step::GoAway);
    }

    #[test]
    fn a_brb_you_put_on_yourself_stays_when_you_talk() {
        let mut a = Away::default();
        for s in 0..5 {
            assert_eq!(a.step(s * 1000, true, true, 2, true), Step::Stay);
        }
    }

    #[test]
    fn switching_away_from_brb_yourself_ends_it() {
        let mut a = Away::default();
        a.step(0, false, true, 1, false);
        assert_eq!(a.step(MIN, false, true, 1, false), Step::GoAway);
        // You switched to another scene: talking then changes nothing.
        a.step(MIN + 1000, false, true, 1, false);
        assert_eq!(a.step(MIN + 2000, true, true, 1, false), Step::Stay);
        assert_eq!(a.step(MIN + 3000, true, true, 1, false), Step::Stay);
    }

    #[test]
    fn off_or_not_live_does_nothing_and_forgets() {
        let mut a = Away::default();
        assert_eq!(a.step(10 * MIN, false, true, 0, false), Step::Stay);
        assert_eq!(a.step(10 * MIN, false, false, 2, false), Step::Stay);
        // Live again: the wait starts over.
        assert_eq!(a.step(11 * MIN, false, true, 2, false), Step::Stay);
        assert_eq!(a.step(12 * MIN + 59_000, false, true, 2, false), Step::Stay);
        assert_eq!(a.step(13 * MIN, false, true, 2, false), Step::GoAway);
    }

    #[test]
    fn talking_reads_any_mic_over_the_threshold() {
        let mics = vec!["Mic".to_string(), "Headset".to_string()];
        assert!(talking(&json!({ "Mic": { "peak": [-60.0, -20.0] } }), &mics));
        assert!(talking(&json!({ "Headset": { "peak": [-35.0, -90.0] } }), &mics));
        assert!(!talking(&json!({ "Mic": { "peak": [-50.0, -45.0] } }), &mics));
        // A muted or missing mic is silence, not an error.
        assert!(!talking(&json!({ "Mic": { "peak": [null, null] } }), &mics));
        assert!(!talking(&json!({}), &mics));
        assert!(!talking(&json!({ "Other": { "peak": [0.0, 0.0] } }), &mics));
    }
}
