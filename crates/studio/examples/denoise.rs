//! Strong noise removal on a real engine (macOS: FERROCAST_OBS_DIR, else an installed OBS.app;
//! Windows: FERROCAST_OBS_DIR): a steady hiss played into the program and recorded twice, as it
//! is and through the filter, and their loudness compared with ffmpeg's volumedetect.
//!
//!   cargo run -p ferrocast-studio --features deepfilter --example denoise -- <out dir> [sound file]
//!
//! With a sound file (a voice over noise, say) it plays that instead and keeps both recordings in
//! <out dir>/rec to listen to, printing their levels without judging them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, RecordPlan, Studio};
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Ten seconds of hiss at 48 kHz, mono, 16-bit: a WAV file of its own making.
fn write_hiss(path: &Path) -> Result<()> {
    let n: u32 = 48_000 * 10;
    let mut b = Vec::with_capacity(44 + n as usize * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n * 2).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&96_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n * 2).to_le_bytes());
    let mut seed = 3u32;
    for _ in 0..n {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let s = ((seed >> 16) as i32 - 32_768) / 6;
        b.extend_from_slice(&(s as i16).to_le_bytes());
    }
    std::fs::write(path, b)?;
    Ok(())
}

/// ffmpeg's mean volume of a file's sound, in dB.
fn mean_db(file: &Path) -> Option<f64> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(file)
        .args(["-af", "volumedetect", "-f", "null", "-"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains("mean_volume:"))?;
    line.split("mean_volume:").nth(1)?.trim().trim_end_matches(" dB").parse().ok()
}

fn main() -> Result<()> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(out.join("rec"))?;
    let own = std::env::args().nth(2).map(PathBuf::from);
    let hiss = out.join("hiss.wav");
    write_hiss(&hiss)?;
    let sound = own.clone().unwrap_or(hiss);
    let paths = if cfg!(windows) {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
        Paths::from_obs_windows(Path::new(&dir))
    } else {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
        Paths::from_obs_app(Path::new(&dir), true)
    };
    let studio = Studio::start(&paths, Canvas { width: 640, height: 360, fps: 30 }, &out.join("plugin-config"))?;
    assert!(studio.kinds().filters.iter().any(|f| f.id == "ferrocast_deepfilter"), "the filter is listed");
    let c = Collection::new(&studio, "Main")?;
    let file = json!({ "local_file": sound.display().to_string(), "looping": true });
    c.add_source("Main", "ffmpeg_source", "Fan", &file, false)?;
    let enc = EncodeSettings::with_encoder("obs_x264");
    let record = |name: &str| -> Result<PathBuf> {
        let plan = RecordPlan {
            folder: out.join("rec"),
            name: name.into(),
            format: "mp4".into(),
            tracks: vec![1],
            // Its own two files are replaced; nothing else in the folder is touched.
            overwrite: true,
            encode: enc.clone(),
            ..RecordPlan::default()
        };
        let path = studio.record_path(&plan);
        let r = studio.record_to(&path, &plan)?;
        std::thread::sleep(Duration::from_secs(if own.is_some() { 10 } else { 4 }));
        drop(r);
        Ok(path)
    };
    std::thread::sleep(Duration::from_millis(500));
    let plain = record("As it is")?;
    c.add_filter("Fan", "ferrocast_deepfilter", "Clean", &json!({ "strength": 100 }))?;
    std::thread::sleep(Duration::from_millis(500));
    let clean = record("Cleaned")?;
    let (a, b) = (mean_db(&plain).ok_or("no level as it is")?, mean_db(&clean).ok_or("no level cleaned")?);
    println!("as it is: {a:.1} dB, through the filter: {b:.1} dB");
    if own.is_some() {
        println!("listen: {} and {}", plain.display(), clean.display());
        return Ok(());
    }
    assert!(b < a - 20.0, "at least 20 dB quieter");
    println!("strong noise removal: ok");
    drop(c);
    Ok(())
}
