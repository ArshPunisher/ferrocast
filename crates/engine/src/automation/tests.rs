use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrocast_connector::Connector;
use ferrocast_core::event::{Actor, Content, Millis, Platform};
use ferrocast_core::moderation::{Capabilities, ModeChange};

use super::*;
use crate::studio_host::{StudioHost, StudioSettings, StudioStatus};

fn rule(id: &str, when: When, then: Vec<Step>) -> Rule {
    Rule { id: id.into(), name: format!("Rule {id}"), on: true, when, then, cooldown_s: 10 }
}

fn chat_from(text: &str, roles: Roles) -> Event {
    let mut a = Actor::named(Platform::Twitch, "Ana");
    a.roles = roles;
    Event::live(
        Platform::Twitch,
        Some(format!("m-{text}")),
        Millis(0),
        EventKind::ChatMessage { actor: a, content: Content::plain(text), reply_to: None, is_action: false },
    )
}

pub(crate) fn live(kind: EventKind) -> Event {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Event::live(Platform::Twitch, Some(format!("e{n}")), Millis(0), kind)
}

fn refused(r: Result<(), EngineError>) -> String {
    match r {
        Err(EngineError::Refused { status: 422, code: "automation_invalid", message, .. }) => message,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn good_rules_pass_and_bad_ones_say_why() {
    let ok = rule("a", When::Follow, vec![Step::Scene { name: "Thanks".into() }]);
    assert!(check(std::slice::from_ref(&ok)).is_ok());
    assert!(check(&[]).is_ok(), "no rules is fine");
    let mut cases: Vec<(Rule, &str)> = Vec::new();
    let mut r = ok.clone();
    r.id = "bad id!".into();
    cases.push((r, "isn't one"));
    let mut r = ok.clone();
    r.name = "  ".into();
    cases.push((r, "name is empty"));
    let mut r = ok.clone();
    r.name = "x".repeat(61);
    cases.push((r, "longer than 60"));
    let mut r = ok.clone();
    r.cooldown_s = 1;
    cases.push((r, "3 to 3600"));
    let mut r = ok.clone();
    r.then.clear();
    cases.push((r, "nothing to do"));
    let mut r = ok.clone();
    r.then = vec![Step::SaveReplay; 11];
    cases.push((r, "up to 10 steps"));
    cases.push((rule("b", When::Chat { text: " ".into(), anyone: true }, ok.then.clone()), "chat command is empty"));
    cases.push((rule("c", When::Every { minutes: 0 }, ok.then.clone()), "every 1 to 720"));
    cases.push((rule("d", When::Every { minutes: 721 }, ok.then.clone()), "every 1 to 720"));
    cases.push((rule("e", When::Scene { name: String::new() }, ok.then.clone()), "scene is empty"));
    cases.push((rule("f", When::Follow, vec![Step::Wait { seconds: 0 }]), "wait 1 to 600"));
    cases.push((rule("g", When::Follow, vec![Step::Wait { seconds: 601 }]), "wait 1 to 600"));
    cases.push((rule("h", When::Follow, vec![Step::Chat { text: "x".repeat(301) }]), "longer than 300"));
    cases.push((
        rule("i", When::Follow, vec![Step::Show { scene: "Main".into(), source: "".into() }]),
        "source is empty",
    ));
    cases.push((
        rule("j", When::Follow, vec![Step::Filter { source: "Cam".into(), filter: "".into(), on: true }]),
        "filter is empty",
    ));
    for (r, why) in cases {
        let got = refused(check(std::slice::from_ref(&r)));
        assert!(got.contains(why), "{r:?}: {got}");
    }
    assert!(refused(check(&[ok.clone(), ok.clone()])).contains("Two rules"));
    let many: Vec<Rule> = (0..51).map(|i| rule(&format!("r{i}"), When::Follow, ok.then.clone())).collect();
    assert!(refused(check(&many)).contains("Up to 50"));
}

#[test]
fn what_sets_a_rule_off() {
    use ferrocast_core::event::{Tier, TipProvider};
    let a = || Actor::named(Platform::Twitch, "Ana");
    assert!(sets_off(&When::Follow, &live(EventKind::Follow { actor: a() })));
    assert!(!sets_off(&When::Follow, &live(EventKind::StreamOffline)));
    let raid = |v| live(EventKind::Raid { from: a(), viewers: v });
    assert!(sets_off(&When::Raid { min: 10 }, &raid(10)));
    assert!(!sets_off(&When::Raid { min: 10 }, &raid(9)), "too small a raid");
    assert!(sets_off(&When::Raid { min: 0 }, &raid(1)));
    let sub =
        live(EventKind::Subscription { actor: a(), tier: Tier::Tier1, months: 1, message: None, gifted_by: None });
    assert!(sets_off(&When::Sub, &sub));
    assert!(sets_off(&When::Sub, &live(EventKind::GiftBatch { gifter: a(), tier: Tier::Tier1, count: 5 })));
    let tip = |status| {
        live(EventKind::Tip {
            actor: a(),
            money: ferrocast_core::money::Money { minor: 500, currency: "INR".into() },
            message: None,
            provider: TipProvider::Razorpay,
            payment_ref: "p".into(),
            status,
        })
    };
    assert!(sets_off(&When::Support, &tip(PaymentStatus::Captured)));
    assert!(!sets_off(&When::Support, &tip(PaymentStatus::Pending)), "only money that arrived");
    let reward =
        |name: &str| live(EventKind::RewardRedemption { actor: a(), reward: name.into(), cost: 100, input: None });
    assert!(sets_off(&When::Reward { name: "Hydrate".into() }, &reward("hydrate ")));
    assert!(!sets_off(&When::Reward { name: "Hydrate".into() }, &reward("Dance")));
    assert!(sets_off(&When::Reward { name: String::new() }, &reward("Dance")), "any reward");
    assert!(sets_off(&When::LiveStart, &live(EventKind::StreamOnline { title: None })));
    assert!(sets_off(&When::LiveEnd, &live(EventKind::StreamOffline)));
    // A scene or the clock never comes as an event.
    assert!(!sets_off(&When::Scene { name: "Main".into() }, &live(EventKind::StreamOffline)));
}

#[test]
fn a_chat_command_answers_whom_it_should() {
    let mods = When::Chat { text: "!clip".into(), anyone: false };
    let all = When::Chat { text: "!clip".into(), anyone: true };
    assert!(sets_off(&mods, &chat_from("!clip", Roles::MODERATOR)));
    assert!(sets_off(&mods, &chat_from("  !CLIP that", Roles::BROADCASTER)), "any case, with words after");
    assert!(!sets_off(&mods, &chat_from("!clip", Roles::empty())), "a viewer can't, unless anyone may");
    assert!(!sets_off(&mods, &chat_from("!clip", Roles::VIP)));
    assert!(sets_off(&all, &chat_from("!clip", Roles::empty())));
    assert!(!sets_off(&all, &chat_from("!clipboard", Roles::empty())), "a longer word isn't the command");
    assert!(!sets_off(&all, &chat_from("say !clip", Roles::empty())), "it starts the message");
    assert!(!sets_off(&all, &chat_from("!cl", Roles::empty())));
    assert!(!sets_off(&all, &chat_from("!clé", Roles::empty())), "no panic on letters of several bytes");
    assert!(!sets_off(&When::Chat { text: "!é".into(), anyone: true }, &chat_from("!x", Roles::empty())));
}

#[derive(Debug, Default)]
pub(crate) struct Studio {
    pub(crate) did: Mutex<Vec<String>>,
}

#[async_trait]
impl StudioHost for Studio {
    async fn status(&self) -> StudioStatus {
        StudioStatus { available: true, running: true, ..Default::default() }
    }
    async fn start(&self) -> Result<StudioStatus, String> {
        Ok(self.status().await)
    }
    async fn stop(&self) -> Result<StudioStatus, String> {
        Ok(self.status().await)
    }
    async fn kinds(&self) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn options(&self, _kind: String) -> Result<Value, String> {
        Ok(json!([]))
    }
    async fn preview(&self, _rect: Option<crate::studio_host::PreviewRect>) -> Result<(), String> {
        Ok(())
    }
    async fn defaults(&self, _kind: String) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn settings(&self) -> StudioSettings {
        StudioSettings::default()
    }
    async fn set_settings(&self, _s: StudioSettings) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn stats(&self) -> Result<Value, String> {
        Ok(Value::Null)
    }
    async fn record(&self, on: bool) -> Result<Value, String> {
        self.did.lock().unwrap_or_else(|e| e.into_inner()).push(format!("record {on}"));
        Ok(json!({}))
    }
    async fn record_pause(&self, _on: bool) -> Result<(), String> {
        Ok(())
    }
    async fn replay_save(&self) -> Result<Value, String> {
        self.did.lock().unwrap_or_else(|e| e.into_inner()).push("save replay".into());
        Ok(json!({ "file": null }))
    }
    async fn monitoring(&self) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn act(&self, act: StudioAct) -> Result<Value, String> {
        let line = match &act {
            StudioAct::Items { .. } => return Ok(json!([{ "id": 7, "name": "Logo" }])),
            StudioAct::Scenes => return Ok(json!({ "current": "Main" })),
            StudioAct::SetCurrent { scene } if scene == "Nope" => {
                return Err("There's no scene called \"Nope\".".into());
            }
            StudioAct::SetCurrent { scene } => format!("scene {scene}"),
            StudioAct::EditItem { scene, id, edit } => format!("{edit:?} {scene} {id}"),
            StudioAct::EditFilter { name, filter, edit } => format!("{edit:?} {name} {filter}"),
            other => format!("{other:?}"),
        };
        self.did.lock().unwrap_or_else(|e| e.into_inner()).push(line);
        Ok(Value::Null)
    }
    async fn stream_start(&self, _server: String, _key: String) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn stream_stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stream_stats(&self) -> Result<Value, String> {
        Ok(Value::Null)
    }
    async fn obs_collections(&self) -> Result<Value, String> {
        Ok(json!([]))
    }
    async fn import_obs(&self, _id: String) -> Result<Value, String> {
        Ok(Value::Null)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Chat {
    pub(crate) said: Mutex<Vec<String>>,
}

#[async_trait]
impl Connector for Chat {
    fn platform(&self) -> Platform {
        Platform::Twitch
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
        self.said.lock().unwrap_or_else(|e| e.into_inner()).push(text.into());
        Ok(())
    }
    async fn delete_message(&self, _id: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn timeout(&self, _u: &str, _s: u32, _r: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ban(&self, _u: &str, _r: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn unban(&self, _u: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn set_mode(&self, _c: ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
}

pub(crate) async fn engine() -> (Engine, tempfile::TempDir, Arc<Studio>, Arc<Chat>) {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let e = Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default()))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let studio = Arc::new(Studio::default());
    e.set_studio_host(studio.clone());
    let chat = Arc::new(Chat::default());
    e.register_connector(chat.clone());
    (e, dir, studio, chat)
}

pub(crate) async fn until(mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened");
}

pub(crate) fn did(s: &Studio) -> Vec<String> {
    s.did.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tokio::test]
async fn a_rule_runs_its_steps_once_per_cooldown_and_is_kept() {
    let (e, dir, studio, chat) = engine().await;
    let clip = rule(
        "clip",
        When::Chat { text: "!clip".into(), anyone: true },
        vec![
            Step::SaveReplay,
            Step::Show { scene: "Main".into(), source: "Logo".into() },
            Step::Filter { source: "Cam".into(), filter: "Blur".into(), on: false },
            Step::Scene { name: "Replay".into() },
            Step::Chat { text: "Saved, {name}!".into() },
        ],
    );
    let saved = e.set_automations(vec![clip.clone()]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(saved, vec![clip.clone()]);
    e.ingest(chat_from("!clip", Roles::empty())).await;
    until(|| chat.said.lock().unwrap_or_else(|e| e.into_inner()).len() == 1).await;
    assert_eq!(did(&studio), ["save replay", "Show Main 7", "Disable Cam Blur", "scene Replay"], "in order");
    assert_eq!(chat.said.lock().unwrap_or_else(|e| e.into_inner())[0], "Saved, Ana!");
    // Again at once: the cooldown holds it.
    e.ingest(chat_from("!clip again", Roles::empty())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(did(&studio).len(), 4, "not twice within its cooldown");
    // The Test button runs it anyway.
    e.run_automation("clip").unwrap_or_else(|e| panic!("{e}"));
    until(|| did(&studio).len() == 8).await;
    assert!(matches!(e.run_automation("nope"), Err(EngineError::Missing { code: "automation_not_found", .. })));
    // Kept in the data folder, read back by a new engine.
    let kept: Vec<Rule> =
        serde_json::from_slice(&std::fs::read(dir.path().join("automations.json")).unwrap_or_default())
            .unwrap_or_default();
    assert_eq!(kept, vec![clip]);
    // A bad list isn't saved and the old one stays.
    assert!(e.set_automations(vec![rule("x", When::Follow, vec![])]).is_err());
    assert_eq!(e.automations().len(), 1);
}

#[tokio::test]
async fn off_old_or_failing_rules_do_nothing_more() {
    let (e, _dir, studio, _chat) = engine().await;
    let mut off = rule("off", When::Follow, vec![Step::Record { on: true }]);
    off.on = false;
    let failing =
        rule("fail", When::Raid { min: 0 }, vec![Step::Scene { name: "Nope".into() }, Step::Record { on: true }]);
    let follow = rule("follow", When::Follow, vec![Step::Hide { scene: "Main".into(), source: "Missing".into() }]);
    e.set_automations(vec![off, failing, follow]).unwrap_or_else(|e| panic!("{e}"));
    let mut rx = e.subscribe_live();
    // Caught up on after a gap: never runs.
    let mut old = live(EventKind::Raid { from: Actor::named(Platform::Twitch, "Bo"), viewers: 5 });
    old.origin = Origin::Backfill;
    e.ingest(old).await;
    // A raid: its first step fails, so the second never runs, and the dashboard is told.
    e.ingest(live(EventKind::Raid { from: Actor::named(Platform::Twitch, "Bo"), viewers: 5 })).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no notice"))
            .unwrap_or_else(|e| panic!("{e}"));
        if f.text.contains("stopped at step 1") {
            assert!(f.text.contains("Nope"), "{}", f.text);
            break;
        }
    }
    assert!(did(&studio).iter().all(|d| !d.starts_with("record")), "an off rule and a stopped one record nothing");
    // A follow: the source isn't in the scene, said plainly.
    e.ingest(live(EventKind::Follow { actor: Actor::named(Platform::Twitch, "Cy") })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(did(&studio).iter().all(|d| !d.starts_with("Hide")));
}

#[tokio::test]
async fn a_scene_rule_runs_when_its_scene_goes_on_air() {
    let (e, _dir, studio, _chat) = engine().await;
    e.set_automations(vec![rule("brb", When::Scene { name: "Main".into() }, vec![Step::Record { on: false }])])
        .unwrap_or_else(|e| panic!("{e}"));
    // The first look only learns what's on air (it didn't just go there).
    e.automation_tick().await;
    assert!(did(&studio).is_empty());
    e.with_automations(|a| a.scene = Some("Other".into()));
    e.automation_tick().await;
    until(|| did(&studio).contains(&"record false".to_string())).await;
}
