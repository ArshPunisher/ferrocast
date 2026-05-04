//! Tests against a fake Twitch (ID server, Helix and an EventSub WebSocket the test drives).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrocast_core::event::{ChatMode, ConnectorStatus, EventKind, Fragment, Roles};
use ferrocast_core::moderation::ModeChange;
use ferrocast_testkit::twitch::{notification, push};
use serde_json::json;
use tokio::sync::mpsc;

use super::*;

async fn fake() -> (ferrocast_testkit::twitch::St, Endpoints) {
    let (st, (id, api, eventsub)) = ferrocast_testkit::twitch::fake().await;
    (st, Endpoints { id, api, eventsub })
}
use crate::auth::{Poll, Tokens};
use crate::helix::Helix;

fn auth_for(ep: &Endpoints, saved: Arc<Mutex<Vec<Tokens>>>) -> Arc<Auth> {
    let tokens = Tokens {
        access_token: "access-0".into(),
        refresh_token: "refresh-0".into(),
        expires_at: now_ms() + 3_600_000,
        scopes: vec![],
    };
    Arc::new(Auth::new(
        http_client(),
        ep.clone(),
        "client".into(),
        tokens,
        Arc::new(move |t: &Tokens| {
            saved.lock().unwrap().push(t.clone());
            Ok(())
        }),
    ))
}

async fn next_event(rx: &mut mpsc::Receiver<ferrocast_core::event::Event>) -> ferrocast_core::event::Event {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("timed out").expect("closed")
}

async fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened");
}

#[tokio::test]
async fn device_code_sign_in() {
    let (_st, ep) = fake().await;
    let http = http_client();
    let code = auth::start(&http, &ep, "client").await.unwrap();
    assert_eq!(code.user_code, "ABCD-EFGH");
    assert_eq!(auth::poll(&http, &ep, "client", &code.device_code).await.unwrap(), Poll::Pending);
    let Poll::Done(t) = auth::poll(&http, &ep, "client", &code.device_code).await.unwrap() else { panic!("not done") };
    assert_eq!(t.access_token, "access-0");
    assert!(t.missing_scopes().is_empty());
    assert!(!t.needs_refresh(now_ms()));
}

#[tokio::test]
async fn a_401_refreshes_once_saves_the_new_tokens_and_retries() {
    let (st, ep) = fake().await;
    st.lock().unwrap().fail_next_with_401 = true;
    let saved = Arc::new(Mutex::new(Vec::new()));
    let helix = Helix::new(auth_for(&ep, saved.clone()));
    let me = helix.me().await.unwrap();
    assert_eq!(me.id, "111");
    assert_eq!(st.lock().unwrap().refreshes, 1);
    assert_eq!(saved.lock().unwrap()[0].refresh_token, "refresh-1", "the new single-use refresh token was saved");
}

#[tokio::test]
async fn a_revoked_refresh_token_means_sign_in_again() {
    let (_st, ep) = fake().await;
    let http = http_client();
    assert_eq!(auth::refresh(&http, &ep, "client", "revoked").await.unwrap_err(), TwitchError::SignInNeeded);
}

#[tokio::test]
async fn chat_that_twitch_drops_is_reported_not_silently_lost() {
    let (st, ep) = fake().await;
    let helix = Helix::new(auth_for(&ep, Arc::default()));
    helix.send_chat("111", "111", "hello", Some("parent")).await.unwrap();
    assert_eq!(st.lock().unwrap().chats[0]["reply_parent_message_id"], "parent");
    let err = helix.send_chat("111", "111", "automod bait", None).await.unwrap_err();
    assert!(err.to_string().contains("AutoMod"), "{err}");
}

