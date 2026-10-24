//! Studio through the real engine: chat in, replies out (a fake platform records them), OBS
//! requests against the fake OBS from the testkit (never a real OBS), stream info against
//! fake platforms that accept or refuse.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrocast_connector::{CategoryHit, Connector, ConnectorError, StreamInfo};
use ferrocast_core::chatbot::{CounterDef, CounterOp};
use ferrocast_core::event::*;
use ferrocast_core::moderation::{Capabilities, ModeChange};
use ferrocast_core::moments::MomentKind;
use ferrocast_core::money::Money;
use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, closed_port, start as start_obs};
use serde_json::{Value, json};

use crate::{Engine, PasswordChange};

#[derive(Debug, Default)]
struct Fake {
    platform: Option<Platform>,
    said: Mutex<Vec<String>>,
    infos: Mutex<Vec<StreamInfo>>,
    refuse_scope: bool,
    markers: Mutex<Vec<String>>,
    /// Thumbnails taken: (bytes, mime). Only a YouTube fake takes them.
    thumbs: Mutex<Vec<(usize, String)>>,
}

impl Fake {
    fn new(p: Platform) -> Arc<Fake> {
        Arc::new(Fake { platform: Some(p), ..Default::default() })
    }
    fn said(&self) -> Vec<String> {
        self.said.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connector for Fake {
    fn platform(&self) -> Platform {
        self.platform.unwrap_or(Platform::Twitch)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { modes: vec![], can_ban: true, can_timeout: true, can_delete: true }
    }
    async fn send_chat(&self, text: &str, _reply_to: Option<&str>) -> ferrocast_connector::Result<()> {
        self.said.lock().unwrap().push(text.to_string());
        Ok(())
    }
    async fn delete_message(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn timeout(&self, _: &str, _: u32, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn ban(&self, _: &str, _: Option<&str>) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn unban(&self, _: &str) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn set_mode(&self, _: ModeChange) -> ferrocast_connector::Result<()> {
        Ok(())
    }
    async fn update_stream_info(&self, info: &StreamInfo) -> ferrocast_connector::Result<Vec<String>> {
        if self.refuse_scope {
            return Err(ConnectorError::MissingScope { platform: self.platform(), scope: "channel:write".into() });
        }
        self.infos.lock().unwrap().push(info.clone());
        Ok(info.fields().into_iter().map(String::from).collect())
    }
    async fn set_thumbnail(&self, image: &[u8], mime: &str) -> ferrocast_connector::Result<()> {
        if self.platform() != Platform::Youtube {
            return Err(ConnectorError::NotSupported { platform: self.platform(), what: "set a stream thumbnail" });
        }
        self.thumbs.lock().unwrap().push((image.len(), mime.to_string()));
        Ok(())
    }
    async fn search_categories(&self, q: &str) -> ferrocast_connector::Result<Vec<CategoryHit>> {
        Ok(vec![CategoryHit { id: "1".into(), name: format!("{q} result"), image_url: None }])
    }
    async fn create_marker(&self, d: &str) -> ferrocast_connector::Result<()> {
        self.markers.lock().unwrap().push(d.into());
        Ok(())
    }
    async fn followed_at(&self, user_id: &str) -> ferrocast_connector::Result<Option<i64>> {
        Ok((user_id == "fan-id").then(|| crate::now().0 - 400 * 86_400_000))
    }
}

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    (e, dir)
}

/// Points the engine at a fake OBS (tests never reach a real one).
async fn with_obs(e: &Engine) -> ferrocast_testkit::obs::St {
    let st = FakeObs::studio();
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    st
}

async fn without_obs(e: &Engine) {
    let port = closed_port().await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
}

fn person(p: Platform, name: &str, roles: Roles) -> Actor {
    let mut a = Actor::named(p, name);
    a.id = Some(format!("{}-id", name.to_lowercase()));
    a.roles = roles;
    a
}

fn chat_as(a: &Actor, text: &str) -> Event {
    Event::live(
        a.platform,
        Some(format!("m-{}-{}", a.display_name, crate::now().0 ^ (text.len() as i64) ^ rand_id())),
        Millis(0),
        EventKind::ChatMessage { actor: a.clone(), content: Content::plain(text), reply_to: None, is_action: false },
    )
}

fn rand_id() -> i64 {
    i64::try_from(super::chatbot::random_u64() >> 2).unwrap_or(0)
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

async fn said_eventually(fake: &Fake, needle: &str) {
    for _ in 0..300 {
        if fake.said().iter().any(|s| s.contains(needle)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never said {needle:?}; said {:?}", fake.said());
}

async fn go_live(e: &Engine) {
    e.ingest(Event::live(
        Platform::Twitch,
        Some(format!("online-{}", rand_id())),
        Millis(0),
        EventKind::StreamOnline { title: None },
    ))
    .await;
}

// ---------------------------------------------------------------- counters

#[tokio::test]
async fn counters_show_for_viewers_and_change_for_mods() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let c = e.create_counter(CounterDef::new("deaths"), 0).await.unwrap();
    assert_eq!((c.name.as_str(), c.value, c.command.as_deref()), ("deaths", 0, Some("!deaths")));
    let viewer = person(Platform::Twitch, "Viewer", Roles::empty());
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&viewer, "!deaths +")).await;
    said_eventually(&tw, "Deaths: 0").await;
    assert_eq!(e.counter_view("deaths").unwrap().value, 0, "viewers can't change it");
    for (cmd, want) in
        [("!deaths +", 1), ("!DEATHS +5", 6), ("!deaths -", 5), ("!deaths set 10", 10), ("!deaths reset", 0)]
    {
        e.ingest(chat_as(&m, cmd)).await;
        let v = e.counter_view("deaths").unwrap().value;
        assert_eq!(v, want, "{cmd}");
    }
    e.ingest(chat_as(&m, "!deaths set lots")).await;
    said_eventually(&tw, "!deaths set 10").await;
    // The dashboard changes it too.
    assert_eq!(e.change_counter("deaths", CounterOp::Add(3)).await.unwrap().value, 3);
    assert!(matches!(
        e.change_counter("nope", CounterOp::Add(1)).await,
        Err(crate::EngineError::Missing { code: "counter_not_found", .. })
    ));
}

#[tokio::test]
async fn counters_clash_checks_and_template_variables() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.create_counter(CounterDef::new("deaths"), 4).await.unwrap();
    assert!(matches!(e.create_counter(CounterDef::new("deaths"), 0).await, Err(crate::EngineError::Conflict(_))));
    let mut clash = CounterDef::new("wins");
    clash.command = Some("!deaths".into());
    assert!(matches!(e.create_counter(clash, 0).await, Err(crate::EngineError::Conflict(_))));
    let mut builtin = CounterDef::new("wins");
    builtin.command = Some("!quote".into());
    assert!(matches!(e.create_counter(builtin, 0).await, Err(crate::EngineError::Conflict(_))));
    let bad = CounterDef { name: "Bad Name".into(), ..CounterDef::new("x") };
    assert!(matches!(e.create_counter(bad, 0).await, Err(crate::EngineError::Invalid(_))));
    // A custom command can read any counter.
    let cmd = ferrocast_core::command::Command {
        id: String::new(),
        name: "!rip".into(),
        aliases: vec![],
        response: "We died {count:deaths} times".into(),
        access: ferrocast_core::command::Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    };
    e.save_command(cmd).await.unwrap();
    e.ingest(chat_as(&person(Platform::Twitch, "V", Roles::empty()), "!rip")).await;
    said_eventually(&tw, "We died 4 times").await;
    // A counter can't take a custom command's name.
    let mut taken = CounterDef::new("rips");
    taken.command = Some("!rip".into());
    assert!(matches!(e.create_counter(taken, 0).await, Err(crate::EngineError::Conflict(_))));
    // Patch: command off, then delete.
    let patch = json!({ "command": null, "reply": "{counter} = {count}" });
    let v = e.update_counter("deaths", patch.as_object().unwrap()).await.unwrap();
    assert!(v.command.is_none());
    assert!(e.update_counter("deaths", json!({ "value": "x" }).as_object().unwrap()).await.is_err());
    e.delete_counter("deaths").await.unwrap();
    assert!(e.delete_counter("deaths").await.is_err());
    assert!(e.counters().is_empty());
}

#[tokio::test]
async fn a_counter_change_updates_overlay_labels_that_show_it() {
    let (e, _d) = engine().await;
    e.create_counter(CounterDef::new("deaths"), 0).await.unwrap();
    let mut o = e
        .db(|s| ferrocast_store::repo::overlays::create(s, "Main", ferrocast_core::overlay::Preset::Blank))
        .await
        .unwrap();
    o.widgets.push(ferrocast_core::overlay::Widget::new(
        "w1",
        ferrocast_core::overlay::WidgetKind::Label,
        (0, 0, 100, 40),
        json!({ "source": "session_follows", "template": "Deaths: {count:deaths}", "theme": "molten" }),
    ));
    let saved = o.clone();
    e.db(move |s| ferrocast_store::repo::overlays::update(s, &saved)).await.unwrap();
    let mut rx = e.subscribe_overlays();
    e.change_counter("deaths", CounterOp::Add(2)).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, rx.recv()).await.expect("no label frame").unwrap();
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "state" && v["data"]["labels"]["w1"].is_string() {
            assert_eq!(v["data"]["labels"]["w1"], "Deaths: 2");
            break;
        }
    }
}

