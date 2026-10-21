//! Smart features through the real engine: the local first look (pure), the settings and
//! the approval of the reply facts, then chat in and suggestions, actions, undo, automatic
//! timeouts (never bans), replies and the recap against a fake relay and a fake platform.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use ferrocast_connector::Connector;
use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform, Roles};
use ferrocast_core::moderation::{Capabilities, ModeChange};
use serde_json::{Value, json};

use super::*;
use crate::PlatformApps;

// ── The local first look ─────────────────────────────────────────────────────────────

#[test]
fn the_first_look_catches_hinglish_abuse_links_and_numbers_but_not_banter() {
    for t in [
        "tu ch*tiya hai",
        "bh3nch0d",
        "BHENCHOD",
        "teri m.c",
        "mc kya kar raha",
        "r a n d i",
        "join discord.gg/abc",
        "call me 98765 43210",
        "WHY IS NOBODY TALKING HERE",
        "aaaaaaaaaaaa",
    ] {
        assert!(worth_checking(t, false), "{t}");
    }
    for t in ["bhai gg", "pagal hai kya lol", "nice clutch yaar", "kya scene hai", "mcdonalds later?", "ok"] {
        assert!(!worth_checking(t, false), "{t}");
    }
    assert!(worth_checking("hello", true), "a first message is always looked at");
}

#[test]
fn questions_are_spotted_in_english_and_hinglish() {
    for t in ["when do you stream?", "kab stream hoti hai", "kaise kiya ye", "what mouse do you use", "rig kya hai?"] {
        assert!(looks_like_question(t), "{t}");
    }
    for t in ["!uptime?", "gg", "lol nice", "what", "hi?"] {
        assert!(!looks_like_question(t), "{t}");
    }
}

// ── A fake relay and platform ────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Relay {
    moderate: Option<(u16, Value)>,
    reply: Option<(u16, Value)>,
    recap: Option<(u16, Value)>,
    calls: Vec<(String, Value)>,
}

type Shared = Arc<Mutex<Relay>>;

async fn fake_relay() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn installs() -> axum::Json<Value> {
        axum::Json(json!({ "install_id": "i1", "install_secret": "s1" }))
    }
    async fn smart(
        State(f): State<Shared>,
        uri: axum::http::Uri,
        axum::Json(body): axum::Json<Value>,
    ) -> (StatusCode, [(&'static str, &'static str); 1], axum::Json<Value>) {
        let mut f = f.lock().unwrap();
        f.calls.push((uri.path().to_string(), body.clone()));
        let answer = match uri.path() {
            "/v1/pro/smart/moderate" => f.moderate.clone(),
            "/v1/pro/smart/reply" => f.reply.clone(),
            _ => f.recap.clone(),
        };
        let (s, mut v) = answer.unwrap_or((500, json!({})));
        // `"$0"`, `"$1"`… stand for the asked messages' ids, in order.
        if let Some(rs) = v["results"].as_array_mut() {
            for r in rs {
                if let Some(i) =
                    r["id"].as_str().and_then(|x| x.strip_prefix('$')).and_then(|n| n.parse::<usize>().ok())
                {
                    r["id"] = body["messages"][i]["id"].clone();
                }
            }
        }
        (StatusCode::from_u16(s).unwrap(), [("retry-after", "120")], axum::Json(v))
    }
    let app = axum::Router::new()
        .route("/v1/installs", post(installs))
        .route("/v1/pro/smart/moderate", post(smart))
        .route("/v1/pro/smart/reply", post(smart))
        .route("/v1/pro/smart/recap", post(smart))
        .with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, shared)
}

#[derive(Debug, Default)]
struct Fake {
    said: Mutex<Vec<String>>,
    /// (what, user or message, seconds).
    did: Mutex<Vec<(String, String, u32)>>,
}

