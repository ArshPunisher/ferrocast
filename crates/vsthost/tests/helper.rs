//! The helper with our own test plugin (`ferrocast-vst-testgain`): found, run, its state kept,
//! and a plugin that hangs or crashes never holds anything up. On a Mac, its sandbox too.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrocast_vsthost::client::{self, Helper};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ferrocast-vst"))
}

/// The test plugin as a `.vst3` bundle in its own temp folder.
fn bundle(tag: &str) -> PathBuf {
    let target = exe().parent().map(Path::to_path_buf).unwrap_or_default();
    let lib = ["libferrocast_vst_testgain.dylib", "ferrocast_vst_testgain.dll", "libferrocast_vst_testgain.so"]
        .iter()
        .map(|n| target.join(n))
        .find(|p| p.is_file())
        .or_else(|| {
            ["libferrocast_vst_testgain.dylib", "ferrocast_vst_testgain.dll", "libferrocast_vst_testgain.so"]
                .iter()
                .map(|n| target.join("deps").join(n))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| panic!("build the test plugin first: cargo build -p ferrocast-vst-testgain"));
    let dir = std::env::temp_dir().join(format!("fc-vst-{tag}-{}", std::process::id()));
    let b = dir.join("TestGain.vst3");
    let inner = if cfg!(target_os = "macos") {
        b.join("Contents/MacOS/TestGain")
    } else if cfg!(target_os = "windows") {
        b.join("Contents/x86_64-win/TestGain.vst3")
    } else {
        b.join("Contents/x86_64-linux/TestGain.so")
    };
    std::fs::create_dir_all(inner.parent().unwrap_or(&dir)).unwrap_or_else(|e| panic!("{e}"));
    std::fs::copy(&lib, &inner).unwrap_or_else(|e| panic!("{e}"));
    b
}

fn cid() -> String {
    ferrocast_vst_testgain::PROCESSOR_CID.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn state(gain: f64, mode: u8) -> Vec<u8> {
    let mut b = gain.to_le_bytes().to_vec();
    b.push(mode);
    b
}

#[test]
fn the_test_plugin_is_found() {
    let b = bundle("scan");
    let found = client::scan_bundles(&exe(), std::slice::from_ref(&b));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, ferrocast_vst_testgain::NAME);
    assert_eq!(found[0].vendor, ferrocast_vst_testgain::VENDOR);
    assert_eq!(found[0].cid, cid());
    // Not a plugin: left out, not a failure.
    let junk = b.with_file_name("Junk.vst3");
    std::fs::create_dir_all(&junk).unwrap_or_else(|e| panic!("{e}"));
    assert!(client::scan_bundles(&exe(), &[junk]).is_empty());
}

#[test]
fn blocks_go_through_and_its_state_is_kept() {
    let b = bundle("run");
    let mut h = Helper::start(&exe(), &b, &cid(), 48_000, 1024, None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(h.name, ferrocast_vst_testgain::NAME);
    let (mut l, mut r) = (vec![0.5f32; 1024], vec![-0.25f32; 1024]);
    assert!(h.process(&mut l, &mut r));
    assert!(l.iter().all(|v| *v == 0.5) && r.iter().all(|v| *v == -0.25), "gain 1 by default");
    h.set_state(&state(2.0, 0), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(h.process(&mut l, &mut r));
    assert_eq!((l[0], r[0]), (1.0, -0.5), "gain 2 from its state");
    let (comp, _) = h.state().unwrap_or_else(|| panic!("a state"));
    assert_eq!(comp, state(2.0, 0));
    drop(h);
    // Started again with that state: the same sound.
    let mut again =
        Helper::start(&exe(), &b, &cid(), 48_000, 1024, Some((&comp, &[]))).unwrap_or_else(|e| panic!("{e}"));
    let (mut l, mut r) = (vec![0.25f32; 512], vec![0.25f32; 512]);
    assert!(again.process(&mut l, &mut r));
    assert_eq!(l[0], 0.5);
    // A class it doesn't have, a bundle that isn't there: said, not hung.
    assert!(Helper::start(&exe(), &b, "00000000000000000000000000000000", 48_000, 1024, None).is_err());
    assert!(Helper::start(&exe(), &b.with_file_name("Nope.vst3"), &cid(), 48_000, 1024, None).is_err());
}

#[test]
fn a_plugin_that_hangs_or_crashes_holds_nothing_up() {
    let b = bundle("bad");
    let mut hung =
        Helper::start(&exe(), &b, &cid(), 48_000, 1024, Some((&state(1.0, 1), &[]))).unwrap_or_else(|e| panic!("{e}"));
    // Each block comes back at once, as it was; after two seconds of silence it's ended.
    let t = Instant::now();
    while hung.dead.is_none() && t.elapsed() < Duration::from_secs(4) {
        let b = Instant::now();
        let (mut l, mut r) = (vec![0.5f32; 1024], vec![0.5f32; 1024]);
        assert!(!hung.process(&mut l, &mut r), "late: left as it was");
        assert_eq!(l[0], 0.5);
        assert!(b.elapsed() < Duration::from_millis(60), "a block never waits long: {:?}", b.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(hung.dead.is_some(), "ended after two silent seconds");
    assert!(t.elapsed() >= ferrocast_vsthost::client::HUNG_AFTER, "not before");
    let mut crashed =
        Helper::start(&exe(), &b, &cid(), 48_000, 1024, Some((&state(1.0, 2), &[]))).unwrap_or_else(|e| panic!("{e}"));
    let (mut l, mut r) = (vec![0.5f32; 64], vec![0.5f32; 64]);
    assert!(!crashed.process(&mut l, &mut r));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!crashed.process(&mut l, &mut r));
    assert!(crashed.dead.as_deref().is_some_and(|d| d.contains("crashed")), "{:?}", crashed.dead);
}

/// The Mac's sandbox: no network, no reads of Ferrocast's folder or the owner's documents, no
/// writes outside its own temp folder; the plugin still loads and runs (the tests above).
#[cfg(target_os = "macos")]
#[test]
fn the_helper_is_locked_down() {
    let private = std::env::temp_dir().join(format!("fc-vst-private-{}", std::process::id()));
    std::fs::create_dir_all(&private).unwrap_or_else(|e| panic!("{e}"));
    let secret = private.join("secrets.json");
    std::fs::write(&secret, b"{}").unwrap_or_else(|e| panic!("{e}"));
    client::keep_private(vec![std::fs::canonicalize(&private).unwrap_or(private.clone())]);
    let s = secret.to_string_lossy().to_string();
    assert!(std::fs::read(&secret).is_ok(), "Ferrocast itself can");
    assert!(!client::probe(&exe(), "read", &s), "a plugin can't read Ferrocast's folder");
    assert!(!client::probe(&exe(), "write", &private.join("x").to_string_lossy()), "nor write there");
    // A listener on this machine, and it still can't reach it.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let addr = l.local_addr().unwrap_or_else(|e| panic!("{e}")).to_string();
    assert!(!client::probe(&exe(), "net", &addr), "no network");
    // The owner's documents: this repo's own Cargo.toml when it's kept there (only whether it
    // can be read is checked; nothing is printed).
    let manifest = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap_or_default().join("Cargo.toml");
    let docs = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Documents"));
    if docs.is_some_and(|d| manifest.starts_with(std::fs::canonicalize(&d).unwrap_or(d))) {
        assert!(!client::probe(&exe(), "read", &manifest.to_string_lossy()), "no reads of ~/Documents");
    }
    // What it may do: read the system.
    assert!(client::probe(&exe(), "read", "/System/Library/CoreServices/SystemVersion.plist"));
    let _ = std::fs::remove_dir_all(&private);
}

/// The plugin's own window opens (in front), opens again (the same one), and closes, while
/// blocks keep going. Ignored by default: it puts a small empty window on the screen for a
/// moment. `cargo test -p ferrocast-vsthost -- --ignored`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "shows a window"]
fn its_window_opens_and_closes() {
    let b = bundle("window");
    let mut h = Helper::start(&exe(), &b, &cid(), 48_000, 1024, None).unwrap_or_else(|e| panic!("{e}"));
    h.window(true).unwrap_or_else(|e| panic!("{e}"));
    let (mut l, mut r) = (vec![0.5f32; 256], vec![0.5f32; 256]);
    assert!(h.process(&mut l, &mut r), "blocks go on while it's open");
    h.window(true).unwrap_or_else(|e| panic!("again: {e}"));
    std::thread::sleep(Duration::from_millis(400));
    h.window(false).unwrap_or_else(|e| panic!("{e}"));
    assert!(h.process(&mut l, &mut r));
    assert!(h.state().is_some());
}