// ---------------------------------------------------------------- quotes

#[tokio::test]
async fn quotes_from_chat_and_the_dashboard() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    let v = person(Platform::Twitch, "Viewer", Roles::empty());
    e.ingest(chat_as(&v, "!quote add sneaky viewer")).await;
    e.ingest(chat_as(&v, "!quote")).await;
    said_eventually(&tw, "No quotes yet").await;
    assert!(e.quotes().await.unwrap().is_empty(), "viewers can't add");
    e.ingest(chat_as(&m, "!quote add gg no re")).await;
    said_eventually(&tw, "Added quote #1.").await;
    e.ingest(chat_as(&m, "!quote 1")).await;
    said_eventually(&tw, "Quote #1: \"gg no re\" (Mod,").await;
    e.ingest(chat_as(&m, "!quote 9")).await;
    said_eventually(&tw, "There's no quote #9.").await;
    let q = e.add_quote("from the dashboard", "Streamer").await.unwrap();
    assert_eq!(e.edit_quote(q.id, "edited").await.unwrap().text, "edited");
    assert!(matches!(e.edit_quote(999, "x").await, Err(crate::EngineError::Missing { code: "quote_not_found", .. })));
    assert!(matches!(e.add_quote(" ", "x").await, Err(crate::EngineError::Invalid(_))));
    e.ingest(chat_as(&m, "!quote remove 1")).await;
    said_eventually(&tw, "Removed quote #1.").await;
    assert_eq!(e.quotes().await.unwrap().len(), 1);
    assert!(matches!(e.delete_quote(1).await, Err(crate::EngineError::Missing { .. })));
}

// ---------------------------------------------------------------- modules

