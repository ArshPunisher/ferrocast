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

/// A plain sleep wakes a share of its length late on a Mac (20 ms: 7-10 ms), so a 21.3 ms block's
/// wake was 4.5 ms late at the median; a half sleep fixed that for 5 % of a core. `mach_wait_until`
/// is close to the block's time and costs about nothing.
#[test]
fn the_audio_clock_wakes_within_half_a_millisecond_for_about_nothing() {
    // The audio thread's own setup (its QoS class and, if it takes, the real-time class).
    let _guard = ferrocore::engine::raise_audio_thread();
    let c = SystemClock;
    let step = 21_333_333u64;
    let blocks = 150usize;
    let (cpu0, wall0) = (thread_cpu_ns(), std::time::Instant::now());
    let mut target = c.now_ns() + step;
    let mut late_us = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        c.sleep_until_ns(target);
        late_us.push(c.now_ns().saturating_sub(target) / 1000);
        target += step;
    }
    let cpu = thread_cpu_ns() - cpu0;
    let wall = wall0.elapsed().as_nanos() as u64;
    late_us.sort_unstable();
    let (p50, p99) = (late_us[blocks / 2], late_us[blocks * 99 / 100]);
    let cpu_pct = 100.0 * cpu as f64 / wall as f64;
    eprintln!("wake error p50 {p50} us, p99 {p99} us, thread CPU {cpu_pct:.2} % of wall");
    assert!(p50 < 250 && p99 < 500, "woke {p50} us (median) and {p99} us (p99) after the block's time");
    // A loaded machine reschedules a little: the bound is 1 %, a busy wait is 5-100 %.
    assert!(cpu_pct < 1.0, "the wait used {cpu_pct:.2} % of a core");
}
