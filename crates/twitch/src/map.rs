//! EventSub notifications → Ferrocast events, and the list of subscriptions we create.
//!
//! Parsing is lenient (R-27): a missing optional field never drops an event, and a type we
//! don't know becomes `Unknown` with the raw payload so drift shows up in the dashboard.

use ferrocast_core::event::{
    Actor, Badge, ChannelRef, Content, EmoteProvider, Event, EventKind, Fragment, Millis, Platform, Roles, Tier,
};
use ferrocast_core::money::{PlatformValue, ValueUnit};
use serde_json::{Value, json};

/// An EventSub subscription: type, version, and how to build its condition.
#[derive(Debug, Clone, Copy)]
pub struct Sub {
    pub kind: &'static str,
    pub version: &'static str,
    /// Which ids go into the condition.
    pub condition: Condition,
}

#[derive(Debug, Clone, Copy)]
pub enum Condition {
    Broadcaster,
    BroadcasterAndUser,
    BroadcasterAndModerator,
    ToBroadcaster,
}

impl Sub {
    pub fn condition_json(&self, broadcaster: &str) -> Value {
        match self.condition {
            Condition::Broadcaster => json!({ "broadcaster_user_id": broadcaster }),
            Condition::BroadcasterAndUser => {
                json!({ "broadcaster_user_id": broadcaster, "user_id": broadcaster })
            }
            Condition::BroadcasterAndModerator => {
                json!({ "broadcaster_user_id": broadcaster, "moderator_user_id": broadcaster })
            }
            Condition::ToBroadcaster => json!({ "to_broadcaster_user_id": broadcaster }),
        }
    }
}

/// Everything we listen to. Signed-in streamer is broadcaster, moderator and chat user, so
/// every subscription costs 0 against Twitch's limit.
pub const SUBSCRIPTIONS: &[Sub] = &[
    Sub { kind: "channel.chat.message", version: "1", condition: Condition::BroadcasterAndUser },
    Sub { kind: "channel.chat.message_delete", version: "1", condition: Condition::BroadcasterAndUser },
    Sub { kind: "channel.chat.clear_user_messages", version: "1", condition: Condition::BroadcasterAndUser },
    Sub { kind: "channel.follow", version: "2", condition: Condition::BroadcasterAndModerator },
    Sub { kind: "channel.subscribe", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "channel.subscription.message", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "channel.subscription.gift", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "channel.cheer", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "channel.raid", version: "1", condition: Condition::ToBroadcaster },
    Sub {
        kind: "channel.channel_points_custom_reward_redemption.add",
        version: "1",
        condition: Condition::Broadcaster,
    },
    Sub { kind: "channel.ban", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "stream.online", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "stream.offline", version: "1", condition: Condition::Broadcaster },
    Sub { kind: "channel.ad_break.begin", version: "1", condition: Condition::Broadcaster },
];

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// An actor from the usual `<prefix>_user_id/_login/_name` triple.
/// The plain viewer fields are `user_id/user_login/user_name`; others carry a prefix
/// (`chatter_user_id`, `from_broadcaster_user_id`, `target_user_id`).
fn actor(v: &Value, prefix: &str) -> Actor {
    let key = |field: &str| {
        if prefix == "user" { format!("user_{field}") } else { format!("{prefix}_user_{field}") }
    };
    let id = s(v, &key("id")).map(String::from);
    let login = s(v, &key("login")).map(String::from);
    let name = s(v, &key("name")).or(login.as_deref()).unwrap_or("Someone").to_string();
    Actor {
        platform: Platform::Twitch,
        anonymous: id.is_none(),
        id,
        login,
        display_name: name,
        avatar_url: None,
        color: None,
        roles: Roles::empty(),
        badges: Vec::new(),
    }
}

fn tier(v: &Value) -> Tier {
    match v.get("tier").and_then(Value::as_str).unwrap_or("1000") {
        "2000" => Tier::Tier2,
        "3000" => Tier::Tier3,
        _ if v.get("is_prime").and_then(Value::as_bool) == Some(true) => Tier::Prime,
        _ => Tier::Tier1,
    }
}

fn roles_from_badges(badges: &[Value]) -> (Roles, Vec<Badge>) {
    let mut roles = Roles::empty();
    let mut out = Vec::new();
    for b in badges {
        let set = s(b, "set_id").unwrap_or("").to_string();
        match set.as_str() {
            "broadcaster" => roles.insert(Roles::BROADCASTER),
            "moderator" => roles.insert(Roles::MODERATOR),
            "vip" => roles.insert(Roles::VIP),
            "subscriber" => roles.insert(Roles::SUBSCRIBER),
            "founder" => roles.insert(Roles::FOUNDER.union(Roles::SUBSCRIBER)),
            "staff" | "admin" | "global_mod" => roles.insert(Roles::STAFF),
            "partner" => roles.insert(Roles::VERIFIED),
            _ => {}
        }
        out.push(Badge {
            set,
            version: s(b, "id").unwrap_or("").to_string(),
            title: s(b, "info").map(String::from),
            image_url: None,
        });
    }
    (roles, out)
}

