//! High-resolution waitable timer sleeps on Windows.
//!
//! Windows' default scheduler tick is 15.625 ms, so `std::thread::sleep(Duration::from_millis(5))`
//! or sleeping 20 ms for a 21.333 ms audio block wakes up to 15 ms late unless the process alters
//! the system-wide timer period (`timeBeginPeriod(1)`) or uses a high-resolution waitable timer.
//! `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803+) gives ~0.5 ms wakeups scoped to
//! Ferrocast's own threads without raising system-wide power draw, falling back to
//! `std::thread::sleep` on older builds.

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer, TIMER_ALL_ACCESS,
    WaitForSingleObject,
};
use windows::core::PCWSTR;

struct HighResTimer(HANDLE);

impl HighResTimer {
    fn new() -> Option<Self> {
        // SAFETY: null security attributes and unnamed timer; handle is closed in `Drop`.
        let handle = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
        }
        .ok()?;
        Some(Self(handle))
    }

    fn wait_relative(&self, due_100ns: i64) -> bool {
        // SAFETY: `self.0` is an open waitable timer owned by this thread-local, and `due_100ns`
        // points to a stack `i64` alive for the call.
        if unsafe { SetWaitableTimer(self.0, &due_100ns, 0, None, None, false) }.is_err() {
            return false;
        }
        // SAFETY: `self.0` is a valid waitable timer handle armed above.
        unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        // SAFETY: `self.0` was created by `CreateWaitableTimerExW` and is closed once on thread exit.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

thread_local! {
    static THREAD_TIMER: Option<HighResTimer> = HighResTimer::new();
}

/// Sleeps for `dur` using a per-thread high-resolution waitable timer, falling back to
/// `std::thread::sleep` if the OS build does not support `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`
/// or the thread is tearing down TLS.
pub(crate) fn sleep(dur: Duration) {
    let Some(due) = crate::win_logic::relative_due_100ns(dur) else {
        return;
    };
    let ok = THREAD_TIMER.try_with(|slot| slot.as_ref().is_some_and(|t| t.wait_relative(due))).unwrap_or(false);
    if !ok {
        std::thread::sleep(dur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn high_res_timer_sleeps_below_default_15ms_tick() {
        sleep(Duration::ZERO);
        let mut elapsed_ms = Vec::with_capacity(10);
        for _ in 0..10 {
            let t0 = Instant::now();
            sleep(Duration::from_millis(2));
            elapsed_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        elapsed_ms.sort_by(f64::total_cmp);
        let median = elapsed_ms[elapsed_ms.len() / 2];
        assert!(median >= 1.5, "timer returned too early: {median:.2} ms");
        assert!(median < 10.0, "timer stuck on 15.6 ms tick: median {median:.2} ms ({elapsed_ms:?})");
    }
}
