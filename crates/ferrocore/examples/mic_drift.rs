//! How far each microphone's clock is from the Mac's: its samples counted against the times
//! they're stamped with, over a while. A device at 48,000 Hz on its own crystal is a little
//! fast or slow (parts per million); the mixer has to follow that or the sound breaks.
//!
//!   cargo run --release -p ferrocore --example mic_drift -- [seconds]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let mics = ferrocore::camera::microphones();
    let mut open = Vec::new();
    for m in &mics {
        match ferrocore::camera::Capture::microphone(&m.id) {
            Ok(c) => open.push((m.name.clone(), c, None::<(u64, u64)>, 0u64, 0u64, Vec::<(u64, i64)>::new())),
            Err(e) => println!("{}: can't open ({e})", m.name),
        }
    }
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < seconds {
        std::thread::sleep(std::time::Duration::from_millis(20));
        for (_, c, first, frames, last_ts, lag) in &mut open {
            let now = ferrocore::engine::now_ns();
            for (pcm, ts) in c.take_sound() {
                lag.push((now, now as i64 - ts as i64));
                if first.is_none() {
                    *first = Some((ts, 0));
                    *frames = 0;
                }
                *frames += (pcm.len() / 2) as u64;
                *last_ts = ts + (pcm.len() / 2) as u64 * 1_000_000_000 / 48_000;
            }
        }
    }
    for (name, _, first, frames, last_ts, lag) in &open {
        let Some((t0, _)) = first else {
            println!("{name}: no sound came");
            continue;
        };
        let span_ns = last_ts.saturating_sub(*t0) as f64;
        let by_time = span_ns / 1e9 * 48_000.0;
        let ppm = (*frames as f64 - by_time) / by_time * 1e6;
        println!(
            "{name}: {frames} samples in {:.1} s of time stamps → {ppm:+.1} ppm ({:+.2} ms a minute)",
            span_ns / 1e9,
            ppm * 60.0 / 1000.0
        );
        // How late each piece arrives against the Mac's clock: its trend is the device clock's
        // drift against the Mac's (what the mixer sees). The least lag in each 10 s window.
        let mut mins = Vec::new();
        if let Some((t0, _)) = lag.first() {
            for (t, l) in lag {
                let w = ((t - t0) / 10_000_000_000) as usize;
                if mins.len() <= w {
                    mins.resize(w + 1, i64::MAX);
                }
                mins[w] = mins[w].min(*l);
            }
        }
        let shown: Vec<String> = mins.iter().map(|m| format!("{:.2}", *m as f64 / 1e6)).collect();
        if mins.len() >= 3 {
            let drift_ms = (mins[mins.len() - 2] - mins[0]) as f64 / 1e6;
            let secs = (mins.len() - 2) as f64 * 10.0;
            println!("  arrival lag (ms, min per 10 s): {}", shown.join(" "));
            println!("  → device vs Mac clock: {:+.1} ppm", drift_ms / secs * 1000.0);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
