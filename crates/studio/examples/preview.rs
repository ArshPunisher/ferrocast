//! Opens a window and shows the Studio's live picture in it through `Studio::preview`, drawn by
//! libobs into a native view (macOS). Saves a screenshot of the window after a few seconds.
//!
//!   cargo run -p ferrocast-studio --example preview -- <out dir>
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::ptr::NonNull;

    use ferrocast_studio::{Canvas, Paths, Rect, Studio};
    use objc2::MainThreadMarker;
    use objc2::MainThreadOnly;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize, NSString};
    use raw_window_handle::{AppKitWindowHandle, RawWindowHandle};
    use serde_json::json;

    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let mtm = MainThreadMarker::new().ok_or("run me on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // SAFETY: a plain titled window built and shown on the main thread.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(720.0, 440.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the window isn't released on close while this program holds it.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Ferrocast Studio preview"));
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let content = window.contentView().ok_or("no content view")?;
    let handle = RawWindowHandle::AppKit(AppKitWindowHandle::new(
        NonNull::new(objc2::rc::Retained::as_ptr(&content) as *mut std::ffi::c_void).ok_or("null view")?,
    ));

    let paths = Paths::from_obs_app(std::path::Path::new("/Applications/OBS.app/Contents"), true);
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let scene = studio.scene("Preview")?;
    scene.add(
        "color_source",
        "Background",
        &json!({ "color": 0xFF_A3_3F_FFu32, "width": 1280, "height": 720 }),
        (0.0, 0.0),
    )?;
    scene.add(
        "text_ft2_source",
        "Title",
        &json!({ "text": "Live in Ferrocast", "font": { "face": "Helvetica", "size": 110 } }),
        (150.0, 290.0),
    )?;
    studio.set_program(&scene);
    let preview = studio.preview(handle, Rect { x: 20.0, y: 20.0, w: 640.0, h: 360.0 }, window.backingScaleFactor())?;
    println!("preview on; window {}", window.windowNumber());

    // Screenshot of just this window after 3 s, while the run loop keeps it drawing.
    let (num, shot) = (window.windowNumber(), out.join("preview.png"));
    let taker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        std::process::Command::new("screencapture").args(["-x", "-o", &format!("-l{num}")]).arg(&shot).status()
    });
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(5.0));
    let _ = taker.join();
    let (frames, lagged) = studio.frames();
    println!("{frames} frames, {lagged} late; screenshot {}", out.join("preview.png").display());
    drop(preview);
    drop(scene);
    drop(studio);
    window.close();
    println!("closed cleanly");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This example is for macOS.");
}
