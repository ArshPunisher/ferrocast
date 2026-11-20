//! The engine under a streamer's real load (owner's 15 Nov stream): a 2560x1440 canvas at 60
//! fps scaled to a 1080p stream, a 1080p web page handing in new pictures 15 times a second,
//! a camera-sized picture moving every frame, and a replay buffer beside the stream, both on
//! the Mac's hardware encoder. Prints how many frames were made on time and how long each took.
//!
//! `game` as the second argument also keeps the graphics card busy from another device, as a
//! game does while it's streamed (4K frames of many full-screen layers, without a break).
//!
//!   cargo run --release -p ferrocore --example live_load -- [seconds] [game]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferrocore::audio::AudioConfig;
use ferrocore::engine::{Canvas, Engine, EngineConfig, OutputKind, SceneItem, SourceKind};
use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocore::{Bounds, Picture, Placement};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(15);
    let game = std::env::args().nth(2).as_deref() == Some("game");
    let config = EngineConfig { width: 2560, height: 1440, fps: 60, ..Default::default() };
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: 1920,
        height: 1080,
        fps_num: 60,
        fps_den: 1,
        bitrate_kbps: 6000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
        profile: ferrocore::video::H264Profile::High,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 160 };
    let e = Engine::new(config, video, audio)?;

    // The web page: 1080p pictures, a new one 15 times a second (what WebKit snapshots gave).
    e.set_source(1, SourceKind::Fed { width: 1920, height: 1080 })?;
    let feed = e.fed(1).ok_or("no feed")?;
    let alive = Arc::new(AtomicBool::new(true));
    let page = {
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut k = 0u8;
            while alive.load(Ordering::Acquire) {
                let mut rgba = vec![0u8; 1920 * 1080 * 4];
                for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    px.copy_from_slice(&[(i % 1920) as u8, k, (i / 1920) as u8, 255]);
                }
                feed.picture(Picture { width: 1920, height: 1080, rgba, premultiplied: true });
                k = k.wrapping_add(9);
                std::thread::sleep(Duration::from_millis(66));
            }
        })
    };
    // The game: another device drawing heavy frames as fast as it can.
    let gamer = game.then(|| {
        let alive = alive.clone();
        std::thread::spawn(move || {
            let Ok(c) = ferrocore::Compositor::new() else { return };
            let layer = Picture::solid(3840, 2160, [30, 60, 90, 200]);
            if c.upload(1, &layer).is_err() {
                return;
            }
            let item = |r: f32| ferrocore::Item {
                content: ferrocore::Content::Uploaded { id: 1, width: 3840, height: 2160, premultiplied: false },
                effects: Vec::new(),
                placement: Placement { rotation: r, ..Default::default() },
                filter: ferrocore::Filter::Bicubic,
                blend: ferrocore::Blend::Normal,
                srgb_off: false,
                linear: true,
                opacity: 0.5,
            };
            let scene =
                ferrocore::Scene { width: 3840, height: 2160, items: (0..24).map(|i| item(i as f32)).collect() };
            while alive.load(Ordering::Acquire) {
                let _ = c.render_nv12(&scene);
            }
        })
    });
    // A camera-like picture on top.
    e.set_source(2, SourceKind::Color { width: 640, height: 360, rgba: [200, 120, 40, 255] })?;
    let fill = Placement { bounds: Bounds::Fit, bounds_width: 2560.0, bounds_height: 1440.0, ..Default::default() };
    e.set_scene(
        "Main",
        vec![SceneItem::new(1, fill), SceneItem::new(2, Placement { x: 1800.0, y: 1000.0, ..Default::default() })],
    );
    e.go("Main")?;

    let bytes = Arc::new(AtomicU64::new(0));
    let sink = {
        let bytes = bytes.clone();
        move |t: ferrocore::pipeline::Tag| {
            let n = match &t {
                ferrocore::pipeline::Tag::Video { body, .. } | ferrocore::pipeline::Tag::Audio { body, .. } => {
                    body.len()
                }
            };
            bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
    };
    let canvas = Canvas::scaled(config, 1920, 1080);
    let stream = e.start_output_with(OutputKind::Stream(Box::new(sink)), canvas.clone(), video, audio)?;
    let replay = e.start_output_with(OutputKind::Replay { seconds: 30, mb: 0 }, canvas, video, audio)?;

    std::thread::sleep(Duration::from_secs(2));
    let before = e.stats();
    let t = Instant::now();
    let mut worst_seen = 0f32;
    let mut samples = Vec::new();
    while t.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(250));
        let s = e.stats();
        samples.push(s.frame_ms);
        worst_seen = worst_seen.max(s.frame_ms);
    }
    let after = e.stats();
    alive.store(false, Ordering::Release);
    let _ = page.join();
    if let Some(g) = gamer {
        let _ = g.join();
    }
    e.stop_output(stream)?;
    e.stop_output(replay)?;

    let made = after.frames - before.frames;
    let skipped = after.skipped - before.skipped;
    let secs = t.elapsed().as_secs_f64();
    samples.sort_by(f32::total_cmp);
    let median = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
    println!(
        "{seconds} s{} at 2560x1440 → 1080p60, web page 15 fps, stream + replay: {:.1} fps made, {} skipped ({:.1}%), frame time median {median:.2} ms, worst seen {worst_seen:.2} ms, stream {:.0} kbps",
        if game { " with a game on the card" } else { "" },
        made as f64 / secs,
        skipped,
        skipped as f64 * 100.0 / (made + skipped).max(1) as f64,
        bytes.load(Ordering::Relaxed) as f64 * 8.0 / 1000.0 / (secs + 2.0),
    );
    Ok(())
}
