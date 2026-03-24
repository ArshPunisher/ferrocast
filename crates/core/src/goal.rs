//! Goals ("100 followers today", "₹5,000 for a new mic") and how events count toward them.
//!
//! Each event's contribution is computed here and remembered by the store against the
//! event's source id, so a refund or chargeback can take exactly that amount back (R-32).

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, Millis, Origin};
use crate::money::ValueUnit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalKind {
    Follows,
    /// New subscriptions and memberships, counting a gift batch of 5 as 5.
    Subs,
    /// Tips and paid messages in the goal's currency, in minor units.
    Tips,
    Bits,
    /// Moved only by hand from the dashboard.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub kind: GoalKind,
    /// In the goal's unit: count, bits, or minor units of `currency` for tips.
    pub target: i64,
    pub current: i64,
    /// Required for `Tips`. Money in another currency doesn't count: converting would make
    /// the bar lie about what actually arrived.
    pub currency: Option<String>,
    pub starts_at: Millis,
    pub ends_at: Option<Millis>,
}

impl Goal {
    /// 0.0 to 1.0 for the progress bar (can exceed 1.0 when the goal is beaten).
    pub fn progress(&self) -> f64 {
        if self.target <= 0 {
            1.0
        } else {
            self.current.max(0) as f64 / self.target as f64
        }
    }

    pub fn is_active(&self, now: Millis) -> bool {
        now >= self.starts_at && self.ends_at.is_none_or(|end| now < end)
    }
}

/// How much `event` adds to `goal`, or 0. Test and backfill events never count: a test
/// alert must not move a real goal, and backfilled follows were counted when they happened
/// or belong to a time the goal didn't cover.
pub fn contribution(goal: &Goal, event: &Event) -> i64 {
    if !matches!(event.origin, Origin::Live) || !goal.is_active(event.occurred_at) {
        return 0;
    }
    match (goal.kind, &event.kind) {
        (GoalKind::Follows, EventKind::Follow { .. }) => 1,
        (
            GoalKind::Subs,
            EventKind::Subscription {
                gifted_by: None, ..
            },
        ) => 1,
        (GoalKind::Subs, EventKind::GiftBatch { count, .. }) => i64::from(*count),
        (GoalKind::Bits, EventKind::PlatformGift { value, .. })
            if value.unit == ValueUnit::Bits =>
        {
            i64::try_from(value.amount).unwrap_or(i64::MAX)
        }
        (GoalKind::Tips, EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. })
            if goal
                .currency
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(&money.currency)) =>
        {
            // Tip events only alert once captured; the same rule keeps pending money out.
            if let EventKind::Tip { status, .. } = &event.kind
                && *status != crate::event::PaymentStatus::Captured
            {
                return 0;
            }
            money.minor.max(0)
        }
        _ => 0,
    }
}

/// Applies a contribution, saturating instead of overflowing.
pub fn apply(goal: &mut Goal, delta: i64) {
    goal.current = goal.current.saturating_add(delta);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, PaymentStatus, Platform, Tier, TipProvider};
    use crate::money::{Money, PlatformValue};

    fn goal(kind: GoalKind, currency: Option<&str>) -> Goal {
        Goal {
            id: "g".into(),
            title: "t".into(),
            kind,
            target: 100,
            current: 0,
            currency: currency.map(String::from),
            starts_at: Millis(0),
            ends_at: Some(Millis(10_000)),
        }
    }

    fn live(kind: EventKind) -> Event {
        Event::live(Platform::Twitch, Some("x".into()), Millis(5), kind)
    }

    fn tip(minor: i64, cur: &str, status: PaymentStatus) -> Event {
        live(EventKind::Tip {
            actor: Actor::named(Platform::Tips, "a"),
            money: Money::new(minor, cur),
            message: None,
            provider: TipProvider::Razorpay,
            payment_ref: "p".into(),
            status,
        })
    }

    #[test]
    fn subs_count_batches_not_recipients() {
        let g = goal(GoalKind::Subs, None);
        let batch = live(EventKind::GiftBatch {
            gifter: Actor::named(Platform::Twitch, "g"),
            tier: Tier::Tier1,
            count: 5,
        });
        let recipient = live(EventKind::Subscription {
            actor: Actor::named(Platform::Twitch, "r"),
            tier: Tier::Tier1,
            months: 1,
            message: None,
            gifted_by: Some(Actor::named(Platform::Twitch, "g")),
        });
        assert_eq!(contribution(&g, &batch), 5);
        assert_eq!(contribution(&g, &recipient), 0);
    }

    #[test]
    fn tips_only_count_captured_money_in_the_goal_currency() {
        let g = goal(GoalKind::Tips, Some("INR"));
        assert_eq!(
            contribution(&g, &tip(50_000, "INR", PaymentStatus::Captured)),
            50_000
        );
        assert_eq!(
            contribution(&g, &tip(50_000, "INR", PaymentStatus::Pending)),
            0
        );
        assert_eq!(
            contribution(&g, &tip(500, "USD", PaymentStatus::Captured)),
            0
        );
        assert_eq!(
            contribution(
                &goal(GoalKind::Tips, None),
                &tip(1, "INR", PaymentStatus::Captured)
            ),
            0
        );
    }

    #[test]
    fn tests_backfill_and_out_of_window_events_never_count() {
        let g = goal(GoalKind::Follows, None);
        let mut e = live(EventKind::Follow {
            actor: Actor::named(Platform::Twitch, "f"),
        });
        assert_eq!(contribution(&g, &e), 1);
        e.origin = Origin::Test;
        assert_eq!(contribution(&g, &e), 0);
        e.origin = Origin::Backfill;
        assert_eq!(contribution(&g, &e), 0);
        e.origin = Origin::Live;
        e.occurred_at = Millis(20_000);
        assert_eq!(contribution(&g, &e), 0);
    }

    #[test]
    fn bits_only_count_bits() {
        let g = goal(GoalKind::Bits, None);
        let bits = live(EventKind::PlatformGift {
            actor: Actor::named(Platform::Twitch, "b"),
            value: PlatformValue {
                unit: ValueUnit::Bits,
                amount: 500,
            },
            message: None,
        });
        let kicks = live(EventKind::PlatformGift {
            actor: Actor::named(Platform::Kick, "k"),
            value: PlatformValue {
                unit: ValueUnit::Kicks,
                amount: 500,
            },
            message: None,
        });
        assert_eq!(contribution(&g, &bits), 500);
        assert_eq!(contribution(&g, &kicks), 0);
    }

    #[test]
    fn apply_saturates_and_progress_handles_zero_targets() {
        let mut g = goal(GoalKind::Custom, None);
        g.current = i64::MAX - 1;
        apply(&mut g, 10);
        assert_eq!(g.current, i64::MAX);
        g.target = 0;
        assert_eq!(g.progress(), 1.0);
        g.target = 200;
        g.current = -50;
        assert_eq!(g.progress(), 0.0);
    }
}
