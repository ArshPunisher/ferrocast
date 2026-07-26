//! The end-of-stream recap: who followed, subbed, tipped and raided, who chatted most, and
//! a thank-you list, built from the event log since the stream started.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, Millis, Origin, PaymentStatus, Platform};
use crate::money::{Money, ValueUnit};

/// Longest list of each kind (a huge stream still gives a readable page).
pub const MAX_ROWS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub platform: Platform,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscriber {
    pub platform: Platform,
    pub name: String,
    /// Months for their own sub (0 when they only gifted).
    pub months: u32,
    /// Subs they gifted.
    pub gifts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tipper {
    pub platform: Platform,
    pub name: String,
    /// One per currency, never converted.
    pub amounts: Vec<Money>,
    pub bits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Raider {
    pub platform: Platform,
    pub name: String,
    pub viewers: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chatter {
    pub platform: Platform,
    pub name: String,
    pub messages: u64,
}

/// This stream's totals (from the live session when it's the same stream).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub follows: u64,
    pub subs: u64,
    pub tips: Vec<Money>,
    pub bits: u64,
    pub messages: u64,
    pub peak_viewers: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recap {
    pub started_at: Option<Millis>,
    pub ended_at: Option<Millis>,
    pub live: bool,
    #[serde(flatten)]
    pub totals: Totals,
    pub new_followers: Vec<Person>,
    pub subscribers: Vec<Subscriber>,
    pub tippers: Vec<Tipper>,
    pub raiders: Vec<Raider>,
    pub top_chatters: Vec<Chatter>,
    /// Everyone to thank, supporters first, each name once.
    pub thanks: Vec<String>,
}

fn cap<T>(mut v: Vec<T>) -> Vec<T> {
    v.truncate(MAX_ROWS);
    v
}

fn add_money(list: &mut Vec<Money>, m: &Money) {
    match list.iter_mut().find(|x| x.currency == m.currency) {
        Some(x) => x.minor = x.minor.saturating_add(m.minor),
        None => list.push(m.clone()),
    }
}

/// Builds the recap from live events in order. `session` totals win when given (they
/// already count refunds and the viewer peak); otherwise totals come from the events.
pub fn build(
    started_at: Option<Millis>,
    ended_at: Option<Millis>,
    live: bool,
    events: &[Event],
    top_chatters: Vec<Chatter>,
    messages: u64,
    session: Option<Totals>,
) -> Recap {
    let reversed: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Reversal { original_source_id, .. } => Some(original_source_id.as_str()),
            _ => None,
        })
        .collect();
    let mut followers: Vec<Person> = Vec::new();
    let mut subs: Vec<Subscriber> = Vec::new();
    let mut tippers: Vec<Tipper> = Vec::new();
    let mut raiders: Vec<Raider> = Vec::new();
    let mut seen_follow: HashMap<(Platform, String), ()> = HashMap::new();
    let mut counted = Totals { messages, ..Totals::default() };
    for e in events.iter().filter(|e| e.origin == Origin::Live) {
        let refunded = e.source_id.as_deref().is_some_and(|s| reversed.contains(s));
        match &e.kind {
            EventKind::Follow { actor } => {
                if seen_follow.insert((e.platform, actor.display_name.clone()), ()).is_none() {
                    counted.follows += 1;
                    followers.push(Person { platform: e.platform, name: actor.display_name.clone() });
                }
            }
            EventKind::Subscription { actor, months, gifted_by: None, .. } => {
                counted.subs += 1;
                match subs.iter_mut().find(|s| s.platform == e.platform && s.name == actor.display_name) {
                    Some(s) => s.months = s.months.max(*months),
                    None => subs.push(Subscriber {
                        platform: e.platform,
                        name: actor.display_name.clone(),
                        months: *months,
                        gifts: 0,
                    }),
                }
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                counted.subs += u64::from(*count);
                match subs.iter_mut().find(|s| s.platform == e.platform && s.name == gifter.display_name) {
                    Some(s) => s.gifts = s.gifts.saturating_add(*count),
                    None => subs.push(Subscriber {
                        platform: e.platform,
                        name: gifter.display_name.clone(),
                        months: 0,
                        gifts: *count,
                    }),
                }
            }
            EventKind::Tip { actor, money, status: PaymentStatus::Captured, .. }
            | EventKind::PaidMessage { actor, money, .. }
                if !refunded =>
            {
                add_money(&mut counted.tips, money);
                let name = if actor.anonymous { "Anonymous".to_string() } else { actor.display_name.clone() };
                match tippers.iter_mut().find(|t| t.platform == e.platform && t.name == name) {
                    Some(t) => add_money(&mut t.amounts, money),
                    None => tippers.push(Tipper { platform: e.platform, name, amounts: vec![money.clone()], bits: 0 }),
                }
            }
            EventKind::PlatformGift { actor, value, .. } if value.unit == ValueUnit::Bits => {
                counted.bits = counted.bits.saturating_add(value.amount);
                match tippers.iter_mut().find(|t| t.platform == e.platform && t.name == actor.display_name) {
                    Some(t) => t.bits = t.bits.saturating_add(value.amount),
                    None => tippers.push(Tipper {
                        platform: e.platform,
                        name: actor.display_name.clone(),
                        amounts: vec![],
                        bits: value.amount,
                    }),
                }
            }
            EventKind::Raid { from, viewers } => {
                raiders.push(Raider { platform: e.platform, name: from.display_name.clone(), viewers: *viewers })
            }
            _ => {}
        }
    }
    let mut thanks: Vec<String> = Vec::new();
    let names = subs
        .iter()
        .map(|s| &s.name)
        .chain(tippers.iter().map(|t| &t.name))
        .chain(raiders.iter().map(|r| &r.name))
        .chain(followers.iter().map(|f| &f.name));
    for n in names {
        if n != "Anonymous" && !thanks.iter().any(|t| t.eq_ignore_ascii_case(n)) && thanks.len() < MAX_ROWS {
            thanks.push(n.clone());
        }
    }
    let totals = session.unwrap_or(counted);
    Recap {
        started_at,
        ended_at,
        live,
        totals,
        new_followers: cap(followers),
        subscribers: cap(subs),
        tippers: cap(tippers),
        raiders: cap(raiders),
        top_chatters,
        thanks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, ReversalReason, Tier, TipProvider};
    use crate::money::PlatformValue;

    fn ev(p: Platform, src: &str, kind: EventKind) -> Event {
        Event::live(p, Some(src.into()), Millis(1), kind)
    }
    fn who(n: &str) -> Actor {
        Actor::named(Platform::Twitch, n)
    }
    fn tip(src: &str, n: &str, minor: i64) -> Event {
        ev(
            Platform::Tips,
            src,
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, n),
                money: Money::new(minor, "INR"),
                message: None,
                provider: TipProvider::Razorpay,
                payment_ref: src.into(),
                status: PaymentStatus::Captured,
            },
        )
    }

    #[test]
    fn builds_lists_totals_and_thanks_without_refunded_tips() {
        let mut test_follow = ev(Platform::Twitch, "t", EventKind::Follow { actor: who("Tester") });
        test_follow.origin = Origin::Backfill;
        let events = vec![
            ev(Platform::Twitch, "f1", EventKind::Follow { actor: who("Ana") }),
            ev(Platform::Twitch, "f2", EventKind::Follow { actor: who("Ana") }),
            test_follow,
            ev(
                Platform::Twitch,
                "s1",
                EventKind::Subscription {
                    actor: who("Bo"),
                    tier: Tier::Tier1,
                    months: 7,
                    message: None,
                    gifted_by: None,
                },
            ),
            ev(Platform::Twitch, "g1", EventKind::GiftBatch { gifter: who("Bo"), tier: Tier::Tier1, count: 5 }),
            tip("p1", "Cy", 50_000),
            tip("p2", "Cy", 10_000),
            tip("p3", "Di", 99_900),
            ev(
                Platform::Tips,
                "r",
                EventKind::Reversal { original_source_id: "p3".into(), reason: ReversalReason::Refund },
            ),
            ev(
                Platform::Twitch,
                "b1",
                EventKind::PlatformGift {
                    actor: who("Ana"),
                    value: PlatformValue { unit: ValueUnit::Bits, amount: 500 },
                    message: None,
                },
            ),
            ev(Platform::Kick, "raid", EventKind::Raid { from: who("Friend"), viewers: 40 }),
        ];
        let chat = vec![Chatter { platform: Platform::Twitch, name: "Ana".into(), messages: 30 }];
        let r = build(Some(Millis(0)), None, true, &events, chat, 77, None);
        assert_eq!(r.new_followers.len(), 1, "a refollow counts once; backfill not at all");
        assert_eq!(
            r.subscribers,
            vec![Subscriber { platform: Platform::Twitch, name: "Bo".into(), months: 7, gifts: 5 }]
        );
        assert_eq!(r.totals.subs, 6);
        assert_eq!(r.totals.tips, vec![Money::new(60_000, "INR")], "the refunded tip is gone");
        assert_eq!(r.tippers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Cy", "Ana"]);
        assert_eq!(r.totals.bits, 500);
        assert_eq!(r.totals.messages, 77);
        assert_eq!(r.thanks, vec!["Bo", "Cy", "Ana", "Friend"], "supporters first, each once");
        let s = Totals { follows: 9, peak_viewers: 120, ..Totals::default() };
        let r = build(Some(Millis(0)), None, true, &events, vec![], 0, Some(s.clone()));
        assert_eq!(r.totals, s, "the live session's numbers win");
    }
}
