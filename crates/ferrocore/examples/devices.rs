//! Lists cameras and microphones; with `--try`, runs the first of each for 3 seconds and says
//! how many frames and sound pieces came, and the sound's loudest level (nothing is saved).
//!
//!   cargo run -p ferrocore --example devices -- [--try]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ferrocore::camera::{Capture, cameras, microphones};
    let cams = cameras();
    let mics = microphones();
    for c in &cams {
        println!("camera: {} ({})", c.name, c.id);
    }
    for m in &mics {
        println!("microphone: {} ({})", m.name, m.id);
    }
    if std::env::args().nth(1).as_deref() != Some("--try") {
        return Ok(());
    }
    if let Some(c) = cams.first() {
        let cap = Capture::camera(&c.id.as_str().into())?;
        std::thread::sleep(std::time::Duration::from_secs(3));
        let size = cap.take_frame().map(|(p, _)| (p.width, p.height));
        println!("{}: {} frames in 3 s, {size:?}", c.name, cap.received());
    }
    if let Some(m) = mics.first() {
        let cap = Capture::microphone(&m.id)?;
        std::thread::sleep(std::time::Duration::from_secs(3));
        let pieces = cap.take_sound();
        let peak = pieces.iter().flat_map(|(p, _)| p.iter()).fold(0f32, |a, v| a.max(v.abs()));
        let samples: usize = pieces.iter().map(|(p, _)| p.len() / 2).sum();
        println!(
            "{}: {} pieces, {samples} frames in 3 s (about 144000 expected), peak {:.1} dBFS",
            m.name,
            pieces.len(),
            20.0 * peak.max(1e-9).log10()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("cameras and microphones here are macOS only so far");
}
