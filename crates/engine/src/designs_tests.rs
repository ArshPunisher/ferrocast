//! Designs in the engine: the catalogue, branding rules, Use, test alerts and TTS.

use std::time::Duration;

use ferrocast_core::design::{Design, DesignContent, DesignKind, OriginKind, SCENE_KEYS, Scene, ThemeContent};
use ferrocast_core::event::*;
use ferrocast_core::money::Money;
use ferrocast_core::overlay::{Preset, Widget, WidgetKind};
use serde_json::{Value, json};

use super::*;

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::start_with_secrets(dir.path(), std::sync::Arc::new(ferrocast_secrets::MemorySecrets::default()))
        .await
        .unwrap();
    (e, dir)
}

pub(crate) fn alert_fixture() -> Design {
    serde_json::from_str(include_str!("../../core/fixtures/designs/alert-design.json")).unwrap()
}

fn scene_fixture() -> Scene {
    let d: Design = serde_json::from_str(include_str!("../../core/fixtures/designs/scene-design.json")).unwrap();
    let DesignContent::Scene(s) = d.content else { unreachable!() };
    s
}

/// A small catalogue: a branded alert design, a branded theme (with its mark), a widget design.
pub(crate) fn catalogue() -> Vec<Design> {
    let mut molten = alert_fixture();
    molten.id = "builtin.molten".into();
    molten.name = "Molten".into();
    molten.built_in = true;
    molten.branded = true;
    molten.origin.kind = OriginKind::BuiltIn;
    let mut theme = molten.clone();
    theme.id = "builtin.forge".into();
    theme.name = "Forge".into();
    let scenes = SCENE_KEYS
        .iter()
        .map(|k| {
            let mut s = scene_fixture();
            s.key = (*k).into();
            s.layers.push(Widget::new(
                "spark",
                WidgetKind::Layer,
                (1840, 1010, 56, 48),
                json!({ "type": "brand", "variant": "mark" }),
            ));
            s
        })
        .collect();
    theme.content = DesignContent::Theme(ThemeContent { scenes, alert_design: Some("builtin.molten".into()) });
    let mut goal = molten.clone();
    goal.id = "builtin.goal-segments".into();
    goal.name = "Segment goal".into();
    goal.content = DesignContent::Widget(ferrocast_core::design::WidgetContent {
        widget: WidgetKind::Goal,
        w: 640,
        h: 96,
        config: json!({ "goal_id": "", "theme": "molten" }),
        style: json!({ "variant": "segments", "accent": "#ffa424", "brand": "corner" }),
    });
    let mut broken = molten.clone();
    broken.id = "builtin.broken".into();
    broken.name = String::new();
    let mut not_builtin_id = molten.clone();
    not_builtin_id.id = "sneaky".into();
    vec![molten, theme, goal, broken, not_builtin_id]
}

async fn with_catalogue() -> (Engine, tempfile::TempDir) {
    let (e, d) = engine().await;
    e.set_builtin_designs(catalogue()).await;
    (e, d)
}

async fn next_alert(rx: &mut tokio::sync::broadcast::Receiver<crate::frames::OverlayFrame>) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let f = tokio::time::timeout_at(deadline, rx.recv()).await.expect("timed out").expect("closed");
        let v: Value = serde_json::from_str(&f.text).unwrap();
        if v["type"] == "alert" {
            return v["data"].clone();
        }
    }
}

fn refusal_code(e: EngineError) -> (u16, &'static str) {
    match e {
        EngineError::Refused { status, code, .. } => (status, code),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn the_catalogue_keeps_only_valid_built_ins_and_lists_them_first() {
    let (e, _d) = with_catalogue().await;
    let ids: Vec<String> = e.builtin_designs().iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, ["builtin.molten", "builtin.forge", "builtin.goal-segments"], "broken and badly named are skipped");
    let mine = e.create_design(alert_fixture()).await.unwrap();
    let all = e.designs(None).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all.last().unwrap().id, mine.id);
    assert_eq!(e.designs(Some(DesignKind::Theme)).await.unwrap().len(), 1);
    assert!(matches!(e.design("builtin.nope").await, Err(EngineError::NotFound(_))));
}

