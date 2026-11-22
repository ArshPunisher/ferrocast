//! Per-key sliding-window rate limiter for local API keys.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default request allowance per key per sliding window.
pub const DEFAULT_LIMIT: usize = 120;
/// Default sliding window duration.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);
/// Cap tracked distinct key ids to prevent unbounded memory growth.
const MAX_TRACKED_KEYS: usize = 1024;

/// Decision from checking one API key request against its rate budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    Allowed { limit: usize, remaining: usize },
    Exceeded { limit: usize, retry_after_secs: u64 },
}

/// Thread-safe sliding-window rate limiter keyed by `api_key.id`.
#[derive(Debug, Clone)]
pub struct ApiKeyRateLimiter {
    limit: usize,
    window: Duration,
    buckets: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

impl Default for ApiKeyRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_LIMIT, DEFAULT_WINDOW)
    }
}

impl ApiKeyRateLimiter {
    /// Creates a rate limiter permitting `limit` requests per `window` for each key.
    #[must_use]
    pub fn new(limit: usize, window: Duration) -> Self {
        Self { limit: limit.max(1), window, buckets: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Records one attempt for `key_id` at `Instant::now()`.
    pub fn check(&self, key_id: &str) -> RateDecision {
        self.check_at(key_id, Instant::now())
    }

    /// Deterministic clock-injected variant used by unit tests.
    pub fn check_at(&self, key_id: &str, now: Instant) -> RateDecision {
        let mut map = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > MAX_TRACKED_KEYS && !map.contains_key(key_id) {
            map.retain(|_, q| q.back().is_some_and(|t| now.saturating_duration_since(*t) < self.window));
        }
        let q = map.entry(key_id.to_string()).or_default();
        while let Some(&front) = q.front() {
            if now.saturating_duration_since(front) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.limit {
            let oldest = q.front().copied().unwrap_or(now);
            let wait = self.window.saturating_sub(now.saturating_duration_since(oldest));
            let retry_after_secs = wait.as_secs().saturating_add(u64::from(wait.subsec_nanos() > 0)).max(1);
            return RateDecision::Exceeded { limit: self.limit, retry_after_secs };
        }
        q.push_back(now);
        RateDecision::Allowed { limit: self.limit, remaining: self.limit.saturating_sub(q.len()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_blocks_over_limit_and_recovers_after_window() {
        let limiter = ApiKeyRateLimiter::new(3, Duration::from_secs(60));
        let start = Instant::now();

        assert_eq!(limiter.check_at("k1", start), RateDecision::Allowed { limit: 3, remaining: 2 });
        assert_eq!(
            limiter.check_at("k1", start + Duration::from_secs(10)),
            RateDecision::Allowed { limit: 3, remaining: 1 }
        );
        assert_eq!(
            limiter.check_at("k1", start + Duration::from_secs(20)),
            RateDecision::Allowed { limit: 3, remaining: 0 }
        );

        let blocked = limiter.check_at("k1", start + Duration::from_secs(30));
        assert_eq!(blocked, RateDecision::Exceeded { limit: 3, retry_after_secs: 30 });

        // Independent key still has its own full allowance.
        assert_eq!(
            limiter.check_at("k2", start + Duration::from_secs(30)),
            RateDecision::Allowed { limit: 3, remaining: 2 }
        );

        // Once the oldest sample falls out of the window, k1 is permitted again.
        assert_eq!(
            limiter.check_at("k1", start + Duration::from_secs(60)),
            RateDecision::Allowed { limit: 3, remaining: 0 }
        );
    }
}
