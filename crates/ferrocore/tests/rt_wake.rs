//! The audio clock's wake: close to a block's time, without burning a core (macOS).
#![cfg(target_os = "macos")]
#![allow(unsafe_code)]

use ferrocore::rt::{AudioClock, SystemClock};

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a plain system call writing into `ts`, which is ours.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// One run: (median and p99 wake error in µs, the thread's CPU as a % of the wall time).
fn measure(blocks: usize) -> (u64, u64, f64) {
    let c = SystemClock;
    let step = 21_333_333u64;
    // A few blocks to settle (the first sleeps run on cold caches and a freshly changed class).
    let mut target = c.now_ns() + step;
    for _ in 0..10 {
        c.sleep_until_ns(target);
        target += step;
    }
    let (cpu0, wall0) = (thread_cpu_ns(), std::time::Instant::now());
    let mut late_us = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        c.sleep_until_ns(target);
        late_us.push(c.now_ns().saturating_sub(target) / 1000);
        target += step;
    }
    let cpu = thread_cpu_ns() - cpu0;
    let wall = wall0.elapsed().as_nanos() as u64;
    late_us.sort_unstable();
    (late_us[blocks / 2], late_us[blocks * 99 / 100], 100.0 * cpu as f64 / wall as f64)
}

/// A plain sleep wakes a share of its length late on a Mac (20 ms: 7-10 ms), so a 21.3 ms block's
/// wake was 4.5 ms late at the median (9.9 ms at p99); a half sleep fixed that for 5 % of a core.
/// `mach_wait_until` on the real-time class is close to the block's time and costs about nothing.
///
/// What it must still catch is a median in milliseconds or a core burned, not a loaded CI machine
/// (dozens of test binaries at once) delaying one wake: the median and the CPU are strict, the p99
/// loose (2 ms: the old code's was 9.9), and a run that misses gets two more tries; one good run
/// of the three passes.
#[test]
fn the_audio_clock_wakes_within_half_a_millisecond_for_about_nothing() {
    // A shared CI Mac (a VM with neighbours) can't promise sub-millisecond wakes, so its misses
    // say nothing about this code; run it on a real Mac (CI sets CI=true).
    if std::env::var_os("CI").is_some() {
        eprintln!("skipped on a shared CI runner: wake timing needs a real Mac");
        return;
    }
    // The audio thread's own setup (its QoS class and, if it takes, the real-time class).
    let _guard = ferrocore::engine::raise_audio_thread();
    let mut tries = Vec::new();
    for attempt in 1..=3 {
        let (p50, p99, cpu_pct) = measure(150);
        eprintln!("try {attempt}: wake error p50 {p50} us, p99 {p99} us, thread CPU {cpu_pct:.2} % of wall");
        // The bound is 1 % of a core; a busy wait is 5-100 %.
        if p50 < 250 && p99 < 2_000 && cpu_pct < 1.0 {
            return;
        }
        tries.push((p50, p99, cpu_pct));
    }
    panic!("no run woke within 250 us (median) and 2 ms (p99) at under 1 % CPU: {tries:?}");
}
