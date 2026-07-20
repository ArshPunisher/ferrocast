//! Loyalty points: earned for chatting while live and for supporting the stream, shared by
//! one person across platforms (through linked viewer identities), and spent on things
//! like giveaway entries.
//!
//! Everything here is pure: the engine passes time in, and the store keeps the balances
//! (never below zero, never above [`MAX_BALANCE`]).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::{Actor, Event, EventKind, Millis, Origin, PaymentStatus, Platform, Roles};

/// The most points anyone can hold. Far beyond any real stream, small enough that adding
/// two balances can never overflow an `i64`.
pub const MAX_BALANCE: i64 = 1_000_000_000_000;
/// The largest single change a mod or the dashboard can make.
pub const MAX_ADJUST: i64 = 1_000_000_000;
/// The largest amount any one rule (interval or bonus) can award.
pub const MAX_AWARD: u32 = 1_000_000;

/// How points are earned and what they're called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoyaltyConfig {
    pub enabled: bool,
    /// What chat calls them: "points", "coins", "beans". `!points` always works too.
    pub name: String,
    /// Points for each viewer who chatted during the interval while live.
    pub per_interval: u32,
    /// The interval, in minutes.
    pub interval_min: u32,
    pub follow_bonus: u32,
    /// Per new sub, and per gifted sub for the gifter.
    pub sub_bonus: u32,
    /// Per tip, Super Chat, Bits cheer or Kicks gift.
    pub tip_bonus: u32,
    /// For the streamer who raided in.
    pub raid_bonus: u32,
}

impl Default for LoyaltyConfig {
    fn default() -> Self {
        LoyaltyConfig {
            enabled: true,
            name: "points".into(),
            per_interval: 10,
            interval_min: 5,
            follow_bonus: 50,
            sub_bonus: 250,
            tip_bonus: 100,
            raid_bonus: 100,
        }
    }
}

impl LoyaltyConfig {
    /// Checks every field; the message names the field.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 20 {
            return Err("name: call them something 1 to 20 characters long, like points.".into());
        }
        if !name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_') {
            return Err("name: use letters, numbers, spaces, - and _ only.".into());
        }
        if !(1..=120).contains(&self.interval_min) {
            return Err("interval_min: must be between 1 and 120 minutes.".into());
        }
        for (field, v) in [
            ("per_interval", self.per_interval),
            ("follow_bonus", self.follow_bonus),
            ("sub_bonus", self.sub_bonus),
            ("tip_bonus", self.tip_bonus),
            ("raid_bonus", self.raid_bonus),
        ] {
            if v > MAX_AWARD {
                return Err(format!("{field}: must be between 0 and 1,000,000."));
            }
        }
        Ok(())
    }

    /// The command word for the balance in this name ("!coins"), if it's one word.
    pub fn name_command(&self) -> Option<String> {
        let n = self.name.trim().to_lowercase();
        (!n.is_empty() && !n.contains(' ') && n != "points").then(|| format!("!{n}"))
    }
}

/// Bots that chat in many channels. They never earn points or enter giveaways.
pub const KNOWN_BOTS: &[&str] = &[
    "nightbot",
    "streamelements",
    "streamlabs",
    "moobot",
    "fossabot",
    "wizebot",
    "botrix",
    "botrixoficial",
    "sery_bot",
    "kofistreambot",
    "soundalerts",
    "commanderroot",
    "streamstickers",
    "pokemoncommunitygame",
    "own3d",
    "frostytoolsdotcom",
    "kickbot",
];

pub fn is_known_bot(login: &str) -> bool {
    let l = login.trim().trim_start_matches('@').to_lowercase();
    KNOWN_BOTS.contains(&l.as_str())
}

