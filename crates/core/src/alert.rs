//! Alert rules: which events show an alert, and which variation of it.
//!
//! A streamer sets one rule per kind ("Subscriptions") with a default look and optional
//! variations ("10+ months gets the gold one"). Selection is a pure function of the rules
//! and the event so the editor can preview it and tests can pin it down.

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, Platform};
use crate::template::{Template, TemplateError, Vars, event_vars, with_plurals};

/// One condition. Variations combine them with all/any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// Amount in major units (₹500 → 500, 1500 bits → 1500, 5 gifted subs → 5).
    /// With `currency`, only money in that currency matches.
    AmountAtLeast {
        value: f64,
        currency: Option<String>,
    },
    AmountBelow {
        value: f64,
        currency: Option<String>,
    },
    TierAtLeast {
        tier: u8,
    },
    MonthsAtLeast {
        months: u32,
    },
    PlatformIs {
        platform: Platform,
    },
    IsGifted,
    HasMessage,
    MessageContains {
        text: String,
    },
    All {
        of: Vec<Condition>,
    },
    Any {
        of: Vec<Condition>,
    },
}

impl Condition {
    pub fn matches(&self, event: &Event) -> bool {
        match self {
            Condition::AmountAtLeast { value, currency } => {
                amount_of(event, currency.as_deref()).is_some_and(|a| a >= *value)
            }
            Condition::AmountBelow { value, currency } => {
                amount_of(event, currency.as_deref()).is_some_and(|a| a < *value)
            }
            Condition::TierAtLeast { tier } => match &event.kind {
                EventKind::Subscription { tier: t, .. } | EventKind::GiftBatch { tier: t, .. } => t.rank() >= *tier,
                _ => false,
            },
            Condition::MonthsAtLeast { months } => {
                matches!(&event.kind, EventKind::Subscription { months: m, .. } if m >= months)
            }
            Condition::PlatformIs { platform } => event.platform == *platform,
            Condition::IsGifted => {
                matches!(&event.kind, EventKind::Subscription { gifted_by: Some(_), .. } | EventKind::GiftBatch { .. })
            }
            Condition::HasMessage => message_of(event).is_some_and(|m| !m.trim().is_empty()),
            Condition::MessageContains { text } => {
                let needle = text.to_lowercase();
                !needle.is_empty() && message_of(event).is_some_and(|m| m.to_lowercase().contains(&needle))
            }
            Condition::All { of } => of.iter().all(|c| c.matches(event)),
            Condition::Any { of } => of.iter().any(|c| c.matches(event)),
        }
    }
}

/// The number conditions compare against, in major units.
pub fn amount_of(event: &Event, currency: Option<&str>) -> Option<f64> {
    let money_ok = |c: &str| currency.is_none_or(|want| want.eq_ignore_ascii_case(c));
    match &event.kind {
        EventKind::Tip { money, .. } | EventKind::PaidMessage { money, .. } if money_ok(&money.currency) => {
            Some(money.major())
        }
        EventKind::PlatformGift { value, .. } if currency.is_none() => Some(value.amount as f64),
        EventKind::GiftBatch { count, .. } if currency.is_none() => Some(f64::from(*count)),
        EventKind::Raid { viewers, .. } if currency.is_none() => Some(f64::from(*viewers)),
        EventKind::Subscription { months, .. } if currency.is_none() => Some(f64::from(*months)),
        EventKind::RewardRedemption { cost, .. } if currency.is_none() => Some(*cost as f64),
        _ => None,
    }
}

fn message_of(event: &Event) -> Option<&str> {
    match &event.kind {
        EventKind::Subscription { message: Some(m), .. }
        | EventKind::PlatformGift { message: Some(m), .. }
        | EventKind::PaidMessage { content: Some(m), .. } => Some(&m.text),
        EventKind::Tip { message: Some(m), .. } => Some(m),
        EventKind::RewardRedemption { input: Some(i), .. } => Some(i),
        _ => None,
    }
}

