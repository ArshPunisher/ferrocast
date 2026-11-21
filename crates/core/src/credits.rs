//! Session end credits: collects followers, subscribers, gifters, bits/tips givers,
//! raiders, and human moderators for the scrolling End Credits widget.

use serde::{Deserialize, Serialize};

use crate::event::{Actor, Event, EventKind, Millis, Origin, PaymentStatus, Platform, Roles};
use crate::money::{Money, ValueUnit};
use crate::recap::{Person, Raider, Subscriber};

/// Cap per credits section so a massive raid/bot wave cannot bloat snapshots or frames.
pub const MAX_CREDITS_ROWS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditGifter {
    pub platform: Platform,
    pub name: String,
    pub gifts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditTipper {
    pub platform: Platform,
    pub name: String,
    pub amounts: Vec<Money>,
    pub bits: u64,
    #[serde(default)]
    pub kicks: u64,
    #[serde(default)]
    pub jewels: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credits {
    pub started_at: Option<Millis>,
    pub followers: Vec<Person>,
    pub subs: Vec<Subscriber>,
    pub gifters: Vec<CreditGifter>,
    pub tippers: Vec<CreditTipper>,
    pub raiders: Vec<Raider>,
    pub mods: Vec<Person>,
}

fn clean_name(actor: &Actor, allow_anon: bool) -> Option<String> {
    if actor.anonymous {
        return allow_anon.then(|| "Anonymous".to_string());
    }
    let name = actor.display_name.trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

fn add_money(list: &mut Vec<Money>, money: &Money) {
    if let Some(existing) = list.iter_mut().find(|m| m.currency == money.currency) {
        existing.minor = existing.minor.saturating_add(money.minor);
    } else if list.len() < 16 {
        list.push(money.clone());
    }
}

impl Credits {
    pub fn new(started_at: Option<Millis>) -> Self {
        Self { started_at, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.followers.is_empty()
            && self.subs.is_empty()
            && self.gifters.is_empty()
            && self.tippers.is_empty()
            && self.raiders.is_empty()
            && self.mods.is_empty()
    }

    /// Incorporates one live event into session credits. Returns `true` when credits data changed
    /// so the engine only broadcasts overlay updates when something new appears.
    pub fn record(&mut self, event: &Event) -> bool {
        if event.origin != Origin::Live {
            return false;
        }
        if self.started_at.is_none() {
            self.started_at = Some(event.occurred_at);
        }
        match &event.kind {
            EventKind::Follow { actor } => {
                let Some(name) = clean_name(actor, false) else { return false };
                self.push_person_follow(event.platform, name)
            }
            EventKind::Subscription { actor, months, gifted_by, .. } => {
                if gifted_by.is_some() {
                    return false;
                }
                let Some(name) = clean_name(actor, false) else { return false };
                self.upsert_sub(event.platform, name, *months)
            }
            EventKind::GiftBatch { gifter, count, .. } => {
                let Some(name) = clean_name(gifter, true) else { return false };
                if *count == 0 {
                    return false;
                }
                self.upsert_gifter(event.platform, name, *count)
            }
            EventKind::Tip { actor, money, status, .. } => {
                if *status != PaymentStatus::Captured {
                    return false;
                }
                let Some(name) = clean_name(actor, true) else { return false };
                self.upsert_tipper_money(event.platform, name, money)
            }
            EventKind::PaidMessage { actor, money, .. } => {
                let Some(name) = clean_name(actor, true) else { return false };
                self.upsert_tipper_money(event.platform, name, money)
            }
            EventKind::PlatformGift { actor, value, .. } => {
                if value.amount == 0 {
                    return false;
                }
                let Some(name) = clean_name(actor, true) else { return false };
                self.upsert_tipper_gift(event.platform, name, &value.unit, value.amount)
            }
            EventKind::Raid { from, viewers } => {
                let Some(name) = clean_name(from, false) else { return false };
                self.upsert_raider(event.platform, name, *viewers)
            }
            EventKind::ChatMessage { actor, .. } => {
                if !actor.roles.contains(Roles::MODERATOR) || actor.roles.contains(Roles::BOT) {
                    return false;
                }
                let Some(name) = clean_name(actor, false) else { return false };
                self.push_mod(event.platform, name)
            }
            _ => false,
        }
    }

    fn push_person_follow(&mut self, platform: Platform, name: String) -> bool {
        if self.followers.iter().any(|p| p.platform == platform && p.name.eq_ignore_ascii_case(&name)) {
            return false;
        }
        if self.followers.len() >= MAX_CREDITS_ROWS {
            return false;
        }
        self.followers.push(Person { platform, name });
        true
    }

    fn push_mod(&mut self, platform: Platform, name: String) -> bool {
        if self.mods.iter().any(|p| p.platform == platform && p.name.eq_ignore_ascii_case(&name)) {
            return false;
        }
        if self.mods.len() >= MAX_CREDITS_ROWS {
            return false;
        }
        self.mods.push(Person { platform, name });
        self.mods.sort_by_cached_key(|a| a.name.to_ascii_lowercase());
        true
    }

    fn upsert_sub(&mut self, platform: Platform, name: String, months: u32) -> bool {
        let months = months.max(1);
        if let Some(sub) = self.subs.iter_mut().find(|s| s.platform == platform && s.name.eq_ignore_ascii_case(&name)) {
            if months > sub.months {
                sub.months = months;
                self.subs.sort_by(|a, b| {
                    b.months.cmp(&a.months).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                });
                return true;
            }
            return false;
        }
        if self.subs.len() >= MAX_CREDITS_ROWS {
            return false;
        }
        self.subs.push(Subscriber { platform, name, months, gifts: 0 });
        self.subs.sort_by(|a, b| {
            b.months.cmp(&a.months).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
        });
        true
    }

    fn upsert_gifter(&mut self, platform: Platform, name: String, count: u32) -> bool {
        if let Some(g) = self.gifters.iter_mut().find(|g| g.platform == platform && g.name.eq_ignore_ascii_case(&name))
        {
            g.gifts = g.gifts.saturating_add(count);
        } else if self.gifters.len() < MAX_CREDITS_ROWS {
            self.gifters.push(CreditGifter { platform, name, gifts: count });
        } else {
            return false;
        }
        self.gifters.sort_by(|a, b| {
            b.gifts.cmp(&a.gifts).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
        });
        true
    }

    fn find_or_insert_tipper(&mut self, platform: Platform, name: String) -> Option<&mut CreditTipper> {
        if let Some(idx) =
            self.tippers.iter().position(|t| t.platform == platform && t.name.eq_ignore_ascii_case(&name))
        {
            return Some(&mut self.tippers[idx]);
        }
        if self.tippers.len() >= MAX_CREDITS_ROWS {
            return None;
        }
        self.tippers.push(CreditTipper { platform, name, amounts: Vec::new(), bits: 0, kicks: 0, jewels: 0 });
        self.tippers.last_mut()
    }

    fn upsert_tipper_money(&mut self, platform: Platform, name: String, money: &Money) -> bool {
        let Some(t) = self.find_or_insert_tipper(platform, name) else { return false };
        add_money(&mut t.amounts, money);
        self.sort_tippers();
        true
    }

    fn upsert_tipper_gift(&mut self, platform: Platform, name: String, unit: &ValueUnit, amount: u64) -> bool {
        let Some(t) = self.find_or_insert_tipper(platform, name) else { return false };
        match unit {
            ValueUnit::Bits => t.bits = t.bits.saturating_add(amount),
            ValueUnit::Kicks => t.kicks = t.kicks.saturating_add(amount),
            ValueUnit::Jewels => t.jewels = t.jewels.saturating_add(amount),
            ValueUnit::Other(_) => {}
        }
        self.sort_tippers();
        true
    }

    fn sort_tippers(&mut self) {
        self.tippers.sort_by(|a, b| {
            let score = |t: &CreditTipper| {
                let money_sum: i64 = t.amounts.iter().map(|m| m.minor.max(0)).sum();
                (money_sum, t.bits.saturating_add(t.kicks).saturating_add(t.jewels))
            };
            score(b).cmp(&score(a)).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
        });
    }

    fn upsert_raider(&mut self, platform: Platform, name: String, viewers: u32) -> bool {
        if let Some(r) = self.raiders.iter_mut().find(|r| r.platform == platform && r.name.eq_ignore_ascii_case(&name))
        {
            r.viewers = r.viewers.saturating_add(viewers);
        } else if self.raiders.len() < MAX_CREDITS_ROWS {
            self.raiders.push(Raider { platform, name, viewers });
        } else {
            return false;
        }
        self.raiders.sort_by(|a, b| {
            b.viewers.cmp(&a.viewers).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
        });
        true
    }
}

pub fn build_credits(events: &[Event], started_at: Option<Millis>) -> Credits {
    let mut credits = Credits::new(started_at);
    for event in events {
        if let Some(start) = started_at
            && event.occurred_at < start
        {
            continue;
        }
        credits.record(event);
    }
    credits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Content, Tier, TipProvider};
    use crate::money::PlatformValue;

    fn actor(platform: Platform, name: &str, roles: Roles) -> Actor {
        Actor {
            platform,
            id: Some(name.to_ascii_lowercase()),
            login: Some(name.to_ascii_lowercase()),
            display_name: name.into(),
            avatar_url: None,
            color: None,
            roles,
            badges: Vec::new(),
            anonymous: false,
        }
    }

    fn ev(platform: Platform, at: i64, kind: EventKind) -> Event {
        Event::live(platform, None, Millis(at), kind)
    }

    #[test]
    fn collects_all_credit_categories_and_ignores_test_events_and_bots() {
        let mut bot_roles = Roles::MODERATOR;
        bot_roles.insert(Roles::BOT);
        let mut mod_bot = actor(Platform::Twitch, "Nightbot", bot_roles);
        mod_bot.id = Some("bot-1".into());
        let events = vec![
            Event {
                origin: Origin::Test,
                ..ev(
                    Platform::Twitch,
                    50,
                    EventKind::Follow { actor: actor(Platform::Twitch, "FakeTest", Roles::empty()) },
                )
            },
            ev(
                Platform::Twitch,
                100,
                EventKind::Follow { actor: actor(Platform::Twitch, "NovaRiver", Roles::empty()) },
            ),
            ev(
                Platform::Twitch,
                110,
                EventKind::Follow { actor: actor(Platform::Twitch, "novariver", Roles::empty()) },
            ),
            ev(
                Platform::Twitch,
                120,
                EventKind::Subscription {
                    actor: actor(Platform::Twitch, "PixelPanda", Roles::SUBSCRIBER),
                    tier: Tier::Tier1,
                    months: 6,
                    message: None,
                    gifted_by: None,
                },
            ),
            ev(
                Platform::Twitch,
                130,
                EventKind::Subscription {
                    actor: actor(Platform::Twitch, "LuckyOne", Roles::SUBSCRIBER),
                    tier: Tier::Tier1,
                    months: 1,
                    message: None,
                    gifted_by: Some(Actor::anonymous(Platform::Twitch)),
                },
            ),
            ev(
                Platform::Twitch,
                140,
                EventKind::GiftBatch {
                    gifter: actor(Platform::Twitch, "GiftGoblin", Roles::empty()),
                    tier: Tier::Tier1,
                    count: 5,
                },
            ),
            ev(
                Platform::Twitch,
                145,
                EventKind::GiftBatch {
                    gifter: actor(Platform::Twitch, "GiftGoblin", Roles::empty()),
                    tier: Tier::Tier1,
                    count: 2,
                },
            ),
            ev(
                Platform::Tips,
                150,
                EventKind::Tip {
                    actor: actor(Platform::Tips, "Asha", Roles::empty()),
                    money: Money::new(50_000, "INR"),
                    message: None,
                    provider: TipProvider::Razorpay,
                    payment_ref: "r1".into(),
                    status: PaymentStatus::Captured,
                },
            ),
            ev(
                Platform::Tips,
                155,
                EventKind::Tip {
                    actor: actor(Platform::Tips, "PendingTip", Roles::empty()),
                    money: Money::new(10_000, "INR"),
                    message: None,
                    provider: TipProvider::Razorpay,
                    payment_ref: "r2".into(),
                    status: PaymentStatus::Pending,
                },
            ),
            ev(
                Platform::Twitch,
                160,
                EventKind::PlatformGift {
                    actor: actor(Platform::Twitch, "BitBaron", Roles::empty()),
                    value: PlatformValue { unit: ValueUnit::Bits, amount: 500 },
                    message: None,
                },
            ),
            ev(
                Platform::Kick,
                165,
                EventKind::PlatformGift {
                    actor: actor(Platform::Kick, "KickKing", Roles::empty()),
                    value: PlatformValue { unit: ValueUnit::Kicks, amount: 100 },
                    message: None,
                },
            ),
            ev(
                Platform::Twitch,
                170,
                EventKind::Raid { from: actor(Platform::Twitch, "CaptainCozy", Roles::empty()), viewers: 42 },
            ),
            ev(
                Platform::Youtube,
                180,
                EventKind::ChatMessage {
                    actor: actor(Platform::Youtube, "ModMaya", Roles::MODERATOR),
                    content: Content::plain("hi"),
                    reply_to: None,
                    is_action: false,
                },
            ),
            ev(
                Platform::Twitch,
                190,
                EventKind::ChatMessage {
                    actor: mod_bot,
                    content: Content::plain("!cmds"),
                    reply_to: None,
                    is_action: false,
                },
            ),
        ];

        let mut credits = Credits::new(Some(Millis(80)));
        let mut recorded_flags = Vec::new();
        for e in &events {
            if e.occurred_at >= Millis(80) {
                recorded_flags.push(credits.record(e));
            }
        }
        assert_eq!(credits.followers.len(), 1);
        assert_eq!(credits.followers[0].name, "NovaRiver");
        assert_eq!(credits.subs.len(), 1);
        assert_eq!(credits.subs[0].name, "PixelPanda");
        assert_eq!(credits.subs[0].months, 6);
        assert_eq!(credits.gifters.len(), 1);
        assert_eq!(credits.gifters[0].gifts, 7);
        assert_eq!(credits.tippers.len(), 3);
        assert!(credits.tippers.iter().any(|t| t.name == "Asha" && t.amounts[0].minor == 50_000));
        assert!(credits.tippers.iter().any(|t| t.name == "BitBaron" && t.bits == 500));
        assert!(credits.tippers.iter().any(|t| t.name == "KickKing" && t.kicks == 100));
        assert_eq!(credits.raiders.len(), 1);
        assert_eq!(credits.raiders[0].viewers, 42);
        assert_eq!(credits.mods.len(), 1);
        assert_eq!(credits.mods[0].name, "ModMaya");
        // Repeating the same mod chat message does not mark credits changed.
        assert!(!credits.record(&ev(
            Platform::Youtube,
            200,
            EventKind::ChatMessage {
                actor: actor(Platform::Youtube, "ModMaya", Roles::MODERATOR),
                content: Content::plain("second line"),
                reply_to: None,
                is_action: false,
            }
        )));
    }

    #[test]
    fn enforces_max_rows_cap_per_section() {
        let mut credits = Credits::new(Some(Millis(0)));
        for i in 0..(MAX_CREDITS_ROWS + 50) {
            credits.record(&ev(
                Platform::Twitch,
                i as i64,
                EventKind::Follow { actor: actor(Platform::Twitch, &format!("User{i}"), Roles::empty()) },
            ));
            credits.record(&ev(
                Platform::Twitch,
                i as i64,
                EventKind::ChatMessage {
                    actor: actor(Platform::Twitch, &format!("Mod{i}"), Roles::MODERATOR),
                    content: Content::plain("ok"),
                    reply_to: None,
                    is_action: false,
                },
            ));
        }
        assert_eq!(credits.followers.len(), MAX_CREDITS_ROWS);
        assert_eq!(credits.mods.len(), MAX_CREDITS_ROWS);
    }
}
