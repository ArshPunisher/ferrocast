//! Who answers chat (R-11): the PC while it's heard from, the cloud after 90 s of silence.
//!
//! A PC with the cloud bot on sends `bot_seen` on its relay link every 30 s. The lease is in
//! memory (one relay process): after a relay restart the cloud waits the full 90 s before
//! taking over, so a PC that's on never sees the cloud step in because the relay restarted.
//! Times are `tokio::time::Instant`, so tests can pause and move the clock.

use std::time::Duration;

use tokio::time::Instant;

/// Silence from every PC for this long and the cloud takes over.
pub const TAKEOVER_AFTER: Duration = Duration::from_secs(90);

/// Who's on duty for a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Desktop,
    Cloud,
}

/// One subscription's lease.
#[derive(Debug, Clone, Copy)]
pub struct Lease {
    /// The last `bot_seen` (monotonic) and its wall time.
    seen: Option<(Instant, i64)>,
    /// When the PCs' current run of heartbeats started (wall ms).
    run_since: Option<i64>,
    /// Counting starts here when no PC was heard yet (relay start, or switching on).
    base: (Instant, i64),
}

impl Lease {
    pub fn new(now: Instant, now_ms: i64) -> Lease {
        Lease { seen: None, run_since: None, base: (now, now_ms) }
    }

    /// A PC said it's there.
    pub fn seen(&mut self, now: Instant, now_ms: i64) {
        if self.who(now) == Who::Cloud || self.run_since.is_none() {
            self.run_since = Some(now_ms);
        }
        self.seen = Some((now, now_ms));
    }

    /// Starts the 90 s again without a PC (the cloud bot was just switched on).
    pub fn restart(&mut self, now: Instant, now_ms: i64) {
        if self.seen.is_none_or(|(at, _)| now.duration_since(at) >= TAKEOVER_AFTER) {
            self.seen = None;
            self.run_since = None;
            self.base = (now, now_ms);
        }
    }

    fn last(&self) -> (Instant, i64) {
        self.seen.unwrap_or(self.base)
    }

    pub fn who(&self, now: Instant) -> Who {
        if now.duration_since(self.last().0) >= TAKEOVER_AFTER { Who::Cloud } else { Who::Desktop }
    }

    /// When the current holder took it (wall ms): the cloud 90 s after the last sign of a PC.
    pub fn since(&self, now: Instant) -> i64 {
        match self.who(now) {
            Who::Cloud => self.last().1 + i64::try_from(TAKEOVER_AFTER.as_millis()).unwrap_or(90_000),
            Who::Desktop => self.run_since.unwrap_or(self.base.1),
        }
    }

    /// The last `bot_seen` (wall ms).
    pub fn seen_at(&self) -> Option<i64> {
        self.seen.map(|(_, ms)| ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_cloud_takes_over_after_90_s_and_hands_back_at_once() {
        let t0 = Instant::now();
        let mut l = Lease::new(t0, 1_000);
        assert_eq!(l.who(t0), Who::Desktop, "a fresh relay waits before stepping in");
        tokio::time::advance(Duration::from_secs(89)).await;
        assert_eq!(l.who(Instant::now()), Who::Desktop);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(l.who(Instant::now()), Who::Cloud);
        assert_eq!(l.since(Instant::now()), 91_000);
        // The PC comes back: the desktop answers from that moment.
        l.seen(Instant::now(), 200_000);
        assert_eq!(l.who(Instant::now()), Who::Desktop);
        assert_eq!((l.since(Instant::now()), l.seen_at()), (200_000, Some(200_000)));
        // Heartbeats every 30 s keep it.
        for i in 1..=10 {
            tokio::time::advance(Duration::from_secs(30)).await;
            l.seen(Instant::now(), 200_000 + i * 30_000);
            assert_eq!(l.who(Instant::now()), Who::Desktop);
        }
        assert_eq!(l.since(Instant::now()), 200_000, "the same run");
        // Silence again.
        tokio::time::advance(Duration::from_secs(89)).await;
        assert_eq!(l.who(Instant::now()), Who::Desktop);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(l.who(Instant::now()), Who::Cloud);
        assert_eq!(l.since(Instant::now()), 500_000 + 90_000);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_on_waits_unless_a_pc_is_there() {
        let mut l = Lease::new(Instant::now(), 0);
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(l.who(Instant::now()), Who::Cloud);
        l.restart(Instant::now(), 300_000);
        assert_eq!(l.who(Instant::now()), Who::Desktop, "90 s again after switching on");
        l.seen(Instant::now(), 300_000);
        tokio::time::advance(Duration::from_secs(10)).await;
        l.restart(Instant::now(), 310_000);
        assert_eq!(l.seen_at(), Some(300_000), "a PC heard recently is kept");
    }
}
