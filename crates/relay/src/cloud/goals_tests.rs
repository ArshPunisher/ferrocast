//! Goals in cloud mode (docs/CLOUD.md §7.1) through the real router: the engine's checks,
//! live events moving them (tests never), progress kept across edits and a restart of the
//! bus, adjust by hand, overlays and the dashboard told, the cap, and the desktop's sync
//! never seeing them.

use reqwest::Method;
use serde_json::{Value, json};

use super::live_tests::{next, next_of};
use super::tests::world;

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn goals_count_live_follows_keep_progress_and_reach_overlays() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    w.bind_channels(&u);

    let (s, g) = w
        .web(
            &u,
            Method::POST,
            "/c/api/v1/goals",
            Some(json!({ "title": "100 followers", "kind": "follows", "target": 100, "currency": null,
                         "starts_at": 0, "ends_at": null })),
        )
        .await;
    assert_eq!(s, 201, "{g}");
    assert_eq!((g["current"].as_f64(), g["target"].as_f64()), (Some(0.0), Some(100.0)));
    assert!(g["starts_at"].as_i64().unwrap() > 0, "starts now when not given");
    let id = g["id"].as_str().unwrap().to_string();

    let (_, token) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token, "").await;
    let snap = next(&mut ws).await;
    assert_eq!(snap["data"]["state"]["goals"][0]["id"], id.as_str(), "{snap}");

    // A live follow moves it; the page hears it.
    let follow =
        json!({ "user_id": "9", "user_login": "nova", "user_name": "Nova", "followed_at": "2026-10-21T10:00:00Z" });
    assert_eq!(w.twitch_event("g-1", "channel.follow", follow).await, 204);
    let state = loop {
        let f = next_of(&mut ws, "state").await;
        if f["data"].get("goals").is_some() {
            break f;
        }
    };
    assert_eq!(state["data"]["goals"][0]["current"], 1.0, "{state}");

    // A test follow never counts.
    let (s, _) = w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "follow" }))).await;
    assert_eq!(s, 202);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // An edit keeps the progress (it's never taken from the body).
    let (s, g) = w
        .web(
            &u,
            Method::PUT,
            &format!("/c/api/v1/goals/{id}"),
            Some(json!({ "title": "New mic followers", "kind": "follows", "target": 120, "current": 99,
                         "currency": null, "starts_at": 0, "ends_at": null })),
        )
        .await;
    assert_eq!(s, 200, "{g}");
    assert_eq!((g["current"].as_f64(), g["title"].as_str()), (Some(1.0), Some("New mic followers")));
    // With progress, its kind can't change.
    let (s, v) = w
        .web(
            &u,
            Method::PUT,
            &format!("/c/api/v1/goals/{id}"),
            Some(json!({ "title": "x", "kind": "subs", "target": 10, "currency": null, "starts_at": 0, "ends_at": null })),
        )
        .await;
    assert_eq!((s, code(&v)), (409, "conflict"), "{v}");
    assert!(v["error"]["hint"].is_string());

    // By hand.
    let (s, g) = w.web(&u, Method::POST, &format!("/c/api/v1/goals/{id}/adjust"), Some(json!({ "delta": 4 }))).await;
    assert_eq!((s, g["current"].as_f64()), (200, Some(5.0)), "{g}");
    let (s, v) = w.web(&u, Method::POST, &format!("/c/api/v1/goals/{id}/adjust"), Some(json!({ "delta": 0.5 }))).await;
    assert_eq!((s, code(&v)), (422, "invalid"), "{v}");

    // The bus forgets (dropped, or a restart): progress is in the document.
    w.state.cloud.buses.drop_bus(&u.account.id);
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/goals", None).await;
    assert_eq!(list[0]["current"], 5.0);

    // The desktop's sync never sees goals.
    let (s, pulled) = w.app(&u, Method::GET, "/v1/cloud/docs?since=0", None).await;
    assert_eq!(s, 200);
    assert!(pulled["docs"].as_array().unwrap().iter().all(|d| d["kind"] != "goal"), "{pulled}");
    let (s, v) = w
        .app(
            &u,
            Method::PUT,
            "/v1/cloud/docs",
            Some(json!({ "docs": [{ "kind": "goal", "id": "x", "base_version": 0, "body": {} }] })),
        )
        .await;
    assert_eq!((s, code(&v)), (422, "invalid"));

    // Deleted: gone, and a second delete is a 404.
    let (s, _) = w.web(&u, Method::DELETE, &format!("/c/api/v1/goals/{id}"), None).await;
    assert_eq!(s, 204);
    let (s, v) = w.web(&u, Method::DELETE, &format!("/c/api/v1/goals/{id}"), None).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/goals", None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn tips_goals_count_their_currency_and_bad_goals_name_the_field() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    let new = |v: Value| async { w.web(&u, Method::POST, "/c/api/v1/goals", Some(v)).await };
    let (s, v) = new(json!({ "title": "Mic", "kind": "tips", "target": 5000, "currency": null })).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    assert!(v["error"]["message"].as_str().unwrap().contains("currency"));
    let (s, v) = new(json!({ "title": "", "kind": "follows", "target": 10, "currency": null })).await;
    assert_eq!((s, code(&v)), (422, "invalid"));
    let (s, v) = new(json!({ "title": "Subs", "kind": "subs", "target": 2.5, "currency": null })).await;
    assert_eq!((s, code(&v)), (422, "invalid"), "{v}");
    let (s, g) = new(json!({ "title": "Mic", "kind": "tips", "target": 5000, "currency": "inr" })).await;
    assert_eq!(s, 201, "{g}");
    assert_eq!((g["currency"].as_str(), g["target"].as_f64()), (Some("INR"), Some(5000.0)));
    let id = g["id"].as_str().unwrap();

    // A tip in INR counts in rupees; one in USD doesn't; a test never does.
    let bus = w.state.cloud.buses.open(&u.account.id, 10).unwrap();
    let cached = super::ingest::cached(&w.state, &bus).unwrap();
    for (cur, minor, src) in [("INR", 34_550, "t1"), ("USD", 500, "t2")] {
        let p = json!({ "provider": "razorpay", "payment_id": src, "status": "captured", "currency": cur,
                        "amount_minor": minor, "digits": 2, "name": "Meera", "message": "" });
        let e = super::ingest::tip_event(&p).unwrap();
        let ev = super::ingest::process(&bus, &cached, e).unwrap();
        super::goals::on_event(&w.state, &bus, &ev);
    }
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/goals", None).await;
    assert_eq!(list[0]["current"], 345.5, "{list}");
    let (s, g) =
        w.web(&u, Method::POST, &format!("/c/api/v1/goals/{id}/adjust"), Some(json!({ "delta": -45.5 }))).await;
    assert_eq!((s, g["current"].as_f64()), (200, Some(300.0)));

    // The cap: 20 goals.
    for i in 1..super::goals::MAX_GOALS {
        let (s, _) = new(json!({ "title": format!("G{i}"), "kind": "custom", "target": 3, "currency": null })).await;
        assert_eq!(s, 201);
    }
    let (s, v) = new(json!({ "title": "One more", "kind": "custom", "target": 3, "currency": null })).await;
    assert_eq!((s, code(&v)), (409, "cloud_limit"));
    let (_, sum) = w.web(&u, Method::GET, "/c/v1/cloud", None).await;
    assert_eq!(sum["usage"]["goals"], super::goals::MAX_GOALS);

    let (s, v) = w
        .web(
            &u,
            Method::PUT,
            "/c/api/v1/goals/nope",
            Some(json!({ "title": "x", "kind": "custom", "target": 1, "currency": null })),
        )
        .await;
    assert_eq!((s, code(&v)), (404, "not_found"));
}