/// How an alert looks and sounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variation {
    pub id: String,
    pub name: String,
    /// `None` = the rule's default look.
    pub condition: Option<Condition>,
    /// Higher wins when several variations match.
    pub priority: i32,
    /// Headline, e.g. "{name} subscribed for {months} months!"
    pub title: String,
    /// Second line; usually "{message}".
    pub body: String,
    pub image: Option<String>,
    pub sound: Option<String>,
    /// 0–100.
    pub volume: u8,
    pub duration_ms: u32,
    /// Read the message aloud (still subject to the TTS policy and approval queue).
    pub tts: bool,
    /// Layout preset name the overlay understands ("stack", "side", "banner"…).
    pub layout: String,
    /// Extra CSS the streamer wrote. The overlay scopes it to the alert (R-39).
    #[serde(default)]
    pub css: String,
}

impl Variation {
    pub fn new(id: &str, title: &str) -> Self {
        Variation {
            id: id.into(),
            name: "Default".into(),
            condition: None,
            priority: 0,
            title: title.into(),
            body: "{message}".into(),
            image: None,
            sound: None,
            volume: 70,
            duration_ms: 7_000,
            tts: false,
            layout: "stack".into(),
            css: String::new(),
        }
    }
}

/// An alert rule for one or more event kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Event kind names (`EventKind::name`).
    pub kinds: Vec<String>,
    /// Empty = every platform.
    #[serde(default)]
    pub platforms: Vec<Platform>,
    pub variations: Vec<Variation>,
}

/// Shortest and longest an alert may stay on screen.
pub const MIN_DURATION_MS: u32 = 1_000;
pub const MAX_DURATION_MS: u32 = 60_000;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RuleError {
    #[error("the rule needs at least one event type")]
    NoKinds,
    #[error("'{0}' isn't an event type that can show an alert")]
    UnknownKind(String),
    #[error("the rule needs a default look (a variation without a condition)")]
    NoDefault,
    #[error("variation '{name}': {source}")]
    Template { name: String, source: TemplateError },
    #[error("variation '{0}': volume must be 0 to 100")]
    Volume(String),
    #[error("variation '{0}': duration must be 1 to 60 seconds")]
    Duration(String),
}

/// Event kinds a rule may target.
pub const ALERT_KINDS: [&str; 8] =
    ["follow", "subscription", "gift_batch", "platform_gift", "paid_message", "tip", "raid", "reward_redemption"];

impl AlertRule {
    /// Checks everything the editor should refuse to save.
    pub fn validate(&self) -> Result<(), RuleError> {
        if self.kinds.is_empty() {
            return Err(RuleError::NoKinds);
        }
        if let Some(bad) = self.kinds.iter().find(|k| !ALERT_KINDS.contains(&k.as_str())) {
            return Err(RuleError::UnknownKind(bad.clone()));
        }
        if !self.variations.iter().any(|v| v.condition.is_none()) {
            return Err(RuleError::NoDefault);
        }
        for v in &self.variations {
            for src in [&v.title, &v.body] {
                Template::parse(src).map_err(|source| RuleError::Template { name: v.name.clone(), source })?;
            }
            if v.volume > 100 {
                return Err(RuleError::Volume(v.name.clone()));
            }
            if !(MIN_DURATION_MS..=MAX_DURATION_MS).contains(&v.duration_ms) {
                return Err(RuleError::Duration(v.name.clone()));
            }
        }
        Ok(())
    }

    fn applies_to(&self, event: &Event) -> bool {
        self.enabled
            && self.kinds.iter().any(|k| k == event.kind.name())
            && (self.platforms.is_empty() || self.platforms.contains(&event.platform))
    }

    /// The variation for `event`: the highest-priority matching one, else the default.
    pub fn pick(&self, event: &Event) -> Option<&Variation> {
        let conditional = self
            .variations
            .iter()
            .filter(|v| v.condition.as_ref().is_some_and(|c| c.matches(event)))
            .max_by_key(|v| v.priority);
        conditional.or_else(|| self.variations.iter().find(|v| v.condition.is_none()))
    }
}

