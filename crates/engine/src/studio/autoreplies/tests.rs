//! Auto-replies through the real engine: chat in, replies out (a fake platform records
//! them). Covers the loop guard, cooldowns, live-only, panic, bot off, commands winning,
//! the send budget and the platform's own rate limit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrocast_connector::{Connector, ConnectorError};
use ferrocast_core::autoreply::{AutoReply, MatchKind};
use ferrocast_core::event::*;
use ferrocast_core::moderation::{Capabilities, ModeChange};

use crate::{Engine, EngineError};

#[derive(Debug, Default)]
struct Fake {
    platform: Option<Platform>,
    said: Mutex<Vec<String>>,
    /// Answer every send with "rate limited".
    limited: AtomicBool,
    tries: Mutex<u32>,
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
        *self.tries.lock().unwrap() += 1;
        if self.limited.load(Ordering::SeqCst) {
            return Err(ConnectorError::RateLimited {
                platform: self.platform(),
                retry_after: Duration::from_secs(30),
            });
        }
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
}

async fn engine() -> (Engine, tempfile::TempDir, Arc<Fake>) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    let tw = Fake::new(Platform::Twitch);
    e.register_connector(tw.clone());
    (e, dir, tw)
}

fn person(name: &str, roles: Roles) -> Actor {
    let mut a = Actor::named(Platform::Twitch, name);
    a.id = Some(format!("{}-id", name.to_lowercase()));
    a.roles = roles;
    a
}

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn chat_as(a: &Actor, text: &str) -> Event {
    let n = N.fetch_add(1, Ordering::SeqCst);
    Event::live(
        a.platform,
        Some(format!("ar-{}-{n}", a.display_name)),
        Millis(0),
        EventKind::ChatMessage { actor: a.clone(), content: Content::plain(text), reply_to: None, is_action: false },
    )
}

