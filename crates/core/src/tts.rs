//! What text-to-speech may read, and when it must wait for approval (R-48).
//!
//! TTS is off by default. When on, it reads only what passes these rules, never numbers
//! that look like phone numbers or addresses, never links, and it can hold everything for a
//! mod to approve. Offensive TTS has cost streamers their channels; this is the guard.

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind};
use crate::filter::{self, FilterConfig};
use crate::money::Money;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsPolicy {
    pub enabled: bool,
    pub voice: Option<String>,
    /// Minimum paid amounts that get read, per currency (minor units). A currency not
    /// listed is never read: no silent conversions.
    pub min_amounts: Vec<Money>,
    /// Minimum bits/KICKs/jewels.
    pub min_platform_value: u64,
    pub max_chars: usize,
    pub read_names: bool,
    /// Everything waits in the approval list until a mod presses approve.
    pub hold_for_approval: bool,
}

impl Default for TtsPolicy {
    fn default() -> Self {
        TtsPolicy {
            enabled: false,
            voice: None,
            min_amounts: vec![Money::new(10_000, "INR"), Money::new(200, "USD"), Money::new(200, "EUR")],
            min_platform_value: 200,
            max_chars: 200,
            read_names: true,
            hold_for_approval: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    Skip { reason: String },
    Hold { text: String },
    Speak { text: String },
}

/// Decides what to say for `event`'s message.
pub fn decide(policy: &TtsPolicy, filter_cfg: &FilterConfig, event: &Event) -> Decision {
    let skip = |r: &str| Decision::Skip { reason: r.into() };
    if !policy.enabled {
        return skip("TTS is off");
    }
    let (actor, message, paid_ok) = match &event.kind {
        EventKind::Tip { actor, money, message, .. } => (actor, message.clone(), meets(&policy.min_amounts, money)),
        EventKind::PaidMessage { actor, money, content, .. } => {
            (actor, content.as_ref().map(|c| c.text.clone()), meets(&policy.min_amounts, money))
        }
        EventKind::PlatformGift { actor, value, message } => {
            (actor, message.as_ref().map(|c| c.text.clone()), value.amount >= policy.min_platform_value)
        }
        EventKind::RewardRedemption { actor, input, .. } => (actor, input.clone(), true),
        _ => return skip("this event type isn't read aloud"),
    };
    if !paid_ok {
        return skip("below the minimum amount");
    }
    let Some(message) = message.filter(|m| !m.trim().is_empty()) else {
        return skip("no message");
    };
    let verdict = filter::check(filter_cfg, actor, &message, 0);
    if verdict.action != filter::Action::Allow {
        return Decision::Hold { text: clean(&message, policy.max_chars) };
    }
    let body = clean(&message, policy.max_chars);
    if body.is_empty() {
        return skip("nothing readable left");
    }
    let text = if policy.read_names && !actor.anonymous {
        format!("{} says: {}", clean(&actor.display_name, 40), body)
    } else {
        body
    };
    if policy.hold_for_approval { Decision::Hold { text } } else { Decision::Speak { text } }
}

fn meets(mins: &[Money], money: &Money) -> bool {
    mins.iter().any(|m| m.currency.eq_ignore_ascii_case(&money.currency) && money.minor >= m.minor)
}

/// Removes links, emails, long digit runs (phones, addresses, card numbers), repeated
/// characters and control characters, then caps the length at a word boundary.
pub fn clean(text: &str, max_chars: usize) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .filter(|w| !looks_like_link(w) && !w.contains('@'))
        .map(|w| {
            let digits = w.chars().filter(char::is_ascii_digit).count();
            if digits >= 5 { "number".to_string() } else { squeeze_runs(w, 3) }
        })
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .collect();
    let mut out = String::new();
    for w in words {
        let w: String = w.chars().filter(|c| !c.is_control()).collect();
        if out.chars().count() + w.chars().count() + 1 > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w);
    }
    out
}

fn looks_like_link(w: &str) -> bool {
    let l = w.to_lowercase();
    l.contains("://") || l.starts_with("www.") || !filter::links(&l).is_empty()
}

fn squeeze_runs(w: &str, max: usize) -> String {
    let mut out = String::new();
    let (mut last, mut run) = (None, 0);
    for c in w.chars() {
        run = if Some(c) == last { run + 1 } else { 1 };
        last = Some(c);
        if run <= max {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, Millis, PaymentStatus, Platform, TipProvider};

    fn tip(minor: i64, cur: &str, msg: &str) -> Event {
        Event::live(
            Platform::Tips,
            Some("t".into()),
            Millis(0),
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Asha"),
                money: Money::new(minor, cur),
                message: Some(msg.into()),
                provider: TipProvider::Razorpay,
                payment_ref: "p".into(),
                status: PaymentStatus::Captured,
            },
        )
    }

    fn on() -> TtsPolicy {
        TtsPolicy { enabled: true, ..TtsPolicy::default() }
    }

    #[test]
    fn off_by_default() {
        assert!(matches!(
            decide(&TtsPolicy::default(), &FilterConfig::default(), &tip(50_000, "INR", "hi")),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn reads_above_minimum_in_known_currencies_only() {
        let f = FilterConfig::default();
        assert_eq!(
            decide(&on(), &f, &tip(50_000, "INR", "great run")),
            Decision::Speak { text: "Asha says: great run".into() }
        );
        assert!(matches!(decide(&on(), &f, &tip(5_000, "INR", "hi")), Decision::Skip { .. }));
        assert!(matches!(decide(&on(), &f, &tip(1_000_000, "JPY", "hi")), Decision::Skip { .. }), "JPY not listed");
    }

    #[test]
    fn strips_doxxing_and_links() {
        let out = clean("call me 9876543210 or visit evil.com / https://x.y at 221B Baker st, mail a@b.co", 200);
        assert_eq!(out, "call me number or visit at 221B Baker st, mail");
        assert_eq!(clean("hiiiiiiiiii !!!!!!", 200), "hiii");
        assert_eq!(clean("one two three four", 9), "one two");
    }

    #[test]
    fn filtered_messages_are_held_not_spoken() {
        let mut f = FilterConfig::default();
        f.words.list.push("badword".into());
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "b4dw0rd lol")), Decision::Hold { .. }));
    }

    #[test]
    fn approval_mode_holds_everything() {
        let p = TtsPolicy { hold_for_approval: true, ..on() };
        assert!(matches!(decide(&p, &FilterConfig::default(), &tip(50_000, "INR", "hi")), Decision::Hold { .. }));
    }

    #[test]
    fn empty_or_unreadable_messages_are_skipped() {
        let f = FilterConfig::default();
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "   ")), Decision::Skip { .. }));
        assert!(matches!(decide(&on(), &f, &tip(50_000, "INR", "!!!! ???")), Decision::Skip { .. }));
    }
}
