//! A theme into OBS through the real engine, against the testkit's fake OBS (never a real one).

use std::sync::Arc;

use ferrocast_core::design::{Design, DesignContent, SCENE_KEYS, ThemeContent};
use ferrocast_obs::ObsConfig;
use ferrocast_testkit::obs::{FakeObs, Input, Item, St, closed_port, start as start_obs};
use serde_json::{Map, json};

use super::*;
use crate::PasswordChange;

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e =
        Engine::start_with_secrets(dir.path(), Arc::new(ferrocast_secrets::MemorySecrets::default())).await.unwrap();
    (e, dir)
}

/// A built-in theme like the real ones: five screens, its own alert look.
fn theme(id: &str, name: &str) -> Design {
    let alert: Design = serde_json::from_str(include_str!("../../core/fixtures/designs/alert-design.json")).unwrap();
    let scene_design: Design =
        serde_json::from_str(include_str!("../../core/fixtures/designs/scene-design.json")).unwrap();
    let DesignContent::Scene(scene) = scene_design.content else { unreachable!() };
    let names = ["In game", "Just chatting", "Starting soon", "BRB", "Ending"];
    let mut d = alert;
    d.id = id.into();
    d.name = name.into();
    d.built_in = true;
    d.content = DesignContent::Theme(ThemeContent {
        scenes: SCENE_KEYS
            .iter()
            .zip(names)
            .map(|(k, n)| {
                let mut s = scene.clone();
                s.key = (*k).into();
                s.name = n.into();
                s
            })
            .collect(),
        alert_design: Some("builtin.molten".into()),
    });
    d
}

fn molten() -> Design {
    let mut a: Design = serde_json::from_str(include_str!("../../core/fixtures/designs/alert-design.json")).unwrap();
    a.id = "builtin.molten".into();
    a.built_in = true;
    a
}

async fn setup() -> (Engine, tempfile::TempDir, St) {
    let (e, d) = engine().await;
    e.set_builtin_designs(vec![molten(), theme("builtin.forge", "Forge"), theme("builtin.prism", "Prism")]).await;
    let st = FakeObs::studio();
    {
        // An old Streamlabs alert box in Gameplay, with its key in the address.
        let mut s = st.lock().unwrap();
        s.inputs.push(Input {
            name: "Alert box".into(),
            kind: "browser_source".into(),
            muted: false,
            settings: json!({ "url": "https://streamlabs.com/alert-box/v3/SECRETKEY123" }),
        });
        let g = s.scenes.iter_mut().find(|x| x.name == "Gameplay").unwrap();
        g.items.push(Item { id: 9, source: "Alert box".into(), enabled: true, kind: "browser_source".into() });
    }
    let port = start_obs(st.clone()).await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    // What the streamer had before: Start show on their own scenes.
    let mut show = Map::new();
    show.insert("starting_scene".into(), json!("Starting Soon"));
    show.insert("live_scene".into(), json!("Gameplay"));
    e.patch_show_config(&show).await.unwrap();
    (e, d, st)
}

fn scene_names(st: &St) -> Vec<String> {
    st.lock().unwrap().scenes.iter().map(|s| s.name.clone()).collect()
}