#[tokio::test]
async fn an_event_counts_by_the_stored_goal_not_a_stale_copy() {
    let w = world().await;
    let u = w.cloud_user("asha@example.com").await;
    w.bind_channels(&u);
    let (_, g) = w
        .web(
            &u,
            Method::POST,
            "/c/api/v1/goals",
            Some(json!({ "title": "F", "kind": "follows", "target": 10, "currency": null })),
        )
        .await;
    let id = g["id"].as_str().unwrap().to_string();
    let bus = w.state.cloud.buses.open(&u.account.id, 10).unwrap();
    super::goals::load(&w.state, &bus);
    let stale = bus.state().goals.clone();
    // Turned into a tips goal (no progress yet, so allowed), while the bus still has it as follows.
    let (s, _) = w
        .web(
            &u,
            Method::PUT,
            &format!("/c/api/v1/goals/{id}"),
            Some(json!({ "title": "F", "kind": "tips", "target": 10, "currency": "INR" })),
        )
        .await;
    assert_eq!(s, 200);
    bus.state().goals = stale;
    let cached = super::ingest::cached(&w.state, &bus).unwrap();
    let follow = ferrocast_core::sample::any_sample("follow", ferrocast_core::event::Millis(crate::now_ms())).unwrap();
    let mut follow = follow;
    follow.origin = ferrocast_core::event::Origin::Live;
    follow.source_id = Some("f-race".into());
    let ev = super::ingest::process(&bus, &cached, follow).unwrap();
    super::goals::on_event(&w.state, &bus, &ev);
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/goals", None).await;
    assert_eq!(list[0]["current"], 0.0, "a follow never counts toward a tips goal: {list}");
}
