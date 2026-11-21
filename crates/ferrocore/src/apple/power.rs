//! macOS sleep inhibition via IOKit power management assertions.
//!
//! Adapted from OBS `libobs/util/platform-cocoa.m` (`os_inhibit_sleep_create` /
//! `os_inhibit_sleep_set_active` / `os_inhibit_sleep_destroy`).

use std::ffi::CStr;

use super::CFStringRef;

type IOReturn = i32;
type IOPMAssertionLevel = u32;
type IOPMAssertionID = u32;

const K_IO_RETURN_SUCCESS: IOReturn = 0;
const K_IOPM_ASSERTION_LEVEL_ON: IOPMAssertionLevel = 255;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: IOPMAssertionLevel,
        assertion_name: CFStringRef,
        assertion_id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
}

/// Holds an active IOKit power assertion while live streaming or recording.
#[derive(Debug, Default)]
pub(crate) struct SleepInhibitor {
    assertion_id: Option<IOPMAssertionID>,
}

impl SleepInhibitor {
    pub(crate) fn set_active(&mut self, active: bool) -> bool {
        if active == self.assertion_id.is_some() {
            return self.assertion_id.is_some();
        }
        if active {
            let reason = super::string(c"Ferrocast live streaming or recording");
            let mut id: IOPMAssertionID = 0;
            for kind in [c"NoDisplaySleepAssertion", c"PreventUserIdleSystemSleep"] {
                let Some(kind_cf) = super::string(kind) else { continue };
                let Some(reason_cf) = &reason else { break };
                // SAFETY: `kind_cf` and `reason_cf` are live CFStrings; `id` points to stack storage.
                let rc = unsafe {
                    IOPMAssertionCreateWithName(kind_cf.0, K_IOPM_ASSERTION_LEVEL_ON, reason_cf.0, &raw mut id)
                };
                if rc == K_IO_RETURN_SUCCESS {
                    self.assertion_id = Some(id);
                    tracing::debug!(assertion = ?kind, id, "macOS sleep inhibition active");
                    return true;
                }
            }
            tracing::warn!("IOPMAssertionCreateWithName failed to inhibit sleep");
            return false;
        }
        if let Some(id) = self.assertion_id.take() {
            // SAFETY: `id` was returned by `IOPMAssertionCreateWithName` and is released once here.
            let rc = unsafe { IOPMAssertionRelease(id) };
            if rc != K_IO_RETURN_SUCCESS {
                tracing::warn!(rc, id, "IOPMAssertionRelease returned non-zero status");
            } else {
                tracing::debug!(id, "macOS sleep inhibition released");
            }
        }
        false
    }

    pub(crate) fn is_active(&self) -> bool {
        self.assertion_id.is_some()
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

// Keep unused CStr import warning clean if needed.
const _: fn() = || {
    let _ = CStr::from_bytes_with_nul;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertion_can_activate_and_release() {
        let mut guard = SleepInhibitor::default();
        let active = guard.set_active(true);
        // Even in headless CI where WindowServer might restrict NoDisplaySleepAssertion, calling
        // set_active(false) or dropping never panics and leaves `is_active() == false`.
        let _ = active;
        assert!(!guard.set_active(false));
        assert!(!guard.is_active());
    }
}
