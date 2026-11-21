//! The live watch's mic and picture checks: speaking into a muted mic, a voice that stays too
//! quiet, a mic that sends nothing, and an on-air scene with no picture. Pure state machines fed
//! by `LiveWatch::tick`, so every threshold is tested with synthetic levels.
//!
//! Why these numbers never alarm in silence or normal speech: every rule needs a mic that is
//! (or, for "muted", would be) carrying speech, and speech sits well above a room's noise.
//! Room tone is -70..-55 dBFS and a keyboard thump lasts one tick; speech peaks every two
//! seconds at -20..-6 dBFS on a well-set mic.

use std::collections::VecDeque;

use super::{LiveAlert, LiveWatch, LiveWatchConfig, WatchSample};

/// A muted mic whose input peaks above this is carrying speech (room tone and one keyboard
/// click stay below it or last one tick).
pub const MUTED_TALK_DB: f32 = -38.0;
/// Loud ticks in a row (2 s each) before "you're muted": the second tick, 2 to 4 s in.
pub const MUTED_TALK_TICKS: u32 = 2;
/// Calm ticks in a row before that notice clears by itself (speech has pauses).
pub const MUTED_CALM_TICKS: u32 = 5;
/// Another mic that is on and above this is how the voice gets out: the muted one is intended.
pub const OTHER_MIC_LOUD_DB: f32 = -40.0;
/// Digital silence: a working mic's own noise is always above this; a dead device reads -100.
pub const DEAD_DB: f32 = -90.0;
/// A device that was reading digital silence is "back" only above this (a hardware-muted device
/// floats around `DEAD_DB` with dither, so the edge itself must not decide).
pub const HEARS_DB: f32 = -80.0;
/// Ticks in a row above `HEARS_DB` that count as hearing it again.
pub const HEARS_TICKS: u32 = 3;
/// The least a mic must send nothing at all before saying so (the Show's `watch.mic_silent_s`
/// says how long, and is never taken under this).
pub const DEAD_S: i64 = 30;
/// After a "you're muted" notice cleared by itself (the talking stopped), it isn't raised again
/// for this long: a streamer who muted on purpose isn't told at every burst of talk.
pub const MUTED_RERAISE_S: i64 = 600;
/// A fader this low or lower is a mute as far as viewers go.
pub const FADER_DOWN_DB: f32 = -60.0;
/// At least this many ticks the mic was open in the window before judging its level (a few
/// short push-to-talk presses spread over a minute are not a minute of talk).
pub const QUIET_TICKS_MIN: usize = 12;
/// Ticks held shut in a row (30 s) after which a "very quiet" notice is let go: nothing is
/// being said, and nothing more can clear it.
pub const GATED_CLEAR_TICKS: u32 = 15;
/// The window the voice's level is judged over, and the least of it that must be seen.
pub const QUIET_WINDOW_S: i64 = 60;
pub const QUIET_SPAN_S: i64 = 45;
/// A tick counts as speech above the room's floor plus 6 dB, and never below this.
pub const SPEECH_MIN_DB: f32 = -54.0;
/// A voice reaching this on the stream is fine (aim is -18 to -6).
pub const GOOD_PEAK_DB: f32 = -24.0;
/// The speech's 75th percentile under this, with nothing ever reaching `GOOD_PEAK_DB`: too quiet.
pub const QUIET_P75_DB: f32 = -28.0;
/// At least this many speech ticks (12 s of talk) in the window before judging.
pub const SPEECH_TICKS_MIN: usize = 6;
/// Ticks reaching `GOOD_PEAK_DB` that clear the notice (one thump isn't enough).
pub const GOOD_TICKS_CLEAR: usize = 3;
/// An on-air scene with no picture this long (seconds) is said.
pub const EMPTY_SCENE_S: i64 = 5;

/// One mic's levels now (dBFS, -100 for none).
#[derive(Debug, Clone, PartialEq)]
pub struct MicLevel {
    pub name: String,
    /// After its filters and fader: what the stream gets.
    pub peak_db: f32,
    /// Before them, and before the mute: what the device hears.
    pub input_db: f32,
    pub muted: bool,
    /// Held shut by a push-to-talk or push-to-mute key: on purpose.
    pub gated: bool,
    /// Its fader, dB.
    pub volume_db: f32,
    /// A mic the Show's own list already watches for silence (no second "no sound" notice).
    pub listed: bool,
}

