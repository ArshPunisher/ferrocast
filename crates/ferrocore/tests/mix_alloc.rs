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
    m.set(3, SourceSettings::default());
    m.set_effects(3, &[AudioEffect::Gain { db: -2.0 }]);
    m.set_true_peak(true);
    let mic = vec![0.2f32; 960 * 2];
    let card = vec![0.1f32; 882 * 2];
    let mut out: Vec<Block> = Vec::with_capacity(8);
    let t0 = 10_000_000_000u64;
    let _ = m.mix_until(t0);
    let mut step = |m: &mut Mixer, k: u64| {
        let at = t0 + k * 20_000_000;
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
}
