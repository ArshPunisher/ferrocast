//! One-line things an engine wants said on the dashboard that happen deep in a capture (which
//! has no way to speak): held by the engine that opened the capture, taken by whoever owns that
//! engine. Bounded: one per key, a handful in all.

/// A calm line for the dashboard: the key (same key replaces), the line, what's behind its (i).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: &'static str,
    pub line: String,
    pub detail: String,
}

/// Most notices held at once.
const MAX: usize = 8;

/// An engine's waiting notices.
#[derive(Debug, Default)]
pub struct Notices(Vec<Notice>);

impl Notices {
    /// Holds one; a notice with the same key is replaced (never a stack), and the oldest go
    /// when there are more than a handful.
    pub fn push(&mut self, n: Notice) {
        self.0.retain(|o| o.key != n.key);
        self.0.push(n);
        let extra = self.0.len().saturating_sub(MAX);
        self.0.drain(..extra);
    }

    /// Everything held since the last call.
    pub fn take(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.0)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(key: &'static str, line: &str) -> Notice {
        Notice { key, line: line.into(), detail: String::new() }
    }

    #[test]
    fn ten_thousand_raises_stay_bounded_and_one_per_key() {
        let mut q = Notices::default();
        let keys = ["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "k10", "k11"];
        for i in 0..10_000 {
            q.push(n(keys[i % keys.len()], &format!("line {i}")));
            assert!(q.len() <= MAX);
        }
        let got = q.take();
        assert_eq!(got.len(), MAX);
        let mut seen: Vec<_> = got.iter().map(|x| x.key).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), MAX, "one per key");
        assert!(q.is_empty());
    }
}
