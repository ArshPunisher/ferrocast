//! Stream history: counting (pure), then uploads against a fake relay that can be down,
//! slow to accept, or refuse.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Origin, Platform};
use ferrocast_core::sample::sample;
use serde_json::{Value, json};

use super::*;
use crate::{Engine, PlatformApps};

const MIN: i64 = 60_000;
const T0: i64 = 1_792_411_200_000;

fn live(platform: Platform, at: i64, kind: EventKind) -> Event {
    let mut e = Event::live(platform, Some(format!("e{at}{}", rand_id())), Millis(at), kind);
    e.origin = Origin::Live;
    e
}

fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn sampled(kind: &str, platform: Platform, at: i64) -> Event {
    let mut e = sample(kind, platform, Millis(at)).unwrap();
    e.origin = Origin::Live;
    e.source_id = Some(format!("s{}", rand_id()));
    e
}

fn chat(platform: Platform, at: i64, user: &str) -> Event {
    let mut a = Actor::named(platform, user);
    a.id = Some(format!("{user}-id"));
    live(
        platform,
        at,
        EventKind::ChatMessage { actor: a, content: Content::plain("hi"), reply_to: None, is_action: false },
    )
}

fn on() -> History {
    History { on: true, ..Default::default() }
}

fn bodies(h: &History) -> Vec<Value> {
    h.saved.queue.iter().map(Upload::body).collect()
}

// ── Counting ─────────────────────────────────────────────────────────────────────────

#[test]
fn a_stream_is_counted_per_platform_and_queued_at_start_and_end() {
    let mut h = on();
    let online = live(Platform::Twitch, T0, EventKind::StreamOnline { title: Some("Ranked grind".into()) });
    assert!(h.saw(&online, None, Some("VALORANT".into())), "the start is queued");
    assert!(!h.saw(&online, None, None), "a second online for the same platform changes nothing");
    h.saw(&live(Platform::Kick, T0 + MIN, EventKind::StreamOnline { title: None }), None, None);
    for (i, n) in [100, 200, 150].into_iter().enumerate() {
        h.saw(&live(Platform::Twitch, T0 + i as i64 * MIN, EventKind::ViewerCount { count: n }), None, None);
    }
    h.saw(&chat(Platform::Twitch, T0 + MIN, "ana"), Some(&ChatterMark::First), None);
    h.saw(&chat(Platform::Twitch, T0 + 2 * MIN, "ana"), None, None);
    h.saw(&chat(Platform::Twitch, T0 + 3 * MIN, "bo"), Some(&ChatterMark::Returning { days: 40 }), None);
    h.saw(&chat(Platform::Kick, T0 + 3 * MIN, "cy"), Some(&ChatterMark::First), None);
    h.saw(&sampled("follow", Platform::Twitch, T0 + 4 * MIN), None, None);
    h.saw(&sampled("subscription", Platform::Twitch, T0 + 4 * MIN), None, None);
    h.saw(&sampled("gift_batch", Platform::Twitch, T0 + 4 * MIN), None, None);
    h.saw(&sampled("platform_gift", Platform::Twitch, T0 + 4 * MIN), None, None);
    h.saw(&sampled("platform_gift", Platform::Twitch, T0 + 5 * MIN), None, None);
    // A tip-page tip goes to the stream that started first (Twitch here).
    h.saw(&sampled("tip", Platform::Twitch, T0 + 5 * MIN), None, None);
    h.saw(&sampled("tip", Platform::Twitch, T0 + 6 * MIN), None, None);
    // Test alerts never count.
    h.saw(&sample("follow", Platform::Twitch, Millis(T0)).unwrap(), None, None);
    assert!(h.saw(&live(Platform::Twitch, T0 + 60 * MIN, EventKind::StreamOffline), None, None));

    let b = bodies(&h);
    assert_eq!(b.len(), 4, "twitch start, kick start, twitch's last count, twitch end: {b:#?}");
    assert_eq!(b[0]["key"], format!("twitch:{T0}"));
    assert_eq!((b[0]["title"].as_str(), b[0]["category"].as_str()), (Some("Ranked grind"), Some("VALORANT")));
    assert_eq!(b[0]["ended_at"], Value::Null);
    assert_eq!(b[1]["platform"], "kick");
    assert_eq!(b[2]["samples"], json!([{ "at": T0 + 60 * MIN, "viewers": 150 }]));
    let end = &b[3];
    assert_eq!(end["ended_at"].as_i64(), Some(T0 + 60 * MIN));
    assert_eq!((end["peak_viewers"].as_u64(), end["avg_viewers"].as_u64()), (Some(200), Some(150)));
    assert_eq!(
        (end["chat_lines"].as_u64(), end["chatters"].as_u64(), end["new_chatters"].as_u64()),
        (Some(3), Some(2), Some(1))
    );
    assert_eq!((end["follows"].as_u64(), end["subs"].as_u64()), (Some(1), Some(6)));
    assert_eq!(end["tips"], json!([{ "currency": "INR", "minor": 100000 }]));
    assert_eq!(end["platform_value"], json!([{ "unit": "bits", "amount": 1000 }]));
    // Kick is still being counted, with its own chatter.
    assert_eq!(h.saved.tracks["kick"].chatters.len(), 1);
    assert!(!h.saved.tracks.contains_key("twitch"));
}

