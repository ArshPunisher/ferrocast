//! Screen capture on this Mac: lists displays and windows, captures the main display for a few
//! seconds, and says how many frames came and how big they were. No picture is saved (what's
//! on screen is private).
//!
//!   cargo run -p ferrocast-av --example capture -- [seconds]

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
    let cap = ScreenCapture::start(Target::Display(main.id), 1280, 720, 30, true)?;
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
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("screen capture here is macOS only so far");
}