/// Chat fragments; emotes point at Twitch's CDN, never at markup.
fn content(message: &Value) -> Content {
    let text = s(message, "text").unwrap_or("").to_string();
    let mut fragments = Vec::new();
    for f in message.get("fragments").and_then(Value::as_array).into_iter().flatten() {
        let ftext = s(f, "text").unwrap_or("").to_string();
        match f.get("type").and_then(Value::as_str) {
            Some("emote") => {
                let id = f.pointer("/emote/id").and_then(Value::as_str).unwrap_or("").to_string();
                let animated = f
                    .pointer("/emote/format")
                    .and_then(Value::as_array)
                    .is_some_and(|fs| fs.iter().any(|x| x == "animated"));
                let kind = if animated { "animated" } else { "static" };
                fragments.push(Fragment::Emote {
                    provider: EmoteProvider::Native,
                    url: format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/{kind}/dark/2.0"),
                    id,
                    name: ftext,
                    zero_width: false,
                });
            }
            Some("mention") => {
                let login = f.pointer("/mention/user_login").and_then(Value::as_str).unwrap_or(&ftext).to_string();
                fragments.push(Fragment::Mention { login });
            }
            _ => fragments.push(Fragment::Text { text: ftext }),
        }
    }
    if fragments.is_empty() && !text.is_empty() {
        fragments.push(Fragment::Text { text: text.clone() });
    }
    Content { text, fragments }
}

/// Maps one EventSub notification. `message_id` is EventSub's delivery id (the dedupe key
/// for events without their own id); `at` is the notification time.
pub fn map(kind: &str, message_id: &str, event: &Value, at: Millis) -> Event {
    let mut source = Some(message_id.to_string());
    let mut channel = None;
    let kind = match kind {
        "channel.chat.message" => {
            let mut a = actor(event, "chatter");
            let badges = event.get("badges").and_then(Value::as_array).cloned().unwrap_or_default();
            let (roles, badges) = roles_from_badges(&badges);
            a.roles = roles;
            a.badges = badges;
            a.color = s(event, "color").map(String::from);
            // The chat message's own id is the key: mods delete and reply by it.
            source = s(event, "message_id").map(String::from).or(source);
            // Shared Chat: a message that started in a partner channel is marked with that
            // channel, so the bot never answers it twice (R-11).
            let src = s(event, "source_broadcaster_user_id");
            if let Some(src) = src.filter(|src| Some(*src) != s(event, "broadcaster_user_id")) {
                channel = Some(ChannelRef {
                    platform: Platform::Twitch,
                    id: src.to_string(),
                    name: s(event, "source_broadcaster_user_name").unwrap_or("").to_string(),
                });
            }
            EventKind::ChatMessage {
                actor: a,
                content: content(event.get("message").unwrap_or(&Value::Null)),
                reply_to: event.pointer("/reply/parent_message_id").and_then(Value::as_str).map(String::from),
                is_action: false,
            }
        }
        "channel.chat.message_delete" => {
            EventKind::MessageDeleted { message_id: s(event, "message_id").unwrap_or("").to_string() }
        }
        "channel.chat.clear_user_messages" => EventKind::UserMessagesCleared { target: actor(event, "target") },
        "channel.follow" => EventKind::Follow { actor: actor(event, "user") },
        "channel.subscribe" => {
            let gifted = event.get("is_gift").and_then(Value::as_bool).unwrap_or(false);
            EventKind::Subscription {
                actor: actor(event, "user"),
                tier: tier(event),
                months: 1,
                message: None,
                // EventSub doesn't name the gifter here; the gift event does. Marking it
                // gifted keeps it out of alerts and sub counts (the batch counts once).
                gifted_by: gifted.then(|| Actor::anonymous(Platform::Twitch)),
            }
        }
        "channel.subscription.message" => EventKind::Subscription {
            actor: actor(event, "user"),
            tier: tier(event),
            months: u32::try_from(n(event, "cumulative_months").max(1)).unwrap_or(1),
            message: event.get("message").map(content).filter(|c| !c.text.is_empty()),
            gifted_by: None,
        },
        "channel.subscription.gift" => {
            let mut gifter = actor(event, "user");
            if event.get("is_anonymous").and_then(Value::as_bool) == Some(true) {
                gifter = Actor::anonymous(Platform::Twitch);
            }
            EventKind::GiftBatch {
                gifter,
                tier: tier(event),
                count: u32::try_from(n(event, "total").max(1)).unwrap_or(1),
            }
        }
        "channel.cheer" => {
            let mut a = actor(event, "user");
            if event.get("is_anonymous").and_then(Value::as_bool) == Some(true) {
                a = Actor::anonymous(Platform::Twitch);
            }
            EventKind::PlatformGift {
                actor: a,
                value: PlatformValue { unit: ValueUnit::Bits, amount: n(event, "bits") },
                message: s(event, "message").map(Content::plain),
            }
        }
        "channel.raid" => EventKind::Raid {
            from: actor(event, "from_broadcaster"),
            viewers: u32::try_from(n(event, "viewers")).unwrap_or(u32::MAX),
        },
        "channel.channel_points_custom_reward_redemption.add" => EventKind::RewardRedemption {
            actor: actor(event, "user"),
            reward: event.pointer("/reward/title").and_then(Value::as_str).unwrap_or("a reward").to_string(),
            cost: event.pointer("/reward/cost").and_then(Value::as_u64).unwrap_or(0),
            input: s(event, "user_input").map(String::from),
        },
        "channel.ban" => {
            EventKind::Ban { target: actor(event, "user"), reason: s(event, "reason").map(String::from), until: None }
        }
        "stream.online" => EventKind::StreamOnline { title: None },
        "stream.offline" => EventKind::StreamOffline,
        "channel.ad_break.begin" => {
            EventKind::AdBreak { seconds: u32::try_from(n(event, "duration_seconds")).unwrap_or(0) }
        }
        other => EventKind::Unknown { name: other.to_string() },
    };
    let mut e = Event::live(Platform::Twitch, source, at, kind);
    e.channel = channel;
    if matches!(e.kind, EventKind::Unknown { .. }) {
        e.raw = Some(event.clone());
    }
    e
}