async fn go_live(e: &Engine) {
    let n = N.fetch_add(1, Ordering::SeqCst);
    e.ingest(Event::live(
        Platform::Twitch,
        Some(format!("online-{n}")),
        Millis(0),
        EventKind::StreamOnline { title: None },
    ))
    .await;
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

/// Gives spawned sends time to land, then returns what was said.
async fn settle(fake: &Fake) -> Vec<String> {
    tokio::time::sleep(Duration::from_millis(80)).await;
    fake.said()
}

fn discord() -> AutoReply {
    AutoReply::new("Discord", MatchKind::Word, "discord", "Join us, {user}: discord.gg/x")
}

#[tokio::test]
async fn answers_live_chat_once_then_cools_down() {
    let (e, _d, tw) = engine().await;
    e.create_autoreply(discord()).await.unwrap();
    let fan = person("Fan", Roles::empty());
    // Offline: only_when_live keeps it quiet.
    e.ingest(chat_as(&fan, "is there a discord?")).await;
    assert!(settle(&tw).await.is_empty(), "offline");
    go_live(&e).await;
    e.ingest(chat_as(&fan, "is there a DISCORD?")).await;
    said_eventually(&tw, "Join us, Fan: discord.gg/x").await;
    // Global cooldown (30 s): someone else right away gets nothing.
    e.ingest(chat_as(&person("Other", Roles::empty()), "discord pls")).await;
    assert_eq!(settle(&tw).await.len(), 1, "cooldown");
    // Not a whole word: no match at all.
    e.ingest(chat_as(&person("Third", Roles::empty()), "discordant")).await;
    assert_eq!(settle(&tw).await.len(), 1);
    // Editing the rule starts its cooldowns fresh.
    let id = e.autoreplies()[0].id.clone();
    let mut r = discord();
    r.reply = "New link: discord.gg/y".into();
    e.update_autoreply(&id, r).await.unwrap();
    e.ingest(chat_as(&person("Other", Roles::empty()), "discord pls")).await;
    said_eventually(&tw, "New link: discord.gg/y").await;
}

#[tokio::test]
async fn loop_guard_ignores_the_streamer_bots_and_echoes() {
    let (e, _d, tw) = engine().await;
    let mut r = discord();
    r.cooldown_s = 0;
    r.user_cooldown_s = 0;
    r.only_when_live = false;
    e.create_autoreply(r).await.unwrap();
    // The streamer's own account is the bot (R-28).
    e.ingest(chat_as(&person("Streamer", Roles::BROADCASTER), "discord")).await;
    // A bot badge, or a well-known bot by name.
    e.ingest(chat_as(&person("Helper", Roles::BOT), "discord")).await;
    e.ingest(chat_as(&person("Nightbot", Roles::empty()), "discord")).await;
    // The connected account (a separate bot account) speaking.
    if let Some(c) = e.state_mut().connectors.get_mut("twitch") {
        c.account = Some(crate::state::AccountView { name: "MyBotAccount".into(), avatar_url: None });
    }
    e.ingest(chat_as(&person("mybotaccount", Roles::empty()), "discord")).await;
    assert!(settle(&tw).await.is_empty(), "never answers itself or bots: {:?}", tw.said());

    // A real viewer gets an answer; the same text coming back (the bot's own message, or
    // another account copying it) is never answered, so two bots can't ping-pong.
    e.ingest(chat_as(&person("Fan", Roles::empty()), "discord?")).await;
    said_eventually(&tw, "Join us, Fan").await;
    tokio::time::sleep(Duration::from_millis(5_100)).await; // past the 5 s floor
    e.ingest(chat_as(&person("Copycat", Roles::empty()), "Join us, Fan: discord.gg/x")).await;
    assert_eq!(settle(&tw).await.len(), 1, "echo guard");
}

#[tokio::test]
async fn commands_win_and_panic_or_bot_off_silence_it() {
    let (e, _d, tw) = engine().await;
    let mut r = discord();
    r.matcher.kind = MatchKind::Contains;
    r.cooldown_s = 0;
    r.user_cooldown_s = 0;
    r.only_when_live = false;
    e.create_autoreply(r).await.unwrap();
    let cmd = ferrocast_core::command::Command {
        id: String::new(),
        name: "!discord".into(),
        aliases: vec![],
        response: "the command".into(),
        access: ferrocast_core::command::Access::Everyone,
        cooldown_s: 0,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    };
    e.save_command(ferrocast_core::command::Command { id: "c1".into(), ..cmd }).await.unwrap();
    let fan = person("Fan", Roles::empty());
    e.ingest(chat_as(&fan, "!discord")).await;
    said_eventually(&tw, "the command").await;
    assert_eq!(settle(&tw).await, vec!["the command".to_string()], "the command answered, not the auto-reply");
    // A built-in command (counters, modules) also wins.
    e.ingest(chat_as(&fan, "!8ball discord?")).await;
    said_eventually(&tw, "@Fan, ").await;
    assert!(!settle(&tw).await.iter().any(|s| s.starts_with("Join us")));

    // Panic mode: quiet.
    e.panic(true).await.unwrap();
    e.ingest(chat_as(&person("A", Roles::empty()), "discord")).await;
    assert!(!settle(&tw).await.iter().any(|s| s.starts_with("Join us")), "panic");
    e.panic(false).await.unwrap();
    // Bot switched off: quiet.
    let mut s = e.settings();
    s.bot_enabled = false;
    e.save_settings(s.clone()).await.unwrap();
    e.ingest(chat_as(&person("B", Roles::empty()), "discord")).await;
    assert!(!settle(&tw).await.iter().any(|s| s.starts_with("Join us")), "bot off");
    s.bot_enabled = true;
    e.save_settings(s).await.unwrap();
    e.ingest(chat_as(&person("C", Roles::empty()), "discord")).await;
    said_eventually(&tw, "Join us, C").await;
}

#[tokio::test]
async fn shared_chat_and_replayed_history_get_no_answer() {
    let (e, _d, tw) = engine().await;
    let mut r = discord();
    r.only_when_live = false;
    e.create_autoreply(r).await.unwrap();
    // Shared Chat from a partner channel never makes the bot speak (R-11).
    let mut shared = chat_as(&person("Partner", Roles::empty()), "discord");
    shared.channel = Some(ChannelRef { platform: Platform::Twitch, id: "other".into(), name: "other".into() });
    e.ingest(shared).await;
    // Replayed history neither.
    let mut old = chat_as(&person("Old", Roles::empty()), "discord");
    old.origin = Origin::Backfill;
    e.ingest(old).await;
    assert!(settle(&tw).await.is_empty());
}

#[tokio::test]
async fn first_rule_wins_roles_and_order() {
    let (e, _d, tw) = engine().await;
    go_live(&e).await;
    let mut subs = AutoReply::new("Subs", MatchKind::Contains, "emote", "Sub emotes: see panels");
    subs.min_role = ferrocast_core::command::Access::Subs;
    let general = AutoReply::new("Anyone", MatchKind::Contains, "emote", "Emotes are 7TV");
    let a = e.create_autoreply(subs).await.unwrap();
    let b = e.create_autoreply(general).await.unwrap();
    e.ingest(chat_as(&person("Viewer", Roles::empty()), "emote?")).await;
    said_eventually(&tw, "Emotes are 7TV").await;
    e.ingest(chat_as(&person("Sub", Roles::SUBSCRIBER), "emote?")).await;
    said_eventually(&tw, "Sub emotes").await;
    // Reorder: every id once, else 409.
    let list = e.reorder_autoreplies(vec![b.id.clone(), a.id.clone()]).await.unwrap();
    assert_eq!(list.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), [b.id.as_str(), a.id.as_str()]);
    let err = e.reorder_autoreplies(vec![b.id.clone()]).await.unwrap_err();
    assert!(matches!(err, EngineError::Store(ferrocast_store::StoreError::Conflict(_))), "{err:?}");
}

