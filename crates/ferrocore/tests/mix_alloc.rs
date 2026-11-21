//! A steady mix allocates nothing (D1, Commit 2): the mixer's working buffers and the blocks it
//! hands out are made once and reused. A test program of its own, so its counting allocator
//! sees only this one test.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocore::afx::AudioEffect;
use ferrocore::mix::{Block, Mixer, SourceSettings};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: everything is handed straight to the system allocator, which upholds the contract; the
// counter is an atomic add beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's layout, unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: a pointer and layout given out by `alloc` above.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn a_steady_mix_allocates_nothing() {
    let mut m = Mixer::new(48_000, 40_000_000);
    m.set(1, SourceSettings::default());
    m.set(2, SourceSettings::default());
    // Source 3 is on push-to-talk and source 2 on push-to-mute: their keys go up and down below.
    m.set(2, SourceSettings { push_to_mute: Some(200), ..SourceSettings::default() });
    m.set(3, SourceSettings { push_to_talk: Some(150), ..SourceSettings::default() });
    m.set_effects(
        3,
        &[
            AudioEffect::Gain { db: -2.0 },
            AudioEffect::InvertPolarity,
            AudioEffect::NoiseGate {
                open_db: -26.0,
                close_db: -32.0,
                attack_ms: 25.0,
                hold_ms: 200.0,
                release_ms: 150.0,
            },
            AudioEffect::Compressor {
                ratio: 4.0,
                threshold_db: -18.0,
                attack_ms: 6.0,
                release_ms: 60.0,
                output_db: 0.0,
                sidechain: None,
            },
            AudioEffect::Compressor {
                ratio: 10.0,
                threshold_db: -24.0,
                attack_ms: 10.0,
                release_ms: 150.0,
                output_db: 0.0,
                sidechain: Some(1),
            },
            AudioEffect::Rnnoise { amount: 100 },
            AudioEffect::Limiter { threshold_db: -1.0, release_ms: 60.0 },
            AudioEffect::Expander {
                ratio: 2.0,
                threshold_db: -40.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                rms: true,
            },
            AudioEffect::UpwardCompressor {
                ratio: 0.5,
                threshold_db: -20.0,
                attack_ms: 10.0,
                release_ms: 50.0,
                output_db: 0.0,
                knee_db: 10.0,
            },
            AudioEffect::Eq3 { low_db: 2.0, mid_db: -1.0, high_db: 3.0 },
            AudioEffect::Denoise { strength: 50 },
        ],
    );
    m.set_true_peak(true);
    let mic = vec![0.2f32; 960 * 2];
    let card = vec![0.1f32; 882 * 2];
    let mut out: Vec<Block> = Vec::with_capacity(8);
    let t0 = 10_000_000_000u64;
    let _ = m.mix_until(t0);
    let mut step = |m: &mut Mixer, k: u64| {
        let at = t0 + k * 20_000_000;
        // A key pressed and let go over and over (the gate, its delay counters and the levels).
        if k.is_multiple_of(25) {
            m.push_key(3, true, (k / 25).is_multiple_of(2));
            m.push_key(2, false, (k / 25).is_multiple_of(3));
        }
        m.push(1, &mic, 2, 48_000, at);
        m.push(2, &card, 2, 44_100, at);
        m.push(3, &mic, 2, 48_000, at);
        m.mix_until_into(at + 20_000_000, &mut out);
        for b in out.drain(..) {
            m.recycle(b);
        }
    };
    // Everything that allocates happens in the warm-up.
    for k in 0..200 {
        step(&mut m, k);
    }
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for k in 200..1200 {
        step(&mut m, k);
    }
    let after = ALLOCATIONS.load(Ordering::SeqCst);
    assert_eq!(after - before, 0, "{} allocations in 1,000 steady blocks of three sources", after - before);

    // M5: dragging/reordering effects via set_effects_nonblocking allocates 0 times on the mixer.
    let mut bid = 1u64;
    let mut next_id = || {
        let cur = bid;
        bid += 1;
        cur
    };
    let fx_a = [AudioEffect::Gain { db: -1.0 }, AudioEffect::Eq3 { low_db: 1.0, mid_db: 0.0, high_db: 2.0 }];
    let fx_b = [AudioEffect::Eq3 { low_db: 1.5, mid_db: -0.5, high_db: 2.5 }, AudioEffect::Gain { db: -3.0 }];
    m.set(4, SourceSettings::default());
    let (evicted, missing) = m.set_effects_nonblocking(4, &fx_a, &mut next_id, &|_, _| false);
    assert!(evicted.is_empty() && missing.is_empty());
    let before_fx = ALLOCATIONS.load(Ordering::SeqCst);
    for round in 0..20 {
        let fx = if round % 2 == 0 { &fx_b } else { &fx_a };
        let (evicted, missing) = m.set_effects_nonblocking(4, fx, &mut next_id, &|_, _| false);
        assert!(evicted.is_empty() && missing.is_empty());
    }
    let after_fx = ALLOCATIONS.load(Ordering::SeqCst);
    assert_eq!(after_fx - before_fx, 0, "effect retune/reorder allocated {} times", after_fx - before_fx);
}
