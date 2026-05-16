//! YouTube live chat messages → Ferrocast events.

use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform, Roles, Tier};
use ferrocast_core::money::{Money, PlatformValue, ValueUnit, exponent};

use crate::proto::live_chat_message_snippet::DisplayedContent;
use crate::proto::live_chat_message_snippet::type_wrapper::Type;
use crate::proto::{LiveChatMessage, LiveChatMessageAuthorDetails};

fn author(a: Option<&LiveChatMessageAuthorDetails>) -> Actor {
    let Some(a) = a else { return Actor::anonymous(Platform::Youtube) };
    let mut roles = Roles::empty();
    if a.is_chat_owner() {
        roles.insert(Roles::BROADCASTER);
    }
    if a.is_chat_moderator() {
        roles.insert(Roles::MODERATOR);
    }
    if a.is_chat_sponsor() {
        roles.insert(Roles::MEMBER);
    }
    if a.is_verified() {
        roles.insert(Roles::VERIFIED);
    }
    Actor {
        platform: Platform::Youtube,
        id: a.channel_id.clone().filter(|s| !s.is_empty()),
        login: None,
        display_name: a.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "Someone".into()),
        avatar_url: a.profile_image_url.clone(),
        color: None,
        roles,
        badges: Vec::new(),
        anonymous: false,
    }
}

/// Super Chat amounts arrive in micros (1,750,000 = 1.75); money is kept in minor units.
pub fn micros_to_money(micros: u64, currency: &str) -> Money {
    let currency = currency.to_ascii_uppercase();
    let exp = exponent(&currency);
    let minor = (u128::from(micros) * 10u128.pow(exp) / 1_000_000).min(i64::MAX as u128);
    Money::new(i64::try_from(minor).unwrap_or(i64::MAX), &currency)
}

fn level(name: Option<&str>) -> Tier {
    let name = name.filter(|n| !n.is_empty()).unwrap_or("Member");
    Tier::Level { id: name.to_lowercase(), name: name.to_string() }
}

/// Maps one message, or `None` for kinds we don't show (polls, sponsor-mode notices).
pub fn map(m: &LiveChatMessage, at: Millis) -> Option<Event> {
    let snippet = m.snippet.as_ref()?;
    let actor = author(m.author_details.as_ref());
    let id = m.id.clone().filter(|s| !s.is_empty());
    let kind = match (snippet.r#type(), snippet.displayed_content.as_ref()) {
        (Type::TextMessageEvent, Some(DisplayedContent::TextMessageDetails(d))) => EventKind::ChatMessage {
            actor,
            content: Content::plain(d.message_text()),
            reply_to: None,
            is_action: false,
        },
        (Type::SuperChatEvent, Some(DisplayedContent::SuperChatDetails(d))) => EventKind::PaidMessage {
            actor,
            money: micros_to_money(d.amount_micros(), d.currency()),
            content: d.user_comment.as_deref().filter(|c| !c.is_empty()).map(Content::plain),
            sticker_url: None,
        },
        (Type::SuperStickerEvent, Some(DisplayedContent::SuperStickerDetails(d))) => EventKind::PaidMessage {
            actor,
            money: micros_to_money(d.amount_micros(), d.currency()),
            content: d.super_sticker_metadata.as_ref().and_then(|s| s.alt_text.clone()).map(|t| Content::plain(&t)),
            sticker_url: None,
        },
        (Type::NewSponsorEvent, details) => {
            let name = match details {
                Some(DisplayedContent::NewSponsorDetails(d)) => d.member_level_name.as_deref(),
                _ => None,
            };
            EventKind::Subscription { actor, tier: level(name), months: 1, message: None, gifted_by: None }
        }
        (Type::MemberMilestoneChatEvent, Some(DisplayedContent::MemberMilestoneChatDetails(d))) => {
            EventKind::Subscription {
                actor,
                tier: level(d.member_level_name.as_deref()),
                months: d.member_month().max(1),
                message: d.user_comment.as_deref().filter(|c| !c.is_empty()).map(Content::plain),
                gifted_by: None,
            }
        }
        (Type::MembershipGiftingEvent, Some(DisplayedContent::MembershipGiftingDetails(d))) => EventKind::GiftBatch {
            gifter: actor,
            tier: level(d.gift_memberships_level_name.as_deref()),
            count: u32::try_from(d.gift_memberships_count().max(1)).unwrap_or(1),
        },
        (Type::GiftMembershipReceivedEvent, Some(DisplayedContent::GiftMembershipReceivedDetails(d))) => {
            let mut gifter = Actor::anonymous(Platform::Youtube);
            gifter.id = d.gifter_channel_id.clone();
            EventKind::Subscription {
                actor,
                tier: level(d.member_level_name.as_deref()),
                months: 1,
                message: None,
                gifted_by: Some(gifter),
            }
        }
        (Type::GiftEvent, Some(DisplayedContent::GiftDetails(d))) => EventKind::PlatformGift {
            actor,
            value: PlatformValue {
                unit: ValueUnit::Jewels,
                amount: u64::try_from(d.jewels_amount().max(0)).unwrap_or(0)
                    * u64::try_from(d.combo_count().max(1)).unwrap_or(1),
            },
            message: None,
        },
        (Type::UserBannedEvent, Some(DisplayedContent::UserBannedDetails(d))) => {
            let target = d.banned_user_details.as_ref().map(|u| Actor {
                platform: Platform::Youtube,
                id: u.channel_id.clone(),
                login: None,
                display_name: u.display_name.clone().unwrap_or_else(|| "Someone".into()),
                avatar_url: u.profile_image_url.clone(),
                color: None,
                roles: Roles::empty(),
                badges: Vec::new(),
                anonymous: false,
            })?;
            EventKind::Ban { target, reason: None, until: None }
        }
        // A tombstone replaces a deleted message under the same id.
        (Type::Tombstone, _) => EventKind::MessageDeleted { message_id: id.clone()? },
        (Type::ChatEndedEvent, _) => EventKind::StreamOffline,
        _ => return None,
    };
    // Tombstones and chat-end notices reuse ids of earlier messages; give them their own key.
    let source = match kind {
        EventKind::MessageDeleted { .. } | EventKind::StreamOffline => {
            id.map(|i| format!("{i}#{}", snippet.r#type().as_str_name()))
        }
        _ => id,
    };
    Some(Event::live(Platform::Youtube, source, at, kind))
}
