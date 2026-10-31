//! Ferrocast's own encoders and outputs (ENGINE.md E1) on a real engine: libobs mixes a scene
//! that flashes white and beeps once a second; its frames and sound go through our
//! VideoToolbox and AAC encoders into Go live's hub, which streams over RTMP to an FFmpeg server
//! here; at the same time our MP4 writer records. Both results are then checked: length, frame
//! count, and how far each beep is from its flash (A/V sync), at the start and at the end.
//!
//!   cargo run -p ferrocast-studio --example own_stream -- <out dir> [seconds] [kbps]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ferrocast_av::audio::AudioConfig;
use ferrocast_av::pipeline::{Pipeline, Tag};
use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocast_golive::push::{self, Destination, Health, PushState, Retry};
use ferrocast_golive::{Hub, Target};
use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const W: u32 = 1280;
const H: u32 = 720;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| ".".into());
    if first == "measure" {
        // `measure <file>`: only the sync check, for comparing with other recordings.
        let file = PathBuf::from(args.next().unwrap_or_default());
        let s = sync(&file);
        println!("{}: {:?} s; beeps vs flashes ms: {s:.0?}", file.display(), probe(&file));
        return Ok(());
    }
    let out = PathBuf::from(first);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    // Lower for long runs (three files of this rate are written).
    let kbps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6000);
    std::fs::create_dir_all(&out)?;
    let clip = out.join("flash-beep.mp4");
    make_clip(&clip)?;

    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let paths = Paths::from_obs_app(Path::new(&dir), true);
    let studio = Studio::start(&paths, Canvas { width: W, height: H, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Main")?;
    let settings = json!({ "local_file": clip.display().to_string(), "is_local_file": true, "looping": true });
    c.add_source("Main", "ffmpeg_source", "Clip", &settings, true)?;

    // The receiving end: FFmpeg as an RTMP server, copying what arrives into a file.
    let port = free_port()?;
    let received = out.join("received.flv");
    let _ = std::fs::remove_file(&received);
    let mut server = listen(port, &received)?;
    std::thread::sleep(Duration::from_millis(800));

    let rt = tokio::runtime::Runtime::new()?;
    let hub = Hub::new();
    let (health_tx, health) =
        tokio::sync::watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
    let (stop_tx, stop) = tokio::sync::watch::channel(false);
    let dest = Destination {
        id: "local".into(),
        target: Target::parse(&format!("rtmp://127.0.0.1:{port}/live"))?,
        key: "k".into(),
        retry: Retry::default(),
        extra_audio: false,
    };
    let pusher = rt.spawn(push::run(dest, hub.clone(), health_tx, stop));

    let feed = studio.program_feed()?;
    println!(
        "program feed: {}x{} at {}/{} fps, {} Hz",
        feed.width, feed.height, feed.fps_num, feed.fps_den, feed.sample_rate
    );
    let video = VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: feed.width,
        height: feed.height,
        fps_num: feed.fps_num,
        fps_den: feed.fps_den,
        bitrate_kbps: kbps,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
    };
    let audio = AudioConfig { sample_rate: feed.sample_rate, channels: 2, bitrate_kbps: 160 };
    let recorded = out.join("recorded.mp4");
    // libobs's own recording of the same program, to compare against.
    let theirs = out.join("libobs.mp4");
    let _ = std::fs::remove_file(&theirs);
    let libobs_rec = studio.record(
        &theirs,
        &EncodeSettings {
            video_kbps: kbps,
            ..EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc")
        },
    )?;
    let mut pipe = Pipeline::new(video, audio, Some(&recorded))?;
    hub.start();
    let send = |hub: &Arc<Hub>, tags: Vec<Tag>| {
        for t in tags {
            match t {
                Tag::Video { ms, body } => hub.video(ms, Bytes::from(body)),
                Tag::Audio { ms, body } => hub.audio(ms, Bytes::from(body)),
            }
        }
    };
    let began = Instant::now();
    let (mut frames, mut slowest) = (0u64, Duration::ZERO);
    while began.elapsed() < Duration::from_secs(seconds) {
        if let Ok(f) = feed.video.recv_timeout(Duration::from_millis(100)) {
            let t = Instant::now();
            let frame = Frame::Nv12 { y: &f.y, y_stride: f.y_stride, uv: &f.uv, uv_stride: f.uv_stride };
            send(&hub, pipe.video(frame, f.timestamp_ns)?);
            slowest = slowest.max(t.elapsed());
            frames += 1;
        }
        while let Ok(s) = feed.sound.try_recv() {
            send(&hub, pipe.audio(&s.samples, s.timestamp_ns)?);
        }
    }
    let dropped = feed.dropped();
    drop(feed);
    drop(libobs_rec);
    send(&hub, pipe.finish()?);
    hub.end();
    println!(
        "sent {frames} frames in {seconds} s; slowest frame took {:.1} ms to encode; {dropped} dropped; destination: {:?}",
        slowest.as_secs_f64() * 1000.0,
        health.borrow().state
    );
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
    drop(c);

    // libobs's own recording is the reference: its media source drifts a little each time the
    // clip loops, and ours must follow it, adding nothing of its own.
    let (d, n) = probe(&theirs);
    let reference = sync(&theirs);
    let span = |v: &[f64]| {
        (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max))
    };
    let (lo, hi) = span(&reference);
    println!("libobs's own recording: {d:.2} s, {n} frames; beeps vs flashes from {lo:+.0} to {hi:+.0} ms");
    let mut ok = true;
    for (name, file) in [("recorded (MP4)", &recorded), ("received over RTMP", &received)] {
        let (dur, n) = probe(file);
        let ours = sync(file);
        // Pair each of ours with libobs's at the same flash (both lists start at the first flash).
        let apart: Vec<f64> = ours.iter().zip(&reference).map(|(a, b)| a - b).collect();
        let worst = apart.iter().map(|d| d.abs()).fold(0.0, f64::max);
        let (lo, hi) = span(&ours);
        println!(
            "{name}: {dur:.2} s, {n} frames; beeps vs flashes from {lo:+.0} to {hi:+.0} ms; against libobs's own, {} pairs, worst {worst:.0} ms apart",
            apart.len()
        );
        ok &= dur > seconds as f64 - 3.0 && ours.len() + 3 >= seconds as usize && worst <= 10.0;
    }
    if !ok {
        return Err("a result was short, or its sync strayed from libobs's own by more than 10 ms".into());
    }
    println!("own_stream: OK");
    Ok(())
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn listen(port: u16, to: &Path) -> Result<Child> {
    Ok(Command::new("ffmpeg")
        .args(["-v", "error", "-listen", "1", "-i", &format!("rtmp://127.0.0.1:{port}/live/k"), "-c", "copy", "-y"])
        .arg(to)
        .stdin(Stdio::null())
        .spawn()?)
}

