//! A VST filter the way the engine runs it: from a collection's `ferrocast_vst` filter to a slot,
//! the app's maker, the helper started in the background, and the sound through it.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ferrocore::afx::{AudioEffect, AudioFilter};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ferrocast-vst"))
}

fn bundle() -> PathBuf {
    let target = exe().parent().map(Path::to_path_buf).unwrap_or_default();
    let names = ["libferrocast_vst_testgain.dylib", "ferrocast_vst_testgain.dll", "libferrocast_vst_testgain.so"];
    let lib = names
        .iter()
        .flat_map(|n| [target.join(n), target.join("deps").join(n)])
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("build the test plugin first: cargo build -p ferrocast-vst-testgain"));
    let b = std::env::temp_dir().join(format!("fc-vst-fx-{}", std::process::id())).join("TestGain.vst3");
    let inner = if cfg!(target_os = "macos") {
        b.join("Contents/MacOS/TestGain")
    } else if cfg!(target_os = "windows") {
        b.join("Contents/x86_64-win/TestGain.vst3")
    } else {
        b.join("Contents/x86_64-linux/TestGain.so")
    };
    std::fs::create_dir_all(inner.parent().unwrap_or(&b)).unwrap_or_else(|e| panic!("{e}"));
    std::fs::copy(&lib, &inner).unwrap_or_else(|e| panic!("{e}"));
    b
}

// VSTs run only where the helper is locked down (`available()`): a Mac for now.
#[cfg(target_os = "macos")]
#[test]
fn a_collections_vst_filter_plays_through_the_helper() {
    // SAFETY: set before anything reads it, in this test's own process.
    unsafe { std::env::set_var("FERROCAST_VST_HELPER", exe()) };
    ferrocast_vsthost::install();
    let cid: String = ferrocast_vst_testgain::PROCESSOR_CID.iter().map(|b| format!("{:02X}", *b as u8)).collect();
    let mut gain2 = 2.0f64.to_le_bytes().to_vec();
    gain2.push(0);
    let state = base64::engine::general_purpose::STANDARD.encode(ferrocast_vsthost::proto::state_bytes(&gain2, &[]));
    let settings = serde_json::json!({ "path": bundle().display().to_string(), "cid": cid, "state": state });
    let e = ferrocore::obs::vst_effect("Mic", "Warm", &settings).unwrap_or_else(|| panic!("an effect"));
    // The same filter again: the same slot (its plugin isn't started twice).
    assert_eq!(ferrocore::obs::vst_effect("Mic", "Warm", &settings), Some(e));
    assert_eq!(ferrocore::obs::vst_effect("Mic", "Warm", &serde_json::json!({ "path": "" })), None, "none picked");
    let AudioEffect::Plugin { slot } = e else { panic!("{e:?}") };
    let mut f = AudioFilter::new(e, 48_000);
    // Dry while its helper starts, then through it.
    let until = Instant::now() + Duration::from_secs(10);
    let through = loop {
        let (mut l, mut r) = (vec![0.25f32; 1024], vec![0.25f32; 1024]);
        f.process([&mut l, &mut r]);
        if l[0] == 0.5 {
            break true;
        }
        assert_eq!(l[0], 0.25, "dry until it runs");
        if Instant::now() > until {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(through, "the plugin's gain of 2 from its saved state");
    let live = ferrocast_vsthost::live(slot).unwrap_or_else(|| panic!("running"));
    assert!(live.running() && live.problem().is_none());
    assert_eq!(live.state().map(|b| ferrocast_vsthost::proto::state_of(&b).map(|(c, _)| c)), Some(Some(gain2)));
    // Closing a window that isn't open: fine, and quick.
    assert!(live.window(false).is_ok() || cfg!(not(target_os = "macos")));
    drop(live);
    drop(f);
    assert!(ferrocast_vsthost::live(slot).is_none(), "the filter gone, its helper goes too");
}