#[test]
fn nothing_is_counted_without_pro() {
    let mut h = History::default();
    assert!(!h.saw(&live(Platform::Twitch, T0, EventKind::StreamOnline { title: None }), None, None));
    h.saw(&live(Platform::Twitch, T0, EventKind::ViewerCount { count: 5 }), None, None);
    assert!(h.saved.queue.is_empty() && h.saved.tracks.is_empty());
    h.tick(T0 + 60 * MIN, &["twitch"]);
    assert!(h.saved.queue.is_empty());
}

#[test]
fn samples_every_five_minutes_join_into_one_upload_while_offline() {
    let mut h = on();
    h.saw(&live(Platform::Youtube, T0, EventKind::StreamOnline { title: None }), None, None);
    h.saw(&live(Platform::Youtube, T0, EventKind::ViewerCount { count: 42 }), None, None);
    h.tick(T0 + 4 * MIN, &["youtube"]);
    assert_eq!(h.saved.queue.len(), 1, "not five minutes yet");
    for i in 1..=120 {
        h.tick(T0 + i * SAMPLE_EVERY_MS, &["youtube"]);
    }
    // The start, then 120 samples in uploads of at most 100.
    assert_eq!(h.saved.queue.len(), 3);
    let Upload::Samples { samples, .. } = &h.saved.queue[1] else { panic!("samples") };
    assert_eq!(samples.len(), 100);
    assert_eq!(samples[0], (T0 + SAMPLE_EVERY_MS, 42));
    // A platform that isn't live gets no samples.
    h.tick(T0 + 200 * SAMPLE_EVERY_MS, &[]);
    let n: usize =
        h.saved.queue.iter().map(|u| if let Upload::Samples { samples, .. } = u { samples.len() } else { 0 }).sum();
    assert!(n <= 121, "{n}");
}

#[test]
fn a_stream_from_before_a_restart_ends_where_it_was_last_seen() {
    let mut h = on();
    h.saw(&live(Platform::Kick, T0, EventKind::StreamOnline { title: None }), None, None);
    h.saw(&chat(Platform::Kick, T0 + 30 * MIN, "ana"), None, None);
    h.saved.queue.clear();
    // Ferrocast restarts two hours later; Kick never says it's live again.
    h.started = T0 + 120 * MIN;
    h.tick(T0 + 125 * MIN, &[]);
    assert!(h.saved.tracks.contains_key("kick"), "it might still come back");
    h.tick(T0 + 131 * MIN, &[]);
    assert!(h.saved.tracks.is_empty());
    let b = bodies(&h);
    assert_eq!(b.last().unwrap()["ended_at"].as_i64(), Some(T0 + 30 * MIN));
}

