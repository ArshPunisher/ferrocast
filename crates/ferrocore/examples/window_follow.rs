//! A window capture follows the window's size: prints each new picture size while the window
//! (by id, from `shareable`) is resized by hand or by a script.
//!
//!   cargo run --release -p ferrocore --example window_follow -- <window id | app name> [seconds]

// One CoreGraphics call to open the window-server connection (see main).
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocore::screen::{ScreenCapture, Target};
    use std::time::{Duration, Instant};
    // A command-line process has no window-server connection until something asks for one
    // (the app has one from AppKit); reading a window filter's scale needs it.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    // SAFETY: a plain CoreGraphics query with no arguments.
    let _ = unsafe { CGMainDisplayID() };
    let mut args = std::env::args().skip(1);
    let what = args.next().ok_or("a window id or an app's name")?;
    let id: u32 = match what.parse() {
        Ok(id) => id,
        Err(_) => {
            let (_, windows) = ferrocore::screen::shareable()?;
            let w = windows.iter().find(|w| w.app == what || w.title.contains(&what)).ok_or("no window of that app")?;
            println!("window {} \"{}\"", w.id, w.title);
            w.id
        }
    };
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    // With sound: the window's app's sound comes with it (FERROCORE_SOUND=1 prints its level).
    let sound = std::env::var_os("FERROCORE_SOUND").is_some();
    let cap = ScreenCapture::start_with(Target::Window(id), 1920, 1080, 30, false, sound, &[])?;
    let (mut samples, mut peak) = (0usize, 0f32);
    let (start, mut last) = (Instant::now(), (0, 0));
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(s) = cap.take_surface()
            && s.size() != last
        {
            last = s.size();
            println!("{:>6} ms  {}x{}", start.elapsed().as_millis(), last.0, last.1);
        }
        for (pcm, _) in cap.take_sound() {
            samples += pcm.len() / 2;
            peak = pcm.iter().fold(peak, |p, v| p.max(v.abs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if sound {
        println!("sound: {samples} frames ({:.1} s), peak {peak:.3}", samples as f32 / 48_000.0);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