fn items(st: &St, scene: &str) -> Vec<(String, bool)> {
    let s = st.lock().unwrap();
    s.scenes
        .iter()
        .find(|x| x.name == scene)
        .map(|x| x.items.iter().map(|i| (i.source.clone(), i.enabled)).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_guesses_find_the_game_the_camera_and_old_widgets_without_their_keys() {
    let (e, _d, _st) = setup().await;
    let p = e.theme_obs_plan("builtin.forge").await.unwrap();
    assert_eq!(p.plan.prefix, "Forge · ");
    assert_eq!(p.plan.screens[0].under.as_deref(), Some("Gameplay"));
    assert_eq!(
        p.plan.screens[1].under.as_deref(),
        Some("Webcam"),
        "no chat scene: the camera goes under Just chatting"
    );
    assert_eq!(p.old_widgets.len(), 1);
    assert_eq!(p.old_widgets[0].vendor, "Streamlabs");
    assert_eq!(p.plan.hide_old, ["Alert box"]);
    assert_eq!(p.obs_cameras, ["Webcam"]);
    let text = serde_json::to_string(&p).unwrap();
    assert!(!text.contains("SECRETKEY123") && !text.contains("streamlabs.com"), "an old widget's address never leaves");
    assert!(p.applied.is_none());
}

#[tokio::test]
async fn one_click_puts_every_screen_in_obs_and_undo_takes_only_that_away() {
    let (e, _d, st) = setup().await;
    let before_scenes = scene_names(&st);
    let gameplay_before = items(&st, "Gameplay");
    let r = e.apply_theme_to_obs("builtin.forge", None).await.unwrap();
    assert!(r.steps.iter().all(|s| s.status == "done"), "{:#?}", r.steps);

    let names = scene_names(&st);
    for n in ["In game", "Just chatting", "Starting soon", "BRB", "Ending"] {
        assert!(names.contains(&format!("Forge · {n}")), "{names:?}");
    }
    for n in &before_scenes {
        assert!(names.contains(n), "the streamer's own scenes stay");
    }
    let in_game = items(&st, "Forge · In game");
    assert_eq!(in_game.first().map(|i| i.0.as_str()), Some("Gameplay"), "their scene goes under: {in_game:?}");
    assert!(in_game.last().unwrap().0.starts_with("Ferrocast: In game"), "the overlay is on top: {in_game:?}");
    assert_eq!(items(&st, "Forge · Just chatting")[0].0, "Webcam");
    {
        // A 1080p theme fills the 1440p canvas.
        let s = st.lock().unwrap();
        let t = s.transforms.iter().find(|t| t.0 == "Forge · In game").expect("the overlay was fitted");
        assert_eq!((t.2["boundsWidth"].as_f64(), t.2["boundsHeight"].as_f64()), (Some(2560.0), Some(1440.0)));
        assert_eq!(t.2["boundsType"], "OBS_BOUNDS_SCALE_INNER");
    }

    // Start show and the BRB button use the new scenes; OBS is on Starting soon.
    let show = e.show_config();
    assert_eq!(show.starting_scene.as_deref(), Some("Forge · Starting soon"));
    assert_eq!(show.live_scene.as_deref(), Some("Forge · In game"));
    assert_eq!(show.ending_scene.as_deref(), Some("Forge · Ending"));
    assert_eq!(e.scenes_config().brb_scene.as_deref(), Some("Forge · BRB"));
    assert_eq!(st.lock().unwrap().current, "Forge · Starting soon");
    assert_eq!(e.active_alert_design().await.unwrap().as_deref(), Some("builtin.molten"));

    // The old widget is hidden, not deleted.
    let g = items(&st, "Gameplay");
    assert!(g.contains(&("Alert box".into(), false)), "{g:?}");

    // Five overlays, each made from its screen.
    let overlays = e.overlays().await.unwrap();
    assert_eq!(overlays.len(), 5);
    let view = e.obs_theme().await.unwrap().unwrap();
    assert_eq!(view.design_name, "Forge");
    assert!(view.screens.iter().all(|s| s.found == Some(true)));

    // Again: nothing doubles.
    let inputs = st.lock().unwrap().inputs.len();
    e.apply_theme_to_obs("builtin.forge", None).await.unwrap();
    assert_eq!(scene_names(&st).len(), names.len());
    assert_eq!(st.lock().unwrap().inputs.len(), inputs);
    assert_eq!(e.overlays().await.unwrap().len(), 5, "the same overlays are reused (their addresses stay)");
    assert_eq!(items(&st, "Forge · In game").len(), 2);

    // Undo.
    let u = e.undo_theme_in_obs().await.unwrap();
    assert!(u.steps.iter().all(|s| s.status == "done"), "{:#?}", u.steps);
    assert_eq!(scene_names(&st), before_scenes);
    assert_eq!(items(&st, "Gameplay"), gameplay_before, "the streamer's scene is exactly as it was");
    assert!(!st.lock().unwrap().inputs.iter().any(|i| i.name.starts_with("Ferrocast:")));
    let show = e.show_config();
    assert_eq!(show.starting_scene.as_deref(), Some("Starting Soon"));
    assert_eq!(show.live_scene.as_deref(), Some("Gameplay"));
    assert_eq!(show.ending_scene, None);
    assert_eq!(e.scenes_config().brb_scene, None);
    assert_eq!(st.lock().unwrap().current, "Starting Soon", "OBS is back on the scene it was on");
    assert_eq!(e.active_alert_design().await.unwrap(), None, "the alert look is back to none");
    assert!(e.obs_theme().await.unwrap().is_none());
    assert!(matches!(e.undo_theme_in_obs().await, Err(EngineError::Missing { code: "no_theme_in_obs", .. })));
}

#[tokio::test]
async fn advanced_plans_go_on_top_of_existing_scenes_and_replace_their_look() {
    let (e, _d, st) = setup().await;
    let mut p = e.theme_obs_plan("builtin.forge").await.unwrap().plan;
    p.prefix = String::new();
    p.screens[3].place = Place::Replace { scene: "BRB".into() };
    p.screens[4].place = Place::Skip;
    p.screens[4].role = None;
    p.screens[1].place = Place::OnTop { scene: "Starting Soon".into() };
    p.screens[1].under = None;
    p.screens[2].place = Place::New;
    p.switch_to_starting = false;
    p.hide_old.clear();
    p.alert_style = false;
    let r = e.apply_theme_to_obs("builtin.forge", Some(p)).await.unwrap();
    assert!(r.steps.iter().any(|s| s.id == "screen.ending" && s.status == "skipped"));
    assert!(scene_names(&st).contains(&"In game".to_string()), "an empty prefix names scenes after the screen");
    let brb = items(&st, "BRB");
    assert_eq!(brb[0], ("BRB card".into(), false), "replace hides the old look: {brb:?}");
    assert!(brb.last().unwrap().0.starts_with("Ferrocast: BRB"));
    assert_eq!(e.scenes_config().brb_scene.as_deref(), Some("BRB"));
    assert!(items(&st, "Gameplay").contains(&("Alert box".into(), true)), "old widgets left alone when not asked");
    assert_eq!(st.lock().unwrap().current, "Starting Soon", "not switched");
    assert_ne!(e.active_alert_design().await.unwrap().as_deref(), Some("builtin.molten"));

    e.undo_theme_in_obs().await.unwrap();
    assert_eq!(items(&st, "BRB"), [("BRB card".into(), true)], "the old look is back, Ferrocast's source gone");
    assert!(scene_names(&st).contains(&"BRB".to_string()), "a scene Ferrocast didn't make stays");
    assert_eq!(items(&st, "Starting Soon").len(), 2, "only what Ferrocast added is taken out");
}

#[tokio::test]
async fn refusals_say_what_to_do() {
    let (e, _d, _st) = setup().await;
    // Not a theme.
    assert!(
        matches!(e.theme_obs_plan("builtin.molten").await, Err(EngineError::Invalid(m)) if m.starts_with("design:"))
    );
    // A bad plan names the field.
    let mut p = e.theme_obs_plan("builtin.forge").await.unwrap().plan;
    p.screens[0].under = Some("Nope".into());
    assert!(
        matches!(e.apply_theme_to_obs("builtin.forge", Some(p)).await, Err(EngineError::Invalid(m)) if m.starts_with("screens.0.under"))
    );
    e.apply_theme_to_obs("builtin.forge", None).await.unwrap();
    // OBS closed.
    let port = closed_port().await;
    e.save_obs(ObsConfig { host: "127.0.0.1".into(), port }, PasswordChange::Keep).await.unwrap();
    assert!(matches!(
        e.theme_obs_plan("builtin.forge").await,
        Err(EngineError::Refused { status: 422, code: "obs_unreachable", .. })
    ));
    let view = e.obs_theme().await.unwrap().unwrap();
    assert!(view.screens.iter().all(|s| s.found.is_none()), "unknown while OBS is closed");
}

#[tokio::test]
async fn an_undo_that_obs_partly_refuses_can_be_tried_again() {
    let (e, _d, st) = setup().await;
    let before = scene_names(&st);
    e.apply_theme_to_obs("builtin.forge", None).await.unwrap();
    // The streamer deleted one of Ferrocast's sources by hand: that's fine.
    st.lock().unwrap().inputs.retain(|i| !i.name.starts_with("Ferrocast: BRB"));
    st.lock().unwrap().refuse = vec!["RemoveScene".into()];
    let r = e.undo_theme_in_obs().await.unwrap();
    let obs = r.steps.iter().find(|s| s.id == "obs").unwrap();
    assert_eq!(obs.status, "failed", "{obs:?}");
    assert!(obs.detail.contains("refused 5 changes"), "{}", obs.detail);
    let left = r.applied.expect("what's left is still tracked");
    assert_eq!(left.design_name, "Forge");
    assert_eq!(e.show_config().starting_scene.as_deref(), Some("Starting Soon"), "Start show is back already");
    // OBS says yes now: the rest goes.
    st.lock().unwrap().refuse.clear();
    let r = e.undo_theme_in_obs().await.unwrap();
    assert!(r.steps.iter().all(|s| s.status == "done"), "{:#?}", r.steps);
    assert_eq!(scene_names(&st), before);
    assert!(e.obs_theme().await.unwrap().is_none());
    assert_eq!(e.show_config().starting_scene.as_deref(), Some("Starting Soon"), "a second undo doesn't move it again");
}

#[tokio::test]
async fn switching_theme_keeps_the_same_scenes_sources_and_addresses() {
    let (e, _d, st) = setup().await;
    let before = scene_names(&st);
    e.apply_theme_to_obs("builtin.forge", None).await.unwrap();
    let urls: Vec<String> = e.overlays().await.unwrap().iter().map(|o| o.url.clone()).collect();
    let inputs = st.lock().unwrap().inputs.len();
    let r = e.apply_theme_to_obs("builtin.prism", None).await.unwrap();
    assert!(r.steps.iter().all(|s| s.status == "done"), "{:#?}", r.steps);
    let names = scene_names(&st);
    assert_eq!(names.len(), before.len() + 5, "no new scenes: {names:?}");
    assert!(names.contains(&"Prism · In game".to_string()) && !names.iter().any(|n| n.starts_with("Forge")));
    let overlays = e.overlays().await.unwrap();
    assert_eq!(overlays.iter().map(|o| o.url.clone()).collect::<Vec<_>>(), urls, "OBS keeps the same addresses");
    assert!(overlays.iter().all(|o| o.name.ends_with("(Prism)") && o.design.as_ref().unwrap().id == "builtin.prism"));
    assert_eq!(st.lock().unwrap().inputs.len(), inputs, "sources renamed, not added");
    assert!(items(&st, "Prism · In game").iter().any(|(s, _)| s == "Ferrocast: In game (Prism)"));
    assert_eq!(e.show_config().live_scene.as_deref(), Some("Prism · In game"));
    assert_eq!(e.obs_theme().await.unwrap().unwrap().design_name, "Prism");
    e.undo_theme_in_obs().await.unwrap();
    assert_eq!(scene_names(&st), before);
    assert_eq!(e.show_config().live_scene.as_deref(), Some("Gameplay"), "back to what was there before either theme");
}

#[tokio::test]
async fn screens_follow_their_theme_until_edited_by_hand() {
    let (e, _d, _st) = setup().await;
    let mine = e.duplicate_design("builtin.forge", Some("Forge (mine)")).await.unwrap();
    e.apply_theme_to_obs(&mine.id, None).await.unwrap();
    let find = |list: &[crate::snapshot::OverlayView], key: &str| {
        list.iter().find(|o| o.design.as_ref().is_some_and(|d| d.scene == key)).unwrap().clone()
    };
    let brb = find(&e.overlays().await.unwrap(), "brb");
    assert!(brb.design.as_ref().unwrap().follows);

    // Editing the theme changes every screen that follows it.
    let mut edited = e.design(&mine.id).await.unwrap();
    let DesignContent::Theme(t) = &mut edited.content else { unreachable!() };
    for s in &mut t.scenes {
        s.layers.remove(0);
    }
    let n_before = brb.widgets.len();
    e.update_design(&mine.id, edited.clone()).await.unwrap();
    let brb = find(&e.overlays().await.unwrap(), "brb");
    assert_eq!(brb.widgets.len(), n_before - 1, "the screen followed the theme");

    // A hand edit stops it following.
    let mut o = e.overlay(&brb.id).await.unwrap();
    o.widgets.pop();
    let saved = e.update_overlay(o).await.unwrap();
    assert!(!saved.design.as_ref().unwrap().follows);
    let DesignContent::Theme(t) = &mut edited.content else { unreachable!() };
    t.scenes.iter_mut().for_each(|s| s.layers.truncate(1));
    e.update_design(&mine.id, edited).await.unwrap();
    assert_eq!(e.overlay(&brb.id).await.unwrap().widgets.len(), saved.widgets.len(), "left alone");
    let in_game = find(&e.overlays().await.unwrap(), "in_game");
    assert_eq!(in_game.widgets.len(), 1, "the others still follow");

    // "Follow the theme again".
    let back = e.follow_design(&brb.id).await.unwrap();
    assert!(back.design.as_ref().unwrap().follows);
    assert_eq!(back.widgets.len(), 1);
    assert_eq!(back.url, brb.url);
}

#[tokio::test]
async fn without_a_plan_a_theme_in_obs_keeps_its_screens() {
    let (e, _d, st) = setup().await;
    let mut p = e.theme_obs_plan("builtin.forge").await.unwrap().plan;
    p.screens[4] = ScreenPlan { key: "ending".into(), place: Place::Skip, under: None, role: None };
    e.apply_theme_to_obs("builtin.forge", Some(p)).await.unwrap();
    e.apply_theme_to_obs("builtin.prism", None).await.unwrap();
    let view = e.obs_theme().await.unwrap().unwrap();
    assert_eq!(view.screens.len(), 4, "Ending stays out");
    assert!(!scene_names(&st).iter().any(|n| n.ends_with("Ending") && n.contains('·')));
}
