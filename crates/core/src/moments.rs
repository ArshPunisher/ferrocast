//! Moments: the "Don't miss" lane (big tips, raids, first-time chatters pinned on Home)
//! and stream markers (big events and chat spikes with their time in the stream), exported
//! as CSV or text for editing.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, Origin, Platform, Roles};
use crate::money::ValueUnit;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MomentsConfig {
    /// Pin tips and Super Chats at or above this (major units of the main currency; any
    /// amount in another currency counts once it's at least this number).
    pub big_tip: i64,
    /// Bits or Kicks at or above this.
    pub big_gift_value: u64,
    /// Gifted subs at once at or above this.
    pub big_sub_gift: u32,
    /// Raids with at least this many viewers (1 = every raid).
    pub raid_viewers: u32,
    pub first_chatters: bool,
    /// A sound only the streamer hears (the dashboard plays it): `chime`, `coin`, `none`.
    pub sound: String,
    pub volume: u8,
    /// Markers for big moments and chat spikes.
    pub markers: bool,
    /// A spike is this many times the usual chat speed (2–10) …
    pub spike_factor: u32,
    /// … with at least this many messages in 30 seconds.
    pub spike_min: u32,
    /// Also put a stream marker on Twitch (needs `channel:manage:broadcast`).
    pub twitch_markers: bool,
}

impl Default for MomentsConfig {
    fn default() -> Self {
        MomentsConfig {
            big_tip: 500,
            big_gift_value: 500,
            big_sub_gift: 5,
            raid_viewers: 1,
            first_chatters: true,
            sound: "chime".into(),
            volume: 70,
            markers: true,
            spike_factor: 3,
            spike_min: 15,
            twitch_markers: false,
        }
    }
}

impl MomentsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(0..=100_000_000).contains(&self.big_tip) {
            return Err("big_tip: 0 to 100,000,000.".into());
        }
        if self.big_gift_value > 100_000_000 || self.big_sub_gift > 1000 || self.raid_viewers > 1_000_000 {
            return Err("big_gift_value: that number is too big.".into());
        }
        if !["chime", "coin", "bell", "none"].contains(&self.sound.as_str()) {
            return Err("sound: use chime, coin, bell or none.".into());
        }
        if self.volume > 100 {
            return Err("volume: 0 to 100.".into());
        }
        if !(2..=10).contains(&self.spike_factor) {
            return Err("spike_factor: 2 to 10 times the usual speed.".into());
        }
        if !(5..=500).contains(&self.spike_min) {
            return Err("spike_min: 5 to 500 messages.".into());
        }
        Ok(())
    }
}

/// What kind of moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MomentKind {
    Tip,
    Superchat,
    Bits,
    Gift,
    Raid,
    FirstChatter,
    ChatSpike,
    Clip,
    Manual,
}

impl MomentKind {
    pub fn key(self) -> &'static str {
        match self {
            MomentKind::Tip => "tip",
            MomentKind::Superchat => "superchat",
            MomentKind::Bits => "bits",
            MomentKind::Gift => "gift",
            MomentKind::Raid => "raid",
            MomentKind::FirstChatter => "first_chatter",
            MomentKind::ChatSpike => "chat_spike",
            MomentKind::Clip => "clip",
            MomentKind::Manual => "manual",
        }
    }

    pub fn from_key(s: &str) -> Option<MomentKind> {
        Some(match s {
            "tip" => MomentKind::Tip,
            "superchat" => MomentKind::Superchat,
            "bits" => MomentKind::Bits,
            "gift" => MomentKind::Gift,
            "raid" => MomentKind::Raid,
            "first_chatter" => MomentKind::FirstChatter,
            "chat_spike" => MomentKind::ChatSpike,
            "clip" => MomentKind::Clip,
            "manual" => MomentKind::Manual,
            _ => return None,
        })
    }
}

/// A big moment worth pinning and marking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moment {
    pub kind: MomentKind,
    /// "Asha tipped ₹2,000".
    pub title: String,
    /// The message, if any (plain text, ≤200 characters).
    pub detail: Option<String>,
    pub platform: Platform,
    /// Only for big events; first chatters are pinned but not marked.
    pub mark: bool,
}

fn short(s: &str) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = one.trim();
    if t.chars().count() > 200 { format!("{}…", t.chars().take(199).collect::<String>()) } else { t.to_string() }
}

