use ferrocast_core::command::Access;
use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis};
use ferrocast_store::import::{Kind, OnConflict, PreviewSection};
use ferrocast_store::repo;
use ferrocast_testkit::streamelements::{self as se, TOKEN};
use serde_json::Value;

use super::streamelements::{Raw, map};
use super::*;

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
        .await
        .unwrap();
    (e, dir)
}

fn section(p: &ImportPreview, kind: Kind) -> &PreviewSection {
    p.preview.sections.iter().find(|s| s.kind == kind).unwrap()
}

fn refused_status(e: EngineError) -> (u16, &'static str) {
    match e {
        EngineError::Refused { status, code, .. } => (status, code),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn raw_from_fixtures() -> Raw {
    let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
    Raw {
        platform: Some(Platform::Twitch),
        provider: "twitch".into(),
        commands: Some(v(se::COMMANDS)),
        timers: Some(v(se::TIMERS)),
        loyalty: Some(v(se::LOYALTY)),
        points: vec![serde_json::json!({ "username": "sam", "points": 120 }), serde_json::json!({ "points": 5 })],
        tips: v(se::TIPS)["docs"].as_array().unwrap().clone(),
        ..Default::default()
    }
}

#[test]
fn documented_shapes_map_to_ferrocast() {
    let inc = map(raw_from_fixtures());
    let discord = inc.commands.iter().find(|c| c.name == "!discord").unwrap();
    assert_eq!(discord.response, "Join the Discord, {user}: https://discord.gg/example");
    assert_eq!((discord.cooldown_s, discord.user_cooldown_s, discord.access), (5, 15, Access::Everyone));
    assert_eq!(discord.aliases, vec!["!dc".to_string()], "an alias with a space is left off");
    let flip = inc.commands.iter().find(|c| c.name == "!flip").unwrap();
    assert!(!flip.enabled, "a reply Ferrocast can't do is imported switched off");
    assert_eq!(flip.access, Access::Mods);
    let hi = inc.commands.iter().find(|c| c.name == "!hi").unwrap();
    assert_eq!((hi.access, hi.response.as_str()), (Access::Vips, "Hi {target}!"));
    assert!(inc.skipped.iter().any(|(k, i)| *k == Kind::Commands && i.name == "!has space"));
    assert!(inc.skipped.iter().any(|(k, i)| *k == Kind::Commands && i.name == "!empty"));
    let notes: Vec<&str> = inc.notes.iter().map(|(_, n)| n.as_str()).collect();
    assert!(notes.iter().any(|n| n.contains("Regulars-only")));
    assert!(notes.iter().any(|n| n.contains("whispered")));
    assert!(notes.iter().any(|n| n.contains("free to use")));

    let socials = inc.timers.iter().find(|t| t.name == "socials").unwrap();
    assert_eq!((socials.every_min, socials.min_chat_lines, socials.enabled), (15, 5, true));
    assert_eq!(inc.timers.iter().find(|t| t.name == "fast").unwrap().every_min, 5);
    assert!(!inc.timers.iter().find(|t| t.name == "offline only").unwrap().enabled);
    assert!(inc.skipped.iter().any(|(k, i)| *k == Kind::Timers && i.name == "nothing"));

    let l = inc.loyalty.as_ref().unwrap();
    assert_eq!(
        (l["name"].as_str(), l["per_interval"].as_u64(), l["interval_min"].as_u64()),
        (Some("coins"), Some(5), Some(10))
    );
    assert_eq!((l["sub_bonus"].as_u64(), l["raid_bonus"].as_u64()), (Some(500), Some(300)));

    assert_eq!(inc.points.len(), 1);
    assert_eq!(inc.tips.len(), 2, "the refunded tip is skipped");
    let usd = inc.tips.iter().find(|t| t.currency == "USD").unwrap();
    assert_eq!((usd.amount_minor, usd.occurred_at), (1250, 1_544_203_141_957));
    assert_eq!(inc.tips.iter().find(|t| t.currency == "INR").unwrap().amount_minor, 50_000);
}

#[test]
fn tokens_are_checked_before_they_leave_the_computer() {
    assert_eq!(streamelements::check_token(&format!("  Bearer {TOKEN} ")).unwrap(), TOKEN);
    for bad in ["", "abc", "not a jwt at all with spaces.x.y", "aaaaaaaaaaaaaaaaaaaaaaa", "a..b"] {
        assert!(matches!(streamelements::check_token(bad), Err(EngineError::Invalid(m)) if m.starts_with("token:")));
    }
}

#[tokio::test]
async fn preview_then_apply_against_a_fake_streamelements() {
    let fake = se::fake().await;
    fake.set(|s| s.points = 2_345);
    let (e, dir) = engine().await;
    e.set_streamelements_api(&fake.base);
    // Someone Ferrocast already knows, with points of their own.
    let mut a = Actor::named(Platform::Twitch, "viewer1");
    a.id = Some("t-1".into());
    a.login = Some("viewer1".into());
    let kind = EventKind::ChatMessage { actor: a, content: Content::plain("hi"), reply_to: None, is_action: false };
    e.ingest(Event::live(Platform::Twitch, Some("m1".into()), Millis(0), kind)).await;
    e.db(|s| repo::loyalty::change(s, Platform::Twitch, "t-1", 7, repo::loyalty::Floor::Strict)).await.unwrap();

    let p = e.preview_streamelements(TOKEN).await.unwrap();
    assert_eq!(p.account.as_ref().map(|a| a.name.as_str()), Some("FixtureStreamer"));
    let pts = section(&p, Kind::Points);
    assert_eq!((pts.found, pts.conflicts, pts.waiting), (2_345, 1, 2_344), "paged 1,000 at a time");
    assert_eq!(section(&p, Kind::Commands).new, 3);
    assert_eq!(section(&p, Kind::Tips).new, 2);
    assert!(p.preview.not_imported.iter().any(|n| n.what.starts_with("Overlays") && n.count == 2));
    let reqs = fake.requests();
    assert!(reqs.iter().all(|(m, _)| m == "GET"), "only GET requests, ever");
    assert_eq!(reqs.iter().filter(|(_, p)| p.contains("/points/")).count(), 3);
    assert!(e.commands().is_empty(), "a preview writes nothing");

    let r = e.apply_import(&p.preview_id, OnConflict::Keep).await.unwrap();
    assert!(r.sections.iter().any(|s| s.kind == Kind::Commands && s.added == 3));
    assert_eq!(e.commands().len(), 3, "the engine reloaded what was imported");
    assert_eq!(e.timers().len(), 3);
    assert_eq!(e.loyalty().name, "coins");
    assert_eq!(e.points(Platform::Twitch, "t-1").await.unwrap(), 7, "kept Ferrocast's balance");
    assert!(dir.path().join("backups").join(format!("{}.db", r.backup)).exists());
    // The token is never written anywhere in the data folder.
    for entry in walk(dir.path()) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        assert!(!bytes.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()), "token found in {entry:?}");
    }
    // A preview can be applied once.
    assert!(matches!(
        e.apply_import(&p.preview_id, OnConflict::Keep).await,
        Err(EngineError::Missing { code: "preview_expired", .. })
    ));
    assert!(matches!(e.apply_import("nope", OnConflict::Keep).await, Err(EngineError::Missing { .. })));
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
    }
    out
}