#[tokio::test]
async fn modules_answer_and_custom_commands_win() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let fan = person(Platform::Twitch, "Fan", Roles::empty());
    e.ingest(chat_as(&fan, "!8ball will we win?")).await;
    said_eventually(&tw, "@Fan, ").await;
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&m, "!roll 2d6")).await;
    said_eventually(&tw, "@Mod rolled 2d6:").await;
    e.ingest(chat_as(&m, "!uptime")).await;
    said_eventually(&tw, "We're not live right now.").await;
    e.ingest(chat_as(&fan, "!followage")).await;
    said_eventually(&tw, "Fan has followed for 1 year, 1 month.").await;
    e.ingest(chat_as(&m, "!so @friend")).await;
    said_eventually(&tw, "Go show some love to friend! https://twitch.tv/friend").await;
    // Cooldown: the same viewer at once gets nothing more.
    let before = tw.said().len();
    e.ingest(chat_as(&fan, "!8ball again?")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tw.said().len(), before, "viewer cooldown");
    // Switched off: silent.
    e.patch_modules(json!({ "dice": { "enabled": false } }).as_object().unwrap()).await.unwrap();
    e.ingest(chat_as(&m, "!roll")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tw.said().len(), before);
    // A custom !uptime wins over the module.
    let cmd = ferrocast_core::command::Command {
        id: String::new(),
        name: "!uptime".into(),
        aliases: vec![],
        response: "custom uptime".into(),
        access: ferrocast_core::command::Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    };
    e.save_command(cmd).await.unwrap();
    e.ingest(chat_as(&m, "!uptime")).await;
    said_eventually(&tw, "custom uptime").await;
    // Bad settings name the field.
    let err = e.patch_modules(json!({ "roulette": { "win_percent": 0 } }).as_object().unwrap()).await.unwrap_err();
    assert!(err.to_string().starts_with("roulette.win_percent"), "{err}");
    let err = e.patch_modules(json!({ "dice": { "enabled": "yes" } }).as_object().unwrap()).await.unwrap_err();
    assert_eq!(err.to_string(), "dice.enabled: must be true or false.");
    let err = e.patch_modules(json!({ "dice": true }).as_object().unwrap()).await.unwrap_err();
    assert!(err.to_string().starts_with("dice:"));
}

#[tokio::test]
async fn roulette_and_duels_move_points() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    e.patch_modules(json!({ "roulette": { "enabled": true }, "duel": { "enabled": true } }).as_object().unwrap())
        .await
        .unwrap();
    let a = person(Platform::Twitch, "Asha", Roles::MODERATOR);
    let b = person(Platform::Twitch, "Bo", Roles::MODERATOR);
    e.ingest(chat_as(&a, "hi")).await;
    e.ingest(chat_as(&b, "hello")).await;
    e.adjust_points(Platform::Twitch, "asha-id", 1000).await.unwrap();
    e.adjust_points(Platform::Twitch, "bo-id", 1000).await.unwrap();
    e.ingest(chat_as(&a, "!roulette 100")).await;
    said_eventually(&tw, "@Asha ").await;
    let after = e.points(Platform::Twitch, "asha-id").await.unwrap();
    assert!(after == 1100 || after == 900, "{after}");
    e.ingest(chat_as(&a, "!roulette 5")).await;
    said_eventually(&tw, "the smallest bet is 10").await;
    e.ingest(chat_as(&a, "!roulette lots")).await;
    said_eventually(&tw, "bet a number").await;
    e.ingest(chat_as(&a, "!duel @bo 200")).await;
    said_eventually(&tw, "@Bo, Asha challenges you to a duel for 200 points!").await;
    e.ingest(chat_as(&b, "!accept")).await;
    said_eventually(&tw, "wins the duel against").await;
    let total =
        e.points(Platform::Twitch, "asha-id").await.unwrap() + e.points(Platform::Twitch, "bo-id").await.unwrap();
    assert_eq!(total, after + 1000, "a duel moves points, never makes them");
    e.ingest(chat_as(&a, "!duel @asha 10")).await;
    said_eventually(&tw, "you can't duel yourself").await;
    e.ingest(chat_as(&a, "!duel @ghost 10")).await;
    said_eventually(&tw, "I haven't seen @ghost").await;
    e.ingest(chat_as(&a, "!duel @bo 10")).await;
    e.ingest(chat_as(&b, "!deny")).await;
    said_eventually(&tw, "Bo turned down the duel with Asha.").await;
}

#[tokio::test]
async fn watch_time_counts_chatters_while_live() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let fan = person(Platform::Twitch, "Fan", Roles::empty());
    e.ingest(chat_as(&fan, "offline chat")).await;
    assert_eq!(e.credit_watch_time(5).await, 0, "only while live");
    go_live(&e).await;
    e.ingest(chat_as(&fan, "live chat")).await;
    assert_eq!(e.credit_watch_time(5).await, 1);
    e.ingest(chat_as(&fan, "!watchtime")).await;
    said_eventually(&tw, "@Fan has watched for 5 minutes.").await;
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&m, "!uptime")).await;
    said_eventually(&tw, "We've been live for").await;
}

// ---------------------------------------------------------------- stream info

