//! Polls and giveaways in cloud mode (docs/CLOUD.md §7.2) through the real router: the
//! engine's checks, `!vote` and keyword entry from live chat (one per person), the results on
//! the dashboard's socket and in overlays with the widget, the timer, draws without repeats,
//! chat announcements through the cloud bot, and the bus kept while one runs.

use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use super::tests::{User, World, world};
use crate::bot::tests::STREAMER_ID;

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

impl World {
    async fn say(&self, n: u32, who: &str, text: &str) {
        let chat = json!({
            "broadcaster_user_id": STREAMER_ID, "chatter_user_id": who, "chatter_user_login": format!("v{who}"),
            "chatter_user_name": format!("V{who}"), "message_id": format!("cm-{n}"),
            "message": { "text": text, "fragments": [] }, "badges": [], "color": "", "message_type": "text"
        });
        assert_eq!(self.twitch_event(&format!("ev-{n}"), "channel.chat.message", chat).await, 204);
    }

    async fn until(&self, u: &User, path: &str, ok: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..80 {
            let (_, v) = self.web(u, Method::GET, path, None).await;
            if ok(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{path} never got there");
    }
}

#[tokio::test]
async fn a_chat_poll_counts_one_vote_per_person_and_closes_once() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    w.bind_channels(&u);
    let (_, none) = w.web(&u, Method::GET, "/c/api/v1/polls/current", None).await;
    assert_eq!(none, Value::Null);

    let (s, v) =
        w.web(&u, Method::POST, "/c/api/v1/polls", Some(json!({ "question": "Next?", "options": ["A"] }))).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().starts_with("options:"));
    let spec = json!({ "question": "Which map next?", "options": ["Dust", "Mirage", "Inferno"], "duration_s": 60 });
    let (s, p) = w.web(&u, Method::POST, "/c/api/v1/polls", Some(spec.clone())).await;
    assert_eq!(s, 201, "{p}");
    assert_eq!(p["options"].as_array().unwrap().len(), 3);
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/polls", Some(spec)).await;
    assert_eq!((s, code(&v)), (409, "conflict"));

    // Votes from live chat: one per person (a change of mind moves it), bad numbers ignored.
    w.say(1, "11", "!vote 2").await;
    w.say(2, "12", "!vote 2").await;
    w.say(3, "11", "!vote 3").await;
    w.say(4, "13", "!vote 9").await;
    let cur = w.until(&u, "/c/api/v1/polls/current", |v| v["total"] == 2 && v["options"][2]["votes"] == 1).await;
    assert_eq!(cur["options"][1]["votes"], 1, "{cur}");

    // The bus stays while the poll runs, even with nothing open.
    assert_eq!(w.state.cloud.buses.reap(crate::now_ms() + 3_600_000), 0);

    let (s, end) = w.web(&u, Method::POST, "/c/api/v1/polls/current/end", None).await;
    assert_eq!((s, end["closed"].as_bool()), (200, Some(true)), "{end}");
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/polls/current/end", None).await;
    assert_eq!((s, code(&v)), (409, "conflict"));
    w.say(5, "14", "!vote 1").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, after) = w.web(&u, Method::GET, "/c/api/v1/polls/current", None).await;
    assert_eq!(after["total"], 2, "a vote after the end doesn't count");

    let (s, _) = w.web(&u, Method::DELETE, "/c/api/v1/polls/current", None).await;
    assert_eq!(s, 204);
    let (s, v) = w.web(&u, Method::DELETE, "/c/api/v1/polls/current", None).await;
    assert_eq!((s, code(&v)), (404, "poll_not_found"));
    assert!(v["error"]["hint"].is_string());
}

