//! Windows sleep inhibition via `SetThreadExecutionState`.
//!
//! Adapted from OBS `libobs/util/platform-windows.c` (`os_inhibit_sleep_set_active`), pinned to a
//! single persistent worker thread because Windows binds `SetThreadExecutionState` per thread.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use windows::Win32::System::Power::{
    ES_AWAYMODE_REQUIRED, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
};

#[derive(Debug, Default)]
struct ThreadState {
    want: bool,
    quit: bool,
}

/// Manages a persistent worker thread calling `SetThreadExecutionState` on Windows.
#[derive(Debug, Default)]
pub(crate) struct SleepInhibitor {
    shared: Option<Arc<(Mutex<ThreadState>, Condvar)>>,
    thread: Option<JoinHandle<()>>,
    active: bool,
}

impl SleepInhibitor {
    fn ensure_worker(&mut self) -> Option<&Arc<(Mutex<ThreadState>, Condvar)>> {
        if self.shared.is_none() {
            let pair = Arc::new((Mutex::new(ThreadState::default()), Condvar::new()));
            let worker_pair = pair.clone();
            let handle = std::thread::Builder::new()
                .name("ferrocore-power".into())
                .spawn(move || run_worker(&worker_pair))
                .ok()?;
            self.shared = Some(pair);
            self.thread = Some(handle);
        }
        self.shared.as_ref()
    }

    pub(crate) fn set_active(&mut self, active: bool) -> bool {
        if active == self.active {
            return self.active;
        }
        if active {
            if let Some(pair) = self.ensure_worker().cloned() {
                let mut st = pair.0.lock().unwrap_or_else(|e| e.into_inner());
                st.want = true;
                pair.1.notify_one();
                drop(st);
                self.active = true;
            }
        } else {
            if let Some(pair) = &self.shared {
                let mut st = pair.0.lock().unwrap_or_else(|e| e.into_inner());
                st.want = false;
                pair.1.notify_one();
            }
            self.active = false;
        }
        self.active
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        if let Some(pair) = self.shared.take() {
            {
                let mut st = pair.0.lock().unwrap_or_else(|e| e.into_inner());
                st.want = false;
                st.quit = true;
                pair.1.notify_one();
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
        self.active = false;
    }
}

fn run_worker(pair: &(Mutex<ThreadState>, Condvar)) {
    let mut current = false;
    loop {
        let (target, quit) = {
            let mut st = pair.0.lock().unwrap_or_else(|e| e.into_inner());
            while st.want == current && !st.quit {
                st = pair.1.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            (if st.quit { false } else { st.want }, st.quit)
        };
        if target != current {
            let flags = if target {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_AWAYMODE_REQUIRED | ES_DISPLAY_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            // SAFETY: sets or clears the calling thread's own execution state flags; no pointers.
            let prev = unsafe { SetThreadExecutionState(flags) };
            if prev.0 == 0 {
                tracing::warn!(target, "SetThreadExecutionState failed");
            } else {
                tracing::debug!(target, "Windows sleep inhibition updated");
            }
            current = target;
        }
        if quit {
            break;
        }
    }
}
