//! Auto quality: while live, the stream's bitrate follows what gets through. Trouble (a
//! platform dropping packets, upload protection pausing one, the encoder dropping frames)
//! lowers it a step at once; quiet for a while raises it a step, back to what was set. It
//! never goes under `FLOOR` of it: past that, upload protection's job is to pause platforms.

/// The share of the set bitrate it never goes under, and the steps (percent).
pub const FLOOR: u8 = 40;
const DOWN: u8 = 15;
const UP: u8 = 10;
/// Quiet this long (ms) before a step up, and between two steps down.
const CALM_MS: i64 = 30_000;
const SETTLE_MS: i64 = 6_000;

/// What's counted at each look: totals since the stream started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals {
    /// Packets the platforms' senders skipped (they fell behind).
    pub net_dropped: u64,
    /// Frames the encoder dropped (it fell behind).
    pub enc_dropped: u64,
    /// Upload protection has a platform paused.
    pub paused: bool,
}

/// The bitrate's share now, and what it saw last.
#[derive(Debug, Clone)]
pub struct AutoQuality {
    pct: u8,
    last: Option<Signals>,
    last_trouble: i64,
    last_change: i64,
}

impl Default for AutoQuality {
    fn default() -> Self {
        AutoQuality { pct: 100, last: None, last_trouble: i64::MIN / 2, last_change: i64::MIN / 2 }
    }
}

impl AutoQuality {
    /// The share of the set bitrate now (percent).
    pub fn pct(&self) -> u8 {
        self.pct
    }

    /// One look at `at` (ms) → the new share when it changes.
    pub fn tick(&mut self, at: i64, s: Signals) -> Option<u8> {
        let trouble = match self.last {
            // The first look only learns where the counts start.
            None => false,
            Some(l) => s.net_dropped > l.net_dropped || s.enc_dropped > l.enc_dropped || s.paused,
        };
        self.last = Some(s);
        if trouble {
            self.last_trouble = at;
            if self.pct > FLOOR && at - self.last_change >= SETTLE_MS {
                self.pct = self.pct.saturating_sub(DOWN).max(FLOOR);
                self.last_change = at;
                return Some(self.pct);
            }
            return None;
        }
        if self.pct < 100 && at - self.last_trouble >= CALM_MS && at - self.last_change >= CALM_MS {
            self.pct = (self.pct + UP).min(100);
            self.last_change = at;
            return Some(self.pct);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(net: u64, enc: u64) -> Signals {
        Signals { net_dropped: net, enc_dropped: enc, paused: false }
    }

    #[test]
    fn trouble_steps_down_and_calm_steps_back_up() {
        let mut q = AutoQuality::default();
        assert_eq!(q.tick(0, s(5, 0)), None, "the first look only learns the counts");
        assert_eq!(q.tick(2_000, s(5, 0)), None);
        // Packets dropped: down a step at once; more trouble right after waits to settle.
        assert_eq!(q.tick(4_000, s(9, 0)), Some(85));
        assert_eq!(q.tick(6_000, s(12, 0)), None);
        assert_eq!(q.tick(10_000, s(15, 0)), Some(70));
        // The encoder dropping frames counts too, and a paused platform.
        assert_eq!(q.tick(16_000, s(15, 3)), Some(55));
        assert_eq!(q.tick(22_000, Signals { paused: true, ..s(15, 3) }), Some(40));
        assert_eq!(q.tick(28_000, s(20, 3)), None, "never under the floor");
        assert_eq!(q.pct(), FLOOR);
        // Quiet: up a step every 30 s, back to all of it.
        assert_eq!(q.tick(40_000, s(20, 3)), None, "not quiet long enough");
        let mut at = 58_000;
        let mut seen = Vec::new();
        while q.pct() < 100 {
            if let Some(p) = q.tick(at, s(20, 3)) {
                seen.push(p);
            }
            at += 2_000;
        }
        assert_eq!(seen, [50, 60, 70, 80, 90, 100]);
    }
}
