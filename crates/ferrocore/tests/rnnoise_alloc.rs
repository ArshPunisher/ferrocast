//! RNNoise on the audio thread allocates nothing once built (blocks of odd sizes, enable
//! toggles and retunes included). A test program of its own: its allocator counts only this.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

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
use ferrocore::afx::{AudioEffect, AudioFilter};
use ferrocore::rnnoise::Rnnoise;

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn rnnoise_process_allocates_nothing() {
    let mut f = AudioFilter::new_without_plugin(AudioEffect::Rnnoise { amount: 100 }, 48_000);
    f.install_plugin(Box::new(Rnnoise::new(100)));
    let (mut l, mut r) = (vec![0.3f32; 1000], vec![-0.2f32; 1000]);
    f.process([&mut l, &mut r]);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 0..400 {
        for (k, v) in l.iter_mut().enumerate() {
            *v = ((i * 1000 + k) as f32 * 0.05).sin() * 0.9;
        }
        r.copy_from_slice(&l);
        if i % 50 == 0 {
            f.set_enabled(i % 100 != 0);
            assert!(f.retune(AudioEffect::Rnnoise { amount: 20 + (i / 50) as u32 * 10 }));
        }
        let n = 100 + (i % 7) * 131;
        f.process([&mut l[..n], &mut r[..n]]);
    }
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed) - before, 0, "audio-thread allocations");
    assert_eq!(f.latency_frames(), 480);
}
