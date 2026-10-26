//! Studio Mode's extras and what undo puts back, against an installed OBS.app (macOS): the
//! T-bar (held, let go near either end), swap, "Duplicate scene", fade to black; a removed
//! scene, filters, settings and sound put back as they were; show/hide transitions and
//! deinterlacing, saved and loaded back.
//!
//!   cargo run -p ferrocast-studio --example studio_mode -- <out dir>

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, Paths, Studio, StudioOptions, TransitionInfo};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let tone = out.join("tone.wav");
    if !tone.exists() {
        let ok = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=1000:duration=10", "-ac", "2"])
            .arg(&tone)
            .status()?
            .success();
        assert!(ok, "ffmpeg made the tone");
    }
    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Live")?;
    c.add_scene("Next")?;
    c.add_scene("Later")?;
    c.add_source("Live", "color_source_v3", "Red", &json!({ "color": 0xFF_00_00_FFu32 }), true)?;
    let box_item = c.add_source("Next", "color_source_v3", "Box", &json!({ "color": 0xFF_FF_00_00u32 }), false)?;
    let tone_item = c.add_source(
        "Next",
        "ffmpeg_source",
        "Tone",
        &json!({ "local_file": tone.display().to_string(), "looping": true }),
        false,
    )?;
    c.add_filter("Box", "color_filter_v2", "Warm", &json!({ "brightness": 0.1 }))?;

    // A removed scene comes back with its sources, filters and place in the list.
    let snap = c.snap_scene("Next")?;
    c.remove_scene("Next")?;
    assert!(c.items("Next").is_err());
    c.restore_scene(&snap)?;
    let names: Vec<_> = c.scenes().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["Live", "Next", "Later"], "back at its place");
    let items = c.items("Next")?;
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [tone_item.id, box_item.id], "same items, same ids");
    assert_eq!(c.filters("Box")?.len(), 1, "its sources kept their filters");
    assert!(c.restore_scene(&snap).is_err(), "a name in use can't come back twice");
    // A removed source held for undo doesn't keep its name: a new one can take it, and is
    // never saved; the old one then can't come back under it.
    let held = c.snap("Next", box_item.id)?;
    c.edit_item("Next", box_item.id, ferrocast_studio::ItemEdit::Remove)?;
    let other = c.add_source("Later", "color_source_v3", "Box", &json!({}), false)?;
    let saved = c.save("Held");
    let n = saved["sources"].as_array().ok_or("sources")?.iter().filter(|s| s["name"] == "Box").count();
    assert_eq!(n, 1, "only the Box in use is saved");
    assert!(c.restore(&held).is_err(), "its name is taken now");
    c.edit_item("Later", other.id, ferrocast_studio::ItemEdit::Remove)?;
    let back = c.restore(&held)?;
    assert!(c.items("Next")?.iter().any(|i| i.id == back && i.name == "Box"), "back under its own name");
    drop(held);
    // A removed scene's sources free their names too, and get them back with it.
    let snap2 = c.snap_scene("Next")?;
    c.remove_scene("Next")?;
    let tmp = c.add_source("Later", "color_source_v3", "Tone", &json!({}), false)?;
    c.edit_item("Later", tmp.id, ferrocast_studio::ItemEdit::Remove)?;
    c.restore_scene(&snap2)?;
    let names: Vec<_> = c.items("Next")?.into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["Tone", "Box"], "sources back under their names, in order");
    // Undo steps hold sources: they go before another collection loads (as the app does).
    drop(snap);
    drop(snap2);
    // Box came back as a new item.
    let box_id = back;

    // Filters: added, changed, turned off, moved, removed; then put back exactly.
    c.add_filter("Box", "crop_filter", "Crop", &json!({ "left": 10 }))?;
    let filters = c.snap_filters("Box")?;
    let before = (c.filters("Box")?, c.filter_settings("Box", "Warm")?);
    c.update_filter("Box", "Warm", &json!({ "brightness": 0.5 }))?;
    c.set_filter_enabled("Box", "Crop", false)?;
    c.move_filter("Box", "Crop", ferrocast_studio::ItemEdit::Top)?;
    c.remove_filter("Box", "Warm")?;
    c.add_filter("Box", "sharpness_filter_v2", "Sharp", &json!({}))?;
    c.restore_filters(&filters)?;
    assert_eq!((c.filters("Box")?, c.filter_settings("Box", "Warm")?), before, "filters put back");

    // A source's settings put back, keys it didn't have included.
    let was = c.snap_settings("Box")?;
    c.update("Box", &json!({ "width": 64, "color": 0xFF_00_FF_00u32 }))?;
    c.restore_settings("Box", &was)?;
    assert_eq!(c.snap_settings("Box")?, was);

    // Sound: balance, sync, mono and tracks, and putting all of it back.
    let audio = c.snap_audio("Tone")?;
    c.set_volume("Tone", -12.0)?;
    c.set_muted("Tone", true)?;
    c.set_balance("Tone", 0.25)?;
    c.set_sync("Tone", 120)?;
    c.set_mono("Tone", true)?;
    c.set_tracks("Tone", &[1, 3])?;
    let a = c.audio_of("Tone")?;
    assert!((a.balance - 0.25).abs() < 1e-6 && a.sync_ms == 120 && a.mono && a.tracks == [1, 3], "{a:?}");
    assert!(
        c.set_sync("Tone", 30_000).is_err()
            && c.set_tracks("Tone", &[7]).is_err()
            && c.set_balance("Tone", 2.0).is_err()
    );
    c.restore_audio(&audio)?;
    let a = c.audio_of("Tone")?;
    assert!(!a.muted && !a.mono && a.sync_ms == 0 && a.tracks == [1, 2, 3, 4, 5, 6], "{a:?}");

    // Show and hide transitions, and deinterlacing (video from a file only).
    c.set_item_transition("Next", box_id, true, Some(&TransitionInfo { kind: "fade_transition".into(), ms: 700 }))?;
    c.set_item_transition("Next", box_id, false, Some(&TransitionInfo { kind: "swipe_transition".into(), ms: 250 }))?;
    assert!(c.set_item_transition("Next", box_id, true, Some(&TransitionInfo { kind: "boom".into(), ms: 1 })).is_err());
    c.set_deinterlace("Tone", "yadif", false)?;
    assert!(c.set_deinterlace("Box", "yadif", true).is_err(), "a colour isn't interlaced video");
    let saved = c.save("Test");
    std::fs::write(out.join("saved.json"), serde_json::to_string_pretty(&saved)?)?;
    let back_items = {
        let items = c.items("Next")?;
        drop(c);
        let back = Collection::load(&studio, &saved, "Scene")?;
        std::thread::sleep(Duration::from_millis(200));
        // Sizes are known once drawn: compared without them.
        let plain = |v: Vec<ferrocast_studio::ItemInfo>| {
            v.into_iter().map(|i| ferrocast_studio::ItemInfo { frame: [0.0; 6], ..i }).collect::<Vec<_>>()
        };
        assert_eq!(plain(back.items("Next")?), plain(items), "transitions and deinterlacing saved and loaded back");
        back
    };
    let c = back_items;
    let bx = c.items("Next")?.into_iter().find(|i| i.name == "Box").ok_or("Box")?;
    assert_eq!(bx.show_transition, Some(TransitionInfo { kind: "fade_transition".into(), ms: 700 }));
    assert_eq!(bx.hide_transition.as_ref().map(|t| t.kind.as_str()), Some("swipe_transition"));
    let tn = c.items("Next")?.into_iter().find(|i| i.name == "Tone").ok_or("Tone")?;
    assert_eq!((tn.deinterlace.as_deref(), tn.top_field_first), (Some("yadif"), false));
    assert_eq!(bx.deinterlace, None);

    // Studio Mode: what goes on air is a copy, so editing the preview doesn't touch it.
    c.set_current("Live")?;
    c.set_studio_mode(true);
    assert!(c.on_air_is_copy(), "on: a copy goes on air");
    assert_eq!(c.studio_options(), StudioOptions::default());
    c.set_preview("Next")?;
    c.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }))?;
    assert_eq!(c.current(), "Next");
    assert_eq!(c.preview_scene().as_deref(), Some("Live"), "swap: the preview shows what was on air");
    // A click mid-transition does nothing (OBS: one at a time).
    c.set_preview("Later")?;
    c.transition_now(Some(&TransitionInfo { kind: "fade_transition".into(), ms: 1500 }))?;
    std::thread::sleep(Duration::from_millis(200));
    c.set_preview("Live")?;
    c.transition_now(None)?;
    assert_eq!(c.current(), "Later", "the second click waited");
    std::thread::sleep(Duration::from_millis(1600));

    // The T-bar: held part way, let go near the start (back), then near the end (done).
    c.set_preview("Next")?;
    let st = c.tbar(0.4, false)?;
    assert!(st.active);
    assert!(c.transition_now(None).is_err(), "Transition waits for the T-bar");
    assert!(c.remove_scene("Live").is_err(), "no removing while the T-bar is held");
    assert!(c.tbar(0.5, true)?.active, "let go in the middle: held between both");
    assert!(!c.tbar(0.05, true)?.active);
    assert_eq!(c.current(), "Later", "let go near the start: back to what was on air");
    assert_eq!(c.preview_scene().as_deref(), Some("Next"));
    c.tbar(0.3, false)?;
    c.tbar(0.7, false)?;
    assert!(!c.tbar(0.95, true)?.active);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(c.current(), "Next", "let go near the end: the preview is on air");
    assert_eq!(c.preview_scene().as_deref(), Some("Later"), "and swapped");

    // Swap with a cut; options without swap or copies.
    c.swap_scenes()?;
    assert_eq!((c.current().as_str(), c.preview_scene().as_deref()), ("Later", Some("Next")));
    c.set_studio_options(StudioOptions { swap: false, duplicate_scene: false, ..StudioOptions::default() })?;
    assert!(!c.on_air_is_copy(), "no copy: the scene itself is on air");
    c.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }))?;
    assert_eq!(c.preview_scene().as_deref(), Some("Next"), "no swap: the preview stays");
    c.set_studio_options(StudioOptions::default())?;
    assert!(c.on_air_is_copy());

    // Removing the scene on air while black stays black; fading back brings the next in.
    c.add_scene("Gone soon")?;
    c.set_preview("Gone soon")?;
    c.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }))?;
    assert!(c.fade_to_black()?);
    c.remove_scene("Gone soon")?;
    assert!(c.is_black(), "still black after removing the scene on air");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!c.fade_to_black()?);
    std::thread::sleep(Duration::from_millis(600));
    // A switch from elsewhere mid-drag ends the T-bar drag.
    c.set_preview("Later")?;
    let on = c.current();
    assert!(c.tbar(0.5, false)?.active);
    c.set_current(&on)?;
    assert!(!c.tbar_state().active, "a switch made elsewhere ends the drag");
    std::thread::sleep(Duration::from_millis(400));
    c.set_current("Next")?;
    std::thread::sleep(Duration::from_millis(400));

    // Fade to black and back.
    assert!(c.fade_to_black()?);
    assert!(c.is_black());
    std::thread::sleep(Duration::from_millis(600));
    assert!(!c.fade_to_black()?);
    assert!(!c.is_black());
    std::thread::sleep(Duration::from_millis(600));

    // Off: the scene itself is on air again; the options are saved.
    c.set_studio_options(StudioOptions { double_click: true, ..StudioOptions::default() })?;
    let saved = c.save("Test");
    c.set_studio_mode(false);
    assert!(!c.on_air_is_copy());
    assert_eq!(c.current(), "Next");
    drop(c);
    let back = Collection::load(&studio, &saved, "Scene")?;
    assert!(back.studio_options().double_click, "options come back");
    assert!(back.on_air_is_copy(), "Studio Mode comes back with a copy on air");
    drop(back);
    drop(studio);
    println!("studio mode, undo snapshots, item transitions and deinterlacing all work");
    Ok(())
}
