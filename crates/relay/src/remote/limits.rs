//! In-memory rate limits for the dashboard from anywhere: one token bucket per (kind, key).
//!
//! A bucket holds up to `max` tries and refills at `max` per `window_ms`, so a burst is
//! allowed and then one try every `window_ms / max` (a smooth sliding limit, never a reset at
//! a window edge). The map is capped ([`MAX_KEYS`]): past it the oldest key goes first, in
//! O(1). Full buckets (nothing to remember) are dropped at most once a minute
//! ([`PRUNE_EVERY_MS`]), never on every call. IPv6 addresses count per /64 ([`ip_key`]), so
//! one machine can't get fresh buckets by walking its own prefix.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

/// Keys kept at most (a few MB at worst).
pub const MAX_KEYS: usize = 100_000;
/// How often full buckets are dropped.
pub const PRUNE_EVERY_MS: i64 = 60_000;

type Key = (&'static str, String);

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: i64,
    max: u32,
    window_ms: i64,
    /// Matches the bucket's entry in `order` (a re-added key gets a new stamp).
    stamp: u64,
}

impl Bucket {
    fn refill(&mut self, now: i64) {
        let rate = f64::from(self.max) / self.window_ms as f64;
        let gained = (now - self.last).max(0) as f64 * rate;
        self.tokens = (self.tokens + gained).min(f64::from(self.max));
        self.last = now;
    }

    fn full_at(&self, now: i64) -> bool {
        let rate = f64::from(self.max) / self.window_ms as f64;
        self.tokens + (now - self.last).max(0) as f64 * rate >= f64::from(self.max)
    }
}

/// Every bucket, oldest first in `order`.
#[derive(Debug, Default)]
pub(crate) struct Limits {
    map: HashMap<Key, Bucket>,
    order: VecDeque<(Key, u64)>,
    stamp: u64,
    last_prune: i64,
}

impl Limits {
    /// Takes one try for `key`: `Ok` or `Err(seconds until the next one)`.
    pub fn take(&mut self, kind: &'static str, key: &str, max: u32, window_ms: i64, now: i64) -> Result<(), u64> {
        let max = max.max(1);
        let window_ms = window_ms.max(1);
        if now - self.last_prune >= PRUNE_EVERY_MS {
            self.prune(now);
        }
        let k: Key = (kind, key.to_string());
        if !self.map.contains_key(&k) {
            while self.map.len() >= MAX_KEYS {
                let Some((old, stamp)) = self.order.pop_front() else { break };
                if self.map.get(&old).is_some_and(|b| b.stamp == stamp) {
                    self.map.remove(&old);
                }
            }
            self.stamp += 1;
            self.order.push_back((k.clone(), self.stamp));
            self.map.insert(k.clone(), Bucket { tokens: f64::from(max), last: now, max, window_ms, stamp: self.stamp });
        }
        let Some(b) = self.map.get_mut(&k) else { return Ok(()) };
        b.max = max;
        b.window_ms = window_ms;
        b.refill(now);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let per_token_ms = window_ms as f64 / f64::from(max);
            let wait_ms = (1.0 - b.tokens) * per_token_ms;
            Err(((wait_ms / 1000.0).ceil() as u64).max(1))
        }
    }

    /// Drops full buckets (they'd start full anyway) and their places in the order.
    pub fn prune(&mut self, now: i64) {
        self.last_prune = now;
        self.map.retain(|_, b| !b.full_at(now));
        let map = &self.map;
        self.order.retain(|(k, stamp)| map.get(k).is_some_and(|b| b.stamp == *stamp));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// The rate-limit key for an address: IPv4 as is, IPv6 by its /64 (IPv4-mapped as IPv4).
pub fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_then_a_steady_trickle_never_a_reset_at_a_window_edge() {
        let mut l = Limits::default();
        // 5 per 10 minutes: five at once, then one every two minutes.
        for _ in 0..5 {
            assert!(l.take("pair", "1.2.3.4", 5, 600_000, 1_000).is_ok());
        }
        assert_eq!(l.take("pair", "1.2.3.4", 5, 600_000, 1_000), Err(120));
        // A fixed window would allow five more right after its edge; a bucket allows one.
        assert!(l.take("pair", "1.2.3.4", 5, 600_000, 121_000).is_ok());
        assert!(l.take("pair", "1.2.3.4", 5, 600_000, 121_000).is_err());
        // Others aren't touched.
        assert!(l.take("pair", "5.6.7.8", 5, 600_000, 121_000).is_ok());
        assert!(l.take("device", "1.2.3.4", 5, 600_000, 121_000).is_ok());
    }

    #[test]
    fn ipv6_counts_per_slash_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(ip_key(a), ip_key(b));
        assert_ne!(ip_key(a), ip_key(c));
        assert_eq!(ip_key("::ffff:10.0.0.1".parse().unwrap()), "10.0.0.1");
        let mut l = Limits::default();
        for _ in 0..5 {
            assert!(l.take("pair", &ip_key(a), 5, 600_000, 0).is_ok());
        }
        assert!(l.take("pair", &ip_key(b), 5, 600_000, 0).is_err(), "same /64, same bucket");
    }

    #[test]
    fn the_map_is_capped_and_pruned_on_a_timer() {
        let mut l = Limits::default();
        for i in 0..(MAX_KEYS + 10) {
            let _ = l.take("pair", &i.to_string(), 5, 600_000, 1);
        }
        assert_eq!(l.len(), MAX_KEYS, "the oldest keys went");
        assert!(l.order.len() <= MAX_KEYS + 10);
        // Within the minute nothing is swept, however many calls.
        let _ = l.take("pair", "x", 5, 600_000, 30_000);
        assert_eq!(l.len(), MAX_KEYS);
        // After it, full buckets go (all of these refilled long ago).
        let _ = l.take("pair", "y", 5, 600_000, 2_000_000);
        assert_eq!(l.len(), 1);
        assert_eq!(l.order.len(), 1);
    }
}
