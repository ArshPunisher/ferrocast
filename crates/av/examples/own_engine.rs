//! Ferrocast's own engine end to end, with no libobs at all: the main display captured
//! (ScreenCaptureKit), a title in our text renderer, and a test patch that flashes white while
//! our mixer plays a beep, once a second, all drawn by our compositor, mixed by our mixer,
//! encoded by our VideoToolbox and AAC encoders, streamed through Go live's hub over RTMP to an
//! FFmpeg server here and recorded to MP4. Both results are checked for length, frames and
//! picture-to-sound sync, then deleted (they hold what was on screen).
//!
//!   cargo run --release -p ferrocast-av --example own_engine -- <out dir> [seconds]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use ferrocast_av::audio::AudioConfig;
    use ferrocast_av::mix::{Mixer, SourceSettings};
    use ferrocast_av::pipeline::{Pipeline, Tag};
    use ferrocast_av::screen::{ScreenCapture, Target, shareable};
    use ferrocast_av::source::text::{TextRenderer, TextStyle};
    use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig};
    use ferrocast_av::{Bounds, Compositor, Content, Effect, Item, Picture, Placement, Scene};
    use ferrocast_golive::push::{self, Destination, Health, PushState, Retry};
    use ferrocast_golive::{Hub, Target as Url};

    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    std::fs::create_dir_all(&out)?;
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u64 = 30;
    const RATE: u32 = 48_000;

    // The receiving end.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let received = out.join("own-received.flv");
    let recorded = out.join("own-recorded.mp4");
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
        since_ms: 0,
    });
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let dest = Destination {
        id: "local".into(),
        target: Url::parse(&format!("rtmp://127.0.0.1:{port}/live"))?,
        key: "k".into(),
        retry: Retry::default(),
        extra_audio: false,
    };
    let pusher = rt.spawn(push::run(dest, hub.clone(), health_tx, stop));

    // Sources.
    let (displays, _) = shareable()?;
    let display = displays.first().ok_or("no display")?;
    let screen = ScreenCapture::start(Target::Display(display.id), W, H, FPS as u32, true)?;
    let gpu = Compositor::new()?;
    let mut text = TextRenderer::new();
    let title = text.render(
        "Ferrocast's own engine · no libobs",
        &TextStyle { size: 40.0, bold: true, outline: Some((3.0, [0, 0, 0, 255])), ..Default::default() },
    );
    gpu.upload(2, &title)?;
    let mut mixer = Mixer::new(RATE, 40_000_000);
    mixer.set(1, SourceSettings::default());

    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: W,
        height: H,
        fps_num: FPS as u32,
        fps_den: 1,
        bitrate_kbps: 4000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
    };
    let mut pipe =
        Pipeline::new(video, AudioConfig { sample_rate: RATE, channels: 2, bitrate_kbps: 160 }, Some(&recorded))?;
    hub.start();
    let send = |tags: Vec<Tag>| {
        for t in tags {
            match t {
                Tag::Video { ms, body } => hub.video(ms, body.into()),
                Tag::Audio { ms, body } => hub.audio(ms, body.into()),
            }
        }
    };

    let clock = Instant::now();
    let epoch = 1_000_000_000u64;
    let now_ns = || epoch + clock.elapsed().as_nanos() as u64;
    let frame_ns = 1_000_000_000 / FPS;
    let (mut screen_size, mut frames, mut slowest) = (None, 0u64, Duration::ZERO);
    let mut beeped_until = epoch;
    let mut next = epoch;
    while clock.elapsed() < Duration::from_secs(seconds) {
        // The beep: 100 ms of 1 kHz every 30 frames (a second), on the frame clock exactly, pushed
        // ahead of time.
        while beeped_until < now_ns() + 200_000_000 {
            let n = (RATE / 10) as usize;
            let second_start = beeped_until;
            let pcm: Vec<f32> = (0..n)
                .flat_map(|i| {
                    let v = (i as f32 * 1000.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5;
                    [v, v]
                })
                .collect();
            mixer.push(1, &pcm, 2, RATE, second_start);
            beeped_until += FPS * frame_ns;
        }
        let t = now_ns();
        if t < next {
            std::thread::sleep(Duration::from_nanos(next - t));
            continue;
        }
        let ts = next;
        next += frame_ns;
        let started = Instant::now();
        if let Some(p) = screen.take() {
            screen_size = Some((p.width, p.height));
            gpu.upload(1, &p)?;
        }
        // The flash: the same 3 frames (100 ms) of every 30.
        let flash = frames % FPS < 3;
        let mut items = vec![Item::picture(Picture::solid(W, H, [12, 14, 28, 255]), Placement::default())];
        if let Some((sw, sh)) = screen_size {
            items.push(Item {
                effects: vec![Effect::RoundedCorners { radius: 24.0 }],
                ..Item::picture(
                    Picture::solid(1, 1, [0; 4]),
                    Placement {
                        x: 40.0,
                        y: 100.0,
                        bounds: Bounds::Fit,
                        bounds_width: 960.0,
                        bounds_height: 540.0,
                        ..Default::default()
                    },
                )
            });
            if let Some(last) = items.last_mut() {
                last.content = Content::Uploaded { id: 1, width: sw, height: sh, premultiplied: true };
            }
        }
        items.push(Item {
            content: Content::Uploaded { id: 2, width: title.width, height: title.height, premultiplied: false },
            ..Item::picture(Picture::solid(1, 1, [0; 4]), Placement { x: 40.0, y: 24.0, ..Default::default() })
        });
        let patch = if flash { [255, 255, 255, 255] } else { [0, 0, 0, 255] };
        items.push(Item::picture(
            Picture::solid(200, 200, patch),
            Placement { x: 1040.0, y: 100.0, ..Default::default() },
        ));
        let f = gpu.render_nv12(&Scene { width: W, height: H, items })?;
        send(pipe.video(Frame::Nv12 { y: &f.y, y_stride: W as usize, uv: &f.uv, uv_stride: W as usize }, ts)?);
        for b in mixer.mix_until(now_ns()) {
            send(pipe.audio(&b.tracks[0], b.ts_ns)?);
        }
        slowest = slowest.max(started.elapsed());
        frames += 1;
    }
    send(pipe.finish()?);
    hub.end();
    println!(
        "made {frames} frames in {seconds} s with no libobs ({} screen frames came); slowest frame {:.1} ms; destination {:?}",
        screen.frames(),
        slowest.as_secs_f64() * 1000.0,
        health.borrow().state
    );
    drop(screen);
    std::thread::sleep(Duration::from_secs(2));
    let _ = stop_tx.send(true);
    rt.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(5), pusher).await;
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.try_wait()?.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = server.kill();

    let mut ok = true;
    for (name, file) in [("recorded (MP4)", &recorded), ("received over RTMP", &received)] {
        let (dur, n) = probe(file);
        let s = sync(file);
        let worst = s.iter().map(|d| d.abs()).fold(0.0, f64::max);
        println!(
            "{name}: {dur:.2} s, {n} frames; {} beeps against flashes, worst {worst:.0} ms apart: {s:.0?}",
            s.len()
        );
        ok &= dur > seconds as f64 - 3.0 && s.len() + 3 >= seconds as usize && worst <= 40.0;
        let _ = std::fs::remove_file(file);
    }
    if !ok {
        return Err("a result was short or out of sync".into());
    }
    println!("own_engine: OK (files deleted: they held the screen)");
    Ok(())
}