/// The scene on air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneLook {
    pub name: String,
    /// No visible picture source (audio-only ones don't count).
    pub empty: bool,
    /// Faded to black on purpose.
    pub faded: bool,
}

/// What's been seen of one mic.
#[derive(Debug, Default)]
pub(super) struct MicTrack {
    loud_run: u32,
    calm_run: u32,
    /// When its "muted" notice last cleared by itself (the talk stopped): no new one for a while.
    cleared_at: Option<i64>,
    /// Ticks in a row it was held shut by a push-to-talk or push-to-mute key.
    gated_run: u32,
    /// When it first read digital silence in this stretch (readings between `DEAD_DB` and
    /// `HEARS_DB` neither start nor end it).
    dead_since: Option<i64>,
    /// Ticks in a row above `HEARS_DB`.
    heard_run: u32,
    /// `(at, post-fader peak)` while it was on, last `QUIET_WINDOW_S`.
    window: VecDeque<(i64, f32)>,
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    let i = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Whether the voice is too quiet over the window; `raised` keeps it so until it's clearly
/// fixed (hysteresis, so it never flaps).
fn judge_quiet(window: &VecDeque<(i64, f32)>, raised: bool) -> bool {
    let (Some(first), Some(last)) = (window.front(), window.back()) else { return false };
    if (last.0 - first.0) / 1000 < QUIET_SPAN_S {
        return raised;
    }
    let mut all: Vec<f32> = window.iter().map(|w| w.1).collect();
    all.sort_by(f32::total_cmp);
    let floor = percentile(&all, 0.2).clamp(-70.0, -45.0);
    let threshold = (floor + 6.0).max(SPEECH_MIN_DB);
    let speech: Vec<f32> = all.iter().copied().filter(|v| *v > threshold).collect();
    let good = all.iter().filter(|v| **v >= GOOD_PEAK_DB).count();
    if !raised && window.len() < QUIET_TICKS_MIN {
        return false;
    }
    if raised {
        return good < GOOD_TICKS_CLEAR && !speech.is_empty() && percentile(&speech, 0.75) < QUIET_P75_DB;
    }
    good == 0 && speech.len() >= SPEECH_TICKS_MIN && percentile(&speech, 0.75) < QUIET_P75_DB
}

impl LiveWatch {
    /// Raises or clears one alert: `Some` when wrong (said once), `None` when fine.
    fn flip(&mut self, out: &mut Vec<LiveAlert>, id: &str, wrong: Option<LiveAlert>, fixed: &str) {
        match (wrong, self.raised.contains(id)) {
            (Some(a), false) => {
                self.raised.insert(id.to_string());
                out.push(a);
            }
            (None, true) => {
                self.raised.remove(id);
                out.push(LiveAlert::fixed(id, "warn", fixed));
            }
            _ => {}
        }
    }

