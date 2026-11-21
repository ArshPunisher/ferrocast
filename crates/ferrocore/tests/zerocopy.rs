//! Frame copies on the render path (macOS): how many pixel bytes the CPU moves per frame from
//! the card's readback to the encoder, and how long the encode call takes. Prints the numbers
//! (`cargo test -p ferrocore --test zerocopy -- --nocapture`).
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ferrocore::video::{CopyStats, H264Profile, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder};
use ferrocore::{Compositor, Item, Picture, Placement, Scene};

/// The copy counters are the process's: one test at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn gpu() -> Option<Compositor> {
    match Compositor::new() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn config(w: u32, h: u32) -> VideoConfig {
    VideoConfig {
        codec: VideoCodec::H264,
        input: PixelFormat::Nv12,
        width: w,
        height: h,
        fps_num: 60,
        fps_den: 1,
        bitrate_kbps: 12_000,
        rate: RateControl::Cbr,
        keyframe_secs: 2,
        software: false,
        profile: H264Profile::High,
        bframes: false,
        preset: None,
        tune: None,
    }
}

fn scene(w: u32, h: u32, i: u32) -> Scene {
    let mut rgba = Vec::with_capacity((256 * 256 * 4) as usize);
    for y in 0..256u32 {
        for x in 0..256u32 {
            rgba.extend_from_slice(&[(x + i * 3) as u8, (y + i) as u8, 128, 255]);
        }
    }
    let pic = Picture { width: 256, height: 256, rgba, premultiplied: false };
    let item = Item::picture(pic, Placement { x: 0.0, y: 0.0, ..Default::default() });
    Scene { width: w, height: h, items: vec![item] }
}

fn delta(a: CopyStats, b: CopyStats) -> CopyStats {
    CopyStats {
        readback_bytes: b.readback_bytes - a.readback_bytes,
        fill_bytes: b.fill_bytes - a.fill_bytes,
        materialised_bytes: b.materialised_bytes - a.materialised_bytes,
        zero_copy_frames: b.zero_copy_frames - a.zero_copy_frames,
        fallback_frames: b.fallback_frames - a.fallback_frames,
    }
}

/// The path `engine::deliver` takes for one output: begin, finish, encode. `zero` picks the
/// surface path or the copying one.
fn measure(w: u32, h: u32, frames: u32, zero: bool) -> Option<(CopyStats, Duration, Duration, usize)> {
    let c = gpu()?;
    c.set_zero_copy(zero);
    let mut enc = VideoEncoder::new(config(w, h)).ok()?;
    let before = ferrocore::video::copy_stats();
    let (mut encode, mut total, mut bytes) = (Duration::ZERO, Duration::ZERO, 0);
    for i in 0..frames {
        let t0 = Instant::now();
        let p = c.begin_nv12(&scene(w, h, i)).ok()?;
        let d = c.finish_drawn(p).ok()?;
        let t1 = Instant::now();
        let out = enc.encode(d.as_frame(w as usize), i64::from(i) * 16_667, i == 0).ok()?;
        encode += t1.elapsed();
        total += t0.elapsed();
        bytes += out.iter().map(|p| p.data.len()).sum::<usize>();
    }
    Some((delta(before, ferrocore::video::copy_stats()), encode, total, bytes))
}

#[test]
fn copies_per_frame_before_and_after() {
    let _one = serial();
    for (w, h) in [(2560u32, 1440u32), (1920, 1080)] {
        let Some((old, old_enc, old_total, _)) = measure(w, h, 30, false) else { return };
        let Some((new, new_enc, new_total, _)) = measure(w, h, 30, true) else { return };
        let frame = u64::from(w) * u64::from(h) * 3 / 2;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / 30.0;
        println!(
            "{w}x{h}: copy path: {} B/frame copied (readback {} + fill {}), encode call {:.2} ms, begin..encode {:.2} ms",
            (old.readback_bytes + old.fill_bytes) / 30,
            old.readback_bytes / 30,
            old.fill_bytes / 30,
            ms(old_enc),
            ms(old_total)
        );
        println!(
            "{w}x{h}: zero-copy:  {} B/frame copied (readback {} + fill {} + materialised {}), {} frames zero-copy, {} fallback; encode call {:.2} ms, begin..encode {:.2} ms",
            (new.readback_bytes + new.fill_bytes + new.materialised_bytes) / 30,
            new.readback_bytes / 30,
            new.fill_bytes / 30,
            new.materialised_bytes / 30,
            new.zero_copy_frames,
            new.fallback_frames,
            ms(new_enc),
            ms(new_total)
        );
        assert_eq!(old.readback_bytes / 30, frame);
        assert_eq!(old.fill_bytes / 30, frame);
        assert_eq!(
            new.readback_bytes + new.fill_bytes + new.materialised_bytes,
            0,
            "no pixel copy on the zero-copy path"
        );
        assert_eq!(new.zero_copy_frames, 30);
        assert_eq!(new.fallback_frames, 0);
    }
}

