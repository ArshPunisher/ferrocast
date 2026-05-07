//! Sample events for every kind a streamer can test (R-33).
//!
//! "Test alert" must always work, even before any platform is connected, so every
//! testable kind has a realistic sample here. The samples are `Origin::Test`.

use crate::event::{Actor, Content, Event, EventKind, Millis, Origin, PaymentStatus, Platform, Tier, TipProvider};
use crate::money::{Money, PlatformValue, ValueUnit};

/// Kinds that have a test button in the dashboard, in the order they're listed.
pub const TESTABLE: [&str; 8] =
    ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid", "reward_redemption"];

/// A sample event of `kind` on `platform`, or `None` if that platform doesn't have the kind
/// (YouTube has no follow event; Bits are Twitch-only).
pub fn sample(kind: &str, platform: Platform, at: Millis) -> Option<Event> {
    let who = |name: &str| Actor::named(platform, name);
    let kind = match (kind, platform) {
        ("follow", Platform::Twitch | Platform::Kick) => EventKind::Follow { actor: who("NovaRiver") },
        ("subscription", Platform::Twitch | Platform::Kick) => EventKind::Subscription {
            actor: who("PixelPanda"),
            tier: Tier::Tier1,
            months: 6,
            message: Some(Content::plain("Six months already, love the streams")),
            gifted_by: None,
        },
        ("subscription", Platform::Youtube) => EventKind::Subscription {
            actor: who("Meera Plays"),
            tier: Tier::Level { id: "lvl1".into(), name: "Member".into() },
            months: 1,
            message: None,
            gifted_by: None,
        },
        ("gift_batch", Platform::Twitch | Platform::Kick | Platform::Youtube) => {
            EventKind::GiftBatch { gifter: who("GiftGoblin"), tier: Tier::Tier1, count: 5 }
        }
        ("platform_gift", Platform::Twitch) => EventKind::PlatformGift {
            actor: who("BitBaron"),
            value: PlatformValue { unit: ValueUnit::Bits, amount: 500 },
            message: Some(Content::plain("Cheer500 clutch play")),
        },
        ("platform_gift", Platform::Kick) => EventKind::PlatformGift {
            actor: who("KickKing"),
            value: PlatformValue { unit: ValueUnit::Kicks, amount: 100 },
            message: None,
        },
        ("paid_message", Platform::Youtube) => EventKind::PaidMessage {
            actor: who("Arjun"),
            money: Money::new(19_900, "INR"),
            content: Some(Content::plain("Super Chat for the win")),
            sticker_url: None,
        },
        ("tip", _) => EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: Money::new(50_000, "INR"),
            message: Some("Keep it up!".into()),
            provider: TipProvider::Razorpay,
            payment_ref: "test".into(),
            status: PaymentStatus::Captured,
        },
        ("raid", Platform::Twitch | Platform::Kick) => EventKind::Raid { from: who("CaptainCozy"), viewers: 42 },
        ("reward_redemption", Platform::Twitch | Platform::Kick) => {
            EventKind::RewardRedemption { actor: who("SnackAttack"), reward: "Hydrate!".into(), cost: 500, input: None }
        }
        _ => return None,
    };
    let platform = if matches!(kind, EventKind::Tip { .. }) { Platform::Tips } else { platform };
    let mut event = Event::live(platform, None, at, kind);
    event.origin = Origin::Test;
    Some(event)
}

/// The first platform that has a sample for `kind`, so a test button works with no setup.
pub fn any_sample(kind: &str, at: Millis) -> Option<Event> {
    [Platform::Twitch, Platform::Youtube, Platform::Kick].into_iter().find_map(|p| sample(kind, p, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_testable_kind_has_a_sample_and_alerts() {
        for kind in TESTABLE {
            let e = any_sample(kind, Millis(1)).unwrap_or_else(|| panic!("no sample for {kind}"));
            assert_eq!(e.kind.name(), kind);
            assert_eq!(e.origin, Origin::Test);
            assert!(e.may_alert(), "{kind} sample should be alertable");
        }
    }

    #[test]
    fn platforms_without_a_kind_return_none() {
        assert!(sample("follow", Platform::Youtube, Millis(0)).is_none());
        assert!(sample("paid_message", Platform::Twitch, Millis(0)).is_none());
        assert!(sample("nonsense", Platform::Twitch, Millis(0)).is_none());
    }

    #[test]
    fn tips_are_on_the_tips_platform() {
        assert_eq!(sample("tip", Platform::Kick, Millis(0)).map(|e| e.platform), Some(Platform::Tips));
    }
}