#[test]
fn going_live_soon_after_a_restart_starts_a_new_stream() {
    let mut h = on();
    h.saw(&live(Platform::Twitch, T0, EventKind::StreamOnline { title: None }), None, None);
    h.saw(&chat(Platform::Twitch, T0 + 20 * MIN, "ana"), None, None);
    h.saved.queue.clear();
    // Ferrocast closed mid-stream; the next day it opens and the streamer goes live at once,
    // before the restored stream's grace has run out.
    let next_day = T0 + 24 * 60 * MIN;
    h.started = next_day;
    h.saw(&live(Platform::Twitch, next_day + MIN, EventKind::StreamOnline { title: None }), None, None);
    let b = bodies(&h);
    assert_eq!(b[0]["ended_at"].as_i64(), Some(T0 + 20 * MIN), "yesterday's stream ends where it was last seen");
    assert_eq!(b[1]["started_at"].as_i64(), Some(next_day + MIN), "today's is its own stream");
    assert_ne!(b[0]["key"], b[1]["key"]);
    // Said twice within moments: still one stream.
    h.saw(&live(Platform::Twitch, next_day + 2 * MIN, EventKind::StreamOnline { title: None }), None, None);
    assert_eq!(bodies(&h).len(), 2);
}

#[test]
fn the_queue_is_capped_and_samples_go_first() {
    let mut s = Saved::default();
    s.push(Upload::Stream { body: json!({ "key": "first" }) });
    for i in 0..(MAX_QUEUE as i64 + 10) {
        // Different keys, so they don't join.
        s.push(Upload::Samples { platform: "twitch".into(), key: format!("k{i}"), samples: vec![(i, 1)] });
    }
    assert_eq!(s.queue.len(), MAX_QUEUE);
    assert_eq!(s.queue[0], Upload::Stream { body: json!({ "key": "first" }) }, "streams are kept over samples");
}

// ── Uploads against a fake relay ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Fake {
    /// The status every history call gets (200 by default).
    answer: Option<u16>,
    calls: Vec<(String, Value)>,
}

type Shared = Arc<Mutex<Fake>>;

async fn fake_relay() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn installs() -> axum::Json<Value> {
        axum::Json(json!({ "install_id": "i1", "install_secret": "s1" }))
    }
    async fn history(
        State(f): State<Shared>,
        uri: axum::http::Uri,
        axum::Json(body): axum::Json<Value>,
    ) -> (StatusCode, axum::Json<Value>) {
        let mut f = f.lock().unwrap();
        f.calls.push((uri.path().to_string(), body));
        let status = StatusCode::from_u16(f.answer.unwrap_or(200)).unwrap();
        (status, axum::Json(json!({ "error": { "code": "x", "message": "x" } })))
    }
    let app = axum::Router::new()
        .route("/v1/installs", post(installs))
        .route("/v1/pro/history/streams", post(history))
        .route("/v1/pro/history/samples", post(history))
        .with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, shared)
}

async fn engine(relay: &str) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(relay.into()), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    (e, dir)
}

