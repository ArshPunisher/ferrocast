//! Pushing to and popping from the audio ring never allocate (D1, `rt.rs`). This is a test
//! program of its own so its counting allocator sees only this one test.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocore::mix::Levels;
use ferrocore::rt::{AtomicLevels, SpscAudioRing};

/// The system allocator, counting every allocation, reallocation and zeroed allocation.
struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: everything is handed straight to the system allocator, which upholds the contract;
// the counter is only an atomic add beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's layout, passed on unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout are the ones given out by `alloc` above.
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
fn pushing_popping_and_metering_allocate_nothing() {
    // Everything that allocates happens first.
    let (mut producer, mut consumer) = SpscAudioRing::for_seconds(1, 48_000, 2, 1024);
    let levels = AtomicLevels::shared();
    let pcm = vec![0.5f32; 2048];
    let mut out = vec![0.0f32; 2048];
    let l = Levels { peak: [-3.0, -4.0], rms: [-20.0, -21.0], input_peak: [-1.0, -2.0], gated: false };

    let before = ALLOCATIONS.load(Ordering::SeqCst);
    for n in 0..20_000u64 {
        producer.push(&pcm, 2, 48_000, n).unwrap();
        assert!(consumer.peek().is_some());
        consumer.pop_into(&mut out).unwrap();
        levels.store(&l);
        assert_eq!(levels.load(), l);
    }
    // A full ring drops without allocating too.
    while producer.push(&pcm, 2, 48_000, 0).is_ok() {}
    assert!(producer.push(&pcm, 2, 48_000, 0).is_err());
    consumer.clear();
    let after = ALLOCATIONS.load(Ordering::SeqCst);

    assert_eq!(after - before, 0, "{} allocations in 20,000 pushes and pops", after - before);
}