/// Whether `e` is a moment, by the streamer's thresholds. `first_message` is the chat
/// filter's "first message ever" mark. Only live events count (never tests or replays).
pub fn classify(cfg: &MomentsConfig, main_currency: &str, e: &Event, first_message: bool) -> Option<Moment> {
    if e.origin != Origin::Live {
        return None;
    }
    // Another currency is never converted (R-32): a rough "one hundredth of the threshold"
    // keeps a $10 tip big for a ₹500 threshold without pretending to know the rate.
    let money_big = |m: &crate::money::Money| {
        let major = m.major();
        if m.currency == main_currency {
            major >= cfg.big_tip as f64
        } else {
            major >= 1.0 && major >= cfg.big_tip as f64 / 100.0
        }
    };
    let m =
        |kind, title: String, detail: Option<String>, mark| Moment { kind, title, detail, platform: e.platform, mark };
    match &e.kind {
        EventKind::Tip { actor, money, message, .. } if e.kind.is_alertable() && money_big(money) => Some(m(
            MomentKind::Tip,
            format!("{} tipped {}", actor.display_name, money.display()),
            message.as_deref().map(short).filter(|s| !s.is_empty()),
            true,
        )),
        EventKind::PaidMessage { actor, money, content, .. } if money_big(money) => Some(m(
            MomentKind::Superchat,
            format!("{} sent a Super Chat of {}", actor.display_name, money.display()),
            content.as_ref().map(|c| short(&c.text)).filter(|s| !s.is_empty()),
            true,
        )),
        EventKind::PlatformGift { actor, value, message } if value.amount >= cfg.big_gift_value => {
            let what = if value.unit == ValueUnit::Bits { "cheered" } else { "sent" };
            Some(m(
                MomentKind::Bits,
                format!("{} {what} {}", actor.display_name, value.display()),
                message.as_ref().map(|c| short(&c.text)).filter(|s| !s.is_empty()),
                true,
            ))
        }
        EventKind::GiftBatch { gifter, count, .. } if *count >= cfg.big_sub_gift => Some(m(
            MomentKind::Gift,
            format!("{} gifted {}", gifter.display_name, crate::template::plural(u64::from(*count), "sub", "subs")),
            None,
            true,
        )),
        EventKind::Raid { from, viewers } if *viewers >= cfg.raid_viewers.max(1) => Some(m(
            MomentKind::Raid,
            format!(
                "{} raided with {}",
                from.display_name,
                crate::template::plural(u64::from(*viewers), "viewer", "viewers")
            ),
            None,
            true,
        )),
        EventKind::ChatMessage { actor, content, .. }
            if cfg.first_chatters
                && first_message
                && e.channel.is_none()
                && !actor.roles.contains(Roles::BROADCASTER)
                && !actor.roles.contains(Roles::BOT) =>
        {
            Some(m(
                MomentKind::FirstChatter,
                format!("{} is chatting for the first time", actor.display_name),
                Some(short(&content.text)).filter(|s| !s.is_empty()),
                false,
            ))
        }
        _ => None,
    }
}

/// Chat speed: 5-second buckets over the last 5 minutes. A spike is the last 30 seconds
/// running at `factor` times the usual pace (and at least `min` messages); one spike is
/// marked per 2 minutes so a long hype moment is one marker.
#[derive(Debug, Default)]
pub struct SpikeMeter {
    buckets: VecDeque<(i64, u32)>,
    last_spike: Option<i64>,
}

const BUCKET_MS: i64 = 5_000;
const WINDOW_BUCKETS: usize = 60;
const RECENT_BUCKETS: usize = 6;

impl SpikeMeter {
    /// Counts one message at `at`; returns the message count of the spike when this one
    /// starts a spike.
    pub fn message(&mut self, at: i64, factor: u32, min: u32) -> Option<u32> {
        let slot = at.div_euclid(BUCKET_MS);
        match self.buckets.back_mut() {
            Some((s, n)) if *s == slot => *n += 1,
            Some((s, _)) if *s > slot => return None, // out of order: ignore
            _ => self.buckets.push_back((slot, 1)),
        }
        while self.buckets.front().is_some_and(|(s, _)| slot - s >= WINDOW_BUCKETS as i64) {
            self.buckets.pop_front();
        }
        let recent: u32 = self.buckets.iter().filter(|(s, _)| slot - s < RECENT_BUCKETS as i64).map(|(_, n)| *n).sum();
        let older: Vec<u32> =
            self.buckets.iter().filter(|(s, _)| slot - s >= RECENT_BUCKETS as i64).map(|(_, n)| *n).collect();
        // The usual pace per 30 s, counting quiet buckets as zero, over what we've seen.
        let span = self.buckets.front().map_or(0, |(s, _)| slot - s + 1) as usize;
        let older_buckets = span.saturating_sub(RECENT_BUCKETS).max(1);
        let usual = older.iter().sum::<u32>() as f64 / older_buckets as f64 * RECENT_BUCKETS as f64;
        let enough_history = span >= RECENT_BUCKETS * 2;
        let spiking = enough_history && recent >= min && f64::from(recent) >= usual.max(1.0) * f64::from(factor);
        if spiking && self.last_spike.is_none_or(|t| at - t >= 120_000) {
            self.last_spike = Some(at);
            return Some(recent);
        }
        None
    }
}

