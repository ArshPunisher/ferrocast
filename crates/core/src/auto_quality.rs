//! Auto quality: while live, the stream's bitrate follows what the upload carries. Trouble
//! lowers it a step; quiet for a while raises it a step, back to what was set. It never goes
//! under `FLOOR` of it: past that, upload protection's job is to pause platforms.
//!
//! Only the upload counts as trouble (owner, 17 Nov: the bitrate fell to 40% in 23 s while the
//! Mac drew slowly and Kick kept hanging up; neither is fixed by a lower bitrate):
//! - most of the platforms falling behind at once (a full upload slows them all; one alone is
//!   that platform's own trouble, which upload protection and its retries handle); a platform
//!   that was live and hung up counts as behind (an upload so full that they all keep
//!   reconnecting never stays live long enough to count its drops);
//! - the Studio's own sender falling behind;
//! - upload protection pausing a platform (counted once, when it pauses).
//!
//! Frames drawn late are not counted: they're the computer's trouble, not the line's. The first
//! seconds of a stream aren't judged either: every connection fills its buffers then.

/// The share of the set bitrate it never goes under, and the steps (percent).
pub const FLOOR: u8 = 40;
const DOWN: u8 = 15;
const UP: u8 = 10;
/// Quiet this long (ms) before a step up, and between two steps down.
const CALM_MS: i64 = 30_000;
const SETTLE_MS: i64 = 6_000;
/// A stream's first seconds (ms) aren't judged: connections are still warming up.
const GRACE_MS: i64 = 10_000;
/// A platform live for less than this (ms) isn't judged: it just (re)connected.
const WARM_MS: u64 = 10_000;
/// Trouble this recent (ms) still counts as "the upload is struggling" for the status bar.
const RECENT_MS: i64 = 10_000;

/// One platform's sender at a look.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dest {
    pub id: String,
    /// Packets it skipped this session (they only grow until it reconnects).
    pub dropped: u64,
    /// How long it has been live this session.
    pub live_ms: u64,
    /// Sending now (not connecting or retrying).
    pub live: bool,
}

/// What's counted at each look.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Signals {
    /// The platforms sending now.
    pub dests: Vec<Dest>,
    /// Packets the Studio's own sender skipped since the stream started.
    pub feed_dropped: u64,
    /// Upload protection has a platform paused.
    pub paused: bool,
}

/// The bitrate's share now, and what it saw last.
#[derive(Debug, Clone)]
pub struct AutoQuality {
    pct: u8,
    started: Option<i64>,
    last: Option<Signals>,
    last_trouble: i64,
    last_change: i64,
}

impl Default for AutoQuality {
    fn default() -> Self {
        AutoQuality { pct: 100, started: None, last: None, last_trouble: i64::MIN / 2, last_change: i64::MIN / 2 }
    }
}

impl AutoQuality {
    /// The share of the set bitrate now (percent).
    pub fn pct(&self) -> u8 {
        self.pct
    }

    /// Whether the upload struggled in the last few seconds (as of `at`, ms).
    pub fn struggling(&self, at: i64) -> bool {
        at - self.last_trouble < RECENT_MS
    }

    /// Whether `s` shows the upload in trouble since the look before.
    fn trouble(last: &Signals, s: &Signals) -> bool {
        if s.feed_dropped > last.feed_dropped || (s.paused && !last.paused) {
            return true;
        }
        // Each platform seen at both looks: one that hung up since is behind; one live long
        // enough (live at both looks) is judged by its drops. One just (re)connected isn't judged.
        let mut judged = 0;
        let mut behind = 0;
        for d in &s.dests {
            let Some(l) = last.dests.iter().find(|l| l.id == d.id) else { continue };
            let hung_up = l.live && (!d.live || d.live_ms < l.live_ms);
            let warm = d.live && l.live && d.live_ms >= WARM_MS;
            if hung_up {
                judged += 1;
                behind += 1;
            } else if warm {
                judged += 1;
                behind += usize::from(d.dropped > l.dropped);
            }
        }
        // One platform: it alone is the stream. Several: more than half at once.
        match judged {
            0 => false,
            1 => behind == 1,
            n => behind * 2 > n,
        }
    }

    /// One look at `at` (ms) with auto quality off: trouble is noted (for the status bar), the
    /// bitrate never changes.
    pub fn observe(&mut self, at: i64, s: Signals) {
        let started = *self.started.get_or_insert(at);
        let trouble = self.last.as_ref().is_some_and(|l| Self::trouble(l, &s));
        self.last = Some(s);
        if trouble && at - started >= GRACE_MS {
            self.last_trouble = at;
        }
    }

