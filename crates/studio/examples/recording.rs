//! Recording as OBS does, against an installed OBS.app (macOS): names from a pattern, MKV
//! through OBS's mux helper (needs `obs-ffmpeg-mux` beside this program), two audio tracks,
//! a split by hand, a chapter marker, and a remux to MP4. Checked with ffprobe.
//!
//!   cargo run -p ferrocast-studio --example recording -- <out dir>

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, RecordPlan, Studio};
use serde_json::json;

fn probe(file: &Path, what: &str) -> serde_json::Value {
    let out =
        std::process::Command::new("ffprobe").args(["-v", "error", "-print_format", "json", what]).arg(file).output();
    out.ok().and_then(|o| serde_json::from_slice(&o.stdout).ok()).unwrap_or_default()
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let _ = std::fs::remove_dir_all(out.join("rec"));
    std::fs::create_dir_all(out.join("rec"))?;
    let tone = out.join("tone.wav");
    if !tone.exists() {
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=700:duration=10", "-ac", "2"])
            .arg(&tone)
            .status()?;
    }
    let paths = Paths::from_obs_app(Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 640, height: 360, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Live")?;
    c.add_source("Live", "color_source_v3", "Blue", &json!({ "color": 0xFF_E0_20_20u32 }), true)?;
    c.add_source(
        "Live",
        "ffmpeg_source",
        "Tone",
        &json!({ "local_file": tone.display().to_string(), "looping": true }),
        false,
    )?;
    c.set_tracks("Tone", &[1, 2])?;
    let mut enc = EncodeSettings::with_encoder("obs_x264");
    enc.video_kbps = 1500;
    let plan = |format: &str| RecordPlan {
        folder: out.join("rec"),
        name: "Test %CCYY-%MM-%DD %hh-%mm-%ss".into(),
        format: format.into(),
        tracks: vec![1, 2],
        encode: enc.clone(),
        ..RecordPlan::default()
    };

    // MP4: two tracks, a chapter, then split by hand into a second file.
    assert!(Studio::muxer_ready(), "obs-ffmpeg-mux is beside this program");
    let p = plan("mp4");
    let path = studio.record_path(&p);
    assert!(file_name(&path).starts_with("Test 20"), "{path:?}");
    let rec = studio.record_to(&path, &p)?;
    std::thread::sleep(Duration::from_millis(1500));
    rec.chapter("Big moment")?;
    std::thread::sleep(Duration::from_millis(1500));
    rec.split()?;
    std::thread::sleep(Duration::from_millis(2000));
    drop(rec);
    let mut files: Vec<_> = std::fs::read_dir(out.join("rec"))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    println!("files: {files:?}");
    assert!(files.len() >= 2, "split made a second file");
    let streams = probe(&path, "-show_streams");
    let audio = streams["streams"].as_array().map(|s| s.iter().filter(|x| x["codec_type"] == "audio").count());
    assert_eq!(audio, Some(2), "two audio tracks: {streams}");
    let chapters = probe(&path, "-show_chapters");
    println!("chapters: {}", chapters["chapters"]);
    assert!(chapters["chapters"].as_array().is_some_and(|c| c.iter().any(|x| x["tags"]["title"] == "Big moment")));
    // The same name again gets " (2)".
    std::fs::write(out.join("rec/Same.mp4"), b"x")?;
    let same = studio.record_path(&RecordPlan { name: "Same".into(), ..plan("mp4") });
    assert_eq!(file_name(&same), "Same (2).mp4");

    // MKV through OBS's helper, then remuxed to MP4 without encoding again.
    std::thread::sleep(Duration::from_millis(1100));
    let p = plan("mkv");
    let mkv = studio.record_path(&p);
    let rec = studio.record_to(&mkv, &p)?;
    std::thread::sleep(Duration::from_millis(3000));
    let st = rec.stats();
    drop(rec);
    println!("mkv: {st:?} {:?}", std::fs::metadata(&mkv).map(|m| m.len()));
    let fmt = probe(&mkv, "-show_format");
    assert_eq!(fmt["format"]["format_name"], "matroska,webm", "{fmt}");
    let mp4 = mkv.with_extension("mp4");
    let mut last = 0.0;
    studio.remux(&mkv, &mp4, |pct| {
        last = pct;
        true
    })?;
    println!("remux ended at {last}%");
    let fmt = probe(&mp4, "-show_format");
    assert!(fmt["format"]["format_name"].as_str().unwrap_or("").contains("mp4"), "{fmt}");
    assert!(studio.remux(&mkv, &mp4, |_| true).is_err(), "never over a file that's there");
    assert!(studio.record_to(&mkv, &RecordPlan { format: "avi".into(), ..plan("mkv") }).is_err());
    // HLS: a playlist and its segments; and an MP4 with muxer settings of the streamer's own.
    let hls = RecordPlan { tracks: vec![1], ..plan("hls") };
    let m3u8 = studio.record_path(&hls);
    assert_eq!(m3u8.extension().and_then(|e| e.to_str()), Some("m3u8"));
    let rec = studio.record_to(&m3u8, &hls)?;
    std::thread::sleep(Duration::from_secs(4));
    drop(rec);
    let playlist = std::fs::read_to_string(&m3u8)?;
    assert!(playlist.contains("#EXTM3U") && playlist.contains(".ts"), "{playlist}");
    let tuned = RecordPlan { name: "Tuned".into(), muxer: "movflags=+faststart".into(), ..plan("mp4") };
    let file = studio.record_path(&tuned);
    let rec = studio.record_to(&file, &tuned)?;
    std::thread::sleep(Duration::from_secs(2));
    drop(rec);
    assert!(probe(&file, "-show_format")["format"]["duration"].as_str().is_some(), "plays");
    drop(c);
    drop(studio);
    println!("names, two tracks, a chapter, a split, MKV and a remux all work");
    Ok(())
}
