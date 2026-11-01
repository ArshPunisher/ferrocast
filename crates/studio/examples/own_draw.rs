//! Our engine drawing the Studio's scenes (ENGINE.md E7): two scenes made in the Studio are
//! streamed over RTMP to an FFmpeg server here by `Collection::stream_drawn` (libobs draws
//! nothing that goes out). Half way the scene changes, and later a picture moves; the stream
//! follows both. What arrived is checked against libobs's own pictures of the two scenes.
//! macOS (FERROCAST_OBS_DIR, else an installed OBS.app).
//!
//!   cargo run -p ferrocast-studio --example own_draw -- <out dir> [seconds]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, StreamSettings, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const W: u32 = 1280;
const H: u32 = 720;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(9).max(6);
    std::fs::create_dir_all(&out)?;
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let studio = Studio::start(
        &Paths::from_obs_app(Path::new(&dir), true),
        Canvas { width: W, height: H, fps: 30 },
        &out.join("plugin-config"),
    )?;
    let c = Collection::new(&studio, "First")?;
    c.add_source(
        "First",
        "color_source_v3",
        "Blue",
        &json!({ "color": 0xFF_A0_40_10u32, "width": W, "height": H }),
        false,
    )?;
    let pic = out.join("card.png");
    let px: Vec<u8> = (0..200u32)
        .flat_map(|y| (0..300u32).flat_map(move |x| [(x * 255 / 299) as u8, (y * 255 / 199) as u8, 90, 255]))
        .collect();
    image::save_buffer(&pic, &px, 300, 200, image::ExtendedColorType::Rgba8)?;
    let card = c.add_source("First", "image_source", "Card", &json!({ "file": pic.display().to_string() }), false)?;
    let mut t = c.transform("First", card.id)?;
    (t.x, t.y, t.rotation, t.scale_x, t.scale_y) = (420.0, 180.0, 8.0, 1.5, 1.5);
    c.set_transform("First", card.id, &t)?;
    c.add_filter("Card", "ferrocast_rounded_corners", "Round", &json!({ "radius": 24 }))?;
    c.add_scene("Second")?;
    c.add_source(
        "Second",
        "color_source_v3",
        "Green",
        &json!({ "color": 0xFF_20_A0_30u32, "width": W, "height": H }),
        false,
    )?;
    let again = c.add_existing("Second", "Card")?;
    let mut t = c.transform("Second", again.id)?;
    (t.x, t.y) = (100.0, 400.0);
    c.set_transform("Second", again.id, &t)?;
    c.set_current("First")?;
    std::thread::sleep(Duration::from_millis(400));
    // libobs's own pictures of each scene, to check what arrives against.
    c.screenshot("First", &out.join("First obs.png"))?;
    c.screenshot("Second", &out.join("Second obs.png"))?;

    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let got = out.join("own-draw.flv");
    let _ = std::fs::remove_file(&got);
    let mut server = Command::new("ffmpeg")
        .args(["-v", "error", "-listen", "1", "-i", &format!("rtmp://127.0.0.1:{port}/live/k"), "-c", "copy", "-y"])
        .arg(&got)
        .stdin(Stdio::null())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(800));
    let settings = StreamSettings {
        encode: EncodeSettings {
            video_kbps: 12_000,
            ..EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc")
        },
        reconnect: Some((3, 2)),
        delay_s: 0,
        vod_track: None,
        own_encoder: true,
    };
    let mut stream = c.stream_drawn(&format!("rtmp://127.0.0.1:{port}/live"), "k", &settings)?;
    println!("couldn't bring over: {:?}", stream.skipped());
    let start = Instant::now();
    let (mut switched, mut moved, mut reopened) = (None, None, 0);
    while start.elapsed().as_secs() < secs {
        let at = start.elapsed().as_secs_f64();
        if switched.is_none() && at >= secs as f64 / 3.0 {
            c.set_current("Second")?;
            switched = Some(at);
        }
        if moved.is_none() && at >= secs as f64 * 2.0 / 3.0 {
            let mut t = c.transform("Second", again.id)?;
            t.x = 700.0;
            c.set_transform("Second", again.id, &t)?;
            // A lighter bitrate while live, as the stream doctor asks for.
            stream.set_video_kbps(2_000);
            moved = Some(at);
        }
        if stream.follow(&c) {
            reopened += 1;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let st = stream.stats();
    println!("stats: {st:?}, error: {:?}, opened again {reopened}×", stream.last_error());
    c.screenshot("Second", &out.join("Moved obs.png"))?;
    drop(stream);
    for _ in 0..50 {
        if server.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = server.kill();

    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-show_entries", "stream=codec_name,nb_read_frames", "-of", "csv=p=0"])
        .arg(&got)
        .output()?;
    let text = String::from_utf8_lossy(&probe.stdout).trim().replace('\n', " | ");
    println!("arrived: {text}");
    // A frame from each part, against libobs's picture of it (the stream is lossy: a mean
    // difference of a few steps is the encoder's).
    let (sw, mv) = (switched.unwrap_or(0.0), moved.unwrap_or(0.0));
    let mut failed = Vec::new();
    for (what, at, obs) in [
        ("First", sw * 0.5, "First obs.png"),
        ("Second", (sw + mv) / 2.0, "Second obs.png"),
        ("Moved", (mv + secs as f64) / 2.0, "Moved obs.png"),
    ] {
        let frame = out.join(format!("{what} ours.png"));
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-ss", &format!("{at:.2}"), "-i"])
            .arg(&got)
            .args(["-frames:v", "1"])
            .arg(&frame)
            .status()?
            .success();
        if !ok {
            failed.push(what);
            continue;
        }
        let ours = image::open(&frame)?.to_rgb8();
        let theirs = image::open(out.join(obs))?.to_rgb8();
        let n = ours.as_raw().len().min(theirs.as_raw().len());
        let mean =
            ours.as_raw()[..n].iter().zip(&theirs.as_raw()[..n]).map(|(a, b)| f64::from(a.abs_diff(*b))).sum::<f64>()
                / n as f64;
        println!("own_draw {what} at {at:.1} s: mean difference {mean:.2} from libobs's picture");
        if mean > 4.0 {
            failed.push(what);
        }
    }
    // The bitrate before and after the change (it lands within a second).
    let packets = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,size", "-of", "csv=p=0"])
        .arg(&got)
        .output()?;
    let (mut before, mut after) = (0f64, 0f64);
    for line in String::from_utf8_lossy(&packets.stdout).lines() {
        let mut it = line.split(',');
        let (Some(t), Some(n)) =
            (it.next().and_then(|t| t.parse::<f64>().ok()), it.next().and_then(|n| n.parse::<f64>().ok()))
        else {
            continue;
        };
        if t > 1.0 && t < mv {
            before += n;
        } else if t > mv + 1.0 {
            after += n;
        }
    }
    let kbps = |bytes: f64, secs: f64| bytes * 8.0 / secs.max(0.1) / 1000.0;
    let (k1, k2) = (kbps(before, mv - 1.0), kbps(after, secs as f64 - mv - 1.0));
    println!("own_draw bitrate: {k1:.0} kbps, then {k2:.0} kbps after asking for 2000");
    if k2 > 2_600.0 {
        failed.push("Bitrate");
    }
    let frames: u64 = text.split(',').nth(1).and_then(|n| n.trim().split(' ').next()?.parse().ok()).unwrap_or(0);
    if !text.contains("h264") || !text.contains("aac") || frames < (secs - 1) * 30 {
        return Err(format!("the stream came up short: {text}").into());
    }
    if reopened != 1 {
        return Err(format!("the edit should open the collection again once, not {reopened}×").into());
    }
    if failed.is_empty() {
        println!("own_draw: OK");
        Ok(())
    } else {
        Err(format!("these parts didn't look like libobs's: {failed:?}").into())
    }
}