#[tokio::test]
async fn the_send_budget_and_platform_rate_limits_hold_back_replies() {
    let (e, _d, tw) = engine().await;
    let mut r = discord();
    r.cooldown_s = 0;
    r.user_cooldown_s = 0;
    r.only_when_live = false;
    e.create_autoreply(r).await.unwrap();
    // Several rules, so one rule's 5 s floor doesn't hide the budget.
    for i in 0..7 {
        let mut x = AutoReply::new(&format!("r{i}"), MatchKind::Word, &format!("word{i}"), &format!("reply {i}"));
        x.only_when_live = false;
        e.create_autoreply(x).await.unwrap();
    }
    for i in 0..7 {
        e.ingest(chat_as(&person(&format!("V{i}"), Roles::empty()), &format!("word{i}"))).await;
    }
    let said = settle(&tw).await;
    assert_eq!(said.len(), 6, "six per 30 s on Twitch: {said:?}");

    // The platform says "slow down": auto-replies pause there.
    let (e2, _d2, tw2) = engine().await;
    let mut r = discord();
    r.only_when_live = false;
    e2.create_autoreply(r).await.unwrap();
    let mut x = AutoReply::new("x", MatchKind::Word, "hello", "hi");
    x.only_when_live = false;
    e2.create_autoreply(x).await.unwrap();
    tw2.limited.store(true, Ordering::SeqCst);
    e2.ingest(chat_as(&person("A", Roles::empty()), "discord")).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(*tw2.tries.lock().unwrap(), 1);
    tw2.limited.store(false, Ordering::SeqCst);
    e2.ingest(chat_as(&person("B", Roles::empty()), "hello")).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(*tw2.tries.lock().unwrap(), 1, "paused after the platform's rate limit");
    assert!(tw2.said().is_empty());
}

