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

// SAFETY: everything is handed straight to the system allocator; the counter is an atomic add.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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

const OUT: u32 = 48_000;
const IN: u32 = 44_100;

#[test]
fn a_44k_source_added_to_a_48k_mix_allocates_nothing_and_is_heard_soon() {
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
        m.push(1, &a, 2, OUT, ts);
        if k >= start_piece {
            m.push(2, &b, 2, IN, ts);
            frame_in += 441;
        }
        m.mix_until_into(ts + 10_000_000, &mut out);
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