/// A saved marker (API `Marker`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub id: i64,
    pub at: i64,
    /// Time into the stream, when it was live.
    pub offset_ms: Option<i64>,
    pub kind: MomentKind,
    pub label: String,
    pub platform: Option<Platform>,
    /// A replay-buffer clip was saved for it.
    pub clip: bool,
}

/// `01:02:03` from milliseconds.
pub fn timecode(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// One CSV cell: quoted, and a leading `= + - @` defused so a spreadsheet never runs it.
fn cell(s: &str) -> String {
    let mut s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        s.insert(0, '\'');
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// CSV for editors: `timecode,offset_seconds,kind,label,platform,clip,unix_ms`.
pub fn to_csv(markers: &[Marker]) -> String {
    let mut out = String::from("timecode,offset_seconds,kind,label,platform,clip,unix_ms\n");
    for m in markers {
        let (tc, secs) = match m.offset_ms {
            Some(o) => (timecode(o), (o / 1000).to_string()),
            None => (String::new(), String::new()),
        };
        out.push_str(&format!(
            "{tc},{secs},{},{},{},{},{}\n",
            m.kind.key(),
            cell(&m.label),
            m.platform.map(Platform::label).unwrap_or(""),
            if m.clip { "yes" } else { "no" },
            m.at
        ));
    }
    out
}

/// Plain text, one line per marker: `00:42:10  Raid: Bo raided with 30 viewers` (YouTube
/// chapters style when pasted into a description).
pub fn to_txt(markers: &[Marker]) -> String {
    let mut out = String::new();
    for m in markers {
        let tc = m.offset_ms.map(timecode).unwrap_or_else(|| "--:--:--".into());
        let label: String = m.label.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        out.push_str(&format!("{tc}  {}{}\n", label.trim(), if m.clip { " [clip saved]" } else { "" }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Actor, Content, Millis, PaymentStatus, TipProvider};
    use crate::money::{Money, PlatformValue};

    fn live(kind: EventKind) -> Event {
        Event::live(Platform::Twitch, Some("x".into()), Millis(0), kind)
    }

    fn tip(minor: i64, currency: &str) -> Event {
        live(EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: Money::new(minor, currency),
            message: Some("for the\nstream".into()),
            provider: TipProvider::Razorpay,
            payment_ref: "p".into(),
            status: PaymentStatus::Captured,
        })
    }

    #[test]
    fn big_things_are_moments() {
        let c = MomentsConfig::default();
        let big = classify(&c, "INR", &tip(200_000, "INR"), false).unwrap();
        assert_eq!(big.kind, MomentKind::Tip);
        assert_eq!(big.title, "Asha tipped ₹2,000");
        assert_eq!(big.detail.as_deref(), Some("for the stream"));
        assert!(big.mark);
        assert!(classify(&c, "INR", &tip(10_000, "INR"), false).is_none(), "₹100 is below ₹500");
        assert!(classify(&c, "INR", &tip(1_000, "USD"), false).is_some(), "$10 in another currency counts");
        let mut pending = tip(900_000, "INR");
        if let EventKind::Tip { status, .. } = &mut pending.kind {
            *status = PaymentStatus::Pending;
        }
        assert!(classify(&c, "INR", &pending, false).is_none(), "not settled (R-49)");
        let mut test = tip(900_000, "INR");
        test.origin = Origin::Test;
        assert!(classify(&c, "INR", &test, false).is_none());
        let raid = live(EventKind::Raid { from: Actor::named(Platform::Twitch, "Bo"), viewers: 1 });
        assert_eq!(classify(&c, "INR", &raid, false).unwrap().title, "Bo raided with 1 viewer");
        let bits = live(EventKind::PlatformGift {
            actor: Actor::named(Platform::Twitch, "Cy"),
            value: PlatformValue { unit: ValueUnit::Bits, amount: 1000 },
            message: None,
        });
        assert_eq!(classify(&c, "INR", &bits, false).unwrap().kind, MomentKind::Bits);
        let gifts = live(EventKind::GiftBatch {
            gifter: Actor::named(Platform::Twitch, "G"),
            tier: crate::event::Tier::Tier1,
            count: 4,
        });
        assert!(classify(&c, "INR", &gifts, false).is_none());
        let mut chat = live(EventKind::ChatMessage {
            actor: Actor::named(Platform::Twitch, "New"),
            content: Content::plain("hi!"),
            reply_to: None,
            is_action: false,
        });
        assert!(classify(&c, "INR", &chat, false).is_none());
        let first = classify(&c, "INR", &chat, true).unwrap();
        assert_eq!((first.kind, first.mark), (MomentKind::FirstChatter, false));
        chat.channel = Some(crate::event::ChannelRef { platform: Platform::Twitch, id: "p".into(), name: "p".into() });
        assert!(classify(&c, "INR", &chat, true).is_none(), "Shared Chat partners aren't ours");
        let off = MomentsConfig { first_chatters: false, ..c };
        chat.channel = None;
        assert!(classify(&off, "INR", &chat, true).is_none());
    }

    #[test]
    fn config_validation() {
        assert!(MomentsConfig::default().validate().is_ok());
        assert!(MomentsConfig { sound: "airhorn".into(), ..Default::default() }.validate().is_err());
        assert!(MomentsConfig { volume: 101, ..Default::default() }.validate().is_err());
        assert!(MomentsConfig { spike_factor: 1, ..Default::default() }.validate().is_err());
        assert!(MomentsConfig { spike_min: 1, ..Default::default() }.validate().is_err());
        assert!(MomentsConfig { big_tip: -1, ..Default::default() }.validate().is_err());
    }

    #[test]
    fn chat_spikes() {
        let mut m = SpikeMeter::default();
        let mut spikes = Vec::new();
        // Two minutes of slow chat: one message every 5 s.
        for i in 0..24 {
            assert!(m.message(i * 5_000, 3, 15).is_none());
        }
        // Then 30 messages in 10 seconds.
        for i in 0..30 {
            if let Some(n) = m.message(120_000 + i * 330, 3, 15) {
                spikes.push(n);
            }
        }
        assert_eq!(spikes.len(), 1, "one marker per spike");
        assert!(spikes[0] >= 15);
        // Still busy a minute later: no second marker.
        for i in 0..30 {
            assert!(m.message(180_000 + i * 300, 3, 15).is_none());
        }
        // A brand-new meter needs history before it calls anything a spike.
        let mut fresh = SpikeMeter::default();
        assert!((0..50).all(|i| fresh.message(i * 10, 3, 15).is_none()));
        // Out-of-order times don't panic.
        assert!(fresh.message(-50_000, 3, 15).is_none());
    }

    #[test]
    fn exports() {
        let ms = vec![
            Marker {
                id: 1,
                at: 1_000,
                offset_ms: Some(3_723_000),
                kind: MomentKind::Raid,
                label: "Bo raided, \"30\" viewers".into(),
                platform: Some(Platform::Twitch),
                clip: true,
            },
            Marker {
                id: 2,
                at: 2_000,
                offset_ms: None,
                kind: MomentKind::Manual,
                label: "=HYPERLINK(\"x\")\nhi".into(),
                platform: None,
                clip: false,
            },
        ];
        let csv = to_csv(&ms);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "timecode,offset_seconds,kind,label,platform,clip,unix_ms");
        assert_eq!(lines[1], "01:02:03,3723,raid,\"Bo raided, \"\"30\"\" viewers\",Twitch,yes,1000");
        assert_eq!(lines[2], ",,manual,\"'=HYPERLINK(\"\"x\"\") hi\",,no,2000", "formulas defused, one line");
        let txt = to_txt(&ms);
        assert_eq!(txt, "01:02:03  Bo raided, \"30\" viewers [clip saved]\n--:--:--  =HYPERLINK(\"x\") hi\n");
        assert_eq!(timecode(-5), "00:00:00");
        assert_eq!(MomentKind::from_key("clip"), Some(MomentKind::Clip));
        assert_eq!(MomentKind::from_key("x"), None);
        for k in ["tip", "superchat", "bits", "gift", "raid", "first_chatter", "chat_spike", "clip", "manual"] {
            assert_eq!(MomentKind::from_key(k).map(MomentKind::key), Some(k));
        }
    }
}
