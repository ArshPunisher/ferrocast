use std::sync::{Arc, Mutex};

use ferrocast_core::event::{EventKind, Fragment, Millis, Roles};
use serde_json::{Value, json};

use super::*;

#[test]
fn emotes_become_fragments_and_junk_stays_text() {
    let c = map::content("Hello [emote:4148074:HYPERCLAP] gg [emote:37226:KEKW]");
    assert_eq!(c.text, "Hello HYPERCLAP gg KEKW");
    assert!(matches!(&c.fragments[1], Fragment::Emote { id, name, .. } if id == "4148074" && name == "HYPERCLAP"));
    for (raw, plain) in [
        ("[emote:abc:x] hi", "[emote:abc:x] hi"),
        ("[emote:12] hi", "[emote:12] hi"),
        ("unterminated [emote:1:x", "unterminated [emote:1:x"),
        ("<img src=x onerror=alert(1)>", "<img src=x onerror=alert(1)>"),
        ("नमस्ते [emote:1:wave]", "नमस्ते wave"),
        ("", ""),
    ] {
        assert_eq!(map::content(raw).text, plain, "{raw:?}");
    }
}

#[test]
fn maps_the_documented_payloads() {
    let at = Millis(1);
    let chat = map::map(
        "chat.message.sent",
        "evt-1",
        &json!({
            "message_id": "msg-1",
            "replies_to": { "message_id": "msg-0" },
            "sender": { "is_anonymous": false, "user_id": 987654321, "username": "sender_name", "channel_slug": "sender_channel",
                        "identity": { "username_color": "#FF5733", "badges": [{ "text": "Moderator", "type": "moderator" }, { "text": "Subscriber", "type": "subscriber", "count": 3 }] } },
            "content": "hi [emote:37226:KEKW]",
        }),
        at,
    );
    assert_eq!(chat.source_id.as_deref(), Some("msg-1"));
    let EventKind::ChatMessage { actor, reply_to, .. } = chat.kind else { panic!() };
    assert_eq!(actor.id.as_deref(), Some("987654321"));
    assert!(actor.roles.contains(Roles::MODERATOR) && actor.roles.contains(Roles::SUBSCRIBER));
    assert_eq!(reply_to.as_deref(), Some("msg-0"));
    let gifts = map::map(
        "channel.subscription.gifts",
        "evt-2",
        &json!({
        "gifter": { "is_anonymous": true, "user_id": null }, "giftees": [{ "user_id": 1 }, { "user_id": 2 }, { "user_id": 3 }] }),
        at,
    );
    assert!(matches!(&gifts.kind, EventKind::GiftBatch { count: 3, gifter, .. } if gifter.anonymous));
    let kicks = map::map(
        "kicks.gifted",
        "evt-3",
        &json!({ "sender": { "user_id": 5, "username": "big" }, "gift": { "amount": 500, "message": "w" } }),
        at,
    );
    assert!(matches!(&kicks.kind, EventKind::PlatformGift { value, .. } if value.amount == 500));
    let renewal = map::map(
        "channel.subscription.renewal",
        "evt-4",
        &json!({ "subscriber": { "user_id": 9, "username": "loyal" }, "duration": 3 }),
        at,
    );
    assert!(matches!(renewal.kind, EventKind::Subscription { months: 3, .. }));
    let live = map::map("livestream.status.updated", "evt-5", &json!({ "is_live": true, "title": "go" }), at);
    assert!(matches!(live.kind, EventKind::StreamOnline { .. }));
    let ended = map::map("livestream.status.updated", "evt-6", &json!({ "is_live": false }), at);
    assert!(matches!(ended.kind, EventKind::StreamOffline));
    let accepted = map::map("channel.reward.redemption.updated", "evt-7", &json!({ "status": "accepted" }), at);
    assert!(!accepted.kind.is_alertable(), "only a new redemption alerts, not its later updates");
    let weird = map::map("channel.brand_new", "evt-8", &json!({ "x": 1 }), at);
    assert!(weird.raw.is_some());
    let empty = map::map("channel.followed", "evt-9", &Value::Null, at);
    assert!(matches!(empty.kind, EventKind::Follow { .. }), "a body Kick sent empty still maps");
}

#[derive(Default)]
struct Seen {
    bodies: Vec<(String, Value)>,
    refreshed: u32,
}

async fn fake_api(fail_first: bool) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let first = Arc::new(Mutex::new(fail_first));
    let handler =
        move |method: axum::http::Method, uri: axum::http::Uri, headers: axum::http::HeaderMap, body: String| {
            let s = s.clone();
            let first = first.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                if auth == "Bearer old" && std::mem::replace(&mut *first.lock().unwrap(), false) {
                    return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({ "message": "expired" })));
                }
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                s.lock().unwrap().bodies.push((format!("{method} {}", uri.path()), v));
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({ "data": [{ "user_id": 42, "name": "streamer" }], "message": "OK" })),
                )
            }
        };
    let app = axum::Router::new().fallback(handler);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, seen)
}

fn api(base: &str, seen: Arc<Mutex<Seen>>) -> KickApi {
    let tokens = KickTokens { access_token: "old".into(), refresh_token: "r".into(), expires_at: now_ms() + 3_600_000 };
    KickApi::new(
        base,
        tokens,
        Arc::new(move |_rt| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().refreshed += 1;
                Ok(KickTokens {
                    access_token: "new".into(),
                    refresh_token: "r2".into(),
                    expires_at: now_ms() + 3_600_000,
                })
            })
        }),
        Arc::new(|_| Ok(())),
    )
}

