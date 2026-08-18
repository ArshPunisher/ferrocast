use super::*;
use crate::repo;
use ferrocast_core::command::Access;
use ferrocast_core::event::Actor;
use serde_json::json;

fn store() -> (Store, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    (Store::open(d.path()).unwrap(), d)
}

fn cmd(name: &str, reply: &str) -> Command {
    Command {
        id: String::new(),
        name: name.into(),
        aliases: vec![],
        response: reply.into(),
        access: Access::Everyone,
        cooldown_s: 5,
        user_cooldown_s: 0,
        platforms: vec![],
        enabled: true,
    }
}

fn timer(name: &str, msg: &str) -> Timer {
    Timer {
        id: String::new(),
        name: name.into(),
        message: msg.into(),
        every_min: 15,
        min_chat_lines: 5,
        platforms: vec![],
        enabled: true,
    }
}

fn seen(s: &Store, p: Platform, id: &str, login: &str, display: &str) {
    let mut a = Actor::named(p, display);
    a.id = Some(id.into());
    a.login = Some(login.into());
    repo::viewers::touch(s, &a, true).unwrap();
}

fn incoming() -> Incoming {
    let mut inc = Incoming::new(Source::StreamElements, Some(Platform::Twitch));
    inc.kinds = vec![Kind::Commands, Kind::Timers, Kind::Points, Kind::LoyaltySettings, Kind::Tips];
    inc
}

fn section(p: &Preview, kind: Kind) -> &PreviewSection {
    p.sections.iter().find(|s| s.kind == kind).unwrap()
}

fn report_section(r: &Report, kind: Kind) -> &ReportSection {
    r.sections.iter().find(|s| s.kind == kind).unwrap()
}

#[test]
fn preview_writes_nothing_and_apply_does_what_it_promised() {
    let (s, d) = store();
    repo::bot::save_command(&s, &Command { id: "mine".into(), ..cmd("!discord", "my discord") }).unwrap();
    seen(&s, Platform::Twitch, "1", "sam", "Sam");
    repo::loyalty::change(&s, Platform::Twitch, "1", 40, repo::loyalty::Floor::Strict).unwrap();
    seen(&s, Platform::Twitch, "2", "ana", "Ana");

    let mut inc = incoming();
    inc.commands = vec![cmd("!discord", "theirs"), cmd("!lurk", "{user} lurks"), cmd("!LURK", "again")];
    inc.timers = vec![timer("socials", "Follow me")];
    inc.points = vec![
        PointsRow { name: "Sam".into(), user_id: None, points: 5_000 },
        PointsRow { name: "ana".into(), user_id: None, points: 300 },
        PointsRow { name: "newcomer".into(), user_id: None, points: 75 },
        PointsRow { name: "nightbot".into(), user_id: None, points: 10 },
        PointsRow { name: "zero".into(), user_id: None, points: 0 },
    ];
    inc.loyalty =
        Some(serde_json::from_value(json!({ "name": "coins", "per_interval": 5, "interval_min": 10 })).unwrap());
    inc.tips = vec![TipRow {
        source_id: "t1".into(),
        occurred_at: 1_700_000_000_000,
        name: "Big Fan".into(),
        amount_minor: 500,
        currency: "USD".into(),
        message: Some("gg".into()),
        provider: "paypal".into(),
    }];
    inc.skip(Kind::Commands, "!weird name", "spaces");

    let p = preview(&s, &inc).unwrap();
    let c = section(&p, Kind::Commands);
    assert_eq!((c.found, c.new, c.conflicts, c.skipped), (4, 1, 1, 2), "{c:?}");
    let pts = section(&p, Kind::Points);
    assert_eq!((pts.found, pts.new, pts.conflicts, pts.waiting, pts.skipped), (5, 1, 1, 1, 2), "{pts:?}");
    assert!(pts.conflict_items[0].reason.contains("Ferrocast has 40"));
    assert_eq!(section(&p, Kind::LoyaltySettings).new, 1, "no settings saved yet: nothing clashes");
    assert_eq!(section(&p, Kind::Tips).new, 1);
    assert_eq!(repo::bot::commands(&s).unwrap().len(), 1, "a preview writes nothing");
    assert!(!d.path().join("backups").exists(), "and takes no backup");

    let r = apply(&s, &inc, OnConflict::Keep).unwrap();
    assert!(d.path().join("backups").join(format!("{}.db", r.backup)).exists(), "a backup comes first");
    let rc = report_section(&r, Kind::Commands);
    assert_eq!((rc.added, rc.kept, rc.replaced), (1, 1, 0));
    let cmds = repo::bot::commands(&s).unwrap();
    assert_eq!(cmds.iter().find(|c| c.name == "!discord").unwrap().response, "my discord", "kept");
    assert!(cmds.iter().any(|c| c.name == "!lurk"));
    assert_eq!(repo::bot::timers(&s).unwrap().len(), 1);
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "1").unwrap(), Some(40), "kept Ferrocast's");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "2").unwrap(), Some(300));
    let cfg: LoyaltyConfig = repo::settings::get(&s, "loyalty").unwrap().unwrap();
    assert_eq!((cfg.name.as_str(), cfg.per_interval, cfg.interval_min), ("coins", 5, 10));
    assert_eq!(pending_count(&s).unwrap(), 1);

    // The newcomer's points arrive with them, once.
    seen(&s, Platform::Twitch, "3", "newcomer", "Newcomer");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "3").unwrap(), Some(75));
    assert_eq!(pending_count(&s).unwrap(), 0);
    seen(&s, Platform::Twitch, "3", "newcomer", "Newcomer");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "3").unwrap(), Some(75));

    // Running the same import again adds nothing new; tips are never doubled.
    let again = preview(&s, &inc).unwrap();
    assert_eq!(section(&again, Kind::Tips).new, 0);
    assert_eq!(section(&again, Kind::Commands).new, 0);
}

