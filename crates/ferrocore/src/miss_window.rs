//! The last minute of missed frames, one second at a time, by cause: for Studio's Stats (the
//! "Frame rate" panel colours and explains itself from the last 60 s, not from everything since
//! the Studio started, where one old hiccup kept it red for good).
//!
//! The frame loop adds to it under the state lock it already holds (a few integer additions, no
//! allocation, no extra lock); readers get a copy aged to their own clock, so a loop that has
//! stopped drawing still lets old misses fall out of the window.

/// How many seconds the window covers (and how many buckets it keeps).
pub const WINDOW_S: usize = 60;

const NS_PER_S: u64 = 1_000_000_000;

/// What was missed in one second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MissBucket {
    /// Frames missed because drawing fell behind.
    pub render: u32,
    /// Frames missed while the encoders held the frame loop back.
    pub held: u32,
    /// Frames an encoder left out (its queue was full).
    pub skipped: u32,
    /// Of `render + held`, the frames missed while a window or divider was being resized.
    pub resize: u32,
}

impl MissBucket {
    fn total(&self) -> u32 {
        self.render.saturating_add(self.held).saturating_add(self.skipped)
    }
}

/// A ring of one-second buckets: `second` is the newest second written, its bucket
/// `buckets[second % WINDOW_S]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissWindow {
    second: u64,
    buckets: [MissBucket; WINDOW_S],
}

impl Default for MissWindow {
    fn default() -> Self {
        Self { second: 0, buckets: [MissBucket::default(); WINDOW_S] }
    }
}

/// The last `WINDOW_S` seconds, summed and second by second (oldest first, the current second
/// last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentMisses {
    pub render: u64,
    pub held: u64,
    pub skipped: u64,
    pub resize: u64,
    /// Missed plus skipped frames in each of the last `WINDOW_S` seconds, oldest first.
    pub per_second: [u32; WINDOW_S],
}

impl Default for RecentMisses {
    fn default() -> Self {
        Self { render: 0, held: 0, skipped: 0, resize: 0, per_second: [0; WINDOW_S] }
    }
}

impl MissWindow {
    /// Moves the window on to `second`, emptying the seconds it passes over.
    fn advance(&mut self, second: u64) {
        if second <= self.second {
            return;
        }
        let gap = (second - self.second).min(WINDOW_S as u64);
        for i in 1..=gap {
            self.buckets[((self.second + i) % WINDOW_S as u64) as usize] = MissBucket::default();
        }
        self.second = second;
    }

    /// Notes frames missed at `now_ns`; `resizing` when a window or divider was being resized.
    pub fn add(&mut self, now_ns: u64, render: u64, held: u64, skipped: u64, resizing: bool) {
        if render == 0 && held == 0 && skipped == 0 {
            return;
        }
        self.advance(now_ns / NS_PER_S);
        let b = &mut self.buckets[(self.second % WINDOW_S as u64) as usize];
        let clamp = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        b.render = b.render.saturating_add(clamp(render));
        b.held = b.held.saturating_add(clamp(held));
        b.skipped = b.skipped.saturating_add(clamp(skipped));
        if resizing {
            b.resize = b.resize.saturating_add(clamp(render.saturating_add(held)));
        }
    }

    /// The last minute as of `now_ns`: seconds older than the window have fallen out.
    pub fn recent(&self, now_ns: u64) -> RecentMisses {
        let now = (now_ns / NS_PER_S).max(self.second);
        let mut out = RecentMisses::default();
        for (i, slot) in out.per_second.iter_mut().enumerate() {
            // The second this slot stands for: `now` is the last slot.
            let Some(s) = (now + 1 + i as u64).checked_sub(WINDOW_S as u64) else { continue };
            // Written at all (not beyond the newest write) and not overwritten since.
            if s > self.second || self.second - s >= WINDOW_S as u64 {
                continue;
            }
            let b = &self.buckets[(s % WINDOW_S as u64) as usize];
            *slot = b.total();
            out.render += u64::from(b.render);
            out.held += u64::from(b.held);
            out.skipped += u64::from(b.skipped);
            out.resize += u64::from(b.resize);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = NS_PER_S;

    #[test]
    fn empty_window_is_calm() {
        let w = MissWindow::default();
        assert_eq!(w.recent(100 * S), RecentMisses::default());
    }

    #[test]
    fn sums_by_cause_and_lays_out_seconds_oldest_first() {
        let mut w = MissWindow::default();
        w.add(100 * S, 3, 0, 0, false);
        w.add(100 * S + 500, 0, 2, 1, false);
        w.add(103 * S, 4, 0, 0, true);
        let r = w.recent(103 * S + 10);
        assert_eq!((r.render, r.held, r.skipped, r.resize), (7, 2, 1, 4));
        // Newest second last: 103 -> 4, 102 and 101 empty, 100 -> 3 + 2 + 1.
        assert_eq!(&r.per_second[WINDOW_S - 4..], &[6, 0, 0, 4]);
        assert!(r.per_second[..WINDOW_S - 4].iter().all(|&n| n == 0));
    }

    #[test]
    fn old_misses_age_out_even_when_the_loop_writes_nothing_more() {
        let mut w = MissWindow::default();
        w.add(100 * S, 5, 0, 0, false);
        // 59 s later the second is still the oldest slot; 60 s later it has fallen out.
        assert_eq!(w.recent(159 * S).render, 5);
        assert_eq!(w.recent(159 * S).per_second[0], 5);
        assert_eq!(w.recent(160 * S), RecentMisses::default());
        assert_eq!(w.recent(10_000 * S), RecentMisses::default());
    }

    #[test]
    fn a_long_gap_clears_every_bucket_it_passes_and_keeps_the_new_miss() {
        let mut w = MissWindow::default();
        w.add(100 * S, 9, 0, 0, false);
        w.add(500 * S, 0, 1, 0, false);
        let r = w.recent(500 * S);
        assert_eq!((r.render, r.held), (0, 1));
        assert_eq!(r.per_second[WINDOW_S - 1], 1);
    }

    #[test]
    fn a_bucket_is_reused_after_a_full_window() {
        let mut w = MissWindow::default();
        w.add(10 * S, 2, 0, 0, false);
        w.add(70 * S, 3, 0, 0, false); // same slot as second 10
        let r = w.recent(70 * S);
        assert_eq!(r.render, 3);
    }

    #[test]
    fn a_clock_that_steps_back_is_added_to_the_newest_second() {
        let mut w = MissWindow::default();
        w.add(100 * S, 1, 0, 0, false);
        w.add(99 * S, 1, 0, 0, false);
        assert_eq!(w.recent(100 * S).render, 2);
    }

    #[test]
    fn nothing_missed_writes_nothing() {
        let mut w = MissWindow::default();
        w.add(100 * S, 0, 0, 0, true);
        assert_eq!(w, MissWindow::default());
    }

    #[test]
    fn huge_counts_saturate_instead_of_wrapping() {
        let mut w = MissWindow::default();
        w.add(5 * S, u64::MAX, 0, 0, true);
        w.add(5 * S, 10, 0, 0, false);
        assert_eq!(w.recent(5 * S).render, u64::from(u32::MAX));
    }
}
