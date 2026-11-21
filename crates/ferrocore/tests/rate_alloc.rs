//! A 44.1 kHz source appears while a 48 kHz mix runs: its resampler is made on a background
//! thread (played here by the test, outside the counted calls), the mixer holds the source's
//! sound until it is there, and nothing is allocated in the mix. The sound shows up within
//! 100 ms and has no step beyond the signal's own. A test program of its own (counting
//! allocator).

#![allow(unsafe_code, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocore::mix::{Mixer, SourceSettings};

struct Counting;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// SAFETY: everything is handed straight to the system allocator; the counter is an atomic add.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        if CAPTURE.swap(false, Ordering::SeqCst) {
            // Where the first allocation in the mixer came from (the capture allocates too).
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        // SAFETY: the caller's layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: from `alloc`.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The two tests share one counting allocator: one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const OUT: u32 = 48_000;
const IN: u32 = 44_100;

#[test]
fn a_44k_source_added_to_a_48k_mix_allocates_nothing_and_is_heard_soon() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mixer::new(OUT, 40_000_000);
    m.set_background_resamplers(true);
    m.set(1, SourceSettings::default());
    m.set(2, SourceSettings::default());
    let mut out = Vec::with_capacity(64);
    let (hz, amp) = (440.0f64, 0.5f32);
    let steepest = amp * (2.0 * std::f64::consts::PI * hz / f64::from(OUT)) as f32;
    let t0 = 10_000_000_000u64;
    let start_piece = 300u64; // 3 s in
    let (mut frame_in, mut frame_48) = (0u64, 0u64);
    let (mut last, mut worst) = (None::<f32>, 0.0f32);
    let mut first_heard = None;
    let mut worst_at = 0;
    let mut asked = Vec::new();
    let trace = std::env::var_os("RATE_TRACE").is_some();
    for k in 0..1_000u64 {
        let ts = t0 + k * 10_000_000;
        // Source 1: 48 kHz, steady.
        let a: Vec<f32> = (0..480)
            .flat_map(|i| {
                let v =
                    0.1 * (2.0 * std::f64::consts::PI * 220.0 * (frame_48 + i) as f64 / f64::from(OUT)).sin() as f32;
                [v, v]
            })
            .collect();
        frame_48 += 480;
        // Source 2: 44.1 kHz, from piece 300: 441 frames every 10 ms.
        let b: Vec<f32> = (0..441)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f64::consts::PI * hz * (frame_in + i) as f64 / f64::from(IN)).sin() as f32;
                [v, v]
            })
            .collect();
        let before = ALLOCATIONS.load(Ordering::SeqCst);
        CAPTURE.store(trace && k >= start_piece, Ordering::SeqCst);
        m.push(1, &a, 2, OUT, ts);
        if k >= start_piece {
            m.push(2, &b, 2, IN, ts);
            frame_in += 441;
        }
        m.mix_until_into(ts + 10_000_000, &mut out);
        CAPTURE.store(false, Ordering::SeqCst);
        let in_mixer = ALLOCATIONS.load(Ordering::SeqCst) - before;
        assert_eq!(in_mixer, 0, "the mixer allocated {in_mixer} times at piece {k}");
        // The background thread's part: what was asked for is made (not counted) and handed in
        // a few ms later (here: before the next piece).
        while let Some(rate) = m.pop_resampler_request() {
            asked.push(rate);
            m.add_spare_resampler(Mixer::make_spare_resampler(rate, OUT).unwrap());
        }
        for blk in out.drain(..) {
            if first_heard.is_none() && k >= start_piece && blk.tracks[0].iter().any(|v| v.abs() > 0.2) {
                first_heard = Some(k);
            }
            // Source 2 is on track 2 (settings default: track 1 both; read the mix track).
            for f in blk.tracks[0].as_chunks::<2>().0 {
                // Judged on both sines together (each one's steepest slope adds); the abrupt
                // start of a source is not a click of the resampler, so the first blocks are left out.
                if let Some(l) = last
                    && k > 20
                    && first_heard.is_none_or(|h| k > h + 5)
                    && (f[0] - l).abs() > worst
                {
                    worst = (f[0] - l).abs();
                    worst_at = k;
                }
                last = Some(f[0]);
            }
            m.recycle(blk);
        }
    }
    assert!(asked.contains(&IN), "a resampler for 44.1 kHz was asked for: {asked:?}");
    let heard = first_heard.expect("the 44.1 kHz source was never heard");
    assert!(heard - start_piece <= 10 + 4, "heard {} pieces (10 ms each) after it started", heard - start_piece);
    let own = steepest + 0.1 * (2.0 * std::f64::consts::PI * 220.0 / f64::from(OUT)) as f32;
    assert!(worst <= own * 1.25, "a step of {worst} at piece {worst_at} (heard from {heard}) (limit {})", own * 1.25);
}