/// Picks the first enabled rule that applies, and its variation.
pub fn select<'a>(rules: &'a [AlertRule], event: &Event) -> Option<(&'a AlertRule, &'a Variation)> {
    if !event.may_alert() {
        return None;
    }
    rules.iter().filter(|r| r.applies_to(event)).find_map(|r| r.pick(event).map(|v| (r, v)))
}

/// What the overlay receives: text already rendered, media by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedAlert {
    pub alert_id: String,
    pub event_id: String,
    pub rule_id: String,
    pub variation_id: String,
    pub kind: String,
    pub platform: Platform,
    pub title: String,
    pub body: String,
    pub image: Option<String>,
    pub sound: Option<String>,
    pub volume: u8,
    pub duration_ms: u32,
    pub layout: String,
    pub css: String,
    /// Text for TTS if the variation wants it; the TTS policy decides if it's read.
    pub tts_text: Option<String>,
    /// Media id of the synthesised speech, once the engine made it.
    #[serde(default)]
    pub tts_audio: Option<String>,
    pub test: bool,
    /// The look from the alert design in use, when there is one (API.md §4 Designs). Absent
    /// otherwise, so overlays without design support see the same alert as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<crate::design::AlertDesignView>,
}

/// Renders the chosen variation for `event`. Templates were validated when saved; a
/// template that fails to parse here falls back to its raw text rather than dropping a
/// paid alert.
pub fn render(alert_id: &str, rule: &AlertRule, v: &Variation, event: &Event) -> RenderedAlert {
    let vars: Vars = event_vars(event);
    let fill =
        |src: &str| Template::parse(&with_plurals(src)).map(|t| t.render(&vars)).unwrap_or_else(|_| src.to_string());
    let body = fill(&v.body);
    RenderedAlert {
        alert_id: alert_id.into(),
        event_id: event.id.clone(),
        rule_id: rule.id.clone(),
        variation_id: v.id.clone(),
        kind: event.kind.name().into(),
        platform: event.platform,
        title: fill(&v.title),
        tts_text: (v.tts && !body.trim().is_empty()).then(|| body.clone()),
        tts_audio: None,
        body,
        image: v.image.clone(),
        sound: v.sound.clone(),
        volume: v.volume.min(100),
        duration_ms: v.duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS),
        layout: v.layout.clone(),
        css: v.css.clone(),
        test: event.origin == crate::event::Origin::Test,
        design: None,
    }
}

/// A variation's `sound` that names a built-in sound instead of a library file
/// (`builtin:glint`); the overlay plays its own file. Passed through as is.
pub const BUILTIN_SOUND_PREFIX: &str = "builtin:";

/// The built-in sound each alert kind starts with on a new install (the owner's pre-matched
/// set, 20 Oct 2026). Saved rules never change.
pub fn default_sound(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "follow" => "glint",
        "subscription" => "cascade",
        "gift_batch" => "bloom",
        "platform_gift" | "paid_message" | "tip" => "tally",
        "raid" => "lift",
        "reward_redemption" => "pebble",
        _ => return None,
    })
}