#[tokio::test]
async fn stream_info_reports_each_platform_and_keeps_history() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    let kick = Arc::new(Fake { platform: Some(Platform::Kick), refuse_scope: true, ..Default::default() });
    e.register_connector(tw.clone());
    e.register_connector(kick.clone());
    let info = StreamInfo { title: Some(" Ranked grind ".into()), language: Some("en".into()), ..Default::default() };
    let r = e.apply_stream_info(info, None, None).await.unwrap();
    assert_eq!((r.ok, r.failed), (1, 1));
    let k = r.results.iter().find(|x| x.platform == Platform::Kick).unwrap();
    let err = k.error.as_ref().unwrap();
    assert_eq!(err.code, "reconnect_needed");
    assert!(err.message.contains("channel:write"), "{}", err.message);
    assert!(k.skipped.iter().any(|s| s.starts_with("language")), "Kick has no language");
    assert_eq!(tw.infos.lock().unwrap()[0].title.as_deref(), Some("Ranked grind"));
    let view = e.stream_info().await.unwrap();
    assert_eq!(view.last.unwrap().info.title.as_deref(), Some("Ranked grind"));
    // Only some platforms.
    let r = e
        .apply_stream_info(
            StreamInfo { title: Some("Twitch only".into()), ..Default::default() },
            Some(vec![Platform::Twitch]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(r.results.len(), 1);
    // Not connected platform asked for: its own result.
    let r = e
        .apply_stream_info(
            StreamInfo { title: Some("x".into()), ..Default::default() },
            Some(vec![Platform::Youtube]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(r.results[0].error.as_ref().unwrap().code, "not_connected");
    // Bad input and nowhere to send.
    assert!(matches!(
        e.apply_stream_info(StreamInfo::default(), None, None).await,
        Err(crate::EngineError::Invalid(_))
    ));
    assert!(matches!(
        e.apply_stream_info(
            StreamInfo { title: Some("x".into()), ..Default::default() },
            Some(vec![Platform::Tips]),
            None
        )
        .await,
        Err(crate::EngineError::Invalid(_))
    ));
    e.unregister_connector(Platform::Twitch);
    e.unregister_connector(Platform::Kick);
    assert!(matches!(
        e.apply_stream_info(StreamInfo { title: Some("x".into()), ..Default::default() }, None, None).await,
        Err(crate::EngineError::Refused { status: 422, code: "no_platforms", .. })
    ));
}

#[tokio::test]
async fn each_platform_gets_its_own_fields_on_top_of_everywhere() {
    use ferrocast_core::stream_info::PerPlatform;
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    let yt = Fake::new(Platform::Youtube);
    e.register_connector(tw.clone());
    e.register_connector(yt.clone());
    let info = StreamInfo {
        title: Some("Day 41: Immortal or bust".into()),
        tags: Some(vec!["valorant".into()]),
        per: PerPlatform {
            twitch: Some(Box::new(StreamInfo { title: Some("Ranked grind | !socials".into()), ..Default::default() })),
            ..Default::default()
        },
        ..Default::default()
    };
    let r = e.apply_stream_info(info, None, None).await.unwrap();
    assert_eq!((r.ok, r.failed), (2, 0));
    let got = tw.infos.lock().unwrap()[0].clone();
    assert_eq!(got.title.as_deref(), Some("Ranked grind | !socials"));
    assert_eq!(got.tags, Some(vec!["valorant".to_string()]));
    assert!(got.per.is_empty(), "a platform only sees its own merged fields");
    assert_eq!(yt.infos.lock().unwrap()[0].title.as_deref(), Some("Day 41: Immortal or bust"));
    // History and presets keep the per-platform choices.
    let last = e.stream_info().await.unwrap().last.unwrap();
    assert_eq!(last.info.per.twitch.as_ref().and_then(|t| t.title.as_deref()), Some("Ranked grind | !socials"));
}

#[tokio::test]
async fn presets_and_reconnect_hints() {
    use ferrocast_core::stream_info::Preset;
    let (e, _d) = engine().await;
    let p = Preset {
        id: String::new(),
        name: "Valorant ranked".into(),
        info: StreamInfo { title: Some("Ranked".into()), ..Default::default() },
        updated_at: 0,
    };
    let saved = e.save_preset(p.clone(), true).await.unwrap();
    assert!(!saved.id.is_empty());
    assert!(matches!(e.save_preset(p.clone(), true).await, Err(crate::EngineError::Conflict(_))));
    let renamed = Preset { name: "Valorant".into(), ..saved.clone() };
    assert_eq!(e.save_preset(renamed, false).await.unwrap().name, "Valorant");
    let ghost = Preset { id: "nope".into(), ..saved.clone() };
    assert!(matches!(e.save_preset(ghost, false).await, Err(crate::EngineError::Missing { .. })));
    e.delete_preset(&saved.id).await.unwrap();
    assert!(e.delete_preset(&saved.id).await.is_err());
    // A Twitch sign-in from before the editor: connected, but it can't edit.
    let acct = ferrocast_store::repo::settings::Account {
        platform: "twitch".into(),
        user_id: "1".into(),
        login: "me".into(),
        display_name: "Me".into(),
        avatar_url: None,
        scopes: vec!["user:read:chat".into()],
        connected_at: 1,
    };
    e.db(move |s| ferrocast_store::repo::settings::save_account(s, &acct)).await.unwrap();
    e.register_connector(Fake::new(Platform::Twitch));
    let access = e.stream_info_access().await.unwrap();
    let t = access.iter().find(|a| a.platform == Platform::Twitch).unwrap();
    assert!(t.connected && !t.can_edit);
    assert!(t.needs.as_deref().unwrap().contains("channel:manage:broadcast"));
    let y = access.iter().find(|a| a.platform == Platform::Youtube).unwrap();
    assert!(!y.connected && y.needs.as_deref().unwrap().starts_with("Connect YouTube"));
    assert_eq!(e.search_categories(Platform::Twitch, "valo").await.unwrap()[0].name, "valo result");
    assert!(matches!(e.search_categories(Platform::Twitch, " ").await, Err(crate::EngineError::Invalid(_))));
    assert!(matches!(
        e.search_categories(Platform::Kick, "x").await,
        Err(crate::EngineError::Connector(ConnectorError::NotConnected(_)))
    ));
}

// ---------------------------------------------------------------- show

#[tokio::test]
async fn start_show_runs_the_checklist_and_the_countdown_switches_scenes() {
    use ferrocast_core::stream_info::Preset;
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let preset = e
        .save_preset(
            Preset {
                id: String::new(),
                name: "Ranked".into(),
                info: StreamInfo { title: Some("Ranked night".into()), ..Default::default() },
                updated_at: 0,
            },
            true,
        )
        .await
        .unwrap();
    let cfg = json!({
        "preset_id": preset.id, "starting_scene": "Starting Soon", "live_scene": "Gameplay", "countdown_s": 10,
        "ending_scene": "Ending", "replay_buffer": true, "mic_sources": ["Mic"], "cam_sources": ["Webcam"], "discord": false,
        "recap_message": "Thanks {thanks}! ({follows} follows)"
    });
    e.patch_show_config(cfg.as_object().unwrap()).await.unwrap();
    // A muted mic fails the pre-flight; the one-click fix unmutes it.
    obs.lock().unwrap().inputs[0].muted = true;
    let pf = e.preflight().await;
    assert!(!pf.ready);
    let mic = pf.checks.iter().find(|c| c.id == "mic:Mic").unwrap();
    assert_eq!(mic.fix.as_ref().unwrap().href, "action:unmute:Mic");
    assert!(matches!(e.start_show(false).await, Err(crate::EngineError::Refused { code: "preflight_failed", .. })));
    let pf = e.preflight_fix("action:unmute:Mic".trim_start_matches("action:")).await.unwrap();
    assert!(pf.ready, "{:?}", pf.checks);
    assert!(!obs.lock().unwrap().inputs[0].muted);
    e.queue_pause().await;
    let run = e.start_show(false).await.unwrap();
    assert_eq!(run.phase, super::show::Phase::Countdown);
    let ids: Vec<(&str, &str)> = run.steps.iter().map(|s| (s.id.as_str(), s.status.as_str())).collect();
    assert_eq!(
        ids,
        vec![
            ("stream_info", "done"),
            ("replay", "done"),
            ("starting_scene", "done"),
            ("alerts", "done"),
            ("bot", "done"),
            ("countdown", "waiting")
        ]
    );
    assert_eq!(tw.infos.lock().unwrap()[0].title.as_deref(), Some("Ranked night"));
    assert!(!e.queue().await.paused, "alerts back on");
    assert!(obs.lock().unwrap().replay_active);
    assert_eq!(obs.lock().unwrap().current, "Starting Soon");
    assert!(matches!(e.start_show(true).await, Err(crate::EngineError::Conflict(_))), "one show at a time");
    let live = e.skip_countdown().await.unwrap();
    assert_eq!(live.phase, super::show::Phase::Live);
    assert_eq!(obs.lock().unwrap().current, "Gameplay");
    assert!(matches!(e.skip_countdown().await, Err(crate::EngineError::Missing { code: "no_countdown", .. })));
    let ended = e.end_show().await.unwrap();
    assert_eq!(ended.phase, super::show::Phase::Ended);
    assert_eq!(obs.lock().unwrap().current, "Ending");
    said_eventually(&tw, "Thanks everyone who stopped by! (0 follows)").await;
    // A new show can start once the last one ended.
    assert!(e.start_show(true).await.is_ok());
}

#[tokio::test]
async fn the_countdown_reaches_zero_by_itself() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    e.patch_show_config(json!({ "live_scene": "Gameplay", "countdown_s": 10, "discord": false }).as_object().unwrap())
        .await
        .unwrap();
    let run = e.start_show(true).await.unwrap();
    assert_eq!(run.phase, super::show::Phase::Countdown);
    // Pull the end in so the test doesn't wait 10 s: the loop wakes on the change.
    {
        let mut s = e.studio().show.lock().unwrap();
        if let Some(r) = s.as_mut() {
            r.countdown_ends_at = Some(crate::now().0 + 50);
        }
    }
    e.studio().show_wake.notify_one();
    eventually(|| obs.lock().unwrap().current == "Gameplay").await;
    eventually(|| e.show_run().is_some_and(|r| r.phase == super::show::Phase::Live)).await;
    let c = e.start_show(true).await;
    assert!(c.is_err(), "still live");
    // Cancel works only during a countdown.
    assert!(e.cancel_countdown().is_err());
}

#[tokio::test]
async fn with_obs_closed_the_preflight_says_so_and_steps_fail_cleanly() {
    let (e, _d) = engine().await;
    without_obs(&e).await;
    e.patch_show_config(
        json!({ "starting_scene": "Starting Soon", "countdown_s": 0, "discord": true }).as_object().unwrap(),
    )
    .await
    .unwrap();
    let pf = e.preflight().await;
    let obs = pf.checks.iter().find(|c| c.id == "obs").unwrap();
    assert_eq!(obs.status, ferrocast_core::show::CheckStatus::Fail);
    assert_eq!(obs.fix.as_ref().unwrap().href, "#/settings/obs");
    assert!(pf.checks.iter().any(|c| c.id == "discord"), "no webhook is a warning");
    assert!(pf.checks.iter().any(|c| c.id == "platforms"));
    let run = e.start_show(true).await.unwrap();
    let scene = run.steps.iter().find(|s| s.id == "starting_scene").unwrap();
    assert_eq!(scene.status, "failed");
    assert!(scene.detail.contains("OBS isn't running"), "{}", scene.detail);
    assert_eq!(run.steps.iter().find(|s| s.id == "discord").unwrap().status, "skipped");
    assert!(e.patch_show_config(json!({ "countdown_s": 5 }).as_object().unwrap()).await.is_err());
    assert!(e.patch_show_config(json!({ "preset_id": "ghost" }).as_object().unwrap()).await.is_err());
}

// ---------------------------------------------------------------- OBS helpers

#[tokio::test]
async fn scenes_sources_brb_and_chat_switching() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let scenes = e.obs_scenes().await.unwrap();
    assert_eq!(scenes.scenes.len(), 4);
    e.switch_scene("Gameplay").await.unwrap();
    assert!(matches!(
        e.switch_scene("Nope").await,
        Err(crate::EngineError::Refused { status: 422, code: "obs_refused", .. })
    ));
    assert!(matches!(e.switch_scene("").await, Err(crate::EngineError::Invalid(_))));
    let cam = scenes.scenes[1].items.iter().find(|i| i.source == "Webcam").unwrap().id;
    e.set_source_visible("Gameplay", cam, false).await.unwrap();
    assert!(!obs.lock().unwrap().scenes[1].items[1].enabled);
    // BRB: no scene picked yet.
    assert!(matches!(e.set_brb(true).await, Err(crate::EngineError::Refused { code: "no_brb_scene", .. })));
    e.patch_scenes_config(
        json!({ "brb_scene": "BRB", "brb_mute": ["Mic"], "chat_enabled": true, "chat_allowed": ["Gameplay", "Just Chatting"] }).as_object().unwrap(),
    )
    .await
    .unwrap();
    e.set_brb(true).await.unwrap();
    assert_eq!(obs.lock().unwrap().current, "BRB");
    assert!(obs.lock().unwrap().inputs[0].muted);
    e.set_brb(false).await.unwrap();
    assert_eq!(obs.lock().unwrap().current, "Gameplay", "back where we were");
    assert!(!obs.lock().unwrap().inputs[0].muted);
    // !scene from mods, only allowed scenes; viewers are ignored.
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&m, "!scene ending")).await;
    said_eventually(&tw, "\"ending\" isn't a scene mods can switch to.").await;
    e.ingest(chat_as(&person(Platform::Twitch, "V", Roles::empty()), "!scene just")).await;
    e.ingest(chat_as(&m, "!brb")).await;
    eventually(|| obs.lock().unwrap().current == "BRB").await;
    e.ingest(chat_as(&m, "!back")).await;
    eventually(|| obs.lock().unwrap().current == "Gameplay").await;
    assert!(!obs.lock().unwrap().switches.iter().any(|s| s == "Just Chatting"), "the viewer's !scene did nothing");
    assert!(
        e.patch_scenes_config(json!({ "brb_on_ads": true, "brb_scene": null }).as_object().unwrap()).await.is_err()
    );
}

