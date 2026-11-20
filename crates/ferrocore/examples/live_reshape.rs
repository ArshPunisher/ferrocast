//! Optimize while live, end to end: our engine streams a test picture through Go live's sender
//! over real RTMP to an FFmpeg server on this machine, at 1080p60; halfway it changes to 720p30
//! without stopping (`Engine::reshape_output`). FFmpeg's file is then read back: one stream,
//! both sizes, time only going forward, no gap where the picture changed.
//!
//!   cargo run -p ferrocore --example live_reshape -- <out dir>

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use ferrocast_golive::Hub;
    use ferrocast_golive::push::{self, Destination, Health, PushState, Retry};
    use ferrocore::Placement;
    use ferrocore::audio::AudioConfig;
    use ferrocore::engine::{Canvas, Engine, EngineConfig, OutputKind, SceneItem, SourceKind};
    use ferrocore::pipeline::Tag;
    use ferrocore::video::{PixelFormat, RateControl, VideoCodec, VideoConfig};

    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let received = out.join("reshape-received.flv");
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let mut server = Command::new("ffmpeg")
        .args(["-v", "error", "-listen", "1", "-i", &format!("rtmp://127.0.0.1:{port}/live/k"), "-c", "copy", "-y"])
        .arg(&received)
        .stdin(Stdio::null())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(800));

    let rt = tokio::runtime::Runtime::new()?;
    let hub = Hub::new();
    let (health_tx, health) = tokio::sync::watch::channel(Health {
        state: PushState::Waiting,
        kbps: 0,
        dropped: 0,
        sent: 0,
        link: None,
        via: None,
        since_ms: 0,
    });
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let dest = Destination {
        id: "local".into(),
        target: ferrocast_golive::Target::parse(&format!("rtmp://127.0.0.1:{port}/live"))?,
        key: "k".into(),
        retry: Retry::default(),
        extra_audio: false,
        extra_video: false,
        backup: None,
    };
    let pusher = rt.spawn(push::run(dest, hub.clone(), health_tx, stop));

    let config = EngineConfig { width: 1920, height: 1080, fps: 60, ..Default::default() };
    let video = |w: u32, h: u32, fps: u32| VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: w,
        height: h,
        fps_num: fps,
        fps_den: 1,
        bitrate_kbps: 4000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
        profile: ferrocore::video::H264Profile::High,
        preset: None,
        tune: None,
    };
    let audio = AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 160 };
    let e = Engine::new(config, video(1920, 1080, 60), audio)?;
    e.set_source(1, SourceKind::Color { width: 1920, height: 1080, rgba: [30, 90, 200, 255] })?;
    e.set_source(2, SourceKind::Tone { hz: 440.0, every_ms: 1000, beep_ms: 100 })?;
    e.set_scene("Main", vec![SceneItem::new(1, Placement::default()), SceneItem::new(2, Placement::default())]);
    e.go("Main")?;
    hub.start();
    let h2 = hub.clone();
    let send = move |t: Tag| match t {
        Tag::Video { ms, body } => h2.video(ms, body.into()),
        Tag::Audio { ms, body } => h2.audio(ms, body.into()),
    };
    let id =
        e.start_output_with(OutputKind::Stream(Box::new(send)), Canvas::main(config), video(1920, 1080, 60), audio)?;
    std::thread::sleep(Duration::from_secs(4));
    if std::env::var_os("NO_RESHAPE").is_none() {
        e.reshape_output(id, 1280, 720, 2, video(1280, 720, 30))?;
    }
    std::thread::sleep(Duration::from_secs(4));
    e.stop_output(id)?;
    hub.end();
    std::thread::sleep(Duration::from_millis(800));
    let _ = stop_tx.send(true);
    let _ = rt.block_on(pusher);
    println!("sender: {:?}, dropped {}", health.borrow().state, health.borrow().dropped);
    let _ = server.wait();

    // What FFmpeg got: every video frame's size and time.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=width,height,pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(&received)
        .output()?;
    let text = String::from_utf8_lossy(&probe.stdout);
    let frames: Vec<(u32, u32, f64)> = text
        .lines()
        .filter_map(|l| {
            // ffprobe's own order: time, width, height.
            let mut p = l.split(',');
            let t: f64 = p.next()?.parse().ok()?;
            Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, t))
        })
        .collect();
    let big = frames.iter().filter(|f| f.0 == 1920).count();
    let small = frames.iter().filter(|f| f.0 == 1280).count();
    let forward = frames.windows(2).all(|w| w[1].2 >= w[0].2);
    let gap = frames.windows(2).map(|w| w[1].2 - w[0].2).fold(0.0f64, f64::max);
    println!(
        "decoded {} frames: {big} at 1080p, {small} at 720p; time forward: {forward}; longest gap {gap:.3} s",
        frames.len()
    );
    let errors = String::from_utf8_lossy(&probe.stderr);
    if !errors.trim().is_empty() {
        println!("ffprobe said: {}", errors.trim());
    }
    let ok = big > 150 && small > 80 && forward && gap < 0.2;
    if ok {
        std::fs::remove_file(&received).ok();
    }
    if ok {
        println!("PASS");
        Ok(())
    } else {
        Err("the stream didn't carry on through the change".into())
    }
}