    /// Auto quality switched off: back to the whole bitrate → whether it was lowered. What it saw
    /// (and when the stream began) is kept, so switching it on again doesn't restart the grace.
    pub fn back_to_full(&mut self) -> bool {
        let lowered = self.pct < 100;
        self.pct = 100;
        self.last_change = i64::MIN / 2;
        lowered
    }

    /// One look at `at` (ms) → the new share when it changes.
    pub fn tick(&mut self, at: i64, s: Signals) -> Option<u8> {
        let started = *self.started.get_or_insert(at);
        let trouble = match &self.last {
            // The first look only learns where the counts start.
            None => false,
            Some(l) => Self::trouble(l, &s),
        };
        // Down at the look before: the first look with platforms live again proves nothing either.
        let was_down = s_dests_all_down(&self.last);
        self.last = Some(s);
        let now_down = s_dests_all_down(&self.last);
        if at - started < GRACE_MS {
            return None;
        }
        if trouble {
            self.last_trouble = at;
            if self.pct > FLOOR && at - self.last_change >= SETTLE_MS {
                self.pct = self.pct.saturating_sub(DOWN).max(FLOOR);
                self.last_change = at;
                return Some(self.pct);
            }
            return None;
        }
        // Every platform down: nothing is being sent, so a quiet look proves nothing.
        if !was_down
            && !now_down
            && self.pct < 100
            && at - self.last_trouble >= CALM_MS
            && at - self.last_change >= CALM_MS
        {
            self.pct = (self.pct + UP).min(100);
            self.last_change = at;
            return Some(self.pct);
        }
        None
    }
}

