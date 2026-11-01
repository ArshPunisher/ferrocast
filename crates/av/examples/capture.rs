//! Screen capture on this Mac: lists displays and windows, captures the main display for a few
//! seconds, and says how many frames came and how big they were. No picture is saved (what's
//! on screen is private).
//!
//!   cargo run -p ferrocast-av --example capture -- [seconds] [--sound] [--tiny]
//!
//! `--tiny` takes a 2×2 picture once a second, as the engine's desktop sound source does.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocast_av::screen::{ScreenCapture, Target, shareable};
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let (displays, windows) = shareable()?;
    println!("{} displays, {} titled windows", displays.len(), windows.len());
    for d in &displays {
        println!("  display {}: {}x{}", d.id, d.width, d.height);
    }
    let main = displays.first().ok_or("no display")?;
    // `--sound` also captures what the Mac plays (a short quiet tone is played to check).
    let sound = std::env::args().any(|a| a == "--sound");
    let (w, h, fps) = if std::env::args().any(|a| a == "--tiny") { (2, 2, 1) } else { (1280, 720, 30) };
    let cap = ScreenCapture::start_with(Target::Display(main.id), w, h, fps, true, sound)?;
    let tone = if sound {
        let wav = std::env::temp_dir().join("ferrocast-tone.wav");
        write_tone(&wav)?;
        Some(std::process::Command::new("afplay").args(["-v", "0.2"]).arg(&wav).spawn()?)
    } else {
        None
    };
    let t = std::time::Instant::now();
    let (mut got, mut size) = (0u32, (0, 0));
    while t.elapsed().as_secs() < secs {
        if let Some(p) = cap.take() {
            got += 1;
            size = (p.width, p.height);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    println!("{} frames from macOS in {secs} s ({} taken here), {}x{}", cap.frames(), got, size.0, size.1);
    if let Some(mut t) = tone {
        let _ = t.wait();
        let pieces = cap.take_sound();
        let frames: usize = pieces.iter().map(|(p, _)| p.len() / 2).sum();
        let peak = pieces.iter().flat_map(|(p, _)| p.iter()).fold(0f32, |a, v| a.max(v.abs()));
        println!("sound: {} pieces, {frames} frames, peak {:.1} dBFS", pieces.len(), 20.0 * peak.max(1e-9).log10());
    }
    Ok(())
}

/// One second of 440 Hz at 48 kHz, 16-bit mono WAV.
#[cfg(target_os = "macos")]
fn write_tone(path: &std::path::Path) -> std::io::Result<()> {
    let n = 48_000u32;
    let mut b = Vec::new();
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
    for i in 0..n {
        let v = ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 12_000.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b)
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("screen capture here is macOS only so far");
}
