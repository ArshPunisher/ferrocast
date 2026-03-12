//! The one event envelope every platform is translated into (ARCHITECTURE.md §3).
//!
//! Adapters own the messy parts (payload shapes, transport quirks); everything after the
//! adapter only ever sees [`Event`]. New kinds are additive so older overlays keep working.

use serde::{Deserialize, Serialize};

use crate::macros::bitflags_like;
use crate::money::{Money, PlatformValue};

/// Version of the event schema sent to overlays and stored in the log.
pub const SCHEMA_VERSION: u32 = 1;

/// Milliseconds since the Unix epoch. A plain integer keeps `core` free of clocks and
/// time-zone crates; adapters convert platform timestamps at the edge.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct Millis(pub i64);

impl Millis {
    /// Whole seconds between two instants, never negative.
    pub fn seconds_since(self, earlier: Millis) -> i64 {
        (self.0 - earlier.0).max(0) / 1000
    }
}

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Twitch,
    Youtube,
    Kick,
    /// A tip page payment (the provider is on the `Tip` kind).
    Tips,
    /// Generated inside Ferrocast (bot replies, timers, panic mode notices).
    Ferrocast,
}

impl Platform {
    /// The platforms viewers chat on. Tips and internal events are not chat platforms.
    pub const CHAT: [Platform; 3] = [Platform::Twitch, Platform::Youtube, Platform::Kick];

    pub fn label(self) -> &'static str {
        match self {
            Platform::Twitch => "Twitch",
            Platform::Youtube => "YouTube",
            Platform::Kick => "Kick",
            Platform::Tips => "Tips",
            Platform::Ferrocast => "Ferrocast",
        }
    }
}

/// Why an event exists. Only `Live` fires alerts unless a rule opts in (R-31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Arrived from the platform while we were connected.
    Live,
    /// Recovered after a gap (for example new followers polled after a reconnect).
    /// Shown in the feed, never alerted by default: a burst of stale alerts is worse than none.
    Backfill,
    /// The streamer pressed "replay" on a past event.
    Replay,
    /// A test button. Delivered only to the overlay or rule that asked for it.
    Test,
}

/// The channel an event belongs to. Multistreamers have one per platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelRef {
    pub platform: Platform,
    pub id: String,
    pub name: String,
}

bitflags_like! {
    /// What a person is in this channel. Kept as a small set so filters can test it cheaply.
    Roles: u16 {
        BROADCASTER = 1 << 0,
        MODERATOR = 1 << 1,
        VIP = 1 << 2,
        SUBSCRIBER = 1 << 3,
        MEMBER = 1 << 4,
        FOUNDER = 1 << 5,
        STAFF = 1 << 6,
        VERIFIED = 1 << 7,
        BOT = 1 << 8,
        FIRST_TIME = 1 << 9,
    }
}

/// A person on a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub platform: Platform,
    /// The platform's user id. `None` for anonymous gifts and tips.
    pub id: Option<String>,
    pub login: Option<String>,
    pub display_name: String,
    pub avatar_url: Option<String>,
    /// `#rrggbb`, when the platform gives one.
    pub color: Option<String>,
    #[serde(default)]
    pub roles: Roles,
    #[serde(default)]
    pub badges: Vec<Badge>,
    #[serde(default)]
    pub anonymous: bool,
}

impl Actor {
    /// A named person with no id, used for samples and tips.
    pub fn named(platform: Platform, name: &str) -> Self {
        Actor {
            platform,
            id: None,
            login: Some(name.to_lowercase()),
            display_name: name.to_string(),
            avatar_url: None,
            color: None,
            roles: Roles::empty(),
            badges: Vec::new(),
            anonymous: false,
        }
    }