    /// The mic and scene checks of one sample.
    pub(super) fn tick_audio(&mut self, cfg: &LiveWatchConfig, s: &WatchSample, out: &mut Vec<LiveAlert>) {
        // A mic with its fader all the way down is muted as far as viewers go.
        let off = |m: &MicLevel| m.muted || m.volume_db <= FADER_DOWN_DB;
        let others_loud =
            |me: &str| s.levels.iter().any(|m| m.name != me && !off(m) && !m.gated && m.peak_db > OTHER_MIC_LOUD_DB);
        for m in &s.levels {
            let name = &m.name;
            let (id_m, id_q, id_s) =
                (format!("mic:muted:{name}"), format!("mic:quiet:{name}"), format!("mic:silent:{name}"));
            let was_muted_alert = self.raised.contains(&id_m);
            let mut t = self.mics.remove(name).unwrap_or_default();
            let muted = off(m);
            let live = !muted && !m.gated;
            let other = others_loud(name);
            // Speaking into a muted mic: not while another mic is carrying the voice, not in the
            // Show's own starting, ending and BRB scenes, not once said to be on purpose.
            let wrong = if muted && !m.gated && !other && !s.scene_exempt && !self.silenced.contains(&id_m) {
                if m.input_db > MUTED_TALK_DB {
                    t.loud_run += 1;
                    t.calm_run = 0;
                } else {
                    t.calm_run += 1;
                    if !was_muted_alert {
                        t.loud_run = 0;
                    }
                }
                let locked = t.cleared_at.is_some_and(|at| s.at - at < MUTED_RERAISE_S * 1000);
                let bad = if was_muted_alert {
                    t.calm_run < MUTED_CALM_TICKS
                } else {
                    t.loud_run >= MUTED_TALK_TICKS && !locked
                };
                bad.then(|| {
                    let (message, action) = if m.muted {
                        (format!("You're talking but {name} is muted."), Some("unmute"))
                    } else {
                        (format!("You're talking but {name} is turned all the way down."), None)
                    };
                    let a = LiveAlert::raised(&id_m, "warn", message)
                        .detail("Nobody hears you. Unmute it, or you're using another mic on purpose.")
                        .dismissable("muted_on_purpose");
                    match action {
                        Some(act) => a.action(name, act),
                        None => a,
                    }
                })
            } else {
                t.loud_run = 0;
                t.calm_run = 0;
                None
            };
            self.flip(out, &id_m, wrong, &format!("{name} is on."));
            if was_muted_alert && !self.raised.contains(&id_m) && muted {
                t.cleared_at = Some(s.at);
            }
            if !muted {
                t.cleared_at = None;
            }
            // A mic that sends nothing at all (for as long as the Show's setting says, at least
            // 30 s), unless another mic is carrying the voice.
            let dead_wrong = if live && cfg.mic_silent_s > 0 && !m.listed && !other {
                // Hysteresis: it starts at `DEAD_DB` or lower, and only `HEARS_TICKS` ticks in a
                // row above `HEARS_DB` end it, so dither around -90 neither blocks nor flaps it.
                t.heard_run = if m.input_db > HEARS_DB { t.heard_run + 1 } else { 0 };
                if m.input_db <= DEAD_DB {
                    t.dead_since.get_or_insert(s.at);
                }
                if t.heard_run >= HEARS_TICKS {
                    t.dead_since = None;
                }
                let dead_s = i64::from(cfg.mic_silent_s).max(DEAD_S);
                let raised = self.raised.contains(&id_s);
                t.dead_since.filter(|since| raised || (s.at - since) / 1000 >= dead_s).map(|_| {
                    LiveAlert::raised(&id_s, "error", format!("No sound from {name}.")).detail(
                        "It sends nothing at all: unplugged, switched off on the device, or taken by another app?",
                    )
                })
            } else {
                t.dead_since = None;
                t.heard_run = 0;
                None
            };
            self.flip(out, &id_s, dead_wrong, &format!("{name} hears you again."));
            // A voice that stays too quiet.
            if live {
                t.window.push_back((s.at, m.peak_db));
            }
            while t.window.front().is_some_and(|w| s.at - w.0 > QUIET_WINDOW_S * 1000) {
                t.window.pop_front();
            }
            t.gated_run = if m.gated { t.gated_run + 1 } else { 0 };
            if muted {
                // Muted on purpose (or "you're muted" says so): the level no longer matters.
                t.window.clear();
                self.flip(out, &id_q, None, "Your voice is loud enough now.");
            } else if m.gated {
                // Held shut: nothing is said. A notice already up is let go after a while: only
                // holding the key long enough could clear it.
                if t.gated_run >= GATED_CLEAR_TICKS {
                    t.window.clear();
                    self.flip(out, &id_q, None, "Your voice is loud enough now.");
                }
            } else if other || s.scene_exempt || self.silenced.contains(&id_q) {
                // Another mic carries the voice, or the Show's own scene: not this mic's level.
                t.window.clear();
                self.flip(out, &id_q, None, "Your voice is loud enough now.");
            } else {
                let quiet = judge_quiet(&t.window, self.raised.contains(&id_q));
                let wrong = quiet.then(|| {
                    LiveAlert::raised(&id_q, "warn", format!("{name} is very quiet."))
                        .detail(format!(
                            "{name} is far below the level viewers need (aim for peaks of -18 to -6 dB). \
                             Raise the mic's gain or volume; +6 dB is a safe first step."
                        ))
                        .action(name, "gain_up")
                });
                self.flip(out, &id_q, wrong, "Your voice is loud enough now.");
            }
            self.mics.insert(name.clone(), t);
        }
        self.mics.retain(|n, _| s.levels.iter().any(|m| &m.name == n));
        // The scene on air has no picture.
        if let Some(look) = &s.scene {
            if look.empty && !look.faded {
                self.empty_since.get_or_insert(s.at);
            } else {
                self.empty_since = None;
            }
            let wrong = self.empty_since.filter(|since| (s.at - since) / 1000 >= EMPTY_SCENE_S).map(|_| {
                LiveAlert::raised("scene:empty", "error", format!("\"{}\" has no picture: viewers see black.", look.name))
                    .detail("Nothing visible is in the scene on air (a mic alone shows nothing). Switch to a scene with a camera or screen.")
                    .action(&look.name, "pick_scene")
            });
            self.flip(out, "scene:empty", wrong, "Your scene has a picture again.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic(peak: f32, input: f32, muted: bool) -> MicLevel {
        MicLevel {
            name: "KREO".into(),
            peak_db: peak,
            input_db: input,
            muted,
            gated: false,
            volume_db: 0.0,
            listed: false,
        }
    }

    /// Feeds one level every 2 s for `secs`; returns every alert in order.
    fn feed(w: &mut LiveWatch, t: &mut i64, secs: i64, levels: impl Fn(i64) -> Vec<MicLevel>) -> Vec<LiveAlert> {
        let cfg = LiveWatchConfig::default();
        let mut all = Vec::new();
        for i in 0..secs / 2 {
            let s = WatchSample { at: *t, levels: levels(i), ..Default::default() };
            all.extend(w.tick(&cfg, &s));
            *t += 2000;
        }
        all
    }

    /// Room tone around -62 with a word or a thump now and then.
    fn room(i: i64) -> f32 {
        -66.0 + (i % 5) as f32
    }

    fn ids(a: &[LiveAlert]) -> Vec<(String, bool)> {
        a.iter().map(|a| (a.id.clone(), a.cleared)).collect()
    }

    #[test]
    fn silence_and_normal_speech_never_alarm() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        // Two minutes of room tone on an open mic: nothing.
        let a = feed(&mut w, &mut t, 120, |i| vec![mic(room(i), room(i), false)]);
        assert!(a.is_empty(), "{a:?}");
        // Five minutes of normal speech (peaks -20 to -8) with pauses: nothing.
        let a = feed(&mut w, &mut t, 300, |i| {
            let p = if i % 4 == 3 { room(i) } else { -20.0 + (i % 3) as f32 * 6.0 };
            vec![mic(p, p, false)]
        });
        assert!(a.is_empty(), "{a:?}");
        // A muted mic in a quiet room, and one keyboard thump on it: nothing.
        let a = feed(&mut w, &mut t, 60, |i| {
            let p = if i == 7 { -30.0 } else { room(i) };
            vec![mic(-100.0, p, true)]
        });
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn talking_into_a_muted_mic_is_said_within_four_seconds_once_then_cleared() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        feed(&mut w, &mut t, 10, |i| vec![mic(-100.0, room(i), true)]);
        let a = feed(&mut w, &mut t, 4, |_| vec![mic(-100.0, -22.0, true)]);
        assert_eq!(ids(&a), [("mic:muted:KREO".to_string(), false)], "{a:?}");
        assert_eq!(a[0].action, Some("unmute"));
        assert_eq!(a[0].source.as_deref(), Some("KREO"));
        // A pause in the speech doesn't clear it, and it isn't said twice.
        let a = feed(&mut w, &mut t, 6, |i| vec![mic(-100.0, if i == 1 { -20.0 } else { -60.0 }, true)]);
        assert!(a.is_empty(), "{a:?}");
        // Unmuted: cleared.
        let a = feed(&mut w, &mut t, 2, |_| vec![mic(-20.0, -20.0, false)]);
        assert_eq!(ids(&a), [("mic:muted:KREO".to_string(), true)]);
    }

    #[test]
    fn a_muted_mic_next_to_a_working_one_is_not_said() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 20, |_| {
            let mut other = mic(-15.0, -15.0, false);
            other.name = "Desk".into();
            vec![mic(-100.0, -20.0, true), other]
        });
        assert!(a.is_empty(), "{a:?}");
        // Push-to-talk held shut is on purpose too.
        let a = feed(&mut w, &mut t, 20, |_| {
            let mut m = mic(-100.0, -20.0, true);
            m.gated = true;
            vec![m]
        });
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn a_quiet_voice_is_said_after_a_minute_of_talk_and_clears_when_raised() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        // 19 Nov: speech peaking at -50 over a -65 room, every other tick.
        let quiet = |i: i64| {
            let p = if i % 2 == 0 { -50.0 + (i % 3) as f32 } else { -65.0 };
            vec![mic(p, p, false)]
        };
        let a = feed(&mut w, &mut t, 40, quiet);
        assert!(a.is_empty(), "not before a minute of data: {a:?}");
        let a = feed(&mut w, &mut t, 30, |i| quiet(i + 20));
        assert_eq!(ids(&a), [("mic:quiet:KREO".to_string(), false)], "{a:?}");
        assert_eq!(a[0].action, Some("gain_up"));
        // Said once.
        assert!(feed(&mut w, &mut t, 60, quiet).is_empty());
        // Gain raised: peaks at -12 for a while → cleared.
        let a = feed(&mut w, &mut t, 30, |i| {
            let p = if i % 2 == 0 { -12.0 } else { -60.0 };
            vec![mic(p, p, false)]
        });
        assert_eq!(ids(&a), [("mic:quiet:KREO".to_string(), true)], "{a:?}");
    }

