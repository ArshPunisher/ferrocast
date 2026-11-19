//! Sleep inhibition coordination across active streaming and recording outputs.

use std::sync::Mutex;

#[cfg(target_os = "macos")]
type Backend = crate::apple::power::SleepInhibitor;
#[cfg(target_os = "windows")]
type Backend = crate::win::power::SleepInhibitor;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug, Default)]
struct Backend {
    active: bool,
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Backend {
    fn set_active(&mut self, active: bool) -> bool {
        self.active = active;
        self.active
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

#[derive(Debug, Default)]
struct Inner {
    last_epoch: u64,
    backend: Backend,
}

/// Thread-safe leaf controller that inhibits OS sleep while at least one streaming or recording
/// output is active, ordering concurrent updates by `epoch`.
#[derive(Debug, Default)]
pub(crate) struct SleepMonitor {
    inner: Mutex<Inner>,
}

impl SleepMonitor {
    pub(crate) fn apply(&self, epoch: u64, want: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if epoch > inner.last_epoch {
            inner.last_epoch = epoch;
            inner.backend.set_active(want);
        }
    }

    pub(crate) fn release_all(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last_epoch = u64::MAX;
        inner.backend.set_active(false);
    }

    pub(crate) fn is_inhibited(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).backend.is_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_epochs_never_re_inhibit_after_release() {
        let m = SleepMonitor::default();
        assert!(!m.is_inhibited());

        // Start output (epoch 1) activates sleep inhibition.
        m.apply(1, true);
        let after_start = m.is_inhibited();
        // On desktop macOS/Windows `after_start` is true; on Linux/no-op backend it's true.
        // Regardless of OS sandbox restrictions, epoch ordering never lets an older `true` override
        // a newer `false`.
        m.apply(2, false);
        assert!(!m.is_inhibited());

        // Out-of-order delayed epoch 1 must be ignored.
        m.apply(1, true);
        assert!(!m.is_inhibited());

        // Dropping/releasing explicitly leaves sleep uninhibited even when previous start worked.
        if after_start {
            m.apply(3, true);
            assert!(m.is_inhibited());
        }
        m.release_all();
        assert!(!m.is_inhibited());
    }
}