impl Fake {
    fn did(&self) -> Vec<(String, String, u32)> {
        self.did.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connector for Fake {
    fn platform(&self) -> Platform {
        Platform::Twitch
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: true, can_timeout: true, can_delete: true }
    }
    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
        self.said.lock().unwrap().push(text.to_string());
        Ok(())
    }
    async fn delete_message(&self, id: &str) -> ferrocast_connector::Result<()> {
        self.did.lock().unwrap().push(("delete".into(), id.into(), 0));
        Ok(())
    }
    async fn timeout(&self, user: &str, s: u32, _: Option<&str>) -> ferrocast_connector::Result<()> {
        self.did.lock().unwrap().push(("timeout".into(), user.into(), s));
        Ok(())
    }
    async fn ban(&self, user: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        self.did.lock().unwrap().push(("ban".into(), user.into(), 0));
        Ok(())
    }
    async fn unban(&self, user: &str) -> ferrocast_connector::Result<()> {
        self.did.lock().unwrap().push(("unban".into(), user.into(), 0));
        Ok(())
    }
    async fn set_mode(&self, _: ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
}

struct World {
    e: Engine,
    relay: Shared,
    tw: Arc<Fake>,
    _dir: tempfile::TempDir,
}

/// Pro with smart features (or not), as the relay last said.
async fn remember(e: &Engine, pro: bool, smart: bool) {
    let now = now().0;
    let status = json!({
        "pro": pro, "status": if pro { "active" } else { "none" }, "plan": "monthly", "subscription_id": "sub_1",
        "current_end": now + 30 * 86_400_000, "access_until": now + 33 * 86_400_000, "checked_at": now,
        "features": { "smart": smart, "history": true },
        "quotas": { "smart_replies": { "used": 1, "limit": 20, "period": "day", "reset_at": now + 1 } }
    });
    e.db(move |s| repo::settings::set(s, "pro_status", &status)).await.unwrap();
}

async fn world(pro: bool) -> World {
    let (base, relay) = fake_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let apps = PlatformApps { relay_url: Some(base), ..Default::default() };
    let e = Engine::start_with(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()), apps).await.unwrap();
    let tw = Arc::new(Fake::default());
    e.register_connector(tw.clone());
    remember(&e, pro, true).await;
    e.smart_round().await;
    World { e, relay, tw, _dir: dir }
}

static N: AtomicU64 = AtomicU64::new(0);

fn chat(name: &str, text: &str, roles: Roles) -> Event {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let mut a = Actor::named(Platform::Twitch, name);
    a.id = Some(format!("{name}-id"));
    a.login = Some(name.to_lowercase());
    a.roles = roles;
    let mut e = Event::live(
        Platform::Twitch,
        Some(format!("msg-{n}")),
        Millis(now().0),
        EventKind::ChatMessage { actor: a, content: Content::plain(text), reply_to: None, is_action: false },
    );
    e.id = format!("ev-{n}");
    e
}

async fn go_live(e: &Engine) {
    let n = N.fetch_add(1, Ordering::SeqCst);
    e.ingest(Event::live(
        Platform::Twitch,
        Some(format!("on-{n}")),
        Millis(now().0),
        EventKind::StreamOnline { title: None },
    ))
    .await;
}

impl World {
    /// The batch's wait is over: the next round checks it.
    async fn check_now(&self) {
        for w in self.e.smart().queue.iter_mut() {
            w.at -= BATCH_WAIT_MS;
        }
        self.e.smart_round().await;
    }

    fn calls(&self, path: &str) -> Vec<Value> {
        self.relay.lock().unwrap().calls.iter().filter(|(p, _)| p == path).map(|(_, b)| b.clone()).collect()
    }

    async fn did_eventually(&self, what: &str) {
        for _ in 0..300 {
            if self.tw.did().iter().any(|d| d.0 == what) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never did {what}: {:?}", self.tw.did());
    }
}

fn verdict(id: &str, action: &str, confidence: f64) -> Value {
    json!({ "results": [{ "id": id, "action": action, "category": "abuse", "reason": "Hinglish insult", "confidence": confidence }] })
}

// ── Settings ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn settings_are_checked_and_new_facts_need_approving_again() {
    let w = world(true).await;
    let v = w.e.smart_view().await;
    assert_eq!((v["active"].as_bool(), v["settings"]["moderation"].as_str()), (Some(true), Some("off")));
    assert_eq!(v["quotas"]["smart_replies"]["limit"], 20);
    for bad in [
        SmartPatch { act_timeout_s: Some(5), ..Default::default() },
        SmartPatch { reply_gap_s: Some(10), ..Default::default() },
        SmartPatch { rules: Some("r".repeat(501)), ..Default::default() },
        SmartPatch { about: Some("a".repeat(2_001)), ..Default::default() },
    ] {
        assert!(matches!(w.e.smart_save(bad).await, Err(EngineError::Invalid(_))));
    }
    let about = "Live Mon-Fri 8pm IST. Discord: discord.gg/abc";
    w.e.smart_save(SmartPatch { replies: Some(true), about: Some(about.into()), ..Default::default() }).await.unwrap();
    assert_eq!(w.e.smart_view().await["about_approved"], false);
    let err = w.e.smart_approve("something else").await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "about_changed", .. }), "{err:?}");
    assert_eq!(w.e.smart_approve(about).await.unwrap()["about_approved"], true);
    // Kept across a restart (read from the store).
    w.e.load_smart().await;
    assert!(w.e.smart().settings.approved());
    // Changing the facts takes the approval away.
    w.e.smart_save(SmartPatch { about: Some(format!("{about} New rig.")), ..Default::default() }).await.unwrap();
    assert_eq!(w.e.smart_view().await["about_approved"], false);
}