#[tokio::test]
async fn eventsub_subscribes_maps_events_and_survives_a_handover() {
    let (st, ep) = fake().await;
    let helix = Helix::new(auth_for(&ep, Arc::default()));
    let (sink, mut rx) = mpsc::channel(64);
    tokio::spawn(eventsub::run(helix, "111".into(), ep.eventsub.clone(), sink));

    let first = next_event(&mut rx).await;
    assert!(
        matches!(&first.kind, EventKind::Connector(ConnectorStatus::ScopesMissing { scopes }) if scopes == &["channel.ad_break.begin"])
    );
    let connected = next_event(&mut rx).await;
    assert!(matches!(connected.kind, EventKind::Connector(ConnectorStatus::Connected)));
    let created = st.lock().unwrap().subs.len();
    assert_eq!(created, map::SUBSCRIPTIONS.len() - 1);
    assert!(st.lock().unwrap().subs.iter().all(|(s, _)| s == "session-1"));

    push(
        &st,
        "session-1",
        notification(
            "channel.follow",
            "n-1",
            json!({
        "user_id": "9", "user_login": "fan", "user_name": "Fan", "broadcaster_user_id": "111" }),
        ),
    );
    let follow = next_event(&mut rx).await;
    assert!(matches!(&follow.kind, EventKind::Follow { actor } if actor.display_name == "Fan"));
    assert_eq!(follow.source_id.as_deref(), Some("n-1"));

    // Twitch moves us. Subscriptions must carry over (no new ones), and an event sent to the
    // old socket during the switch still arrives.
    let base = st.lock().unwrap().base.replace("http", "ws");
    push(
        &st,
        "session-1",
        json!({ "metadata": { "message_type": "session_reconnect" },
                                   "payload": { "session": { "reconnect_url": format!("{base}/ws") } } }),
    );
    push(
        &st,
        "session-1",
        notification("channel.follow", "n-2", json!({ "user_id": "8", "user_login": "late", "user_name": "Late" })),
    );
    let during = next_event(&mut rx).await;
    assert_eq!(during.source_id.as_deref(), Some("n-2"));
    wait_for(|| st.lock().unwrap().sessions == 2).await;
    push(
        &st,
        "session-2",
        notification(
            "channel.raid",
            "n-3",
            json!({
        "from_broadcaster_user_id": "5", "from_broadcaster_user_login": "pal", "from_broadcaster_user_name": "Pal", "viewers": 42 }),
        ),
    );
    let raid = next_event(&mut rx).await;
    assert!(matches!(raid.kind, EventKind::Raid { viewers: 42, .. }));
    assert_eq!(st.lock().unwrap().subs.len(), created, "no re-subscription after a handover");
}

#[tokio::test]
async fn a_silent_socket_is_detected_and_replaced() {
    let (st, ep) = fake().await;
    st.lock().unwrap().keepalive = 1;
    let helix = Helix::new(auth_for(&ep, Arc::default()));
    let (sink, mut rx) = mpsc::channel(64);
    tokio::spawn(eventsub::run(helix, "111".into(), ep.eventsub.clone(), sink));
    let mut saw_reconnecting = false;
    for _ in 0..6 {
        if let EventKind::Connector(ConnectorStatus::Reconnecting { .. }) = next_event(&mut rx).await.kind {
            saw_reconnecting = true;
            break;
        }
    }
    assert!(saw_reconnecting, "the watchdog noticed the dead socket");
    wait_for(|| st.lock().unwrap().sessions >= 2).await;
}

