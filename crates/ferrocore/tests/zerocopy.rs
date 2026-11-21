//! Frame copies on the render path (macOS): how many pixel bytes the CPU moves per frame from
//! the card's readback to the encoder, and how long the encode call takes. Prints the numbers
//! (`cargo test -p ferrocore --test zerocopy -- --nocapture`).
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use ferrocore::video::{CopyStats, Frame, H264Profile, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder};
use ferrocore::{Compositor, Item, Picture, Placement, Scene};

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

/// The path `engine::deliver` takes for one output: begin, finish, encode.
fn measure(w: u32, h: u32, frames: u32) -> Option<(CopyStats, Duration, Duration)> {
    let c = gpu()?;
    let mut enc = VideoEncoder::new(config(w, h)).ok()?;
    let before = ferrocore::video::copy_stats();
    let (mut encode, mut total) = (Duration::ZERO, Duration::ZERO);
    for i in 0..frames {
        let t0 = Instant::now();
        let p = c.begin_nv12(&scene(w, h, i)).ok()?;
        let f = c.finish_nv12(p).ok()?;
        let t1 = Instant::now();
        enc.encode(Frame::Nv12 { y: &f.y, y_stride: w as usize, uv: &f.uv, uv_stride: w as usize }, i64::from(i) * 16_667, i == 0)
            .ok()?;
        encode += t1.elapsed();
        total += t0.elapsed();
    }
    Some((delta(before, ferrocore::video::copy_stats()), encode, total))
}

#[test]
fn baseline_copies_per_frame() {
    for (w, h) in [(2560u32, 1440u32), (1920, 1080)] {
        let Some((s, enc, total)) = measure(w, h, 30) else { return };
        let frame = u64::from(w) * u64::from(h) * 3 / 2;
        println!(
            "{w}x{h}: per frame readback {} B, fill {} B (frame is {frame} B); encode call {:.2} ms, whole {:.2} ms",
            s.readback_bytes / 30,
            s.fill_bytes / 30,
            enc.as_secs_f64() * 1000.0 / 30.0,
            total.as_secs_f64() * 1000.0 / 30.0
        );
        assert_eq!(s.readback_bytes / 30, frame);
        assert_eq!(s.fill_bytes / 30, frame);
    }
}