/// Pro as the relay last confirmed it, with or without history on that relay.
async fn remember_pro(e: &Engine, history: bool) {
    let now = now().0;
    let status = json!({
        "pro": true, "status": "active", "plan": "monthly", "subscription_id": "sub_1",
        "current_end": now + 30 * 86_400_000, "access_until": now + 33 * 86_400_000, "checked_at": now,
        "features": { "history": history }, "quotas": {}
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
}

async fn stream(e: &Engine) {
    let at = now().0;
    e.ingest(live(Platform::Twitch, at - 30 * MIN, EventKind::StreamOnline { title: Some("Chill".into()) })).await;
    e.ingest(live(Platform::Twitch, at - 20 * MIN, EventKind::ViewerCount { count: 33 })).await;
    e.ingest(chat(Platform::Twitch, at - 10 * MIN, "ana")).await;
    e.ingest(live(Platform::Twitch, at - MIN, EventKind::StreamOffline)).await;
}

#[tokio::test]
async fn without_pro_nothing_is_counted_or_sent() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    stream(&e).await;
    e.history_round().await;
    assert!(e.history().saved.queue.is_empty());
    // Pro, but an older relay without history: still nothing.
    remember_pro(&e, false).await;
    e.history_round().await;
    stream(&e).await;
    e.history_round().await;
    assert!(fake.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn a_stream_reaches_the_relay_and_waits_while_its_down() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    e.history_round().await; // picks up Pro
    fake.lock().unwrap().answer = Some(503);
    stream(&e).await;
    e.history_round().await;
    let calls = fake.lock().unwrap().calls.len();
    assert_eq!(calls, 1, "the first upload failed, so the rest waited");
    assert_eq!(e.history().saved.queue.len(), 3, "start, last count, end");
    assert!(e.history().retry_at > now().0, "backing off");
    // It's in the store: a restart (here: forgetting the memory and loading again) keeps it.
    let saved = e.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.unwrap().unwrap();
    assert_eq!(saved.queue.len(), 3);
    e.history().saved = Saved::default();
    e.load_history().await;
    assert_eq!(e.history().saved.queue.len(), 3);
    // Back up: everything goes, in order.
    fake.lock().unwrap().answer = None;
    e.history().retry_at = 0;
    e.history_round().await;
    let calls = fake.lock().unwrap().calls.clone();
    let paths: Vec<&str> = calls.iter().skip(1).map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["/v1/pro/history/streams", "/v1/pro/history/samples", "/v1/pro/history/streams"]);
    let end = &calls.last().unwrap().1;
    assert_eq!(
        (end["title"].as_str(), end["peak_viewers"].as_u64(), end["chat_lines"].as_u64()),
        (Some("Chill"), Some(33), Some(1))
    );
    assert!(end["ended_at"].is_i64());
    assert!(e.history().saved.queue.is_empty());
    let saved = e.db(|s| repo::settings::get::<Saved>(s, SAVE_KEY)).await.unwrap().unwrap();
    assert!(saved.queue.is_empty(), "the store is emptied too");
}

#[tokio::test]
async fn refusals_that_retrying_cant_fix_are_dropped_and_402_waits() {
    let (relay, fake) = fake_relay().await;
    let (e, _d) = engine(&relay).await;
    remember_pro(&e, true).await;
    e.history_round().await;
    // Pro lapsed on the relay before the app noticed: keep everything for later.
    fake.lock().unwrap().answer = Some(402);
    stream(&e).await;
    e.history_round().await;
    assert_eq!(e.history().saved.queue.len(), 3);
    // A locked stream (409) or bad data (422) can never go: dropped, the queue moves on.
    fake.lock().unwrap().answer = Some(409);
    e.history().retry_at = 0;
    e.history_round().await;
    assert!(e.history().saved.queue.is_empty());
    // Not reachable at all: kept.
    let (e2, _d2) = engine("http://127.0.0.1:1").await;
    remember_pro(&e2, true).await;
    e2.history_round().await;
    stream(&e2).await;
    e2.history_round().await;
    assert_eq!(e2.history().saved.queue.len(), 3);
}

/// No Pro, as the relay last said, on a relay that keeps history.
async fn remember_free(e: &Engine) {
    let status = json!({
        "pro": false, "status": "none", "plan": null, "subscription_id": null, "current_end": null,
        "access_until": null, "checked_at": now().0, "features": { "history": true }, "quotas": {}
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
}

#[tokio::test]
async fn a_free_pc_signed_in_to_an_account_uploads_too() {
    let (relay, fake) = fake_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    let apps = PlatformApps { relay_url: Some(relay.clone()), ..Default::default() };
    let e = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
    remember_free(&e).await;
    // Free and signed out: nothing.
    e.history_round().await;
    stream(&e).await;
    e.history_round().await;
    assert!(fake.lock().unwrap().calls.is_empty());
    // Signed in to a free account: counted and sent (the relay keeps 30 days).
    ferrocast_secrets::Secrets::set(&*secrets, crate::ferro_account::SESSION, "fca_x.y").unwrap();
    assert!(e.history_allowed().await);
    e.history_round().await;
    stream(&e).await;
    e.history_round().await;
    let paths: Vec<String> = fake.lock().unwrap().calls.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(paths.first().map(String::as_str), Some("/v1/pro/history/streams"), "{paths:?}");
    assert!(e.history().saved.queue.is_empty());
}