#[tokio::test]
async fn created_designs_are_always_the_streamers_own() {
    let (e, _d) = with_catalogue().await;
    let mut forged = alert_fixture();
    forged.id = "builtin.molten".into();
    forged.built_in = true;
    forged.branded = true;
    forged.pro = true;
    forged.origin.kind = OriginKind::BuiltIn;
    let mine = e.create_design(forged).await.unwrap();
    assert!(!mine.built_in && !mine.branded && !mine.pro);
    assert_ne!(mine.id, "builtin.molten");
    assert_eq!(mine.origin.kind, OriginKind::Scratch);
    // Edits: built-ins can't change, fields are named, kinds stay.
    let (status, code) = refusal_code(e.update_design("builtin.molten", alert_fixture()).await.unwrap_err());
    assert_eq!((status, code), (409, "built_in"));
    let mut bad = mine.clone();
    bad.name = String::new();
    assert!(matches!(e.update_design(&mine.id, bad).await, Err(EngineError::Invalid(m)) if m.starts_with("name:")));
    let mut theme = e.design("builtin.forge").await.unwrap();
    theme.built_in = false;
    theme.branded = false;
    theme.strip_brand_layers();
    assert!(matches!(e.update_design(&mine.id, theme).await, Err(EngineError::Store(_))));
    let mut branded = mine.clone();
    branded.branded = true;
    assert!(matches!(e.update_design(&mine.id, branded).await, Err(EngineError::Invalid(_))));
    let (status, code) = refusal_code(e.delete_design("builtin.molten").await.unwrap_err());
    assert_eq!((status, code), (409, "built_in"));
}

#[tokio::test]
async fn duplicating_a_built_in_theme_makes_an_unbranded_copy_with_its_own_alert_design() {
    let (e, _d) = with_catalogue().await;
    let copy = e.duplicate_design("builtin.forge", None).await.unwrap();
    assert_eq!(copy.name, "Forge copy");
    assert!(!copy.branded && !copy.built_in);
    assert_eq!(copy.origin.kind, OriginKind::Duplicate);
    let DesignContent::Theme(t) = &copy.content else { unreachable!() };
    assert!(t.scenes.iter().all(|s| !s.layers.iter().any(ferrocast_core::design::is_brand_layer)), "no mark");
    let alert_id = t.alert_design.clone().unwrap();
    assert!(!alert_id.starts_with("builtin."), "the built-in alert design was copied too");
    let alert = e.design(&alert_id).await.unwrap();
    assert!(!alert.branded);
    let named = e.duplicate_design("builtin.molten", Some("  My molten  ")).await.unwrap();
    assert_eq!(named.name, "My molten");
}

#[tokio::test]
async fn the_active_alert_design_dresses_real_and_test_alerts() {
    let (e, _d) = with_catalogue().await;
    assert!(matches!(e.set_active_alert_design(Some("builtin.forge")).await, Err(EngineError::Invalid(_))));
    assert!(matches!(e.set_active_alert_design(Some("missing")).await, Err(EngineError::NotFound(_))));
    let mut rx = e.subscribe_overlays();
    e.test("tip", None).await.unwrap();
    let plain = next_alert(&mut rx).await;
    assert!(plain.get("design").is_none(), "no design picked: alerts look as before");
    e.ack(plain["alert_id"].as_str().unwrap(), ferrocast_core::queue::AckPhase::Finished).await;

    e.set_active_alert_design(Some("builtin.molten")).await.unwrap();
    e.test("tip", None).await.unwrap();
    let a = next_alert(&mut rx).await;
    assert_eq!(a["layout"], "design");
    assert_eq!(a["design"]["design_id"], "builtin.molten");
    assert_eq!(a["design"]["branded"], true);
    assert_eq!(a["design"]["event_key"], "tip");
    assert!(a["title"].as_str().unwrap().ends_with(" tipped ₹500!"), "{}", a["title"]);
    assert_eq!(a["design"]["title_parts"][0]["hl"], true);
    let (status, code) = refusal_code(e.delete_design("builtin.molten").await.unwrap_err());
    assert_eq!((status, code), (409, "built_in"));
    e.ack(a["alert_id"].as_str().unwrap(), ferrocast_core::queue::AckPhase::Finished).await;

    // The designer's test uses the design being edited and the amount asked for.
    let mine = e.create_design(alert_fixture()).await.unwrap();
    let opts = crate::TestOptions { design_id: Some(mine.id.clone()), amount: Some(2500.0) };
    e.test_with("tip", None, opts).await.unwrap();
    let t = next_alert(&mut rx).await;
    assert_eq!(t["design"]["design_id"], mine.id.as_str());
    assert_eq!(t["design"]["branded"], false);
    assert_eq!(t["design"]["variation_id"], "tip-big", "₹2,500 picks the big-tip look");
    assert!(t["title"].as_str().unwrap().contains("2,500"), "{}", t["title"]);
    let bad = crate::TestOptions { design_id: None, amount: Some(-3.0) };
    assert!(matches!(e.test_with("tip", None, bad).await, Err(EngineError::Invalid(m)) if m.starts_with("amount:")));
    let none = crate::TestOptions { design_id: None, amount: Some(3.0) };
    assert!(matches!(e.test_with("follow", None, none).await, Err(EngineError::Invalid(_))));
    let theme = crate::TestOptions { design_id: Some("builtin.forge".into()), amount: None };
    assert!(matches!(e.test_with("tip", None, theme).await, Err(EngineError::Invalid(_))));

    // The active design can't be deleted out from under the alerts.
    e.set_active_alert_design(Some(&mine.id)).await.unwrap();
    let (status, code) = refusal_code(e.delete_design(&mine.id).await.unwrap_err());
    assert_eq!((status, code), (409, "in_use"));
    e.set_active_alert_design(None).await.unwrap();
    e.delete_design(&mine.id).await.unwrap();
}