/// Whether this person can earn points (and enter giveaways): a real, named viewer, not
/// the streamer and not a bot.
pub fn is_audience(actor: &Actor) -> bool {
    if actor.id.is_none() || actor.anonymous {
        return false;
    }
    if actor.roles.contains(Roles::BROADCASTER) || actor.roles.contains(Roles::BOT) {
        return false;
    }
    let login = actor.login.as_deref().unwrap_or(&actor.display_name);
    !is_known_bot(login) && !is_known_bot(&actor.display_name)
}

/// Who a support event rewards, and with how much. Only live events count: a test alert,
/// a replay or a backfilled follow never pays out.
pub fn bonus<'a>(cfg: &LoyaltyConfig, event: &'a Event) -> Option<(&'a Actor, u32)> {
    if !cfg.enabled || event.origin != Origin::Live {
        return None;
    }
    let (actor, amount) = match &event.kind {
        EventKind::Follow { actor } => (actor, cfg.follow_bonus),
        EventKind::Subscription { actor, gifted_by: None, .. } => (actor, cfg.sub_bonus),
        EventKind::GiftBatch { gifter, count, .. } => (gifter, cfg.sub_bonus.saturating_mul(*count).min(MAX_AWARD)),
        EventKind::Tip { actor, status: PaymentStatus::Captured, .. }
        | EventKind::PaidMessage { actor, .. }
        | EventKind::PlatformGift { actor, .. } => (actor, cfg.tip_bonus),
        EventKind::Raid { from, .. } => (from, cfg.raid_bonus),
        _ => return None,
    };
    (amount > 0 && is_audience(actor)).then_some((actor, amount))
}

/// Who chatted during the current interval while live. The engine asks [`Activity::due`]
/// regularly; when an interval has passed it hands back everyone to pay and starts over.
#[derive(Debug, Default)]
pub struct Activity {
    started: Option<Millis>,
    active: HashMap<(Platform, String), Actor>,
}

/// Upper bound on the people tracked per interval, so a bot flood can't grow memory.
const MAX_ACTIVE: usize = 50_000;

impl Activity {
    /// Notes a chat message from `actor` while their platform is live.
    pub fn saw(&mut self, actor: &Actor) {
        let Some(id) = actor.id.clone() else { return };
        if !is_audience(actor) {
            return;
        }
        if self.active.len() >= MAX_ACTIVE && !self.active.contains_key(&(actor.platform, id.clone())) {
            return;
        }
        self.active.insert((actor.platform, id), actor.clone());
    }

    /// Everyone to pay now, or nothing yet. Offline clears the slate: chat between streams
    /// earns nothing.
    pub fn due(&mut self, cfg: &LoyaltyConfig, live: bool, now: Millis) -> Vec<Actor> {
        if !cfg.enabled || !live {
            self.started = None;
            self.active.clear();
            return Vec::new();
        }
        let start = *self.started.get_or_insert(now);
        let every_s = i64::from(cfg.interval_min.max(1)) * 60;
        if now.seconds_since(start) < every_s {
            return Vec::new();
        }
        self.started = Some(now);
        std::mem::take(&mut self.active).into_values().collect()
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

/// A points command from chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointsCommand {
    /// `!points`: the sender's own balance.
    Balance,
    /// `!addpoints user N` (mods).
    Add { target: String, amount: i64 },
    /// `!removepoints user N` (mods).
    Remove { target: String, amount: i64 },
    /// A mod command with missing or bad arguments: how to use it.
    Usage(&'static str),
}

/// Recognises the points commands. `None` for anything else.
pub fn parse_command(cfg: &LoyaltyConfig, text: &str) -> Option<PointsCommand> {
    let inv = crate::command::parse(text)?;
    let word = inv.word.to_lowercase();
    if word == "!points" || cfg.name_command().is_some_and(|n| n == word) {
        return Some(PointsCommand::Balance);
    }
    let add = match word.as_str() {
        "!addpoints" => true,
        "!removepoints" => false,
        _ => return None,
    };
    let usage = if add { "Use !addpoints @name 100" } else { "Use !removepoints @name 100" };
    let (Some(target), Some(n)) = (inv.args.first(), inv.args.get(1)) else {
        return Some(PointsCommand::Usage(usage));
    };
    let target = target.trim_start_matches('@').to_lowercase();
    let amount = n.replace([',', '_'], "").parse::<i64>().ok().filter(|v| (1..=MAX_ADJUST).contains(v));
    match (target.is_empty(), amount) {
        (false, Some(amount)) if add => Some(PointsCommand::Add { target, amount }),
        (false, Some(amount)) => Some(PointsCommand::Remove { target, amount }),
        _ => Some(PointsCommand::Usage(usage)),
    }
}

/// "1,234 points", or "1 point" when the name is a simple plural.
pub fn label(points: i64, name: &str) -> String {
    let name = name.trim();
    let unit = if points == 1 && name.len() > 1 && name.ends_with('s') && !name.ends_with("ss") {
        &name[..name.len() - 1]
    } else {
        name
    };
    format!("{} {unit}", group(points))
}

/// 1234567 → "1,234,567".
pub fn group(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Tier, TipProvider};
    use crate::money::{Money, PlatformValue, ValueUnit};

    fn viewer(name: &str) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(format!("{name}-id"));
        a
    }