#[tokio::test]
async fn upstream_failures_become_clear_errors() {
    let fake = se::fake().await;
    let (e, _d) = engine().await;
    e.set_streamelements_api(&fake.base);
    let other = "eyJhbGciOiJIUzI1NiJ9.eyJ3cm9uZyI6dHJ1ZX0.c2lnbmF0dXJlLXdyb25nLXdyb25n";
    assert_eq!(
        refused_status(e.preview_streamelements(other).await.unwrap_err()),
        (401, "streamelements_token_rejected")
    );
    fake.set(|s| {
        s.fail.insert("/bot/timers".into(), 503);
    });
    assert_eq!(refused_status(e.preview_streamelements(TOKEN).await.unwrap_err()), (502, "streamelements_unavailable"));
    fake.set(|s| {
        s.fail.clear();
        s.fail.insert("/points".into(), 429);
    });
    match e.preview_streamelements(TOKEN).await.unwrap_err() {
        EngineError::Refused { status: 429, retry_after: Some(7), .. } => {}
        other => panic!("{other:?}"),
    }
    fake.set(|s| {
        s.fail.clear();
        s.garbage = Some("/bot/commands".into());
    });
    assert_eq!(
        refused_status(e.preview_streamelements(TOKEN).await.unwrap_err()),
        (502, "streamelements_bad_response")
    );
    e.set_streamelements_api("http://127.0.0.1:9/kappa/v2");
    assert_eq!(refused_status(e.preview_streamelements(TOKEN).await.unwrap_err()), (502, "streamelements_unreachable"));
    // A failed preview leaves nothing to apply.
    assert!(e.inner.import.lock().unwrap().is_none());
}

