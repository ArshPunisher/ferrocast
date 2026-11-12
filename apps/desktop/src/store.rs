//! Ferrocast installed from the Microsoft Store (an MSIX package, packaging/msix).
//!
//! The Store signs the package with Microsoft's certificate, so Smart App Control and
//! SmartScreen let it run from the first install, with no paid signing certificate, and
//! Windows shows our own publisher name. A packaged app plays by the package's rules:
//! - updates come from the Store, so the app's own updater stays off;
//! - "launch at login" is the package's startup task (the Run key the autostart plugin writes
//!   is private to the package, so Windows would never read it);
//! - FFmpeg ships inside the package, next to the program, because Store apps may not fetch
//!   programs to run.
//!
//! Everywhere else (the NSIS install, macOS, Linux) these answer "not packaged".

#[cfg(windows)]
mod imp {
    use windows::ApplicationModel::Activation::ActivationKind;
    use windows::ApplicationModel::{AppInstance, Package, StartupTask, StartupTaskState};

    /// The startup task's id in AppxManifest.xml (`desktop:StartupTask TaskId`).
    const STARTUP_TASK: &str = "FerrocastStartup";

    pub fn packaged() -> bool {
        static PACKAGED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PACKAGED.get_or_init(|| Package::Current().is_ok())
    }

    pub fn started_at_login() -> bool {
        packaged()
            && AppInstance::GetActivatedEventArgs()
                .and_then(|a| a.Kind())
                .is_ok_and(|k| k == ActivationKind::StartupTask)
    }

    pub fn set_launch_at_login(on: bool) -> Result<(), String> {
        let task = StartupTask::GetAsync(&windows::core::HSTRING::from(STARTUP_TASK))
            .and_then(|op| op.get())
            .map_err(|e| format!("no startup task in the package ({e})"))?;
        if !on {
            return task.Disable().map_err(|e| e.to_string());
        }
        match task.RequestEnableAsync().and_then(|op| op.get()).map_err(|e| e.to_string())? {
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => Ok(()),
            StartupTaskState::DisabledByUser => {
                Err("turned off in Windows Settings > Apps > Startup; turn Ferrocast on there".into())
            }
            StartupTaskState::DisabledByPolicy => Err("turned off by this PC's policy".into()),
            other => Err(format!("Windows left it off (state {})", other.0)),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn packaged() -> bool {
        false
    }

    pub fn started_at_login() -> bool {
        false
    }

    pub fn set_launch_at_login(_on: bool) -> Result<(), String> {
        Err("only an MSIX package has a startup task".into())
    }
}

/// Whether this copy runs from an MSIX package (the Microsoft Store, or a test install of one).
pub fn packaged() -> bool {
    imp::packaged()
}

/// Whether Windows started this copy at sign-in through the startup task. The task can't pass
/// `--minimized` the way the autostart plugin's Run key does, so this stands in for it.
pub fn started_at_login() -> bool {
    imp::started_at_login()
}

/// Turns the package's startup task on or off. Blocking (it waits on WinRT): call it off the
/// async runtime. Windows keeps the last word: a task the person turned off in Windows'
/// Startup apps settings can't be turned back on by the app, and that comes back as an error.
pub fn set_launch_at_login(on: bool) -> Result<(), String> {
    imp::set_launch_at_login(on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_build_is_not_packaged() {
        // Tests run from cargo's target folder, never from an MSIX package.
        assert!(!packaged());
        assert!(!started_at_login());
    }
}