    /// Someone who chose not to be named.
    pub fn anonymous(platform: Platform) -> Self {
        Actor {
            anonymous: true,
            ..Actor::named(platform, "Anonymous")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Badge {
    pub set: String,
    pub version: String,
    pub title: Option<String>,
    pub image_url: Option<String>,
}

/// Chat text as pieces. Overlays build DOM from these with `textContent`, never HTML (R-37).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Content {
    /// Plain text, always present (for TTS, filters, logs).
    pub text: String,
    pub fragments: Vec<Fragment>,
}

impl Content {
    /// Plain text with no emotes or mentions.
    pub fn plain(text: &str) -> Self {
        Content {
            text: text.to_string(),
            fragments: vec![Fragment::Text {
                text: text.to_string(),
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fragment {
    Text {
        text: String,
    },
    Emote {
        provider: EmoteProvider,
        id: String,
        name: String,
        url: String,
        zero_width: bool,
    },
    Mention {
        login: String,
    },
    Link {
        url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmoteProvider {
    Native,
    SevenTv,
    Bttv,
    Ffz,
}

/// Subscription or membership level. `Level` covers YouTube membership levels and
/// anything that isn't Twitch's 1/2/3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tier {
    Prime,
    Tier1,
    Tier2,
    Tier3,
    Level { id: String, name: String },
}

impl Tier {
    /// A number for conditions like "tier at least 2". Prime counts as tier 1.
    pub fn rank(&self) -> u8 {
        match self {
            Tier::Prime | Tier::Tier1 => 1,
            Tier::Tier2 => 2,
            Tier::Tier3 => 3,
            Tier::Level { .. } => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipProvider {
    Razorpay,
    Stripe,
    Paypal,
    KoFi,
}

/// Only `Captured` money is real enough to alert on (R-49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Pending,
    Captured,
    Refunded,
    Disputed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReversalReason {
    Refund,
    Chargeback,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMode {
    FollowersOnly,
    SubscribersOnly,
    EmoteOnly,
    Slow,
    UniqueChat,
}

/// Health of a platform connection. Never alerts; drives the status strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectorStatus {
    Connected,
    Reconnecting {
        attempt: u32,
    },
    Disconnected {
        reason: String,
    },
    /// Sign-in expired: one clear "Reconnect" banner, not a stream of errors (R-45).
    SignInNeeded,
    ScopesMissing {
        scopes: Vec<String>,
    },
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    // chat
    ChatMessage {
        actor: Actor,
        content: Content,
        reply_to: Option<String>,
        is_action: bool,
    },
    MessageDeleted {
        message_id: String,
    },
    UserMessagesCleared {
        target: Actor,
    },

    // audience
    Follow {
        actor: Actor,
    },
    Raid {
        from: Actor,
        viewers: u32,
    },
    ViewerCount {
        count: u64,
    },

    // support
    Subscription {
        actor: Actor,
        tier: Tier,
        months: u32,
        message: Option<Content>,
        gifted_by: Option<Actor>,
    },
    GiftBatch {
        gifter: Actor,
        tier: Tier,
        count: u32,
    },
    PlatformGift {
        actor: Actor,
        value: PlatformValue,
        message: Option<Content>,
    },
    PaidMessage {
        actor: Actor,
        money: Money,
        content: Option<Content>,
        sticker_url: Option<String>,
    },
    Tip {
        actor: Actor,
        money: Money,
        message: Option<String>,
        provider: TipProvider,
        payment_ref: String,
        status: PaymentStatus,
    },
    /// Undoes the money side of an earlier event (refund, chargeback).
    Reversal {
        original_source_id: String,
        reason: ReversalReason,
    },

    // community
    RewardRedemption {
        actor: Actor,
        reward: String,
        cost: u64,
        input: Option<String>,
    },

    // moderation
    Ban {
        target: Actor,
        reason: Option<String>,
        until: Option<Millis>,
    },
    Unban {
        target: Actor,
    },
    ModeChanged {
        mode: ChatMode,
        enabled: bool,
    },

    // stream
    StreamOnline {
        title: Option<String>,
    },
    StreamOffline,
    AdBreak {
        seconds: u32,
    },

    // connection health
    Connector(ConnectorStatus),

    /// Something we don't understand yet. Kept with the raw payload so schema drift is
    /// visible instead of silently dropped (R-27).
    Unknown {
        name: String,
    },
}

impl EventKind {
    /// A short stable name, used for rules, filters and the SE-compat layer.
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::ChatMessage { .. } => "chat_message",
            EventKind::MessageDeleted { .. } => "message_deleted",
            EventKind::UserMessagesCleared { .. } => "user_messages_cleared",
            EventKind::Follow { .. } => "follow",
            EventKind::Raid { .. } => "raid",
            EventKind::ViewerCount { .. } => "viewer_count",
            EventKind::Subscription { .. } => "subscription",
            EventKind::GiftBatch { .. } => "gift_batch",
            EventKind::PlatformGift { .. } => "platform_gift",
            EventKind::PaidMessage { .. } => "paid_message",
            EventKind::Tip { .. } => "tip",
            EventKind::Reversal { .. } => "reversal",
            EventKind::RewardRedemption { .. } => "reward_redemption",
            EventKind::Ban { .. } => "ban",
            EventKind::Unban { .. } => "unban",
            EventKind::ModeChanged { .. } => "mode_changed",
            EventKind::StreamOnline { .. } => "stream_online",
            EventKind::StreamOffline => "stream_offline",
            EventKind::AdBreak { .. } => "ad_break",
            EventKind::Connector(_) => "connector",
            EventKind::Unknown { .. } => "unknown",
        }
    }

    /// The person the event is about, when there is one.
    pub fn actor(&self) -> Option<&Actor> {
        match self {
            EventKind::ChatMessage { actor, .. }
            | EventKind::Follow { actor }
            | EventKind::Subscription { actor, .. }
            | EventKind::PlatformGift { actor, .. }
            | EventKind::PaidMessage { actor, .. }
            | EventKind::Tip { actor, .. }
            | EventKind::RewardRedemption { actor, .. } => Some(actor),
            EventKind::GiftBatch { gifter, .. } => Some(gifter),
            EventKind::Raid { from, .. } => Some(from),
            EventKind::UserMessagesCleared { target }
            | EventKind::Ban { target, .. }
            | EventKind::Unban { target } => Some(target),
            _ => None,
        }
    }

    /// Kinds an alert box can show. Chat, moderation and health events never alert.
    pub fn is_alertable(&self) -> bool {
        match self {
            EventKind::Follow { .. }
            | EventKind::Raid { .. }
            | EventKind::Subscription { .. }
            | EventKind::GiftBatch { .. }
            | EventKind::PlatformGift { .. }
            | EventKind::PaidMessage { .. }
            | EventKind::RewardRedemption { .. } => true,
            EventKind::Tip { status, .. } => *status == PaymentStatus::Captured,
            _ => false,
        }
    }
}

/// The envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Ferrocast's own id (ULID string): sortable, unique across platforms.
    pub id: String,
    /// Monotonic within one app run; overlays resync from it.
    pub seq: u64,
    pub platform: Platform,
    pub channel: Option<ChannelRef>,
    /// The platform's id for this event or message: the dedupe key with `platform`.
    pub source_id: Option<String>,
    pub occurred_at: Millis,
    pub received_at: Millis,
    pub origin: Origin,
    pub kind: EventKind,
    /// The original payload, size-capped by the adapter. Never sent to overlays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl Event {
    /// A new live event; `id` and `seq` are filled in by the bus.
    pub fn live(
        platform: Platform,
        source_id: Option<String>,
        at: Millis,
        kind: EventKind,
    ) -> Self {
        Event {
            id: String::new(),
            seq: 0,
            platform,
            channel: None,
            source_id,
            occurred_at: at,
            received_at: at,
            origin: Origin::Live,
            kind,
            raw: None,
        }
    }

    /// Whether the alert pipeline should consider this event at all.
    pub fn may_alert(&self) -> bool {
        matches!(self.origin, Origin::Live | Origin::Replay | Origin::Test)
            && self.kind.is_alertable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_alert_only_when_captured() {
        for (status, alerts) in [
            (PaymentStatus::Pending, false),
            (PaymentStatus::Captured, true),
            (PaymentStatus::Refunded, false),
            (PaymentStatus::Disputed, false),
        ] {
            let kind = EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Asha"),
                money: Money::new(50_000, "INR"),
                message: None,
                provider: TipProvider::Razorpay,
                payment_ref: "pay_1".into(),
                status,
            };
            assert_eq!(kind.is_alertable(), alerts, "{status:?}");
        }
    }

    #[test]
    fn backfill_never_alerts() {
        let mut e = Event::live(
            Platform::Twitch,
            Some("m1".into()),
            Millis(0),
            EventKind::Follow {
                actor: Actor::named(Platform::Twitch, "a"),
            },
        );
        assert!(e.may_alert());
        e.origin = Origin::Backfill;
        assert!(!e.may_alert());
    }

    #[test]
    fn chat_and_health_never_alert() {
        let chat = EventKind::ChatMessage {
            actor: Actor::named(Platform::Kick, "x"),
            content: Content::plain("hi"),
            reply_to: None,
            is_action: false,
        };
        assert!(!chat.is_alertable());
        assert!(!EventKind::Connector(ConnectorStatus::Connected).is_alertable());
    }

    #[test]
    fn roles_set_and_names() {
        let mut r = Roles::MODERATOR;
        r.insert(Roles::VIP);
        assert!(r.contains(Roles::VIP) && r.contains(Roles::MODERATOR));
        assert!(!r.contains(Roles::BROADCASTER));
        assert_eq!(r.names(), vec!["MODERATOR", "VIP"]);
    }

    #[test]
    fn events_round_trip_through_json() {
        let e = Event::live(
            Platform::Youtube,
            Some("yt1".into()),
            Millis(5),
            EventKind::PaidMessage {
                actor: Actor::named(Platform::Youtube, "Ravi"),
                money: Money::new(19_900, "INR"),
                content: Some(Content::plain("great stream")),
                sticker_url: None,
            },
        );
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
        assert!(json.contains("\"type\":\"paid_message\""));
    }

    #[test]
    fn seconds_since_never_negative() {
        assert_eq!(Millis(5_000).seconds_since(Millis(1_000)), 4);
        assert_eq!(Millis(1_000).seconds_since(Millis(5_000)), 0);
    }
}
