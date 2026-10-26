//! OBS's Missing Files against an installed OBS.app (macOS): an image, a slideshow, a
//! filter's image and a stinger whose files moved are found, searched for by name, replaced
//! and cleared.
//!
//!   cargo run -p ferrocast-studio --example missing -- <out dir>

use std::path::{Path, PathBuf};

use ferrocast_studio::{Canvas, Collection, Paths, Studio, find_files};
use serde_json::json;

fn png(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let ok = std::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=64x64", "-frames:v", "1"])
        .arg(path)
        .status()?
        .success();
    if ok { Ok(()) } else { Err("ffmpeg made no picture".into()) }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let (here, moved) = (out.join("media"), out.join("moved/deep/er"));
    let _ = std::fs::remove_dir_all(out.join("media"));
    let _ = std::fs::remove_dir_all(out.join("moved"));
    std::fs::create_dir_all(&here)?;
    std::fs::create_dir_all(&moved)?;
    for n in ["logo.png", "a.png", "b.png", "mask.png"] {
        png(&here.join(n))?;
    }
    let paths = Paths::from_obs_app(Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 640, height: 360, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Live")?;
    let p = |n: &str| here.join(n).display().to_string();
    c.add_source("Live", "image_source", "Logo", &json!({ "file": p("logo.png") }), false)?;
    c.add_source(
        "Live",
        "slideshow_v2",
        "Slides",
        &json!({ "files": [{ "value": p("a.png") }, { "value": p("b.png") }] }),
        false,
    )?;
    c.add_filter("Logo", "mask_filter_v2", "Mask", &json!({ "image_path": p("mask.png") }))?;
    c.set_transition_settings("obs_stinger_transition", &json!({ "path": p("stinger.webm") }))?;
    // A Luma Wipe names one of OBS's own images: never missing.
    c.set_transition_settings("wipe_transition", &json!({ "luma_image": "linear-h.png" }))?;
    assert_eq!(c.missing_files().len(), 1, "only the stinger, which never existed: {:?}", c.missing_files());
    // The media moves away.
    for n in ["logo.png", "a.png", "b.png", "mask.png"] {
        std::fs::rename(here.join(n), moved.join(n))?;
    }
    let missing = c.missing_files();
    let mut list: Vec<_> = missing.iter().map(|m| (m.source.as_str(), m.key.as_str(), m.parent.as_deref())).collect();
    list.sort();
    println!("missing: {list:?}");
    assert_eq!(
        list,
        [
            ("", "path", None),
            ("Logo", "file", None),
            ("Mask", "image_path", Some("Logo")),
            ("Slides", "files", None),
            ("Slides", "files", None)
        ]
    );
    let names: Vec<String> = missing.iter().map(|m| m.path.clone()).collect();
    let found = find_files(&out.join("moved"), &names);
    assert_eq!(found.len(), 4, "four found by name, deep down: {found:?}");
    for m in &missing {
        match found.get(&m.path) {
            Some(to) => c.replace_file(m, &to.display().to_string())?,
            // The stinger was never anywhere: cleared.
            None => c.replace_file(m, "")?,
        }
    }
    assert!(c.missing_files().is_empty(), "nothing missing now: {:?}", c.missing_files());
    assert_eq!(c.settings("Logo")?["file"], moved.join("logo.png").display().to_string());
    let slides = c.settings("Slides")?;
    assert_eq!(slides["files"].as_array().map(Vec::len), Some(2));
    // Clearing a slideshow's file takes it off the list.
    std::fs::remove_file(moved.join("b.png"))?;
    let m = c.missing_files();
    assert_eq!(m.len(), 1);
    c.replace_file(&m[0], "")?;
    assert_eq!(c.settings("Slides")?["files"].as_array().map(Vec::len), Some(1));
    drop(c);
    drop(studio);
    println!("missing files found, searched for, replaced and cleared");
    Ok(())
}
