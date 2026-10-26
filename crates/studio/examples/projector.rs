//! Opens projectors (macOS): what's on air, one scene, and a multiview of three scenes, each
//! in its own window, and saves a screenshot of each after a few seconds.
//!
//!   cargo run -p ferrocast-studio --example projector -- <out dir>

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    use ferrocast_studio::{Canvas, Collection, Paths, ProjectorOf, Studio, screens};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};
    use serde_json::json;

    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let mtm = MainThreadMarker::new().ok_or("run me on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), false);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Red")?;
    c.add_scene("Green")?;
    c.add_scene("Blue")?;
    for (scene, color) in [("Red", 0xFF_20_20_E0u32), ("Green", 0xFF_30_C0_30), ("Blue", 0xFF_E0_60_20)] {
        c.add_source(
            scene,
            "color_source_v3",
            &format!("{scene} fill"),
            &json!({ "color": color, "width": 1280, "height": 720 }),
            false,
        )?;
    }
    c.set_current("Green")?;
    let names: Vec<String> = c.scenes().into_iter().map(|s| s.name).collect();
    println!("screens: {:?}", screens());

    let picked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let open = |of: ProjectorOf| {
        let p = picked.clone();
        studio.projector(&of, &names, None, Box::new(|| println!("closed")), Box::new(move |s| p.borrow_mut().push(s)))
    };
    let program = open(ProjectorOf::Program)?;
    let blue = open(ProjectorOf::Source("Blue".into()))?;
    let multi = open(ProjectorOf::Multiview)?;
    assert!(open(ProjectorOf::Source("Nope".into())).is_err(), "an unknown source is refused");
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let shots: Vec<(isize, PathBuf)> = [(&program, "program"), (&blue, "blue"), (&multi, "multiview")]
        .iter()
        .map(|(p, n)| (p.window_number(), out.join(format!("projector-{n}.png"))))
        .collect();
    let taker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        for (num, shot) in shots {
            let _ =
                std::process::Command::new("screencapture").args(["-x", "-o", &format!("-l{num}")]).arg(&shot).status();
        }
    });
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(4.0));
    let _ = taker.join();
    // A new scene joins the multiview.
    c.add_scene("Purple")?;
    multi.set_scenes(&c.scenes().into_iter().map(|s| s.name).collect::<Vec<_>>());
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(1.0));
    drop((program, blue, multi));
    drop(c);
    drop(studio);
    println!("closed cleanly; shots in {}", out.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This example is for macOS.");
}