#[tokio::test]
async fn the_design_decides_whether_tts_reads_on_top_of_the_policy() {
    let (e, _d) = with_catalogue().await;
    let mut tts = e.tts_policy();
    tts.enabled = true;
    e.save_tts(tts).await.unwrap();
    e.queue_pause().await;
    let mut quiet = alert_fixture();
    if let DesignContent::Alert(a) = &mut quiet.content {
        for v in a.events.values_mut().flat_map(|l| &mut l.variations) {
            v.look.tts.enabled = false;
        }
    }
    let quiet = e.create_design(quiet).await.unwrap();
    let sc = |id: &str| {
        Event::live(
            Platform::Tips,
            Some(id.into()),
            Millis(0),
            EventKind::Tip {
                actor: Actor::named(Platform::Tips, "Ravi"),
                money: Money::new(50_000, "INR"),
                message: Some("read this out".into()),
                provider: TipProvider::Razorpay,
                payment_ref: id.into(),
                status: PaymentStatus::Captured,
            },
        )
    };
    e.set_active_alert_design(Some(&quiet.id)).await.unwrap();
    e.ingest(sc("t-1")).await;
    assert!(e.queue().await.waiting.last().unwrap().alert.tts_text.is_none(), "the design keeps it quiet");
    let mut loud = alert_fixture();
    if let DesignContent::Alert(a) = &mut loud.content {
        for v in a.events.values_mut().flat_map(|l| &mut l.variations) {
            v.look.tts = ferrocast_core::design::Tts { enabled: true, min_amount: 100.0 };
        }
    }
    let loud = e.create_design(loud).await.unwrap();
    e.set_active_alert_design(Some(&loud.id)).await.unwrap();
    e.ingest(sc("t-2")).await;
    let text = e.queue().await.waiting.last().unwrap().alert.tts_text.clone();
    assert!(text.as_deref().is_some_and(|t| t.ends_with("read this out")), "{text:?}");
}

#[tokio::test]
async fn using_a_theme_makes_an_overlay_per_scene_that_keeps_the_mark() {
    let (e, _d) = with_catalogue().await;
    let r = e.use_design("builtin.forge", UseRequest::default()).await.unwrap();
    assert_eq!(r.overlays.len(), SCENE_KEYS.len(), "one per screen");
    assert_eq!(r.alert_design.as_deref(), Some("builtin.molten"), "the theme's alerts come along");
    let o = e.overlay(&r.overlays[0].id).await.unwrap();
    assert_eq!(o.design.as_ref().unwrap().id, "builtin.forge");
    assert!(o.design.as_ref().unwrap().branded);
    assert!(o.has_brand_layer());
    assert!(r.overlays[0].name.contains("Forge"), "{}", r.overlays[0].name);
    // The mark can't be edited away; moving everything else is fine.
    let mut edited = o.clone();
    edited.widgets.retain(|w| !ferrocast_core::design::is_brand_layer(w));
    let (status, code) = refusal_code(e.update_overlay(edited).await.unwrap_err());
    assert_eq!((status, code), (409, "branding"));
    let mut hidden = o.clone();
    for w in &mut hidden.widgets {
        if ferrocast_core::design::is_brand_layer(w) {
            w.hidden = true;
        }
    }
    assert!(e.update_overlay(hidden).await.is_err(), "hiding it counts as removing it");
    let mut moved = o.clone();
    moved.widgets[1].x += 10;
    e.update_overlay(moved).await.unwrap();

    // One scene onto an existing overlay keeps its URL; without alert_style the alerts stay.
    e.set_active_alert_design(None).await.unwrap();
    let mine = e.create_overlay("Mine", Preset::Starter).await.unwrap();
    let req = UseRequest { overlay_id: Some(mine.id.clone()), scenes: vec!["brb".into()], alert_style: Some(false) };
    let r = e.use_design("builtin.forge", req).await.unwrap();
    assert_eq!(r.overlays[0].url, mine.url, "OBS keeps working");
    assert_eq!(r.alert_design, None);
    let req = UseRequest { overlay_id: Some(mine.id.clone()), scenes: vec![], alert_style: None };
    assert!(matches!(e.use_design("builtin.forge", req).await, Err(EngineError::Invalid(_))), "one scene only");
    let req = UseRequest { overlay_id: None, scenes: vec!["lobby".into()], alert_style: None };
    assert!(matches!(e.use_design("builtin.forge", req).await, Err(EngineError::Invalid(_))));

    // An unbranded copy carries no mark and can be edited freely.
    let copy = e.duplicate_design("builtin.forge", None).await.unwrap();
    let r = e.use_design(&copy.id, UseRequest { scenes: vec!["ending".into()], ..Default::default() }).await.unwrap();
    let o = e.overlay(&r.overlays[0].id).await.unwrap();
    assert!(!o.has_brand_layer());
    assert!(!o.design.unwrap().branded);
}

