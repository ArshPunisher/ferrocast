//! Putting a delayed filter on a running source makes no large allocation. A test program of its own, so
//! its counting allocator sees only this one test.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocore::afx::AudioEffect;
use ferrocore::mix::{Block, Mixer, SourceSettings};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// The largest allocation made since the last reset.
static BIGGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: everything is handed straight to the system allocator, which upholds the contract; the
// counter is an atomic add beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BIGGEST.fetch_max(layout.size(), Ordering::Relaxed);
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
        BIGGEST.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

static LAT: AtomicUsize = AtomicUsize::new(3968);

#[derive(Debug)]
struct Late([std::collections::VecDeque<f32>; 2]);
impl ferrocore::afx::SoundPlugin for Late {
    fn process(&mut self, ch: [&mut [f32]; 2]) -> bool {
        for (c, q) in ch.into_iter().zip(self.0.iter_mut()) {
            for v in c.iter_mut() {
                q.push_back(*v);
                *v = q.pop_front().unwrap_or(0.0);
            }
        }
        true
    }
    fn latency_frames(&self) -> usize {
        LAT.load(Ordering::Relaxed)
    }
}

/// Putting a filter that holds sound back on a running source (the dry bypass it makes room
/// for when it is asked for) makes no large allocation while it goes on and is played.
#[test]
fn attaching_a_latency_filter_allocates_nothing_large() {
    if std::env::var_os("NOLAT").is_some() {
        LAT.store(0, Ordering::Relaxed);
    }
    let mut m = Mixer::new(48_000, 40_000_000);
    m.set(1, SourceSettings::default());
    let mic = vec![0.2f32; 960 * 2];
    let mut out: Vec<Block> = Vec::with_capacity(8);
    let mut heard: Vec<f32> = Vec::with_capacity(1_000_000);
    let t0 = 10_000_000_000u64;
    let _ = m.mix_until(t0);
    let mut step = |m: &mut Mixer, heard: &mut Vec<f32>, k: u64| {
        let at = t0 + k * 20_000_000;
        m.push(1, &mic, 2, 48_000, at);
        m.mix_until_into(at + 20_000_000, &mut out);
        for b in out.drain(..) {
            heard.extend(b.tracks[0].iter().copied());
            m.recycle(b);
        }
    };
    for k in 0..100 {
        step(&mut m, &mut heard, k);
    }
    // Asked for (room made here), built elsewhere, then installed.
    let effect = AudioEffect::Gain { db: 0.0 };
    let next = 7u64;
    let (_gone, missing) = m.set_effects_nonblocking(1, &[effect], &mut || next, &|_, _| true);
    assert_eq!(missing.len(), 1);
    let plugin: Box<dyn ferrocore::afx::SoundPlugin> = Box::new(Late([
        {
            let mut q = std::collections::VecDeque::with_capacity(4096);
            q.extend(std::iter::repeat_n(0.0, 3968));
            q
        },
        {
            let mut q = std::collections::VecDeque::with_capacity(4096);
            q.extend(std::iter::repeat_n(0.0, 3968));
            q
        },
    ]));
    BIGGEST.store(0, Ordering::SeqCst);
    let from = heard.len();
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    let out_ = m.attach_plugins(1, vec![(7, effect, plugin)]);
    assert!(out_.rejected.is_empty());
    for k in 100..400 {
        step(&mut m, &mut heard, k);
    }
    let after = ALLOCATIONS.load(Ordering::SeqCst);
    // The dry bypass played (a constant sound has no silence in it) and it was made without the
    // big allocation only because room was made when the filter was asked for.
    let tail = &heard[from + 2048 * 3..];
    assert!(tail.iter().all(|v| *v > 0.1), "a hole in the sound after the filter went on");
    // A few small bookkeeping allocations (a boxed filter's state) may happen; the dry bypass
    // (67 KB, made when the filter was asked for), the queue and the blocks must not.
    let big = BIGGEST.load(Ordering::SeqCst);
    assert!(big < 1024, "an allocation of {big} bytes ({} in all) putting a delayed filter on", after - before);
}