/// A short sound at 44.1 kHz (an alert: 60 ms) that arrives before its resampler does (the
/// background thread takes 30 ms here) comes out whole: what was held is run through when the
/// resampler arrives, nothing is dropped.
#[test]
fn a_short_44k_sound_that_beats_its_resampler_keeps_its_start() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mixer::new(OUT, 40_000_000);
    m.set_background_resamplers(true);
    m.set(2, SourceSettings::default());
    let mut out = Vec::with_capacity(64);
    let t0 = 10_000_000_000u64;
    let mut frame_in = 0u64;
    let mut input: Vec<f32> = Vec::new();
    let mut energy_out = 0.0f64;
    let mut asked_at = None;
    for k in 0..300u64 {
        let ts = t0 + k * 10_000_000;
        let mut before = ALLOCATIONS.load(Ordering::SeqCst);
        if (100..106).contains(&k) {
            // A cosine: it starts at its peak, so a missing start is a missing loud part.
            let b: Vec<f32> = (0..441)
                .flat_map(|i| {
                    let v =
                        0.5 * (2.0 * std::f64::consts::PI * 440.0 * (frame_in + i) as f64 / f64::from(IN)).cos() as f32;
                    input.push(v);
                    [v, v]
                })
                .collect();
            frame_in += 441;
            before = ALLOCATIONS.load(Ordering::SeqCst);
            m.push(2, &b, 2, IN, ts);
        }
        m.mix_until_into(ts + 10_000_000, &mut out);
        m.release_held();
        let in_mixer = ALLOCATIONS.load(Ordering::SeqCst) - before;
        assert_eq!(in_mixer, 0, "the mixer allocated {in_mixer} times at piece {k}");
        // The background thread: the request is answered 3 pieces (30 ms) later.
        if let Some(rate) = m.pop_resampler_request() {
            asked_at = Some((k, rate));
        }
        if let Some((at, rate)) = asked_at
            && k == at + 3
        {
            m.add_spare_resampler(Mixer::make_spare_resampler(rate, OUT).unwrap());
            asked_at = None;
            m.release_held();
        }
        for blk in out.drain(..) {
            energy_out += blk.tracks[0].iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>();
            m.recycle(blk);
        }
    }
    // The resampler takes whole chunks of 480 and keeps the last ~64 frames of each inside: what
    // has come out covers the first `done` input frames. The same sound at 48 kHz carries 48/44.1
    // as many samples at the same level. A dropped first piece (441 frames) would be ~19% short.
    let done = input.len() / 480 * 480 - 64;
    let energy_in: f64 = input[..done].iter().map(|v| f64::from(*v) * f64::from(*v) * 2.0).sum();
    let expect = energy_in * f64::from(OUT) / f64::from(IN);
    assert!(energy_out > 0.92 * expect && energy_out < 1.08 * expect, "out {energy_out:.2} vs {expect:.2} expected");
}

#[test]
fn a_mono_44k_source_held_over_half_second_allocates_nothing_on_release() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mixer::new(OUT, 40_000_000);
    m.set_background_resamplers(true);
    m.set(1, SourceSettings::default());
    let t0 = 10_000_000_000u64;
    let _ = m.mix_until(t0);
    let mono = vec![0.25f32; 882]; // 20 ms mono at 44.1 kHz
    // Pre-build the resampler outside the measured window.
    let rs = Mixer::make_spare_resampler(IN, OUT).unwrap();
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for k in 0..45u64 {
        m.push(1, &mono, 1, IN, t0 + k * 20_000_000);
    }
    let after_push = ALLOCATIONS.load(Ordering::SeqCst);
    assert_eq!(after_push - before, 0, "mono hold allocated during push");
    m.add_spare_resampler(rs);
    let before_rel = ALLOCATIONS.load(Ordering::SeqCst);
    m.release_held();
    let after_rel = ALLOCATIONS.load(Ordering::SeqCst);
    assert_eq!(after_rel - before_rel, 0, "mono release_held allocated {} times", after_rel - before_rel);
}
