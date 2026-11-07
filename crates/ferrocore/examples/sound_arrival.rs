//! How sound arrives from a capture: for `seconds`, each piece's size, how late it comes (now
//! minus the end of its sound) and whether it continues the last piece (a gap or an overlap in
//! its timestamps), and how its clock keeps time against the Mac's (frames received over the
//! time their timestamps span). The computer's sound, one window's (an app's name or a title's
//! part), or a microphone's (`mic`, or `mic:<part of its name>`).
//!
//!   cargo run --release -p ferrocore --example sound_arrival -- [seconds] [window | mic[:name]]

#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocore::camera::{Capture, microphones};
    use ferrocore::engine::now_ns;
    use ferrocore::screen::{ScreenCapture, Target, shareable};
    use std::time::{Duration, Instant};
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    // SAFETY: a plain CoreGraphics query (opens the window-server connection).
    let _ = unsafe { CGMainDisplayID() };
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let what = args.next();
    let screen = |t: Target| ScreenCapture::start_with(t, 64, 36, 30, false, true, &[]);
    let (screen_cap, mic_cap) = match what.as_deref() {
        Some(m) if m.starts_with("mic") => {
            let part = m.strip_prefix("mic:").unwrap_or("");
            let mics = microphones();
            let d = mics.iter().find(|d| d.name.contains(part)).ok_or("no such microphone")?;
            println!("microphone {}", d.name);
            (None, Some(Capture::microphone(&d.id)?))
        }
        Some(w) => {
            let (_, windows) = shareable()?;
            let id = windows.iter().find(|x| x.app == w || x.title.contains(w)).ok_or("no such window")?.id;
            (Some(screen(Target::Window(id))?), None)
        }
        None => {
            let (displays, _) = shareable()?;
            (Some(screen(Target::Display(displays.first().ok_or("no display")?.id))?), None)
        }
    };
    let take = || match (&screen_cap, &mic_cap) {
        (Some(c), _) => c.take_sound(),
        (_, Some(c)) => c.take_sound(),
        _ => Vec::new(),
    };
    let start = Instant::now();
    let (mut sizes, mut late, mut jumps) = (Vec::new(), Vec::new(), Vec::new());
    let (mut prev_end, mut first_ts, mut total) = (None::<u64>, None::<u64>, 0usize);
    let (mut peak, mut bad) = (0f32, 0usize);
    while start.elapsed() < Duration::from_secs(secs) {
        for (pcm, ts) in take() {
            let n = pcm.len() / 2;
            let end = ts + n as u64 * 1_000_000_000 / 48_000;
            first_ts.get_or_insert(ts);
            total += n;
            sizes.push(n);
            late.push(now_ns().saturating_sub(end) as f64 / 1e6);
            if let Some(p) = prev_end {
                jumps.push((ts as i64 - p as i64) as f64 / 1e6);
            }
            prev_end = Some(end);
            for v in &pcm {
                if !v.is_finite() || v.abs() > 4.0 {
                    bad += 1;
                } else {
                    peak = peak.max(v.abs());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let stats = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        if v.is_empty() {
            return "none".to_string();
        }
        format!(
            "min {:.1} median {:.1} p95 {:.1} max {:.1}",
            v[0],
            v[v.len() / 2],
            v[v.len() * 95 / 100],
            v[v.len() - 1]
        )
    };
    let off = jumps.iter().filter(|j| j.abs() > 1.0).count();
    sizes.sort_unstable();
    println!("{} pieces, frames each: {:?}..{:?}", sizes.len(), sizes.first(), sizes.last());
    println!("late (ms after its end): {}", stats(&mut late));
    println!("gap to the last piece (ms; 0 is continuous): {}", stats(&mut jumps));
    println!("pieces not continuing the last: {off}; peak {peak:.3}; bad samples {bad}");
    if let (Some(f), Some(e)) = (first_ts, prev_end) {
        let span = (e - f) as f64 / 1e9;
        let rate = total as f64 / span;
        println!(
            "{total} frames over {span:.2} s of its timestamps: {rate:.1} Hz ({:+.0} ppm against 48 kHz)",
            (rate / 48_000.0 - 1.0) * 1e6
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