#[test]
fn replace_keeps_ids_and_never_deletes() {
    let (s, _d) = store();
    repo::bot::save_command(&s, &Command { id: "mine".into(), ..cmd("!discord", "old") }).unwrap();
    repo::bot::save_command(&s, &Command { id: "other".into(), aliases: vec!["!dc".into()], ..cmd("!chat", "x") })
        .unwrap();
    repo::bot::save_command(&s, &Command { id: "untouched".into(), ..cmd("!keep", "x") }).unwrap();
    repo::bot::save_timer(&s, &Timer { id: "t".into(), ..timer("Socials", "old") }).unwrap();
    seen(&s, Platform::Twitch, "1", "sam", "Sam");
    repo::loyalty::change(&s, Platform::Twitch, "1", 40, repo::loyalty::Floor::Strict).unwrap();
    repo::settings::set(&s, "loyalty", &LoyaltyConfig::default()).unwrap();

    let mut inc = incoming();
    inc.commands = vec![Command { aliases: vec!["!dc".into(), "!server".into()], ..cmd("!discord", "new") }];
    inc.timers = vec![timer("socials", "new")];
    inc.points = vec![PointsRow { name: "SAM".into(), user_id: None, points: 5_000 }];
    inc.loyalty = Some(serde_json::from_value(json!({ "name": "coins" })).unwrap());

    let p = preview(&s, &inc).unwrap();
    assert_eq!(section(&p, Kind::LoyaltySettings).conflicts, 1);
    let r = apply(&s, &inc, OnConflict::Replace).unwrap();
    assert_eq!(report_section(&r, Kind::Commands).replaced, 1);
    let cmds = repo::bot::commands(&s).unwrap();
    assert_eq!(cmds.len(), 3, "nothing deleted");
    let d = cmds.iter().find(|c| c.id == "mine").unwrap();
    assert_eq!(d.response, "new", "replaced in place, same id");
    assert_eq!(d.aliases, vec!["!server".to_string()], "an alias another command uses is left off");
    assert!(cmds.iter().find(|c| c.id == "other").unwrap().aliases.contains(&"!dc".to_string()));
    let t = repo::bot::timers(&s).unwrap();
    assert_eq!((t.len(), t[0].id.as_str(), t[0].message.as_str()), (1, "t", "new"));
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "1").unwrap(), Some(5_000));
    let cfg: LoyaltyConfig = repo::settings::get(&s, "loyalty").unwrap().unwrap();
    assert_eq!(cfg.name, "coins");
}