/// The rules a new install starts with, so alerts work the moment an account is connected.
/// Each starts with its kind's built-in sound.
pub fn default_rules() -> Vec<AlertRule> {
    let rule = |id: &str, name: &str, kind: &str, title: &str| {
        let mut v = Variation::new(&format!("{id}-default"), title);
        v.sound = default_sound(kind).map(|s| format!("{BUILTIN_SOUND_PREFIX}{s}"));
        AlertRule {
            id: id.into(),
            name: name.into(),
            enabled: true,
            kinds: vec![kind.into()],
            platforms: vec![],
            variations: vec![v],
        }
    };
    vec![
        rule("follow", "Follows", "follow", "{name} followed!"),
        rule("sub", "Subscriptions", "subscription", "{name} subscribed for {months_text}!"),
        rule("gift", "Gifted subs", "gift_batch", "{name} gifted {count_text}!"),
        rule("bits", "Bits and KICKs", "platform_gift", "{name} sent {value}!"),
        rule("superchat", "Super Chats", "paid_message", "{name} sent {amount}!"),
        rule("tip", "Tips", "tip", "{name} tipped {amount}!"),
        rule("raid", "Raids", "raid", "{name} is raiding with {viewers_text}!"),
        rule("redeem", "Channel points", "reward_redemption", "{name} redeemed {reward}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Millis, Origin};
    use crate::sample::{any_sample, sample};

    fn sub_rule() -> AlertRule {
        let mut gold = Variation::new("gold", "{name} is a legend: {months} months!");
        gold.name = "Gold".into();
        gold.priority = 10;
        gold.condition = Some(Condition::MonthsAtLeast { months: 12 });
        let mut silver = Variation::new("silver", "{name}: {months} months");
        silver.name = "Silver".into();
        silver.priority = 5;
        silver.condition = Some(Condition::MonthsAtLeast { months: 3 });
        AlertRule {
            id: "sub".into(),
            name: "Subs".into(),
            enabled: true,
            kinds: vec!["subscription".into()],
            platforms: vec![],
            variations: vec![Variation::new("default", "{name} subscribed"), silver, gold],
        }
    }

    fn sub_with_months(m: u32) -> Event {
        let mut e = sample("subscription", Platform::Twitch, Millis(0)).unwrap();
        e.origin = Origin::Live;
        if let EventKind::Subscription { months, .. } = &mut e.kind {
            *months = m;
        }
        e
    }

    #[test]
    fn highest_priority_matching_variation_wins() {
        let rules = vec![sub_rule()];
        assert_eq!(select(&rules, &sub_with_months(1)).unwrap().1.id, "default");
        assert_eq!(select(&rules, &sub_with_months(6)).unwrap().1.id, "silver");
        assert_eq!(select(&rules, &sub_with_months(24)).unwrap().1.id, "gold");
    }

    #[test]
    fn disabled_rules_wrong_platform_and_backfill_are_skipped() {
        let mut rules = vec![sub_rule()];
        rules[0].platforms = vec![Platform::Kick];
        assert!(select(&rules, &sub_with_months(1)).is_none());
        rules[0].platforms.clear();
        rules[0].enabled = false;
        assert!(select(&rules, &sub_with_months(1)).is_none());
        rules[0].enabled = true;
        let mut e = sub_with_months(1);
        e.origin = Origin::Backfill;
        assert!(select(&rules, &e).is_none());
    }

    #[test]
    fn currency_conditions_never_cross_currencies() {
        let tip = any_sample("tip", Millis(0)).unwrap(); // ₹500
        assert!(Condition::AmountAtLeast { value: 500.0, currency: Some("INR".into()) }.matches(&tip));
        assert!(!Condition::AmountAtLeast { value: 5.0, currency: Some("USD".into()) }.matches(&tip));
        assert!(Condition::AmountBelow { value: 501.0, currency: None }.matches(&tip));
        let bits = sample("platform_gift", Platform::Twitch, Millis(0)).unwrap();
        assert!(!Condition::AmountAtLeast { value: 1.0, currency: Some("USD".into()) }.matches(&bits));
        assert!(Condition::AmountAtLeast { value: 500.0, currency: None }.matches(&bits));
    }

    #[test]
    fn empty_needle_never_matches() {
        let tip = any_sample("tip", Millis(0)).unwrap();
        assert!(!Condition::MessageContains { text: String::new() }.matches(&tip));
        assert!(Condition::MessageContains { text: "KEEP".into() }.matches(&tip));
    }

    #[test]
    fn validation_catches_editor_mistakes() {
        let mut r = sub_rule();
        assert!(r.validate().is_ok());
        r.variations.retain(|v| v.condition.is_some());
        assert_eq!(r.validate(), Err(RuleError::NoDefault));
        let mut r = sub_rule();
        r.kinds = vec!["chat_message".into()];
        assert!(matches!(r.validate(), Err(RuleError::UnknownKind(_))));
        let mut r = sub_rule();
        r.variations[0].title = "{name".into();
        assert!(matches!(r.validate(), Err(RuleError::Template { .. })));
        let mut r = sub_rule();
        r.variations[0].duration_ms = 0;
        assert!(matches!(r.validate(), Err(RuleError::Duration(_))));
        let mut r = sub_rule();
        r.variations[0].volume = 101;
        assert!(matches!(r.validate(), Err(RuleError::Volume(_))));
        let mut r = sub_rule();
        r.kinds.clear();
        assert_eq!(r.validate(), Err(RuleError::NoKinds));
    }

    #[test]
    fn default_rules_start_with_their_kinds_sound() {
        let rules = default_rules();
        let sound = |id: &str| {
            rules.iter().find(|r| r.id == id).and_then(|r| r.variations[0].sound.clone()).unwrap_or_default()
        };
        assert_eq!(sound("follow"), "builtin:glint");
        assert_eq!(sound("sub"), "builtin:cascade");
        assert_eq!(sound("gift"), "builtin:bloom");
        assert_eq!(sound("tip"), "builtin:tally");
        assert_eq!(sound("raid"), "builtin:lift");
        assert_eq!(sound("redeem"), "builtin:pebble");
        assert!(rules.iter().all(|r| r.variations.iter().all(|v| v.sound.is_some())));
        assert_eq!(default_sound("chat_message"), None);
        // A rendered alert carries the sound for the overlay to play.
        let r = rules.iter().find(|r| r.id == "follow").unwrap();
        let a = render("a1", r, &r.variations[0], &sample("follow", Platform::Twitch, Millis(0)).unwrap());
        assert_eq!(a.sound.as_deref(), Some("builtin:glint"));
    }

    #[test]
    fn default_rules_are_valid_and_cover_every_sample() {
        let rules = default_rules();
        for r in &rules {
            r.validate().unwrap();
        }
        for kind in crate::sample::TESTABLE {
            let e = any_sample(kind, Millis(0)).unwrap();
            let (rule, v) = select(&rules, &e).unwrap_or_else(|| panic!("no default rule for {kind}"));
            let a = render("a1", rule, v, &e);
            assert!(!a.title.contains('{'), "unrendered title for {kind}: {}", a.title);
            assert!(a.test);
        }
    }

    #[test]
    fn render_clamps_and_reads_tts_only_with_text() {
        let e = any_sample("tip", Millis(0)).unwrap();
        let mut v = Variation::new("v", "{name}");
        v.duration_ms = 999_999;
        v.volume = 250;
        v.tts = true;
        let rule = AlertRule {
            id: "r".into(),
            name: "r".into(),
            enabled: true,
            kinds: vec!["tip".into()],
            platforms: vec![],
            variations: vec![v.clone()],
        };
        let a = render("a", &rule, &v, &e);
        assert_eq!(a.duration_ms, MAX_DURATION_MS);
        assert_eq!(a.volume, 100);
        assert_eq!(a.tts_text.as_deref(), Some("Keep it up!"));
        let follow = any_sample("follow", Millis(0)).unwrap();
        assert_eq!(render("a", &rule, &v, &follow).tts_text, None);
    }

    #[test]
    fn a_first_sub_reads_one_month_with_old_and_new_templates() {
        let defaults = default_rules();
        let sub_rule = defaults.iter().find(|r| r.id == "sub").unwrap();
        let one = sub_with_months(1);
        let a = render("a", sub_rule, &sub_rule.variations[0], &one);
        assert!(a.title.ends_with("subscribed for 1 month!"), "{}", a.title);
        let many = render("a", sub_rule, &sub_rule.variations[0], &sub_with_months(12));
        assert!(many.title.ends_with("subscribed for 12 months!"), "{}", many.title);
        // A rule saved with the old default text still reads right.
        let old = Variation::new("old", "{name} subscribed for {months} months!");
        assert!(render("a", sub_rule, &old, &one).title.ends_with("subscribed for 1 month!"));
        // And other wording is left alone.
        let own = Variation::new("own", "{months}");
        assert_eq!(render("a", sub_rule, &own, &one).title, "1");
    }
}