#[test]
fn chat_mapping_with_emotes_badges_replies_and_shared_chat() {
    let ev = json!({
        "broadcaster_user_id": "111", "chatter_user_id": "42", "chatter_user_login": "pix", "chatter_user_name": "Pix",
        "message_id": "abc", "color": "#FF0000",
        "message": { "text": "hi Kappa @streamer", "fragments": [
            { "type": "text", "text": "hi " },
            { "type": "emote", "text": "Kappa", "emote": { "id": "25", "format": ["static"] } },
            { "type": "text", "text": " " },
            { "type": "mention", "text": "@streamer", "mention": { "user_login": "streamer" } } ] },
        "badges": [{ "set_id": "moderator", "id": "1", "info": "" }, { "set_id": "subscriber", "id": "12", "info": "12" }],
        "reply": { "parent_message_id": "parent-1" },
        "source_broadcaster_user_id": "777", "source_broadcaster_user_name": "Partner"
    });
    let e = map::map("channel.chat.message", "delivery-1", &ev, ferrocast_core::event::Millis(1));
    assert_eq!(e.source_id.as_deref(), Some("abc"), "chat uses the message id, not the delivery id");
    assert_eq!(e.channel.as_ref().map(|c| c.id.as_str()), Some("777"));
    let EventKind::ChatMessage { actor, content, reply_to, .. } = e.kind else { panic!() };
    assert!(actor.roles.contains(Roles::MODERATOR) && actor.roles.contains(Roles::SUBSCRIBER));
    assert_eq!(reply_to.as_deref(), Some("parent-1"));
    assert!(matches!(&content.fragments[1], Fragment::Emote { url, .. } if url.contains("/25/static/")));
    assert!(matches!(&content.fragments[3], Fragment::Mention { login } if login == "streamer"));
}

#[test]
fn support_mapping_and_unknown_types() {
    let at = ferrocast_core::event::Millis(1);
    let gift = map::map(
        "channel.subscription.gift",
        "d",
        &json!({ "user_id": "1", "user_name": "G", "total": 5, "tier": "2000", "is_anonymous": false }),
        at,
    );
    assert!(
        matches!(&gift.kind, EventKind::GiftBatch { count: 5, tier: ferrocast_core::event::Tier::Tier2, gifter } if gifter.display_name == "G" && gifter.id.as_deref() == Some("1"))
    );
    let cleared = map::map(
        "channel.chat.clear_user_messages",
        "d6",
        &json!({ "target_user_id": "66", "target_user_login": "t", "target_user_name": "T" }),
        at,
    );
    assert!(matches!(&cleared.kind, EventKind::UserMessagesCleared { target } if target.id.as_deref() == Some("66")));
    let recipient = map::map(
        "channel.subscribe",
        "d2",
        &json!({ "user_id": "2", "user_name": "R", "tier": "1000", "is_gift": true }),
        at,
    );
    assert!(!recipient.kind.is_alertable(), "gift recipients don't alert one by one");
    let cheer =
        map::map("channel.cheer", "d3", &json!({ "is_anonymous": true, "bits": 100, "message": "Cheer100" }), at);
    let EventKind::PlatformGift { actor, value, .. } = cheer.kind else { panic!() };
    assert!(actor.anonymous);
    assert_eq!(value.amount, 100);
    let weird = map::map("channel.something_new", "d4", &json!({ "x": 1 }), at);
    assert!(matches!(weird.kind, EventKind::Unknown { .. }));
    assert!(weird.raw.is_some(), "unknown types keep their payload");
    let empty = map::map("channel.follow", "d5", &json!({}), at);
    assert!(matches!(&empty.kind, EventKind::Follow { actor } if actor.display_name == "Someone"), "lenient parsing");
}

#[test]
fn chat_mode_bodies() {
    let on = connector::mode_patch(ModeChange { mode: ChatMode::Slow, enabled: true, value: Some(1) });
    assert_eq!(on, json!({ "slow_mode": true, "slow_mode_wait_time": 3 }), "Twitch's minimum is 3 s");
    let f = connector::mode_patch(ModeChange { mode: ChatMode::FollowersOnly, enabled: true, value: Some(10) });
    assert_eq!(f["follower_mode_duration"], 10);
    assert_eq!(
        connector::mode_patch(ModeChange { mode: ChatMode::EmoteOnly, enabled: false, value: None }),
        json!({ "emote_mode": false })
    );
}

#[test]
fn backoff_grows_and_caps() {
    assert!(eventsub::backoff(1) < Duration::from_secs(2));
    assert!(eventsub::backoff(3) >= Duration::from_secs(4));
    assert!(eventsub::backoff(50) <= Duration::from_millis(60_500));
}