#[tokio::test]
async fn a_401_refreshes_through_the_relay_then_retries() {
    let (base, seen) = fake_api(true).await;
    let kick = api(&base, seen.clone());
    assert_eq!(kick.me().await.unwrap().id, "42");
    assert_eq!(seen.lock().unwrap().refreshed, 1);
}

#[tokio::test]
async fn requests_have_the_shapes_kick_documents() {
    let (base, seen) = fake_api(false).await;
    let kick = api(&base, seen.clone());
    kick.send_chat("42", "hello", Some("msg-1")).await.unwrap();
    kick.ban("42", "7", Some(90), Some("spam")).await.unwrap();
    kick.ban("42", "7", None, None).await.unwrap();
    kick.subscribe_events("42").await.unwrap();
    let b = seen.lock().unwrap().bodies.clone();
    assert_eq!(
        b[0],
        (
            "POST /public/v1/chat".into(),
            json!({ "type": "user", "content": "hello", "broadcaster_user_id": 42, "reply_to_message_id": "msg-1" })
        )
    );
    assert_eq!(b[1].1["duration"], 2, "90 seconds rounds up to 2 minutes");
    assert!(b[2].1.get("duration").is_none(), "no duration means a ban");
    assert_eq!(b[3].1["events"].as_array().unwrap().len(), EVENTS.len());
    assert!(kick.ban("42", "not-a-number", None, None).await.is_err());
    assert!(kick.delete_message("../../admin").await.is_ok(), "path characters are stripped, not followed");
    assert_eq!(seen.lock().unwrap().bodies.last().unwrap().0, "DELETE /public/v1/chat/admin");
    assert!(kick.delete_message("///").await.is_err());
}

// ---------------------------------------------------------------- stream info (Studio)

/// A fake Kick channel API: categories "Valorant" (12) and "Just Chatting" (15); PATCH
/// /channels answers 403 when `forbid` is set (an old sign-in without `channel:write`).
async fn fake_channels(forbid: bool) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let handler = move |method: axum::http::Method, uri: axum::http::Uri, body: String| {
        let s = s.clone();
        async move {
            let path = uri.path().to_string();
            if path == "/public/v1/categories" {
                let q = uri.query().unwrap_or("").to_lowercase();
                let all = [(12, "Valorant"), (15, "Just Chatting")];
                let data: Vec<Value> = all
                    .iter()
                    .filter(|(_, n)| {
                        q.contains(&n.to_lowercase().replace(' ', "+")) || q.contains(&n.to_lowercase()[..3])
                    })
                    .map(|(id, n)| json!({ "id": id, "name": n, "thumbnail": "" }))
                    .collect();
                return (axum::http::StatusCode::OK, axum::Json(json!({ "data": data })));
            }
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            s.lock().unwrap().bodies.push((format!("{method} {path}"), v));
            if forbid {
                return (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({ "message": "Forbidden" })));
            }
            (axum::http::StatusCode::NO_CONTENT, axum::Json(json!({})))
        }
    };
    let app = axum::Router::new().fallback(handler);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, seen)
}

#[tokio::test]
async fn stream_info_patches_the_channel_and_finds_the_category() {
    use ferrocast_core::stream_info::{Category, CategoryIds, StreamInfo};
    let (base, seen) = fake_channels(false).await;
    let kick = api(&base, seen.clone());
    let info = StreamInfo {
        title: Some("Ranked".into()),
        category: Some(Category { name: "valorant".into(), ids: CategoryIds::default() }),
        tags: Some(vec!["English".into()]),
        language: Some("en".into()),
    };
    assert_eq!(kick.update_channel(&info).await.unwrap(), vec!["title", "category", "tags"], "no language on Kick");
    let b = seen.lock().unwrap().bodies.clone();
    assert_eq!(
        b[0],
        (
            "PATCH /public/v1/channels".into(),
            json!({ "stream_title": "Ranked", "category_id": 12, "custom_tags": ["English"] })
        )
    );
    let hits = kick.search_categories("just").await.unwrap();
    assert_eq!(hits[0].id, "15");
    assert!(hits[0].image_url.is_none(), "empty thumbnails are left out");
    let unknown = StreamInfo {
        category: Some(Category { name: "Nope".into(), ids: CategoryIds::default() }),
        ..Default::default()
    };
    assert!(kick.update_channel(&unknown).await.unwrap_err().to_string().contains("no category called"));
    let bad_id = StreamInfo {
        category: Some(Category {
            name: "x".into(),
            ids: CategoryIds { kick: Some("abc".into()), ..Default::default() },
        }),
        ..Default::default()
    };
    assert!(kick.update_channel(&bad_id).await.is_err());
    let only_language = StreamInfo { language: Some("en".into()), ..Default::default() };
    assert!(kick.update_channel(&only_language).await.unwrap().is_empty(), "nothing Kick takes: no request");
}

#[tokio::test]
async fn a_403_on_the_channel_means_reconnect_for_channel_write() {
    use ferrocast_connector::ConnectorError;
    use ferrocast_core::stream_info::StreamInfo;
    let (base, seen) = fake_channels(true).await;
    let kick = api(&base, seen);
    let err = kick.update_channel(&StreamInfo { title: Some("x".into()), ..Default::default() }).await.unwrap_err();
    assert_eq!(err, KickError::MissingScope("channel:write".into()));
    assert!(
        matches!(ConnectorError::from(err), ConnectorError::MissingScope { scope, .. } if scope == "channel:write")
    );
    assert!(SCOPES.contains(&"channel:write"));
}