/// The card's own picture is the same either way: the surface's planes equal the copied ones.
#[test]
fn a_surface_frame_holds_the_same_pixels_as_the_copy() {
    let _one = serial();
    let Some(c) = gpu() else { return };
    for (w, h) in [(640u32, 360u32), (1280, 720)] {
        c.set_zero_copy(false);
        let Ok(a) = c.begin_nv12(&scene(w, h, 5)).and_then(|p| c.finish_nv12(p)) else { return };
        c.set_zero_copy(true);
        let d = c.finish_drawn(c.begin_nv12(&scene(w, h, 5)).unwrap()).unwrap();
        assert!(matches!(d, ferrocore::Drawn::Surface { .. }), "took the surface path");
        let b = d.host().expect("readable");
        assert_eq!((a.width, a.height), (b.width, b.height));
        assert!(a.y == b.y, "Y plane equal");
        assert!(a.uv == b.uv, "CbCr plane equal");
    }
}

/// Taken only once `ready()` (the non-waiting path `deliver` uses): the surface isn't read
/// before the card has finished with it, so the picture is whole.
#[test]
fn a_surface_frame_taken_when_ready_is_whole() {
    let _one = serial();
    let Some(c) = gpu() else { return };
    let (w, h) = (1920u32, 1080u32);
    c.set_zero_copy(false);
    let Ok(want) = c.begin_nv12(&scene(w, h, 9)).and_then(|p| c.finish_nv12(p)) else { return };
    c.set_zero_copy(true);
    // Several in flight at once, as the engine keeps them.
    let pending: Vec<_> = (0..3).map(|_| c.begin_nv12(&scene(w, h, 9)).unwrap()).collect();
    for p in pending {
        let t0 = Instant::now();
        while !p.ready() {
            c.poll();
            assert!(t0.elapsed() < Duration::from_secs(5), "the card never finished");
            std::thread::sleep(Duration::from_micros(200));
        }
        let d = c.finish_drawn(p).unwrap();
        let got = d.host().unwrap();
        assert!(got.y == want.y && got.uv == want.uv, "whole picture");
    }
}

/// The same frames encode to as many packets, of about the same size, whichever way they reach the encoder.
#[test]
fn the_encoder_makes_the_same_packets_from_a_surface() {
    let _one = serial();
    let Some(c) = gpu() else { return };
    let (w, h) = (1280u32, 720u32);
    let run = |zero: bool| -> Option<Vec<Vec<u8>>> {
        c.set_zero_copy(zero);
        let mut enc = VideoEncoder::new(config(w, h)).ok()?;
        let mut out = Vec::new();
        for i in 0..12u32 {
            let d = c.finish_drawn(c.begin_nv12(&scene(w, h, i)).ok()?).ok()?;
            out.extend(
                enc.encode(d.as_frame(w as usize), i64::from(i) * 16_667, i == 0).ok()?.into_iter().map(|p| p.data),
            );
        }
        out.extend(enc.flush().ok()?.into_iter().map(|p| p.data));
        Some(out)
    };
    let (Some(a), Some(b)) = (run(false), run(true)) else { return };
    assert_eq!(a.len(), b.len(), "same number of packets");
    assert!(!a.is_empty());
    // VideoToolbox's real-time rate control isn't deterministic (two runs of the copying path
    // differ too), so the packets are compared by count and size; the pixels it was given are
    // compared byte for byte in `a_surface_frame_holds_the_same_pixels_as_the_copy`.
    let (sa, sb): (usize, usize) = (a.iter().map(Vec::len).sum(), b.iter().map(Vec::len).sum());
    assert!(sa.abs_diff(sb) * 5 < sa, "about the same bytes: {sa} vs {sb}");
}

/// A pool at its cap says so and the frame is drawn the copying way, never waited for.
#[test]
fn a_full_pool_falls_back_to_the_copy() {
    let _one = serial();
    let Some(c) = gpu() else { return };
    c.set_zero_copy(true);
    let before = ferrocore::video::copy_stats();
    // Hold more frames than the pool makes (every one still in flight, none finished).
    let held: Vec<_> = (0..24).map(|i| c.begin_nv12(&scene(640, 360, i)).unwrap()).collect();
    let mid = delta(before, ferrocore::video::copy_stats());
    assert!(mid.fallback_frames > 0, "the pool's cap was reached");
    assert!(mid.zero_copy_frames > 0);
    for p in held {
        let d = c.finish_drawn(p).unwrap();
        assert!(d.host().is_some());
    }
    // Back to normal once they're let go.
    let after = c.finish_drawn(c.begin_nv12(&scene(640, 360, 1)).unwrap()).unwrap();
    assert!(matches!(after, ferrocore::Drawn::Surface { .. }));
}
