//! Streams the Studio to a local media server by RTMP and, at the same time, straight by SRT
//! and WebRTC (WHIP), checking each arrives; RIST is tried too. Needs an installed OBS.app and
//! a media server such as mediamtx (RTMP on 1936, SRT on 8890, WHIP on 8889, API on 9997).
//!
//!   cargo run -p ferrocast-studio --example direct -- <out dir>

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, StreamSettings, Studio};
use serde_json::json;

fn paths_live() -> Vec<String> {
    let out = std::process::Command::new("curl").args(["-s", "http://127.0.0.1:9997/v3/paths/list"]).output();
    let Ok(out) = out else { return Vec::new() };
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["items"]
        .as_array()
        .map(|a| a.iter().filter(|p| p["ready"] == true).filter_map(|p| p["name"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let outputs: Vec<String> = studio.kinds().outputs.iter().map(|k| k.id.clone()).collect();
    println!("outputs: {outputs:?}");
    let c = Collection::new(&studio, "Live")?;
    c.add_source("Live", "color_source_v3", "Red", &json!({ "color": 0xFF_20_20_E0u32 }), true)?;
    let encoder = "obs_x264";
    let mut enc = EncodeSettings::with_encoder(encoder);
    enc.video_kbps = 2500;
    let settings = StreamSettings { encode: enc, reconnect: Some((3, 2)), delay_s: 0, vod_track: None };
    let mut s = studio.stream("rtmp://127.0.0.1:1936/main", "", &settings)?;
    s.add_direct("srt", "srt://127.0.0.1:8890?streamid=publish:srt1", "")?;
    s.add_direct("whip", "http://127.0.0.1:8889/whip1/whip", "")?;
    assert!(s.add_direct("bad", "rtmp://x/y", "").is_err(), "RTMP isn't a direct one");
    let rist = s.add_direct("rist", "rist://127.0.0.1:8900", "");
    println!("rist: {rist:?}");
    std::thread::sleep(Duration::from_secs(6));
    for id in ["srt", "whip"] {
        let st = s.direct_stats(id).ok_or(id)?;
        println!("{id}: {st:?}");
        assert!(st.active && st.bytes > 0, "{id} sends");
    }
    println!("main: {:?}", s.stats());
    let live = paths_live();
    println!("server has: {live:?}");
    for p in ["main", "srt1", "whip1"] {
        assert!(live.iter().any(|l| l == p), "{p} arrived");
    }
    s.remove_direct("srt");
    assert!(s.direct_stats("srt").is_none());
    std::thread::sleep(Duration::from_secs(2));
    assert!(!paths_live().iter().any(|l| l == "srt1"), "srt stopped");
    assert!(s.stats().active, "the main stream keeps going");
    drop(s);
    drop(c);
    drop(studio);
    println!("RTMP, SRT and WebRTC all arrived; closed cleanly");
    Ok(())
}
