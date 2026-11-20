//! Keeps a chatty third-party warning from filling the log: the noise-removal library says
//! "Possible clipping detected" about 41 times a second (5 MB of log in 25 minutes). One line a
//! minute gets through, and a second line says how many were left out.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Lets one event through per `every`, counting the rest.
#[derive(Debug)]
pub struct RateLimit {
    every: Duration,
    last: Mutex<Option<Instant>>,
    left_out: AtomicU64,
}

impl RateLimit {
    pub const fn new(every: Duration) -> Self {
        Self { every, last: Mutex::new(None), left_out: AtomicU64::new(0) }
    }

    /// Whether an event at `now` goes through (the first one, then one per interval).
    pub fn allow(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_none_or(|t| now.saturating_duration_since(t) >= self.every) {
            *last = Some(now);
            true
        } else {
            self.left_out.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// How many were left out since the last call.
    pub fn take_left_out(&self) -> u64 {
        self.left_out.swap(0, Ordering::Relaxed)
    }
}

/// The limiter for the denoiser's warnings (the `df` crates), once a minute.
pub static CLIPPING: RateLimit = RateLimit::new(Duration::from_secs(60));

/// A global filter for the log: the denoiser library's warnings pass through [`CLIPPING`].
pub fn layer<S: tracing::Subscriber>() -> impl tracing_subscriber::Layer<S> {
    tracing_subscriber::filter::filter_fn(|meta| {
        if meta.level() == &tracing::Level::WARN && meta.target().starts_with("df") {
            return CLIPPING.allow(Instant::now());
        }
        true
    })
}

/// Says once a minute how many of those warnings were left out.
pub fn report_in_background() {
    let _ = std::thread::Builder::new().name("ferrocast-log-limit".into()).spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let n = CLIPPING.take_left_out();
            if n > 0 {
                tracing::info!(
                    left_out = n,
                    "the noise removal reported possible clipping (one warning a minute is kept)"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_a_minute_gets_through_and_the_rest_are_counted() {
        let limit = RateLimit::new(Duration::from_secs(60));
        let start = Instant::now();
        assert!(limit.allow(start));
        // 41 a second for 30 seconds: none of them.
        for i in 1..=1230u32 {
            assert!(!limit.allow(start + Duration::from_millis(u64::from(i) * 24)));
        }
        assert_eq!(limit.take_left_out(), 1230);
        assert_eq!(limit.take_left_out(), 0);
        assert!(!limit.allow(start + Duration::from_secs(59)));
        assert!(limit.allow(start + Duration::from_secs(61)), "the next minute: one more");
    }
}