/// Whether the last look showed platforms and none sending.
fn s_dests_all_down(last: &Option<Signals>) -> bool {
    last.as_ref().is_some_and(|s| !s.dests.is_empty() && s.dests.iter().all(|d| !d.live))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, dropped: u64) -> Dest {
        Dest { id: id.into(), dropped, live_ms: 60_000, live: true }
    }

    fn all(dropped: [u64; 3]) -> Signals {
        Signals {
            dests: vec![d("twitch", dropped[0]), d("youtube", dropped[1]), d("kick", dropped[2])],
            ..Default::default()
        }
    }

    /// Steps from a quiet start at `from` (looks every 2 s until `to`).
    fn quiet(q: &mut AutoQuality, from: i64, to: i64, s: &Signals) {
        let mut at = from;
        while at < to {
            assert_eq!(q.tick(at, s.clone()), None, "at {at}");
            at += 2_000;
        }
    }

    #[test]
    fn a_full_upload_steps_down_and_calm_steps_back_up() {
        let mut q = AutoQuality::default();
        quiet(&mut q, 0, 12_000, &all([0, 0, 0]));
        // All three fall behind at once: down a step; more right after waits to settle.
        assert_eq!(q.tick(12_000, all([5, 5, 5])), Some(85));
        assert_eq!(q.tick(14_000, all([9, 9, 9])), None);
        assert_eq!(q.tick(18_000, all([12, 12, 12])), Some(70));
        // Two of three is most of them.
        assert_eq!(q.tick(24_000, all([15, 15, 12])), Some(55));
        assert_eq!(q.tick(30_000, Signals { paused: true, ..all([15, 15, 12]) }), Some(40));
        assert_eq!(q.tick(36_000, all([20, 20, 20])), None, "never under the floor");
        assert_eq!(q.pct(), FLOOR);
        assert!(q.struggling(40_000));
        // Quiet: up a step every 30 s, back to all of it.
        let mut at = 38_000;
        let mut seen = Vec::new();
        while q.pct() < 100 {
            if let Some(p) = q.tick(at, all([20, 20, 20])) {
                seen.push(p);
            }
            at += 2_000;
        }
        assert_eq!(seen, [50, 60, 70, 80, 90, 100]);
        assert!(!q.struggling(at));
    }

    /// While every platform is down (a network outage) nothing is being sent, so "quiet" proves
    /// nothing: the quality holds where it was and only steps back up after they are live and calm.
    #[test]
    fn it_holds_the_quality_while_every_platform_is_down() {
        let mut q = AutoQuality::default();
        quiet(&mut q, 0, 12_000, &all([0, 0, 0]));
        assert_eq!(q.tick(12_000, all([5, 5, 5])), Some(85));
        let down = Signals {
            dests: ["twitch", "youtube", "kick"]
                .map(|id| Dest { id: id.into(), dropped: 5, live_ms: 0, live: false })
                .to_vec(),
            ..Default::default()
        };
        let mut at = 14_000;
        while at < 200_000 {
            assert_eq!(q.tick(at, down.clone()), None, "no step up with nothing live, at {at}");
            at += 2_000;
        }
        assert_eq!(q.pct(), 85, "held");
        // Live again and calm: it climbs after the usual calm, not at the first look.
        let mut up_at_first_live_look = None;
        let mut up = None;
        for i in 0..40 {
            let s = Signals {
                dests: ["twitch", "youtube", "kick"]
                    .map(|id| Dest { id: id.into(), dropped: 0, live_ms: 60_000, live: true })
                    .to_vec(),
                ..Default::default()
            };
            if let Some(p) = q.tick(200_000 + i * 2_000, s) {
                if i == 0 {
                    up_at_first_live_look = Some(p);
                }
                up = Some(p);
                break;
            }
        }
        assert_eq!(up, Some(95));
        assert!(
            up_at_first_live_look.is_none(),
            "the first look with platforms back stepped the quality up: {up_at_first_live_look:?}"
        );
    }

    #[test]
    fn one_platform_in_trouble_leaves_the_others_alone() {
        // 15 Nov: Kick hung up every 8 s and the bitrate fell for everyone.
        let mut q = AutoQuality::default();
        quiet(&mut q, 0, 12_000, &all([0, 0, 0]));
        let mut at = 12_000;
        for n in 1..30 {
            assert_eq!(q.tick(at, all([0, 0, n * 40])), None, "at {at}");
            at += 2_000;
        }
        // Kick reconnecting (live a moment, its count from zero again) doesn't count either.
        let back = Signals {
            dests: vec![
                d("twitch", 0),
                d("youtube", 0),
                Dest { id: "kick".into(), dropped: 0, live_ms: 1_000, live: true },
            ],
            ..Default::default()
        };
        assert_eq!(q.tick(at, back), None);
        assert_eq!(q.pct(), 100);
    }

    #[test]
    fn the_first_seconds_and_late_frames_dont_count() {
        let mut q = AutoQuality::default();
        // Connections fill up at the start: nothing changes in the grace time.
        assert_eq!(q.tick(0, all([0, 0, 0])), None);
        assert_eq!(q.tick(2_000, all([50, 50, 50])), None);
        assert_eq!(q.tick(8_000, all([90, 90, 90])), None);
        assert_eq!(q.pct(), 100);
        // A platform just connected isn't judged.
        let mut q = AutoQuality::default();
        let fresh = |n| Signals {
            dests: vec![Dest { id: "yt".into(), dropped: n, live_ms: 2_000, live: true }],
            ..Default::default()
        };
        quiet(&mut q, 0, 12_000, &fresh(0));
        assert_eq!(q.tick(12_000, fresh(9)), None);
    }

    #[test]
    fn the_studios_own_sender_and_a_pause_count() {
        let mut q = AutoQuality::default();
        let s = |feed, paused| Signals { feed_dropped: feed, paused, ..all([0, 0, 0]) };
        quiet(&mut q, 0, 12_000, &s(0, false));
        assert_eq!(q.tick(12_000, s(4, false)), Some(85));
        // A pause counts when it starts, not for every look while it lasts.
        assert_eq!(q.tick(20_000, s(4, true)), Some(70));
        assert_eq!(q.tick(28_000, s(4, true)), None);
        assert_eq!(q.pct(), 70);
    }

    #[test]
    fn switched_off_it_only_watches() {
        let mut q = AutoQuality::default();
        q.observe(0, all([0, 0, 0]));
        q.observe(12_000, all([5, 5, 5]));
        assert!(q.struggling(13_000));
        assert_eq!(q.pct(), 100);
        assert!(!q.struggling(30_000));
    }

    #[test]
    fn one_platform_alone_is_the_stream() {
        let mut q = AutoQuality::default();
        let one = |n| Signals { dests: vec![d("twitch", n)], ..Default::default() };
        quiet(&mut q, 0, 12_000, &one(0));
        assert_eq!(q.tick(12_000, one(3)), Some(85));
    }

    #[test]
    fn every_platform_hanging_up_is_a_full_upload() {
        // So full that none stays live 10 s: their drops are never judged, the hang-ups are.
        let mut q = AutoQuality::default();
        quiet(&mut q, 0, 12_000, &all([0, 0, 0]));
        let down = |id: &str| Dest { id: id.into(), dropped: 0, live_ms: 0, live: false };
        let gone = Signals { dests: vec![down("twitch"), down("youtube"), down("kick")], ..Default::default() };
        assert_eq!(q.tick(12_000, gone), Some(85));
    }

    #[test]
    fn switching_it_off_keeps_the_grace_done() {
        let mut q = AutoQuality::default();
        quiet(&mut q, 0, 12_000, &all([0, 0, 0]));
        assert_eq!(q.tick(12_000, all([5, 5, 5])), Some(85));
        assert!(q.back_to_full());
        assert_eq!(q.pct(), 100);
        // On again: still past the stream's first seconds, trouble counts at once.
        assert_eq!(q.tick(14_000, all([9, 9, 9])), Some(85));
    }
}