#[tokio::test]
async fn clip_that_saves_the_replay_buffer_with_a_marker() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let err = e.clip_that(None).await.unwrap_err();
    assert!(err.to_string().contains("replay buffer isn't running"), "{err}");
    obs.lock().unwrap().replay_active = true;
    let c = e.clip_that(Some("Ace!")).await.unwrap();
    assert!(c.saved && c.marker.clip);
    assert_eq!(c.marker.kind, MomentKind::Clip);
    assert_eq!(c.path.as_deref(), Some("/videos/Replay 1.mkv"));
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&m, "!clip")).await;
    said_eventually(&tw, "Clipped!").await;
    assert_eq!(obs.lock().unwrap().replay_saves, 2);
    e.ingest(chat_as(&m, "!clip")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(obs.lock().unwrap().replay_saves, 2, "!clip has a cooldown");
    let markers = e.markers(Some(0)).await.unwrap();
    assert_eq!(markers.iter().filter(|m| m.kind == MomentKind::Clip).count(), 2);
}

#[tokio::test]
async fn health_turns_obs_numbers_into_warnings_and_the_watcher_runs() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    let first = e.check_obs_health().await.unwrap();
    assert!(first.warnings.is_empty());
    {
        let mut s = obs.lock().unwrap();
        s.streaming = true;
        s.stream_total = 1000;
        s.stream_skipped = 100;
        s.cpu = 95.0;
    }
    let h = e.check_obs_health().await.unwrap();
    let ids: Vec<&str> = h.warnings.iter().map(|w| w.id.as_str()).collect();
    assert!(ids.contains(&"dropped") && ids.contains(&"cpu"), "{ids:?}");
    assert_eq!(e.obs_health().unwrap().dropped_pct, 10.0);
    let mut live = e.subscribe_live();
    e.watch_obs_health();
    e.watch_obs_health(); // once per engine
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let f = tokio::time::timeout_at(deadline, live.recv()).await.expect("no health frame").unwrap();
        if f.text.contains("\"type\":\"obs_health\"") {
            break;
        }
    }
    without_obs(&e).await;
    assert!(matches!(e.check_obs_health().await, Err(crate::EngineError::Refused { code: "obs_unreachable", .. })));
}