    #[test]
    fn a_mic_sending_nothing_is_said_after_the_shows_setting() {
        // The Show's setting (120 s by default) says how long; never under 30 s.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 118, |_| vec![mic(-100.0, -100.0, false)]);
        assert!(a.is_empty(), "{a:?}");
        let a = feed(&mut w, &mut t, 6, |_| vec![mic(-100.0, -100.0, false)]);
        assert_eq!(ids(&a), [("mic:silent:KREO".to_string(), false)], "{a:?}");
        // One loud tick is not enough; three in a row are.
        assert!(feed(&mut w, &mut t, 4, |_| vec![mic(-60.0, -60.0, false)]).is_empty());
        let a = feed(&mut w, &mut t, 2, |_| vec![mic(-60.0, -60.0, false)]);
        assert_eq!(ids(&a), [("mic:silent:KREO".to_string(), true)]);
        // A muted dead mic is not "no sound": it's muted.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        assert!(feed(&mut w, &mut t, 120, |_| vec![mic(-100.0, -100.0, true)]).is_empty());
        // A longer setting is kept: 600 s says nothing at 5 minutes.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let cfg = LiveWatchConfig { mic_silent_s: 600, ..Default::default() };
        let mut all = Vec::new();
        for _ in 0..150 {
            let s = WatchSample { at: t, levels: vec![mic(-100.0, -100.0, false)], ..Default::default() };
            all.extend(w.tick(&cfg, &s));
            t += 2000;
        }
        assert!(all.is_empty(), "{all:?}");
        // The shortest is 30 s, whatever it says.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let cfg = LiveWatchConfig { mic_silent_s: 1, ..Default::default() };
        let mut all = Vec::new();
        for _ in 0..10 {
            let s = WatchSample { at: t, levels: vec![mic(-100.0, -100.0, false)], ..Default::default() };
            all.extend(w.tick(&cfg, &s));
            t += 2000;
        }
        assert!(all.is_empty(), "20 s is under the least: {all:?}");
    }

