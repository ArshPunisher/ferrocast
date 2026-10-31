//! The "own encoder" switch through `Studio::stream` and `Studio::record_to`, as the app calls them: a scene on a real
//! engine streams to an FFmpeg RTMP server here for a few seconds; the stats the app shows and
//! what arrived are checked. macOS (FERROCAST_OBS_DIR, else an installed OBS.app).
//!
//!   cargo run -p ferrocast-studio --example own_switch -- <out dir> [seconds]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, RecordPlan, StreamSettings, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    std::fs::create_dir_all(&out)?;
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let studio = Studio::start(&Paths::from_obs_app(Path::new(&dir), true), Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Main")?;
    c.add_source("Main", "color_source_v3", "Back", &json!({ "color": 0xFF_80_40_20u32, "width": 1280, "height": 720 }), false)?;

    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let got = out.join("own-switch.flv");
    let _ = std::fs::remove_file(&got);
    let mut server = Command::new("ffmpeg")
        .args(["-v", "error", "-listen", "1", "-i", &format!("rtmp://127.0.0.1:{port}/live/k"), "-c", "copy", "-y"])
        .arg(&got)
        .stdin(Stdio::null())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(800));
    let settings = StreamSettings {
        encode: EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc"),
        reconnect: Some((3, 2)),
        delay_s: 0,
        vod_track: None,
        own_encoder: true,
    };
    // A recording through our encoder at the same time.
    let file = out.join("own-switch.mp4");
    let _ = std::fs::remove_file(&file);
    let plan = RecordPlan { own_encoder: true, encode: settings.encode.clone(), ..RecordPlan::default() };
    let rec = studio.record_to(&file, &plan)?;
    assert!(rec.pause(true).is_err(), "pause says it needs OBS's encoder");
    let stream = studio.stream(&format!("rtmp://127.0.0.1:{port}/live"), "k", &settings)?;
    assert!(stream.own_encoder(), "the switch took our encoder");
    std::thread::sleep(Duration::from_secs(secs));
    let st = stream.stats();
    println!("stats: {st:?}, error: {:?}", stream.last_error());
    let rs = rec.stats();
    println!("recording: {rs:?}");
    drop(stream);
    drop(rec);
    for _ in 0..50 {
        if server.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = server.kill();
    drop(c);
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "csv=p=0"])
        .arg(&got)
        .output()?;
    let text = String::from_utf8_lossy(&probe.stdout).trim().replace('\n', " | ");
    println!("arrived: {text}");
    let frames: u64 = text.split(" | ").filter_map(|l| l.split(',').nth(3)).filter_map(|n| n.parse().ok()).next().unwrap_or(0);
    let played = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-show_entries", "stream=codec_name,nb_read_frames", "-of", "csv=p=0"])
        .arg(&file)
        .output()?;
    let rec_text = String::from_utf8_lossy(&played.stdout).trim().replace('\n', " | ");
    println!("recorded: {rec_text}");
    if !rs.active || !rec_text.contains("h264") || !rec_text.contains("aac") {
        return Err("the recording through our encoder didn't come out".into());
    }
    if !st.active || st.frames < (secs as u32 - 1) * 30 || frames < (secs - 1) * 30 || !text.contains("aac") {
        return Err("the stream through our encoder came up short".into());
    }
    println!("own_switch: OK");
    Ok(())
}