#[tokio::test]
async fn an_ad_break_switches_to_brb_when_asked() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    e.switch_scene("Gameplay").await.unwrap();
    e.patch_scenes_config(json!({ "brb_scene": "BRB", "brb_on_ads": true }).as_object().unwrap()).await.unwrap();
    e.ingest(Event::live(Platform::Twitch, Some("ad-1".into()), Millis(0), EventKind::AdBreak { seconds: 30 })).await;
    eventually(|| obs.lock().unwrap().current == "BRB").await;
}

// ---------------------------------------------------------------- moments

fn tip(minor: i64, id: &str) -> Event {
    Event::live(
        Platform::Tips,
        Some(id.into()),
        Millis(0),
        EventKind::Tip {
            actor: Actor::named(Platform::Tips, "Asha"),
            money: Money::new(minor, "INR"),
            message: Some("love the stream".into()),
            provider: TipProvider::Razorpay,
            payment_ref: id.into(),
            status: PaymentStatus::Captured,
        },
    )
}

#[tokio::test]
async fn big_moments_pin_in_the_lane_and_mark_the_timeline() {
    let (e, _d) = engine().await;
    e.patch_settings(json!({ "currency": "INR" }).as_object().unwrap()).await.unwrap();
    let mut live = e.subscribe_live();
    go_live(&e).await;
    e.ingest(tip(10_000, "small")).await;
    assert!(e.lane().is_empty(), "₹100 is below the ₹500 default");
    e.ingest(tip(200_000, "big")).await;
    let lane = e.lane();
    assert_eq!(lane.len(), 1);
    assert_eq!(lane[0].title, "Asha tipped ₹2,000");
    assert_eq!(lane[0].kind, MomentKind::Tip);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, live.recv()).await.expect("no lane frame").unwrap();
        if f.text.contains("\"type\":\"lane\"") {
            let v: Value = serde_json::from_str(&f.text).unwrap();
            assert_eq!(v["data"]["added"]["title"], "Asha tipped ₹2,000");
            assert_eq!(v["data"]["sound"], "chime");
            break;
        }
    }
    let markers = e.markers(None).await.unwrap();
    assert_eq!(markers.len(), 1);
    assert!(markers[0].offset_ms.is_some(), "tied to the live stream");
    // A first-time chatter is pinned but not marked.
    let mut newbie = person(Platform::Twitch, "Newbie", Roles::FIRST_TIME);
    newbie.id = Some("newbie-1".into());
    e.ingest(chat_as(&newbie, "hello!")).await;
    assert_eq!(e.lane()[0].kind, MomentKind::FirstChatter);
    assert_eq!(e.markers(None).await.unwrap().len(), 1);
    // Dismiss one, then everything.
    let id = e.lane()[0].id.clone();
    assert_eq!(e.dismiss_lane(&id).unwrap().len(), 1);
    assert!(matches!(e.dismiss_lane(&id), Err(crate::EngineError::Missing { code: "lane_item_not_found", .. })));
    assert!(e.dismiss_all_lane().is_empty());
    // Tests never pin.
    let mut t = tip(900_000, "test");
    t.origin = Origin::Test;
    e.ingest(t).await;
    assert!(e.lane().is_empty());
}