// ── Smart moderation ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_flagged_message_becomes_a_suggestion_with_apply_and_undo() {
    let w = world(true).await;
    w.e.smart_save(SmartPatch {
        moderation: Some(ModMode::Suggest),
        rules: Some("no spoilers".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let bad = chat("Troll", "tu ch*tiya hai", Roles::empty());
    w.relay.lock().unwrap().moderate = Some((200, verdict("$0", "timeout", 0.92)));
    w.e.ingest(chat("Fan", "bhai gg", Roles::empty())).await;
    w.e.ingest(chat("Mod", "teri m.c", Roles::MODERATOR)).await;
    w.e.ingest(bad).await;
    assert_eq!(w.e.smart().queue.len(), 1, "only the doubtful one, never a mod's");
    w.check_now().await;
    let sent = w.calls("/v1/pro/smart/moderate");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["messages"][0]["text"], "tu ch*tiya hai");
    assert_eq!(sent[0]["rules"], "no spoilers");
    let log = w.e.smart_log();
    let s = &log["items"][0];
    assert_eq!((s["action"].as_str(), s["state"].as_str()), (Some("timeout"), Some("suggested")), "{log}");
    let id = s["id"].as_str().unwrap().to_string();
    assert!(w.tw.did().is_empty(), "suggest mode never acts");
    // The streamer applies it, then undoes it.
    let done = w.e.smart_apply(&id, None, Some(300)).await.unwrap();
    assert_eq!(
        (done.state, done.done.as_deref(), done.by.as_deref()),
        (SuggestionState::Done, Some("timeout"), Some("you"))
    );
    assert_eq!(w.tw.did(), [("timeout".to_string(), "Troll-id".to_string(), 300)]);
    assert!(matches!(w.e.smart_apply(&id, None, None).await, Err(EngineError::Conflict(_))));
    let undone = w.e.smart_undo(&id).await.unwrap();
    assert_eq!(undone.state, SuggestionState::Undone);
    assert_eq!(w.tw.did().last().unwrap().0, "unban");
    assert!(matches!(w.e.smart_undo("nope").await, Err(EngineError::Missing { code: "not_found", .. })));
}

#[tokio::test]
async fn deletes_cant_be_undone_and_dismiss_keeps_it_in_the_log() {
    let w = world(true).await;
    w.e.smart_save(SmartPatch { moderation: Some(ModMode::Suggest), ..Default::default() }).await.unwrap();
    let spam = chat("Bot1", "follow 4 follow everyone", Roles::FIRST_TIME);
    w.relay.lock().unwrap().moderate = Some((200, verdict("$0", "delete", 0.7)));
    w.e.ingest(spam).await;
    w.check_now().await;
    let id = w.e.smart_log()["items"][0]["id"].as_str().unwrap().to_string();
    w.e.smart_apply(&id, None, None).await.unwrap();
    assert_eq!(w.tw.did()[0].0, "delete");
    let err = w.e.smart_undo(&id).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "cant_undo", .. }), "{err:?}");
    let other = chat("Bot2", "follow me back pls", Roles::FIRST_TIME);
    w.relay.lock().unwrap().moderate = Some((200, verdict("$0", "delete", 0.65)));
    w.e.ingest(other).await;
    w.check_now().await;
    let oid = w.e.smart_log()["items"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(w.e.smart_dismiss(&oid).unwrap().state, SuggestionState::Dismissed);
    assert_eq!(w.e.smart_log()["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn act_mode_times_out_clear_cases_only_and_never_bans_or_deletes() {
    let w = world(true).await;
    w.e.smart_save(SmartPatch { moderation: Some(ModMode::Act), act_timeout_s: Some(60), ..Default::default() })
        .await
        .unwrap();
    let cases = [("ban", 0.97), ("timeout", 0.7), ("delete", 0.99)];
    for (i, _) in cases.iter().enumerate() {
        w.e.ingest(chat(&format!("T{i}"), "bh3nch0d", Roles::empty())).await;
    }
    let results: Vec<Value> = cases
        .iter()
        .enumerate()
        .map(|(i, (a, c))| json!({ "id": format!("${i}"), "action": a, "category": "abuse", "reason": "r", "confidence": c }))
        .collect();
    w.relay.lock().unwrap().moderate = Some((200, json!({ "results": results })));
    w.check_now().await;
    w.did_eventually("timeout").await;
    assert_eq!(w.tw.did(), [("timeout".to_string(), "T0-id".to_string(), 60)], "only the clear one, as a timeout");
    let items = w.e.smart_log()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 3);
    let first = items.iter().find(|s| s["name"] == "T0").unwrap();
    assert_eq!(
        (first["state"].as_str(), first["by"].as_str(), first["done"].as_str()),
        (Some("done"), Some("smart"), Some("timeout"))
    );
    // Undo lifts it.
    w.e.smart_undo(first["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(w.tw.did().last().unwrap().0, "unban");
}

#[tokio::test]
async fn a_tired_relay_pauses_checks_and_says_why() {
    let w = world(true).await;
    w.e.smart_save(SmartPatch { moderation: Some(ModMode::Suggest), ..Default::default() }).await.unwrap();
    w.relay.lock().unwrap().moderate = Some((
        503,
        json!({ "error": { "code": "smart_resting", "message": "Smart features are resting until tomorrow.", "hint": "h" } }),
    ));
    w.e.ingest(chat("A", "bh3nch0d", Roles::empty())).await;
    w.check_now().await;
    let v = w.e.smart_view().await;
    assert_eq!(v["last_error"]["code"], "smart_resting", "{v}");
    assert!(v["moderation_paused_until"].as_i64().unwrap() > now().0);
    // While paused nothing is queued or sent.
    w.e.ingest(chat("B", "bh3nch0d", Roles::empty())).await;
    assert!(w.e.smart().queue.is_empty());
    assert_eq!(w.calls("/v1/pro/smart/moderate").len(), 1);
}

#[tokio::test]
async fn without_pro_nothing_leaves_the_pc() {
    let w = world(false).await;
    w.e.smart_save(SmartPatch { moderation: Some(ModMode::Act), ..Default::default() }).await.unwrap();
    w.e.ingest(chat("A", "bh3nch0d", Roles::empty())).await;
    w.check_now().await;
    assert!(w.relay.lock().unwrap().calls.is_empty());
    assert_eq!(w.e.smart_view().await["active"], false);
    let err = w.e.smart_recap(None).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 402, code: "pro_required", .. }), "{err:?}");
}

