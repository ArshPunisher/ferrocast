//! What each VideoToolbox output spends per frame, logged once a minute (target `vt_cost`):
//! the copy into the pixel buffer (`fill`), the whole `encode_once`, whether the pool handed
//! back a buffer it had used before, and the page faults the process took during the copy.
//! Fixed buckets and counters only: nothing here allocates on the frame path.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BUCKETS: usize = 32;
const SEEN: usize = 16;
/// How often the line is written.
pub(crate) const EVERY: Duration = Duration::from_secs(60);

/// Microsecond samples in power-of-two buckets: bucket `i` holds `[2^i, 2^(i+1))` (0 in bucket 0).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Hist {
    counts: [u32; BUCKETS],
    n: u64,
    sum_us: u64,
}

impl Hist {
    pub(crate) fn add(&mut self, us: u64) {
        let b = (64 - us.max(1).leading_zeros() as usize - 1).min(BUCKETS - 1);
        self.counts[b] += 1;
        self.n += 1;
        self.sum_us += us;
    }
    pub(crate) fn mean_ms(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum_us as f64 / self.n as f64 / 1000.0 }
    }
    /// The upper edge of the bucket holding the `q` quantile, in ms (at most 2x too high).
    pub(crate) fn quantile_ms(&self, q: f64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let want = (q * self.n as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += u64::from(*c);
            if seen >= want {
                return (1u64 << (i + 1)) as f64 / 1000.0;
            }
        }
        (1u64 << BUCKETS) as f64 / 1000.0
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One encoder's tally since its last line.
#[derive(Debug)]
pub(crate) struct Cost {
    id: u64,
    fill: Hist,
    encode: Hist,
    frames: u64,
    created: u64,
    reused: u64,
    faults: u64,
    /// Buffer addresses seen lately (a pool recycles a few): a new one counts as created.
    seen: [usize; SEEN],
    seen_next: usize,
    since: Instant,
}

impl Cost {
    pub(crate) fn new() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            fill: Hist::default(),
            encode: Hist::default(),
            frames: 0,
            created: 0,
            reused: 0,
            faults: 0,
            seen: [0; SEEN],
            seen_next: 0,
            since: Instant::now(),
        }
    }

    /// A buffer came from the pool: reused when its address was seen before.
    pub(crate) fn buffer(&mut self, addr: usize) {
        if addr != 0 && self.seen.contains(&addr) {
            self.reused += 1;
        } else {
            self.created += 1;
            self.seen[self.seen_next] = addr;
            self.seen_next = (self.seen_next + 1) % SEEN;
        }
    }

    pub(crate) fn fill(&mut self, d: Duration, faults: u64) {
        self.fill.add(d.as_micros() as u64);
        self.faults += faults;
    }

    pub(crate) fn encode(&mut self, d: Duration) {
        self.encode.add(d.as_micros() as u64);
        self.frames += 1;
    }

    /// Writes the line and starts over once a minute has passed.
    pub(crate) fn tick(&mut self, now: Instant) {
        if now.duration_since(self.since) < EVERY || self.frames == 0 {
            return;
        }
        let secs = now.duration_since(self.since).as_secs_f64();
        tracing::info!(
            target: "vt_cost",
            output = self.id,
            frames = self.frames,
            fps = format!("{:.1}", self.frames as f64 / secs),
            fill_mean_ms = format!("{:.3}", self.fill.mean_ms()),
            fill_p99_ms = format!("{:.3}", self.fill.quantile_ms(0.99)),
            encode_mean_ms = format!("{:.3}", self.encode.mean_ms()),
            encode_p99_ms = format!("{:.3}", self.encode.quantile_ms(0.99)),
            buffers_created = self.created,
            buffers_reused = self.reused,
            fill_faults_per_frame = format!("{:.1}", self.faults as f64 / self.frames as f64),
            "VideoToolbox cost per frame"
        );
        *self = Self { id: self.id, seen: self.seen, seen_next: self.seen_next, ..Self::new() };
    }
}

/// Minor page faults the process has taken so far (all threads: the delta around a copy also
/// counts what other threads faulted meanwhile).
pub(crate) fn process_faults() -> u64 {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes a rusage into the buffer we give it.
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) } == 0;
    if ok {
        // SAFETY: getrusage succeeded, so the struct is filled in.
        unsafe { ru.assume_init() }.ru_minflt.max(0) as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_histogram_gives_means_and_bucketed_quantiles() {
        let mut h = Hist::default();
        for _ in 0..99 {
            h.add(1000); // 1 ms
        }
        h.add(50_000); // one 50 ms outlier
        assert!((h.mean_ms() - 1.49).abs() < 0.01, "{}", h.mean_ms());
        // p99 is still the 1 ms bucket (its upper edge, 1.024 ms); p100 reaches the outlier's.
        assert!((h.quantile_ms(0.99) - 1.024).abs() < 1e-9);
        assert!((h.quantile_ms(1.0) - 65.536).abs() < 1e-9);
        assert_eq!(Hist::default().quantile_ms(0.99), 0.0);
        let mut z = Hist::default();
        z.add(0);
        assert_eq!(z.counts[0], 1, "zero goes in the first bucket");
    }

    #[test]
    fn buffers_seen_before_count_as_reused() {
        let mut c = Cost::new();
        for a in [10, 20, 30, 10, 20, 30, 10] {
            c.buffer(a);
        }
        assert_eq!((c.created, c.reused), (3, 4));
        // More distinct buffers than slots: the oldest are forgotten, not counted as reused.
        for a in 100..140 {
            c.buffer(a);
        }
        c.buffer(10);
        assert_eq!(c.created, 3 + 40 + 1);
    }

    #[test]
    fn a_line_is_written_once_a_minute_and_the_tally_starts_over() {
        let mut c = Cost::new();
        c.fill(Duration::from_micros(500), 3);
        c.encode(Duration::from_millis(2));
        let t0 = c.since;
        c.tick(t0 + Duration::from_secs(10));
        assert_eq!(c.frames, 1, "not yet");
        c.tick(t0 + EVERY);
        assert_eq!((c.frames, c.fill.n, c.faults), (0, 0, 0), "reset after the line");
    }

    #[test]
    fn the_frame_path_does_not_allocate() {
        // Plain arrays and counters: the type is Copy-free of heap pointers.
        assert!(std::mem::size_of::<Cost>() < 1024);
        assert!(process_faults() > 0);
    }
}
