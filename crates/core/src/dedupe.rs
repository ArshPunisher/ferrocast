//! Dropping repeats before anything else sees an event (R-29).
//!
//! Platforms deliver at least once: EventSub resends with the same `message_id`, Kick may
//! retry a webhook, and two transports can carry the same message. Every event gets a key
//! and the bus keeps recent keys for a window; a repeat inside the window is dropped.

use std::collections::{HashMap, VecDeque};

use crate::event::{Event, EventKind, Millis, Platform};

/// How a key was made. Hashed keys are weaker (two identical tips in the same second
/// collapse), so they get a shorter window.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DedupeKey {
    Source(Platform, String),
    Hashed(Platform, u64),
}

/// Seconds a hashed key stays remembered. Long enough to absorb a double delivery, short
/// enough that a viewer sending the same "gg" twice a minute apart isn't swallowed.
const HASHED_WINDOW_MS: i64 = 10_000;

/// The key for an event: the platform's id when it has one, otherwise a hash of what the
/// event says, bucketed to the second.
pub fn key_for(event: &Event) -> DedupeKey {
    if let Some(id) = event.source_id.as_deref().filter(|s| !s.is_empty()) {
        return DedupeKey::Source(event.platform, id.to_string());
    }
    let mut h = Fnv::new();
    h.write(event.kind.name().as_bytes());
    if let Some(actor) = event.kind.actor() {
        h.write(actor.id.as_deref().unwrap_or("").as_bytes());
        h.write(actor.login.as_deref().unwrap_or(&actor.display_name).as_bytes());
    }
    match &event.kind {
        EventKind::ChatMessage { content, .. } => h.write(content.text.as_bytes()),
        EventKind::Tip { money, payment_ref, .. } => {
            h.write(payment_ref.as_bytes());
            h.write(&money.minor.to_le_bytes());
        }
        EventKind::PaidMessage { money, .. } => h.write(&money.minor.to_le_bytes()),
        EventKind::PlatformGift { value, .. } => h.write(&value.amount.to_le_bytes()),
        EventKind::GiftBatch { count, .. } => h.write(&count.to_le_bytes()),
        EventKind::Raid { viewers, .. } => h.write(&viewers.to_le_bytes()),
        _ => {}
    }
    h.write(&(event.occurred_at.0 / 1000).to_le_bytes());
    DedupeKey::Hashed(event.platform, h.finish())
}

/// Remembers recent keys, bounded in both time and count so a flood can't grow memory.
#[derive(Debug)]
pub struct Deduper {
    window_ms: i64,
    capacity: usize,
    seen: HashMap<DedupeKey, Millis>,
    order: VecDeque<(DedupeKey, Millis)>,
}

impl Deduper {
    /// `window_ms` for platform ids (EventSub retries arrive within minutes), and a hard
    /// cap on remembered keys.
    pub fn new(window_ms: i64, capacity: usize) -> Self {
        Deduper { window_ms, capacity: capacity.max(1), seen: HashMap::new(), order: VecDeque::new() }
    }

    /// `true` the first time an event is seen, `false` for a repeat.
    pub fn first_time(&mut self, event: &Event, now: Millis) -> bool {
        self.expire(now);
        let key = key_for(event);
        if let Some(&at) = self.seen.get(&key) {
            let window = self.window_for(&key);
            if now.0 - at.0 <= window {
                return false;
            }
        }
        self.seen.insert(key.clone(), now);
        self.order.push_back((key, now));
        while self.order.len() > self.capacity {
            if let Some((old, at)) = self.order.pop_front() {
                // Only forget it if this entry is still the newest record of that key.
                if self.seen.get(&old) == Some(&at) {
                    self.seen.remove(&old);
                }
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn window_for(&self, key: &DedupeKey) -> i64 {
        match key {
            DedupeKey::Source(..) => self.window_ms,
            DedupeKey::Hashed(..) => HASHED_WINDOW_MS.min(self.window_ms),
        }
    }

    fn expire(&mut self, now: Millis) {
        while let Some((key, at)) = self.order.front() {
            if now.0 - at.0 <= self.window_ms {
                break;
            }
            if self.seen.get(key) == Some(at) {
                self.seen.remove(key);
            }
            self.order.pop_front();
        }
    }
}

impl Default for Deduper {
    /// 15 minutes and 50 000 keys: covers EventSub's retry horizon and a busy chat.
    fn default() -> Self {
        Deduper::new(15 * 60 * 1000, 50_000)
    }
}

/// FNV-1a 64: tiny, stable across runs and platforms (std's hasher is randomly seeded,
/// which would make keys differ between restarts).
#[derive(Debug)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Separator so ("ab","c") and ("a","bc") differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, Content};

    fn chat(source: Option<&str>, text: &str, at: i64) -> Event {
        Event::live(
            Platform::Twitch,
            source.map(String::from),
            Millis(at),
            EventKind::ChatMessage {
                actor: Actor::named(Platform::Twitch, "viewer"),
                content: Content::plain(text),
                reply_to: None,
                is_action: false,
            },
        )
    }

    #[test]
    fn repeats_with_the_same_source_id_are_dropped() {
        let mut d = Deduper::default();
        assert!(d.first_time(&chat(Some("m1"), "hi", 0), Millis(0)));
        assert!(!d.first_time(&chat(Some("m1"), "hi", 0), Millis(1_000)));
        assert!(!d.first_time(&chat(Some("m1"), "different text, same id", 0), Millis(60_000)));
        assert!(d.first_time(&chat(Some("m2"), "hi", 0), Millis(1_000)));
    }

    #[test]
    fn same_id_on_different_platforms_is_not_a_repeat() {
        let mut d = Deduper::default();
        let a = chat(Some("1"), "x", 0);
        let mut b = a.clone();
        b.platform = Platform::Kick;
        assert!(d.first_time(&a, Millis(0)));
        assert!(d.first_time(&b, Millis(0)));
    }

    #[test]
    fn keys_expire_after_the_window() {
        let mut d = Deduper::new(1_000, 100);
        assert!(d.first_time(&chat(Some("m"), "x", 0), Millis(0)));
        assert!(!d.first_time(&chat(Some("m"), "x", 0), Millis(900)));
        assert!(d.first_time(&chat(Some("m"), "x", 0), Millis(5_000)));
    }

    #[test]
    fn events_without_ids_dedupe_by_content_briefly() {
        let mut d = Deduper::default();
        assert!(d.first_time(&chat(None, "gg", 1_000), Millis(1_000)));
        assert!(!d.first_time(&chat(None, "gg", 1_000), Millis(1_500)), "double delivery");
        assert!(d.first_time(&chat(None, "gg wp", 1_000), Millis(1_500)), "different text");
        assert!(d.first_time(&chat(None, "gg", 1_000), Millis(20_000)), "same text later is new");
    }

    #[test]
    fn empty_source_id_is_treated_as_missing() {
        assert!(matches!(key_for(&chat(Some(""), "x", 0)), DedupeKey::Hashed(..)));
    }

    #[test]
    fn a_flood_stays_bounded() {
        let mut d = Deduper::new(60_000, 1_000);
        for i in 0..10_000 {
            d.first_time(&chat(Some(&i.to_string()), "spam", 0), Millis(i));
        }
        assert!(d.len() <= 1_000);
    }

    #[test]
    fn hash_separates_field_boundaries() {
        let mut a = Fnv::new();
        a.write(b"ab");
        a.write(b"c");
        let mut b = Fnv::new();
        b.write(b"a");
        b.write(b"bc");
        assert_ne!(a.finish(), b.finish());
    }
}
