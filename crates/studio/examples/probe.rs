//! Runs OBS's engine from an installed OBS (OBS.app on macOS; on Windows the folder in
//! `FERROCAST_OBS_DIR`, an install or OBS's portable zip) and checks the Studio crate end to
//! end: every kind with its options, a scene, and a 5 s recording.
//!
//!   cargo run -p ferrocast-studio --example probe -- <out dir> [metal|opengl] [noopts]

use std::path::PathBuf;
use std::time::Duration;

use ferrocast_studio::{Canvas, Paths, Studio};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERROCAST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let metal = args.next().as_deref() != Some("opengl");
    let read_options = args.next().as_deref() != Some("noopts");
    std::fs::create_dir_all(&out)?;
    let paths = if cfg!(windows) {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
        Paths::from_obs_windows(std::path::Path::new(&dir))
    } else {
        Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), metal)
    };
    let canvas = Canvas { width: 1280, height: 720, fps: 30 };
    let studio = Studio::start(&paths, canvas, &out.join("plugin-config"))?;
    let renderer = if cfg!(windows) {
        "D3D11"
    } else if metal {
        "Metal"
    } else {
        "OpenGL"
    };
    println!("libobs {} on {renderer}", studio.version());

    let k = studio.kinds();
    println!(
        "{} inputs, {} filters, {} transitions, {} encoders, {} outputs",
        k.inputs.len(),
        k.filters.len(),
        k.transitions.len(),
        k.encoders.len(),
        k.outputs.len()
    );
    // Options through live private sources. Capture kinds are left out here: they'd ask macOS
    // for camera or screen permission on this machine.
    let skip = |id: &str| {
        id.contains("avcapture")
            || id.contains("av_capture")
            || id.contains("capture") && !id.contains("coreaudio")
            || id.contains("syphon")
    };
    let mut total = 0;
    let mut report = String::new();
    for kind in k.inputs.iter().chain(&k.filters) {
        if skip(&kind.id) || !read_options {
            report.push_str(&format!("## {} ({}): skipped here (needs permission)\n", kind.name, kind.id));
            continue;
        }
        let opts = studio.options(&kind.id)?;
        total += opts.len();
        report.push_str(&format!("## {} ({}): {} options\n", kind.name, kind.id, opts.len()));
        for o in &opts {
            report.push_str(&format!("- {} [{}] {}\n", o.label, o.kind, o.key));
        }
    }
    std::fs::write(out.join("options.md"), report)?;
    println!("{total} options read through live sources (options.md)");

    let scene = studio.scene("Probe")?;
    scene.add(
        "color_source",
        "Background",
        &json!({ "color": 0xFF_A3_3F_FFu32, "width": 1280, "height": 720 }),
        (0.0, 0.0),
    )?;
    scene.add(
        "text_ft2_source",
        "Title",
        &json!({ "text": "Ferrocast Studio (Rust)", "font": { "face": if cfg!(windows) { "Arial" } else { "Helvetica" }, "size": 96 } }),
        (120.0, 300.0),
    )?;
    studio.set_program(&scene);
    let file = out.join("probe.mp4");
    let rec = studio.record(&file, &ferrocast_studio::EncodeSettings::with_encoder("obs_x264"))?;
    std::thread::sleep(Duration::from_secs(5));
    rec.stop();
    let (frames, lagged) = studio.frames();
    println!("recorded {} ({frames} frames, {lagged} late)", file.display());
    drop(scene);
    drop(studio);
    println!("shut down cleanly");
    Ok(())
}