    #[test]
    fn a_floor_of_dither_around_minus_ninety_neither_blocks_nor_flaps_the_silent_notice() {
        // A hardware-muted device: mostly -91, now and then -88 or -85 (above DEAD_DB, under
        // HEARS_DB). It is said once after the setting and stays up, never cleared and re-raised.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let dither = |i: i64| {
            let v = match i % 7 {
                0 => -88.0,
                3 => -85.0,
                _ => -91.0 - (i % 3) as f32,
            };
            vec![mic(v, v, false)]
        };
        let a = feed(&mut w, &mut t, 200, dither);
        assert_eq!(ids(&a), [("mic:silent:KREO".to_string(), false)], "said once, never cleared: {a:?}");
        // Two loud ticks, then back to the floor: still up.
        assert!(feed(&mut w, &mut t, 4, |_| vec![mic(-60.0, -60.0, false)]).is_empty());
        assert!(feed(&mut w, &mut t, 60, dither).is_empty());
        // A real return (3 ticks above -80) clears it.
        let a = feed(&mut w, &mut t, 6, |_| vec![mic(-62.0, -62.0, false)]);
        assert_eq!(ids(&a), [("mic:silent:KREO".to_string(), true)], "{a:?}");
        // Spikes above -80 that are not three in a row do not restart the 120 s clock either.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let spiky = |i: i64| {
            let v = if i % 4 == 0 { -70.0 } else { -92.0 };
            vec![mic(v, v, false)]
        };
        let a = feed(&mut w, &mut t, 200, spiky);
        assert_eq!(ids(&a), [("mic:silent:KREO".to_string(), false)], "{a:?}");
    }

    #[test]
    fn an_empty_scene_on_air_is_said_after_five_seconds_and_a_fade_is_not() {
        let cfg = LiveWatchConfig::default();
        let mut w = LiveWatch::default();
        let look = |empty, faded| Some(SceneLook { name: "Main".into(), empty, faded });
        let mut tick = |at: i64, scene| w.tick(&cfg, &WatchSample { at, scene, ..Default::default() });
        assert!(tick(0, look(true, false)).is_empty());
        assert!(tick(2000, look(true, false)).is_empty());
        assert!(tick(4000, look(true, false)).is_empty());
        let a = tick(6000, look(true, false));
        assert_eq!(ids(&a), [("scene:empty".to_string(), false)]);
        assert!(a[0].message.contains("Main"));
        assert!(tick(8000, look(true, false)).is_empty(), "once");
        // A scene that couldn't be read keeps the state; a picture clears it.
        assert!(tick(10_000, None).is_empty());
        assert_eq!(ids(&tick(12_000, look(false, false))), [("scene:empty".to_string(), true)]);
        // Faded to black on purpose: never.
        for i in 0..10 {
            assert!(tick(20_000 + i * 2000, look(true, true)).is_empty());
        }
    }

    // ---- S61a: REV21 and the review of the first version.

    fn talk_muted(w: &mut LiveWatch, t: &mut i64, secs: i64) -> Vec<LiveAlert> {
        feed(w, t, secs, |_| vec![mic(-100.0, -22.0, true)])
    }
    fn calm_muted(w: &mut LiveWatch, t: &mut i64, secs: i64) -> Vec<LiveAlert> {
        feed(w, t, secs, |i| vec![mic(-100.0, room(i), true)])
    }

    /// Muted on purpose: said once; after it clears by itself it isn't said again for ten
    /// minutes, however often the talking starts; unmuting and muting again is a change.
    #[test]
    fn a_muted_notice_that_cleared_is_not_said_again_for_ten_minutes() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        assert_eq!(talk_muted(&mut w, &mut t, 4).len(), 1, "said once");
        assert_eq!(ids(&calm_muted(&mut w, &mut t, 12)), [("mic:muted:KREO".to_string(), true)], "cleared by itself");
        // Talking again within ten minutes: nothing.
        for _ in 0..5 {
            assert!(talk_muted(&mut w, &mut t, 6).is_empty());
            assert!(calm_muted(&mut w, &mut t, 14).is_empty());
        }
        // After ten minutes since the clear: said again.
        assert!(calm_muted(&mut w, &mut t, 600).is_empty());
        assert_eq!(talk_muted(&mut w, &mut t, 4).len(), 1, "after ten minutes");
        // Cleared, then unmuted and muted again: a change, said at once.
        assert_eq!(calm_muted(&mut w, &mut t, 12).len(), 1);
        feed(&mut w, &mut t, 2, |_| vec![mic(-20.0, -20.0, false)]);
        assert_eq!(talk_muted(&mut w, &mut t, 4).len(), 1);
    }

    /// "Muted on purpose": cleared now and not raised again for the stream; a new stream starts fresh.
    #[test]
    fn muted_on_purpose_silences_the_notice_for_the_stream() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = talk_muted(&mut w, &mut t, 4);
        assert_eq!(a[0].dismiss, Some("muted_on_purpose"));
        let cleared = w.silence("mic:muted:KREO").unwrap_or_else(|| panic!("it was up"));
        assert!(cleared.cleared);
        assert!(talk_muted(&mut w, &mut t, 20).is_empty());
        // The quiet and silent notices are not silenced by it.
        w.reset();
        assert_eq!(talk_muted(&mut w, &mut t, 4).len(), 1, "a new stream");
    }

    /// In the Show's starting, ending and BRB scenes a mic is muted on purpose: no muted or quiet notice.
    #[test]
    fn the_shows_own_scenes_never_say_muted_or_quiet() {
        let cfg = LiveWatchConfig::default();
        let mut w = LiveWatch::default();
        let mut all = Vec::new();
        let mut at = 0;
        for i in 0..40 {
            let muted =
                WatchSample { at, levels: vec![mic(-100.0, -22.0, true)], scene_exempt: true, ..Default::default() };
            all.extend(w.tick(&cfg, &muted));
            let quiet = WatchSample {
                at,
                levels: vec![mic(if i % 2 == 0 { -50.0 } else { -65.0 }, -50.0, false)],
                scene_exempt: true,
                ..Default::default()
            };
            let mut w2 = LiveWatch::default();
            all.extend(w2.tick(&cfg, &quiet));
            at += 2000;
        }
        assert!(all.is_empty(), "{all:?}");
        // The same mic in an ordinary scene is said.
        let s = |at| WatchSample { at, levels: vec![mic(-100.0, -22.0, true)], ..Default::default() };
        assert!(w.tick(&cfg, &s(100_000)).is_empty());
        assert_eq!(w.tick(&cfg, &s(102_000)).len(), 1);
    }

    /// A fader all the way down is a mute: said as turned down, with no Unmute button.
    #[test]
    fn a_fader_all_the_way_down_counts_as_muted() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 4, |_| {
            let mut m = mic(-100.0, -22.0, false);
            m.volume_db = -80.0;
            vec![m]
        });
        assert_eq!(ids(&a), [("mic:muted:KREO".to_string(), false)], "{a:?}");
        assert!(a[0].message.contains("turned all the way down"), "{}", a[0].message);
        assert_eq!(a[0].action, None);
        // And a silent fader is not "a voice that is very quiet" either.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 120, |_| {
            let mut m = mic(-100.0, room(0), false);
            m.volume_db = -80.0;
            vec![m]
        });
        assert!(a.is_empty(), "{a:?}");
    }

    /// Another mic carrying the voice: a second mic that is dead or faint is not worth a notice.
    #[test]
    fn a_dead_or_faint_second_mic_is_not_said_while_another_carries_the_voice() {
        let desk = |peak: f32, input: f32| {
            let mut m = mic(peak, input, false);
            m.name = "Desk".into();
            m
        };
        // Desk is hardware-muted (digital silence); KREO speaks at -15.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 200, |_| vec![mic(-15.0, -15.0, false), desk(-100.0, -100.0)]);
        assert!(a.is_empty(), "{a:?}");
        // Desk hears the room faintly (-50) for two minutes.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 120, |i| {
            vec![mic(-15.0, -15.0, false), desk(if i % 2 == 0 { -50.0 } else { -65.0 }, -50.0)]
        });
        assert!(a.is_empty(), "{a:?}");
    }

    /// A few short push-to-talk presses spread over a minute are not a minute of talk, and a
    /// "very quiet" notice that is up is let go while the key is not held (nothing else could clear it).
    #[test]
    fn push_to_talk_never_leaves_a_quiet_notice_stuck() {
        // Eight short presses over a minute, each a quiet word: too few open ticks to judge.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let a = feed(&mut w, &mut t, 70, |i| {
            let mut m = mic(if i % 4 == 0 { -45.0 } else { -100.0 }, -45.0, false);
            m.gated = i % 4 != 0;
            vec![m]
        });
        assert!(a.is_empty(), "{a:?}");
        // A raised notice, then the key is let go for good: it is let go too.
        let (mut w, mut t) = (LiveWatch::default(), 0);
        let quiet = |i: i64| vec![mic(if i % 2 == 0 { -50.0 } else { -65.0 }, -50.0, false)];
        let a = feed(&mut w, &mut t, 70, quiet);
        assert_eq!(ids(&a), [("mic:quiet:KREO".to_string(), false)], "{a:?}");
        let a = feed(&mut w, &mut t, 40, |_| {
            let mut m = mic(-100.0, -100.0, false);
            m.gated = true;
            vec![m]
        });
        assert_eq!(ids(&a), [("mic:quiet:KREO".to_string(), true)], "{a:?}");
    }

    /// A new stream or a wake starts fresh, and tells the page what it forgot.
    #[test]
    fn a_reset_tells_the_page_everything_it_had_raised() {
        let (mut w, mut t) = (LiveWatch::default(), 0);
        assert_eq!(talk_muted(&mut w, &mut t, 4).len(), 1);
        let told = w.reset_telling();
        assert_eq!(ids(&told), [("mic:muted:KREO".to_string(), true)]);
        assert!(w.raised().is_empty());
        assert!(w.reset_telling().is_empty());
    }
}