// ── Smart replies ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn replies_need_approved_facts_and_keep_their_gaps() {
    let w = world(true).await;
    w.relay.lock().unwrap().reply = Some((200, json!({ "reply": "Mon to Fri, 8pm IST!" })));
    let about = "Live Mon-Fri 8pm IST.";
    w.e.smart_save(SmartPatch { replies: Some(true), about: Some(about.into()), ..Default::default() }).await.unwrap();
    go_live(&w.e).await;
    // Not approved yet: quiet.
    w.e.ingest(chat("Ana", "when do you stream?", Roles::empty())).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(w.calls("/v1/pro/smart/reply").is_empty());
    w.e.smart_approve(about).await.unwrap();
    w.e.ingest(chat("Ana", "kab stream hoti hai?", Roles::empty())).await;
    for _ in 0..300 {
        if !w.tw.said.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*w.tw.said.lock().unwrap(), ["Mon to Fri, 8pm IST!"]);
    let sent = w.calls("/v1/pro/smart/reply");
    assert_eq!((sent[0]["question"].as_str(), sent[0]["about"].as_str()), (Some("kab stream hoti hai?"), Some(about)));
    // Within the gap: no second call, from anyone. Not a question: never.
    w.e.ingest(chat("Bo", "what game is this?", Roles::empty())).await;
    w.e.ingest(chat("Cy", "nice play", Roles::empty())).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(w.calls("/v1/pro/smart/reply").len(), 1);
    assert_eq!(w.e.smart_log()["replies"][0]["reply"], "Mon to Fri, 8pm IST!");
}

// ── The written recap ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_recap_is_written_and_kept() {
    let w = world(true).await;
    let err = w.e.smart_recap(None).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 409, code: "no_stream", .. }), "{err:?}");
    go_live(&w.e).await;
    w.e.ingest(chat("Ana", "hi", Roles::empty())).await;
    w.relay.lock().unwrap().recap = Some((200, json!({ "summary": "A warm, steady stream." })));
    let v = w.e.smart_recap(None).await.unwrap();
    assert_eq!(v["summary"], "A warm, steady stream.");
    assert_eq!(w.e.smart_recap_saved().await.unwrap()["summary"], "A warm, steady stream.");
    let sent = w.calls("/v1/pro/smart/recap");
    assert!(sent[0]["minutes"].is_i64() && sent[0]["thanks"].is_array(), "{}", sent[0]);
    // The relay's limit reaches the dashboard with its words.
    w.relay.lock().unwrap().recap = Some((
        429,
        json!({ "error": { "code": "smart_quota", "message": "You've used today's stream recaps.", "hint": "It starts again on…" } }),
    ));
    let err = w.e.smart_recap(None).await.unwrap_err();
    assert!(matches!(err, EngineError::Refused { status: 429, code: "smart_quota", .. }), "{err:?}");
}