/// 20 s of black with a white flash, and a 1 kHz beep, for the first 0.1 s of every second.
fn make_clip(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let s = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=black:s=1280x720:r=30:d=20,format=yuv420p,geq=lum='if(lt(mod(T\\,1)\\,0.1)\\,235\\,16)':cb=128:cr=128")
        .args(["-f", "lavfi", "-i"])
        .arg("aevalsrc='if(lt(mod(t\\,1)\\,0.1)\\,0.5*sin(2*PI*1000*t)\\,0)':s=48000:c=stereo:d=20")
        .args(["-c:v", "libx264", "-g", "30", "-c:a", "aac", "-shortest"])
        .arg(path)
        .status()?;
    if !s.success() {
        return Err("ffmpeg couldn't make the test clip".into());
    }
    Ok(())
}

/// Length in seconds and video frames.
fn probe(file: &Path) -> (f64, u64) {
    let out = Command::new("ffprobe")
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
    let d = nums.next().unwrap_or(0.0);
    (d, n)
}

/// For each flash, how far (ms) the nearest beep start is after it (negative: before).
fn sync(file: &Path) -> Vec<f64> {
    let flashes = onsets(
        Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-map", "0:v:0", "-vf", "scale=8:8,format=gray", "-f", "rawvideo", "-"])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default()
            .chunks(64)
            .map(|c| c.iter().map(|v| f64::from(*v)).sum::<f64>() / 64.0)
            .collect(),
        |v| v > 128.0,
        None,
    );
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
    // Loudness per millisecond (8 samples).
    let loud: Vec<f64> = pcm.chunks(8).map(|c| c.iter().map(|v| f64::from(v.abs())).fold(0.0, f64::max)).collect();
    let beeps = onsets(loud, |v| v > 0.1, Some(1.0));
    let frame_ms = 1000.0 / 30.0;
    flashes
        .iter()
        .map(|f| f * frame_ms)
        .filter_map(|f| beeps.iter().map(|b| b - f).min_by(|a, b| a.abs().total_cmp(&b.abs())))
        .filter(|d| d.abs() < 400.0)
        .collect()
}

/// Indexes where `on` becomes true after being false (`step` scales the index).
fn onsets(values: Vec<f64>, on: impl Fn(f64) -> bool, step: Option<f64>) -> Vec<f64> {
    let mut prev = false;
    let mut out = Vec::new();
    for (i, v) in values.into_iter().enumerate() {
        let now = on(v);
        if now && !prev {
            out.push(i as f64 * step.unwrap_or(1.0));
        }
        prev = now;
    }
    out
}