#[test]
fn bad_entries_are_skipped_with_a_reason_not_failed() {
    let (s, _d) = store();
    let mut inc = incoming();
    inc.commands = vec![cmd("!ok", "fine"), cmd("!broken", "{user"), cmd("!empty", "  ")];
    inc.timers = vec![timer("a", "x"), timer("A", "dupe"), timer("b", "{nope")];
    inc.loyalty = Some(serde_json::from_value(json!({ "interval_min": 500 })).unwrap());
    let r = apply(&s, &inc, OnConflict::Replace).unwrap();
    let c = report_section(&r, Kind::Commands);
    assert_eq!((c.added, c.skipped), (1, 2));
    assert!(c.skipped_items.iter().all(|i| i.reason.starts_with("Ferrocast can't use it")));
    let t = report_section(&r, Kind::Timers);
    assert_eq!((t.added, t.skipped), (1, 2));
    assert!(t.skipped_items.iter().any(|i| i.reason.contains("twice")));
    let l = report_section(&r, Kind::LoyaltySettings);
    assert_eq!((l.added, l.skipped), (0, 1));
    assert!(repo::settings::get::<LoyaltyConfig>(&s, "loyalty").unwrap().is_none());
}

#[test]
fn points_without_a_platform_and_an_empty_import() {
    let (s, _d) = store();
    let mut inc = Incoming::new(Source::Streamlabs, None);
    inc.kinds = vec![Kind::Points];
    inc.points = vec![PointsRow { name: "a".into(), user_id: None, points: 5 }];
    let p = preview(&s, &inc).unwrap();
    assert_eq!(section(&p, Kind::Points).skipped, 1);
    let mut empty = incoming();
    empty.not_imported.push(NotImported { what: "Overlays".into(), count: 2, reason: "custom code".into() });
    let r = apply(&s, &empty, OnConflict::Keep).unwrap();
    assert!(r.sections.iter().all(|s| s.found == 0 && s.added == 0));
    assert_eq!(r.not_imported.len(), 1);
}

fn row(name: &str, id: Option<&str>, points: i64) -> PointsRow {
    PointsRow { name: name.into(), user_id: id.map(String::from), points }
}

fn points_import(platform: Platform, rows: Vec<PointsRow>) -> Incoming {
    let mut inc = Incoming::new(Source::StreamElements, Some(platform));
    inc.kinds = vec![Kind::Points];
    inc.points = rows;
    inc
}

#[test]
fn twitch_matches_by_login_never_by_display_name() {
    let (s, _d) = store();
    seen(&s, Platform::Twitch, "1", "sam", "Sammy");
    // "Sammy" is only Sam's display name: it matches nobody known, so it waits under that login.
    let r = apply(
        &s,
        &points_import(Platform::Twitch, vec![row("SAM", None, 10), row("Sammy", None, 20), row("later", None, 5)]),
        OnConflict::Keep,
    )
    .unwrap();
    let p = report_section(&r, Kind::Points);
    assert_eq!((p.added, p.waiting, p.needs_match), (1, 2, 0));
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "1").unwrap(), Some(10), "the login matched");

    // An impostor who sets their display name to "later" (login "evil") gets nothing.
    seen(&s, Platform::Twitch, "666", "evil", "later");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "666").unwrap(), Some(0));
    // The real "later" (that login) gets their points when they show up, once.
    seen(&s, Platform::Twitch, "9", "Later", "Later");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "9").unwrap(), Some(5));
    seen(&s, Platform::Twitch, "10", "later2", "later");
    assert_eq!(repo::loyalty::balance(&s, Platform::Twitch, "10").unwrap(), Some(0));

    // A newer import for someone already waiting is a clash.
    let inc = points_import(Platform::Twitch, vec![row("sammy", None, 9)]);
    assert_eq!(section(&preview(&s, &inc).unwrap(), Kind::Points).conflicts, 1);
}

