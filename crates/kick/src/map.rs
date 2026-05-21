//! Kick webhook payloads → Ferrocast events. Lenient: Kick's payloads change without
//! notice (R-25), so missing fields fall back instead of dropping the event.

use ferrocast_core::event::{Actor, Content, EmoteProvider, Event, EventKind, Fragment, Millis, Platform, Roles, Tier};
use ferrocast_core::money::{PlatformValue, ValueUnit};
use serde_json::Value;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

/// Kick user ids are numbers; we keep them as strings like every other platform.
fn id_of(v: &Value) -> Option<String> {
    match v.get("user_id") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn actor(v: Option<&Value>) -> Actor {
    let Some(v) = v.filter(|v| v.is_object()) else { return Actor::anonymous(Platform::Kick) };
    if v.get("is_anonymous").and_then(Value::as_bool) == Some(true) {
        return Actor::anonymous(Platform::Kick);
    }
    let login = s(v, "channel_slug").or(s(v, "username")).map(String::from);
    let mut roles = Roles::empty();
    let mut badges = Vec::new();
    if let Some(list) = v.pointer("/identity/badges").and_then(Value::as_array) {
        for b in list {
            let ty = s(b, "type").unwrap_or("");
            match ty {
                "broadcaster" => roles.insert(Roles::BROADCASTER),
                "moderator" => roles.insert(Roles::MODERATOR),
                "vip" | "og" => roles.insert(Roles::VIP),
                "subscriber" | "founder" => roles.insert(Roles::SUBSCRIBER),
                "staff" => roles.insert(Roles::STAFF),
                "verified" => roles.insert(Roles::VERIFIED),
                _ => {}
            }
            badges.push(ferrocast_core::event::Badge {
                set: ty.to_string(),
                version: b.get("count").map(|c| c.to_string()).unwrap_or_default(),
                title: s(b, "text").map(String::from),
                image_url: None,
            });
        }
    }
    if v.get("is_verified").and_then(Value::as_bool) == Some(true) {
        roles.insert(Roles::VERIFIED);
    }
    Actor {
        platform: Platform::Kick,
        id: id_of(v),
        display_name: s(v, "username").unwrap_or("Someone").to_string(),
        login,
        avatar_url: s(v, "profile_picture").map(String::from),
        color: v.pointer("/identity/username_color").and_then(Value::as_str).map(String::from),
        roles,
        badges,
        anonymous: false,
    }
}

/// Kick writes emotes inline as `[emote:ID:NAME]`. They become emote fragments so text
/// never carries markup.
pub fn content(raw: &str) -> Content {
    let mut fragments = Vec::new();
    let mut plain = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("[emote:") {
        let (before, after) = rest.split_at(start);
        let Some(end) = after.find(']') else { break };
        let inner = &after[7..end];
        match inner.split_once(':') {
            Some((id, name)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => {
                if !before.is_empty() {
                    fragments.push(Fragment::Text { text: before.to_string() });
                }
                plain.push_str(before);
                plain.push_str(name);
                fragments.push(Fragment::Emote {
                    provider: EmoteProvider::Native,
                    url: format!("https://files.kick.com/emotes/{id}/fullsize"),
                    id: id.to_string(),
                    name: name.to_string(),
                    zero_width: false,
                });
            }
            _ => {
                let literal = &after[..=end];
                fragments.push(Fragment::Text { text: format!("{before}{literal}") });
                plain.push_str(before);
                plain.push_str(literal);
            }
        }
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        fragments.push(Fragment::Text { text: rest.to_string() });
        plain.push_str(rest);
    }
    Content { text: plain, fragments }
}

/// Maps one webhook. `message_id` is Kick's `Kick-Event-Message-Id` (the dedupe key, R-29).
pub fn map(event_type: &str, message_id: &str, body: &Value, at: Millis) -> Event {
    let mut source = Some(message_id.to_string()).filter(|m| !m.is_empty());
    let kind = match event_type {
        "chat.message.sent" => {
            // Chat uses its own message id so mods can delete and reply to it.
            source = s(body, "message_id").map(String::from).or(source);
            EventKind::ChatMessage {
                actor: actor(body.get("sender")),
                content: content(s(body, "content").unwrap_or("")),
                reply_to: body.pointer("/replies_to/message_id").and_then(Value::as_str).map(String::from),
                is_action: false,
            }
        }
        "channel.followed" => EventKind::Follow { actor: actor(body.get("follower")) },
        "channel.subscription.new" | "channel.subscription.renewal" => EventKind::Subscription {
            actor: actor(body.get("subscriber")),
            tier: Tier::Tier1,
            months: u32::try_from(body.get("duration").and_then(Value::as_u64).unwrap_or(1).max(1)).unwrap_or(1),
            message: None,
            gifted_by: None,
        },
        "channel.subscription.gifts" => EventKind::GiftBatch {
            gifter: actor(body.get("gifter")),
            tier: Tier::Tier1,
            count: u32::try_from(body.get("giftees").and_then(Value::as_array).map_or(1, |g| g.len().max(1)))
                .unwrap_or(1),
        },
        "channel.reward.redemption.updated" => {
            // Kick sends updates (pending → accepted/rejected); only a fresh redemption alerts.
            let status = s(body, "status").unwrap_or("pending");
            if status != "pending" {
                EventKind::Unknown { name: format!("reward_redemption_{status}") }
            } else {
                EventKind::RewardRedemption {
                    actor: actor(body.get("redeemer")),
                    reward: body.pointer("/reward/title").and_then(Value::as_str).unwrap_or("a reward").to_string(),
                    cost: body.pointer("/reward/cost").and_then(Value::as_u64).unwrap_or(0),
                    input: s(body, "user_input").map(String::from),
                }
            }
        }
        "livestream.status.updated" => {
            if body.get("is_live").and_then(Value::as_bool).unwrap_or(false) {
                EventKind::StreamOnline { title: s(body, "title").map(String::from) }
            } else {
                EventKind::StreamOffline
            }
        }
        "moderation.banned" => EventKind::Ban {
            target: actor(body.get("banned_user")),
            reason: body.pointer("/metadata/reason").and_then(Value::as_str).map(String::from),
            until: None,
        },
        "kicks.gifted" => EventKind::PlatformGift {
            actor: actor(body.get("sender")),
            value: PlatformValue {
                unit: ValueUnit::Kicks,
                amount: body.pointer("/gift/amount").and_then(Value::as_u64).unwrap_or(0),
            },
            message: body
                .pointer("/gift/message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(Content::plain),
        },
        other => EventKind::Unknown { name: other.to_string() },
    };
    let mut e = Event::live(Platform::Kick, source, at, kind);
    if matches!(e.kind, EventKind::Unknown { .. }) {
        e.raw = Some(body.clone());
    }
    e
}