#[tokio::test]
async fn markers_export_as_csv_and_text() {
    let (e, _d) = engine().await;
    e.add_marker_now(MomentKind::Manual, "=cmd|' /C calc'!A0", None, false).await.unwrap();
    e.add_marker_now(MomentKind::Raid, "Bo raided", Some(Platform::Twitch), true).await.unwrap();
    let (ct, name, body) = e.export_markers("csv", Some(0)).await.unwrap();
    assert!(ct.starts_with("text/csv"));
    assert!(name.starts_with("ferrocast-markers-") && name.ends_with(".csv"));
    assert!(body.contains("\"'=cmd|' /C calc'!A0\""), "formulas are defused: {body}");
    let (ct, _, body) = e.export_markers("txt", Some(0)).await.unwrap();
    assert!(ct.starts_with("text/plain"));
    assert!(body.contains("Bo raided [clip saved]"));
    assert!(matches!(e.export_markers("pdf", None).await, Err(crate::EngineError::Invalid(_))));
    assert!(matches!(e.markers(Some(crate::now().0 + 60_000)).await, Err(crate::EngineError::Invalid(_))));
    let id = e.markers(Some(0)).await.unwrap()[0].id;
    e.delete_marker(id).await.unwrap();
    assert!(matches!(e.delete_marker(id).await, Err(crate::EngineError::Missing { code: "marker_not_found", .. })));
    let err = e.patch_moments_config(json!({ "sound": "airhorn" }).as_object().unwrap()).await.unwrap_err();
    assert!(err.to_string().starts_with("sound:"));
}

#[tokio::test]
async fn a_chat_spike_while_live_is_marked() {
    let (e, _d) = engine().await;
    go_live(&e).await;
    e.patch_moments_config(json!({ "spike_min": 5, "spike_factor": 2 }).as_object().unwrap()).await.unwrap();
    // Seed a quiet minute directly into the meter, then a burst through the pipeline.
    {
        let mut m = e.studio().spike.lock().unwrap();
        let t = crate::now().0;
        for i in 0..12 {
            m.message(t - 60_000 + i * 5_000, 2, 5);
        }
    }
    let fan = person(Platform::Twitch, "Fan", Roles::empty());
    for i in 0..12 {
        e.ingest(chat_as(&fan, &format!("pog {i}"))).await;
    }
    let markers = e.markers(None).await.unwrap();
    assert!(markers.iter().any(|m| m.kind == MomentKind::ChatSpike), "{markers:?}");
}

