//! A scene collection through the Studio crate against an installed OBS.app (macOS): scenes,
//! sources, groups of edits, a sound with a meter and filters, a transition; saved, dropped,
//! loaded back and compared; 3 s recorded across a scene switch.
//!
//!   cargo run -p ferrocast-studio --example collection -- <out dir>

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, ItemEdit, Monitor, Paths, Studio, TransitionInfo};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    // A 1 kHz tone to have a sound with no device (no permission prompt).
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

    let c = Collection::new(&studio, "Gameplay")?;
    c.add_scene("Just chatting")?;
    c.add_scene("Be right back")?;
    assert!(c.add_scene("Gameplay").is_err(), "scene names are unique");
    c.add_source("Gameplay", "color_source_v3", "Background", &json!({ "color": 0xFF_40_20_10u32 }), true)?;
    let title = c.add_source(
        "Gameplay",
        "text_ft2_source_v2",
        "Title",
        &json!({ "text": "Gameplay", "font": { "face": "Helvetica", "size": 96 } }),
        false,
    )?;
    let tone_item = c.add_source(
        "Gameplay",
        "ffmpeg_source",
        "Tone",
        &json!({ "local_file": tone.display().to_string(), "looping": true }),
        false,
    )?;
    assert!(tone_item.audio, "a media file makes sound");
    c.add_existing("Just chatting", "Background")?;
    let blue = c.add_source(
        "Just chatting",
        "color_source_v3",
        "Box",
        &json!({ "color": 0xFF_00_A0_FFu32, "width": 400, "height": 300 }),
        false,
    )?;
    // Moved and sized from the preview: a plain item by its scale, keeping a flip and a turn;
    // one with a bounding box (Stretch) by that box. Each frame matches what libobs draws.
    let same = |a: [f32; 6], b: [f32; 6]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.05);
    let drawn = |want: [f32; 6]| -> Result<(), Box<dyn std::error::Error>> {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let got = c.drawn_frame("Just chatting", blue.id)?;
        assert!(same(got, want), "libobs drew {got:?}, Ferrocast said {want:?}");
        Ok(())
    };
    assert_eq!(blue.frame, [400.0, 0.0, 0.0, 300.0, 0.0, 0.0], "a new item sits at the top left");
    let f = c.set_item_box("Just chatting", blue.id, 100.0, 50.0, 800.0, 600.0)?.frame;
    assert!(same(f, [800.0, 0.0, 0.0, 600.0, 100.0, 50.0]), "moved and twice the size: {f:?}");
    drawn(f)?;
    c.edit_item("Just chatting", blue.id, ItemEdit::FlipH)?;
    c.edit_item("Just chatting", blue.id, ItemEdit::RotateCw)?;
    let turned = c.items("Just chatting")?.into_iter().find(|i| i.id == blue.id).ok_or("box")?.frame;
    drawn(turned)?;
    let f = c.set_item_box("Just chatting", blue.id, 900.0, 50.0, 400.0, 300.0)?.frame;
    assert!(same([f[4], f[5], f[0].hypot(f[1]), f[2].hypot(f[3]), 0.0, 0.0], [900.0, 50.0, 400.0, 300.0, 0.0, 0.0]));
    drawn(f)?;
    c.edit_item("Just chatting", blue.id, ItemEdit::Stretch)?;
    let f = c.set_item_box("Just chatting", blue.id, 10.0, 20.0, 960.0, 540.0)?.frame;
    assert!(same(f, [960.0, 0.0, 0.0, 540.0, 10.0, 20.0]), "the bounding box changed: {f:?}");
    drawn(f)?;
    assert!(c.set_item_box("Just chatting", blue.id, 0.0, 0.0, 0.0, 10.0).is_err(), "no zero sizes");
    // Edit Transform: every field goes in and comes back, and the box is what libobs draws.
    let mut t = c.transform("Just chatting", blue.id)?;
    assert_eq!((t.width, t.height), (400, 300), "the source's own size");
    t.bounds = "none".into();
    t.x = 300.0;
    t.y = 200.0;
    t.rotation = 30.0;
    t.scale_x = 1.5;
    t.scale_y = -1.0;
    t.alignment = 0;
    t.crop_left = 40;
    t.crop_bottom = 20;
    let f = c.set_transform("Just chatting", blue.id, &t)?.frame;
    let back = c.transform("Just chatting", blue.id)?;
    assert_eq!((back.x, back.rotation, back.crop_left, back.crop_bottom), (300.0, 30.0, 40, 20));
    assert_eq!(back.bounds, "none");
    drawn(f)?;
    t.bounds = "fit".into();
    t.bounds_width = 640.0;
    t.bounds_height = 360.0;
    t.bounds_alignment = 0;
    let f = c.set_transform("Just chatting", blue.id, &t)?.frame;
    drawn(f)?;
    t.bounds = "huge".into();
    assert!(c.set_transform("Just chatting", blue.id, &t).is_err(), "unknown bounding box");
    c.edit_item("Just chatting", blue.id, ItemEdit::Reset)?;
    c.edit_item("Gameplay", title.id, ItemEdit::Lock)?;
    c.edit_item("Gameplay", title.id, ItemEdit::Bottom)?;
    c.edit_item("Gameplay", title.id, ItemEdit::Top)?;
    c.edit_item("Gameplay", title.id, ItemEdit::Hide)?;
    c.edit_item("Gameplay", title.id, ItemEdit::Show)?;
    c.edit_item("Gameplay", tone_item.id, ItemEdit::Down)?;
    c.duplicate_scene("Gameplay", "Gameplay 2")?;
    c.move_scene("Be right back", 0)?;
    c.rename("Box", "Blue box")?;
    c.set_volume("Tone", -6.0)?;
    c.set_monitor("Tone", Monitor::Off)?;
    c.add_filter("Tone", "gain_filter", "Louder", &json!({ "db": 3.0 }))?;
    c.add_filter("Background", "color_filter_v2", "Warm", &json!({ "saturation": 0.3 }))?;
    c.set_filter_enabled("Background", "Warm", false)?;
    c.set_transition(&TransitionInfo { kind: "fade_transition".into(), ms: 500 })?;

    println!("scenes: {:?}", c.scenes());
    println!("gameplay: {:#?}", c.items("Gameplay")?);
    println!("audio: {:?}", c.audio("Gameplay")?);
    println!("filters on Tone: {:?}", c.filters("Tone")?);

    let rec =
        studio.record(&out.join("collection.mp4"), &ferrocast_studio::EncodeSettings::with_encoder("obs_x264"))?;
    // The meter needs a moment of sound.
    let names = vec!["Tone".to_string()];
    c.levels(&names);
    std::thread::sleep(Duration::from_millis(1500));
    let lv = c.levels(&names);
    println!("levels: {lv:?}");
    assert!(lv["Tone"].peak[0] > -30.0, "the tone shows on the meter");
    // A sound with a meter keeps it under its new name.
    c.rename("Tone", "Beep")?;
    assert!(c.levels(&["Beep".to_string()]).contains_key("Beep"));
    c.rename("Beep", "Tone")?;
    c.set_current("Just chatting")?;
    std::thread::sleep(Duration::from_millis(1500));
    rec.stop();

    // Each transition keeps its own settings, saved as OBS saves them.
    c.set_transition_settings("swipe_transition", &json!({ "direction": "right" }))?;
    c.set_transition_settings(
        "obs_stinger_transition",
        &json!({ "path": tone.display().to_string(), "transition_point": 200 }),
    )?;
    assert!(c.set_transition_settings("boom_transition", &json!({})).is_err(), "only real transitions");
    c.set_transition(&TransitionInfo { kind: "swipe_transition".into(), ms: 400 })?;
    c.set_current("Gameplay 2")?;
    std::thread::sleep(Duration::from_millis(500));
    c.set_transition(&TransitionInfo { kind: "fade_transition".into(), ms: 500 })?;
    // Studio Mode: a preview to edit, sent on air with a quick transition; it's saved on.
    assert!(c.set_preview("Gameplay").is_err(), "no preview while Studio Mode is off");
    c.set_studio_mode(true);
    c.set_preview("Just chatting")?;
    c.transition_now(Some(&TransitionInfo { kind: "cut_transition".into(), ms: 0 }))?;
    assert_eq!(c.current(), "Just chatting");
    c.set_preview("Gameplay")?;
    c.set_quick_transitions(&[TransitionInfo { kind: "obs_stinger_transition".into(), ms: 0 }])?;
    assert!(c.set_quick_transitions(&[TransitionInfo { kind: "boom".into(), ms: 0 }]).is_err());
    c.transition_now(None)?;
    assert_eq!(c.current(), "Gameplay", "Transition uses the scene's own way in, or the Studio's");
    c.set_preview("Gameplay 2")?;
    let saved = c.save("Test");
    let list = saved["transitions"].as_array().ok_or("transitions")?;
    assert_eq!(list.len(), 2, "both kinds' settings saved: {list:?}");
    assert!(list.iter().any(|t| t["id"] == "obs_stinger_transition" && t["name"] == "Stinger"));
    std::fs::write(out.join("collection.json"), serde_json::to_string_pretty(&saved)?)?;
    let before =
        (c.scenes(), c.items("Gameplay")?, c.items("Just chatting")?, c.filters("Background")?, c.audio("Gameplay")?);
    drop(c);

    let back = Collection::load(&studio, &saved, "Scene")?;
    // A text's size is known once it has drawn.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = (
        back.scenes(),
        back.items("Gameplay")?,
        back.items("Just chatting")?,
        back.filters("Background")?,
        back.audio("Gameplay")?,
    );
    assert_eq!(before.0, after.0, "scenes, their order and the one on air come back");
    assert_eq!(before.1, after.1, "items, order, lock and visibility come back");
    assert_eq!(before.2, after.2);
    assert_eq!(before.3, after.3, "filters come back");
    assert_eq!(before.4, after.4, "volume and monitoring come back");
    assert_eq!(back.transition(), TransitionInfo { kind: "fade_transition".into(), ms: 500 });
    assert_eq!(back.preview_scene().as_deref(), Some("Gameplay 2"), "Studio Mode comes back with its preview");
    assert_eq!(back.quick_transitions()[0].kind, "obs_stinger_transition");
    back.remove_scene("Gameplay 2")?;
    assert_eq!(back.preview_scene().as_deref(), Some("Gameplay"), "a removed preview falls back to what's on air");
    assert!(back.remove_scene("Nope").is_err());
    assert_eq!(back.transition_settings("swipe_transition")["direction"], "right", "settings come back");
    assert_eq!(back.transition_settings("obs_stinger_transition")["transition_point"], 200);
    // Engine stats, the listening device, a new canvas, and a paused MKV recording.
    let st = studio.stats();
    println!("stats: {st:?}");
    assert!(st.fps > 1.0 && st.frames > 0, "the engine renders");
    println!("outputs: {:?}", studio.kinds().outputs.iter().map(|k| k.id.as_str()).collect::<Vec<_>>());
    let devices = studio.monitoring_devices();
    println!("monitoring devices: {} (now {:?})", devices.len(), studio.monitoring_device());
    studio.set_canvas(Canvas { width: 1280, height: 720, fps: 60 }, &paths.renderer)?;
    assert_eq!(studio.canvas().fps, 60);
    let mut enc = ferrocast_studio::EncodeSettings::with_encoder("obs_x264");
    enc.quality = Some(18);
    enc.scale = Some((640, 360));
    let rec = studio.record(&out.join("paused.mov"), &enc)?;
    std::thread::sleep(Duration::from_millis(1000));
    rec.pause(true)?;
    assert!(rec.stats().paused);
    assert!(
        studio.set_canvas(Canvas { width: 1920, height: 1080, fps: 30 }, &paths.renderer).is_err(),
        "no canvas change while recording"
    );
    std::thread::sleep(Duration::from_millis(1000));
    rec.pause(false)?;
    std::thread::sleep(Duration::from_millis(1000));
    println!("record: {:?}", rec.stats());
    rec.stop();
    drop(back);
    // A collection saved by OBS: its stinger is the transition in use, with its settings.
    let obs = json!({
        "name": "From OBS",
        "sources": [],
        "current_transition": "My stinger",
        "transition_duration": 300,
        "transitions": [{ "name": "My stinger", "id": "obs_stinger_transition", "settings": { "path": tone.display().to_string() } }],
    });
    let from_obs = Collection::load(&studio, &obs, "Scene")?;
    assert_eq!(from_obs.transition().kind, "obs_stinger_transition");
    assert_eq!(from_obs.transition_settings("obs_stinger_transition")["path"], tone.display().to_string());
    drop(from_obs);
    drop(studio);
    println!("saved, loaded back the same, closed cleanly");
    Ok(())
}
