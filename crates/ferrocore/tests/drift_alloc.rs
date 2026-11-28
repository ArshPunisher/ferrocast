//! A source whose clock drifts (+300 ppm from 20 s in: a USB mic) starts following it mid-run:
//! the resampler is made ahead, so engaging it allocates nothing in the mix, and the sound has
//! no step beyond the signal's own. A test program of its own, and its counting allocator counts
//! only the test's own thread (where the mixer runs): the test runner's thread allocates too, when
//! it prints that a test has run for over a minute, which a slow machine makes this one do.

#![allow(unsafe_code, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocore::mix::{Mixer, SourceSettings};

struct Counting;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    /// Set on the test's own thread only. (A constant initialiser: reading it allocates nothing.)
    static COUNTED: Cell<bool> = const { Cell::new(false) };
}

fn counted() -> bool {
    COUNTED.try_with(Cell::get).unwrap_or(false)
}
static CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// SAFETY: everything is handed straight to the system allocator; the counter is an atomic add.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let mine = counted();
        if mine {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        if mine && CAPTURE.swap(false, Ordering::SeqCst) {
            // The first allocation inside the mixer: where from (the capture itself allocates).
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        // SAFETY: the caller's layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if counted() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: from `alloc`.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if counted() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: as above.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const RATE: u32 = 48_000;
const PIECE: usize = 480;

#[test]
fn engaging_drift_following_allocates_nothing_and_does_not_click() {
    COUNTED.with(|c| c.set(true));
    let mut m = Mixer::new(RATE, 40_000_000);
    m.set(1, SourceSettings::default());
    let mut out = Vec::with_capacity(64);
    let (hz, amp) = (440.0f64, 0.5f32);
    let steepest = amp * (2.0 * std::f64::consts::PI * hz / f64::from(RATE)) as f32;
    let t0 = 10_000_000_000u64;
    let (mut frame, mut last, mut worst) = (0u64, None::<f32>, 0.0f32);
    let mut engaged_at = None;
    let trace = std::env::var_os("DRIFT_TRACE").is_some();
    let pieces = 60_000u64; // 600 s
    for k in 0..pieces {
        let pcm: Vec<f32> = (0..PIECE)
            .flat_map(|i| {
                let v =
                    amp * (2.0 * std::f64::consts::PI * hz * (frame + i as u64) as f64 / f64::from(RATE)).sin() as f32;
                [v, v]
            })
            .collect();
        frame += PIECE as u64;
        // Stamped on arrival: exact for 20 s, then the card runs 300 ppm fast (its pieces come
        // that much sooner than the clock says).
        let ideal = k * 10_000_000;
        let drift = if k < 2_000 { 0.0 } else { (k - 2_000) as f64 * 10_000_000.0 * 300e-6 };
        let ts = t0 + (ideal as f64 - drift) as u64;
        // (the pcm Vec above is the test's own; only the mixer's allocations matter, so measure
        // around the mixer calls)
        let before = ALLOCATIONS.load(Ordering::SeqCst);
        CAPTURE.store(k >= 2_000 && trace, Ordering::SeqCst);
        m.push(1, &pcm, 2, RATE, ts);
        m.mix_until_into(ts + 10_000_000, &mut out);
        CAPTURE.store(false, Ordering::SeqCst);
        let in_mixer = ALLOCATIONS.load(Ordering::SeqCst) - before;
        if k >= 2_000 && in_mixer > 0 {
            panic!("the mixer allocated {in_mixer} times at piece {k} (engaged: {engaged_at:?})");
        }
        for b in out.drain(..) {
            for f in b.tracks[0].as_chunks::<2>().0 {
                if let Some(l) = last
                    && k > 100
                {
                    worst = worst.max((f[0] - l).abs());
                }
                last = Some(f[0]);
            }
            m.recycle(b);
        }
        if engaged_at.is_none() && m.follows_drift(1) {
            engaged_at = Some(k);
        }
    }
    assert!(engaged_at.is_some(), "the drift was never followed: the test doesn't reach what it checks");
    assert!(worst <= steepest * 1.25, "a step of {worst} in the sound (limit {})", steepest * 1.25);
}