#[test]
fn ids_win_over_names() {
    let (s, _d) = store();
    seen(&s, Platform::Kick, "k1", "realname", "Real");
    apply(
        &s,
        &points_import(Platform::Kick, vec![row("oldname", Some("k1"), 40), row("x", Some("k2"), 7)]),
        OnConflict::Keep,
    )
    .unwrap();
    assert_eq!(repo::loyalty::balance(&s, Platform::Kick, "k1").unwrap(), Some(40), "matched by id after a rename");
    // Someone using the name "x" but with another id never gets the id-keyed points.
    seen(&s, Platform::Kick, "k3", "x", "x");
    assert_eq!(repo::loyalty::balance(&s, Platform::Kick, "k3").unwrap(), Some(0));
    seen(&s, Platform::Kick, "k2", "renamed", "renamed");
    assert_eq!(repo::loyalty::balance(&s, Platform::Kick, "k2").unwrap(), Some(7));
}

#[test]
fn an_unknown_id_never_falls_back_to_the_login() {
    let (s, _d) = store();
    // "x" is known with id k3; the import's "x" row carries id k2 (a different, renamed person).
    seen(&s, Platform::Kick, "k3", "x", "x");
    apply(&s, &points_import(Platform::Kick, vec![row("x", Some("k2"), 70)]), OnConflict::Replace).unwrap();
    assert_eq!(repo::loyalty::balance(&s, Platform::Kick, "k3").unwrap(), Some(0), "the login's owner gets nothing");
    seen(&s, Platform::Kick, "k2", "new-name", "new-name");
    assert_eq!(repo::loyalty::balance(&s, Platform::Kick, "k2").unwrap(), Some(70), "the id's owner does");
}

#[test]
fn youtube_names_alone_are_never_paid_automatically() {
    let (s, _d) = store();
    seen(&s, Platform::Youtube, "UCknown", "Ana Plays", "Ana Plays");
    let inc = points_import(
        Platform::Youtube,
        vec![
            row("Ana Plays", None, 300),
            row("Bo", None, 50),
            row("Cy", Some("UCcy"), 25),
            row("Ana", Some("UCknown"), 90),
        ],
    );
    let p = preview(&s, &inc).unwrap();
    let pts = section(&p, Kind::Points);
    assert_eq!((pts.new, pts.waiting, pts.needs_match), (1, 1, 2), "{pts:?}");
    assert!(pts.needs_match_items.iter().all(|i| i.reason == NEEDS_MATCH_REASON));
    apply(&s, &inc, OnConflict::Keep).unwrap();
    assert_eq!(repo::loyalty::balance(&s, Platform::Youtube, "UCknown").unwrap(), Some(90), "the channel id matched");
    // Someone named "Bo" (or "Ana Plays") showing up gets nothing: names aren't unique.
    let mut bo = Actor::named(Platform::Youtube, "Bo");
    bo.id = Some("UCbo".into());
    repo::viewers::touch(&s, &bo, true).unwrap();
    assert_eq!(repo::loyalty::balance(&s, Platform::Youtube, "UCbo").unwrap(), Some(0));
    let mut cy = Actor::named(Platform::Youtube, "Someone Else");
    cy.id = Some("UCcy".into());
    repo::viewers::touch(&s, &cy, true).unwrap();
    assert_eq!(repo::loyalty::balance(&s, Platform::Youtube, "UCcy").unwrap(), Some(25), "an id entry is paid");
    assert_eq!(pending_count(&s).unwrap(), 0, "only needs-a-match entries are left, and they don't count");
    let left: i64 = s
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM pending_points WHERE needs_match = 1", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(left, 2, "kept, so nothing is lost");
}

#[test]
fn imported_tips_go_into_the_export_csv() {
    let (s, d) = store();
    let mut inc = incoming();
    inc.kinds = vec![Kind::Tips];
    inc.tips = vec![TipRow {
        source_id: "abc".into(),
        occurred_at: 5,
        name: "=cmd".into(),
        amount_minor: 1234,
        currency: "USD".into(),
        message: None,
        provider: "paypal".into(),
    }];
    apply(&s, &inc, OnConflict::Keep).unwrap();
    let out = d.path().join("x.ferropack");
    let m = crate::pack::export(&s, &out, "0").unwrap();
    assert_eq!(m.counts["imported_tips"], 1);
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
    let mut csv = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("tips.csv").unwrap(), &mut csv).unwrap();
    assert!(csv.contains("5,streamelements:paypal,abc,'=cmd,12.34,USD,imported,"), "{csv}");
}