    fn live(kind: EventKind) -> Event {
        Event::live(Platform::Twitch, Some("x".into()), Millis(0), kind)
    }

    #[test]
    fn defaults_are_valid_and_validation_names_the_field() {
        assert!(LoyaltyConfig::default().validate().is_ok());
        let bad = |c: LoyaltyConfig| c.validate().unwrap_err();
        let d = LoyaltyConfig::default;
        assert!(bad(LoyaltyConfig { name: " ".into(), ..d() }).starts_with("name:"));
        assert!(bad(LoyaltyConfig { name: "x".repeat(21), ..d() }).starts_with("name:"));
        assert!(bad(LoyaltyConfig { name: "<b>".into(), ..d() }).starts_with("name:"));
        assert!(bad(LoyaltyConfig { interval_min: 0, ..d() }).starts_with("interval_min:"));
        assert!(bad(LoyaltyConfig { interval_min: 121, ..d() }).starts_with("interval_min:"));
        assert!(bad(LoyaltyConfig { tip_bonus: MAX_AWARD + 1, ..d() }).starts_with("tip_bonus:"));
        assert!(LoyaltyConfig { name: "Gold Coins".into(), per_interval: 0, ..d() }.validate().is_ok());
    }

    #[test]
    fn bonuses_pay_real_live_supporters_only() {
        let c = LoyaltyConfig::default();
        let f = live(EventKind::Follow { actor: viewer("fan") });
        assert_eq!(bonus(&c, &f).map(|(a, n)| (a.display_name.clone(), n)), Some(("fan".into(), 50)));
        let mut test = f.clone();
        test.origin = Origin::Test;
        assert!(bonus(&c, &test).is_none(), "test alerts pay nothing");
        let mut back = f.clone();
        back.origin = Origin::Backfill;
        assert!(bonus(&c, &back).is_none());
        let off = LoyaltyConfig { enabled: false, ..c.clone() };
        assert!(bonus(&off, &f).is_none());
        let batch = live(EventKind::GiftBatch { gifter: viewer("g"), tier: Tier::Tier1, count: 5 });
        assert_eq!(bonus(&c, &batch).unwrap().1, 1_250);
        let huge = live(EventKind::GiftBatch { gifter: viewer("g"), tier: Tier::Tier1, count: u32::MAX });
        assert_eq!(bonus(&c, &huge).unwrap().1, MAX_AWARD, "never overflows");
        let gifted = live(EventKind::Subscription {
            actor: viewer("r"),
            tier: Tier::Tier1,
            months: 1,
            message: None,
            gifted_by: Some(viewer("g")),
        });
        assert!(bonus(&c, &gifted).is_none(), "the gifter is paid by the batch");
        let pending = live(EventKind::Tip {
            actor: viewer("t"),
            money: Money::new(100, "INR"),
            message: None,
            provider: TipProvider::Razorpay,
            payment_ref: "p".into(),
            status: PaymentStatus::Pending,
        });
        assert!(bonus(&c, &pending).is_none(), "pending money isn't real yet");
        let bits = live(EventKind::PlatformGift {
            actor: viewer("b"),
            value: PlatformValue { unit: ValueUnit::Bits, amount: 100 },
            message: None,
        });
        assert_eq!(bonus(&c, &bits).unwrap().1, 100);
        let raid = live(EventKind::Raid { from: viewer("friend"), viewers: 40 });
        assert_eq!(bonus(&c, &raid).unwrap().1, 100);
        let anon = live(EventKind::Follow { actor: Actor::named(Platform::Twitch, "no-id") });
        assert!(bonus(&c, &anon).is_none(), "nobody to pay without an id");
        let mut me = viewer("me");
        me.roles = Roles::BROADCASTER;
        assert!(bonus(&c, &live(EventKind::Follow { actor: me })).is_none());
        assert!(bonus(&c, &live(EventKind::Follow { actor: viewer("Nightbot") })).is_none());
    }