#[tokio::test]
async fn widget_designs_add_a_styled_widget_that_keeps_a_built_ins_mark() {
    let (e, _d) = with_catalogue().await;
    assert!(matches!(
        e.use_design("builtin.goal-segments", UseRequest::default()).await,
        Err(EngineError::Invalid(m)) if m.starts_with("overlay_id:")
    ));
    let o = e.create_overlay("Main", Preset::Blank).await.unwrap();
    let req = UseRequest { overlay_id: Some(o.id.clone()), ..Default::default() };
    let r = e.use_design("builtin.goal-segments", req.clone()).await.unwrap();
    let w = &r.overlays[0].widgets[0];
    assert_eq!(w.kind, WidgetKind::Goal);
    assert_eq!(w.config["style"]["variant"], "segments");
    assert_eq!(w.config["branded"], true);
    assert_eq!((w.w, w.h), (640, 96));
    let again = e.use_design("builtin.goal-segments", req).await.unwrap();
    assert_ne!(again.overlays[0].widgets[1].id, w.id, "ids never clash");
    let mut stripped = e.overlay(&o.id).await.unwrap();
    stripped.widgets[0].config["branded"] = json!(false);
    let (status, code) = refusal_code(e.update_overlay(stripped).await.unwrap_err());
    assert_eq!((status, code), (409, "branding"));
}

#[tokio::test]
async fn the_overlay_test_preview_uses_the_active_design() {
    let (e, _d) = with_catalogue().await;
    e.set_active_alert_design(Some("builtin.molten")).await.unwrap();
    let alert = (0..6).find_map(|n| e.preview(n).1).unwrap();
    assert_eq!(alert.layout, "design");
    assert!(alert.design.unwrap().branded);
}

#[tokio::test]
async fn design_files_round_trip_through_the_engine_and_refuse_built_ins() {
    let (e, d) = with_catalogue().await;
    let out = d.path().join("x.ferrodesign");
    let (status, code) = refusal_code(e.export_design("builtin.molten", &out).await.unwrap_err());
    assert_eq!((status, code), (409, "built_in"));
    let copy = e.duplicate_design("builtin.forge", None).await.unwrap();
    let m = e.export_design(&copy.id, &out).await.unwrap();
    assert_eq!(m.designs, 2, "the theme takes its own alert design along");
    let r = e.import_design_file(&out).await.unwrap();
    assert_eq!(r.designs.len(), 2);
    assert!(r.designs.iter().all(|x| x.origin.kind == OriginKind::Import && !x.branded));
    let junk = d.path().join("junk.ferrodesign");
    std::fs::write(&junk, b"not a zip").unwrap();
    let (status, code) = refusal_code(e.import_design_file(&junk).await.unwrap_err());
    assert_eq!((status, code), (415, "bad_design_file"));
    assert_eq!(e.designs_using_media("nothing").await.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn a_reload_keeps_the_active_design() {
    let (e, _d) = with_catalogue().await;
    e.set_active_alert_design(Some("builtin.molten")).await.unwrap();
    e.reload().await.unwrap();
    assert_eq!(e.state().active_design.as_ref().map(|d| d.id.as_str()), Some("builtin.molten"));
}