#[tokio::test]
async fn validation_limits_and_try_it() {
    let (e, _d, _tw) = engine().await;
    let mut bad = discord();
    bad.matcher.kind = MatchKind::Regex;
    bad.matcher.text = "(discord".into();
    match e.create_autoreply(bad.clone()).await.unwrap_err() {
        EngineError::Refused { status: 422, code: "bad_pattern", message, .. } => {
            assert!(message.contains("character 1"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    let long = AutoReply { reply: "x".repeat(501), ..discord() };
    assert!(matches!(
        e.create_autoreply(long).await.unwrap_err(),
        EngineError::Refused { status: 413, code: "reply_too_long", .. }
    ));
    assert!(matches!(
        e.create_autoreply(AutoReply { name: "".into(), ..discord() }).await.unwrap_err(),
        EngineError::Invalid(m) if m.starts_with("name:")
    ));
    assert!(matches!(e.update_autoreply("nope", discord()).await.unwrap_err(), EngineError::Missing { .. }));
    assert!(matches!(e.delete_autoreply("nope").await.unwrap_err(), EngineError::Missing { .. }));

    // Try it: saved rules, or a draft.
    let saved = e.create_autoreply(discord()).await.unwrap();
    let t = e.try_autoreply("got a discord?", None, None).await.unwrap();
    assert!(t.matched);
    assert_eq!(t.rule_id.as_deref(), Some(saved.id.as_str()));
    assert_eq!(t.reply.as_deref(), Some("Join us, viewer: discord.gg/x"));
    assert!(!e.try_autoreply("hello", None, None).await.unwrap().matched);
    let draft = AutoReply::new("", MatchKind::Contains, "what game", "{user}, it's {game}");
    let t = e.try_autoreply("WHAT game is this", Some(draft.clone()), None).await.unwrap();
    assert_eq!(t.reply.as_deref(), Some("viewer, it's something fun"));
    let t = e.try_autoreply("what game", Some(AutoReply { reply: "".into(), ..draft }), None).await.unwrap();
    assert!(t.matched && t.reply.is_none(), "a draft without a reply still says whether it matches");
    assert!(matches!(
        e.try_autoreply("x", Some(bad), None).await.unwrap_err(),
        EngineError::Refused { status: 422, .. }
    ));

    // 100 at most.
    for i in 1..100 {
        e.create_autoreply(AutoReply::new(&format!("r{i}"), MatchKind::Contains, &format!("w{i}"), "x")).await.unwrap();
    }
    assert!(matches!(
        e.create_autoreply(discord()).await.unwrap_err(),
        EngineError::Refused { status: 422, code: "too_many_autoreplies", .. }
    ));
    // Kept across a restart, in order.
    assert_eq!(e.autoreplies().len(), 100);
}

#[tokio::test]
async fn rules_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
    {
        let e = Engine::start_with_secrets(dir.path(), secrets.clone()).await.unwrap();
        e.create_autoreply(discord()).await.unwrap();
        e.create_autoreply(AutoReply::new("b", MatchKind::Contains, "b", "b")).await.unwrap();
    }
    let e = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
    let names: Vec<String> = e.autoreplies().into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Discord", "b"]);
    assert!(e.try_autoreply("discord", None, None).await.unwrap().matched, "compiled on load");
}

#[tokio::test]
async fn an_import_brings_its_auto_replies_in_without_a_restart() {
    let (e, _d, _fake) = engine().await;
    let mut rule = AutoReply::new("Discord", MatchKind::Word, "discord", "discord.gg/example");
    rule.id = "ar-imported".into();
    // What an import or a cloud restore does: rows land in the store, then the engine reloads.
    ferrocast_store::repo::autoreplies::save(&e.store(), &rule).unwrap();
    assert!(e.autoreplies().iter().all(|r| r.id != "ar-imported"));
    e.reload().await.unwrap();
    assert!(e.autoreplies().iter().any(|r| r.id == "ar-imported"));
}