#[tokio::test]
async fn an_empty_account_previews_cleanly() {
    let fake = se::fake().await;
    fake.set(|s| s.empty = true);
    let (e, _d) = engine().await;
    e.set_streamelements_api(&fake.base);
    let p = e.preview_streamelements(TOKEN).await.unwrap();
    assert!(p.preview.sections.iter().all(|s| s.found == 0 && s.new == 0), "{:?}", p.preview.sections);
    assert!(p.preview.not_imported.is_empty());
    let r = e.apply_import(&p.preview_id, OnConflict::Replace).await.unwrap();
    assert!(r.sections.iter().all(|s| s.added == 0));
}

#[tokio::test]
async fn streamlabs_files_preview_and_apply() {
    let (e, _d) = engine().await;
    let files = streamlabs::Files {
        platform: Some(Platform::Kick),
        points: Some(streamlabs::Upload {
            file_name: "Currency.csv".into(),
            bytes: b"Name,Points\nana,40\nbo,0\n".to_vec(),
        }),
        tips: None,
    };
    let p = e.preview_streamlabs(files).await.unwrap();
    assert_eq!(p.source, Source::Streamlabs);
    let pts = section(&p, Kind::Points);
    assert_eq!((pts.found, pts.waiting, pts.skipped), (2, 1, 1));
    e.apply_import(&p.preview_id, OnConflict::Keep).await.unwrap();
    assert_eq!(e.db(import::pending_count).await.unwrap(), 1);
}

#[tokio::test]
async fn youtube_names_from_a_file_need_a_match() {
    let (e, _d) = engine().await;
    let files = streamlabs::Files {
        platform: Some(Platform::Youtube),
        points: Some(streamlabs::Upload {
            file_name: "Currency.csv".into(),
            bytes: b"Name,Points\nAna Plays,40\n".to_vec(),
        }),
        tips: None,
    };
    let p = e.preview_streamlabs(files).await.unwrap();
    let pts = section(&p, Kind::Points);
    assert_eq!((pts.waiting, pts.needs_match), (0, 1));
    let r = e.apply_import(&p.preview_id, OnConflict::Keep).await.unwrap();
    assert_eq!(r.sections[0].needs_match, 1);
    // Someone called "Ana Plays" chatting later is not paid.
    let mut a = Actor::named(Platform::Youtube, "Ana Plays");
    a.id = Some("UCimpostor".into());
    let kind = EventKind::ChatMessage { actor: a, content: Content::plain("hi"), reply_to: None, is_action: false };
    e.ingest(Event::live(Platform::Youtube, Some("y1".into()), Millis(0), kind)).await;
    assert_eq!(e.points(Platform::Youtube, "UCimpostor").await.unwrap(), 0);
}
