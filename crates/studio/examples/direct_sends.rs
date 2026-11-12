//! SRT, RIST and WHIP sends from the Studio to real servers here: MediaMTX (SRT and WHIP) and
//! librist's `ristreceiver` (RIST). The main stream goes to MediaMTX's RTMP; each send is added
//! with `add_direct`, as the app does, and runs until Enter (or the seconds given) so the servers'
//! own players can be checked.
//!
//!   cargo run -p ferrocast-studio --example direct_sends -- <out dir> <srt url> <rist url> <whip url> [seconds]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrocast_studio::stage::Stage;
use ferrocast_studio::{Canvas, EncodeSettings, StreamSettings};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERROCAST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    let out = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let sends = [("srt", args.get(2)), ("rist", args.get(3)), ("whip", args.get(4))];
    let secs: u64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(30);
    std::fs::create_dir_all(&out)?;
    let clip = out.join("pattern.mp4");
    if !clip.exists() {
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=20"])
            .args(["-f", "lavfi", "-i", "sine=frequency=600:duration=20", "-ac", "2", "-shortest"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&clip)
            .status()?;
    }
    let c = Stage::new(Canvas { width: 1280, height: 720, fps: 30 }, "Live")?;
    c.add_source("Live", "color_source_v3", "Back", &json!({ "color": 0xFF_30_30_30u32 }), true)?;
    c.add_source(
        "Live",
        "ffmpeg_source",
        "Pattern",
        &json!({ "local_file": clip.display().to_string(), "looping": true }),
        true,
    )?;
    // The app's own defaults (B-frames on), as a user would send.
    // FERROCAST_ENCODER picks another (the Mac's HEVC, say); H.264 otherwise.
    let encoder =
        std::env::var("FERROCAST_ENCODER").unwrap_or_else(|_| "com.apple.videotoolbox.videoencoder.ave.avc".into());
    let mut enc = EncodeSettings::with_encoder(&encoder);
    enc.video_kbps = 2500;
    let settings = StreamSettings {
        encode: enc,
        reconnect: None,
        delay_s: 0,
        vod_track: None,
        own_encoder: true,
        video_tracks: Vec::new(),
    };
    let mut live = c.stream("rtmp://127.0.0.1:1935/main", "", &settings)?;
    // FERROCAST_SIMULCAST=1: a lighter stream (360 high) goes beside it to WebRTC as a second layer.
    let lighter = if std::env::var("FERROCAST_SIMULCAST").is_ok() {
        let mut light = settings.clone();
        light.encode.scale = Some((640, 360));
        light.encode.video_kbps = 800;
        Some(c.stream("rtmp://127.0.0.1:1935/light", "", &light)?)
    } else {
        None
    };
    for (id, url) in sends {
        let Some(url) = url.filter(|u| !u.is_empty()) else { continue };
        // FERROCAST_SRT_KEY, FERROCAST_RIST_KEY, FERROCAST_WHIP_KEY: each one's key (a stream id,
        // a passphrase or login password, a token).
        let key = std::env::var(format!("FERROCAST_{}_KEY", id.to_uppercase())).unwrap_or_default();
        let added = match (&lighter, id) {
            (Some(l), "whip") => live.add_direct_layered(id, url, &key, l),
            _ => live.add_direct(id, url, &key),
        };
        match added {
            Ok(()) => println!("{id}: added {url}"),
            Err(e) => println!("{id}: refused: {e}"),
        }
    }
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_secs(3));
        for (id, _) in sends {
            if let Some(st) = live.direct_stats(id) {
                println!("{id}: {st:?}");
            }
        }
    }
    drop(live);
    drop(c);
    Ok(())
}