#[tokio::test]
async fn a_mod_changes_the_title_from_chat_in_the_background() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let m = person(Platform::Twitch, "Mod", Roles::MODERATOR);
    e.ingest(chat_as(&m, "!title Speedrun night")).await;
    said_eventually(&tw, "Title updated on Twitch.").await;
    assert_eq!(tw.infos.lock().unwrap()[0].title.as_deref(), Some("Speedrun night"));
    let v = person(Platform::Twitch, "Viewer", Roles::empty());
    e.ingest(chat_as(&v, "!title")).await;
    said_eventually(&tw, "Title: Speedrun night").await;
    // Twice the same platform is one push.
    let r = e
        .apply_stream_info(
            StreamInfo { title: Some("x".into()), ..Default::default() },
            Some(vec![Platform::Twitch, Platform::Kick, Platform::Twitch]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(r.results.len(), 2);
}

#[tokio::test]
async fn description_and_thumbnail_go_to_youtube_and_are_skipped_elsewhere() {
    let (e, _d) = engine().await;
    let tw = Fake::new(Platform::Twitch);
    let yt = Fake::new(Platform::Youtube);
    e.register_connector(tw.clone());
    e.register_connector(yt.clone());
    let png = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0u8; 64]].concat();
    let media = e.db(move |s| Ok(ferrocast_store::repo::media::save(s, "thumb.png", &png).unwrap())).await.unwrap();
    // Only YouTube's fields: Twitch isn't called, and says why.
    let info = StreamInfo {
        description: Some("Ranked all night\n!discord".into()),
        thumbnail: Some(media.id.clone()),
        ..Default::default()
    };
    let r = e.apply_stream_info(info, None, None).await.unwrap();
    assert_eq!((r.ok, r.failed), (2, 0));
    let t = r.results.iter().find(|x| x.platform == Platform::Twitch).unwrap();
    assert!(t.applied.is_empty());
    assert!(t.skipped.iter().any(|s| s.starts_with("description")), "{:?}", t.skipped);
    assert!(t.skipped.iter().any(|s| s.starts_with("thumbnail")), "{:?}", t.skipped);
    assert!(tw.infos.lock().unwrap().is_empty(), "nothing Twitch takes, so no call");
    let y = r.results.iter().find(|x| x.platform == Platform::Youtube).unwrap();
    assert_eq!(y.applied, vec!["description", "thumbnail"]);
    assert_eq!(yt.infos.lock().unwrap()[0].description.as_deref(), Some("Ranked all night\n!discord"));
    assert!(yt.infos.lock().unwrap()[0].thumbnail.is_none(), "the id never reaches a platform");
    assert_eq!(yt.thumbs.lock().unwrap()[0], (72, "image/png".to_string()));
    // A title too: Twitch gets the title only.
    let info = StreamInfo { title: Some("Ranked".into()), description: Some("d".into()), ..Default::default() };
    e.apply_stream_info(info, None, None).await.unwrap();
    let sent = tw.infos.lock().unwrap()[0].clone();
    assert_eq!((sent.title.as_deref(), sent.description), (Some("Ranked"), None));
    // A picture that's gone: YouTube's thumbnail fails with the reason, alone it's an error.
    let info = StreamInfo { thumbnail: Some("gone".into()), ..Default::default() };
    let r = e.apply_stream_info(info, Some(vec![Platform::Youtube]), None).await.unwrap();
    let y = &r.results[0];
    assert!(!y.ok);
    assert!(y.error.as_ref().unwrap().message.contains("media library"), "{:?}", y.error);
    // With a title it still counts, and says what happened to the picture.
    let info = StreamInfo { title: Some("t".into()), thumbnail: Some("gone".into()), ..Default::default() };
    let r = e.apply_stream_info(info, Some(vec![Platform::Youtube]), None).await.unwrap();
    assert!(r.results[0].ok);
    assert!(r.results[0].skipped.iter().any(|s| s.starts_with("thumbnail:")), "{:?}", r.results[0].skipped);
    // Validation.
    let long = StreamInfo { description: Some("x".repeat(5001)), ..Default::default() };
    assert!(matches!(e.apply_stream_info(long, None, None).await, Err(crate::EngineError::Invalid(_))));
    let angle = StreamInfo { description: Some("<b>hi</b>".into()), ..Default::default() };
    assert!(matches!(e.apply_stream_info(angle, None, None).await, Err(crate::EngineError::Invalid(_))));
}

#[tokio::test]
async fn the_show_can_start_and_stop_streaming_in_obs() {
    let (e, _d) = engine().await;
    let obs = with_obs(&e).await;
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    let cfg = json!({ "start_stream": true, "stop_stream": true, "stop_delay_s": 0, "discord": false, "recap_message": null });
    e.patch_show_config(cfg.as_object().unwrap()).await.unwrap();
    let run = e.start_show(true).await.unwrap();
    let s = run.steps.iter().find(|s| s.id == "stream").unwrap();
    assert_eq!(s.status, "done", "{s:?}");
    assert!(obs.lock().unwrap().streaming);
    assert!(e.obs_streaming().await.unwrap());
    let ended = e.end_show().await.unwrap();
    assert_eq!(ended.steps.iter().find(|s| s.id == "stop_stream").unwrap().status, "done");
    assert!(!obs.lock().unwrap().streaming);
    // With a delay it waits, then stops.
    let cfg = json!({ "stop_delay_s": 1 });
    e.patch_show_config(cfg.as_object().unwrap()).await.unwrap();
    e.start_show(true).await.unwrap();
    let ended = e.end_show().await.unwrap();
    assert_eq!(ended.steps.iter().find(|s| s.id == "stop_stream").unwrap().status, "waiting");
    assert!(obs.lock().unwrap().streaming, "not yet");
    eventually(|| !obs.lock().unwrap().streaming).await;
    eventually(|| e.show_run().unwrap().steps.iter().any(|s| s.id == "stop_stream" && s.status == "done")).await;
    // Going live again before the delay keeps the new stream running.
    let cfg = json!({ "stop_delay_s": 2 });
    e.patch_show_config(cfg.as_object().unwrap()).await.unwrap();
    e.start_show(true).await.unwrap();
    e.end_show().await.unwrap();
    e.start_show(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(obs.lock().unwrap().streaming, "the old show's stop didn't touch the new stream");
    // The plain buttons, and OBS without a stream key.
    assert!(!e.obs_set_streaming(false).await.unwrap());
    assert!(e.obs_set_streaming(true).await.unwrap());
    e.obs_set_streaming(false).await.unwrap();
    obs.lock().unwrap().stream_unconfigured = true;
    let err = e.obs_set_streaming(true).await.unwrap_err().to_string();
    assert!(err.contains("stream key"), "{err}");
    assert!(matches!(
        e.patch_show_config(json!({ "stop_delay_s": 601 }).as_object().unwrap()).await,
        Err(crate::EngineError::Invalid(_))
    ));
}