#[tokio::test]
async fn a_poll_past_its_end_takes_no_votes_and_the_dashboard_hears_it() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    let spec = json!({ "question": "Snack?", "options": ["Chips", "Fruit"], "duration_s": 10 });
    let (s, _) = w.web(&u, Method::POST, "/c/api/v1/polls", Some(spec)).await;
    assert_eq!(s, 201);
    // Its time ran out (the timer that closes it is minutes away in real polls).
    let bus = w.state.cloud.buses.get(&u.account.id).unwrap();
    bus.state().poll.as_mut().unwrap().ends_at = ferrocast_core::event::Millis(crate::now_ms());
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/polls/current", None).await;
    assert_eq!(v["question"], "Snack?");
    let mut frames = bus.subscribe();
    super::community::on_event(
        &w.state,
        &bus,
        &ferrocast_core::event::Event::live(
            ferrocast_core::event::Platform::Twitch,
            None,
            ferrocast_core::event::Millis(crate::now_ms()),
            ferrocast_core::event::EventKind::ChatMessage {
                actor: {
                    let mut a = ferrocast_core::event::Actor::named(ferrocast_core::event::Platform::Twitch, "Late");
                    a.id = Some("99".into());
                    a
                },
                content: ferrocast_core::event::Content::plain("!vote 1"),
                reply_to: None,
                is_action: false,
            },
        ),
    );
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/polls/current", None).await;
    assert_eq!(v["total"], 0, "after its end time nothing counts");
    // Ending it now pushes `poll` to the dashboard and `state` to poll widgets.
    w.web(&u, Method::POST, "/c/api/v1/polls/current/end", None).await;
    let mut kinds = Vec::new();
    while let Ok(Ok(f)) = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await {
        let v: Value = serde_json::from_str(&f.text).unwrap();
        kinds.push((f.audience.clone(), v["type"].as_str().unwrap_or("").to_string()));
    }
    assert!(kinds.contains(&(super::bus::Audience::Dashboard, "poll".into())), "{kinds:?}");
    assert!(kinds.contains(&(super::bus::Audience::Widget(ferrocast_core::overlay::WidgetKind::Poll), "state".into())));
}

#[tokio::test]
async fn giveaways_take_keyword_entries_once_and_draw_without_repeats() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    w.bind_channels(&u);
    let (s, v) =
        w.web(&u, Method::POST, "/c/api/v1/giveaways", Some(json!({ "prize": "A game key", "cost": 100 }))).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().contains("app"));
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/draw", None).await;
    assert_eq!((s, code(&v)), (404, "giveaway_not_found"));

    let (s, g) = w.web(&u, Method::POST, "/c/api/v1/giveaways", Some(json!({ "prize": "A game key" }))).await;
    assert_eq!((s, g["keyword"].as_str()), (201, Some("!join")), "{g}");
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways", Some(json!({ "prize": "Another" }))).await;
    assert_eq!((s, code(&v)), (409, "conflict"));
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/draw", None).await;
    assert_eq!((s, code(&v)), (422, "nobody_entered"));
    assert!(v["error"]["hint"].as_str().unwrap().contains("!join"));

    w.say(1, "21", "!join").await;
    w.say(2, "22", "!JOIN please").await;
    w.say(3, "21", "!join").await;
    w.say(4, "23", "join").await;
    let cur = w.until(&u, "/c/api/v1/giveaways/current", |v| v["entries"] == 2).await;
    assert_eq!(cur["entrants"].as_array().unwrap().len(), 2, "{cur}");

    let (s, d) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/draw", None).await;
    assert_eq!((s, d["winners"].as_array().unwrap().len()), (200, 1), "{d}");
    assert_eq!(d["open"], false, "drawing closes entries");
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/draw", None).await;
    assert_eq!((s, code(&v)), (409, "conflict"));
    let (s, r) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/redraw", None).await;
    assert_eq!(s, 200);
    let winners: Vec<&str> = r["winners"].as_array().unwrap().iter().map(|e| e["user_id"].as_str().unwrap()).collect();
    assert_eq!(winners.len(), 2);
    assert_ne!(winners[0], winners[1], "never the same winner twice");
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/redraw", None).await;
    assert_eq!((s, code(&v)), (422, "everyone_won"));
    let (s, e) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/end", None).await;
    assert_eq!((s, e["ended"].as_bool()), (200, Some(true)));
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/giveaways/current/end", None).await;
    assert_eq!((s, code(&v)), (409, "conflict"));
    let (s, v) = w.web(&u, Method::DELETE, "/c/api/v1/giveaways/current", None).await;
    assert_eq!((s, v["refunded"].as_i64()), (200, Some(0)));
}

#[tokio::test]
async fn the_cloud_bot_posts_the_giveaway_in_chat() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    w.connect_twitch_for(&u, "c1").await;
    let (s, _) = w.web(&u, Method::PUT, "/c/api/v1/pro/bot/enabled", Some(json!({ "enabled": true }))).await;
    assert_eq!(s, 200);
    crate::bot::tests::cloud_takes_over_all(&w.state, &u.account.id);
    let (s, _) = w.web(&u, Method::POST, "/c/api/v1/giveaways", Some(json!({ "prize": "A mouse" }))).await;
    assert_eq!(s, 201);
    let mut sent = Vec::new();
    for _ in 0..50 {
        sent = w.tw.with(|t| t.sent.iter().map(|s| s.2.clone()).collect::<Vec<_>>());
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sent, ["Giveaway: A mouse! Type !join to enter."]);
    // Long text is split for chat.
    let parts = crate::bot::split_chat(&"word ".repeat(150), 500);
    assert!(parts.len() == 2 && parts.iter().all(|p| p.chars().count() <= 500), "{parts:?}");
}
