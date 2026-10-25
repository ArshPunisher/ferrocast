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
    c.add_source(
        "Just chatting",
        "color_source_v3",
        "Box",
        &json!({ "color": 0xFF_00_A0_FFu32, "width": 400, "height": 300 }),
        false,
    )?;
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

    let rec = studio.record(&out.join("collection.mp4"), "obs_x264")?;
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

    let saved = c.save("Test");
    std::fs::write(out.join("collection.json"), serde_json::to_string_pretty(&saved)?)?;
    let before =
        (c.scenes(), c.items("Gameplay")?, c.items("Just chatting")?, c.filters("Background")?, c.audio("Gameplay")?);
    drop(c);

    let back = Collection::load(&studio, &saved, "Scene")?;
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
    back.remove_scene("Gameplay 2")?;
    assert!(back.remove_scene("Nope").is_err());
    drop(back);
    drop(studio);
    println!("saved, loaded back the same, closed cleanly");
    Ok(())
}