#[cfg(target_os = "macos")]
fn probe(file: &std::path::Path) -> (f64, u64) {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames:format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let mut nums = text.split_whitespace().filter_map(|l| l.trim_matches(',').parse::<f64>().ok());
    let n = nums.next().unwrap_or(0.0) as u64;
    (nums.next().unwrap_or(0.0), n)
}

/// Beep starts against flash starts (the patch's own area only), in ms.
#[cfg(target_os = "macos")]
fn sync(file: &std::path::Path) -> Vec<f64> {
    use std::process::Command;
    let raw = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-map", "0:v:0", "-vf", "crop=200:200:1040:100,scale=8:8,format=gray", "-f", "rawvideo", "-"])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let bright: Vec<f64> = raw.chunks(64).map(|c| c.iter().map(|v| f64::from(*v)).sum::<f64>() / 64.0).collect();
    let flashes = onsets(&bright, 128.0, 1000.0 / 30.0);
    let pcm: Vec<f32> = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", "8000", "-f", "f32le", "-"])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let loud: Vec<f64> = pcm.chunks(8).map(|c| c.iter().map(|v| f64::from(v.abs())).fold(0.0, f64::max)).collect();
    let beeps = onsets(&loud, 0.1, 1.0);
    flashes
        .iter()
        .filter_map(|f| beeps.iter().map(|b| b - f).min_by(|a, b| a.abs().total_cmp(&b.abs())))
        .filter(|d| d.abs() < 400.0)
        .collect()
}

#[cfg(target_os = "macos")]
fn onsets(v: &[f64], over: f64, step: f64) -> Vec<f64> {
    let mut prev = false;
    let mut out = Vec::new();
    for (i, x) in v.iter().enumerate() {
        let now = *x > over;
        if now && !prev {
            out.push(i as f64 * step);
        }
        prev = now;
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("this example needs macOS's capture and encoders so far");
}