    #[test]
    fn activity_pays_chatters_once_per_interval_while_live() {
        let c = LoyaltyConfig::default();
        let mut a = Activity::default();
        a.saw(&viewer("a"));
        a.saw(&viewer("a"));
        a.saw(&viewer("b"));
        let mut bot = viewer("bot");
        bot.roles = Roles::BOT;
        a.saw(&bot);
        assert_eq!(a.len(), 2);
        assert!(a.due(&c, true, Millis(0)).is_empty(), "the clock starts");
        assert!(a.due(&c, true, Millis(299_000)).is_empty());
        let paid = a.due(&c, true, Millis(300_000));
        assert_eq!(paid.len(), 2);
        assert!(a.is_empty());
        assert!(a.due(&c, true, Millis(600_000)).is_empty(), "nobody chatted since");
        a.saw(&viewer("c"));
        assert!(a.due(&c, false, Millis(900_000)).is_empty(), "offline");
        assert!(a.is_empty(), "offline chat is forgotten");
    }

    #[test]
    fn commands_parse_with_bad_input_answered_by_usage() {
        let c = LoyaltyConfig { name: "Coins".into(), ..Default::default() };
        assert_eq!(parse_command(&c, "!points"), Some(PointsCommand::Balance));
        assert_eq!(parse_command(&c, "!COINS"), Some(PointsCommand::Balance));
        assert_eq!(parse_command(&c, "hello !points"), None);
        assert_eq!(parse_command(&c, "!pointsx"), None);
        assert_eq!(
            parse_command(&c, "!addpoints @Sam 1,000"),
            Some(PointsCommand::Add { target: "sam".into(), amount: 1_000 })
        );
        assert_eq!(
            parse_command(&c, "!removepoints sam 5"),
            Some(PointsCommand::Remove { target: "sam".into(), amount: 5 })
        );
        for bad in [
            "!addpoints",
            "!addpoints sam",
            "!addpoints sam -5",
            "!addpoints sam 0",
            "!addpoints sam 1.5",
            "!addpoints sam lots",
            "!addpoints sam 99999999999999999999",
            "!addpoints @ 5",
        ] {
            assert!(matches!(parse_command(&c, bad), Some(PointsCommand::Usage(_))), "{bad}");
        }
        assert_eq!(LoyaltyConfig { name: "gold coins".into(), ..c }.name_command(), None);
    }

    #[test]
    fn labels_group_digits_and_singularise() {
        assert_eq!(label(1_234_567, "points"), "1,234,567 points");
        assert_eq!(label(1, "points"), "1 point");
        assert_eq!(label(1, "glass"), "1 glass");
        assert_eq!(label(0, "points"), "0 points");
        assert_eq!(group(-1_000), "-1,000");
        assert_eq!(group(100), "100");
    }
}
